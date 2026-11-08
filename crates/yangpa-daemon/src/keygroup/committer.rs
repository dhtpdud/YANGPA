//! 확정 기기 쪽: 키 그룹 표(`KeyTree`·`CoverGroup`)를 들고 상태에 맞춰 `KeyUpdate`를 냅니다.
//!
//! 한 바퀴(공간 적용 잠금 안):
//! 1. 표를 해소기의 크루 사슬 끝·그룹 사슬 끝과 맞춰 봅니다. 맞지 않거나 표가 없으면(넘겨받지 못함) 공개 끝으로
//!    이어받고(`KeyTree::recover`) 전체 재키부터 합니다.
//! 2. 크루: 빚(자격을 잃은 멤버 기기)을 빼고 새 활성 기기를 넣습니다. 제거가 가입과 같은 `Rotate`에 들어가 제거가 먼저
//!    반영됩니다. 해소기가 요구하거나(표 보유 기기 자격 상실), 표를 잃었거나, 일주일이 지났으면 `FullRekey`.
//! 3. 비공개·덮개 그룹: 멤버 전원이 볼 수 있는 채널은 크루 키를 쓰므로 그룹을 두지 않고, 나머지는 볼 수 있는 규칙
//!    (`KeyGroupId::private`)마다 그룹 하나. 크루가 트리이고 "이 몇 기기만 빼고 크루 전체"면 덮개 그룹
//!    ([`COVER_MAX_EXCLUDED`] 이하), 아니면 자기 트리(256대 이하는 직접 인코딩).
//! 4. 가입한 사용자마다 받은편지함 키 묶음 하나: 최근 상태 키(가입 갱신을 열 키 포함), 크루 R, 트리 가입 기기의 몫 묶음.
//! 5. 채널이 그룹을 떠났거나 기록 공개를 껐으면 옛 그룹의 R만 새로(`RRefresh`), 기록 공개를 켰으면 비공개 시절 뿌리를
//!    그 채널 그룹의 상태 키로 다시 봉인해 채널 스트림에 둡니다.
//!
//! 갱신은 표의 복제본에 하고, 해소기(크루)·그룹 사슬이 받아들인 것을 확인한 뒤에만 표를 바꿉니다(발행 실패·거부면 옛
//! 표로 돌아감). 표는 잠긴 키 저장소의 비밀값으로 둡니다.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};
use yangpa_core::body::{EventBody, KeyShareBody, Secret32, SharedKey, state_key_id};
use yangpa_core::device::BoxPublicKey;
use yangpa_core::event::{MAX_PARENTS, Scope, SignedEvent, StreamId};
use yangpa_core::ids::{ChannelId, DeviceId, EventId, KeyGroupId, SpaceId, UserId};
use yangpa_core::keyupdate::{KeyUpdatePayload, PART_TARGET_LEN};
use yangpa_core::model::{HistoryVisibility, SpaceState};
use yangpa_core::time::Timestamp;
use yangpa_crypto::keytree::{
    BoxKeys, ChainTip, CommitCtx, CoverGroup, KeyTree, KeyUpdateError, KeyUpdateOutput,
    decode_table, encode_table,
};
use yangpa_crypto::{GroupKind, keysched};
use yangpa_sync::KeyView;

use crate::author::{self, KeyChoice};
use crate::error::{Error, Result};
use crate::ingest::Locked;
use crate::node::Node;
use crate::{devkeys, keys};

/// 덮개 그룹으로 둘 "빠지는 기기" 수 상한. 덮개 갱신은 빠지는 기기 k대에 대해 감싸기 약 k·log(n/k)개라 k가 작을 때
/// 자기 트리(가입 때 잎마다 감싸기, 4만 대면 2.2MB)보다 훨씬 작습니다. 32대면 크루 4만 대에서 감싸기 수백 개입니다.
pub(crate) const COVER_MAX_EXCLUDED: usize = 32;
/// 덮개 그룹이 이보다 많이 빼게 되면 자기 트리로 바꿉니다(경계에서 오가지 않게 넉넉히).
const COVER_SWITCH_EXCLUDED: usize = 4 * COVER_MAX_EXCLUDED;
/// 크루 전체 재키 주기.
pub(crate) const FULL_REKEY_EVERY: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// 주기 R 회전: 그룹의 마지막 새 R 뒤로 이만큼 지났거나 이만큼 가입했으면 R만 새로 냅니다(한 번 뚫린 기기나 나간
/// 멤버가 기록 공개 채널의 새 뿌리를 따라가는 기간을 주간 전체 재키보다 짧게).
pub(crate) const R_REFRESH_EVERY: Duration = Duration::from_secs(24 * 60 * 60);
pub(crate) const R_REFRESH_JOINS: u64 = 1000;
/// 다시 봉인하는 뿌리를 한 키 묶음에 담는 수.
const ROOTS_PER_SHARE: usize = 512;

/// 확정 기기의 키 그룹 표. 그룹마다 `Arc`라 바퀴마다 캐시에서 꺼내는 복제가 싸고, 갱신하는 그룹만 새로 만듭니다.
#[derive(Clone)]
pub(crate) struct Table {
    pub(crate) crew: Arc<KeyTree>,
    pub(crate) privates: BTreeMap<KeyGroupId, Arc<KeyTree>>,
    pub(crate) covers: BTreeMap<KeyGroupId, Arc<CoverGroup>>,
    /// 그룹(크루 포함) → 지금 상태를 담은 저장 칸(0·1). 다음 상태는 내기 전에 다른 칸에 써 둡니다.
    slots: BTreeMap<KeyGroupId, u8>,
    /// 아직 저장하지 못한 그룹(바퀴 끝에 다시 씀).
    dirty: BTreeSet<KeyGroupId>,
    /// 표에서 빠져 저장 칸을 지울 그룹.
    gone: BTreeSet<KeyGroupId>,
    /// 그룹 목록 행을 다시 써야 함.
    index_dirty: bool,
}

/// 저장 칸 하나에 든 그룹 상태.
enum Stored {
    Tree(KeyTree),
    Cover(CoverGroup),
}

impl Stored {
    fn tip(&self) -> (u64, [u8; 32]) {
        match self {
            Stored::Tree(t) => (t.seq(), *t.root_commit()),
            Stored::Cover(c) => (c.seq(), *c.root_commit()),
        }
    }
}

impl Table {
    fn new(space: SpaceId) -> Self {
        Table {
            crew: Arc::new(KeyTree::crew(space)),
            privates: BTreeMap::new(),
            covers: BTreeMap::new(),
            slots: BTreeMap::new(),
            dirty: BTreeSet::new(),
            gone: BTreeSet::new(),
            index_dirty: true,
        }
    }

    /// 표 전체의 인코딩(넘기기와 같은 모양). 시험 도구가 표를 비교할 때 씁니다.
    pub(crate) fn encode(&self) -> zeroize::Zeroizing<Vec<u8>> {
        encode_table(&self.trees(), &self.cover_list())
    }

    pub(crate) fn decode(space: &SpaceId, bytes: &[u8], me: &DeviceId) -> Option<Self> {
        let (trees, covers) = decode_table(bytes, me).ok()?;
        Self::from_parts(space, trees, covers)
    }

    pub(crate) fn from_parts(
        space: &SpaceId,
        trees: Vec<KeyTree>,
        covers: Vec<CoverGroup>,
    ) -> Option<Self> {
        let mut crew = None;
        let mut privates = BTreeMap::new();
        for t in trees {
            if t.space() != space {
                return None;
            }
            match t.kind() {
                GroupKind::Crew => crew = Some(t),
                _ => {
                    privates.insert(*t.group(), Arc::new(t));
                }
            }
        }
        if covers.iter().any(|c| c.space() != space) {
            return None;
        }
        let mut t = Table {
            crew: Arc::new(crew?),
            privates,
            covers: covers
                .into_iter()
                .map(|c| (*c.group(), Arc::new(c)))
                .collect(),
            slots: BTreeMap::new(),
            dirty: BTreeSet::new(),
            gone: BTreeSet::new(),
            index_dirty: true,
        };
        t.dirty = t.groups().collect();
        Some(t)
    }

    pub(crate) fn trees(&self) -> Vec<&KeyTree> {
        let mut v = vec![&*self.crew];
        v.extend(self.privates.values().map(|t| &**t));
        v
    }

    pub(crate) fn cover_list(&self) -> Vec<&CoverGroup> {
        self.covers.values().map(|c| &**c).collect()
    }

    /// 크루와 비공개·덮개 그룹 ID.
    fn groups(&self) -> impl Iterator<Item = KeyGroupId> + '_ {
        std::iter::once(*self.crew.group())
            .chain(self.privates.keys().copied())
            .chain(self.covers.keys().copied())
    }

    /// 그룹 하나만 담은 인코딩(저장 칸 하나).
    fn encode_group(&self, g: &KeyGroupId) -> Option<zeroize::Zeroizing<Vec<u8>>> {
        if g == self.crew.group() {
            return Some(encode_table(&[&self.crew], &[]));
        }
        if let Some(t) = self.privates.get(g) {
            return Some(encode_table(&[&**t], &[]));
        }
        self.covers.get(g).map(|c| encode_table(&[], &[&**c]))
    }

    /// 비공개·덮개 그룹을 표에서 뺍니다(저장 칸은 바퀴 끝에 지움).
    fn remove_group(&mut self, g: &KeyGroupId) {
        if self.privates.remove(g).is_some() | self.covers.remove(g).is_some() {
            self.slots.remove(g);
            self.dirty.remove(g);
            self.gone.insert(*g);
            self.index_dirty = true;
        }
    }

    fn put_tree(&mut self, t: KeyTree) {
        let g = *t.group();
        if t.kind() == GroupKind::Crew {
            self.crew = Arc::new(t);
            return;
        }
        if self.covers.remove(&g).is_none() && !self.privates.contains_key(&g) {
            self.index_dirty = true;
        }
        self.gone.remove(&g);
        self.privates.insert(g, Arc::new(t));
    }

    fn put_cover(&mut self, c: CoverGroup) {
        let g = *c.group();
        if self.privates.remove(&g).is_none() && !self.covers.contains_key(&g) {
            self.index_dirty = true;
        }
        self.gone.remove(&g);
        self.covers.insert(g, Arc::new(c));
    }

    /// 내기 전에 써 둔 칸(`staged`)을 지금 칸으로 삼습니다. 써 두지 못했으면 바퀴 끝에 다시 씁니다.
    fn settle(&mut self, g: KeyGroupId, staged: Option<u8>) {
        match staged {
            Some(s) => {
                self.slots.insert(g, s);
                self.dirty.remove(&g);
            }
            None => {
                self.dirty.insert(g);
            }
        }
    }
}

