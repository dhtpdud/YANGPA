//! 모더레이션(W2): 밴·차단·신고.
//!
//! - 밴: 크루 상태 이벤트(`MemberBan`·`MemberUnban`, 상태 키로 암호화)라 해소기가 모든 노드에서 같은 결과를 냅니다.
//!   여기서는 `delete_since`가 실린 밴이 적용될 때 그 사용자의 최근 메시지를 이 노드에서 지우는 일만 합니다.
//! - 차단: 이 기기에만 저장하고 아무에게도 알리지 않습니다(설정 `mod.blocked.<사용자>`, RAM 사본). 보기에서 접고,
//!   새 메시지 알림을 내지 않고, 친구 요청을 조용히 거절하고, 첨부를 자동으로 받지 않고, 시더로서 메시지를 주지 않습니다.
//! - 기기 차단: 남의 계정에서 기기 하나만 막습니다(설정 `mod.blocked_dev.<기기>`). 그 기기의 인증을 거절해 이 노드와
//!   주고받지 않게 하고, 그 기기가 쓴 메시지는 사용자 차단처럼 접고 알림·첨부 자동 받기·입력 표시를 막습니다. 믿지 못할
//!   기기를 끊는 용도입니다. 키 그룹은 바꾸지 않으므로 그 기기가 이미 받은 키·기록과 다른 노드가 전하는 것은
//!   막지 못합니다(그 계정의 활성 기기가 폐기해야 끝남).
//! - 신고: 그 채널을 볼 수 있는 운영진(추방·밴·메시지 관리 권한) 활성 기기마다 따로 봉인해 직접 보냅니다(공간 이벤트가
//!   아니므로 다른 멤버는 모름). 받은 신고와 처리 결과는 운영진 기기에만 남습니다. 휘발성 채널의 신고는 양쪽 다 RAM에만 둡니다.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use base64::Engine;
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use tracing::debug;
use yangpa_core::body::EventBody;
use yangpa_core::device::BoxPublicKey;
use yangpa_core::ids::{ChannelId, DeviceId, EventId, SpaceId, UserId};
use yangpa_core::model::{Permissions, SpaceKind, SpaceState};
use yangpa_core::time::Timestamp;
use yangpa_core::wire::{ModerationRequest, WireErrorCode, YpRequest, YpResponse};
use yangpa_crypto::{open_sealed, seal_to};
use yangpa_rpc::api::{Notification, ReportStatus, ReportView};
use yangpa_store::RequestDirection;
use yangpa_sync::state::limits::MAX_REASON_CHARS;
use yangpa_sync::state::{DecodedEvent, ResolveUpdate};
use zeroize::Zeroizing;

use crate::error::{Error, Result};
use crate::node::Node;
use crate::{messages, peers};

/// `member.ban`의 `delete_recent`가 지우는 구간(밴 시각 앞 24시간).
pub(crate) const DELETE_RECENT_WINDOW: Duration = Duration::from_secs(24 * 60 * 60);
/// 신고를 받는 권한(하나라도 있으면 운영진).
pub(crate) const MODERATOR_PERMS: [Permissions; 3] = [
    Permissions::KICK_MEMBERS,
    Permissions::BAN_MEMBERS,
    Permissions::MANAGE_MESSAGES,
];
/// 공간마다 운영진 기기에 열어 두는 신고 수 상한(넘으면 보낸 쪽이 나중에 다시 보냄).
const MAX_OPEN_PER_SPACE: usize = 1000;
/// 공간마다 남겨 두는 처리한 신고 수(넘으면 오래 전에 처리한 것부터 지움).
const MAX_RESOLVED_PER_SPACE: usize = 1000;
/// 한 사람이 한 공간에 열어 둘 수 있는 신고 수(도배 방지, 넘으면 보낸 쪽이 나중에 다시 보냄).
const MAX_OPEN_PER_REPORTER: usize = 50;
/// 상한에 걸린 신고를 다시 보내 달라고 할 때의 간격.
const BUSY_RETRY_MS: u32 = 60_000;
/// 차단한 사람이 쓴 친구 링크를 기억하는 수(같은 사람이 다시 보내면 처음과 같은 답을 주려고).
const MAX_SILENT_LINKS: usize = 1024;
/// 공간마다 숨길 수 있는 메시지 수.
const MAX_HIDDEN_PER_SPACE: usize = 10_000;
/// 차단할 수 있는 사용자 수.
const MAX_BLOCKED: usize = 10_000;
/// 처리 결과 메모 최대 글자 수.
pub(crate) const MAX_NOTE_CHARS: usize = 512;
/// 보내지 못한 신고를 다시 보내는 간격과 포기하는 나이.
const RETRY_INTERVAL: Duration = Duration::from_secs(5);
const OUTBOX_TTL_MS: u64 = 30 * 24 * 60 * 60 * 1000;
/// 봉인한 신고 크기 상한(이유 512자와 ID 몇 개면 1KiB 안쪽).
const MAX_SEALED: usize = 16 * 1024;

const BLOCKED_PREFIX: &str = "mod.blocked.";
const BLOCKED_DEVICE_PREFIX: &str = "mod.blocked_dev.";
const HIDDEN_PREFIX: &str = "mod.hidden.";
const REPORT_PREFIX: &str = "mod.report.";
const SENT_PREFIX: &str = "mod.sent.";
const OUTBOX_PREFIX: &str = "mod.outbox.";
/// 최근 메시지 삭제를 끝낸 밴 이벤트(`mod.bandel.<공간>.<이벤트>`). 다시 세울 때 되풀이하지 않게.
const BAN_DONE_PREFIX: &str = "mod.bandel.";
const AD_REPORT: &[u8] = b"yangpa/v1/report";
const AD_RESOLVED: &[u8] = b"yangpa/v1/report-resolved";

/// 모더레이션 영역의 노드 수준 상태(`Node::moderation`).
#[derive(Default)]
pub struct ModerationState {
    /// 차단한 사용자 → 차단 시각(설정의 사본). 알림 거르개도 같은 지도를 봅니다.
    blocked: Arc<RwLock<HashMap<UserId, Timestamp>>>,
    /// 차단한 남의 기기 → (그 사용자, 차단 시각). 기기 ID는 사용자마다 겹치지 않으므로 기기로 찾습니다.
    blocked_devices: Arc<RwLock<HashMap<DeviceId, (UserId, Timestamp)>>>,
    blocked_loaded: AtomicBool,
    /// 이 기기에서 숨긴 메시지(신고하며 숨김). 휘발성 채널 것은 RAM에만.
    hidden: Arc<RwLock<HashMap<SpaceId, HashSet<EventId>>>>,
    /// 해소기에 넣은 밴 중 최근 메시지 삭제가 실린 것(밴 이벤트 ID별).
    ban_deletes: Mutex<HashMap<SpaceId, HashMap<EventId, BanDelete>>>,
    /// 휘발성 채널 신고(받은 쪽): 공간 → 신고 ID → 기록. 디스크에 닿지 않게 RAM에만.
    volatile_reports: Mutex<HashMap<SpaceId, BTreeMap<String, ReportRecord>>>,
    /// 휘발성 채널 신고(보낸 쪽): (공간, 메시지) → 신고 ID.
    volatile_sent: Mutex<HashMap<(SpaceId, EventId), String>>,
    /// 보내지 못한 휘발성 채널 신고(RAM).
    volatile_outbox: Mutex<HashMap<String, Outgoing>>,
    /// 차단한 사람이 쓴 친구 링크(사용자, 토큰 해시). 다시 보내면 처음과 같이 '대기 중'으로 답합니다.
    silent_links: Mutex<HashSet<(UserId, [u8; 32])>>,
    /// 다시 보내기를 한 번에 하나만.
    flush_lock: tokio::sync::Mutex<()>,
}

