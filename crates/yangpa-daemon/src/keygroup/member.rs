//! 멤버 쪽 키 그룹 상태(공간마다 `SpaceCore::keys`, 적용 잠금 안에서만 만짐).
//!
//! - **크루**: 해소기가 받아들인 크루 `KeyUpdate`(마지막 조각, `Resolver::key_events`)를 해소 순서대로 적용합니다. 조각은 상태
//!   이벤트처럼 열 때([`note_crew_part`]) (작성 기기, 순번, salt)로 모아 두었다가 합칩니다.
//! - **비공개·덮개 그룹**: 키 그룹 스트림의 갱신은 해소기에 들어가지 않아 그룹마다 [`GroupChain`]이 판정하고, 이긴
//!   사슬을 적용합니다.
//! - 적용한 상태(`MemberKeys`)는 뿌리 약속마다 보관합니다. 늦게 온 후보가 이겨 갈래가 바뀌면 갈림 지점의 상태에서 이긴
//!   갈래를 다시 적용하고, 진 갈래의 상태는 그 키로 쓴 메시지를 읽는 데만 남습니다. 바탕 체크포인트가 그 그룹의 키
//!   머리를 덮을 때까지는 남기고(그 아래로는 갈래가 바뀌지 않음), 최근 [`KEEP_SEQS`] 순번은 늘 남깁니다. 체크포인트가
//!   없으면 최근 [`KEEP_SEQS`] 순번, 체크포인트가 오래돼도 [`MAX_KEPT_SEQS`] 순번까지입니다.
//! - 바탕 체크포인트의 키 머리로 그룹 사슬을 맞춥니다([`GroupChain::apply_head`]): 처음 보는 그룹은 머리에서 시작하고
//!   (새로 자격을 얻은 기기는 머리 뒤 자기 가입 갱신의 묶음으로 키를 얻음), 머리까지 접은 바닥을 저장해 다시 시작할 때
//!   이어 갑니다(`keys::save_chain_bases`).
//! - 상태는 잠긴 키 저장소에 그룹마다 비밀값 하나와 목록 행(`keys::save_member_row`)으로 둡니다. 바뀐 그룹만 다시
//!   씁니다(그룹 하나의 갱신이 모든 그룹의 64개 상태를 다시 쓰지 않게). 휘발성 키는 여기 오지 않습니다.
//! - 계산한 T·R이 뿌리 약속과 다르면(`RootCommitMismatch`) 확정 기기가 기기마다 다른 키를 줬다는 증거라, 그 갱신의
//!   위치만 설정에 남기고 경고합니다(키·내용은 로그에 남기지 않음).

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use tracing::{debug, warn};
use yangpa_core::body::{EventBody, Secret32};
use yangpa_core::event::{EventHeader, KeyRef, SignedEvent};
use yangpa_core::ids::{
    ChannelId, ChannelRootId, DeviceId, EventId, KeyGroupId, SpaceId, StateKeyId,
};
use yangpa_core::keyupdate::KeyUpdatePayload;
use yangpa_core::model::{HistoryVisibility, SpaceState};
use yangpa_crypto::keysched;
use yangpa_crypto::{DeviceBundle, GroupKind, KeyUpdateError, MemberCtx, MemberKeys};
use yangpa_sync::{AuthError, GroupChain, Resolver};
use zeroize::Zeroizing;

use crate::error::Result;
use crate::keys;
use crate::node::Node;

/// 그룹마다 늘 보관하는 상태의 순번 폭(사슬 끝에서 이만큼 앞까지). `yangpa_sync`의 그룹 사슬이 갈래를 바꾸는 깊이
/// 한도(64)와 같습니다.
pub(crate) const KEEP_SEQS: u64 = 64;
/// 체크포인트가 덮지 않은 상태를 남기는 상한(체크포인트가 아주 오래됐을 때).
pub(crate) const MAX_KEPT_SEQS: u64 = 1024;
/// 받아 둔 기기 몫 묶음 상한(그룹·순번마다 내 것 하나면 충분).
const MAX_BUNDLES: usize = 64;
/// 키를 기다리는 키 그룹 이벤트 상한.
const MAX_WAITING: usize = 4096;
/// 남기는 증거 상한.
const MAX_EVIDENCE: usize = 16;
/// 멤버 상태 저장 형식 판(예전 한 행 형식).
const SNAP_VERSION: u8 = 1;
/// 그룹마다 둔 멤버 상태 목록 행의 판.
const INDEX_VERSION: u8 = 2;

type CrewPartKey = (DeviceId, u64, [u8; 24]);

/// 그룹 하나의 멤버 상태.
#[derive(Default)]
pub(crate) struct MemberGroup {
    /// 뿌리 약속 → 그 갱신까지 적용한 상태.
    states: HashMap<[u8; 32], MemberKeys>,
    /// 처리한 갱신 머리(적용했거나 이 기기와 상관없음).
    done: HashSet<EventId>,
    /// 사슬 끝인데 키를 얻지 못함(그 순번): 멤버에게 다시 묻습니다(`relay`).
    pub(crate) stuck: Option<u64>,
}

impl MemberGroup {
    pub(crate) fn state(&self, root_commit: &[u8; 32]) -> Option<&MemberKeys> {
        self.states.get(root_commit)
    }

    pub(crate) fn states(&self) -> impl Iterator<Item = &MemberKeys> {
        self.states.values()
    }

    /// `covered`: 바탕 체크포인트의 이 그룹 키 머리 순번(없으면 체크포인트가 없거나 머리가 없는 그룹).
    fn prune(&mut self, tip_seq: u64, keep: &[u8; 32], covered: Option<u64>) {
        let recent = tip_seq.saturating_sub(KEEP_SEQS);
        let floor = covered
            .map_or(recent, |c| c.min(recent))
            .max(tip_seq.saturating_sub(MAX_KEPT_SEQS));
        self.states.retain(|rc, k| rc == keep || k.seq() >= floor);
    }
}