/// 표와 함께 두는 공개 메타데이터(설정 `kg.meta.<공간>`).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Meta {
    /// 마지막 크루 전체 재키(유닉스 ms).
    last_full_ms: u64,
    /// 채널 → (쓰는 그룹, 기록 공개). 그룹을 떠나거나 기록 공개가 바뀐 채널을 찾습니다.
    channels: BTreeMap<ChannelId, (Option<KeyGroupId>, bool)>,
    /// R을 새로 내야 하는 그룹(크루는 크루 그룹 ID). 새 R을 낸 갱신이 받아들여질 때까지 남깁니다(같은 바퀴의 가입,
    /// 실패, 기다림에 묻혀 사라지지 않게).
    #[serde(default)]
    pending_refresh: BTreeSet<KeyGroupId>,
    /// 그룹마다 마지막 새 R 뒤로 지난 시간·가입 수(주기 R 회전).
    #[serde(default)]
    r_age: BTreeMap<KeyGroupId, RAge>,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
struct RAge {
    since_ms: u64,
    joins: u64,
}

impl Meta {
    /// 새 R을 낸 갱신이 받아들여짐.
    fn fresh_r(&mut self, g: KeyGroupId, now_ms: u64) {
        self.pending_refresh.remove(&g);
        self.r_age.insert(
            g,
            RAge {
                since_ms: now_ms,
                joins: 0,
            },
        );
    }

    fn joined(&mut self, g: KeyGroupId, n: usize, now_ms: u64) {
        self.r_age
            .entry(g)
            .or_insert(RAge {
                since_ms: now_ms,
                joins: 0,
            })
            .joins += n as u64;
    }

    /// 주기 R 회전이 된 그룹을 `pending_refresh`에 넣습니다.
    fn mark_r_due(&mut self, groups: impl Iterator<Item = KeyGroupId>, now_ms: u64) {
        for g in groups {
            let age = *self.r_age.entry(g).or_insert(RAge {
                since_ms: now_ms,
                joins: 0,
            });
            if now_ms.saturating_sub(age.since_ms) >= R_REFRESH_EVERY.as_millis() as u64
                || age.joins >= R_REFRESH_JOINS
            {
                self.pending_refresh.insert(g);
            }
        }
    }
}

fn meta_setting(space: &SpaceId) -> String {
    format!("kg.meta.{space}")
}

fn load_meta(node: &Node, space: &SpaceId) -> Meta {
    node.store
        .get_setting(&meta_setting(space))
        .ok()
        .flatten()
        .unwrap_or_default()
}

/// 표가 그룹 사슬보다 앞설 때 따라잡을 멤버 수와 다시 돌 때까지.
const LAG_CATCH_UP_PEERS: usize = 3;
const LAG_RETRY: Duration = Duration::from_secs(2);
/// 그룹 사슬이 받아들이지 않을 갱신을 내지 않고 다시 볼 때까지(처음, 상한).
const GROUP_RETRY_MIN: Duration = Duration::from_secs(1);
const GROUP_RETRY_MAX: Duration = Duration::from_secs(30);

/// 넘길 메타데이터 상한(채널 지도가 커도 이 안).
const MAX_META_LEN: usize = 256 * 1024;

/// 표를 넘길 때 함께 보낼 메타데이터.
pub(crate) fn meta_bytes(node: &Node, space: &SpaceId) -> Vec<u8> {
    postcard::to_stdvec(&load_meta(node, space)).unwrap_or_default()
}

/// 넘겨받은 표의 메타데이터를 씁니다. 시각은 지금보다 뒤일 수 없게 자릅니다(미래 시각으로 주간 전체 재키·주기 R
/// 회전을 한없이 미루지 못하게). R을 새로 낼 그룹은 내 것과 합칩니다.
pub(crate) fn adopt_meta(node: &Node, space: &SpaceId, bytes: &[u8]) {
    if bytes.is_empty() || bytes.len() > MAX_META_LEN {
        return;
    }
    let Ok(mut meta) = postcard::from_bytes::<Meta>(bytes) else {
        return;
    };
    let now = Timestamp::now().as_millis();
    meta.last_full_ms = meta.last_full_ms.min(now);
    for a in meta.r_age.values_mut() {
        a.since_ms = a.since_ms.min(now);
    }
    let mine = load_meta(node, space);
    meta.pending_refresh.extend(mine.pending_refresh);
    let _ = node.store.put_setting(&meta_setting(space), &meta);
}

pub(crate) fn forget_meta(node: &Node, space: &SpaceId) {
    let _ = node.store.delete_setting(&meta_setting(space));
}

/// 지금 확정 기기 표(캐시, 없으면 잠긴 저장소에서).
pub(crate) fn load_table(node: &Node, space: &SpaceId) -> Option<Table> {
    if let Some(t) = node.keygroup.tables.lock().get(space) {
        return Some(t.clone());
    }
    let t = load_stored(node, space)?;
    node.keygroup.tables.lock().insert(*space, t.clone());
    Some(t)
}

/// 저장 칸에서 표를 읽습니다. 그룹마다 두 칸 가운데 사슬에 있는 상태(크루는 해소기의 크루 사슬 끝, 그룹은 이긴
/// 사슬)를, 둘 다 아니면 순번이 큰 쪽을 고릅니다. 내기 전에 써 둔 칸이 거부되었거나 내지 못하고 멈췄으면 그 칸은
/// 사슬에 없으므로 앞 상태로 돌아갑니다.
fn load_stored(node: &Node, space: &SpaceId) -> Option<Table> {
    let me = node.me().ok()?.device_id();
    let crew_g = KeyGroupId::crew(space);
    let index = keys::load_table_index(node, space).ok()??;
    let read = |g: &KeyGroupId| -> Vec<(u8, Stored)> {
        (0..2u8)
            .filter_map(|slot| {
                let bytes = keys::load_group_row(node, space, g, slot).ok()??;
                let (mut trees, mut covers) = decode_table(&bytes, &me).ok()?;
                let item = match (trees.pop(), covers.pop()) {
                    (Some(t), None) if trees.is_empty() && t.group() == g => Stored::Tree(t),
                    (None, Some(c)) if covers.is_empty() && c.group() == g => Stored::Cover(c),
                    _ => return None,
                };
                Some((slot, item))
            })
            .collect()
    };
    let mut rows: Vec<(KeyGroupId, Vec<(u8, Stored)>)> = std::iter::once(crew_g)
        .chain(index.iter().copied().filter(|g| *g != crew_g))
        .map(|g| {
            let r = read(&g);
            (g, r)
        })
        .collect();
    let core = node.spaces.read().get(space).map(|rt| rt.core.clone());
    let mut table = Table::new(*space);
    table.index_dirty = false;
    let guard = core.as_ref().map(|c| c.lock());
    let kv = guard.as_ref().and_then(|c| c.resolver.key_view());
    for (g, cands) in rows.iter_mut() {
        let on_chain = |(seq, rc): (u64, [u8; 32])| -> bool {
            if *g == crew_g {
                return kv
                    .as_ref()
                    .is_some_and(|k| k.crew.seq == seq && k.crew.root_commit == rc);
            }
            guard
                .as_ref()
                .and_then(|c| c.keys.chains.get(g))
                .is_some_and(|ch| ch.winning_at(seq, &rc).is_some())
        };
        let best = cands
            .iter()
            .enumerate()
            .max_by_key(|(_, (_, item))| {
                let tip = item.tip();
                (on_chain(tip), tip.0)
            })
            .map(|(i, _)| i);
        let Some(i) = best else {
            if *g == crew_g {
                return None;
            }
            // 칸을 읽지 못한 그룹은 빼고(사슬 끝에서 이어받음) 목록을 다시 씁니다.
            table.gone.insert(*g);
            table.index_dirty = true;
            continue;
        };
        let (slot, item) = cands.swap_remove(i);
        match item {
            Stored::Tree(t) => table.put_tree(t),
            Stored::Cover(c) => table.put_cover(c),
        }
        table.slots.insert(*g, slot);
    }
    drop(guard);
    table.index_dirty = table.index_dirty || !table.gone.is_empty();
    Some(table)
}

/// 그룹 하나의 다음 상태를 지금 칸이 아닌 칸에 써 둡니다(발행 전, 갱신이 받아들여지면 그 칸이 지금 칸). 쓰지
/// 못하면 `None`: 그래도 냅니다(제거 회전을 저장소 오류로 막지 않음). 표는 메모리에 있고 바퀴 끝에 다시 씁니다.
fn stage(node: &Node, space: &SpaceId, table: &Table, g: &KeyGroupId, bytes: &[u8]) -> Option<u8> {
    let slot = table.slots.get(g).map_or(0, |s| 1 - s);
    match keys::save_group_row(node, space, g, slot, bytes) {
        Ok(()) => Some(slot),
        Err(e) => {
            warn!(space = %space.short(), error = %e, "키 그룹 상태를 미리 저장하지 못함(메모리 표로 계속)");
            None
        }
    }
}

/// 바퀴 끝: 아직 저장하지 못한 그룹만 쓰고, 빠진 그룹의 칸을 지우고, 목록이 바뀌었으면 목록 행을 씁니다. `fresh`면
/// (넘겨받은 표) 다른 칸을 지워 옛 상태가 다시 골라지지 않게 합니다. 저장소 오류는 표를 되돌리지 않습니다.
fn flush(node: &Node, space: &SpaceId, table: &mut Table, fresh: bool) {
    for g in table.dirty.clone() {
        let Some(bytes) = table.encode_group(&g) else {
            table.dirty.remove(&g);
            continue;
        };
        let slot = table.slots.get(&g).map_or(0, |s| 1 - s);
        match keys::save_group_row(node, space, &g, slot, &bytes) {
            Ok(()) => {
                table.slots.insert(g, slot);
                table.dirty.remove(&g);
                if fresh {
                    let _ = keys::delete_group_row(node, space, &g, 1 - slot);
                }
            }
            Err(e) => {
                warn!(space = %space.short(), error = %e, "키 그룹 상태를 저장하지 못함(다음 바퀴에 다시)");
            }
        }
    }
    if table.index_dirty {
        let groups: Vec<KeyGroupId> = table.groups().skip(1).collect();
        match keys::save_table_index(node, space, &groups) {
            Ok(()) => table.index_dirty = false,
            Err(e) => warn!(space = %space.short(), error = %e, "키 그룹 목록을 저장하지 못함"),
        }
    }
    for g in std::mem::take(&mut table.gone) {
        for slot in 0..2 {
            if keys::delete_group_row(node, space, &g, slot).is_err() {
                table.gone.insert(g);
            }
        }
    }
}

