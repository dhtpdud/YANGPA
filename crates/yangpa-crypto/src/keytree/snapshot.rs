//! 확정 기기 넘기기: 키 그룹 표 전체(모든 노드 키·leaf_master)를 새 확정 기기에게 봉인한 블롭으로 넘깁니다.
//!
//! 블롭은 무작위 키로 암호화하고(내용 주소 블롭으로 보낼 수 있게), 그 키만 받는 기기들에게 X-Wing으로 봉인합니다.
//! 봉인 AD가 블롭 ID를 묶어 다른 블롭과 짝지을 수 없습니다. X-Wing 봉투는 익명이라 누구나 받는 기기 공개키로 만들 수
//! 있으므로, 보내는 확정 기기가 (블롭 ID ‖ 봉투 해시)에 서명하고 받는 쪽은 해소기가 정한 보낸 기기의 서명인지부터
//! 봅니다. 그다음 표가 스스로 맞는지(약속 = T·R, 뿌리 키 = T, 트리 모양, 크루 잎 = leaf_master에서 나온 값)를 확인해
//! 겉보기만 맞는 표로 다른 키를 쓰게 하지 못합니다.
//!
//! 넘겨받은 기기도 leaf_master를 갖게 되므로 표에 표 보유 기기로 적고(보낸 기기·받는 기기 모두), 그 기기를 나중에 뺄
//! 때는 크루 전체 재키가 필요합니다.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use yangpa_core::blob::{BlobManifest, DEFAULT_CHUNK_SIZE};
use yangpa_core::body::Secret32;
use yangpa_core::device::BoxPublicKey;
use yangpa_core::ids::{BlobId, DeviceId, EventId, SpaceId};
use yangpa_core::keyupdate::{KeyGroupId, MAX_DEPTH, NodeIdx};
use yangpa_core::sign::{Sig, SignDomain, SigningKeypair, verify};

use super::committer::Enc;
use super::cover::CoverGroup;
use super::tree::Tree;
use super::{GroupKind, KeyTree, KeyUpdateError, Node, Result, encode_secret, leaf_key};
use crate::blob::{decrypt_blob, encrypt_blob};
use crate::keysched::{self, ct_eq};
use crate::sealed::{BoxKeypair, open_sealed, seal_to};

/// 2: 표 보유 기기·재키 판과 보낸 기기 서명이 들어감. 3: 크루 잎을 싣지 않음(leaf_master에서 다시 만듦).
const SNAPSHOT_VERSION: u8 = 3;

#[derive(Serialize, Deserialize)]
struct Snapshot {
    v: u8,
    trees: Vec<TreeSnap>,
    covers: Vec<CoverSnap>,
}

#[derive(Serialize, Deserialize)]
struct TreeSnap {
    space: SpaceId,
    group: KeyGroupId,
    kind: GroupKind,
    seq: u64,
    root_commit: [u8; 32],
    last_event: Option<EventId>,
    generation: u64,
    t: Secret32,
    r: Secret32,
    enc: EncSnap,
    recovering: Option<bool>,
    holders: Vec<DeviceId>,
    rekey_epoch: u64,
    crew_epoch: u64,
}

#[derive(Serialize, Deserialize)]
enum EncSnap {
    Direct(Vec<DeviceId>),
    /// 크루 트리는 `levels[0]`(잎)을 비워 둡니다. 잎은 leaf_master·기기·세대에서 나오므로 실을 까닭이 없고, 4.5만 대면
    /// 표가 1.6MB쯤 작아집니다.
    Tree {
        depth: u8,
        slots: Vec<Option<DeviceId>>,
        levels: Vec<Vec<Option<Node>>>,
        leaf_master: Option<Secret32>,
    },
}

#[derive(Serialize, Deserialize)]
struct CoverSnap {
    space: SpaceId,
    group: KeyGroupId,
    seq: u64,
    root_commit: [u8; 32],
    last_event: Option<EventId>,
    t: Secret32,
    r: Secret32,
    members: Vec<DeviceId>,
    crew_generation: u64,
    crew_epoch: u64,
    recovering: bool,
}