/// 공간 하나의 멤버 쪽 키 그룹 상태.
#[derive(Default)]
pub(crate) struct MemberState {
    loaded: bool,
    pub(crate) groups: HashMap<KeyGroupId, MemberGroup>,
    pub(crate) chains: HashMap<KeyGroupId, GroupChain>,
    /// 사슬을 맞춘 바탕 체크포인트(바뀌면 키 머리를 다시 적용).
    heads_of: Option<yangpa_core::ids::CheckpointId>,
    /// 바탕 체크포인트가 덮은 그룹별 순번(크루 포함): 보관 상태 정리에 씁니다.
    covered: HashMap<KeyGroupId, u64>,
    /// 사슬 바닥을 다시 저장해야 함.
    bases_dirty: bool,
    /// 마지막으로 저장한 그룹별 바닥(순번, 뿌리 약속, 멤버를 앎): 바뀐 그룹만 다시 씁니다.
    saved: HashMap<KeyGroupId, (u64, [u8; 32], bool)>,
    /// 사슬 판정을 다시 해야 함(멤버 목록을 배움).
    relearned: bool,
    /// 열어 둔 크루 갱신 조각(작성 기기, 순번, salt) → (조각 이벤트, 페이로드). 해소기가 받아들이면 합쳐 적용합니다.
    crew_parts: HashMap<CrewPartKey, Vec<(EventId, KeyUpdatePayload)>>,
    crew_part_of: HashMap<EventId, CrewPartKey>,
    /// 처리한 크루 갱신(마지막 조각 ID).
    crew_done: HashSet<EventId>,
    /// 열어 둔 키 그룹 갱신 조각.
    group_parts: HashMap<EventId, (KeyGroupId, KeyUpdatePayload)>,
    /// 받은 기기 몫 묶음(그룹, 순번, 묶음). 증명은 적용할 때 봅니다.
    bundles: Vec<(KeyGroupId, u64, DeviceBundle)>,
    /// 상태 키를 기다리는 키 그룹 이벤트, 기준점을 기다리는 키 그룹 이벤트.
    waiting_key: HashMap<StateKeyId, Vec<EventId>>,
    waiting_anchor: Vec<(EventId, EventHeader, KeyUpdatePayload)>,
    /// 받은편지함으로 받은 그룹 R(가입 때 확정 기기가 넣어 줌): T 없이도 그 R의 공개 채널 뿌리와 상태 키를 얻습니다.
    rs: Vec<(KeyGroupId, Secret32)>,
    /// 묶다가 막힌 그룹을 다시 볼 때가 됐는지(묶음·중계 키가 옴).
    retry: bool,
    /// 다시 써야 하는 그룹 행과 목록 행(그룹 목록·R이 바뀜).
    dirty: HashSet<KeyGroupId>,
    index_dirty: bool,
    /// 예전 한 행 형식에서 읽음(다음 저장 뒤 지움).
    legacy: bool,
    /// `Resolver::key_events` 앞부분 가운데 모두 처리한 길이와 그 끝 ID. 해소기가 다시 정렬해 그 자리가 바뀌었으면
    /// 처음부터 봅니다(처리한 갱신 기억이 기록과 함께 자라지 않게).
    crew_scan: (usize, Option<EventId>),
    /// 모르는 채널 뿌리를 찾는(그룹 상태에서 파생, 멤버에게 묻기) 예산: (작성 기기, 채널)마다. 아무 뿌리 ID나 붙인
    /// 메시지로 받는 기기마다 파생·중계 요청을 시키지 못하게.
    root_budget: HashMap<(DeviceId, ChannelId), yangpa_sync::policy::TokenBucket>,
    /// 지금 가진 그룹 상태로 찾지 못한 뿌리(새 상태를 얻으면 비움): 같은 뿌리를 다시 파생해 보지 않습니다.
    missed_roots: HashSet<ChannelRootId>,
}

/// 모르는 뿌리 찾기 예산: 한 번에 [`ROOT_LOOKUP_BURST`]번, 그 뒤 [`ROOT_LOOKUP_EVERY_S`]초에 한 번.
const ROOT_LOOKUP_BURST: u64 = 16;
const ROOT_LOOKUP_EVERY_S: u64 = 10;
/// 예산·찾지 못한 뿌리 기억 상한(넘으면 비움).
const MAX_ROOT_BUDGETS: usize = 4096;
const MAX_MISSED_ROOTS: usize = 16 * 1024;

/// 정리 한 번의 결과.
#[derive(Default)]
pub(crate) struct Drained {
    /// 새로 얻은 상태 키(그 키를 기다리던 이벤트를 깨움).
    pub(crate) state_keys: Vec<StateKeyId>,
    /// 새 상태를 하나라도 얻음(뿌리를 기다리던 메시지를 다시 봄).
    pub(crate) progressed: bool,
    /// 사슬 끝인데 키를 얻지 못한 그룹(멤버에게 묻기).
    pub(crate) stuck: Vec<KeyGroupId>,
    /// 사슬 끝 멤버를 모르는 그룹(체크포인트 키 머리에서 시작, 멤버에게 묻기).
    pub(crate) learn: Vec<KeyGroupId>,
    /// 기기 몫이 사용자별 조각 블롭에 든 크루 갱신(순번, 블롭): 내 조각을 받아 옵니다.
    pub(crate) fetch: Vec<(u64, yangpa_core::ids::BlobId)>,
}

#[derive(Serialize, Deserialize)]
struct Snap {
    v: u8,
    groups: Vec<(KeyGroupId, Vec<Vec<u8>>)>,
    rs: Vec<(KeyGroupId, Secret32)>,
}

/// 보관하는 R 상한(그룹마다 가입 때 하나씩).
const MAX_RS: usize = 64;

/// 비밀이 든 값을 정확한 크기로 한 번에 직렬화합니다(버퍼가 자라며 옮겨질 때 옛 버퍼에 평문이 남지 않게).
pub(crate) fn encode_secret<T: Serialize>(v: &T) -> Zeroizing<Vec<u8>> {
    let n = postcard::serialize_with_flavor(v, postcard::ser_flavors::Size::default())
        .expect("직렬화는 실패하지 않음");
    Zeroizing::new(postcard::to_extend(v, Vec::with_capacity(n)).expect("직렬화는 실패하지 않음"))
}