/// 메모리 표를 둡니다(이것이 기준이고 저장은 그 사본).
fn keep(node: &Node, space: &SpaceId, t: &Table) {
    node.keygroup.tables.lock().insert(*space, t.clone());
}

/// 넘겨받은 표를 통째로 씁니다(모든 그룹, 다른 칸은 지움).
pub(crate) fn save_table(node: &Node, space: &SpaceId, t: &Table) {
    let mut t = t.clone();
    t.dirty = t.groups().collect();
    t.index_dirty = true;
    flush(node, space, &mut t, true);
    keep(node, space, &t);
}

/// 그룹마다 넘겨받은 비공개·덮개 그룹을 지금 표에 합칩니다. 표가 없거나 크루와 맞지 않으면(크루 잎 검사) 거절하고,
/// 이미 같거나 뒤 순번을 가졌으면 그대로 둡니다. 합친 그룹 수.
pub(crate) fn merge_groups(
    node: &Node,
    space: &SpaceId,
    trees: Vec<KeyTree>,
    covers: Vec<CoverGroup>,
) -> std::result::Result<usize, &'static str> {
    let mut t = load_table(node, space).ok_or("넘겨받은 크루 표가 없음")?;
    let mut n = 0;
    for p in trees {
        if p.kind() == GroupKind::Crew {
            return Err("크루가 그룹 자리에 있음");
        }
        yangpa_crypto::keytree::check_private_against(&t.crew, &p)
            .map_err(|_| "크루와 맞지 않는 그룹")?;
        let have = t
            .privates
            .get(p.group())
            .map(|x| x.seq())
            .or_else(|| t.covers.get(p.group()).map(|c| c.seq()));
        if have.is_some_and(|s| s >= p.seq()) {
            continue;
        }
        t.dirty.insert(*p.group());
        t.put_tree(p);
        n += 1;
    }
    for c in covers {
        if c.space() != space {
            return Err("다른 공간의 그룹");
        }
        let have = t
            .covers
            .get(c.group())
            .map(|x| x.seq())
            .or_else(|| t.privates.get(c.group()).map(|x| x.seq()));
        if have.is_some_and(|s| s >= c.seq()) {
            continue;
        }
        t.dirty.insert(*c.group());
        t.put_cover(c);
        n += 1;
    }
    flush(node, space, &mut t, true);
    keep(node, space, &t);
    Ok(n)
}

pub(crate) fn drop_table(node: &Node, space: &SpaceId) {
    node.keygroup.tables.lock().remove(space);
    node.keygroup.last_round.lock().remove(space);
    let _ = keys::delete_table_rows(node, space);
}

/// 시험 도구: 저장된 표 행 (그룹, 칸, 바이트). 크루가 맨 앞.
pub(crate) fn stored_rows(node: &Node, space: &SpaceId) -> Vec<(KeyGroupId, u8, usize)> {
    let crew_g = KeyGroupId::crew(space);
    let index = keys::load_table_index(node, space).ok().flatten();
    let groups =
        std::iter::once(crew_g).chain(index.into_iter().flatten().filter(|g| *g != crew_g));
    let mut out = Vec::new();
    for g in groups {
        for slot in 0..2u8 {
            if let Ok(Some(b)) = keys::load_group_row(node, space, &g, slot) {
                out.push((g, slot, b.len()));
            }
        }
    }
    out
}

/// 시험 도구: 크루의 다음 상태(R 새로 하기)를 내지 않고 다른 칸에 써 둡니다(내기 직전에 멈춘 확정 기기). 써 둔 순번.
pub(crate) fn stage_unpublished(node: &Node, space: &SpaceId) -> Option<u64> {
    let table = load_table(node, space)?;
    let mut next = (*table.crew).clone();
    let boxes = Boxes {
        keys: HashMap::new(),
        owners: HashMap::new(),
        lacking: Vec::new(),
    };
    next.refresh_r(&CommitCtx {
        boxes: &boxes,
        crew: None,
    })
    .ok()?;
    stage(
        node,
        space,
        &table,
        &KeyGroupId::crew(space),
        &encode_table(&[&next], &[]),
    )?;
    Some(next.seq())
}

/// 시험용: [`stage_unpublished`]에 더해 메모리 표도 그 다음 상태로 바꿉니다. 낸 크루 갱신이 사슬에 들지 못한(체크포인트
/// 경계 아래로 늦었거나 경쟁에서 진) 확정 기기를 흉내 냅니다(표가 사슬 끝보다 앞섬).
pub(crate) fn stage_ahead(node: &Node, space: &SpaceId) -> Option<u64> {
    let seq = stage_unpublished(node, space)?;
    let mut table = load_table(node, space)?;
    let me = node.me().ok()?.device_id();
    let crew_g = KeyGroupId::crew(space);
    let slot = (0..2u8).find_map(|slot| {
        let bytes = keys::load_group_row(node, space, &crew_g, slot).ok()??;
        let (mut trees, _) = decode_table(&bytes, &me).ok()?;
        trees.pop().filter(|t| t.seq() == seq)
    })?;
    table.crew = Arc::new(slot);
    node.keygroup.tables.lock().insert(*space, table);
    Some(seq)
}

/// 봉인에 쓰는 상자 키와 기기 주인(트리에 넣을 때 한 사용자의 기기를 이웃 칸에 둠).
struct Boxes {
    keys: HashMap<DeviceId, BoxPublicKey>,
    owners: HashMap<DeviceId, UserId>,
    /// 봉인할 기기 가운데 상자 키를 모르는 기기(체크포인트로 들어와 경계 아래 기기 목록 본문을 못 본 확정 기기).
    lacking: Vec<DeviceId>,
}

impl BoxKeys for Boxes {
    fn box_key(&self, device: &DeviceId) -> Option<&BoxPublicKey> {
        self.keys.get(device)
    }

    fn user_of(&self, device: &DeviceId) -> Option<UserId> {
        self.owners.get(device).copied()
    }
}

/// 원하는 비공개·덮개 그룹 하나.
struct Want {
    /// 자격 있는 기기 E(크루 멤버 가운데 그 그룹 채널 하나라도 볼 수 있는 사용자의 기기).
    eligible: BTreeSet<DeviceId>,
}

/// 한 바퀴의 결과.
#[derive(Default)]
pub(crate) struct RoundOut {
    pub(crate) events: Vec<SignedEvent>,
    /// 한 바퀴 더 돌아야 함(크루에서 빠졌다 다시 들어온 기기를 그룹에 다시 넣기 등).
    pub(crate) again: bool,
    /// 넘겨받기를 기다리는 중이라 표 없이 끝냄(이만큼 뒤에 다시).
    pub(crate) retry_after: Option<Duration>,
    /// 잠금 밖에서 멤버에게 상자 키를 먼저 물을 기기(`checkpoint::prefetch_device_keys`). 있으면 이번 바퀴는 갱신을
    /// 내지 않고, 키를 받은 뒤 다시 돕니다.
    pub(crate) fetch_keys: Vec<DeviceId>,
}

/// 확정 기기가 마지막으로 다 끝낸 바퀴: 그때의 상태(들고 있어 다른 상태가 같은 주소를 받지 못함)와 나머지 입력의 지문.
pub(crate) struct RoundMemo {
    state: Arc<SpaceState>,
    key: [u8; 32],
}

type CoreRef = Arc<parking_lot::Mutex<crate::spaces::SpaceCore>>;

/// 바퀴 입력의 지문: 크루 사슬(빚·보유 기기 포함), 모든 그룹 사슬 끝, 시간으로 돌아오는 일(주간 전체 재키·주기 R).
/// 상태는 `RoundMemo::state`로 따로 봅니다.
fn round_key(kv: &KeyView, core: &CoreRef, due: &[KeyGroupId], weekly: bool) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    let c = &kv.crew;
    h.update(&c.seq.to_le_bytes());
    h.update(&c.root_commit);
    h.update(&c.last_rekey.to_le_bytes());
    h.update(&[u8::from(c.rekey_required), u8::from(weekly)]);
    for d in c.debt.iter().chain(c.holders.iter()).chain(c.backup.iter()) {
        h.update(d.as_bytes());
    }
    h.update(&(c.members.len() as u64).to_le_bytes());
    h.update(&(kv.owners.len() as u64).to_le_bytes());
    for g in due {
        h.update(g.as_bytes());
    }
    let core = core.lock();
    for (g, ch) in core.keys.chains.iter() {
        let t = ch.tip();
        h.update(g.as_bytes());
        h.update(&t.seq.to_le_bytes());
        h.update(&t.root_commit);
        h.update(&ch.send_tip().map_or(0, |t| t.seq).to_le_bytes());
    }
    *h.finalize().as_bytes()
}

/// 상자 키를 모르는 기기를 다시 묻기까지의 간격.
const KEY_FETCH_AGAIN: Duration = Duration::from_secs(60);

/// 상자 키를 물을 기기 가운데 [`KEY_FETCH_AGAIN`] 안에 묻지 않은 것만 돌려주고 물은 시각을 적습니다.
fn note_key_fetch(node: &Node, space: &SpaceId, lacking: &[DeviceId]) -> Vec<DeviceId> {
    let mut asked = node.keygroup.key_fetch.lock();
    let m = asked.entry(*space).or_default();
    let now = std::time::Instant::now();
    m.retain(|_, t| now.duration_since(*t) < KEY_FETCH_AGAIN);
    let fresh: Vec<DeviceId> = lacking
        .iter()
        .filter(|d| !m.contains_key(*d))
        .copied()
        .collect();
    for d in &fresh {
        m.insert(*d, now);
    }
    fresh
}

