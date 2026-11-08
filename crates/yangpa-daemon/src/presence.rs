//! 표시 상태(온라인·자리 비움·숨기·오프라인)와 마지막 동기화 시각(decisions.md "표시 상태와 마지막 동기화").
//!
//! - 이 기기의 상태는 설정 `presence.mode`(모드·끝나는 시각)와 `presence.hide_last_seen`에 둡니다(기기마다). 온라인은
//!   자동이라 앱이 알린 입력 없음(`idle`)이면 자리 비움으로 보냅니다. 기간이 끝나면 온라인으로 돌아갑니다.
//! - 인증 직후와 상태가 바뀔 때 인증된 친구·멤버 기기에게 `YpRequest::Presence`를 보냅니다(내 기기·보관 전용 관계 제외).
//!   한 상대에게는 하나씩, 보낼 때의 상태로 보내 늦게 닿은 요청이 새 상태를 덮지 않게 합니다.
//! - 받은 상태는 기기마다 RAM에 둡니다. 화면에 보일 사용자 상태는 보이는 기기 중 온라인 > 자리 비움이고, 숨기 기기와
//!   인증 직후 상태를 아직 받지 못한 기기([`GRACE`] 동안)는 빼서 숨기 기기가 잠깐도 드러나지 않게 합니다. 옛 빌드(상태를
//!   보내지 않음)는 그 뒤 온라인입니다. 연결·동기화 판단(`PeerDirectory::user_online`)은 그대로 둡니다.
//! - 마지막 동기화 시각은 사용자마다 `presence.seen.<사용자>`에 둡니다. 보이는 상태가 바뀔 때(나타남·사라짐·자리 비움)와
//!   종료할 때 지금 시각을 쓰고, 숨기 중에는 쓰지 않아 숨기 전 시각에서 멈춥니다. 상대가 비공개로 했으면 그 표시도 남겨
//!   끊긴 뒤에도 가립니다. 숨기·비공개는 받는 쪽 데몬이 지켜 주는 협조적 가림입니다(옛 빌드는 모름).
//! - 오프라인 모드는 데몬을 망 없이 다시 띄웁니다(`restart_requested`, main.rs). Arti는 같은 프로세스에서 다시 만들 수
//!   없고, 잠재워도 가드 연결을 닫지 않아서입니다.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tracing::debug;
use yangpa_core::ids::{DeviceId, UserId};
use yangpa_core::time::Timestamp;
use yangpa_core::wire::{PresenceShare, PresenceStatus, WireErrorCode, YpRequest, YpResponse};
use yangpa_net::PeerId;
use yangpa_rpc::RpcError;
use yangpa_rpc::api::{Notification, PresenceMode, PresenceSetParams, PresenceView};
use yangpa_store::Store;

use crate::node::Node;

const MODE_KEY: &str = "presence.mode";
const HIDE_KEY: &str = "presence.hide_last_seen";
const SEEN_PREFIX: &str = "presence.seen.";
/// [`backfill`]을 했다는 표시(`presence.seen.`으로 시작하지 않게).
const BACKFILL_KEY: &str = "presence.backfilled";
/// 인증 뒤 상대 상태를 기다리는 시간(그동안은 오프라인으로 보임).
const GRACE: Duration = Duration::from_secs(3);
/// 기간이 끝났는지 보는 간격.
const TICK: Duration = Duration::from_secs(15);
const MAX_DURATION_SECS: u64 = 365 * 24 * 3600;
/// 오프라인 모드에서 막는 호출(망 없이 뜬 데몬이 직접 연결로 나가지 않게).
const OFFLINE_BLOCKED: [&str; 5] = [
    "supporter.github_begin",
    "supporter.patreon_begin",
    "supporter.refresh",
    "update.check",
    "update.download",
];

#[derive(Clone, Copy, Default, Serialize, Deserialize)]
struct Saved {
    mode: PresenceMode,
    #[serde(default)]
    until: Option<Timestamp>,
}