impl MemberState {
    /// 처음 쓸 때 저장한 멤버 상태를 읽습니다(그룹마다 둔 행, 없으면 예전 한 행 형식).
    pub(crate) fn ensure_loaded(&mut self, node: &Node, space: &SpaceId) {
        if self.loaded {
            return;
        }
        self.loaded = true;
        let mut rows: Vec<(KeyGroupId, Vec<Vec<u8>>)> = Vec::new();
        match keys::load_member_row(node, space, None) {
            Ok(Some(bytes)) => {
                let Ok(idx) = postcard::from_bytes::<keys::MemberIndex>(&bytes) else {
                    warn!(space = %space.short(), "저장한 키 그룹 멤버 상태 목록을 읽지 못함");
                    return;
                };
                if idx.v != INDEX_VERSION {
                    return;
                }
                self.rs = idx.rs;
                for g in idx.groups {
                    if let Ok(Some(b)) = keys::load_member_row(node, space, Some(&g))
                        && let Ok(list) = postcard::from_bytes::<Vec<Vec<u8>>>(&b)
                    {
                        rows.push((g, list));
                    }
                }
            }
            _ => {
                let Ok(Some(bytes)) = keys::load_member_blob(node, space) else {
                    return;
                };
                let Ok(snap) = postcard::from_bytes::<Snap>(&bytes) else {
                    warn!(space = %space.short(), "저장한 키 그룹 멤버 상태를 읽지 못함");
                    return;
                };
                if snap.v != SNAP_VERSION {
                    return;
                }
                self.rs = snap.rs;
                rows = snap.groups;
                self.legacy = true;
                self.index_dirty = true;
            }
        }
        for (g, mut list) in rows {
            let grp = self.groups.entry(g).or_default();
            for b in &list {
                match MemberKeys::decode(b) {
                    Ok(k) if *k.space() == *space && *k.group() == g => {
                        grp.states.insert(*k.root_commit(), k);
                    }
                    _ => debug!(space = %space.short(), "망가진 키 그룹 멤버 상태 하나를 버림"),
                }
            }
            for b in &mut list {
                zeroize::Zeroize::zeroize(b);
            }
            if self.legacy {
                self.dirty.insert(g);
            }
        }
        match keys::load_chain_bases(node, space) {
            Ok(bases) => {
                for (g, b) in bases {
                    self.saved
                        .insert(g, (b.seq, b.root_commit, b.members_known));
                    self.chains
                        .entry(g)
                        .or_insert_with(|| GroupChain::from_saved(*space, g, b));
                }
            }
            Err(e) => debug!(error = %e, "저장한 키 그룹 사슬 바닥을 읽지 못함"),
        }
    }

    /// 바탕 체크포인트가 바뀌었으면 그 키 머리로 그룹 사슬을 맞춥니다(처음 보는 그룹은 머리에서 시작). 받은 그룹 갱신을
    /// 넣기 전과 정리 때 부릅니다.
    fn apply_heads(&mut self, space: &SpaceId, resolver: &Resolver) {
        let base = resolver.base();
        let id = base.as_ref().map(|b| b.id);
        if id == self.heads_of {
            return;
        }
        self.heads_of = id;
        self.covered.clear();
        let Some(b) = base else {
            return;
        };
        let crew = KeyGroupId::crew(space);
        for h in b.key_heads.iter() {
            self.covered.insert(h.group, h.seq);
            if h.group == crew {
                continue;
            }
            match self.chains.get_mut(&h.group) {
                Some(c) => {
                    c.apply_head(h, resolver);
                }
                None => {
                    if let Some(c) = GroupChain::from_head(*space, h) {
                        self.chains.insert(h.group, c);
                    }
                }
            }
        }
        self.bases_dirty = true;
    }

    /// 바뀐 사슬 바닥(체크포인트 머리로 옮김, 멤버를 배움, 오래된 앞부분을 접음)만 그룹마다 저장합니다(멤버 목록은 그대로,
    /// 상한 없이).
    fn save_bases(&mut self, node: &Node, space: &SpaceId) -> Result<()> {
        let mut changed = Vec::new();
        let mut groups = Vec::new();
        for (g, c) in &self.chains {
            let key = c.base_key();
            if key.0 == 0 {
                continue;
            }
            groups.push(*g);
            // 바탕 체크포인트가 바뀌면(머리로 바닥을 다시 세움) 같은 순번이어도 다시 씁니다.
            if self.bases_dirty || self.saved.get(g) != Some(&key) {
                changed.push((*g, c.saved_base()));
            }
        }
        if changed.is_empty() && !self.bases_dirty && groups.len() == self.saved.len() {
            return Ok(());
        }
        groups.sort_unstable();
        keys::save_chain_bases(node, space, &changed, &groups)?;
        for (g, b) in &changed {
            self.saved
                .insert(*g, (b.seq, b.root_commit, b.members_known));
        }
        self.saved.retain(|g, _| groups.contains(g));
        self.bases_dirty = false;
        Ok(())
    }

    /// 바뀐 그룹만 저장합니다(그룹 행, 그룹 목록·R이 바뀌었으면 목록 행).
    pub(crate) fn persist(&mut self, node: &Node, space: &SpaceId) -> Result<()> {
        self.save_bases(node, space)?;
        if self.dirty.is_empty() && !self.index_dirty {
            return Ok(());
        }
        for g in std::mem::take(&mut self.dirty) {
            let Some(grp) = self.groups.get(&g) else {
                continue;
            };
            let mut list: Vec<Vec<u8>> = grp.states.values().map(|k| k.encode().to_vec()).collect();
            let bytes = encode_secret(&list);
            for b in list.iter_mut() {
                zeroize::Zeroize::zeroize(b);
            }
            if let Err(e) = keys::save_member_row(node, space, Some(&g), &bytes) {
                self.dirty.insert(g);
                return Err(e);
            }
        }
        if self.index_dirty {
            let idx = keys::MemberIndex {
                v: INDEX_VERSION,
                groups: self
                    .groups
                    .iter()
                    .filter(|(_, g)| !g.states.is_empty())
                    .map(|(id, _)| *id)
                    .collect(),
                rs: self.rs.clone(),
            };
            keys::save_member_row(node, space, None, &encode_secret(&idx))?;
            self.index_dirty = false;
            if std::mem::take(&mut self.legacy) {
                keys::delete_member_blob(node, space)?;
            }
        }
        Ok(())
    }

    /// 그룹 `g`의 상태가 바뀜(새 그룹이면 목록도).
    fn touch(&mut self, g: KeyGroupId) {
        if !self.dirty.contains(&g) && self.groups.get(&g).is_none_or(|grp| grp.states.len() <= 1) {
            self.index_dirty = true;
        }
        self.dirty.insert(g);
    }

    /// 크루 갱신 조각을 열었습니다(상태 이벤트처럼 열림). 해소기가 받아들인 뒤 [`Self::drain`]이 합칩니다.
    pub(crate) fn note_crew_part(
        &mut self,
        id: EventId,
        header: &EventHeader,
        p: KeyUpdatePayload,
    ) {
        if self.crew_part_of.contains_key(&id) {
            return;
        }
        let key = (header.author, p.seq, p.salt);
        self.crew_part_of.insert(id, key);
        self.crew_parts.entry(key).or_default().push((id, p));
    }