/// 밴에 실린 최근 메시지 삭제 요청.
#[derive(Clone, Copy)]
struct BanDelete {
    id: EventId,
    user: UserId,
    since: Timestamp,
    /// 밴 이벤트 시각. 지울 구간의 끝이자 삭제 표시 시각입니다.
    at: Timestamp,
}

/// 운영진 기기에 남는 신고.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct ReportRecord {
    report_id: String,
    space: SpaceId,
    channel: ChannelId,
    message: EventId,
    reporter: UserId,
    reason: String,
    reported_at: Timestamp,
    #[serde(default)]
    resolution: Option<String>,
    #[serde(default)]
    resolved_at: Option<Timestamp>,
    #[serde(default)]
    resolved_by: Option<UserId>,
}

impl ReportRecord {
    fn view(&self) -> ReportView {
        ReportView {
            report_id: self.report_id.clone(),
            space: self.space,
            channel: self.channel,
            message: self.message,
            reporter: self.reporter,
            reason: self.reason.clone(),
            reported_at: self.reported_at,
            status: if self.resolution.is_some() {
                ReportStatus::Resolved
            } else {
                ReportStatus::Open
            },
            resolution: self.resolution.clone(),
        }
    }
}

/// 봉인해 보내는 신고 내용. 공간은 요청 칸과 AD로, 신고한 사람은 인증한 연결로 정해집니다.
#[derive(Serialize, Deserialize)]
struct ReportPayload {
    report_id: String,
    channel: ChannelId,
    message: EventId,
    reason: String,
    reported_at: Timestamp,
}