impl Saved {
    fn expired(&self, now: Timestamp) -> bool {
        self.until.is_some_and(|u| u <= now)
    }
}

/// 사용자의 마지막 동기화(`presence.seen.<사용자>`).
#[derive(Clone, Copy, Serialize, Deserialize)]
struct Seen {
    at: Timestamp,
    #[serde(default)]
    hidden: bool,
}

/// 인증된 상대 기기의 상태. `status`가 없으면 아직 받지 못함.
struct Remote {
    status: Option<PresenceStatus>,
    hide: Option<bool>,
    since: Instant,
}

/// 화면에 보일 사용자 상태.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Shown {
    Offline,
    Online,
    Away,
}

impl Shown {
    pub(crate) fn online(self) -> bool {
        self != Shown::Offline
    }

    pub(crate) fn away(self) -> bool {
        self == Shown::Away
    }
}

#[derive(Default)]
pub struct PresenceState {
    idle: AtomicBool,
    /// 이 프로세스가 망 없이 떴음(시작할 때 정함).
    offline: AtomicBool,
    /// 종료한 뒤 데몬을 다시 띄울지(오프라인 모드로 들어가거나 나올 때).
    restart: AtomicBool,
    devices: Mutex<HashMap<DeviceId, Remote>>,
    /// 마지막으로 알린 사용자 상태(오프라인은 빼고 둠). 바뀔 때만 알리고 시각을 남깁니다.
    shown: Mutex<HashMap<UserId, Shown>>,
    /// 마지막으로 모두에게 보낸 내 상태.
    sent: Mutex<Option<PresenceShare>>,
    /// 지금 표시 상태를 보내는 중인 상대와, 그동안 다시 보낼 일이 생겼는지([`send`]).
    sending: Mutex<HashMap<PeerId, bool>>,
}

fn saved(store: &Store) -> Saved {
    store
        .get_setting::<Saved>(MODE_KEY)
        .ok()
        .flatten()
        .filter(|s| !s.expired(Timestamp::now()))
        .unwrap_or_default()
}

/// 저장된 오프라인 모드가 아직 유효한지(망을 띄우기 전에 [`Node::start_with_hub`](crate::Node)가 봄).
pub(crate) fn offline_at_start(store: &Store) -> bool {
    saved(store).mode == PresenceMode::Offline
}

pub(crate) fn start(node: &Arc<Node>, offline: bool) {
    node.presence.offline.store(offline, Ordering::Relaxed);
    backfill(node);
    let weak = Arc::downgrade(node);
    node.tasks.spawn(async move {
        loop {
            tokio::time::sleep(TICK).await;
            let Some(node) = weak.upgrade() else { break };
            if node.is_shutting_down() {
                break;
            }
            expire(&node);
        }
    });
}

/// 이 기능 전의 기록: 처음 한 번 노드 기록의 마지막 연결 성공 시각으로 채웁니다(업데이트 직후 오프라인인 사람의 카드가
/// 비지 않게). 숨기가 생기기 전 기록이라 드러날 것이 없습니다.
fn backfill(node: &Node) {
    let Ok(me) = node.me() else { return };
    if node
        .store
        .get_setting::<bool>(BACKFILL_KEY)
        .ok()
        .flatten()
        .is_some()
    {
        return;
    }
    let mut latest: HashMap<UserId, Timestamp> = HashMap::new();
    for r in node.store.identified_peers().unwrap_or_default() {
        if let (Some(u), Some(t)) = (r.user, r.last_success)
            && u != me.user()
        {
            let e = latest.entry(u).or_insert(t);
            *e = (*e).max(t);
        }
    }
    for (u, at) in latest {
        if load_seen(&node.store, &u).is_none() {
            save_seen(&node.store, &u, Seen { at, hidden: false });
        }
    }
    let _ = node.store.put_setting(BACKFILL_KEY, &true);
}