/// 봉인한 확정 기기 상태. `ciphertext`는 내용 주소 블롭(`manifest`)으로 보내고, `envelope`은 받는 기기들만 엽니다.
/// `sig`는 보낸 확정 기기가 (블롭 ID ‖ 봉투 해시)에 한 서명입니다.
#[derive(Clone)]
pub struct SealedHandover {
    pub envelope: Vec<u8>,
    pub manifest: BlobManifest,
    pub ciphertext: Vec<u8>,
    pub sig: Sig,
}

impl std::fmt::Debug for SealedHandover {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "SealedHandover(봉투 {}B, 블롭 {}B)",
            self.envelope.len(),
            self.ciphertext.len()
        )
    }
}

fn handover_ad(blob: &BlobId) -> Vec<u8> {
    let mut ad = b"yangpa/v3/key-table".to_vec();
    ad.extend_from_slice(blob.as_bytes());
    ad
}

/// 서명하는 내용: 블롭 ID(표 내용)와 봉투 해시(받는 기기·키). 둘 중 하나만 바꿔 붙일 수 없습니다.
fn handover_msg(blob: &BlobId, envelope: &[u8]) -> Vec<u8> {
    let mut m = blob.as_bytes().to_vec();
    m.extend_from_slice(blake3::hash(envelope).as_bytes());
    m
}

/// 키 그룹 표를 받는 기기들(새 확정 기기와 그 대기 기기)에게 서명·봉인합니다. 표의 크루 그룹에는 보내는 기기(`signer`)와
/// 받는 기기를 표 보유 기기로 적습니다. 보내는 기기가 계속 확정 기기로 남으면 자기 크루 그룹에도
/// [`KeyTree::add_holders`]로 받는 기기를 더해야 합니다.
pub fn seal_handover(
    trees: &[&KeyTree],
    covers: &[&CoverGroup],
    to: &[(DeviceId, BoxPublicKey)],
    signer: &SigningKeypair,
) -> Result<SealedHandover> {
    if to.is_empty() {
        return Err(KeyUpdateError::Malformed("넘겨받을 기기가 없음"));
    }
    let me = DeviceId(signer.public());
    let to_devices: Vec<DeviceId> = to.iter().map(|(d, _)| *d).collect();
    let snap = snapshot_of(trees, covers, Some((me, &to_devices)));
    let plain = encode_secret(&snap);
    drop(snap);
    let key = Secret32::random();
    let (ciphertext, manifest) = encrypt_blob(&key, &plain, DEFAULT_CHUNK_SIZE);
    let id = manifest.id();
    let envelope = seal_to(to, &handover_ad(&id), &key.0);
    let sig = signer.sign(SignDomain::KeyTable, &handover_msg(&id, &envelope));
    Ok(SealedHandover {
        envelope,
        manifest,
        ciphertext,
        sig,
    })
}