    /// 기기 몫 묶음을 받았습니다(받은편지함·조각 블롭·중계). 증명은 적용할 때 갱신의 `bundle_root`로 봅니다.
    pub(crate) fn offer_bundle(&mut self, group: KeyGroupId, seq: u64, bundle: DeviceBundle) {
        if self
            .bundles
            .iter()
            .any(|(g, s, b)| *g == group && *s == seq && *b == bundle)
        {
            return;
        }
        if self.bundles.len() >= MAX_BUNDLES {
            self.bundles.remove(0);
        }
        self.bundles.push((group, seq, bundle));
        self.retry = true;
    }

    /// 멤버가 중계해 준 지금 T·R(뿌리 약속을 확인한 것).
    pub(crate) fn offer_root(&mut self, k: MemberKeys) {
        self.missed_roots.clear();
        let g = *k.group();
        let grp = self.groups.entry(g).or_default();
        grp.states.entry(*k.root_commit()).or_insert(k);
        grp.stuck = None;
        self.touch(g);
        self.retry = true;
    }

    /// 멤버가 알려 준(또는 확정 기기 표의) 그룹 사슬 바닥 (순번, 뿌리 약속) 뒤 멤버 목록([`GroupChain::learn_members`]:
    /// 머리의 멤버 요약과 맞을 때만). 받아들였으면 참이고 다음 정리에서 사슬을 다시 판정합니다.
    pub(crate) fn learn_members(
        &mut self,
        group: KeyGroupId,
        seq: u64,
        root_commit: &[u8; 32],
        members: &[DeviceId],
    ) -> bool {
        let ok = self
            .chains
            .get_mut(&group)
            .is_some_and(|c| c.learn_members(seq, root_commit, members));
        if ok {
            self.relearned = true;
        }
        ok
    }

    /// 이 그룹의 순번 `seq` 키가 있어야 하는데 없다고 적습니다(중계로 받아 오게). 크루 키 그룹 밖에서 확정 기기가 된
    /// 기기는 사슬 끝 갱신을 그 전에 이미 '내 몫 없음'으로 넘겼을 수 있습니다.
    pub(crate) fn mark_stuck(&mut self, group: KeyGroupId, seq: u64) {
        self.groups.entry(group).or_default().stuck = Some(seq);
    }

    /// 받은편지함으로 받은 그룹 R(가입자 몫, ID는 받은 쪽이 확인함).
    pub(crate) fn offer_r(&mut self, group: KeyGroupId, r: Secret32) {
        if self.rs.iter().any(|(g, x)| *g == group && x == &r) {
            return;
        }
        if self.rs.len() >= MAX_RS {
            self.rs.remove(0);
        }
        self.rs.push((group, r));
        self.index_dirty = true;
    }

    /// 작성 기기 `author`가 채널 `channel`에 쓴 메시지의 모르는 뿌리 `root`를 찾아볼지. 지금 상태로 이미 찾지 못한
    /// 뿌리이거나 그 작성 기기의 예산을 다 썼으면 `false`(메시지는 저장해 두고, 새 그룹 상태를 얻으면 다시 봄).
    pub(crate) fn may_look_up_root(
        &mut self,
        author: DeviceId,
        channel: ChannelId,
        root: &ChannelRootId,
        now_ms: u64,
    ) -> bool {
        if self.missed_roots.contains(root) {
            return false;
        }
        if self.root_budget.len() >= MAX_ROOT_BUDGETS {
            self.root_budget.clear();
        }
        self.root_budget
            .entry((author, channel))
            .or_insert_with(|| {
                yangpa_sync::policy::TokenBucket::with_burst(
                    1,
                    ROOT_LOOKUP_BURST * ROOT_LOOKUP_EVERY_S,
                    now_ms,
                )
            })
            .try_consume(ROOT_LOOKUP_EVERY_S, now_ms)
            .is_ok()
    }

    /// 지금 가진 그룹 상태로 찾지 못한 뿌리.
    pub(crate) fn missed_root(&mut self, root: ChannelRootId) {
        if self.missed_roots.len() >= MAX_MISSED_ROOTS {
            self.missed_roots.clear();
        }
        self.missed_roots.insert(root);
    }

    /// 그룹의 종류(중계받은 T·R로 상태를 만들 때): 가진 상태나 받은 갱신의 모양으로 정합니다.
    pub(crate) fn group_kind(&self, space: &SpaceId, group: &KeyGroupId) -> GroupKind {
        if *group == KeyGroupId::crew(space) {
            return GroupKind::Crew;
        }
        if let Some(k) = self
            .groups
            .get(group)
            .and_then(|g| g.states.values().next())
        {
            return k.kind();
        }
        // 자기 트리 없이 크루 덮개 노드로 감싼 갱신이면 덮개 그룹.
        let cover = self
            .group_parts
            .values()
            .any(|(g, p)| g == group && p.depth == 0 && !p.wraps.is_empty());
        if cover {
            GroupKind::Cover
        } else {
            GroupKind::Private
        }
    }

    /// 크루 사슬 끝의 내 상태.
    pub(crate) fn crew_current(&self, space: &SpaceId, resolver: &Resolver) -> Option<&MemberKeys> {
        let kv = resolver.key_view()?;
        if kv.crew.seq == 0 {
            return None;
        }
        self.groups
            .get(&KeyGroupId::crew(space))?
            .state(&kv.crew.root_commit)
    }

    /// 비공개·덮개 그룹에서 지금 보내는 데 쓸 수 있는 사슬 끝의 내 상태([`yangpa_sync::GroupChain::send_tip`]).
    pub(crate) fn group_current(&self, group: &KeyGroupId) -> Option<&MemberKeys> {
        let tip = self.chains.get(group)?.send_tip()?;
        self.groups.get(group)?.state(&tip.root_commit)
    }

    /// 크루 상태 하나(비공개·덮개 그룹 갱신이 기대는 것).
    fn crew_state(&self, space: &SpaceId, root_commit: &[u8; 32]) -> Option<&MemberKeys> {
        self.groups
            .get(&KeyGroupId::crew(space))?
            .state(root_commit)
    }

