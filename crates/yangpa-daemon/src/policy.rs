//! 시더·피어 정책, 열어 본 채널, 휴면 공간(B3).
//!
//! 코어가 부르는 고리:
//! - [`seeder_policy`]: 남의 요청에 답할지(`yangpa_sync::policy::check_seed_request`)에 쓰는 정책.
//! - [`channel_opened`], [`mark_channel_opened`]: "본인이 연 채널만 시드" 판단과 열기 기록.
//! - [`prefetches`]: 열지 않은 채널의 첨부도 미리 받을지(헤드리스 시더의 `prefetch`).
//! - [`is_dormant`]: 주기적 따라잡기·커밋 확정에서 뺄 휴면 공간.
//!
//! 저장(설정 표, JSON). 공간 값이 있으면 전역 값을 통째로 대신합니다.
//! - `seed.policy`, `seed.policy.<공간>`: 시더 설정(개별 메시지 제외는 빼고).
//! - `peer.policy`, `peer.policy.<공간>`: 피어 설정.
//! - `seed.excluded.<공간>`: 개별 메시지 제외 목록(이벤트 표의 `seed_excluded`도 함께 켬).
//! - `policy.opened.<공간>`: 열어 본(보내거나 읽은) 채널.
//! - `policy.active.<공간>`: 마지막 활동 시각(밀리초, 한 시간에 한 번만 씀).
//!
//! 기본값은 `yangpa_sync::policy`의 것(Tor로 돌면 `tor_default`, P2P 전용 채널도 같음).
//! 휴면: 열기·보내기·받은 전파가 `dormant_after`(기본 14일, 0이면 끔) 동안 없던 크루. gossip 구독을 끊고 주기적
//! 따라잡기에서 빠지며, 열거나 전파가 직접 오면 깨어나 인증된 멤버 기기마다 따라잡습니다. 다음은 휴면이 되지
//! 않습니다(이미 휴면이면 깨움): 1:1 스페이스(친구 DM을 놓치지 않게), 내 기기가 확정 기기인 크루(키 그룹
//! 갱신을 멈추지 않게), 열지 않은 채널까지 제공하도록 시더 범위를 정해 두거나 미리 받기를 켠 크루(헤드리스
//! 상시 시더).

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;
use tracing::debug;
use yangpa_core::ids::{ChannelId, EventId, SpaceId};
use yangpa_core::model::{Permissions, SpaceKind, SpaceState};
use yangpa_core::time::Timestamp;
use yangpa_sync::policy::{DEFAULT_RATE_LIMIT_BPS, DataType, PeerPolicy, SeederPolicy};

use crate::error::{Error, Result};
use crate::node::Node;
use crate::{blobs, sync_service};

const SEED_KEY: &str = "seed.policy";
const PEER_KEY: &str = "peer.policy";
const EXCLUDED_PREFIX: &str = "seed.excluded.";
const OPENED_PREFIX: &str = "policy.opened.";
const ACTIVE_PREFIX: &str = "policy.active.";
/// 마지막 활동 시각을 이보다 자주 저장하지 않습니다(열 때마다 쓰기가 생기지 않게).
const ACTIVE_SAVE_MS: u64 = 60 * 60 * 1000;
/// 공간 하나의 개별 메시지 제외 상한(설정 값 크기를 묶음).
pub(crate) const MAX_EXCLUDED: usize = 10_000;
/// 예전 기본 대역폭(2026-10-21 전): 시더 송신 2MiB/s(Tor 256KiB/s), Tor 모드 피어 수신 512KiB/s.
const LEGACY_UPLOAD_DEFAULTS: [u64; 2] = [2 * 1024 * 1024, 256 * 1024];
const LEGACY_DOWNLOAD_DEFAULT: u64 = 512 * 1024;
/// 예전 기본 대역폭을 새 기본값으로 한 번 옮겼는지.
const RATE_MIGRATED_KEY: &str = "policy.rate_defaults_v2";