/// 확정 기기 한 바퀴. 적용 잠금 안에서 부릅니다.
pub(crate) fn round_locked(node: &Arc<Node>, l: &Locked<'_>, space: SpaceId) -> Result<RoundOut> {
    l.check(&space)?;
    let mut out = RoundOut::default();
    let me = node.me()?;
    let Some(state) = node.space_state(&space) else {
        return Ok(out);
    };
    if state.committer != me.device_id() || crate::checkpoint::is_unconfirmed(node, &space) {
        // 확인 전 체크포인트 상태로는 키를 내지 않습니다(확인되면 `checkpoint::confirm`이 다시 부름).
        return Ok(out);
    }
    let core = node
        .spaces
        .read()
        .get(&space)
        .map(|rt| rt.core.clone())
        .ok_or_else(|| Error::not_found("모르는 공간"))?;
    let Some(kv) = core.lock().resolver.key_view() else {
        return Ok(out);
    };
    let crew_g = KeyGroupId::crew(&space);
    let urgent_full = node.keygroup.force_full.lock().remove(&space);
    let mut meta = load_meta(node, &space);
    let now_ms = Timestamp::now().as_millis();
    let weekly = kv.crew.seq > 0
        && now_ms.saturating_sub(meta.last_full_ms) >= FULL_REKEY_EVERY.as_millis() as u64;
    let desired = desired_groups(node, &space, &state, &kv);
    meta.mark_r_due(
        std::iter::once(crew_g).chain(desired.wants.keys().copied()),
        now_ms,
    );
    // 0. 지난 바퀴가 다 끝냈고 입력이 그대로면 쉽니다(상태 이벤트마다 표·상자 키·그룹을 다시 계산하지 않게).
    let due: Vec<KeyGroupId> = meta.pending_refresh.iter().copied().collect();
    let key = round_key(&kv, &core, &due, weekly);
    if !urgent_full
        && node
            .keygroup
            .last_round
            .lock()
            .get(&space)
            .is_some_and(|m| Arc::ptr_eq(&m.state, &state) && m.key == key)
    {
        return Ok(out);
    }
    // 다 끝냈는지(아니면 지문을 남기지 않아 다음 기회에 다시 돔).
    let mut settled = true;

    // 1. 표.
    let mut table = match load_table(node, &space) {
        Some(t) => t,
        None if kv.crew.seq == 0 => Table::new(space),
        None => {
            if let Some(wait) = crate::keygroup::handover_wait(node, &space) {
                out.retry_after = Some(wait);
                return Ok(out);
            }
            // 크루 갱신은 지금 크루 상태 키로 암호화합니다. 크루 키 그룹 밖에서 확정 기기가 됐으면(잃은 소유자 기기를 되살린
            // 새 기기 등) 멤버가 사슬 끝 T·R을 중계해 줄 때까지 기다립니다(확정 기기는 받을 자격이 있음, `relay`).
            if keys::current_state_key(node, &space)?.is_none() {
                let mut guard = core.lock();
                let c = &mut *guard;
                c.keys.ensure_loaded(node, &space);
                c.keys.mark_stuck(crew_g, kv.crew.seq);
                crate::keygroup::relay::ask_group(node, space, crew_g, &c.keys, &c.resolver);
                debug!(space = %space.short(), "확정 기기가 크루 사슬 끝 키를 기다림");
                return Ok(out);
            }
            info!(space = %space.short(), "확정 기기 표가 없어 공개 끝으로 이어받고 전체 재키");
            Table::new(space)
        }
    };
    if crew_ahead(&table, &kv) {
        // 표가 크루 사슬 끝보다 앞섬: 내가 낸 크루 갱신이 사슬에 없습니다(체크포인트 경계 아래로 늦은 이벤트가 됨, 또는
        // 경쟁에서 짐). 그 갱신을 그대로 다시 서명해 내면 안 됩니다: 경계 아래 논리 시계·기준점이면 또 늦은 이벤트이고,
        // 그 사이 사슬이 나아갔으면 앞 약속이 맞지 않습니다. 저장 칸에서 사슬 끝과 맞는 앞 상태를 다시 읽어 그 위에서 새
        // 갱신(새 비밀)을 냅니다. 맞는 칸이 없으면 아래에서 공개 끝으로 이어받습니다(전체 재키).
        node.keygroup.tables.lock().remove(&space);
        if let Some(t) = load_stored(node, &space).filter(|t| !crew_ahead(t, &kv)) {
            info!(space = %space.short(), "사슬에 없는 크루 갱신 대신 저장 칸의 앞 상태에서 다시 냄");
            keep(node, &space, &t);
            table = t;
        }
    }
    reconcile_crew(&mut table, &kv, &space);
    let others: Vec<DeviceId> = kv
        .crew
        .holders
        .iter()
        .filter(|d| **d != me.device_id() && !table.crew.holders().contains(*d))
        .copied()
        .collect();
    if !others.is_empty() {
        Arc::make_mut(&mut table.crew).add_holders(others);
        table.dirty.insert(crew_g);
    }

    // 2. 채널 지도: 그룹을 떠나거나 기록 공개를 끈 채널의 옛 그룹은 R을 새로(크루 갱신보다 먼저 봅니다: 같은 바퀴의 가입
    //    기기에게 새로 내기 전 R을 주지 않게), 켠 채널은 옛 뿌리를 다시 봉인. 새로 낼 일은 받아들여질 때까지 메타에 남깁니다.
    let mut reseal: Vec<(ChannelId, Option<KeyGroupId>)> = Vec::new();
    let mut map = BTreeMap::new();
    let public_seen: HashMap<ChannelId, Option<KeyGroupId>> = {
        let mut seen = node.keygroup.public_seen.lock();
        let mine = seen
            .iter()
            .filter(|((s, _), _)| *s == space)
            .map(|((_, c), g)| (*c, *g))
            .collect();
        seen.retain(|(s, _), _| *s != space);
        mine
    };
    for (c, ch) in &state.channels {
        if !ch.uses_group_keys() {
            continue;
        }
        let g = desired.groups_of_channel.get(c).copied().flatten();
        let public = ch.history == HistoryVisibility::Public;
        match meta.channels.get(c) {
            Some((old_g, old_public)) => {
                if *old_g != g || (*old_public && !public) {
                    meta.pending_refresh.insert(old_g.unwrap_or(crew_g));
                }
                if !*old_public && public && *old_g == g {
                    reseal.push((*c, g));
                }
            }
            // 처음 보는 기록 비공개 채널인데 지난 바퀴 뒤 적용에서 공개로 보였음: 묶음 창 안에서 만들고 공개로 쓰다 끈
            // 채널입니다. 그때 쓴 뿌리는 그때 그룹의 지금 R에서 나오므로 R부터 새로 합니다(같은 바퀴의 가입 기기가 옛 R을
            // 받지 않게).
            None => {
                if !public && let Some(old_g) = public_seen.get(c) {
                    meta.pending_refresh.insert(old_g.unwrap_or(crew_g));
                }
            }
        }
        map.insert(*c, (g, public));
    }
    meta.channels = map;
    // 더 이상 원하지 않는 그룹의 R은 새로 낼 까닭이 없습니다(사슬은 남고, 다시 원하면 새 T부터).
    meta.pending_refresh
        .retain(|g| *g == crew_g || desired.wants.contains_key(g));

    // 3. 크루.
    let removed: Vec<DeviceId> = kv.crew.debt.iter().copied().collect();
    let joined_all: Vec<DeviceId> = kv
        .owners
        .keys()
        .filter(|d| !kv.crew.members.contains(*d))
        .copied()
        .collect();
    let holder_out = removed.iter().any(|d| kv.crew.holders.contains(d));
    let full =
        kv.crew.rekey_required || table.crew.is_recovering() || weekly || holder_out || urgent_full;
    let boxes = collect_boxes(node, &state, &kv, &table, &joined_all, &desired, full);
    // 체크포인트로 들어온 확정 기기는 경계 아래 가입·기기 목록 본문을 본 적이 없어 봉인할 상자 키를 모를 수 있습니다:
    // 이번 바퀴는 내지 않고 잠금 밖에서 멤버에게 먼저 묻습니다(키 약속·체크포인트 기기 트리와 맞는 답만 받음). 같은
    // 기기는 [`KEY_FETCH_AGAIN`]에 한 번만 물어, 끝내 못 받으면 그 기기 없이 냅니다(가입은 다음에, 전체 재키 몫은
    // 중계로).
    if !boxes.lacking.is_empty() && crate::checkpoint::floor(node, &space) > 0 {
        let fresh = note_key_fetch(node, &space, &boxes.lacking);
        if !fresh.is_empty() {
            out.fetch_keys = fresh;
            return Ok(out);
        }
    }
    let joined: Vec<DeviceId> = joined_all
        .iter()
        .filter(|d| boxes.keys.contains_key(d))
        .copied()
        .collect();
    if joined.len() < joined_all.len() {
        // 상자 키를 아직 모르는 가입 기기: 키가 오면 다시 돕니다.
        settled = false;
    }
    let (head_extra, mut joined) = {
        let n = joined.len();
        let (extra, reach) = head_parents(node, &core, &space, joined)?;
        if reach.len() < n {
            if reach.is_empty() {
                // 닿는 가입이 하나도 없으면 이번에 낼 갱신이 머리를 합치지 못할 수 있어 쉬었다가 다시 봅니다.
                out.retry_after = Some(Duration::from_secs(5));
            } else {
                // 이번 갱신이 고른 머리를 합치므로 다음 바퀴에는 남은 가입이 닿습니다.
                out.again = true;
            }
        }
        (extra, reach)
    };
    let mut crew_last_part: Option<EventId> = None;
    let crew_refresh = meta.pending_refresh.contains(&crew_g) && kv.crew.seq > 0;
    if crew_refresh && !full && removed.is_empty() && table.crew.seq() > 0 {
        // 이번 크루 갱신이 R을 그대로 두는 가입(Advance)이면 R부터 새로 내야 가입 기기가 옛 R을 받지 않습니다.
        match crew_step(
            node,
            l,
            &space,
            &core,
            &mut table,
            &boxes,
            &[],
            &mut out,
            |t, cx| t.refresh_r(cx),
        ) {
            Ok(Some(done)) => {
                crew_last_part = Some(done.last);
                meta.fresh_r(crew_g, now_ms);
            }
            Ok(None) => {}
            Err(()) => {
                // 옛 R을 줄 수는 없으니 가입은 다음 바퀴로.
                joined.clear();
                settled = false;
                out.retry_after = Some(out.retry_after.map_or(LAG_RETRY, |d| d.min(LAG_RETRY)));
            }
        }
    }
    if full || !removed.is_empty() || !joined.is_empty() {
        let res = crew_step(
            node,
            l,
            &space,
            &core,
            &mut table,
            &boxes,
            &head_extra,
            &mut out,
            |t, cx| {
                if full {
                    return t.full_rekey(&removed, &joined, cx);
                }
                let before = t.clone();
                match t.update(&removed, &joined, cx) {
                    Err(
                        KeyUpdateError::HolderNeedsFullRekey(_) | KeyUpdateError::NeedFullRekey,
                    ) => {
                        *t = before;
                        t.full_rekey(&removed, &joined, cx)
                    }
                    r => r,
                }
            },
        );
        match res {
            Ok(Some(done)) => {
                crew_last_part = Some(done.last);
                if done.full || meta.last_full_ms == 0 {
                    meta.last_full_ms = now_ms;
                }
                if done.new_r {
                    meta.fresh_r(crew_g, now_ms);
                } else {
                    meta.joined(crew_g, joined.len(), now_ms);
                }
                let all = done.full && !done.sharded;
                let base = core.lock().resolver.base();
                share_bundles(
                    node,
                    l,
                    &space,
                    &state,
                    &kv,
                    base.as_ref(),
                    &table,
                    &joined,
                    &done.bundles,
                    all,
                    &mut out,
                )?;
            }
            Ok(None) => {}
            Err(()) => {
                save_meta(node, &space, &meta);
                return Ok(out);
            }
        }
    } else if crew_refresh && meta.pending_refresh.contains(&crew_g) {
        // 가입·제거 없이 R만 새로(위에서 내지 못했으면 다음 바퀴에).
        settled = false;
    }
    if table.crew.seq() == 0 {
        // 아직 크루 키가 없으면(상자 키를 모르는 등) 그룹도 만들 수 없습니다.
        save_meta(node, &space, &meta);
        return Ok(out);
    }
    // 그룹 갱신은 크루 갱신 뒤의 크루 상태 키로 암호화하고, 기준점에 크루 사슬 끝(마지막 조각)을 넣어 그 크루 상태에
    // 기댄다는 것을 받는 쪽이 셀 수 있게 합니다. 그 조각이 바탕 체크포인트의 조상이면 넣지 않습니다(바탕 상태에 이미
    // 들어 있고, 그런 기준점만 두면 늦은 갱신: `state_anchor`). 기준점과 논리 시계는 한 번 정해 미리 보기와 실제 발행에
    // 같이 씁니다(발행하는 논리 시계는 그 뒤로 커질 뿐이라 경계 판정이 같음).
    let crew_s = keysched::state_key(table.crew.r());
    let kv_now = core.lock().resolver.key_view().unwrap_or(kv.clone());
    let anchor_hint: Vec<EventId> = kv_now
        .crew
        .tip_part
        .or(crew_last_part)
        .into_iter()
        .collect();
    let anchor = author::state_anchor(node, &space, &anchor_hint)?;
    let probe_lamport = yangpa_sync::dag::next_lamport(node.store.max_lamport(&space)?);

    // 4. 비공개·덮개 그룹.
    let crew_members: BTreeSet<DeviceId> = kv_now.crew.members.iter().copied().collect();
    let tips: HashMap<KeyGroupId, yangpa_sync::GroupTip> = {
        let c = core.lock();
        desired
            .wants
            .keys()
            .chain(table.privates.keys())
            .chain(table.covers.keys())
            .filter_map(|g| {
                let ch = c.keys.chains.get(g)?;
                let tip = ch.tip();
                // 확정 기기가 바뀌면 옛 확정 기기가 낸 갱신은 새 확정 기기의 갱신이 뒤에 붙기 전까지 쓸 수 없습니다.
                // 넘겨받은 표의 상태가 이긴 사슬에 있으면 그 끝에 이어 그 갱신들도 살립니다(아니면 같은 순번을 새로 내
                // 갈래에서 지거나 표를 버리고 전체 재키를 해야 함).
                let held = table
                    .privates
                    .get(g)
                    .map(|t| (t.seq(), *t.root_commit()))
                    .or_else(|| table.covers.get(g).map(|c| (c.seq(), *c.root_commit())));
                let tip = match held {
                    Some((s, rc)) if s > tip.seq => ch.winning_at(s, &rc).unwrap_or(tip),
                    _ => tip,
                };
                Some((*g, tip))
            })
            .collect()
    };
    let mut lagging = false;
    for (g, want) in &desired.wants {
        // 그룹 사슬의 자격은 기준점(이 크루 갱신 뒤)의 크루 멤버 가운데서 셉니다.
        let want = Want {
            eligible: want.eligible.intersection(&crew_members).copied().collect(),
        };
        let tip = tips.get(g).cloned();
        let tip_seq = tip.as_ref().map_or(0, |t| t.seq);
        let tip_members: BTreeSet<DeviceId> = match &tip {
            // 체크포인트 키 머리에서 세운 바닥이라 멤버를 모름: 그 순번의 표가 있으면 표의 기기로 사슬이 멤버를 배우게
            // 합니다(머리의 멤버 요약과 맞을 때만 받아들임). 없거나 맞지 않으면 멤버를 배울 때까지 기다립니다(아무도
            // 다음 갱신을 판정하지 않음).
            Some(t) if !t.members_known => {
                let held: Option<BTreeSet<DeviceId>> = table
                    .privates
                    .get(g)
                    .filter(|x| x.seq() == t.seq && *x.root_commit() == t.root_commit)
                    .map(|x| x.devices().into_iter().collect())
                    .or_else(|| {
                        table
                            .covers
                            .get(g)
                            .filter(|x| x.seq() == t.seq && *x.root_commit() == t.root_commit)
                            .map(|x| x.members().clone())
                    });
                let learned = held.is_some_and(|m| {
                    let list: Vec<DeviceId> = m.iter().copied().collect();
                    core.lock()
                        .keys
                        .learn_members(*g, t.seq, &t.root_commit, &list)
                });
                if learned {
                    // 사슬이 기다리던 갱신을 판정한 뒤(다음 정리) 그 끝에서 다시 봅니다.
                    out.again = true;
                } else {
                    debug!(space = %space.short(), "그룹 사슬 끝 멤버를 몰라 배울 때까지 미룸");
                    lagging = true;
                }
                continue;
            }
            Some(t) => t.members.iter().copied().collect(),
            None => BTreeSet::new(),
        };
        let tip_crew = tip.as_ref().map_or(0, |t| t.crew_seq);
        // 공개 사슬로 보아 이 그룹의 마지막 새 T가 크루 전체 재키보다 앞서면(표 보유 기기를 뺀 재키) 새 T가 먼저입니다.
        // 넘겨받은 표의 판 정보가 아니라 사슬로 정합니다.
        let behind_rekey = tip
            .as_ref()
            .is_some_and(|t| t.seq > 0 && kv_now.crew.last_rekey > t.fresh_crew);
        // 표가 내 그룹 사슬보다 앞서면(넘겨받은 표인데 그 그룹 스트림을 아직 받지 못함) 지금 내는 갱신은 사슬 끝을 몰라
        // 거부되거나 갈래를 만듭니다. 사슬을 따라잡은 뒤 다시 돌고, `key_handover_wait`가 지나도 못 받으면 사슬
        // 끝에서 다시 시작합니다(아래 recover → 전체 재키).
        let table_seq = table
            .privates
            .get(g)
            .map(|t| t.seq())
            .or_else(|| table.covers.get(g).map(|c| c.seq()));
        if table_seq.is_none()
            && tip_seq > 0
            && node
                .keygroup
                .received
                .lock()
                .get(&space)
                .is_some_and(|t| t.elapsed() < node.cfg.key_handover_wait)
        {
            // 크루 표를 막 넘겨받았고 이 그룹은 아직 오지 않음(그룹마다 따로 넘김): 사슬 끝에서 새로 시작하지 않고 기다립니다.
            lagging = true;
            continue;
        }
        {
            let mut lag = node.keygroup.lag_since.lock();
            if table_seq.is_some_and(|s| s > tip_seq) {
                let since = *lag.entry((space, *g)).or_insert_with(Instant::now);
                if since.elapsed() < node.cfg.key_handover_wait {
                    lagging = true;
                    continue;
                }
                debug!(space = %space.short(), "넘겨받은 그룹 상태의 사슬을 받지 못해 사슬 끝에서 다시 시작");
            }
            lag.remove(&(space, *g));
        }
        // 그 사이 크루에서 빠졌다 다시 들어온 기기는 한 번 빼야 합니다(다음 바퀴에 다시 넣음).
        let rejoined: BTreeSet<DeviceId> = tip_members
            .iter()
            .filter(|d| kv_now.crew.joined_at.get(*d).is_some_and(|j| *j > tip_crew))
            .copied()
            .collect();
        if !rejoined.is_empty() {
            out.again = true;
        }
        let excluded_now = crew_members.difference(&want.eligible).count();
        let cover = match (table.covers.contains_key(g), table.privates.contains_key(g)) {
            (true, _) => excluded_now <= COVER_SWITCH_EXCLUDED,
            (false, true) => false,
            (false, false) => {
                tip_seq == 0 && table.crew.is_tree() && excluded_now <= COVER_MAX_EXCLUDED
            }
        };
        let refresh_r = meta.pending_refresh.contains(g) && tip_seq > 0;
        let cx = CommitCtx {
            boxes: &boxes,
            crew: Some(&table.crew),
        };
        let res = if cover {
            let mut c = match table.covers.get(g) {
                Some(c)
                    if c.seq() == tip_seq
                        && tip
                            .as_ref()
                            .is_none_or(|t| *c.root_commit() == t.root_commit) =>
                {
                    (**c).clone()
                }
                _ if tip_seq == 0 => CoverGroup::new(space, *g),
                _ => CoverGroup::recover(space, *g, chain_tip(tip.as_ref(), 0), &tip_members),
            };
            if behind_rekey {
                c.mark_stale();
            }
            let mut excluded: BTreeSet<DeviceId> =
                crew_members.difference(&want.eligible).copied().collect();
            excluded.extend(rejoined.iter().copied());
            if let Some(t) = &tip
                && let Some(h) = t.head
            {
                c.set_last_event(h);
            }
            // 크루 전체 재키 뒤에는 새 T가 먼저입니다(R 새로 하기는 그다음 바퀴).
            let r = if refresh_r && !behind_rekey {
                c.refresh_r(&excluded, &cx).map(Some)
            } else {
                c.update(&excluded, &cx)
            };
            match r {
                Ok(Some(o)) => Some((o, Slot::Cover(Box::new(c)))),
                Ok(None) => None,
                Err(e) => {
                    note_group_error(node, &e, &space, &mut out);
                    settled = false;
                    None
                }
            }
        } else {
            let mut t = match table.privates.get(g) {
                Some(t)
                    if t.seq() == tip_seq
                        && tip
                            .as_ref()
                            .is_none_or(|x| *t.root_commit() == x.root_commit) =>
                {
                    (**t).clone()
                }
                _ if tip_seq == 0 => KeyTree::private(space, *g),
                _ => {
                    let devices: Vec<DeviceId> = tip_members.iter().copied().collect();
                    KeyTree::recover(
                        space,
                        *g,
                        GroupKind::Private,
                        chain_tip(tip.as_ref(), 0),
                        &devices,
                    )
                }
            };
            if behind_rekey {
                t.mark_stale();
            }
            if let Some(x) = &tip
                && let Some(h) = x.head
            {
                t.set_last_event(h);
            }
            let removed_g: Vec<DeviceId> = tip_members
                .iter()
                .filter(|d| !want.eligible.contains(d) || rejoined.contains(d))
                .copied()
                .collect();
            let joined_g: Vec<DeviceId> = want
                .eligible
                .iter()
                .filter(|d| !tip_members.contains(d))
                .copied()
                .collect();
            let r = if t.is_recovering() {
                t.full_rekey(&removed_g, &joined_g, &cx)
            } else if refresh_r && removed_g.is_empty() && !behind_rekey {
                // R을 새로 내야 하는데 가입만 있으면(Advance는 R을 그대로 둠) R부터 내고 가입은 다음 바퀴에 넣습니다.
                if !joined_g.is_empty() {
                    out.again = true;
                }
                t.refresh_r(&cx)
            } else {
                t.update(&removed_g, &joined_g, &cx)
            };
            match r {
                Ok(o) => Some((o, Slot::Private(Box::new(t)))),
                Err(KeyUpdateError::NothingToDo) => None,
                Err(e) => {
                    note_group_error(node, &e, &space, &mut out);
                    settled = false;
                    None
                }
            }
        };
        let Some((o, slot)) = res else { continue };
        // 그룹 사슬이 받아들일지 먼저 봅니다. 받아들이지 않을 갱신을 내면 멤버마다 갈래 자리만 차지하고 다시 낼 일도
        // 없으므로, 내지 않고 늘려 가는 대기 뒤에 다시 돕니다.
        let verdict = {
            let c = core.lock();
            let fresh;
            let chain = match c.keys.chains.get(g) {
                Some(ch) => ch,
                None => {
                    fresh = yangpa_sync::GroupChain::new(space, *g);
                    &fresh
                }
            };
            chain.would_accept(
                &me.user(),
                &me.device_id(),
                &anchor,
                probe_lamport,
                &o.payload,
                &c.resolver,
            )
        };
        if let Err(e) = verdict {
            let wait = {
                let mut r = node.keygroup.group_retry.lock();
                let w = r.entry((space, *g)).or_insert(GROUP_RETRY_MIN / 2);
                *w = (*w * 2).clamp(GROUP_RETRY_MIN, GROUP_RETRY_MAX);
                *w
            };
            debug!(space = %space.short(), reason = %e, wait_ms = wait.as_millis() as u64, "그룹 사슬이 받아들이지 않을 키 갱신이라 미룸");
            out.retry_after = Some(out.retry_after.map_or(wait, |d| d.min(wait)));
            settled = false;
            continue;
        }
        node.keygroup.group_retry.lock().remove(&(space, *g));
        let new_r = o.payload.kind.new_r();
        let joins = o.payload.kind.joined().len();
        let staged = {
            let bytes = match &slot {
                Slot::Cover(c) => encode_table(&[], &[&**c]),
                Slot::Private(t) => encode_table(&[&**t], &[]),
            };
            stage(node, &space, &table, g, &bytes)
        };
        match publish_group(node, l, &space, &core, g, o, &crew_s, &anchor, &mut out) {
            Ok(head) => {
                match slot {
                    Slot::Cover(mut c) => {
                        c.set_last_event(head);
                        table.put_cover(*c);
                    }
                    Slot::Private(mut t) => {
                        t.set_last_event(head);
                        table.put_tree(*t);
                    }
                }
                table.settle(*g, staged);
                keep(node, &space, &table);
                if new_r {
                    meta.fresh_r(*g, now_ms);
                } else {
                    meta.joined(*g, joins, now_ms);
                }
            }
            Err(e) => {
                // 미리 보기와 달리 사슬이 받아들이지 않음: 같은 갱신을 바퀴마다 다시 내지 않게 늘려 가는 대기 뒤에 봅니다.
                settled = false;
                let wait = {
                    let mut r = node.keygroup.group_retry.lock();
                    let w = r.entry((space, *g)).or_insert(GROUP_RETRY_MIN / 2);
                    *w = (*w * 2).clamp(GROUP_RETRY_MIN, GROUP_RETRY_MAX);
                    *w
                };
                out.retry_after = Some(out.retry_after.map_or(wait, |d| d.min(wait)));
                debug!(space = %space.short(), error = %e, "그룹 키 갱신을 내지 못함");
            }
        }
    }
    if lagging {
        // 그 그룹 스트림을 가진 멤버(넘겨준 옛 확정 기기 등)와 따라잡고 잠시 뒤 다시 돕니다.
        let peers: Vec<_> = node
            .peers
            .authed_peers()
            .into_iter()
            .filter(|(_, u, d)| {
                *d != me.device_id() && state.is_member(u) && state.device_belongs(u, d)
            })
            .map(|(p, _, _)| p)
            .take(LAG_CATCH_UP_PEERS)
            .collect();
        for p in peers {
            crate::sync_service::spawn_catch_up(node, space, p);
        }
        out.retry_after = Some(out.retry_after.map_or(LAG_RETRY, |d| d.min(LAG_RETRY)));
    }
    // 더 이상 원하지 않는 그룹(채널이 모두 사라졌거나 모두가 볼 수 있게 됨)은 표에서 뺍니다. 사슬은 그대로 남고,
    // 다시 원하게 되면 그 끝에서 이어받아 새 T부터 냅니다.
    let unwanted: Vec<KeyGroupId> = table
        .privates
        .keys()
        .chain(table.covers.keys())
        .filter(|g| !desired.wants.contains_key(g))
        .copied()
        .collect();
    for g in &unwanted {
        table.remove_group(g);
    }

    // 5. 기록 공개를 켠 채널의 비공개 시절 뿌리를 다시 봉인.
    for (c, g) in reseal {
        if let Err(e) = reseal_roots(node, l, &space, &core, &c, g.as_ref(), &mut out) {
            settled = false;
            debug!(space = %space.short(), error = %e, "옛 뿌리 다시 봉인 실패");
        }
    }

    save_meta(node, &space, &meta);
    flush(node, &space, &mut table, false);
    keep(node, &space, &table);
    let mut memo = node.keygroup.last_round.lock();
    if settled && !out.again && out.retry_after.is_none() && out.events.is_empty() {
        // 낸 것이 없고 기다릴 것도 없으면 지문을 남깁니다(낸 바퀴 뒤에는 사슬이 바뀌어 어차피 다시 돎).
        memo.insert(
            space,
            RoundMemo {
                state: state.clone(),
                key,
            },
        );
    } else {
        memo.remove(&space);
    }
    Ok(out)
}