/// 기간이 끝난 모드를 온라인으로 돌립니다.
fn expire(node: &Arc<Node>) {
    let Ok(Some(s)) = node.store.get_setting::<Saved>(MODE_KEY) else {
        return;
    };
    if !s.expired(Timestamp::now()) {
        return;
    }
    let _ = node.store.delete_setting(MODE_KEY);
    changed(node);
}

pub(crate) fn offline(node: &Node) -> bool {
    node.presence.offline.load(Ordering::Relaxed)
}

/// 오프라인 모드에서 막는 메서드면 그 오류.
pub(crate) fn check_blocked(node: &Node, method: &str) -> std::result::Result<(), RpcError> {
    if offline(node) && OFFLINE_BLOCKED.contains(&method) {
        return Err(RpcError::network("오프라인 모드에서는 할 수 없습니다"));
    }
    Ok(())
}

/// 종료한 뒤 데몬을 다시 띄워야 하는지(main.rs).
pub(crate) fn restart_requested(node: &Node) -> bool {
    node.presence.restart.load(Ordering::Relaxed)
}

/// `after` 뒤에 데몬을 멈춰 같은 인자로 다시 뜨게 합니다.
pub(crate) fn request_restart(node: &Arc<Node>, after: Duration) {
    node.presence.restart.store(true, Ordering::Relaxed);
    let n = node.clone();
    tokio::spawn(async move {
        tokio::time::sleep(after).await;
        n.shutdown().await;
    });
}

fn hide_last_seen(node: &Node) -> bool {
    node.store
        .get_setting::<bool>(HIDE_KEY)
        .ok()
        .flatten()
        .unwrap_or(false)
}

pub(crate) fn view(node: &Node) -> PresenceView {
    let s = saved(&node.store);
    PresenceView {
        mode: s.mode,
        until: s.until,
        idle: node.presence.idle.load(Ordering::Relaxed),
        hide_last_seen: hide_last_seen(node),
        offline: offline(node),
    }
}

/// `presence.set`.
pub(crate) fn set(
    node: &Arc<Node>,
    p: PresenceSetParams,
) -> std::result::Result<PresenceView, RpcError> {
    let before = view(node);
    if let Some(mode) = p.mode {
        let until = match (mode, p.duration_secs) {
            (PresenceMode::Online, _) | (_, None) => None,
            (_, Some(s)) if s == 0 || s > MAX_DURATION_SECS => {
                return Err(RpcError::invalid_input(
                    "기간은 1초에서 365일 사이여야 합니다",
                ));
            }
            (_, Some(s)) => Some(Timestamp(Timestamp::now().0.saturating_add(s * 1000))),
        };
        let r = if mode == PresenceMode::Online {
            node.store.delete_setting(MODE_KEY).map(|_| ())
        } else {
            node.store.put_setting(MODE_KEY, &Saved { mode, until })
        };
        r.map_err(|e| RpcError::internal(format!("표시 상태 저장 실패: {e}")))?;
    }
    if let Some(h) = p.hide_last_seen {
        node.store
            .put_setting(HIDE_KEY, &h)
            .map_err(|e| RpcError::internal(format!("표시 상태 저장 실패: {e}")))?;
    }
    if let Some(i) = p.idle {
        node.presence.idle.store(i, Ordering::Relaxed);
    }
    let after = view(node);
    // 앱이 입력 없음을 자주 다시 알리므로 바뀐 것이 없으면 알리지 않습니다.
    if after != before {
        changed(node);
    }
    Ok(after)
}