/// 표 전체의 직렬화 모양. `handover`가 있으면(보내는 기기, 받는 기기들) 크루 그룹의 표 보유 기기에 더합니다.
fn snapshot_of(
    trees: &[&KeyTree],
    covers: &[&CoverGroup],
    handover: Option<(DeviceId, &[DeviceId])>,
) -> Snapshot {
    Snapshot {
        v: SNAPSHOT_VERSION,
        trees: trees
            .iter()
            .map(|k| {
                let mut holders = k.holders.clone();
                if k.kind == GroupKind::Crew
                    && let Some((me, to)) = handover
                {
                    holders.insert(me);
                    holders.extend(to.iter().copied());
                }
                TreeSnap {
                    space: k.space,
                    group: k.group,
                    kind: k.kind,
                    seq: k.seq,
                    root_commit: k.root_commit,
                    last_event: k.last_event,
                    generation: k.generation,
                    t: k.t.clone(),
                    r: k.r.clone(),
                    enc: match &k.enc {
                        Enc::Direct(m) => EncSnap::Direct(m.iter().copied().collect()),
                        Enc::Tree(t) => EncSnap::Tree {
                            depth: t.depth,
                            slots: t.slots.clone(),
                            levels: if t.leaf_master.is_some() {
                                std::iter::once(Vec::new())
                                    .chain(t.levels.iter().skip(1).cloned())
                                    .collect()
                            } else {
                                t.levels.clone()
                            },
                            leaf_master: t.leaf_master.clone(),
                        },
                    },
                    recovering: k.recovering,
                    holders: holders.into_iter().collect(),
                    rekey_epoch: k.rekey_epoch,
                    crew_epoch: k.crew_epoch,
                }
            })
            .collect(),
        covers: covers
            .iter()
            .map(|c| CoverSnap {
                space: c.space,
                group: c.group,
                seq: c.seq,
                root_commit: c.root_commit,
                last_event: c.last_event,
                t: c.t.clone(),
                r: c.r.clone(),
                members: c.members.iter().copied().collect(),
                crew_generation: c.crew_generation,
                crew_epoch: c.crew_epoch,
                recovering: c.recovering,
            })
            .collect(),
    }
}

/// 확정 기기가 자기 표를 이 기기의 잠긴 키 저장소에 둘 인코딩(넘기기와 같은 모양, 봉인·서명 없음). 다시 시작해도
/// 표를 잃지 않아 전체 재키 없이 이어 갑니다.
pub fn encode_table(trees: &[&KeyTree], covers: &[&CoverGroup]) -> zeroize::Zeroizing<Vec<u8>> {
    encode_secret(&snapshot_of(trees, covers, None))
}

/// [`encode_table`]의 역. 넘겨받은 표와 같은 검사(약속·트리 모양·잎)를 거칩니다.
pub fn decode_table(bytes: &[u8], me: &DeviceId) -> Result<(Vec<KeyTree>, Vec<CoverGroup>)> {
    let snap: Snapshot =
        postcard::from_bytes(bytes).map_err(|_| KeyUpdateError::Malformed("저장한 키 표"))?;
    from_snapshot(snap, me)
}

/// 넘겨받은 키 그룹 표를 엽니다. `from`은 해소기가 정한 보낸 확정 기기입니다. 서명이 그 기기 것이 아니거나 표가 스스로
/// 맞지 않으면 [`KeyUpdateError::BadHandover`]입니다. 연 뒤에는 각 그룹의 [`KeyTree::tip`]이 내가 적용한 사슬 끝과
/// 같은지 부르는 쪽이 봅니다.
pub fn open_handover(
    me: &DeviceId,
    kp: &BoxKeypair,
    h: &SealedHandover,
    from: &DeviceId,
) -> Result<(Vec<KeyTree>, Vec<CoverGroup>)> {
    let id = h.manifest.id();
    verify(
        from.as_bytes(),
        SignDomain::KeyTable,
        &handover_msg(&id, &h.envelope),
        &h.sig,
    )
    .map_err(|_| KeyUpdateError::BadHandover("보낸 확정 기기의 서명이 아님"))?;
    let key = open_sealed(me, kp, &handover_ad(&id), &h.envelope)?;
    let key = Secret32(
        key.as_slice()
            .try_into()
            .map_err(|_| KeyUpdateError::Malformed("넘기기 키 길이"))?,
    );
    let plain = zeroize::Zeroizing::new(decrypt_blob(&key, &h.manifest, &h.ciphertext)?);
    let snap: Snapshot =
        postcard::from_bytes(&plain).map_err(|_| KeyUpdateError::Malformed("넘긴 상태"))?;
    from_snapshot(snap, me)
}