/// 정책 영역의 노드 수준 상태(`Node::policy`).
#[derive(Default)]
pub struct PolicyState {
    /// 열어 본 채널(시작 때 설정에서 읽음).
    opened: Mutex<HashSet<(SpaceId, ChannelId)>>,
    /// 읽어 둔 저장 설정(키: 공간, `None`이면 전역). 값이 `None`이면 저장된 것이 없음.
    seed: Mutex<HashMap<Option<SpaceId>, Option<SeederPolicy>>>,
    peer: Mutex<HashMap<Option<SpaceId>, Option<PeerPolicy>>>,
    excluded: Mutex<HashMap<SpaceId, BTreeSet<EventId>>>,
    activity: Mutex<HashMap<SpaceId, Activity>>,
    /// `seed.coverage`가 시더·보관 노드 0으로 본 채널(0이 된 때 한 번만 경고하게).
    uncovered: Mutex<HashMap<SpaceId, HashSet<ChannelId>>>,
    /// 휴면이라 주기 작업이 건너뛴 횟수(시험용).
    dormant_skips: AtomicU64,
}

#[derive(Clone, Copy)]
struct Activity {
    last_ms: u64,
    saved_ms: u64,
}

fn now_ms() -> u64 {
    Timestamp::now().0
}

fn scoped_key(base: &str, space: Option<&SpaceId>) -> String {
    match space {
        Some(s) => format!("{base}.{s}"),
        None => base.to_owned(),
    }
}

pub(crate) fn start(node: &Arc<Node>) {
    migrate_rate_defaults(node);
    // 열어 본 채널을 다시 읽습니다(재시작 뒤에도 '본인이 연 채널만' 제공 범위가 유지되게).
    let rows = node
        .store
        .settings_with_prefix(OPENED_PREFIX)
        .unwrap_or_default();
    let mut opened = node.policy.opened.lock();
    for (key, value) in rows {
        let Ok(space) = key[OPENED_PREFIX.len()..].parse::<SpaceId>() else {
            continue;
        };
        let Ok(channels) = serde_json::from_value::<Vec<ChannelId>>(value) else {
            continue;
        };
        opened.extend(channels.into_iter().map(|c| (space, c)));
    }
}

/// 한 번만: 예전 기본 대역폭을 그대로 저장해 둔 설정을 새 값으로 옮깁니다. 크루 화면의 시더 스위치나 Tor 모드에서
/// 저장한 설정은 그때의 기본값을 통째로 복사해 두어, 기본값을 올려도 따라오지 않고 전역 값을 바꿔도 따라가지
/// 않았습니다([`follow_global_rate`]는 이전 전역 값과 같은 것만 바꿈). 전역 값이 먼저 새 기본값이 되고, 공간 값은 그
/// 전역 값을 따릅니다. 사용자가 일부러 고른 값이 예전 기본값과 같으면 함께 바뀝니다.
fn migrate_rate_defaults(node: &Node) {
    if node
        .store
        .get_setting::<bool>(RATE_MIGRATED_KEY)
        .ok()
        .flatten()
        .is_some()
    {
        return;
    }
    let legacy_up = |v: Option<u64>| v.is_some_and(|b| LEGACY_UPLOAD_DEFAULTS.contains(&b));
    let legacy_down = |v: Option<u64>| v == Some(LEGACY_DOWNLOAD_DEFAULT);
    let new = Some(DEFAULT_RATE_LIMIT_BPS);

    let mut global_up = new;
    if let Ok(Some(mut p)) = node.store.get_setting::<SeederPolicy>(SEED_KEY) {
        if legacy_up(p.upload_limit_bps) {
            p.upload_limit_bps = new;
            let _ = node.store.put_setting(SEED_KEY, &p);
        }
        global_up = p.upload_limit_bps;
    }
    let mut global_down = new;
    if let Ok(Some(mut p)) = node.store.get_setting::<PeerPolicy>(PEER_KEY) {
        if legacy_down(p.download_limit_bps) {
            p.download_limit_bps = new;
            let _ = node.store.put_setting(PEER_KEY, &p);
        }
        global_down = p.download_limit_bps;
    }
    let seed_prefix = format!("{SEED_KEY}.");
    for (key, value) in node
        .store
        .settings_with_prefix(&seed_prefix)
        .unwrap_or_default()
    {
        if let Ok(mut p) = serde_json::from_value::<SeederPolicy>(value)
            && legacy_up(p.upload_limit_bps)
        {
            p.upload_limit_bps = global_up;
            let _ = node.store.put_setting(&key, &p);
        }
    }
    let peer_prefix = format!("{PEER_KEY}.");
    for (key, value) in node
        .store
        .settings_with_prefix(&peer_prefix)
        .unwrap_or_default()
    {
        if let Ok(mut p) = serde_json::from_value::<PeerPolicy>(value)
            && legacy_down(p.download_limit_bps)
        {
            p.download_limit_bps = global_down;
            let _ = node.store.put_setting(&key, &p);
        }
    }
    node.policy.seed.lock().clear();
    node.policy.peer.lock().clear();
    let _ = node.store.put_setting(RATE_MIGRATED_KEY, &true);
}

