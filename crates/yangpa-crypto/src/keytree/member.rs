//! 멤버 쪽 키 상태([`MemberKeys`]). 갱신을 적용하면 새 값을 돌려주므로 부르는 쪽이 순번마다 보관했다가 이긴 갈래로
//! 옮길 수 있습니다.

use serde::{Deserialize, Serialize};
use yangpa_core::body::Secret32;
use yangpa_core::ids::{ChannelId, DeviceId, SpaceId};
use yangpa_core::keyupdate::{KeyGroupId, KeyUpdateKind, KeyUpdatePayload, LayoutOp, NodeIdx};
use zeroize::Zeroizing;

use super::{
    Bundle, DeviceBundle, GroupKind, KeyUpdateError, Node, Result, WrapCx, WrapKind, WrapSite,
    advance_node, bundle_ad, direct_ad, encode_secret, find_wrap, private_leaf,
};
use crate::error::CryptoError;
use crate::keysched;
use crate::sealed::{BoxKeypair, open_sealed, sealed_recipients};

/// 멤버 기기 하나가 가진 그룹 키(T·R)와, 트리 인코딩이면 자기 잎·경로.
#[derive(Clone)]
pub struct MemberKeys {
    space: SpaceId,
    group: KeyGroupId,
    kind: GroupKind,
    device: DeviceId,
    seq: u64,
    root_commit: [u8; 32],
    t: Secret32,
    r: Secret32,
    pos: Pos,
}

#[derive(Clone)]
pub(crate) enum Pos {
    Direct,
    Tree(Box<TreePos>),
    /// 덮개 그룹: 자기 자리 없이 크루 경로로 엶.
    Cover,
}

/// 트리 안 자기 자리: 칸, 잎, 높이 1..=깊이의 경로(마지막이 뿌리 = T).
#[derive(Clone)]
pub(crate) struct TreePos {
    pub slot: u32,
    pub generation: u64,
    pub leaf: Node,
    pub path: Vec<Node>,
}

impl TreePos {
    fn depth(&self) -> u8 {
        self.path.len() as u8
    }

    fn below(&self, path: &[Node], level: u8) -> Secret32 {
        if level == 1 {
            self.leaf.key.clone()
        } else {
            path[usize::from(level) - 2].key.clone()
        }
    }

    /// 잎부터 뿌리까지 (노드 번호, 키·판).
    pub(crate) fn nodes(&self) -> impl Iterator<Item = (NodeIdx, &Node)> {
        std::iter::once((NodeIdx::leaf(self.slot), &self.leaf)).chain(
            self.path
                .iter()
                .enumerate()
                .map(|(i, n)| (NodeIdx::ancestor(self.slot, i as u8 + 1), n)),
        )
    }
}

impl std::fmt::Debug for MemberKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemberKeys")
            .field("group", &self.group)
            .field("kind", &self.kind)
            .field("device", &self.device)
            .field("seq", &self.seq)
            .field("depth", &self.depth())
            .finish_non_exhaustive()
    }
}

/// 갱신을 적용할 때 필요한 것.
#[derive(Clone, Copy, Default)]
pub struct MemberCtx<'a> {
    /// 이 기기의 상자 키(직접 인코딩 봉인 상자, 기기 몫 봉인 묶음을 열 때).
    pub box_keys: Option<&'a BoxKeypair>,
    /// 이 갱신에서 이 기기에게 온 봉인 묶음(크루 가입·전체 재키, Inbox·블롭 조각·중계로 받음). 페이로드의
    /// `bundle_root`와 맞는 것만 엽니다.
    pub bundle: Option<&'a DeviceBundle>,
    /// 같은 공간의 크루 키: 페이로드의 `crew`(크루 순번·약속)와 같은 순번까지 적용한 크루 상태. 비공개 트리·덮개
    /// 그룹에서 크루 잎·노드로 열 때 필요합니다.
    pub crew: Option<&'a MemberKeys>,
}

#[derive(Clone, Copy)]
struct Base {
    space: SpaceId,
    group: KeyGroupId,
    kind: GroupKind,
    device: DeviceId,
}