fn save_meta(node: &Node, space: &SpaceId, meta: &Meta) {
    if let Err(e) = node.store.put_setting(&meta_setting(space), meta) {
        warn!(space = %space.short(), error = %e, "키 그룹 메타데이터를 저장하지 못함");
    }
}

/// 받아들여진 크루 갱신.
struct CrewDone {
    last: EventId,
    bundles: Vec<yangpa_crypto::DeviceBundle>,
    sharded: bool,
    new_r: bool,
    full: bool,
}

/// 크루 갱신 하나: 표 크루의 복제본으로 만들고(`make`), 다음 상태를 미리 저장한 뒤 내고, 해소기가 받아들이면 표에
/// 넣습니다. 할 일이 없으면 `Ok(None)`, 만들거나 내지 못하면 `Err(())`(표는 그대로).
#[allow(clippy::too_many_arguments)]
fn crew_step(
    node: &Arc<Node>,
    l: &Locked<'_>,
    space: &SpaceId,
    core: &CoreRef,
    table: &mut Table,
    boxes: &Boxes,
    head_extra: &[Head],
    out: &mut RoundOut,
    make: impl FnOnce(
        &mut KeyTree,
        &CommitCtx<'_>,
    ) -> std::result::Result<KeyUpdateOutput, KeyUpdateError>,
) -> std::result::Result<Option<CrewDone>, ()> {
    let crew_g = KeyGroupId::crew(space);
    let mut next = (*table.crew).clone();
    if let Some(h) = core.lock().resolver.key_view().and_then(|k| k.crew.head) {
        next.set_last_event(h);
    }
    let cx = CommitCtx { boxes, crew: None };
    let o = match make(&mut next, &cx) {
        Ok(o) => o,
        Err(KeyUpdateError::NothingToDo) => return Ok(None),
        Err(e) => {
            warn!(space = %space.short(), error = %e, "크루 키 갱신을 만들지 못함");
            return Err(());
        }
    };
    let full = o.payload.kind.is_full_rekey();
    let new_r = o.payload.kind.new_r();
    let staged = stage(node, space, table, &crew_g, &encode_table(&[&next], &[]));
    match publish_crew(node, l, space, core, &mut next, o, boxes, head_extra, out) {
        Ok((last, bundles, sharded)) => {
            table.crew = Arc::new(next);
            table.settle(crew_g, staged);
            // 받아들여진 크루 갱신은 바로 메모리 표에 둡니다(뒤 단계가 실패해도 이 갱신을 잃고 전체 재키하지 않게).
            keep(node, space, table);
            Ok(Some(CrewDone {
                last,
                bundles,
                sharded,
                new_r,
                full,
            }))
        }
        Err(e) => {
            debug!(space = %space.short(), error = %e, "크루 키 갱신을 내지 못함");
            Err(())
        }
    }
}