// ───────────── 시더·피어 설정 ─────────────

/// Tor로 도는지(직접 연결 스웜은 일반 기본값).
fn tor_mode(node: &Node) -> bool {
    crate::tor::tor_active(node)
}

fn default_seed(node: &Node) -> SeederPolicy {
    if tor_mode(node) {
        SeederPolicy::tor_default()
    } else {
        SeederPolicy::default()
    }
}

fn default_peer(node: &Node) -> PeerPolicy {
    if tor_mode(node) {
        PeerPolicy::tor_default()
    } else {
        PeerPolicy::default()
    }
}

fn cached_seed(node: &Node, scope: Option<&SpaceId>) -> Option<SeederPolicy> {
    if let Some(v) = node.policy.seed.lock().get(&scope.copied()) {
        return v.clone();
    }
    let v: Option<SeederPolicy> = node
        .store
        .get_setting(&scoped_key(SEED_KEY, scope))
        .unwrap_or_default();
    node.policy.seed.lock().insert(scope.copied(), v.clone());
    v
}

fn cached_peer(node: &Node, scope: Option<&SpaceId>) -> Option<PeerPolicy> {
    if let Some(v) = node.policy.peer.lock().get(&scope.copied()) {
        return v.clone();
    }
    let v: Option<PeerPolicy> = node
        .store
        .get_setting(&scoped_key(PEER_KEY, scope))
        .unwrap_or_default();
    node.policy.peer.lock().insert(scope.copied(), v.clone());
    v
}

/// 저장된 시더 설정(공간 값 → 전역 값 → 기본값). 개별 메시지 제외 전 값입니다.
pub(crate) fn stored_seeder_policy(node: &Node, space: Option<&SpaceId>) -> SeederPolicy {
    if space.is_some()
        && let Some(p) = cached_seed(node, space)
    {
        return p;
    }
    cached_seed(node, None).unwrap_or_else(|| default_seed(node))
}

/// 남의 요청을 검사할 때 쓰는 시더 설정: 저장된 설정 + 그 공간의 개별 메시지 제외.
pub(crate) fn seeder_policy(node: &Node, space: &SpaceId) -> SeederPolicy {
    let mut p = stored_seeder_policy(node, Some(space));
    p.excluded_events.extend(excluded_events(node, space));
    p
}

/// 시더 설정 저장(`space`가 `None`이면 전역). 개별 메시지 제외는 [`set_excluded`]로 따로 둡니다.
pub(crate) fn set_seeder_policy(
    node: &Node,
    space: Option<&SpaceId>,
    mut p: SeederPolicy,
) -> Result<()> {
    p.excluded_events.clear();
    node.store.put_setting(&scoped_key(SEED_KEY, space), &p)?;
    node.policy.seed.lock().insert(space.copied(), Some(p));
    Ok(())
}