/// 직렬화한 표를 풀고 스스로 맞는지 확인합니다(넘겨받은 표·저장한 표 모두).
fn from_snapshot(snap: Snapshot, me: &DeviceId) -> Result<(Vec<KeyTree>, Vec<CoverGroup>)> {
    if snap.v != SNAPSHOT_VERSION {
        return Err(KeyUpdateError::Malformed("넘긴 상태 판"));
    }
    let trees = snap
        .trees
        .into_iter()
        .map(|s| tree_from(s, me))
        .collect::<Result<Vec<_>>>()?;
    check_private_leaves(&trees)?;
    let covers = snap
        .covers
        .into_iter()
        .map(|c| {
            if !c.recovering && !commit_ok(&c.space, &c.group, c.seq, &c.root_commit, &c.t, &c.r) {
                return Err(KeyUpdateError::BadHandover("덮개 그룹 약속"));
            }
            Ok(CoverGroup {
                space: c.space,
                group: c.group,
                seq: c.seq,
                root_commit: c.root_commit,
                last_event: c.last_event,
                t: c.t,
                r: c.r,
                members: c.members.into_iter().collect(),
                crew_generation: c.crew_generation,
                crew_epoch: c.crew_epoch,
                recovering: c.recovering,
                #[cfg(test)]
                last_sites: Vec::new(),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok((trees, covers))
}

/// 순번 0(아직 갱신 없음)이면 약속이 없습니다.
fn commit_ok(
    space: &SpaceId,
    group: &KeyGroupId,
    seq: u64,
    commit: &[u8; 32],
    t: &Secret32,
    r: &Secret32,
) -> bool {
    seq == 0 || ct_eq(&keysched::root_commit(space, group, seq, t, r), commit)
}

fn tree_from(s: TreeSnap, me: &DeviceId) -> Result<KeyTree> {
    let crew_group = KeyGroupId::crew(&s.space);
    match s.kind {
        GroupKind::Crew if s.group != crew_group => {
            return Err(KeyUpdateError::BadHandover("크루 그룹 ID"));
        }
        GroupKind::Private if s.group == crew_group => {
            return Err(KeyUpdateError::BadHandover("비공개 그룹 ID"));
        }
        GroupKind::Cover => {
            return Err(KeyUpdateError::BadHandover("덮개 그룹이 트리 자리에 있음"));
        }
        _ => {}
    }
    if s.recovering.is_none() && !commit_ok(&s.space, &s.group, s.seq, &s.root_commit, &s.t, &s.r) {
        return Err(KeyUpdateError::BadHandover("뿌리 약속"));
    }
    let enc = match s.enc {
        EncSnap::Direct(m) => Enc::Direct(m.into_iter().collect::<BTreeSet<_>>()),
        EncSnap::Tree {
            depth,
            slots,
            mut levels,
            leaf_master,
        } => {
            if depth == 0 || depth > MAX_DEPTH {
                return Err(KeyUpdateError::Malformed("넘긴 트리 깊이"));
            }
            if let Some(lm) = &leaf_master {
                // 크루 잎은 싣지 않았으므로 다시 만듭니다(크루 잎의 판은 늘 그 세대).
                if levels.first().is_none_or(|l| !l.is_empty()) || slots.len() != 1usize << depth {
                    return Err(KeyUpdateError::Malformed("넘긴 트리 모양"));
                }
                levels[0] = slots
                    .iter()
                    .map(|d| {
                        d.map(|d| Node {
                            key: leaf_key(lm, &s.group, &d, s.generation),
                            version: s.generation,
                        })
                    })
                    .collect();
            }
            let tree = Tree::from_parts(depth, slots, levels, leaf_master)
                .ok_or(KeyUpdateError::Malformed("넘긴 트리 모양"))?;
            if s.recovering.is_some() {
                return Err(KeyUpdateError::BadHandover("이어받은 상태에 트리가 있음"));
            }
            check_tree(s.kind, &s.group, s.generation, &s.t, &tree)?;
            Enc::Tree(Box::new(tree))
        }
    };
    let mut holders: BTreeSet<DeviceId> = s.holders.into_iter().collect();
    holders.remove(me);
    Ok(KeyTree {
        space: s.space,
        group: s.group,
        kind: s.kind,
        seq: s.seq,
        root_commit: s.root_commit,
        last_event: s.last_event,
        generation: s.generation,
        t: s.t,
        r: s.r,
        enc,
        recovering: s.recovering,
        holders,
        rekey_epoch: s.rekey_epoch,
        crew_epoch: s.crew_epoch,
        #[cfg(test)]
        last_sites: Vec::new(),
    })
}

/// 트리 표가 스스로 맞는지: 키가 기기 있는 곳에만 있고, 뿌리 키가 T이고, 크루면 잎이 모두
/// leaf_master에서 나온 값.
fn check_tree(
    kind: GroupKind,
    group: &KeyGroupId,
    generation: u64,
    t: &Secret32,
    tree: &Tree,
) -> Result<()> {
    if !tree.well_formed() || tree.len() == 0 {
        return Err(KeyUpdateError::BadHandover("트리 모양"));
    }
    let root = tree
        .get(tree.root())
        .ok_or(KeyUpdateError::BadHandover("트리 뿌리"))?;
    if !ct_eq(&root.key.0, &t.0) {
        return Err(KeyUpdateError::BadHandover("뿌리 키가 T가 아님"));
    }
    match (kind, &tree.leaf_master) {
        (GroupKind::Crew, Some(lm)) => {
            for (slot, d) in tree.devices() {
                let leaf = tree.get(NodeIdx::leaf(slot)).expect("모양 검사를 거침");
                let want = leaf_key(lm, group, d, generation);
                if leaf.version != generation || !ct_eq(&leaf.key.0, &want.0) {
                    return Err(KeyUpdateError::BadHandover("크루 잎"));
                }
            }
            Ok(())
        }
        (GroupKind::Private, None) => Ok(()),
        _ => Err(KeyUpdateError::BadHandover("leaf_master")),
    }
}

/// 비공개 트리 잎이 같은 표의 크루 잎에서 나온 값인지(같은 크루 세대이고 크루에 남은 기기만 확인할 수 있음).
fn check_private_leaves(trees: &[KeyTree]) -> Result<()> {
    let Some(crew) = trees.iter().find(|k| k.kind == GroupKind::Crew) else {
        return Ok(());
    };
    for p in trees.iter().filter(|k| k.kind == GroupKind::Private) {
        check_private_against(crew, p)?;
    }
    Ok(())
}

/// 그룹마다 따로 넘겨받은 비공개 트리를 이미 가진 크루 표와 맞춰 봅니다(같은 공간이어야 하고, 잎은
/// [`check_private_leaves`]와 같은 검사).
pub fn check_private_against(crew: &KeyTree, p: &KeyTree) -> Result<()> {
    if crew.kind != GroupKind::Crew || p.space != crew.space {
        return Err(KeyUpdateError::BadHandover("다른 공간의 그룹"));
    }
    let (Some(pt), Some(ct)) = (p.tree(), crew.tree()) else {
        return Ok(());
    };
    if p.kind != GroupKind::Private || crew.generation != p.generation {
        return Ok(());
    }
    for (slot, d) in pt.devices() {
        // 크루에서 막 빠졌고 비공개 그룹은 아직인 기기는 확인할 크루 잎이 없음(같은 묶음의 다음 갱신이 뺌).
        let Some(crew_leaf) = ct.dev_slot.get(d).and_then(|s| ct.get(NodeIdx::leaf(*s))) else {
            continue;
        };
        let leaf = pt.get(NodeIdx::leaf(slot)).expect("모양 검사를 거침");
        let want = super::private_leaf(&crew_leaf.key, &p.group, p.generation);
        if !ct_eq(&leaf.key.0, &want.0) {
            return Err(KeyUpdateError::BadHandover("비공개 잎"));
        }
    }
    Ok(())
}