impl MemberKeys {
    /// 이 기기가 들어가는 갱신(가입 목록·전체 재키)으로 그룹 키를 처음 얻습니다. 앞 사슬은 볼 수 없으므로 확인하지
    /// 않고, 계산한 T가 `root_commit`과 맞는지만 봅니다.
    pub fn join(
        space: SpaceId,
        group: KeyGroupId,
        kind: GroupKind,
        device: DeviceId,
        payload: &KeyUpdatePayload,
        cx: &MemberCtx<'_>,
    ) -> Result<MemberKeys> {
        let base = Base {
            space,
            group,
            kind,
            device,
        };
        step(&base, None, payload, cx)
    }

    /// 다음 갱신(합친 페이로드)을 적용한 새 값. 순번이 이어지지 않거나([`KeyUpdateError::SeqGap`]) 앞 약속이 다르면
    /// ([`KeyUpdateError::ChainMismatch`]) 적용하지 않습니다. 계산한 T가 약속과 다르면
    /// [`KeyUpdateError::RootCommitMismatch`]입니다(확정 기기가 기기마다 다른 키를 줬다는 증거).
    pub fn apply(&self, payload: &KeyUpdatePayload, cx: &MemberCtx<'_>) -> Result<MemberKeys> {
        let base = Base {
            space: self.space,
            group: self.group,
            kind: self.kind,
            device: self.device,
        };
        step(&base, Some(self), payload, cx)
    }

    pub fn space(&self) -> &SpaceId {
        &self.space
    }

    pub fn group(&self) -> &KeyGroupId {
        &self.group
    }

    pub fn kind(&self) -> GroupKind {
        self.kind
    }

    pub fn device(&self) -> &DeviceId {
        &self.device
    }

    pub fn seq(&self) -> u64 {
        self.seq
    }

    pub fn root_commit(&self) -> &[u8; 32] {
        &self.root_commit
    }

    pub fn t(&self) -> &Secret32 {
        &self.t
    }

    pub fn r(&self) -> &Secret32 {
        &self.r
    }

    pub fn state_key(&self) -> Secret32 {
        keysched::state_key(&self.r)
    }

    pub fn public_root(&self, channel: &ChannelId) -> Secret32 {
        keysched::public_root(&self.r, &self.space, channel)
    }

    pub fn private_root(&self, channel: &ChannelId) -> Secret32 {
        keysched::private_root(&self.t, &self.space, channel)
    }

    /// 트리 깊이(직접 인코딩·덮개 그룹이면 0).
    pub fn depth(&self) -> u8 {
        self.tree_pos().map_or(0, TreePos::depth)
    }

    /// 트리 안 칸(트리 인코딩일 때만).
    pub fn slot(&self) -> Option<u32> {
        self.tree_pos().map(|p| p.slot)
    }

    pub(crate) fn tree_pos(&self) -> Option<&TreePos> {
        match &self.pos {
            Pos::Tree(p) => Some(p),
            _ => None,
        }
    }

    /// 멤버가 중계해 준 지금 T·R로 만든 상태(묶음을 놓친 기기). 사슬 끝의 뿌리 약속과 맞을 때만 만듭니다. 트리 자리는
    /// 없어 키 파생(상태 키·채널 뿌리)과 직접 인코딩 갱신에만 쓰이고, 다음 트리 갱신은 적용하지 못하면 다시 묻습니다.
    #[allow(clippy::too_many_arguments)]
    pub fn from_root(
        space: SpaceId,
        group: KeyGroupId,
        kind: GroupKind,
        device: DeviceId,
        seq: u64,
        root_commit: &[u8; 32],
        t: Secret32,
        r: Secret32,
    ) -> Result<MemberKeys> {
        let commit = keysched::root_commit(&space, &group, seq, &t, &r);
        if seq == 0 || !keysched::ct_eq(&commit, root_commit) {
            return Err(KeyUpdateError::RootCommitMismatch);
        }
        Ok(MemberKeys {
            space,
            group,
            kind,
            device,
            seq,
            root_commit: *root_commit,
            t,
            r,
            pos: if kind == GroupKind::Cover {
                Pos::Cover
            } else {
                Pos::Direct
            },
        })
    }