/// 내 상태가 바뀌었을 수 있음: 앱에 알리고, 오프라인 모드가 바뀌었으면 데몬을 다시 띄우고, 아니면 상대 기기에 보냅니다.
fn changed(node: &Arc<Node>) {
    let v = view(node);
    if (v.mode == PresenceMode::Offline) != v.offline {
        // 응답이 먼저 나가도록 조금 뒤에 멈춥니다(`system.shutdown`과 같음).
        request_restart(node, Duration::from_millis(200));
    }
    node.notify.publish(Notification::PresenceSelf(v));
    let Ok(me) = node.me() else { return };
    let mine = my_shown(node);
    node.notify.publish(Notification::Presence {
        user: me.user(),
        online: mine.online(),
        away: mine.away(),
        last_seen: None,
    });
    let share = my_share(node);
    if node.presence.sent.lock().replace(share) == Some(share) {
        return;
    }
    for (peer, user, device) in node.peers.authed_peers() {
        if user != me.user() && !crate::archive::archive_only(node, &user, &device) {
            send(node, peer);
        }
    }
}

fn my_share(node: &Node) -> PresenceShare {
    let status = match saved(&node.store).mode {
        PresenceMode::Online if node.presence.idle.load(Ordering::Relaxed) => PresenceStatus::Away,
        PresenceMode::Online => PresenceStatus::Online,
        PresenceMode::Away => PresenceStatus::Away,
        PresenceMode::Invisible | PresenceMode::Offline => PresenceStatus::Invisible,
    };
    PresenceShare {
        status,
        hide_last_seen: hide_last_seen(node),
    }
}

/// 내 화면에 보일 내 상태(숨기·오프라인은 오프라인).
fn my_shown(node: &Node) -> Shown {
    if offline(node) {
        return Shown::Offline;
    }
    combine([Some(my_share(node).status)])
}

/// 보낼 때의 내 상태를 보냅니다. 한 상대에게는 하나씩 보내고, 보내는 중에 또 부르면 끝난 뒤 한 번 더 보냅니다: 먼저
/// 만든 낡은 요청이 나중에 닿아 새 상태(예: 숨기)를 덮지 않게.
fn send(node: &Arc<Node>, peer: PeerId) {
    {
        let mut m = node.presence.sending.lock();
        if let Some(again) = m.get_mut(&peer) {
            *again = true;
            return;
        }
        m.insert(peer, false);
    }
    node.spawn_detached(move |node| async move {
        loop {
            let share = my_share(&node);
            match crate::identity::request_after_auth(&node, peer, YpRequest::Presence(share)).await
            {
                Ok(YpResponse::Ok) => {}
                // 옛 빌드는 이 요청을 모릅니다.
                Ok(_) => debug!(peer = %peer, "표시 상태를 받지 않음"),
                Err(e) => debug!(peer = %peer, error = %e, "표시 상태 보내기 실패"),
            }
            let mut m = node.presence.sending.lock();
            if m.remove(&peer) != Some(true) {
                break;
            }
            m.insert(peer, false);
        }
    });
}

/// 인증된 상대 기기(`peers::remember`): 내 상태를 보내고, 상대 상태를 [`GRACE`]만큼 기다렸다가 다시 봅니다.
pub(crate) fn on_authed(node: &Arc<Node>, peer: PeerId, user: UserId, device: DeviceId) {
    let Ok(me) = node.me() else { return };
    if user == me.user() {
        return;
    }
    node.presence
        .devices
        .lock()
        .entry(device)
        .or_insert_with(|| Remote {
            status: None,
            hide: None,
            since: Instant::now(),
        });
    if !crate::archive::archive_only(node, &user, &device) {
        send(node, peer);
    }
    node.spawn_detached(move |node| async move {
        tokio::time::sleep(GRACE).await;
        refresh_user(&node, user);
    });
}

/// 인증된 연결이 끊김(`peers::on_disconnected`·`drop_untrusted_user`). 그 기기의 연결이 더 없으면 상태를 잊습니다.
pub(crate) fn on_gone(node: &Node, user: UserId, device: DeviceId) {
    if node.peers.authed_peer_of_device(&device).is_none() {
        node.presence.devices.lock().remove(&device);
    }
    refresh_user(node, user);
}