    /// 모르는 채널 뿌리 ID를 보관한 상태들에서 파생해 찾습니다(크루 그룹과 그 채널의 키 그룹, 공개·비공개 뿌리).
    pub(crate) fn derive_root(
        &self,
        space: &SpaceId,
        channel: &ChannelId,
        group: Option<&KeyGroupId>,
        id: &ChannelRootId,
    ) -> Option<Secret32> {
        let crew = KeyGroupId::crew(space);
        let first: Vec<&KeyGroupId> = std::iter::once(&crew).chain(group).collect();
        // 채널이 그룹을 옮겼을 수 있어, 지금 그룹에 없으면 다른 그룹도 봅니다.
        let rest = self.groups.keys().filter(|g| !first.contains(g));
        for g in first.iter().copied().chain(rest) {
            let Some(grp) = self.groups.get(g) else {
                continue;
            };
            for k in grp.states.values() {
                for root in [k.public_root(channel), k.private_root(channel)] {
                    if keysched::channel_root_id(space, channel, &root) == *id {
                        return Some(root);
                    }
                }
            }
        }
        for (_, r) in &self.rs {
            let root = keysched::public_root(r, space, channel);
            if keysched::channel_root_id(space, channel, &root) == *id {
                return Some(root);
            }
        }
        None
    }

    /// 보관한 모든 상태와 R에서 이 채널의 뿌리 후보(ID, 뿌리)를 파생합니다(뿌리를 기다리는 메시지를 채널마다 한 번에 봄).
    pub(crate) fn candidate_roots(
        &self,
        space: &SpaceId,
        channel: &ChannelId,
    ) -> Vec<(ChannelRootId, Secret32)> {
        let mut out = Vec::new();
        for grp in self.groups.values() {
            for k in grp.states.values() {
                for root in [k.public_root(channel), k.private_root(channel)] {
                    out.push((keysched::channel_root_id(space, channel, &root), root));
                }
            }
        }
        for (_, r) in &self.rs {
            let root = keysched::public_root(r, space, channel);
            out.push((keysched::channel_root_id(space, channel, &root), root));
        }
        out
    }

    /// 키 그룹 스트림의 `KeyUpdate`(기준점 상태로 권한을 확인해 저장한 것). 열 수 있으면 그룹 사슬에 넣습니다.
    pub(crate) fn on_group_event(
        &mut self,
        node: &Node,
        space: &SpaceId,
        resolver: &Resolver,
        ev: &SignedEvent,
        group: KeyGroupId,
    ) {
        let id = ev.id();
        let h = &ev.header;
        let (Some(bytes), KeyRef::State(kid)) = (ev.body.as_deref(), h.key) else {
            return;
        };
        let key = match keys::state_key(node, space, &kid) {
            Ok(Some(k)) => k,
            Ok(None) => {
                let n: usize = self.waiting_key.values().map(Vec::len).sum();
                if n < MAX_WAITING {
                    let v = self.waiting_key.entry(kid).or_default();
                    if !v.contains(&id) {
                        v.push(id);
                    }
                }
                return;
            }
            Err(e) => {
                debug!(error = %e, "키 그룹 갱신의 상태 키를 읽지 못함");
                return;
            }
        };
        let p = match yangpa_crypto::decrypt_body(&key, h, bytes) {
            Ok(EventBody::KeyUpdate(p)) => p,
            _ => {
                debug!(event = %id.short(), "키 그룹 갱신을 열지 못함");
                return;
            }
        };
        if p.group != group {
            return;
        }
        self.insert_group_part(space, resolver, id, h, p);
    }

    fn insert_group_part(
        &mut self,
        space: &SpaceId,
        resolver: &Resolver,
        id: EventId,
        h: &EventHeader,
        p: KeyUpdatePayload,
    ) {
        let group = p.group;
        self.apply_heads(space, resolver);
        let chain = self
            .chains
            .entry(group)
            .or_insert_with(|| GroupChain::new(*space, group));
        match chain.insert(id, h, &p, resolver) {
            Ok(_) => {
                self.group_parts.insert(id, (group, p));
            }
            Err(AuthError::AnchorUnknown) => {
                if self.waiting_anchor.len() < MAX_WAITING
                    && !self.waiting_anchor.iter().any(|(x, _, _)| *x == id)
                {
                    self.waiting_anchor.push((id, h.clone(), p));
                }
            }
            Err(e) => debug!(event = %id.short(), reason = %e, "키 그룹 갱신을 사슬에 넣지 않음"),
        }
    }

    /// 상태 키를 기다리던 키 그룹 이벤트(그 키가 생김).
    pub(crate) fn take_waiting(&mut self, kids: &[StateKeyId]) -> Vec<EventId> {
        let mut out = Vec::new();
        for k in kids {
            out.extend(self.waiting_key.remove(k).unwrap_or_default());
        }
        out
    }

    /// 기준점을 기다리던 조각을 다시 넣고 모든 사슬을 다시 판정합니다(상태가 바뀌었거나 해소기가 더 알게 됨).
    fn rejudge(&mut self, space: &SpaceId, resolver: &Resolver) {
        for (id, h, p) in std::mem::take(&mut self.waiting_anchor) {
            self.insert_group_part(space, resolver, id, &h, p);
        }
        for chain in self.chains.values_mut() {
            chain.rejudge(resolver);
        }
    }

    /// 받아들인 크루 갱신과 그룹 사슬을 적용합니다.
    pub(crate) fn drain(
        &mut self,
        node: &Node,
        space: &SpaceId,
        resolver: &Resolver,
        rejudge: bool,
    ) -> Result<Drained> {
        self.ensure_loaded(node, space);
        let mut out = Drained::default();
        // 기준점이 경계 아래라고 아는 기다리는 조각은 버립니다(그 상태를 셀 수 없어 늦은 이벤트, 영영 기다리지 않게).
        self.waiting_anchor.retain(|(_, h, _)| {
            !h.state_anchor
                .iter()
                .any(|a| crate::checkpoint::is_below(node, space, a))
        });
        let base = self.heads_of;
        self.apply_heads(space, resolver);
        if rejudge || base != self.heads_of || std::mem::take(&mut self.relearned) {
            self.rejudge(space, resolver);
        }
        out.learn = self
            .chains
            .iter()
            .filter(|(_, c)| {
                let t = c.tip();
                t.seq > 0 && !t.members_known
            })
            .map(|(g, _)| *g)
            .collect();
        for (g, chain) in self.chains.iter_mut() {
            for eq in chain.take_equivocations() {
                keep_equivocation(node, space, g, &eq);
            }
        }
        let retry = std::mem::take(&mut self.retry);
        self.drain_crew(node, space, resolver, retry, &mut out)?;
        let groups: Vec<KeyGroupId> = self.chains.keys().copied().collect();
        for g in groups {
            self.drain_group(node, space, &g, retry, &mut out)?;
        }
        self.persist(node, space)?;
        Ok(out)
    }