/// 전역 송신 상한·동시 전송 수를 바꿀 때, 공간 설정 중 이전 전역 값을 그대로 가진 것도 함께 바꿉니다.
/// 크루 화면의 시더 스위치는 그때의 설정 전체를 공간 값으로 복사하므로, 따로 정한 적 없는 값은 전역 설정을 계속 따르게
/// 하려는 것입니다(이전 전역 값과 다르게 정한 공간 값은 그대로 둠).
pub(crate) fn follow_global_rate(
    node: &Node,
    old: &SeederPolicy,
    new: &SeederPolicy,
) -> Result<()> {
    if old.upload_limit_bps == new.upload_limit_bps && old.max_concurrent == new.max_concurrent {
        return Ok(());
    }
    let prefix = format!("{SEED_KEY}.");
    for (key, value) in node.store.settings_with_prefix(&prefix)? {
        let Some(space) = key
            .strip_prefix(&prefix)
            .and_then(|s| s.parse::<SpaceId>().ok())
        else {
            continue;
        };
        let Ok(mut p) = serde_json::from_value::<SeederPolicy>(value) else {
            continue;
        };
        let mut changed = false;
        if p.upload_limit_bps == old.upload_limit_bps && p.upload_limit_bps != new.upload_limit_bps
        {
            p.upload_limit_bps = new.upload_limit_bps;
            changed = true;
        }
        if p.max_concurrent == old.max_concurrent && p.max_concurrent != new.max_concurrent {
            p.max_concurrent = new.max_concurrent;
            changed = true;
        }
        if changed {
            set_seeder_policy(node, Some(&space), p)?;
        }
    }
    Ok(())
}

/// 피어 설정(공간 값 → 전역 값 → 기본값).
pub(crate) fn peer_policy(node: &Node, space: Option<&SpaceId>) -> PeerPolicy {
    if space.is_some()
        && let Some(p) = cached_peer(node, space)
    {
        return p;
    }
    cached_peer(node, None).unwrap_or_else(|| default_peer(node))
}

/// 이 공간에 따로 저장한 피어 설정이 있는지(수신 대역폭을 공간별로 셀지 정함).
pub(crate) fn has_space_peer_policy(node: &Node, space: &SpaceId) -> bool {
    cached_peer(node, Some(space)).is_some()
}

pub(crate) fn set_peer_policy(
    node: &Arc<Node>,
    space: Option<&SpaceId>,
    p: PeerPolicy,
) -> Result<()> {
    node.store.put_setting(&scoped_key(PEER_KEY, space), &p)?;
    node.policy.peer.lock().insert(space.copied(), Some(p));
    blobs::on_peer_policy_changed(node);
    Ok(())
}

/// 공간의 개별 메시지 제외 목록.
pub(crate) fn excluded_events(node: &Node, space: &SpaceId) -> BTreeSet<EventId> {
    if let Some(v) = node.policy.excluded.lock().get(space) {
        return v.clone();
    }
    let v: BTreeSet<EventId> = node
        .store
        .get_setting(&format!("{EXCLUDED_PREFIX}{space}"))
        .ok()
        .flatten()
        .unwrap_or_default();
    node.policy.excluded.lock().insert(*space, v.clone());
    v
}

/// 메시지 하나를 제공 범위에서 빼거나 되돌립니다(그 메시지의 첨부도 함께).
pub(crate) fn set_excluded(
    node: &Node,
    space: &SpaceId,
    event: &EventId,
    excluded: bool,
) -> Result<()> {
    let mut set = excluded_events(node, space);
    let changed = if excluded {
        if set.len() >= MAX_EXCLUDED && !set.contains(event) {
            return Err(Error::invalid(format!(
                "제외할 수 있는 메시지는 공간마다 {MAX_EXCLUDED}개까지입니다"
            )));
        }
        set.insert(*event)
    } else {
        set.remove(event)
    };
    if changed {
        let key = format!("{EXCLUDED_PREFIX}{space}");
        if set.is_empty() {
            node.store.delete_setting(&key)?;
        } else {
            node.store.put_setting(&key, &set)?;
        }
    }
    node.store.set_seed_excluded(event, excluded)?;
    node.policy.excluded.lock().insert(*space, set);
    Ok(())
}

/// 이번 확인에서 시더·보관 노드 0인 채널을 기록하고, 지난번에는 0이 아니었던(또는 결과에 없던) 것만 돌려줍니다
/// (앱이 몇 초마다 다시 물어도 같은 경고가 거듭 나가지 않게).
pub(crate) fn newly_uncovered(
    node: &Node,
    space: &SpaceId,
    zero: HashSet<ChannelId>,
) -> Vec<ChannelId> {
    let prev = node
        .policy
        .uncovered
        .lock()
        .insert(*space, zero.clone())
        .unwrap_or_default();
    zero.difference(&prev).copied().collect()
}