enum Slot {
    Cover(Box<CoverGroup>),
    Private(Box<KeyTree>),
}

fn note_group_error(node: &Node, e: &KeyUpdateError, space: &SpaceId, out: &mut RoundOut) {
    match e {
        KeyUpdateError::HolderNeedsFullRekey(_) => {
            // 크루 전체 재키가 먼저입니다(다음 바퀴에 크루를 전체 재키).
            node.keygroup.force_full.lock().insert(*space);
            out.again = true;
        }
        _ => debug!(space = %space.short(), error = %e, "그룹 키 갱신을 만들지 못함"),
    }
}

fn chain_tip(tip: Option<&yangpa_sync::GroupTip>, depth: u8) -> ChainTip {
    ChainTip {
        seq: tip.map_or(0, |t| t.seq),
        root_commit: tip.map_or([0; 32], |t| t.root_commit),
        depth,
        generation: 0,
    }
}

/// 표의 크루 상태가 해소기의 크루 사슬 끝보다 앞서거나 같은 순번의 다른 갈래인지(내가 낸 갱신이 사슬에 없음).
fn crew_ahead(table: &Table, kv: &KeyView) -> bool {
    let c = &table.crew;
    c.seq() > kv.crew.seq
        || (c.seq() == kv.crew.seq && c.seq() > 0 && *c.root_commit() != kv.crew.root_commit)
}