/// 받은 `YpRequest::Presence`.
pub(crate) fn handle(node: &Node, peer: PeerId, s: PresenceShare) -> YpResponse {
    let Some((user, device)) = node.peers.identity_of(&peer) else {
        return YpResponse::error(WireErrorCode::Unauthenticated, "인증되지 않음");
    };
    node.presence.devices.lock().insert(
        device,
        Remote {
            status: Some(s.status),
            hide: Some(s.hide_last_seen),
            since: Instant::now(),
        },
    );
    // 비공개가 바뀌었으면 끊긴 뒤에도 그대로 가리도록 기록에 반영합니다.
    if let Some(mut seen) = load_seen(&node.store, &user) {
        let hidden = known_hide(node, &user).unwrap_or(seen.hidden);
        if hidden != seen.hidden {
            seen.hidden = hidden;
            save_seen(&node.store, &user, seen);
        }
    }
    refresh_user(node, user);
    YpResponse::Ok
}

/// 보이는 상태를 다시 계산해 바뀌었으면 시각을 남기고 앱에 알립니다.
pub(crate) fn refresh_user(node: &Node, user: UserId) {
    let Ok(me) = node.me() else { return };
    if user == me.user() {
        return;
    }
    // 계산부터 알림까지 한 번에 하나씩: 먼저 계산한 낡은 상태가 나중에 덮어 숨긴 사람을 다시 알리지 않게.
    let mut m = node.presence.shown.lock();
    let now = shown(node, &user);
    let prev = if now == Shown::Offline {
        m.remove(&user)
    } else {
        m.insert(user, now)
    }
    .unwrap_or(Shown::Offline);
    if prev == now {
        return;
    }
    // 둘 중 하나는 보이는 상태였으므로 지금이 그 사람과 보이게 이어져 있던 마지막 때입니다.
    record_seen(node, &user);
    node.notify.publish(Notification::Presence {
        user,
        online: now.online(),
        away: now.away(),
        last_seen: last_seen(node, &user),
    });
}

/// 종료 직전: 보이는 상태로 이어져 있던 사람들의 시각을 남깁니다.
pub(crate) fn on_shutdown(node: &Node) {
    let users: Vec<UserId> = node.presence.shown.lock().keys().copied().collect();
    for u in users {
        record_seen(node, &u);
    }
}

fn combine(states: impl IntoIterator<Item = Option<PresenceStatus>>) -> Shown {
    let mut out = Shown::Offline;
    for s in states.into_iter().flatten() {
        match s {
            PresenceStatus::Online => return Shown::Online,
            PresenceStatus::Away => out = Shown::Away,
            PresenceStatus::Invisible => {}
        }
    }
    out
}

/// 기기 하나의 상태. 아직 받지 못했으면 [`GRACE`] 동안은 없음(보이지 않음), 그 뒤는 온라인(옛 빌드).
fn device_status(r: Option<&Remote>) -> Option<PresenceStatus> {
    let r = r?;
    r.status
        .or_else(|| (r.since.elapsed() >= GRACE).then_some(PresenceStatus::Online))
}

fn shown(node: &Node, user: &UserId) -> Shown {
    let devices: Vec<DeviceId> = node
        .peers
        .authed_peers()
        .into_iter()
        .filter(|(_, u, _)| u == user)
        .map(|(_, _, d)| d)
        .collect();
    let remote = node.presence.devices.lock();
    combine(devices.iter().map(|d| device_status(remote.get(d))))
}

/// 목록용: 화면에 보이는 사용자(오프라인은 빠짐, 나는 내 표시 상태).
pub(crate) fn shown_all(node: &Node) -> HashMap<UserId, Shown> {
    let mut by_user: HashMap<UserId, Vec<Option<PresenceStatus>>> = HashMap::new();
    {
        let remote = node.presence.devices.lock();
        for (_, u, d) in node.peers.authed_peers() {
            by_user
                .entry(u)
                .or_default()
                .push(device_status(remote.get(&d)));
        }
    }
    let mut out: HashMap<UserId, Shown> = by_user
        .into_iter()
        .map(|(u, s)| (u, combine(s)))
        .filter(|(_, s)| s.online())
        .collect();
    if let Ok(me) = node.me() {
        out.remove(&me.user());
        let mine = my_shown(node);
        if mine.online() {
            out.insert(me.user(), mine);
        }
    }
    out
}