    fn drain_crew(
        &mut self,
        node: &Node,
        space: &SpaceId,
        resolver: &Resolver,
        retry: bool,
        out: &mut Drained,
    ) -> Result<()> {
        let Some(kv) = resolver.key_view() else {
            return Ok(());
        };
        let crew = KeyGroupId::crew(space);
        if retry && let Some(g) = self.groups.get_mut(&crew) {
            g.stuck = None;
        }
        let me = node.me()?;
        let evs = resolver.key_events();
        let (mut start, last) = self.crew_scan;
        if start > evs.len() || (start > 0 && Some(evs[start - 1]) != last) {
            start = 0;
        }
        let pending: Vec<EventId> = evs[start..]
            .iter()
            .filter(|id| !self.crew_done.contains(id))
            .copied()
            .collect();
        for id in pending {
            let Some(key) = self.crew_part_of.get(&id).copied() else {
                // 크루 갱신이 아니거나(예비 기기 지정) 이미 처리하고 조각을 버린 것.
                self.crew_done.insert(id);
                continue;
            };
            if key.1.saturating_add(KEEP_SEQS) < kv.crew.seq {
                // 보관 폭보다 오래된 갱신(다시 시작할 때 다시 연 것): 적용해 봐야 버릴 상태입니다.
                self.crew_done.insert(id);
                continue;
            }
            let accepted = resolver.crew_parts(key.1);
            let Some(merged) = self.merge_crew(&key, &id, accepted.as_deref()) else {
                self.crew_done.insert(id);
                continue;
            };
            let last = merged.seq == kv.crew.seq && merged.root_commit == kv.crew.root_commit;
            // 크루 키 그룹 밖에서 확정 기기가 된 기기도 사슬 끝 키를 가져야 합니다(멤버에게 중계받아 전체 재키를 냄).
            let committer = resolver
                .state()
                .is_some_and(|s| s.committer == me.device_id());
            let should_have = last && (kv.crew.members.contains(&me.device_id()) || committer);
            match self.apply(node, space, &crew, GroupKind::Crew, &merged, None, &id)? {
                Applied::New(k) => {
                    self.got(node, space, &crew, &k, out)?;
                    self.crew_done.insert(id);
                }
                Applied::Have | Applied::Removed => {
                    self.crew_done.insert(id);
                }
                Applied::NoKey if should_have => {
                    // 묶음(받은편지함·조각 블롭)이 아직 오지 않았을 수 있습니다. 다시 볼 수 있게 남겨 둡니다.
                    let g = self.groups.entry(crew).or_default();
                    if g.stuck != Some(merged.seq) {
                        g.stuck = Some(merged.seq);
                        out.stuck.push(crew);
                        if let Some(b) = merged.shards {
                            out.fetch.push((merged.seq, b));
                        }
                    }
                    if !retry {
                        break;
                    }
                }
                Applied::NoKey | Applied::Failed => {
                    self.crew_done.insert(id);
                }
            }
        }
        // 처리한 갱신의 조각은 버리고(다시 시작하면 다시 열어 모음), 오래된 갈래 조각도 버립니다.
        let floor = kv.crew.seq.saturating_sub(KEEP_SEQS);
        let done = &self.crew_done;
        let part_of = &mut self.crew_part_of;
        self.crew_parts.retain(|k, parts| {
            let keep = k.1 > floor && !parts.iter().any(|(id, _)| done.contains(id));
            if !keep {
                for (id, _) in parts.iter() {
                    part_of.remove(id);
                }
            }
            keep
        });
        let covered = self.covered.get(&crew).copied();
        if let Some(g) = self.groups.get_mut(&crew) {
            g.prune(kv.crew.seq, &kv.crew.root_commit, covered);
        }
        // 앞에서부터 모두 처리한 만큼 표시를 옮기고 그 갱신들의 기억은 버립니다(다시 처리해도 뿌리 약속으로 걸러짐).
        let mut end = start;
        while end < evs.len() && self.crew_done.contains(&evs[end]) {
            self.crew_done.remove(&evs[end]);
            end += 1;
        }
        self.crew_scan = (end, end.checked_sub(1).map(|i| evs[i]));
        if self.crew_done.len() > 4 * 4096 {
            self.crew_done.clear();
        }
        Ok(())
    }

    /// 마지막 조각 `last`로 끝난 크루 갱신의 조각을 합칩니다. 해소기가 받아들인 조각 목록(`accepted`, 그 순번의 이긴
    /// 갱신)이 `last`로 끝나면 정확히 그 조각들을 합칩니다: 같은 자리에 다른 조각이 있어도(확정 기기가 두 판을 냄)
    /// 노드마다 도착 순서와 상관없이 같은 갱신을 얻습니다. 목록이 없으면(보관 폭 밖) 같은 자리에 다른 조각이 있을 때
    /// 합치지 않습니다.
    fn merge_crew(
        &self,
        key: &CrewPartKey,
        last: &EventId,
        accepted: Option<&[EventId]>,
    ) -> Option<KeyUpdatePayload> {
        let parts = self.crew_parts.get(key)?;
        if let Some(ids) = accepted.filter(|ids| ids.last() == Some(last)) {
            let all: Option<Vec<KeyUpdatePayload>> = ids
                .iter()
                .map(|id| parts.iter().find(|(x, _)| x == id).map(|(_, p)| p.clone()))
                .collect();
            return merged(all?);
        }
        let (_, lp) = parts.iter().find(|(id, _)| id == last)?;
        let n = usize::from(lp.parts);
        let mut chosen: Vec<Option<KeyUpdatePayload>> = vec![None; n];
        for (_, p) in parts {
            let i = usize::from(p.part);
            if i >= n || !p.same_update(lp) {
                continue;
            }
            match &chosen[i] {
                None => chosen[i] = Some(p.clone()),
                Some(q) if q != p => return None,
                Some(_) => {}
            }
        }
        let all: Option<Vec<KeyUpdatePayload>> = chosen.into_iter().collect();
        merged(all?)
    }