    /// 이 기기의 잠긴 키 저장소에 둘 인코딩(비밀값이 들어 있어 해제 시 0으로 덮음).
    pub fn encode(&self) -> Zeroizing<Vec<u8>> {
        encode_secret(&MemberSnap {
            v: MEMBER_SNAP_VERSION,
            space: self.space,
            group: self.group,
            kind: self.kind,
            device: self.device,
            seq: self.seq,
            root_commit: self.root_commit,
            t: self.t.clone(),
            r: self.r.clone(),
            pos: match &self.pos {
                Pos::Direct => PosSnap::Direct,
                Pos::Cover => PosSnap::Cover,
                Pos::Tree(tp) => PosSnap::Tree {
                    slot: tp.slot,
                    generation: tp.generation,
                    leaf: tp.leaf.clone(),
                    path: tp.path.clone(),
                },
            },
        })
    }

    /// [`Self::encode`]의 역. 저장소가 망가졌어도 엉뚱한 키를 쓰지 않게 약속(T·R)과 트리 뿌리 = T를 다시 봅니다.
    pub fn decode(bytes: &[u8]) -> Result<MemberKeys> {
        let s: MemberSnap =
            postcard::from_bytes(bytes).map_err(|_| KeyUpdateError::Malformed("멤버 키 상태"))?;
        if s.v != MEMBER_SNAP_VERSION {
            return Err(KeyUpdateError::Malformed("멤버 키 상태 판"));
        }
        let commit = keysched::root_commit(&s.space, &s.group, s.seq, &s.t, &s.r);
        if !keysched::ct_eq(&commit, &s.root_commit) {
            return Err(KeyUpdateError::Malformed("멤버 키 상태 약속"));
        }
        let pos = match s.pos {
            PosSnap::Direct => Pos::Direct,
            PosSnap::Cover => Pos::Cover,
            PosSnap::Tree {
                slot,
                generation,
                leaf,
                path,
            } => {
                let depth = path.len();
                if depth == 0
                    || depth > usize::from(yangpa_core::keyupdate::MAX_DEPTH)
                    || u64::from(slot) >> depth != 0
                    || !path
                        .last()
                        .is_some_and(|n| keysched::ct_eq(&n.key.0, &s.t.0))
                {
                    return Err(KeyUpdateError::Malformed("멤버 키 상태 트리"));
                }
                Pos::Tree(Box::new(TreePos {
                    slot,
                    generation,
                    leaf,
                    path,
                }))
            }
        };
        Ok(MemberKeys {
            space: s.space,
            group: s.group,
            kind: s.kind,
            device: s.device,
            seq: s.seq,
            root_commit: s.root_commit,
            t: s.t,
            r: s.r,
            pos,
        })
    }
}

const MEMBER_SNAP_VERSION: u8 = 1;

#[derive(Serialize, Deserialize)]
struct MemberSnap {
    v: u8,
    space: SpaceId,
    group: KeyGroupId,
    kind: GroupKind,
    device: DeviceId,
    seq: u64,
    root_commit: [u8; 32],
    t: Secret32,
    r: Secret32,
    pos: PosSnap,
}

#[derive(Serialize, Deserialize)]
enum PosSnap {
    Direct,
    Tree {
        slot: u32,
        generation: u64,
        leaf: Node,
        path: Vec<Node>,
    },
    Cover,
}