/// 표를 해소기의 크루 사슬 끝과 맞춥니다. 다르면(다른 확정 기기가 이어 냈거나 저장한 표가 오래됨) 공개 끝으로
/// 이어받습니다(다음 작업은 전체 재키).
fn reconcile_crew(table: &mut Table, kv: &KeyView, space: &SpaceId) {
    let c = &table.crew;
    if c.seq() == kv.crew.seq && (kv.crew.seq == 0 || *c.root_commit() == kv.crew.root_commit) {
        return;
    }
    if kv.crew.seq == 0 {
        let groups: Vec<KeyGroupId> = table.groups().skip(1).collect();
        for g in &groups {
            table.remove_group(g);
        }
        table.crew = Arc::new(KeyTree::crew(*space));
        return;
    }
    let devices: Vec<DeviceId> = kv.crew.members.iter().copied().collect();
    let tip = ChainTip {
        seq: kv.crew.seq,
        root_commit: kv.crew.root_commit,
        depth: kv.crew.depth,
        generation: kv.crew.generation,
    };
    table.crew = Arc::new(KeyTree::recover(
        *space,
        KeyGroupId::crew(space),
        GroupKind::Crew,
        tip,
        &devices,
    ));
    // 그룹 표도 크루 잎에 기대므로 크루와 함께 버립니다(그룹 사슬 끝에서 다시 이어받음).
    let groups: Vec<KeyGroupId> = table.groups().skip(1).collect();
    for g in &groups {
        table.remove_group(g);
    }
}

/// 원하는 그룹들과 채널 → 그룹 지도.
struct Desired {
    wants: BTreeMap<KeyGroupId, Want>,
    groups_of_channel: HashMap<ChannelId, Option<KeyGroupId>>,
}

fn desired_groups(node: &Node, space: &SpaceId, state: &Arc<SpaceState>, kv: &KeyView) -> Desired {
    let index = node.sync.index.get(space, state);
    let mut wants: BTreeMap<KeyGroupId, Want> = BTreeMap::new();
    let mut groups_of_channel = HashMap::new();
    let devices = index.devices();
    for (c, ch) in &state.channels {
        if !ch.uses_group_keys() {
            continue;
        }
        // 그룹은 상태만 보는 구조 규칙(`SpaceState::channel_key_group`)으로 정합니다: 받는 쪽 권한 검사·따라잡기·시더가
        // 같은 함수를 쓰므로, 마침 모두가 보는 규칙 있는 채널도 비공개 그룹을 둡니다.
        let Some(g) = state.channel_key_group(space, ch) else {
            groups_of_channel.insert(*c, None);
            continue;
        };
        let view = crate::sync_service::channel_view(&index, c);
        groups_of_channel.insert(*c, Some(g));
        let w = wants.entry(g).or_insert_with(|| Want {
            eligible: BTreeSet::new(),
        });
        // 볼 수 있는 활성 기기(크루 멤버로 거르는 것은 크루 갱신 뒤에).
        let mut add = |d: &DeviceId| {
            if kv.owners.contains_key(d) {
                w.eligible.insert(*d);
            }
        };
        if let Some(few) = &view.few {
            for (_, d) in few.iter() {
                add(d);
            }
        } else {
            for (i, (_, d)) in devices.iter().enumerate() {
                if view.everyone || view.mask_allows(i) {
                    add(d);
                }
            }
        }
    }
    Desired {
        wants,
        groups_of_channel,
    }
}

/// 이번 바퀴에 봉인할 기기의 상자 키. 직접 인코딩·전체 재키면 활성 기기 모두, 트리면 가입 기기와 직접 인코딩 그룹의
/// 멤버만(4.5만 대의 키를 매번 읽지 않게).
fn collect_boxes(
    node: &Node,
    state: &SpaceState,
    kv: &KeyView,
    table: &Table,
    joined: &[DeviceId],
    desired: &Desired,
    full: bool,
) -> Boxes {
    let owners: HashMap<DeviceId, UserId> = kv.owners.iter().map(|(d, u)| (*d, *u)).collect();
    let all = full || !table.crew.is_tree() || owners.len() <= yangpa_crypto::keytree::DIRECT_MAX;
    let mut wanted: BTreeSet<DeviceId> = BTreeSet::new();
    if all {
        wanted.extend(owners.keys().copied());
    } else {
        wanted.extend(joined.iter().copied());
        for w in desired.wants.values() {
            if w.eligible.len() <= yangpa_crypto::keytree::DIRECT_MAX {
                wanted.extend(w.eligible.iter().copied());
            }
        }
    }
    let mut by_user: BTreeMap<UserId, Vec<DeviceId>> = BTreeMap::new();
    for d in &wanted {
        if let Some(u) = owners.get(d) {
            by_user.entry(*u).or_default().push(*d);
        }
    }
    let recs: Vec<(&UserId, &yangpa_core::device::DeviceRecord)> = by_user
        .iter()
        .filter_map(|(u, ds)| {
            let list = state.devices.get(u)?;
            Some(
                list.active()
                    .filter(|r| ds.contains(&r.device))
                    .map(move |r| (u, r))
                    .collect::<Vec<_>>(),
            )
        })
        .flatten()
        .collect();
    let keys: HashMap<DeviceId, BoxPublicKey> =
        devkeys::sealing_keys(node, recs).into_iter().collect();
    let lacking = wanted
        .iter()
        .filter(|d| owners.contains_key(*d) && !keys.contains_key(*d))
        .copied()
        .collect();
    Boxes {
        keys,
        owners,
        lacking,
    }
}

/// 크루 갱신을 조각으로 나눠 공간 스트림에 냅니다(지금 크루 상태 키로 암호화). 내 몫 묶음은 먼저 내 멤버 상태에
/// 넣어 두어 내가 낸 갱신을 바로 적용합니다. 해소기가 받아들였으면 마지막 조각 ID.
/// 공간 스트림 머리 하나: (이벤트 ID, 논리 시계).
type Head = (EventId, u64);

/// 크루 갱신 머리 조각의 부모와, 그 부모에서 닿는 가입 기기. 멤버는 넣은 기기마다 그 기기를 활성으로 만든 이벤트가
/// 갱신 머리의 조상이기를 요구합니다(아니면 갱신 전체를 거부). 공간 스트림 머리가 부모 상한 이하면 전부 부모가 되므로
/// 그대로 둡니다(평소). 넘으면(서로 모르는 노드들의 동시 가입이 쌓인 등) 아직 닿지 않은 가입을 가장 많이 덮는 머리부터
/// 고르고 나머지는 보통 순서로 채워 정확히 그 머리들을 부모로 둡니다. 덮지 못한 가입은 이 갱신이 머리를 합친 뒤
/// 다음 바퀴에 넣습니다(예전에는 모두 넣었다가 갱신이 거부되어, 머리가 저절로 합쳐질 때까지 크루 키가 멈췄음).
fn head_parents(
    node: &Node,
    core: &Arc<parking_lot::Mutex<crate::spaces::SpaceCore>>,
    space: &SpaceId,
    joined: Vec<DeviceId>,
) -> Result<(Vec<Head>, Vec<DeviceId>)> {
    let heads = node.store.heads_with_lamport(space, StreamId::Space)?;
    if heads.len() <= MAX_PARENTS || joined.is_empty() {
        return Ok((Vec::new(), joined));
    }
    // 보통 순서: 논리 시계가 큰 순, 같으면 ID 작은 순(`yangpa_sync::dag::choose_parents`).
    let mut order: Vec<(EventId, u64)> = heads.into_iter().map(|(l, id)| (id, l)).collect();
    order.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    order.dedup_by_key(|h| h.0);
    let c = core.lock();
    let covers: Vec<BTreeSet<DeviceId>> = order
        .iter()
        .map(|(h, _)| {
            c.resolver
                .joins_reachable(std::slice::from_ref(h), &joined)
                .into_iter()
                .collect()
        })
        .collect();
    let mut left: BTreeSet<DeviceId> = joined.iter().copied().collect();
    let mut picked: Vec<usize> = Vec::new();
    while picked.len() < MAX_PARENTS && !left.is_empty() {
        let best = (0..order.len())
            .filter(|i| !picked.contains(i))
            .map(|i| (covers[i].intersection(&left).count(), i))
            .filter(|(n, _)| *n > 0)
            .max_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
        let Some((_, i)) = best else { break };
        for d in &covers[i] {
            left.remove(d);
        }
        picked.push(i);
    }
    for i in 0..order.len() {
        if picked.len() >= MAX_PARENTS {
            break;
        }
        if !picked.contains(&i) {
            picked.push(i);
        }
    }
    let extra: Vec<(EventId, u64)> = picked.iter().map(|i| order[*i]).collect();
    let ids: Vec<EventId> = extra.iter().map(|(id, _)| *id).collect();
    let reach = c.resolver.joins_reachable(&ids, &joined);
    Ok((extra, reach))
}

#[allow(clippy::too_many_arguments)]
fn publish_crew(
    node: &Arc<Node>,
    l: &Locked<'_>,
    space: &SpaceId,
    core: &Arc<parking_lot::Mutex<crate::spaces::SpaceCore>>,
    next: &mut KeyTree,
    o: KeyUpdateOutput,
    boxes: &Boxes,
    head_extra: &[(EventId, u64)],
    out: &mut RoundOut,
) -> Result<(EventId, Vec<yangpa_crypto::DeviceBundle>, bool)> {
    let crew = KeyGroupId::crew(space);
    let seq = o.payload.seq;
    let commit = o.payload.root_commit;
    let me = node.me()?.device_id();
    {
        let mut c = core.lock();
        for b in &o.bundles {
            if b.device == me {
                c.keys.offer_bundle(crew, seq, b.clone());
            }
        }
    }
    let (sid, s) = keys::current_state_key(node, space)?
        .ok_or_else(|| Error::conflict("아직 이 공간의 상태 키가 없습니다"))?;
    let mut payload = o.payload;
    let mut sharded = false;
    if let Some(blob) = crate::keygroup::shards::publish(node, space, &o.bundles, &boxes.owners)? {
        payload.shards = Some(blob);
        sharded = true;
    }
    let parts = payload
        .split(PART_TARGET_LEN)
        .map_err(|e| Error::internal(format!("키 갱신 나누기: {e}")))?;
    let mut head: Option<(EventId, u64)> = None;
    let mut last = None;
    for p in parts {
        let extra: Vec<(EventId, u64)> = match head {
            Some(h) => vec![h],
            None => head_extra.to_vec(),
        };
        let (ev, _) = author::publish_locked(
            node,
            l,
            *space,
            None,
            &EventBody::KeyUpdate(p),
            KeyChoice::StateWith(sid, s.clone()),
            &extra,
        )?;
        if head.is_none() {
            head = Some((ev.id(), ev.header.lamport));
        }
        last = Some(ev.id());
        out.events.push(ev);
    }
    let accepted = core
        .lock()
        .resolver
        .key_view()
        .is_some_and(|k| k.crew.seq == seq && k.crew.root_commit == commit);
    if !accepted {
        return Err(Error::conflict("해소기가 크루 키 갱신을 받아들이지 않음"));
    }
    let (head, _) = head.expect("조각 하나는 있음");
    next.set_last_event(head);
    Ok((last.expect("조각 하나는 있음"), o.bundles, sharded))
}