    fn drain_group(
        &mut self,
        node: &Node,
        space: &SpaceId,
        group: &KeyGroupId,
        retry: bool,
        out: &mut Drained,
    ) -> Result<()> {
        let Some(chain) = self.chains.get(group) else {
            return Ok(());
        };
        let me = node.me()?;
        // 쓸 수 있는 앞부분만 적용하므로 그 끝을 기준으로 봅니다(없으면 아직 쓸 수 있는 갱신이 없음).
        let tip = chain.send_tip().unwrap_or_else(|| chain.tip());
        let heads: Vec<EventId> = chain.winners().to_vec();
        let parts_of: Vec<(EventId, Option<Vec<EventId>>)> =
            heads.iter().map(|h| (*h, chain.parts(h))).collect();
        if retry && let Some(g) = self.groups.get_mut(group) {
            g.stuck = None;
        }
        for (head, parts) in parts_of {
            if self
                .groups
                .get(group)
                .is_some_and(|g| g.done.contains(&head))
            {
                continue;
            }
            let Some(parts) = parts else { continue };
            let payloads: Option<Vec<KeyUpdatePayload>> = parts
                .iter()
                .map(|id| self.group_parts.get(id).map(|(_, p)| p.clone()))
                .collect();
            let Some(merged) = payloads.and_then(merged) else {
                continue;
            };
            let last = merged.seq == tip.seq;
            let should_have = last && tip.members.contains(&me.device_id());
            let crew_ref = merged.crew.map(|c| c.root_commit);
            let applied = {
                let crew_state = crew_ref
                    .as_ref()
                    .and_then(|rc| self.crew_state(space, rc))
                    .cloned();
                self.apply(
                    node,
                    space,
                    group,
                    GroupKind::Private,
                    &merged,
                    crew_state.as_ref(),
                    &head,
                )?
            };
            match applied {
                Applied::New(k) => {
                    self.got(node, space, group, &k, out)?;
                    self.groups.entry(*group).or_default().done.insert(head);
                }
                Applied::Have | Applied::Removed => {
                    self.groups.entry(*group).or_default().done.insert(head);
                }
                Applied::NoKey if should_have => {
                    let g = self.groups.entry(*group).or_default();
                    if g.stuck != Some(merged.seq) {
                        g.stuck = Some(merged.seq);
                        out.stuck.push(*group);
                    }
                }
                Applied::NoKey | Applied::Failed => {
                    self.groups.entry(*group).or_default().done.insert(head);
                }
            }
        }
        // 적용한 갱신의 조각(상태는 뿌리 약속마다 보관하므로 다시 합칠 일이 없음), 사슬에 남지 않은 조각, 오래된 상태를
        // 버립니다. 이기지 못한 갈래의 조각은 갈래가 바뀔 때 쓰므로 보관 폭 안에서 남깁니다.
        let done = self.groups.get(group).map(|g| &g.done);
        let applied: HashSet<EventId> = heads
            .iter()
            .filter(|h| done.is_some_and(|d| d.contains(h)))
            .filter_map(|h| self.chains.get(group).and_then(|c| c.parts(h)))
            .flatten()
            .collect();
        let live: HashSet<EventId> = heads
            .iter()
            .filter_map(|h| self.chains.get(group).and_then(|c| c.parts(h)))
            .flatten()
            .collect();
        let floor = tip.seq.saturating_sub(KEEP_SEQS);
        self.group_parts.retain(|id, (g, p)| {
            g != group
                || (!applied.contains(id)
                    && (live.contains(id) || p.seq > tip.seq || p.seq > floor))
        });
        let covered = self.covered.get(group).copied();
        if let Some(g) = self.groups.get_mut(group) {
            g.prune(tip.seq, &tip.root_commit, covered);
            if g.done.len() > 4096 {
                g.done.clear();
            }
        }
        Ok(())
    }

    /// 새 상태를 얻음: 상태 키를 저장하고 보관합니다.
    fn got(
        &mut self,
        node: &Node,
        space: &SpaceId,
        group: &KeyGroupId,
        k: &MemberKeys,
        out: &mut Drained,
    ) -> Result<()> {
        let s = k.state_key();
        let (id, new) = keys::put_derived_state_key(node, space, group, &s, k.seq())?;
        if new {
            out.state_keys.push(id);
        }
        out.progressed = true;
        self.missed_roots.clear();
        let g = self.groups.entry(*group).or_default();
        g.stuck = None;
        g.states.insert(*k.root_commit(), k.clone());
        self.touch(*group);
        Ok(())
    }

    /// 합친 갱신 하나를 이 기기에 적용해 봅니다.
    #[allow(clippy::too_many_arguments)]
    fn apply(
        &mut self,
        node: &Node,
        space: &SpaceId,
        group: &KeyGroupId,
        kind: GroupKind,
        p: &KeyUpdatePayload,
        crew: Option<&MemberKeys>,
        head: &EventId,
    ) -> Result<Applied> {
        let me = node.me()?;
        let device = me.device_id();
        let grp = self.groups.entry(*group).or_default();
        if grp.states.contains_key(&p.root_commit) {
            return Ok(Applied::Have);
        }
        if p.kind.removed().binary_search(&device).is_ok() {
            return Ok(Applied::Removed);
        }
        let bundle = p.bundle_root.and_then(|root| {
            self.bundles
                .iter()
                .find(|(g, s, b)| {
                    g == group && *s == p.seq && b.device == device && b.verify(&root)
                })
                .map(|(_, _, b)| b.clone())
        });
        let cx = MemberCtx {
            box_keys: Some(&me.device.boxkey),
            bundle: bundle.as_ref(),
            crew,
        };
        let joined_me = p.kind.joined().binary_search(&device).is_ok();
        let grp = self.groups.get(group).expect("위에서 만듦");
        let res = match grp.states.get(&p.prev_root_commit) {
            Some(prev) if !joined_me && p.seq > 1 => prev.apply(p, &cx),
            _ => MemberKeys::join(*space, *group, kind, device, p, &cx),
        };
        match res {
            Ok(k) => Ok(Applied::New(Box::new(k))),
            Err(KeyUpdateError::Removed) => Ok(Applied::Removed),
            Err(KeyUpdateError::NoKeyForMe)
            | Err(KeyUpdateError::BadBundle)
            | Err(KeyUpdateError::CrewMismatch { .. })
            | Err(KeyUpdateError::NeedCrewTree) => Ok(Applied::NoKey),
            // 앞 상태는 `prev_root_commit`으로 골랐으므로 사슬 불일치도 갱신 자체가 앞뒤가 맞지 않는다는 뜻입니다.
            Err(
                e @ (KeyUpdateError::RootCommitMismatch
                | KeyUpdateError::RMismatch
                | KeyUpdateError::ChainMismatch),
            ) => {
                keep_evidence(node, space, group, p.seq, head, &e);
                Ok(Applied::Failed)
            }
            Err(e) => {
                debug!(space = %space.short(), seq = p.seq, reason = %e, "키 그룹 갱신을 적용하지 못함");
                Ok(Applied::NoKey)
            }
        }
    }
}