fn step(
    base: &Base,
    prev: Option<&MemberKeys>,
    p: &KeyUpdatePayload,
    cx: &MemberCtx<'_>,
) -> Result<MemberKeys> {
    if p.group != base.group {
        return Err(KeyUpdateError::WrongGroup);
    }
    if p.part != 0 || p.parts != 1 {
        return Err(KeyUpdateError::Malformed("조각을 합친 뒤 적용해야 함"));
    }
    p.validate_merged()?;
    // 비공개 그룹은 인코딩(자기 트리 ↔ 덮개)이 바뀔 수 있어 종류를 페이로드에서 읽습니다: 깊이가 있으면 자기 트리,
    // 깊이 0에 감싸기가 있으면 덮개, 봉인 상자뿐이면 둘이 같은 계산이라 앞 종류를 그대로 씁니다.
    let inferred = Base {
        kind: infer_kind(base.kind, p),
        ..*base
    };
    let base = &inferred;
    if let Some(prev) = prev {
        if p.seq != prev.seq + 1 {
            return Err(KeyUpdateError::SeqGap {
                have: prev.seq,
                got: p.seq,
            });
        }
        if !keysched::ct_eq(&p.prev_root_commit, &prev.root_commit) {
            return Err(KeyUpdateError::ChainMismatch);
        }
    }
    if p.kind.removed().binary_search(&base.device).is_ok() {
        return Err(KeyUpdateError::Removed);
    }
    let joined_me = p.kind.joined().binary_search(&base.device).is_ok();
    if joined_me && prev.is_some() {
        return Err(KeyUpdateError::Malformed(
            "이미 멤버인 기기가 가입 목록에 있음",
        ));
    }
    // 비공개·덮개 그룹만 크루 상태에 기댑니다.
    if (base.kind == GroupKind::Crew) != p.crew.is_none() {
        return Err(KeyUpdateError::Malformed("크루 상태 표시"));
    }
    let mut got_r = None;
    let (t, pos) = match base.kind {
        GroupKind::Cover => (
            cover_t(base, prev, p, cx, joined_me, &mut got_r)?,
            Pos::Cover,
        ),
        _ if p.depth == 0 => (
            direct_t(base, prev, p, cx, joined_me, &mut got_r)?,
            Pos::Direct,
        ),
        _ => {
            let tp = tree_step(base, prev, p, cx, joined_me, &mut got_r)?;
            let t = tp.path.last().expect("깊이 1 이상").key.clone();
            (t, Pos::Tree(Box::new(tp)))
        }
    };
    let mine = prev.map(|m| m.r.clone()).or(got_r);
    let r = match &p.r_ct {
        Some(ct) => {
            // 계산한 T로 r_ct가 열리지 않으면 T가 약속과 다른 것입니다(확정 기기가 서명한 페이로드 안의 값이므로
            // 증거 쪽 오류로 냄).
            let r = keysched::open_r(&t, &base.space, &base.group, p.seq, &p.salt, ct)
                .map_err(|_| KeyUpdateError::RootCommitMismatch)?;
            // 가입만인 갱신의 r_ct는 지금 R을 다시 암호화한 것이어야 합니다.
            if !p.kind.new_r()
                && let Some(m) = &mine
                && !keysched::ct_eq(&m.0, &r.0)
            {
                return Err(KeyUpdateError::RMismatch);
            }
            r
        }
        None => mine.ok_or(KeyUpdateError::NoKeyForMe)?,
    };
    // T와 R을 함께 약속과 비교합니다(가입자가 묶음에서 받은 R도 그룹 모두와 같은 값이어야 함).
    let commit = keysched::root_commit(&base.space, &base.group, p.seq, &t, &r);
    if !keysched::ct_eq(&commit, &p.root_commit) {
        return Err(KeyUpdateError::RootCommitMismatch);
    }
    Ok(MemberKeys {
        space: base.space,
        group: base.group,
        kind: base.kind,
        device: base.device,
        seq: p.seq,
        root_commit: p.root_commit,
        t,
        r,
        pos,
    })
}

fn infer_kind(kind: GroupKind, p: &KeyUpdatePayload) -> GroupKind {
    match kind {
        GroupKind::Crew => GroupKind::Crew,
        _ if p.depth > 0 => GroupKind::Private,
        _ if !p.wraps.is_empty() => GroupKind::Cover,
        k => k,
    }
}

fn prev_t(prev: Option<&MemberKeys>) -> Result<Secret32> {
    prev.map(|m| m.t.clone()).ok_or(KeyUpdateError::NoKeyForMe)
}

fn map_open(e: CryptoError) -> KeyUpdateError {
    match e {
        CryptoError::NotARecipient => KeyUpdateError::NoKeyForMe,
        e => KeyUpdateError::Crypto(e),
    }
}

/// 직접 인코딩 봉인 상자에서 내 몫을 엽니다.
fn open_direct(
    base: &Base,
    p: &KeyUpdatePayload,
    cx: &MemberCtx<'_>,
    got_r: &mut Option<Secret32>,
) -> Result<Secret32> {
    let kp = cx.box_keys.ok_or(KeyUpdateError::NoKeyForMe)?;
    let ad = direct_ad(&base.space, &base.group, p.seq, &p.root_commit);
    for b in &p.sealed {
        match sealed_recipients(&b.0) {
            Ok(to) if to.contains(&base.device) => {}
            _ => continue,
        }
        let pt = open_sealed(&base.device, kp, &ad, &b.0).map_err(map_open)?;
        return match Bundle::decode(&pt)? {
            Bundle::Direct { t, r } => {
                *got_r = r;
                Ok(t)
            }
            Bundle::Tree { .. } => Err(KeyUpdateError::Malformed("직접 봉인 상자에 트리 묶음")),
        };
    }
    Err(KeyUpdateError::NoKeyForMe)
}