// ───────────── 열어 본 채널 ─────────────

/// 이 채널을 "열어 본 채널"로 볼지. 1:1 공간은 채널이 하나뿐이라 항상 열린 것으로 봅니다.
pub(crate) fn channel_opened(
    node: &Node,
    space: &SpaceId,
    state: &SpaceState,
    channel: &ChannelId,
) -> bool {
    state.kind == SpaceKind::Space || node.policy.opened.lock().contains(&(*space, *channel))
}

/// 열지 않은 채널의 첨부도 미리 받을지: 시더가 켜져 있고 `prefetch`이며, 그 종류·채널·메시지가 제공 범위에 들고
/// (열린 것으로 보고), 내가 볼 수 있는 휘발성이 아닌 채널일 때. 받아 둔 것은 그대로 남에게 내줍니다.
pub(crate) fn prefetches(
    node: &Node,
    space: &SpaceId,
    state: &SpaceState,
    channel: &ChannelId,
    event: &EventId,
    data_type: DataType,
) -> bool {
    let p = seeder_policy(node, space);
    if !(p.enabled && p.prefetch && p.seeds(data_type, Some(channel), Some(event), true)) {
        return false;
    }
    if state.channels.get(channel).is_none_or(|c| c.is_volatile()) {
        return false;
    }
    node.me().is_ok_and(|me| {
        state
            .channel_permissions(&me.user(), channel)
            .contains(Permissions::VIEW_CHANNEL)
    })
}

/// `message.send`·`message.list`: 채널을 연 것으로 기록하고, 공간 활동을 남기고(휴면이면 깨움), 그 채널에서
/// 미뤄 둔 자동 내려받기를 시작합니다.
pub(crate) fn mark_channel_opened(node: &Arc<Node>, space: &SpaceId, channel: &ChannelId) {
    let newly = node.policy.opened.lock().insert((*space, *channel));
    if newly {
        let channels: Vec<ChannelId> = node
            .policy
            .opened
            .lock()
            .iter()
            .filter(|(s, _)| s == space)
            .map(|(_, c)| *c)
            .collect();
        if let Err(e) = node
            .store
            .put_setting(&format!("{OPENED_PREFIX}{space}"), &channels)
        {
            debug!(error = %e, "열어 본 채널 저장 실패");
        }
    }
    touch(node, space);
    blobs::on_channel_opened(node, space, channel);
}

// ───────────── 휴면 ─────────────

/// 마지막 활동 시각. 처음 보는 공간이면 저장된 값·가입 시각·마지막 읽음 중 가장 늦은 것.
fn last_activity(node: &Node, space: &SpaceId) -> u64 {
    if let Some(a) = node.policy.activity.lock().get(space) {
        return a.last_ms;
    }
    let saved: u64 = node
        .store
        .get_setting(&format!("{ACTIVE_PREFIX}{space}"))
        .ok()
        .flatten()
        .unwrap_or(0);
    let from_record = match node.store.get_space(space) {
        Ok(Some(r)) => r.joined_at.0.max(r.last_read_at.map_or(0, |t| t.0)),
        _ => now_ms(),
    };
    let a = Activity {
        last_ms: saved.max(from_record),
        saved_ms: saved,
    };
    node.policy
        .activity
        .lock()
        .entry(*space)
        .or_insert(a)
        .last_ms
}

/// 공간 활동(열기·보내기·받은 전파)을 기록합니다. 휴면이었으면 깨웁니다.
pub(crate) fn touch(node: &Arc<Node>, space: &SpaceId) {
    let now = now_ms();
    last_activity(node, space);
    let save = {
        let mut m = node.policy.activity.lock();
        let a = m.entry(*space).or_insert(Activity {
            last_ms: now,
            saved_ms: 0,
        });
        a.last_ms = a.last_ms.max(now);
        let due = now.saturating_sub(a.saved_ms) >= ACTIVE_SAVE_MS;
        if due {
            a.saved_ms = now;
        }
        due
    };
    if save
        && let Err(e) = node
            .store
            .put_setting(&format!("{ACTIVE_PREFIX}{space}"), &now)
    {
        debug!(error = %e, "공간 활동 시각 저장 실패");
    }
    if is_marked_dormant(node, space) {
        wake(node, space);
    }
}