/// 조각을 합치고 합친 갱신의 한도·구조를 봅니다(해소기·그룹 사슬이 이미 봤지만 적용 전에 한 번 더, 싸게).
fn merged(parts: Vec<KeyUpdatePayload>) -> Option<KeyUpdatePayload> {
    let p = KeyUpdatePayload::merge(parts).ok()?;
    p.validate_merged().ok()?;
    Some(p)
}

enum Applied {
    New(Box<MemberKeys>),
    Have,
    Removed,
    NoKey,
    Failed,
}

/// 증거 한 줄(키·내용 없이 위치만).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Evidence {
    pub(crate) group: KeyGroupId,
    pub(crate) seq: u64,
    pub(crate) head: EventId,
    pub(crate) reason: String,
}

fn evidence_setting(space: &SpaceId) -> String {
    format!("kg.evidence.{space}")
}

/// 이 공간에 남은 키 불일치 증거.
pub(crate) fn evidence(node: &Node, space: &SpaceId) -> Vec<Evidence> {
    node.store
        .get_setting(&evidence_setting(space))
        .ok()
        .flatten()
        .unwrap_or_default()
}

fn keep_evidence(
    node: &Node,
    space: &SpaceId,
    group: &KeyGroupId,
    seq: u64,
    head: &EventId,
    e: &KeyUpdateError,
) {
    warn!(
        space = %space.short(),
        seq,
        event = %head.short(),
        reason = %e,
        "키 갱신의 뿌리 약속이 맞지 않음: 확정 기기가 기기마다 다른 키를 줬을 수 있음"
    );
    put_evidence(node, space, group, seq, head, e.to_string());
}

/// 같은 작성 기기가 같은 그룹·순번에 서로 다른 갱신 두 개를 냄(갈래를 나눠 멤버마다 다른 키를 줄 수 있음).
fn keep_equivocation(
    node: &Node,
    space: &SpaceId,
    group: &KeyGroupId,
    eq: &yangpa_sync::Equivocation,
) {
    warn!(
        space = %space.short(),
        seq = eq.seq,
        device = %eq.author.short(),
        first = %eq.heads[0].short(),
        second = %eq.heads[1].short(),
        "확정 기기가 같은 순번에 다른 키 갱신 두 개를 냄"
    );
    put_evidence(
        node,
        space,
        group,
        eq.seq,
        &eq.heads[0],
        format!("같은 작성 기기의 두 갱신(다른 쪽 {})", eq.heads[1].short()),
    );
}

fn put_evidence(
    node: &Node,
    space: &SpaceId,
    group: &KeyGroupId,
    seq: u64,
    head: &EventId,
    reason: String,
) {
    let mut list = evidence(node, space);
    if list.iter().any(|x| x.head == *head) {
        return;
    }
    if list.len() >= MAX_EVIDENCE {
        list.remove(0);
    }
    list.push(Evidence {
        group: *group,
        seq,
        head: *head,
        reason,
    });
    let _ = node.store.put_setting(&evidence_setting(space), &list);
    node.notify
        .publish(yangpa_rpc::api::Notification::SpaceUpdated { space: *space });
}

/// 채널이 쓰는 키 그룹(`None`이면 크루 그룹): 보기 덮어쓰기가 없고 @everyone이 보는 채널은 크루 키로, 아니면 그
/// 보기 규칙의 비공개 그룹으로(`SpaceState::channel_key_group`, 받는 쪽 검사·확정 기기와 같은 구조 규칙).
pub(crate) fn channel_group(
    _node: &Node,
    space: &SpaceId,
    state: &std::sync::Arc<SpaceState>,
    channel: &ChannelId,
) -> Option<KeyGroupId> {
    let ch = state.channels.get(channel)?;
    state.channel_key_group(space, ch)
}

/// 채널에 보낼 때 쓰는 뿌리를 그 그룹 상태에서 고릅니다(기록 공개면 R의 공개 뿌리, 아니면 T의 비공개 뿌리).
pub(crate) fn root_for(
    k: &MemberKeys,
    history: HistoryVisibility,
    channel: &ChannelId,
) -> Secret32 {
    match history {
        HistoryVisibility::Public => k.public_root(channel),
        HistoryVisibility::Private => k.private_root(channel),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 모르는 뿌리 찾기는 (작성 기기, 채널)마다 한 번에 16번, 그 뒤 10초에 한 번입니다. 지금 상태로 찾지 못한 뿌리는
    /// 예산이 남은 다른 작성 기기가 붙여 와도 다시 파생하지 않습니다.
    #[test]
    fn root_lookups_are_budgeted_per_author_and_channel() {
        let mut s = MemberState::default();
        let (x, y) = (DeviceId([1; 32]), DeviceId([2; 32]));
        let ch = ChannelId([1; 16]);
        let root = |n: u8| ChannelRootId([n; 16]);
        let t0 = 1_000_000;
        for n in 0..ROOT_LOOKUP_BURST as u8 {
            assert!(s.may_look_up_root(x, ch, &root(n), t0), "{n}번째 찾기");
        }
        assert!(
            !s.may_look_up_root(x, ch, &root(200), t0),
            "예산을 넘어 찾음"
        );
        // 다른 채널·다른 작성 기기의 예산은 따로입니다.
        assert!(s.may_look_up_root(x, ChannelId([2; 16]), &root(200), t0));
        assert!(s.may_look_up_root(y, ch, &root(201), t0));
        // 시간이 지나면 한 번 더.
        let later = t0 + ROOT_LOOKUP_EVERY_S * 1000;
        assert!(s.may_look_up_root(x, ch, &root(202), later));
        assert!(!s.may_look_up_root(x, ch, &root(203), later));

        s.missed_root(root(150));
        assert!(
            !s.may_look_up_root(y, ch, &root(150), later),
            "찾지 못한 뿌리를 다시 찾음"
        );
        assert!(s.may_look_up_root(y, ch, &root(151), later));
    }
}