fn direct_t(
    base: &Base,
    prev: Option<&MemberKeys>,
    p: &KeyUpdatePayload,
    cx: &MemberCtx<'_>,
    joined_me: bool,
    got_r: &mut Option<Secret32>,
) -> Result<Secret32> {
    match &p.kind {
        KeyUpdateKind::RRefresh => prev_t(prev),
        KeyUpdateKind::Advance { joined } if !joined_me => {
            let m = prev.ok_or(KeyUpdateError::NoKeyForMe)?;
            if !matches!(m.pos, Pos::Direct) {
                return Err(KeyUpdateError::Malformed(
                    "트리에서 직접 인코딩으로는 새 T로만 바뀜",
                ));
            }
            Ok(keysched::advance(
                &m.t,
                &base.group,
                p.seq,
                &keysched::joined_hash(joined),
            ))
        }
        _ => open_direct(base, p, cx, got_r),
    }
}

/// 이 갱신이 기대는 크루 상태(페이로드의 `crew`와 순번·약속이 같은 것).
fn crew_state<'a>(base: &Base, p: &KeyUpdatePayload, cx: &MemberCtx<'a>) -> Result<&'a MemberKeys> {
    let want = p
        .crew
        .as_ref()
        .ok_or(KeyUpdateError::Malformed("크루 상태 표시가 없음"))?;
    let crew = cx
        .crew
        .filter(|c| c.kind == GroupKind::Crew && c.space == base.space)
        .ok_or(KeyUpdateError::NeedCrewTree)?;
    if crew.seq != want.seq || !keysched::ct_eq(&crew.root_commit, &want.root_commit) {
        return Err(KeyUpdateError::CrewMismatch {
            want: want.seq,
            have: Some(crew.seq),
        });
    }
    Ok(crew)
}

/// 두 칸이 처음 같은 조상을 갖는 높이.
fn meet_level(a: u32, b: u32) -> u8 {
    (32 - (a ^ b).leading_zeros()) as u8
}