/// 받은편지함 키 묶음: 가입한 사용자마다 하나(최근 상태 키, 지금 키를 마지막에; 크루 R; 트리 가입 기기의 몫 묶음).
/// `all`이면(조각 블롭 없이 낸 트리 전체 재키) 다른 사용자에게도 그 사용자 기기의 몫 묶음만 담아 보냅니다.
#[allow(clippy::too_many_arguments)]
fn share_bundles(
    node: &Arc<Node>,
    l: &Locked<'_>,
    space: &SpaceId,
    state: &SpaceState,
    kv: &KeyView,
    base: Option<&yangpa_sync::CheckpointBase>,
    table: &Table,
    joined: &[DeviceId],
    bundles: &[yangpa_crypto::DeviceBundle],
    all: bool,
    out: &mut RoundOut,
) -> Result<()> {
    let crew = KeyGroupId::crew(space);
    let seq = table.crew.seq();
    let me = node.me()?;
    // 사용자 → (가입한 기기가 있는지, 그 사용자 기기의 묶음).
    let mut users: BTreeMap<UserId, (bool, Vec<&yangpa_crypto::DeviceBundle>)> = BTreeMap::new();
    for d in joined {
        if let Some(u) = kv.owners.get(d) {
            users.entry(*u).or_default().0 = true;
        }
    }
    for b in bundles {
        let Some(u) = kv.owners.get(&b.device) else {
            continue;
        };
        if joined.contains(&b.device) || all {
            users.entry(*u).or_default().1.push(b);
        }
    }
    if users.is_empty() {
        return Ok(());
    }
    let history = keys::keys_for_joiner(node, space, base)?;
    let r = table.crew.r().clone();
    let root = SharedKey::Root {
        group: crew,
        id: keysched::group_root_id(space, &crew, &r),
        key: r,
    };
    for (u, (joiner, mine)) in users {
        if u == me.user() && mine.iter().all(|b| b.device == me.device_id()) && !joiner {
            continue;
        }
        let rcpts = devkeys::user_sealing_keys(node, state, &u);
        if rcpts.is_empty() {
            continue;
        }
        let mut keys_v = Vec::new();
        if joiner {
            keys_v.extend(history.iter().cloned());
            keys_v.push(root.clone());
        }
        for b in mine {
            keys_v.push(SharedKey::Bundle {
                group: crew,
                seq,
                bundle: b.encode(),
            });
        }
        // 기준점에 그 사용자의 기기를 넣은 이벤트(가입·기기 목록 갱신)를 넣어, 받는 쪽이 그것을 본 상태로 권한을 봅니다.
        // 크루 갱신은 넣지 않습니다: 가입자는 이 묶음의 상태 키가 있어야 그 갱신을 엽니다.
        let hint: Vec<EventId> = crate::crews::inbox_relays(node, space, &u)
            .first()
            .map(|r| r.event)
            .into_iter()
            .collect();
        // 지난 상태 키가 많으면 묶음 하나의 키 수 상한을 넘으므로 나눠 냅니다(오래된 것부터, 지금 키는 마지막 묶음에).
        for chunk in keys::chunk_shares(keys_v) {
            let body = EventBody::KeyShare(KeyShareBody {
                keys: chunk,
                for_user: Some(u),
            });
            match author::publish_scoped_locked(
                node,
                l,
                *space,
                Scope::Inbox(u),
                &body,
                KeyChoice::Sealed(rcpts.clone()),
                &hint,
            ) {
                Ok(ev) => out.events.push(ev),
                Err(e) => {
                    debug!(space = %space.short(), error = %e, "받은편지함 키 묶음을 내지 못함");
                    break;
                }
            }
        }
    }
    Ok(())
}

/// 그룹 갱신을 키 그룹 스트림에 냅니다(크루 상태 키로 암호화: 가입하는 기기도 이미 크루 멤버라 열 수 있음). 그룹
/// 사슬이 받아들였으면 머리 ID.
#[allow(clippy::too_many_arguments)]
fn publish_group(
    node: &Arc<Node>,
    l: &Locked<'_>,
    space: &SpaceId,
    core: &Arc<parking_lot::Mutex<crate::spaces::SpaceCore>>,
    group: &KeyGroupId,
    o: KeyUpdateOutput,
    crew_s: &Secret32,
    anchor: &[EventId],
    out: &mut RoundOut,
) -> Result<EventId> {
    let seq = o.payload.seq;
    let commit = o.payload.root_commit;
    let sid = state_key_id(space, crew_s);
    let parts: Vec<KeyUpdatePayload> = o
        .payload
        .split(PART_TARGET_LEN)
        .map_err(|e| Error::internal(format!("키 갱신 나누기: {e}")))?;
    let mut head: Option<(EventId, u64)> = None;
    let mut evs = Vec::new();
    for p in parts {
        let extra: Vec<(EventId, u64)> = head.into_iter().collect();
        let ev = author::publish_scoped_parts_locked(
            node,
            l,
            *space,
            Scope::KeyGroup(*group),
            &EventBody::KeyUpdate(p),
            KeyChoice::StateWith(sid, crew_s.clone()),
            anchor,
            &extra,
        )?;
        if head.is_none() {
            head = Some((ev.id(), ev.header.lamport));
        }
        evs.push(ev);
    }
    let accepted = core.lock().keys.chains.get(group).is_some_and(|c| {
        let t = c.tip();
        t.seq == seq && t.root_commit == commit
    });
    if !accepted {
        // 내 사슬이 받지 않은 갱신은 퍼뜨리지 않습니다(다른 노드도 같은 규칙으로 거부함).
        return Err(Error::conflict("그룹 사슬이 키 갱신을 받아들이지 않음"));
    }
    out.events.extend(evs);
    Ok(head.expect("조각 하나는 있음").0)
}

/// 기록 공개를 켠 채널: 비공개 시절 뿌리(뿌리 저장소에 있는 것)를 그 채널 그룹의 지금 상태 키로 채널 스트림에 다시
/// 봉인합니다. 나중에 들어온 멤버도 가입 때 받은 상태 키로 열어 지난 기록을 읽습니다.
fn reseal_roots(
    node: &Arc<Node>,
    l: &Locked<'_>,
    space: &SpaceId,
    core: &Arc<parking_lot::Mutex<crate::spaces::SpaceCore>>,
    channel: &ChannelId,
    group: Option<&KeyGroupId>,
    out: &mut RoundOut,
) -> Result<()> {
    let roots = keys::roots_of_channel(node, space, channel)?;
    if roots.is_empty() {
        return Ok(());
    }
    let s = {
        let c = core.lock();
        let k = match group {
            None => c.keys.crew_current(space, &c.resolver).cloned(),
            Some(g) => c.keys.group_current(g).cloned(),
        };
        k.map(|k| k.state_key())
    };
    let Some(s) = s else {
        return Ok(());
    };
    let sid = state_key_id(space, &s);
    for chunk in roots.chunks(ROOTS_PER_SHARE) {
        let body = EventBody::KeyShare(KeyShareBody {
            keys: chunk
                .iter()
                .map(|(id, key)| SharedKey::ChannelRoot {
                    channel: *channel,
                    id: *id,
                    key: key.clone(),
                })
                .collect(),
            for_user: None,
        });
        let (ev, _) = author::publish_locked(
            node,
            l,
            *space,
            Some(*channel),
            &body,
            KeyChoice::StateWith(sid, s.clone()),
            &[],
        )?;
        out.events.push(ev);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 주기 R 회전: 마지막 새 R 뒤로 하루가 지나거나 가입이 1000번 쌓인 그룹만 `pending_refresh`에 들고, 새 R을 낸
    /// 갱신이 받아들여지면 빠지며 나이가 처음부터 다시 셉니다. 예전 메타(새 칸 없음)도 읽힙니다.
    #[test]
    fn periodic_r_refresh_is_due_by_age_or_joins() {
        let space = SpaceId::random();
        let crew = KeyGroupId::crew(&space);
        let (aged, busy, quiet) = (
            KeyGroupId::private(&space, b"aged"),
            KeyGroupId::private(&space, b"busy"),
            KeyGroupId::private(&space, b"quiet"),
        );
        let day = R_REFRESH_EVERY.as_millis() as u64;
        let t0 = 10 * day;
        let mut meta = Meta::default();
        for g in [crew, aged, busy, quiet] {
            meta.fresh_r(g, t0);
        }
        meta.joined(busy, (R_REFRESH_JOINS - 1) as usize, t0 + 1);
        meta.mark_r_due([crew, aged, busy, quiet].into_iter(), t0 + day - 1);
        assert!(
            meta.pending_refresh.is_empty(),
            "{:?}",
            meta.pending_refresh
        );

        meta.joined(busy, 1, t0 + 2);
        meta.fresh_r(aged, t0 - day); // 하루 전에 마지막 새 R
        meta.mark_r_due([crew, aged, busy, quiet].into_iter(), t0 + day - 1);
        assert_eq!(
            meta.pending_refresh,
            BTreeSet::from([aged, busy]),
            "나이·가입 수로 고른 그룹"
        );
        // 새 R이 받아들여지면 빠지고, 같은 시각에 다시 보아도 들지 않습니다.
        meta.fresh_r(busy, t0 + day);
        meta.mark_r_due([busy].into_iter(), t0 + day);
        assert!(!meta.pending_refresh.contains(&busy));
        assert!(
            meta.pending_refresh.contains(&aged),
            "다른 그룹의 빚은 남음"
        );
        // 처음 보는 그룹은 지금부터 셉니다(곧바로 회전하지 않음).
        let new = KeyGroupId::private(&space, b"new");
        meta.mark_r_due([new].into_iter(), t0 + 5 * day);
        assert!(!meta.pending_refresh.contains(&new));

        let old: Meta = serde_json::from_str(r#"{"last_full_ms":5,"channels":{}}"#).unwrap();
        assert!(old.pending_refresh.is_empty() && old.r_age.is_empty());
    }
}