/// 지금 연결된 그 사람 기기가 알려 준 비공개 여부(한 기기라도 비공개면 비공개). 받은 것이 없으면 없음.
fn known_hide(node: &Node, user: &UserId) -> Option<bool> {
    let devices: Vec<DeviceId> = node
        .peers
        .authed_peers()
        .into_iter()
        .filter(|(_, u, _)| u == user)
        .map(|(_, _, d)| d)
        .collect();
    let remote = node.presence.devices.lock();
    devices
        .iter()
        .filter_map(|d| remote.get(d).and_then(|r| r.hide))
        .reduce(|a, b| a || b)
}

fn seen_key(user: &UserId) -> String {
    format!("{SEEN_PREFIX}{user}")
}

fn load_seen(store: &Store, user: &UserId) -> Option<Seen> {
    store.get_setting(&seen_key(user)).ok().flatten()
}

fn save_seen(store: &Store, user: &UserId, seen: Seen) {
    if let Err(e) = store.put_setting(&seen_key(user), &seen) {
        debug!(error = %e, "마지막 동기화 시각 저장 실패");
    }
}

fn record_seen(node: &Node, user: &UserId) {
    let prev = load_seen(&node.store, user);
    let hidden = known_hide(node, user).unwrap_or(prev.is_some_and(|s| s.hidden));
    save_seen(
        &node.store,
        user,
        Seen {
            at: Timestamp::now(),
            hidden,
        },
    );
}

/// 그 사람과 마지막으로 동기화한 시각(ms). 비공개이거나 기록이 없으면 없음.
pub(crate) fn last_seen(node: &Node, user: &UserId) -> Option<u64> {
    load_seen(&node.store, user)
        .filter(|s| !s.hidden)
        .map(|s| s.at.0)
}

/// 목록용 [`last_seen`] 전부(비공개는 빠짐).
pub(crate) fn last_seen_all(node: &Node) -> HashMap<UserId, u64> {
    node.store
        .settings_with_prefix(SEEN_PREFIX)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(k, v)| {
            let user = k.strip_prefix(SEEN_PREFIX)?.parse::<UserId>().ok()?;
            let seen: Seen = serde_json::from_value(v).ok()?;
            (!seen.hidden).then_some((user, seen.at.0))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn combine_prefers_online_and_skips_invisible() {
        use PresenceStatus::*;
        assert_eq!(combine([]), Shown::Offline);
        assert_eq!(combine([Some(Invisible)]), Shown::Offline);
        assert_eq!(combine([None, Some(Invisible)]), Shown::Offline);
        assert_eq!(combine([Some(Invisible), Some(Away)]), Shown::Away);
        assert_eq!(combine([Some(Away), Some(Online)]), Shown::Online);
    }

    #[test]
    fn unconfirmed_device_waits_for_grace() {
        let fresh = Remote {
            status: None,
            hide: None,
            since: Instant::now(),
        };
        assert_eq!(device_status(Some(&fresh)), None);
        let old = Remote {
            since: Instant::now() - GRACE,
            ..fresh
        };
        assert_eq!(device_status(Some(&old)), Some(PresenceStatus::Online));
        assert_eq!(device_status(None), None);
    }

    #[test]
    fn expired_mode_falls_back() {
        let s = Saved {
            mode: PresenceMode::Away,
            until: Some(Timestamp(10)),
        };
        assert!(s.expired(Timestamp(10)));
        assert!(!s.expired(Timestamp(9)));
        assert!(!Saved::default().expired(Timestamp(u64::MAX)));
    }
}