fn tree_step(
    base: &Base,
    prev: Option<&MemberKeys>,
    p: &KeyUpdatePayload,
    cx: &MemberCtx<'_>,
    joined_me: bool,
    got_r: &mut Option<Secret32>,
) -> Result<TreePos> {
    let prev_tree = prev.and_then(MemberKeys::tree_pos);
    let wcx = WrapCx {
        space: &base.space,
        group: &base.group,
        seq: p.seq,
        salt: &p.salt,
        crew: p.crew.as_ref(),
    };
    match &p.kind {
        KeyUpdateKind::FullRekey { generation, .. } => match base.kind {
            GroupKind::Crew => from_bundle(base, p, cx, got_r),
            _ => private_walk(base, prev, p, cx, &wcx, Some(*generation)),
        },
        KeyUpdateKind::RRefresh => {
            let tp = prev_tree.ok_or(KeyUpdateError::NoKeyForMe)?;
            if tp.depth() != p.depth {
                return Err(KeyUpdateError::Malformed("깊이가 바뀐 RRefresh"));
            }
            Ok(tp.clone())
        }
        _ if prev_tree.is_none() => {
            if !joined_me {
                return Err(if prev.is_some() {
                    KeyUpdateError::Malformed("직접 인코딩에서 트리로는 FullRekey로만 바뀜")
                } else {
                    KeyUpdateError::NoKeyForMe
                });
            }
            let tp = match base.kind {
                GroupKind::Crew => from_bundle(base, p, cx, got_r)?,
                _ => private_walk(base, prev, p, cx, &wcx, None)?,
            };
            let filled = p.layout.iter().any(|op| {
                matches!(op, LayoutOp::Fill { slot, device } if *slot == tp.slot && *device == base.device)
            });
            if !filled {
                return Err(KeyUpdateError::Malformed("가입 칸이 배치와 다름"));
            }
            Ok(tp)
        }
        KeyUpdateKind::Refresh => {
            let tp = prev_tree.expect("위에서 확인");
            let d = tp.depth();
            if p.depth != d {
                return Err(KeyUpdateError::Malformed("깊이가 바뀐 Refresh"));
            }
            let mut next = tp.clone();
            let site = WrapSite {
                kind: WrapKind::Tree,
                node: NodeIdx::root(d),
                version: p.seq,
            };
            let below = tp.below(&tp.path, d);
            let key = find_wrap(&wcx, &p.wraps, &below, site).ok_or(KeyUpdateError::NoKeyForMe)?;
            next.path[usize::from(d) - 1] = Node {
                key,
                version: p.seq,
            };
            Ok(next)
        }
        KeyUpdateKind::Advance { joined } | KeyUpdateKind::Rotate { joined, .. } => {
            let tp = prev_tree.expect("위에서 확인");
            let (d0, d) = (tp.depth(), p.depth);
            if d < d0 {
                return Err(KeyUpdateError::Malformed("트리 깊이가 줄어듦"));
            }
            // 내 경로에서 새로 뽑힌 노드(제거 경로)와 전진한 노드(가입 경로)가 시작하는 높이.
            let (mut dirty_from, mut join_from) = (u8::MAX, u8::MAX);
            for op in &p.layout {
                if op.slot() == tp.slot {
                    return Err(KeyUpdateError::Malformed("내 칸이 바뀜"));
                }
                match op {
                    LayoutOp::Clear { slot } => {
                        dirty_from = dirty_from.min(meet_level(*slot, tp.slot))
                    }
                    LayoutOp::Fill { slot, .. } => {
                        join_from = join_from.min(meet_level(*slot, tp.slot))
                    }
                }
            }
            let jh = keysched::joined_hash(joined);
            let mut path: Vec<Node> = Vec::with_capacity(usize::from(d));
            for l in 1..=d {
                let n = NodeIdx::ancestor(tp.slot, l);
                let node = if l > d0 || (l >= dirty_from && l <= d0) {
                    let below = tp.below(&path, l);
                    let site = WrapSite {
                        kind: WrapKind::Tree,
                        node: n,
                        version: p.seq,
                    };
                    let key = find_wrap(&wcx, &p.wraps, &below, site)
                        .ok_or(KeyUpdateError::NoKeyForMe)?;
                    Node {
                        key,
                        version: p.seq,
                    }
                } else if l >= join_from {
                    let old = &tp.path[usize::from(l) - 1];
                    Node {
                        key: advance_node(&old.key, &base.group, p.seq, n, &jh),
                        version: p.seq,
                    }
                } else {
                    tp.path[usize::from(l) - 1].clone()
                };
                path.push(node);
            }
            Ok(TreePos {
                slot: tp.slot,
                generation: tp.generation,
                leaf: tp.leaf.clone(),
                path,
            })
        }
    }
}

/// 크루 가입·전체 재키: 봉인 묶음에서 잎과 경로를 받습니다. 묶음은 페이로드 밖에서 오므로(누가 만들었는지 모르는 익명
/// 봉투) 먼저 페이로드의 `bundle_root`에 든 것인지 보고, 그다음 이 갱신(갈래까지)에 묶인 AD로 엽니다. 잎·경로·R이
/// 맞는지는 끝에 뿌리 약속(T·R)과 이 증명이 함께 보장합니다.
fn from_bundle(
    base: &Base,
    p: &KeyUpdatePayload,
    cx: &MemberCtx<'_>,
    got_r: &mut Option<Secret32>,
) -> Result<TreePos> {
    let (kp, b) = match (cx.box_keys, cx.bundle) {
        (Some(kp), Some(b)) => (kp, b),
        _ => return Err(KeyUpdateError::NoKeyForMe),
    };
    let root = p
        .bundle_root
        .as_ref()
        .ok_or(KeyUpdateError::Malformed("묶음 뿌리가 없음"))?;
    if b.device != base.device || !b.verify(root) {
        return Err(KeyUpdateError::BadBundle);
    }
    let ad = bundle_ad(
        &base.space,
        &base.group,
        p.seq,
        &base.device,
        &p.root_commit,
    );
    let pt = open_sealed(&base.device, kp, &ad, &b.envelope).map_err(map_open)?;
    let Bundle::Tree {
        slot,
        generation,
        leaf,
        path,
        r,
    } = Bundle::decode(&pt)?
    else {
        return Err(KeyUpdateError::Malformed("트리 묶음이 아님"));
    };
    if path.len() != usize::from(p.depth) || u64::from(slot) >= 1u64 << p.depth {
        return Err(KeyUpdateError::Malformed("묶음 경로가 깊이와 다름"));
    }
    if let KeyUpdateKind::FullRekey { generation: g, .. } = p.kind
        && g != generation
    {
        return Err(KeyUpdateError::Malformed("묶음 세대가 다름"));
    }
    *got_r = r;
    Ok(TreePos {
        slot,
        generation,
        leaf: Node {
            key: leaf,
            version: generation,
        },
        path: path
            .into_iter()
            .map(|key| Node {
                key,
                version: p.seq,
            })
            .collect(),
    })
}