/// 다른 운영진에게 보내는 처리 결과.
#[derive(Serialize, Deserialize)]
struct ResolvedPayload {
    report_id: String,
    resolution: String,
    resolved_at: Timestamp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum OutKind {
    Report,
    Resolved,
}

/// 아직 전하지 못한 봉인 묶음(받는 기기의 키로 봉인돼 있어 이 기기도 다시 열 수 없음).
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Outgoing {
    space: SpaceId,
    device: DeviceId,
    kind: OutKind,
    /// 신고된 채널. 받을 사람이 그 채널을 더는 못 보게 되면 버립니다.
    #[serde(default)]
    channel: Option<ChannelId>,
    /// base64.
    sealed: String,
    created_at: Timestamp,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
struct BlockRecord {
    blocked_at: Timestamp,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
struct DeviceBlockRecord {
    user: UserId,
    blocked_at: Timestamp,
}

#[derive(Clone, Serialize, Deserialize)]
struct SentRecord {
    report_id: String,
}

// ───────────── 시작 ─────────────

pub(crate) fn start(node: &Arc<Node>) {
    load_blocked(node);
    load_hidden(node);
    // 차단한 사용자의 새 메시지와 숨긴 메시지는 알림으로 내보내지 않습니다(목록에는 접혀서 남음).
    let blocked = node.moderation.blocked.clone();
    let devices = node.moderation.blocked_devices.clone();
    let hidden = node.moderation.hidden.clone();
    let author_blocked = move |m: &yangpa_rpc::api::MessageView| {
        blocked.read().contains_key(&m.author) || devices.read().contains_key(&m.author_device)
    };
    // 휘발성 채널 보기처럼 `author_blocked`를 모르고 만든 메시지 보기도 여기서 채웁니다.
    node.notify.set_filter(Box::new(move |n| match n {
        Notification::MessageNew { space, message, .. } => {
            !author_blocked(message)
                && !hidden
                    .read()
                    .get(space)
                    .is_some_and(|s| s.contains(&message.id))
        }
        Notification::MessageUpdated { space, message, .. } => {
            message.author_blocked = author_blocked(message);
            !hidden
                .read()
                .get(space)
                .is_some_and(|s| s.contains(&message.id))
        }
        _ => true,
    }));
    let weak = Arc::downgrade(node);
    node.tasks.spawn(async move {
        let mut tick = tokio::time::interval(RETRY_INTERVAL);
        tick.tick().await;
        loop {
            tick.tick().await;
            let Some(node) = weak.upgrade() else { break };
            if node.is_shutting_down() {
                break;
            }
            flush_outbox(&node, false).await;
        }
    });
}

fn load_blocked(node: &Node) {
    if node.moderation.blocked_loaded.swap(true, Ordering::AcqRel) {
        return;
    }
    let Ok(rows) = node.store.settings_with_prefix(BLOCKED_PREFIX) else {
        node.moderation
            .blocked_loaded
            .store(false, Ordering::Release);
        return;
    };
    let mut map = node.moderation.blocked.write();
    for (key, value) in rows {
        let user = key
            .strip_prefix(BLOCKED_PREFIX)
            .and_then(|s| s.parse::<UserId>().ok());
        let rec = serde_json::from_value::<BlockRecord>(value).ok();
        if let (Some(user), Some(rec)) = (user, rec) {
            map.insert(user, rec.blocked_at);
        }
    }
    drop(map);
    let Ok(rows) = node.store.settings_with_prefix(BLOCKED_DEVICE_PREFIX) else {
        return;
    };
    let mut map = node.moderation.blocked_devices.write();
    for (key, value) in rows {
        let device = key
            .strip_prefix(BLOCKED_DEVICE_PREFIX)
            .and_then(|s| s.parse::<DeviceId>().ok());
        let rec = serde_json::from_value::<DeviceBlockRecord>(value).ok();
        if let (Some(device), Some(rec)) = (device, rec) {
            map.insert(device, (rec.user, rec.blocked_at));
        }
    }
}

fn load_hidden(node: &Node) {
    let Ok(rows) = node.store.settings_with_prefix(HIDDEN_PREFIX) else {
        return;
    };
    let mut map = node.moderation.hidden.write();
    for (key, _) in rows {
        let Some((space, event)) = key
            .strip_prefix(HIDDEN_PREFIX)
            .and_then(|s| s.split_once('.'))
        else {
            continue;
        };
        if let (Ok(space), Ok(event)) = (space.parse::<SpaceId>(), event.parse::<EventId>()) {
            map.entry(space).or_default().insert(event);
        }
    }
}

// ───────────── 공용 판단 ─────────────

/// 운영진(신고를 받고 처리하는 멤버)인지.
/// 신고를 주고받을 수 있는 채널인지. 비공개 채널의 신고 이유가 그 채널을 못 보는 운영진에게 새지 않게 합니다.
fn can_view(state: &SpaceState, user: &UserId, channel: &ChannelId) -> bool {
    state
        .channel_permissions(user, channel)
        .contains(Permissions::VIEW_CHANNEL)
}

pub(crate) fn is_moderator(state: &SpaceState, user: &UserId) -> bool {
    let p = state.base_permissions(user);
    state.is_member(user) && MODERATOR_PERMS.iter().any(|x| p.contains(*x))
}

/// 내가 이 사용자를 차단했는지(RAM 조회).
pub(crate) fn is_blocked(node: &Node, user: &UserId) -> bool {
    load_blocked(node);
    node.moderation.blocked.read().contains_key(user)
}

/// 내가 이 기기를 차단했는지(RAM 조회).
pub(crate) fn is_device_blocked(node: &Node, device: &DeviceId) -> bool {
    load_blocked(node);
    node.moderation.blocked_devices.read().contains_key(device)
}

/// 작성자를 차단했는지: 그 사용자 전체나 그 글을 쓴 기기. 보기에서 접고 알림·첨부 자동 받기·입력 표시를 막는 곳에서 씁니다.
pub(crate) fn is_author_blocked(node: &Node, user: &UserId, device: &DeviceId) -> bool {
    is_blocked(node, user) || is_device_blocked(node, device)
}

/// 차단한 사용자 목록(안 읽은 수 계산 등).
pub(crate) fn blocked_users(node: &Node) -> Vec<UserId> {
    load_blocked(node);
    node.moderation.blocked.read().keys().copied().collect()
}

/// 이 공간에 이 기기에서 숨긴 메시지가 하나라도 있는지(목록을 채워 읽을지 고름).
pub(crate) fn has_hidden(node: &Node, space: &SpaceId) -> bool {
    node.moderation
        .hidden
        .read()
        .get(space)
        .is_some_and(|s| !s.is_empty())
}

/// 이 기기에서 숨긴 메시지인지.
pub(crate) fn is_hidden(node: &Node, space: &SpaceId, id: &EventId) -> bool {
    node.moderation
        .hidden
        .read()
        .get(space)
        .is_some_and(|s| s.contains(id))
}

// ───────────── 차단 ─────────────

pub(crate) fn block(node: &Node, user: &UserId) -> Result<()> {
    let me = node.me()?;
    if *user == me.user() {
        return Err(Error::invalid("자기 자신은 차단할 수 없습니다"));
    }
    load_blocked(node);
    if node.moderation.blocked.read().contains_key(user) {
        return Ok(());
    }
    if node.moderation.blocked.read().len() >= MAX_BLOCKED {
        return Err(Error::invalid(format!(
            "차단은 {MAX_BLOCKED}명까지 할 수 있습니다"
        )));
    }
    let now = Timestamp::now();
    node.store.put_setting(
        &format!("{BLOCKED_PREFIX}{user}"),
        &BlockRecord { blocked_at: now },
    )?;
    node.moderation.blocked.write().insert(*user, now);
    // 오가던 친구 요청은 지웁니다(받은 요청은 사라지고, 보낸 요청을 상대가 수락해도 이어지지 않음).
    for dir in [RequestDirection::Incoming, RequestDirection::Outgoing] {
        for r in node.store.list_friend_requests(dir)? {
            if r.user == *user {
                node.store.delete_friend_request(&r.request_id)?;
            }
        }
    }
    Ok(())
}

pub(crate) fn unblock(node: &Node, user: &UserId) -> Result<()> {
    load_blocked(node);
    node.store
        .delete_setting(&format!("{BLOCKED_PREFIX}{user}"))?;
    node.moderation.blocked.write().remove(user);
    Ok(())
}

/// 남의 기기 하나를 차단합니다. 내 기기는 차단 대신 폐기합니다(`device.revoke`). 인증된 연결을 잊어 바로 끊습니다.
pub(crate) fn block_device(node: &Node, user: &UserId, device: &DeviceId) -> Result<()> {
    let me = node.me()?;
    if *user == me.user() {
        return Err(Error::invalid(
            "내 기기는 차단할 수 없습니다(사용자 설정 > 기기에서 폐기하세요)",
        ));
    }
    load_blocked(node);
    if node.moderation.blocked_devices.read().contains_key(device) {
        return Ok(());
    }
    if node.moderation.blocked_devices.read().len() >= MAX_BLOCKED {
        return Err(Error::invalid(format!(
            "기기 차단은 {MAX_BLOCKED}대까지 할 수 있습니다"
        )));
    }
    let now = Timestamp::now();
    node.store.put_setting(
        &format!("{BLOCKED_DEVICE_PREFIX}{device}"),
        &DeviceBlockRecord {
            user: *user,
            blocked_at: now,
        },
    )?;
    node.moderation
        .blocked_devices
        .write()
        .insert(*device, (*user, now));
    node.peers.forget_devices(&[*device]);
    Ok(())
}

pub(crate) fn unblock_device(node: &Node, device: &DeviceId) -> Result<()> {
    load_blocked(node);
    node.store
        .delete_setting(&format!("{BLOCKED_DEVICE_PREFIX}{device}"))?;
    node.moderation.blocked_devices.write().remove(device);
    Ok(())
}

/// 기기 차단 목록(최근 것부터): (기기, 사용자, 차단 시각).
pub(crate) fn blocked_device_list(node: &Node) -> Vec<(DeviceId, UserId, Timestamp)> {
    load_blocked(node);
    let mut v: Vec<(DeviceId, UserId, Timestamp)> = node
        .moderation
        .blocked_devices
        .read()
        .iter()
        .map(|(d, (u, t))| (*d, *u, *t))
        .collect();
    v.sort_by(|a, b| b.2.cmp(&a.2).then(a.0.cmp(&b.0)));
    v
}

/// 차단 목록(최근 것부터).
pub(crate) fn blocked_list(node: &Node) -> Vec<(UserId, Timestamp)> {
    load_blocked(node);
    let mut v: Vec<(UserId, Timestamp)> = node
        .moderation
        .blocked
        .read()
        .iter()
        .map(|(u, t)| (*u, *t))
        .collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    v
}

/// 차단한 사용자가 친구 링크를 쓰려 할 때의 답. 차단을 드러내지 않도록 링크 사용(1회용 소모·사용 횟수)과 응답은
/// 보통 요청과 똑같이 하되, 요청을 남기지도 알림을 내지도 않습니다(상대는 수락을 영영 기다리는 것처럼 보임).
/// 같은 사람이 같은 링크로 다시 보내면 보통 흐름처럼 링크를 다시 쓰지 않고 '대기 중'으로 답합니다.
pub(crate) fn silent_friend_refusal(
    node: &Node,
    user: &UserId,
    token_hash: &[u8; 32],
) -> YpResponse {
    let key = (*user, *token_hash);
    if node.moderation.silent_links.lock().contains(&key) {
        return YpResponse::FriendPending;
    }
    match node.store.redeem_friend_link(token_hash, Timestamp::now()) {
        Ok(yangpa_store::RedeemOutcome::Accepted) => {
            let mut seen = node.moderation.silent_links.lock();
            if seen.len() >= MAX_SILENT_LINKS {
                seen.clear();
            }
            seen.insert(key);
            YpResponse::FriendPending
        }
        Ok(yangpa_store::RedeemOutcome::Expired) => {
            YpResponse::error(WireErrorCode::Expired, "만료된 링크")
        }
        Ok(_) => YpResponse::error(WireErrorCode::InvalidToken, "쓸 수 없는 링크"),
        Err(_) => YpResponse::error(WireErrorCode::Internal, "저장소 오류"),
    }
}

/// 시더로서 차단한 사용자에게 메시지 레인(메시지·첨부·이모지)을 주지 않습니다. 정책 검사를 통과한 뒤에만 불러,
/// 다른 이유로 거절될 요청은 그 이유 그대로 답합니다(차단 여부가 응답으로 드러나지 않게, 거절 문구는 정책의
/// '제공하지 않는 데이터'와 같음).
pub(crate) fn withholds(node: &Node, requester: &UserId, lane: yangpa_core::event::Lane) -> bool {
    lane == yangpa_core::event::Lane::Message && is_blocked(node, requester)
}

// ───────────── 숨김 ─────────────

fn hide(node: &Node, space: &SpaceId, id: &EventId, volatile: bool) -> Result<()> {
    {
        let mut map = node.moderation.hidden.write();
        let set = map.entry(*space).or_default();
        if set.contains(id) {
            return Ok(());
        }
        if set.len() >= MAX_HIDDEN_PER_SPACE {
            return Err(Error::invalid(format!(
                "공간마다 {MAX_HIDDEN_PER_SPACE}개까지 숨길 수 있습니다"
            )));
        }
        set.insert(*id);
    }
    if volatile {
        return Ok(());
    }
    node.store
        .put_setting(&format!("{HIDDEN_PREFIX}{space}.{id}"), &true)?;
    // 숨긴 메시지는 내가 시더로 남에게 주지도 않습니다(첨부 포함).
    if let Err(e) = crate::policy::set_excluded(node, space, id, true) {
        debug!(error = %e, "숨긴 메시지를 제공 범위에서 빼지 못함");
    }
    Ok(())
}

// ───────────── 밴: 최근 메시지 삭제 ─────────────

/// 해소기에 넣기 직전(적용 잠금 안, `crews::on_decoded`에서). 최근 메시지 삭제가 실린 밴을 기억합니다.
pub(crate) fn on_decoded(node: &Node, events: &[DecodedEvent]) {
    for ev in events {
        if let EventBody::MemberBan {
            user,
            delete_since: Some(since),
            ..
        } = &ev.body
        {
            node.moderation
                .ban_deletes
                .lock()
                .entry(ev.header.space)
                .or_default()
                .insert(
                    ev.id,
                    BanDelete {
                        id: ev.id,
                        user: *user,
                        since: *since,
                        at: ev.header.created_at,
                    },
                );
        }
    }
}

/// 해소 뒤(적용 잠금 안, `crews::on_resolved`에서). 새로 적용된 밴이면 그 사용자가 `delete_since`부터 밴 시각까지
/// 쓴 메시지를 이 노드에서 지웁니다(삭제 이벤트를 따로 내지 않아도 밴을 받은 노드마다 같은 일을 함). 거부된 밴은
/// 잊습니다. 구간 끝을 밴 시각으로 묶어 밴이 풀린 뒤 다시 들어와 쓴 메시지는 건드리지 않고, 끝낸 밴은 기록해 두어
/// 시작 때 상태를 다시 세우며 되풀이하지 않습니다.
pub(crate) fn on_resolved(
    node: &Node,
    space: &SpaceId,
    state: Option<&SpaceState>,
    upd: &ResolveUpdate,
    notify: bool,
) -> Result<()> {
    let todo: Vec<BanDelete> = {
        let mut all = node.moderation.ban_deletes.lock();
        let Some(map) = all.get_mut(space) else {
            return Ok(());
        };
        for (id, _) in &upd.rejected {
            map.remove(id);
        }
        let todo = upd.applied.iter().filter_map(|id| map.remove(id)).collect();
        if map.is_empty() {
            all.remove(space);
        }
        todo
    };
    if todo.is_empty() {
        return Ok(());
    }
    let me = node.me()?.user();
    for BanDelete {
        id: ban,
        user,
        since,
        at,
    } in todo
    {
        let done = format!("{BAN_DONE_PREFIX}{space}.{ban}");
        if node.store.get_setting::<bool>(&done)?.is_some() {
            continue;
        }
        let ids = node
            .store
            .messages_by_author_between(space, &user, since, at)?;
        let count = ids.len();
        for (id, channel) in ids {
            node.store.mark_message_deleted(space, &id, at)?;
            let visible = state.is_some_and(|st| {
                st.channel_permissions(&me, &channel)
                    .contains(Permissions::VIEW_CHANNEL)
            });
            if notify
                && visible
                && let Some(rec) = node.store.get_message(&id, Some(&me))?
            {
                node.notify.publish(Notification::MessageUpdated {
                    space: *space,
                    channel,
                    message: messages::view_of(node, &rec),
                });
            }
        }
        node.store.put_setting(&done, &true)?;
        debug!(space = %space.short(), count, "밴과 함께 최근 메시지 삭제");
    }
    Ok(())
}

// ───────────── 신고: 보내는 쪽 ─────────────

/// 신고할 메시지의 정보(보내는 쪽이 아는 것).
pub(crate) struct ReportTarget {
    pub space: SpaceId,
    pub channel: ChannelId,
    pub message: EventId,
    pub author: UserId,
    pub volatile: bool,
}

/// 신고를 운영진 기기들에 봉인해 보냅니다. 같은 메시지를 다시 신고하면 앞 신고 ID를 돌려줍니다.
pub(crate) async fn report(
    node: &Arc<Node>,
    state: &SpaceState,
    t: ReportTarget,
    reason: &str,
    hide_it: bool,
) -> Result<String> {
    let me = node.me()?;
    let sent_key = format!("{SENT_PREFIX}{}.{}", t.space, t.message);
    let existing = if t.volatile {
        node.moderation
            .volatile_sent
            .lock()
            .get(&(t.space, t.message))
            .cloned()
    } else {
        node.store
            .get_setting::<SentRecord>(&sent_key)?
            .map(|r| r.report_id)
    };
    if let Some(id) = existing {
        if hide_it {
            hide(node, &t.space, &t.message, t.volatile)?;
        }
        return Ok(id);
    }
    // 신고당한 사람이 운영진이어도 그 사람에게는 보내지 않습니다(신고한 사람을 지킴).
    let me_device = me.device_id();
    let targets: Vec<(DeviceId, BoxPublicKey)> = crate::devkeys::sealing_keys(
        node,
        state
            .members
            .keys()
            .filter(|u| **u != t.author && is_moderator(state, u) && can_view(state, u, &t.channel))
            .flat_map(|u| state.member_active_devices(u).map(move |d| (u, d)))
            .filter(|(_, d)| d.device != me_device),
    );
    let i_moderate = is_moderator(state, &me.user()) && me.user() != t.author;
    if targets.is_empty() && !i_moderate {
        return Err(Error::invalid(
            "이 채널을 볼 수 있는 운영진(추방·밴·메시지 관리 권한)이 없어 신고를 받을 사람이 없습니다",
        ));
    }
    let report_id = bs58::encode(rand::random::<[u8; 16]>()).into_string();
    let now = Timestamp::now();
    let payload = ReportPayload {
        report_id: report_id.clone(),
        channel: t.channel,
        message: t.message,
        reason: reason.to_owned(),
        reported_at: now,
    };
    let plain = Zeroizing::new(
        postcard::to_allocvec(&payload).map_err(|_| Error::internal("신고 직렬화 실패"))?,
    );
    // 내가 운영진이면 내 기기에도 남깁니다(다른 내 기기는 위 대상에 들어 있음).
    if i_moderate {
        store_report(
            node,
            ReportRecord {
                report_id: report_id.clone(),
                space: t.space,
                channel: t.channel,
                message: t.message,
                reporter: me.user(),
                reason: reason.to_owned(),
                reported_at: now,
                resolution: None,
                resolved_at: None,
                resolved_by: None,
            },
            t.volatile,
        )?;
    }
    let mut items = Vec::new();
    for (device, key) in targets {
        let sealed = seal_to(
            &[(device, key)],
            &ad(AD_REPORT, &t.space, &me.device_id(), &device),
            &plain,
        );
        items.push(Outgoing {
            space: t.space,
            device,
            kind: OutKind::Report,
            channel: Some(t.channel),
            sealed: b64(&sealed),
            created_at: now,
        });
    }
    if t.volatile {
        node.moderation
            .volatile_sent
            .lock()
            .insert((t.space, t.message), report_id.clone());
    } else {
        node.store.put_setting(
            &sent_key,
            &SentRecord {
                report_id: report_id.clone(),
            },
        )?;
    }
    enqueue(node, items, t.volatile)?;
    if hide_it {
        hide(node, &t.space, &t.message, t.volatile)?;
    }
    Ok(report_id)
}

/// 처리 결과를 나 말고 그 채널을 볼 수 있는 다른 운영진 기기에 보냅니다(신고당한 사람은 운영진이어도 뺌).
fn send_resolved(
    node: &Arc<Node>,
    state: &SpaceState,
    rec: &ReportRecord,
    author: Option<&UserId>,
    volatile: bool,
) -> Result<()> {
    let me = node.me()?;
    let payload = ResolvedPayload {
        report_id: rec.report_id.clone(),
        resolution: rec.resolution.clone().unwrap_or_default(),
        resolved_at: rec.resolved_at.unwrap_or_else(Timestamp::now),
    };
    let plain = Zeroizing::new(
        postcard::to_allocvec(&payload).map_err(|_| Error::internal("처리 결과 직렬화 실패"))?,
    );
    let mut items = Vec::new();
    for user in state.members.keys() {
        if Some(user) == author
            || !is_moderator(state, user)
            || !can_view(state, user, &rec.channel)
        {
            continue;
        }
        let recs = state
            .member_active_devices(user)
            .filter(|d| d.device != me.device_id())
            .map(|d| (user, d));
        for (device, box_key) in crate::devkeys::sealing_keys(node, recs) {
            let sealed = seal_to(
                &[(device, box_key)],
                &ad(AD_RESOLVED, &rec.space, &me.device_id(), &device),
                &plain,
            );
            items.push(Outgoing {
                space: rec.space,
                device,
                kind: OutKind::Resolved,
                channel: Some(rec.channel),
                sealed: b64(&sealed),
                created_at: Timestamp::now(),
            });
        }
    }
    enqueue(node, items, volatile)
}

/// 보낼 묶음을 대기열에 넣고(휘발성은 RAM) 곧바로 한 번 보내 봅니다. 못 보낸 것은 주기적으로 다시 보냅니다.
fn enqueue(node: &Arc<Node>, items: Vec<Outgoing>, volatile: bool) -> Result<()> {
    if items.is_empty() {
        return Ok(());
    }
    for item in items {
        let id = bs58::encode(rand::random::<[u8; 16]>()).into_string();
        if volatile {
            node.moderation.volatile_outbox.lock().insert(id, item);
        } else {
            node.store
                .put_setting(&format!("{OUTBOX_PREFIX}{id}"), &item)?;
        }
    }
    node.spawn_detached(|node| async move {
        flush_outbox(&node, true).await;
    });
    Ok(())
}

/// 대기열을 보냅니다. `dial`이면 아직 인증된 연결이 없는 기기도 주소 기록으로 찾아 연결합니다.
async fn flush_outbox(node: &Arc<Node>, dial: bool) {
    let _g = node.moderation.flush_lock.lock().await;
    let mut items: Vec<(String, Outgoing, bool)> = node
        .moderation
        .volatile_outbox
        .lock()
        .iter()
        .map(|(k, v)| (k.clone(), v.clone(), true))
        .collect();
    if let Ok(rows) = node.store.settings_with_prefix(OUTBOX_PREFIX) {
        for (key, value) in rows {
            let Some(id) = key.strip_prefix(OUTBOX_PREFIX) else {
                continue;
            };
            match serde_json::from_value::<Outgoing>(value) {
                Ok(o) => items.push((id.to_owned(), o, false)),
                Err(_) => {
                    let _ = node.store.delete_setting(&key);
                }
            }
        }
    }
    let now = Timestamp::now();
    for (id, item, volatile) in items {
        let keep = deliver(node, &item, now, dial).await;
        if !keep {
            if volatile {
                node.moderation.volatile_outbox.lock().remove(&id);
            } else {
                let _ = node.store.delete_setting(&format!("{OUTBOX_PREFIX}{id}"));
            }
        }
    }
}

/// 묶음 하나를 보내 봅니다. 대기열에 남길지 돌려줍니다.
async fn deliver(node: &Arc<Node>, item: &Outgoing, now: Timestamp, dial: bool) -> bool {
    if now.0.saturating_sub(item.created_at.0) > OUTBOX_TTL_MS {
        return false;
    }
    let Ok(me) = node.me() else { return true };
    let Some(state) = node.space_state(&item.space) else {
        // 이 기기에서 지운 공간이면 버립니다(시작 때 다시 세우기 전이라 상태만 없는 것이면 남김).
        return node.spaces.read().contains_key(&item.space);
    };
    // 내가 나갔거나 받을 기기가 더는 멤버의 활성 기기가 아니면 버립니다.
    if !state.is_member(&me.user()) {
        return false;
    }
    let owner = state
        .devices
        .iter()
        .find(|(_, l)| l.contains_active(&item.device))
        .map(|(u, _)| *u);
    let Some(owner) = owner.filter(|u| state.is_member(u)) else {
        return false;
    };
    if !is_moderator(&state, &owner)
        || item
            .channel
            .is_some_and(|c| !state.channels.contains_key(&c) || !can_view(&state, &owner, &c))
    {
        return false;
    }
    let peer = if dial {
        peers::peer_for_device(node, &item.device)
    } else {
        node.peers.authed_peer_of_device(&item.device)
    };
    let Some(peer) = peer else { return true };
    if peers::ensure_auth(node, peer).await.is_err() {
        return true;
    }
    let Ok(sealed) = base64::engine::general_purpose::STANDARD.decode(&item.sealed) else {
        return false;
    };
    let req = match item.kind {
        OutKind::Report => ModerationRequest::Report {
            space: item.space,
            sealed,
        },
        OutKind::Resolved => ModerationRequest::Resolved {
            space: item.space,
            sealed,
        },
    };
    match node.net.request(peer, YpRequest::Moderation(req)).await {
        Ok(resp) => {
            if let YpResponse::Error { code, .. } = &resp {
                debug!(?code, "신고 전달 거절");
            }
            keep_after(&resp)
        }
        Err(e) => {
            debug!(error = %e, "신고 전달 실패(다시 보냄)");
            true
        }
    }
}

/// 받는 쪽 답을 보고 묶음을 대기열에 남길지(다시 보낼지) 정합니다.
fn keep_after(resp: &YpResponse) -> bool {
    match resp {
        YpResponse::Ok => false,
        // 상한에 걸림: 운영진이 신고를 처리하면 자리가 생기므로 나중에 다시 보냅니다.
        YpResponse::Busy { .. } => true,
        // 형식이 틀린 것만 다시 보내도 같으므로 버립니다. 모르는 공간·채널, 멤버 아님, 권한 없음은 받는 쪽 상태가
        // 아직 따라오지 않았을 수 있어(방금 들어온 신고자, 막 바뀐 역할) 다시 보냅니다. 이 기기의 상태로도 보낼
        // 까닭이 없어지면 `deliver`의 앞 검사에서 버립니다.
        YpResponse::Error { code, .. } => *code != WireErrorCode::BadRequest,
        _ => false,
    }
}

// ───────────── 신고: 받는 쪽 ─────────────

/// 들어온 `YpRequest::Moderation`. 인증한 같은 공간 멤버의 활성 기기에서 온 것만 받습니다.
pub(crate) async fn handle_request(
    node: &Arc<Node>,
    peer: yangpa_net::PeerId,
    req: ModerationRequest,
) -> YpResponse {
    let Some((user, device)) = node.peers.identity_of(&peer) else {
        return YpResponse::error(WireErrorCode::Unauthenticated, "인증 전");
    };
    let Ok(me) = node.me() else {
        return YpResponse::error(WireErrorCode::Unauthenticated, "신원 없음");
    };
    let (space, sealed, tag) = match &req {
        ModerationRequest::Report { space, sealed } => (*space, sealed, AD_REPORT),
        ModerationRequest::Resolved { space, sealed } => (*space, sealed, AD_RESOLVED),
    };
    let Some(state) = node.space_state(&space) else {
        return YpResponse::error(WireErrorCode::NotFound, "모르는 공간");
    };
    if !state.is_member(&me.user()) || !state.is_member(&user) {
        return YpResponse::error(WireErrorCode::NotMember, "공간 멤버가 아님");
    }
    if !state.device_belongs(&user, &device) {
        return YpResponse::error(WireErrorCode::NotMember, "활성 기기가 아님");
    }
    if state.kind != SpaceKind::Crew || !is_moderator(&state, &me.user()) {
        return YpResponse::error(WireErrorCode::Forbidden, "운영진이 아님");
    }
    if sealed.len() > MAX_SEALED {
        return YpResponse::error(WireErrorCode::BadRequest, "너무 큼");
    }
    let Ok(plain) = open_sealed(
        &me.device_id(),
        &me.device.boxkey,
        &ad(tag, &space, &device, &me.device_id()),
        sealed,
    ) else {
        return YpResponse::error(WireErrorCode::BadRequest, "열 수 없음");
    };
    match req {
        ModerationRequest::Report { .. } => {
            let Ok(p) = postcard::from_bytes::<ReportPayload>(&plain) else {
                return YpResponse::error(WireErrorCode::BadRequest, "형식");
            };
            receive_report(node, &state, space, user, p)
        }
        ModerationRequest::Resolved { .. } => {
            if !is_moderator(&state, &user) {
                return YpResponse::error(WireErrorCode::Forbidden, "운영진이 아님");
            }
            let Ok(p) = postcard::from_bytes::<ResolvedPayload>(&plain) else {
                return YpResponse::error(WireErrorCode::BadRequest, "형식");
            };
            receive_resolved(node, space, user, p)
        }
    }
}

fn valid_report_id(id: &str) -> bool {
    (1..=32).contains(&id.len()) && bs58::decode(id).into_vec().is_ok()
}

fn receive_report(
    node: &Arc<Node>,
    state: &SpaceState,
    space: SpaceId,
    reporter: UserId,
    p: ReportPayload,
) -> YpResponse {
    if !valid_report_id(&p.report_id)
        || crate::crews::check_text(&p.reason, MAX_REASON_CHARS, "신고 사유").is_err()
        || p.reason.trim().is_empty()
    {
        return YpResponse::error(WireErrorCode::BadRequest, "형식");
    }
    let Some(ch) = state.channels.get(&p.channel) else {
        return YpResponse::error(WireErrorCode::NotFound, "없는 채널");
    };
    // 보낸 사람도 받는 나도 그 채널을 볼 수 있어야 합니다(보내는 쪽 규칙과 같음).
    let Ok(me) = node.me() else {
        return YpResponse::error(WireErrorCode::Unauthenticated, "신원 없음");
    };
    if !can_view(state, &reporter, &p.channel) || !can_view(state, &me.user(), &p.channel) {
        return YpResponse::error(WireErrorCode::Forbidden, "볼 수 없는 채널");
    }
    let volatile = ch.is_volatile();
    let existing = load_reports(node, &space);
    if existing.iter().any(|r| r.report_id == p.report_id) {
        return YpResponse::Ok;
    }
    // 열린 신고만 셉니다(처리한 신고는 따로 잘라 냄). 넘으면 거절하지 않고 나중에 다시 보내게 합니다.
    let open: Vec<&ReportRecord> = existing.iter().filter(|r| r.resolution.is_none()).collect();
    let open_by_reporter = open.iter().filter(|r| r.reporter == reporter).count();
    if open.len() >= MAX_OPEN_PER_SPACE || open_by_reporter >= MAX_OPEN_PER_REPORTER {
        return YpResponse::Busy {
            retry_after_ms: BUSY_RETRY_MS,
        };
    }
    let rec = ReportRecord {
        report_id: p.report_id,
        space,
        channel: p.channel,
        message: p.message,
        reporter,
        reason: p.reason,
        reported_at: p.reported_at,
        resolution: None,
        resolved_at: None,
        resolved_by: None,
    };
    if store_report(node, rec, volatile).is_err() {
        return YpResponse::error(WireErrorCode::Internal, "저장소 오류");
    }
    YpResponse::Ok
}

fn receive_resolved(
    node: &Arc<Node>,
    space: SpaceId,
    by: UserId,
    p: ResolvedPayload,
) -> YpResponse {
    if !valid_report_id(&p.report_id)
        || crate::crews::check_text(&p.resolution, MAX_NOTE_CHARS + 32, "처리 결과").is_err()
    {
        return YpResponse::error(WireErrorCode::BadRequest, "형식");
    }
    let Some((mut rec, volatile)) = find_report(node, &space, &p.report_id) else {
        // 모르는 신고(받기 전에 운영진이 됨 등)의 결과는 그냥 넘깁니다.
        return YpResponse::Ok;
    };
    if rec.resolution.is_some() {
        return YpResponse::Ok;
    }
    rec.resolution = Some(p.resolution);
    rec.resolved_at = Some(p.resolved_at);
    rec.resolved_by = Some(by);
    match put_report(node, &rec, volatile) {
        Ok(()) => {
            prune_resolved(node, &space);
            YpResponse::Ok
        }
        Err(_) => YpResponse::error(WireErrorCode::Internal, "저장소 오류"),
    }
}

/// 처리한 신고가 공간마다 [`MAX_RESOLVED_PER_SPACE`]개를 넘으면 오래 전에 처리한 것부터 지웁니다.
fn prune_resolved(node: &Node, space: &SpaceId) {
    let drop = resolved_overflow(load_reports(node, space), MAX_RESOLVED_PER_SPACE);
    if drop.is_empty() {
        return;
    }
    let mut ram = node.moderation.volatile_reports.lock();
    for r in &drop {
        let in_ram = ram
            .get_mut(space)
            .and_then(|m| m.remove(&r.report_id))
            .is_some();
        if !in_ram {
            let _ = node.store.delete_setting(&report_key(space, &r.report_id));
        }
    }
}

/// 처리한 신고 중 `keep`개를 넘는 것(처리한 지 오래된 것부터). 열린 신고는 건드리지 않습니다.
fn resolved_overflow(records: Vec<ReportRecord>, keep: usize) -> Vec<ReportRecord> {
    let mut resolved: Vec<ReportRecord> = records
        .into_iter()
        .filter(|r| r.resolution.is_some())
        .collect();
    if resolved.len() <= keep {
        return Vec::new();
    }
    resolved.sort_by(|a, b| {
        a.resolved_at
            .cmp(&b.resolved_at)
            .then_with(|| a.report_id.cmp(&b.report_id))
    });
    resolved.truncate(resolved.len() - keep);
    resolved
}

fn report_key(space: &SpaceId, id: &str) -> String {
    format!("{REPORT_PREFIX}{space}.{id}")
}

/// 새 신고를 저장하고 운영진 화면에 알립니다.
fn store_report(node: &Node, rec: ReportRecord, volatile: bool) -> Result<()> {
    let (space, id) = (rec.space, rec.report_id.clone());
    put_report(node, &rec, volatile)?;
    node.notify.publish(Notification::ReportNew {
        space,
        report_id: id,
    });
    Ok(())
}

fn put_report(node: &Node, rec: &ReportRecord, volatile: bool) -> Result<()> {
    if volatile {
        node.moderation
            .volatile_reports
            .lock()
            .entry(rec.space)
            .or_default()
            .insert(rec.report_id.clone(), rec.clone());
        return Ok(());
    }
    node.store
        .put_setting(&report_key(&rec.space, &rec.report_id), rec)?;
    Ok(())
}

/// 이 공간의 신고 전부(저장된 것과 RAM의 휘발성 채널 것).
fn load_reports(node: &Node, space: &SpaceId) -> Vec<ReportRecord> {
    let mut out: Vec<ReportRecord> = node
        .store
        .settings_with_prefix(&format!("{REPORT_PREFIX}{space}."))
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(_, v)| serde_json::from_value(v).ok())
        .collect();
    if let Some(m) = node.moderation.volatile_reports.lock().get(space) {
        out.extend(m.values().cloned());
    }
    out
}

fn find_report(node: &Node, space: &SpaceId, id: &str) -> Option<(ReportRecord, bool)> {
    if let Some(r) = node
        .moderation
        .volatile_reports
        .lock()
        .get(space)
        .and_then(|m| m.get(id))
    {
        return Some((r.clone(), true));
    }
    node.store
        .get_setting::<ReportRecord>(&report_key(space, id))
        .ok()
        .flatten()
        .map(|r| (r, false))
}

/// `report.list`: 최근 것부터.
pub(crate) fn list_reports(
    node: &Node,
    space: &SpaceId,
    status: Option<ReportStatus>,
) -> Vec<ReportView> {
    let mut v: Vec<ReportView> = load_reports(node, space)
        .iter()
        .map(ReportRecord::view)
        .filter(|r| status.is_none_or(|s| r.status == s))
        .collect();
    v.sort_by(|a, b| {
        b.reported_at
            .cmp(&a.reported_at)
            .then_with(|| a.report_id.cmp(&b.report_id))
    });
    v
}

/// 처리할 신고를 찾습니다(열린 것만). 휘발성 채널 신고인지도 돌려줍니다.
pub(crate) fn open_report(node: &Node, space: &SpaceId, id: &str) -> Result<(ReportView, bool)> {
    let (rec, volatile) =
        find_report(node, space, id).ok_or_else(|| Error::not_found("없는 신고"))?;
    if rec.resolution.is_some() {
        return Err(Error::conflict("이미 처리한 신고입니다"));
    }
    Ok((rec.view(), volatile))
}

/// 처리 결과를 기록하고 다른 운영진에게 알립니다.
pub(crate) fn mark_resolved(
    node: &Arc<Node>,
    state: &SpaceState,
    space: &SpaceId,
    id: &str,
    resolution: String,
    author: Option<&UserId>,
) -> Result<()> {
    let me = node.me()?;
    let (mut rec, volatile) =
        find_report(node, space, id).ok_or_else(|| Error::not_found("없는 신고"))?;
    rec.resolution = Some(resolution);
    rec.resolved_at = Some(Timestamp::now());
    rec.resolved_by = Some(me.user());
    put_report(node, &rec, volatile)?;
    prune_resolved(node, space);
    send_resolved(node, state, &rec, author, volatile)
}

// ───────────── 공간 정리 ─────────────

/// 이 기기에서 공간을 지울 때(`spaces::discard_space_locked`) 그 공간의 모더레이션 기록을 모두 지웁니다: 받은 신고,
/// 보낸 기록, 숨김, 밴 삭제 기록, 보내지 못한 묶음(RAM 것 포함).
pub(crate) fn forget_space(node: &Node, space: &SpaceId) {
    for prefix in [REPORT_PREFIX, SENT_PREFIX, HIDDEN_PREFIX, BAN_DONE_PREFIX] {
        if let Ok(rows) = node
            .store
            .settings_with_prefix(&format!("{prefix}{space}."))
        {
            for (key, _) in rows {
                let _ = node.store.delete_setting(&key);
            }
        }
    }
    if let Ok(rows) = node.store.settings_with_prefix(OUTBOX_PREFIX) {
        for (key, value) in rows {
            if serde_json::from_value::<Outgoing>(value).is_ok_and(|o| o.space == *space) {
                let _ = node.store.delete_setting(&key);
            }
        }
    }
    let m = &node.moderation;
    m.hidden.write().remove(space);
    m.ban_deletes.lock().remove(space);
    m.volatile_reports.lock().remove(space);
    m.volatile_sent.lock().retain(|(s, _), _| s != space);
    m.volatile_outbox.lock().retain(|_, o| o.space != *space);
}

/// 이 기기가 폐기돼 로컬 데이터를 모두 지울 때(`identity::wipe`) RAM 사본도 비웁니다. 저장소 쪽은 `Store::wipe_all`이
/// 지우므로, 차단 목록은 '읽어 둠' 상태를 그대로 두어 빈 목록으로 남깁니다(같은 프로세스에서 새 신원을 만들 때 옛 차단이
/// 이어지지 않게).
pub(crate) fn wipe_all(node: &Node) {
    let m = &node.moderation;
    m.blocked.write().clear();
    m.blocked_devices.write().clear();
    m.hidden.write().clear();
    m.ban_deletes.lock().clear();
    m.volatile_reports.lock().clear();
    m.volatile_sent.lock().clear();
    m.volatile_outbox.lock().clear();
    m.silent_links.lock().clear();
}

// ───────────── 도우미 ─────────────

/// 봉인 AD: 종류 ‖ 공간 ‖ 보낸 기기 ‖ 받는 기기. 다른 공간·다른 기기 사이로 옮겨 쓰지 못하게 묶습니다.
fn ad(tag: &[u8], space: &SpaceId, from: &DeviceId, to: &DeviceId) -> Vec<u8> {
    let mut v = tag.to_vec();
    v.extend_from_slice(space.as_bytes());
    v.extend_from_slice(from.as_bytes());
    v.extend_from_slice(to.as_bytes());
    v
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// 이 공간의 신고 기록 수(시험용).
pub(crate) fn report_count(node: &Node, space: &SpaceId) -> usize {
    load_reports(node, space).len()
}

/// 이 공간에 대해 저장소에 남은 모더레이션 기록 수(받은 신고·보낸 기록·숨김·밴 삭제 기록, 시험용).
pub(crate) fn stored_for_space(node: &Node, space: &SpaceId) -> usize {
    [REPORT_PREFIX, SENT_PREFIX, HIDDEN_PREFIX, BAN_DONE_PREFIX]
        .iter()
        .map(|p| {
            node.store
                .settings_with_prefix(&format!("{p}{space}."))
                .map(|v| v.len())
                .unwrap_or(0)
        })
        .sum()
}

/// RAM에 든 차단 목록 크기(시험용). 저장소를 다시 읽지 않습니다.
pub(crate) fn blocked_len(node: &Node) -> usize {
    node.moderation.blocked.read().len()
}

/// 대기열에 남은 신고 묶음 수(디스크 + RAM, 시험용).
pub(crate) fn outbox_len(node: &Node) -> usize {
    node.moderation.volatile_outbox.lock().len()
        + node
            .store
            .settings_with_prefix(OUTBOX_PREFIX)
            .map(|v| v.len())
            .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(id: &str, resolved_at: Option<u64>) -> ReportRecord {
        ReportRecord {
            report_id: id.to_owned(),
            space: SpaceId::default(),
            channel: ChannelId::default(),
            message: EventId::default(),
            reporter: UserId::default(),
            reason: "이유".into(),
            reported_at: Timestamp(1),
            resolution: resolved_at.map(|_| "dismiss".to_owned()),
            resolved_at: resolved_at.map(Timestamp),
            resolved_by: None,
        }
    }

    #[test]
    fn retries_until_receiver_state_catches_up() {
        // 받는 쪽 상태가 아직 따라오지 않아 생기는 거절과 상한은 다시 보냅니다.
        for code in [
            WireErrorCode::NotMember,
            WireErrorCode::NotFound,
            WireErrorCode::Forbidden,
            WireErrorCode::Unauthenticated,
            WireErrorCode::Internal,
        ] {
            assert!(keep_after(&YpResponse::error(code, "x")), "{code:?}");
        }
        assert!(keep_after(&YpResponse::Busy {
            retry_after_ms: BUSY_RETRY_MS
        }));
        // 받았거나 형식이 틀린 것은 끝.
        assert!(!keep_after(&YpResponse::Ok));
        assert!(!keep_after(&YpResponse::error(
            WireErrorCode::BadRequest,
            "형식"
        )));
    }

    #[test]
    fn prunes_only_oldest_resolved_reports() {
        let mut all = vec![rec("open1", None), rec("open2", None)];
        all.extend((0..5).map(|i| rec(&format!("r{i}"), Some(100 - i))));
        let drop = resolved_overflow(all.clone(), 3);
        let ids: Vec<&str> = drop.iter().map(|r| r.report_id.as_str()).collect();
        // 처리 시각이 가장 이른 둘(r4=96, r3=97)만 지우고 열린 신고는 그대로.
        assert_eq!(ids, vec!["r4", "r3"]);
        assert!(resolved_overflow(all, 5).is_empty());
    }
}