/// 받은 전파(`PushEvents`·gossip). 휴면 공간이면 깨웁니다.
pub(crate) fn on_push(node: &Arc<Node>, space: &SpaceId) {
    touch(node, space);
}

/// 지금 휴면으로 표시돼 있는지(판단을 새로 하지 않음).
pub(crate) fn is_marked_dormant(node: &Node, space: &SpaceId) -> bool {
    node.spaces.read().get(space).is_some_and(|r| r.dormant)
}

/// 휴면이 되면 안 되는 공간인지(모듈 설명 참고).
fn never_dormant(node: &Node, space: &SpaceId) -> bool {
    if node.cfg.sharing.dormant_after.is_zero() {
        return true;
    }
    let Some(state) = node.space_state(space) else {
        return false;
    };
    if state.kind == SpaceKind::Space {
        return true;
    }
    if node.me().is_ok_and(|me| state.committer == me.device_id()) {
        return true;
    }
    let p = stored_seeder_policy(node, Some(space));
    p.enabled && (p.all_channels || p.prefetch || p.include_channels.is_some())
}

/// 주기 작업이 이 공간을 건너뛸지. 활동이 `dormant_after` 넘게 없으면 여기서 휴면으로 바꿉니다
/// (gossip 구독을 끊고 미뤄 둔 자동 내려받기를 버림). 휴면이 되면 안 되는 공간이 휴면으로 표시돼 있으면
/// (예전 판이 표시했거나 확정 기기가 됨) 여기서 깨웁니다.
pub(crate) fn is_dormant(node: &Arc<Node>, space: &SpaceId) -> bool {
    let marked = match node.spaces.read().get(space) {
        Some(r) => r.dormant,
        None => return false,
    };
    if never_dormant(node, space) {
        if marked {
            wake(node, space);
        }
        return false;
    }
    let after = node.cfg.sharing.dormant_after.as_millis() as u64;
    let dormant = marked || now_ms().saturating_sub(last_activity(node, space)) >= after;
    if dormant {
        if !marked {
            set_dormant(node, space, true);
        }
        node.policy.dormant_skips.fetch_add(1, Ordering::Relaxed);
    }
    dormant
}

/// 휴면 표시를 바꿉니다. 바뀌었으면 true.
fn set_dormant(node: &Node, space: &SpaceId, on: bool) -> bool {
    {
        let mut spaces = node.spaces.write();
        let Some(rt) = spaces.get_mut(space) else {
            return false;
        };
        if rt.dormant == on {
            return false;
        }
        rt.dormant = on;
    }
    if let Err(e) = node.store.set_space_dormant(space, on) {
        debug!(error = %e, "휴면 표시 저장 실패");
    }
    if on {
        node.net
            .unsubscribe(yangpa_core::wire::GossipTopic::Crew(*space));
        crate::topics::forget(node, space);
        crate::chantopics::forget(node, space);
        blobs::on_space_dormant(node, space);
        debug!(space = %space.short(), "공간 휴면");
    } else {
        debug!(space = %space.short(), "공간 깨어남");
    }
    true
}

/// 휴면에서 깨웁니다: gossip을 다시 구독하고, 인증된 멤버 기기마다 놓친 것을 따라잡습니다.
fn wake(node: &Arc<Node>, space: &SpaceId) {
    if !set_dormant(node, space, false) {
        return;
    }
    sync_service::refresh_space_peers(node, space);
    blobs::on_space_woken(node, space);
    let Some(state) = node.space_state(space) else {
        return;
    };
    for (peer, user, device) in node.peers.authed_peers() {
        if state.is_member(&user) && state.device_belongs(&user, &device) {
            sync_service::spawn_catch_up(node, *space, peer);
        }
    }
}

/// 휴면이라 주기 작업이 건너뛴 횟수(시험용).
pub(crate) fn dormant_skips(node: &Node) -> u64 {
    node.policy.dormant_skips.load(Ordering::Relaxed)
}