/// 비공개 트리 가입·재구성: 크루 잎에서 비공개 잎을 만들고 경로를 감싸기에서 차례로 엽니다.
fn private_walk(
    base: &Base,
    prev: Option<&MemberKeys>,
    p: &KeyUpdatePayload,
    cx: &MemberCtx<'_>,
    wcx: &WrapCx<'_>,
    generation: Option<u64>,
) -> Result<TreePos> {
    let crew = crew_state(base, p, cx)?
        .tree_pos()
        .ok_or(KeyUpdateError::NeedCrewTree)?;
    if generation.is_some_and(|g| g != crew.generation) {
        return Err(KeyUpdateError::Malformed("크루 세대가 다름"));
    }
    let slot = p.layout.iter().find_map(|op| match op {
        LayoutOp::Fill { slot, device } if *device == base.device => Some(*slot),
        _ => None,
    });
    let Some(slot) = slot else {
        // 재구성 배치에 내가 없으면 빠진 것입니다.
        return Err(if prev.is_some() {
            KeyUpdateError::Removed
        } else {
            KeyUpdateError::NoKeyForMe
        });
    };
    let leaf = Node {
        key: private_leaf(&crew.leaf.key, &base.group, crew.generation),
        version: crew.generation,
    };
    let mut path: Vec<Node> = Vec::with_capacity(usize::from(p.depth));
    let mut below = leaf.key.clone();
    for l in 1..=p.depth {
        let site = WrapSite {
            kind: WrapKind::Tree,
            node: NodeIdx::ancestor(slot, l),
            version: p.seq,
        };
        let key = find_wrap(wcx, &p.wraps, &below, site).ok_or(KeyUpdateError::NoKeyForMe)?;
        below = key.clone();
        path.push(Node {
            key,
            version: p.seq,
        });
    }
    Ok(TreePos {
        slot,
        generation: crew.generation,
        leaf,
        path,
    })
}

/// 덮개 그룹의 새 T.
fn cover_t(
    base: &Base,
    prev: Option<&MemberKeys>,
    p: &KeyUpdatePayload,
    cx: &MemberCtx<'_>,
    joined_me: bool,
    got_r: &mut Option<Secret32>,
) -> Result<Secret32> {
    match &p.kind {
        KeyUpdateKind::RRefresh => prev_t(prev),
        KeyUpdateKind::Advance { joined } if !joined_me => Ok(keysched::advance(
            &prev_t(prev)?,
            &base.group,
            p.seq,
            &keysched::joined_hash(joined),
        )),
        _ if !p.sealed.is_empty() => open_direct(base, p, cx, got_r),
        _ => {
            let crew = crew_state(base, p, cx)?
                .tree_pos()
                .ok_or(KeyUpdateError::NeedCrewTree)?;
            let wcx = WrapCx {
                space: &base.space,
                group: &base.group,
                seq: p.seq,
                salt: &p.salt,
                crew: p.crew.as_ref(),
            };
            crew.nodes()
                .find_map(|(node, n)| {
                    let site = WrapSite {
                        kind: WrapKind::Cover,
                        node,
                        version: n.version,
                    };
                    find_wrap(&wcx, &p.wraps, &n.key, site)
                })
                .ok_or(KeyUpdateError::NoKeyForMe)
        }
    }
}
