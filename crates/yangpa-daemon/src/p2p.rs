//! Tor 밖 직접 연결의 이 기기 쪽 일: P2P 전용 채널 참여 동의(설정 `p2p.joined`), 채널 보기 채우기, 만들 때 검사,
//! 그리고 신뢰 스위치(`crate::trust`)·참여 채널에 따라 기기끼리 잇는 두 스웜.
//!
//! 채널이 P2P 전용인지는 공간 상태(`SpaceState::is_p2p`, 상태 이벤트 `ChannelTransportSet`)가 정하고, 이 기기가 그
//! 채널에 참여하는지(서로 IP가 보인다는 경고를 받아들였는지)는 이 기기에만 있는 설정입니다.
//!
//! 스웜 둘(모두 Tor 스웜과 다른 임시 키, 띄울 때마다 새 키):
//! - 직접 스웜: 참여한 채널이나 신뢰하는 사람·크루, 내 다른 기기가 있는 동안, 기기 연결 링크를 띄우거나 찍은 동안(직접
//!   TCP·QUIC).
//! - 출구 스웜: 나를 신뢰한 상대에게 Tor 출구를 거쳐 걸 때만(받지 않음, Arti는 주 스웜 것을 함께 씀).
//!
//! 기기끼리는 Tor로 인증된 연결에서 [`P2pHello`]로 노드와 신뢰를 알립니다. 주소(IP)는 내가 신뢰하거나 함께 참여한
//! 채널이 있는 기기에게만 줍니다. 기기 쌍마다(나 → 상대 신뢰, 상대 → 나 신뢰, 함께 참여한 채널):
//! - 서로 신뢰하거나 함께 참여한 채널: 직접 스웜끼리.
//! - 상대만 나를 신뢰: 내 출구 스웜이 상대 직접 스웜으로(상대 IP만 보이고 나는 숨음). 반대면 상대가 그렇게 걸어 옴.
//! - 그 밖: Tor로만.
//!
//! P2P 전용 채널의 채널 스트림(메시지·키)은 그 채널에 함께 참여한 기기와의 직접 연결로만 오가고, 일반 내용(공간
//! 스트림·Tor 채널·파일·휘발성)은 둘 중 하나라도 신뢰하는 연결이 있으면 그쪽으로 먼저, 아니면 Tor로 갑니다
//! ([`route_ok`], [`accepts`], [`split`], [`request`]). 인증·친구·통화 신호·안내는 Tor로 갑니다. 다만 P2P 전용 채널 통화의
//! WebRTC 신호(제안·응답·후보·화면 보기)는 그 채널에 함께 참여한 기기와의 연결로 먼저 보냅니다([`request_call_signal`]).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde_bytes::ByteBuf;
use tracing::{debug, info, warn};
use yangpa_core::body::EventBody;
use yangpa_core::event::{SignedEvent, StreamId};
use yangpa_core::ids::{ChannelId, DeviceId, SpaceId, UserId};
use yangpa_core::links::{DirectContact, MAX_DIRECT_ADDRS};
use yangpa_core::model::{
    ChannelKind, ChannelSettings, ChannelTransport, Permissions, SpaceKind, SpaceState,
};
use yangpa_core::wire::{
    MAX_EVENTS_PER_MESSAGE, MAX_P2P_ADDRS, MAX_P2P_CHANNELS, P2pHello, PeerAuth, SignalPayload,
    WireErrorCode, YpRequest, YpResponse,
};
use yangpa_net::{
    GossipVerdict, Incoming, KadMode, Keypair, Multiaddr, NetConfig, NetError, NetEvent, NetHandle,
    NetMode, PeerId, Protocol,
};
use yangpa_rpc::api::{ChannelView, ConnectionMode, Notification};

use crate::error::{Error, Result};
use crate::node::Node;
use crate::peers::MAX_AUTH_SKEW_MS;
use crate::{blobs, crews, devlink, identity, sync_service, volatile};

/// 참여 중인 (공간, 채널) 목록을 담은 설정 키.
const JOINED_SETTING: &str = "p2p.joined";
/// 참여 목록 상한(설정 한 줄이 끝없이 커지지 않게).
const MAX_JOINED: usize = 4096;

/// 참여 목록 읽고-고쳐-쓰기를 한 번에 하나만(동시에 온 참여·해제가 서로를 덮어쓰지 않게).
static WRITE: Mutex<()> = Mutex::new(());

fn write_guard() -> MutexGuard<'static, ()> {
    WRITE.lock().unwrap_or_else(|e| e.into_inner())
}

type Joined = BTreeSet<(SpaceId, ChannelId)>;

/// 같은 기기에 다시 걸기까지 기다리는 시간(출구로 거는 것은 시도마다 Tor 회로를 만듦).
const REDIAL_GAP: Duration = Duration::from_secs(5 * 60);
/// STUN 이름 풀기·응답을 기다리는 시간.
const STUN_WAIT: Duration = Duration::from_secs(3);
/// 한 번에 묻는 STUN 서버 수(둘 이상이면 대칭형 NAT를 알아봄).
const STUN_ASK: usize = 3;
/// 연결이 없을 때 STUN으로 얻은 공인 주소를 믿는 시간. 공유기는 쓰지 않는 UDP 대응을 보통 30초 넘게 두지만, 지나면
/// 다른 포트를 줄 수 있어 홀펀칭 전에 포트를 다시 열고 다시 묻습니다(ICE가 연결 직전에 주소를 모으는 것과 같은 순서).
const STUN_FRESH: Duration = Duration::from_secs(25);
/// 새로 연 직접 스웜이 수신 주소를 알릴 때까지 기다리는 시간.
const LISTEN_WAIT: Duration = Duration::from_secs(3);
/// 건 뒤 P2P 인증을 해 보는 횟수와 그 사이 기다림(상대가 내 안내를 막 받는 중일 수 있음).
const AUTH_TRIES: usize = 3;
const AUTH_RETRY: Duration = Duration::from_millis(500);
/// 홀펀칭을 받은 쪽이 구멍을 뚫으며 기다리는 횟수(한 번에 QUIC 핸드셰이크 시간만큼).
const PUNCH_TRIES: usize = 3;

/// Tor로 인증된 멤버 기기가 알려 준 P2P 쪽 노드. 모두 RAM에만 둡니다(상대 주소를 디스크에 남기지 않음).
struct Learned {
    user: UserId,
    /// 상대 직접 스웜(참여한 채널이나 신뢰가 있을 때 알려 옴).
    peer: Option<PeerId>,
    /// 상대 직접 스웜 주소(상대가 나를 신뢰하거나 함께 참여한 채널이 있을 때만 옴).
    addrs: Vec<Multiaddr>,
    /// 상대 출구 스웜(내가 상대를 신뢰할 때 상대가 숨은 채 이 노드로 걸어 옴).
    exit_peer: Option<PeerId>,
    /// 상대가 참여 중이라고 알려 온 채널 중 지금 맞는 것.
    channels: Joined,
    /// 상대가 나를 신뢰함(내게 IP를 보여도 됨).
    trusted: bool,
}

/// P2P 쪽 스웜과 참여자 기록. 모두 RAM에만 둡니다.
#[derive(Default)]
pub(crate) struct P2pState {
    /// 지금 도는 직접 스웜.
    net: parking_lot::RwLock<Option<NetHandle>>,
    /// 지금 도는 출구 스웜.
    exit: parking_lot::RwLock<Option<NetHandle>>,
    /// 스웜 띄우기·멈추기를 하나씩.
    life: tokio::sync::Mutex<()>,
    /// 참여 목록 캐시(이벤트마다 설정을 읽지 않게). `None`이면 아직 읽지 않음.
    joined: parking_lot::Mutex<Option<Joined>>,
    /// 기기마다 알게 된 노드.
    learned: parking_lot::Mutex<HashMap<DeviceId, Learned>>,
    /// P2P로 인증된 피어가 내 어느 스웜에 있는지.
    sides: parking_lot::Mutex<HashMap<PeerId, Side>>,
    /// 스웜마다 끊긴 뒤 아직 다시 이어지지 않은 피어. 인증을 주고받는 사이 끊긴 피어를 인증 표에 넣지 않으려고
    /// (`GONE_MAX`개까지, 넘으면 비움).
    gone: parking_lot::Mutex<HashSet<(Side, PeerId)>>,
    /// 인증된 직접 연결이 끊긴 수(시험: 끊긴 뒤 몇 ms 안에 다시 이어져 빈 표를 볼 수 없을 때가 있음).
    pub(crate) drops: std::sync::atomic::AtomicU64,
    /// (기기, 내 스웜, 상대 노드)마다 마지막으로 건 시각([`REDIAL_GAP`]). 쪽이나 상대 노드가 바뀌면(신뢰가 바뀜, 상대가
    /// 스웜을 다시 띄움) 바로 다시 겁니다.
    dialed: parking_lot::Mutex<HashMap<(DeviceId, Side, PeerId), Instant>>,
    /// 끊긴 뒤 곧 다시 거는 중인 기기([`redial_soon`]).
    redialing: parking_lot::Mutex<HashSet<DeviceId>>,
    /// 마지막으로 보낸 안내의 `seq`.
    seq: parking_lot::Mutex<u64>,
    /// 기기마다 받아들인 안내의 가장 큰 `seq`(그보다 작은 것은 늦게 닿은 옛 안내).
    seen: parking_lot::Mutex<HashMap<DeviceId, u64>>,
    /// 직접 스웜의 공인 주소를 STUN으로 물은 시각(그 포트에서 묻고 바로 열었음). 시험 설정처럼 STUN을 쓰지 않으면 `None`.
    stun_at: parking_lot::Mutex<Option<Instant>>,
    /// Tor 피어마다 마지막으로 보낸 안내와 전해졌는지(같은 것을 거듭 보내지 않고, 못 전한 것은 주기 작업이 다시 보냄).
    sent: parking_lot::Mutex<HashMap<PeerId, (P2pHello, bool)>>,
    auth_locks: parking_lot::Mutex<HashMap<PeerId, Arc<tokio::sync::Mutex<()>>>>,
}

fn load(node: &Node) -> Joined {
    let mut cache = node.p2p.joined.lock();
    if let Some(j) = &*cache {
        return j.clone();
    }
    match node
        .store
        .get_setting::<Vec<(SpaceId, ChannelId)>>(JOINED_SETTING)
    {
        Ok(v) => {
            let j: Joined = v.unwrap_or_default().into_iter().collect();
            *cache = Some(j.clone());
            j
        }
        Err(e) => {
            warn!(error = %e, "P2P 참여 목록을 읽지 못함");
            Joined::new()
        }
    }
}

fn save(node: &Node, joined: &Joined) -> Result<()> {
    if joined.is_empty() {
        node.store.delete_setting(JOINED_SETTING)?;
    } else {
        let v: Vec<_> = joined.iter().copied().collect();
        node.store.put_setting(JOINED_SETTING, &v)?;
    }
    *node.p2p.joined.lock() = Some(joined.clone());
    Ok(())
}

/// 이 기기가 그 P2P 전용 채널에 참여 중인지. 채널이 지금 P2P 전용이 아니면 거짓입니다.
pub(crate) fn p2p_joined(node: &Node, space: &SpaceId, channel: &ChannelId) -> bool {
    node.space_state(space).is_some_and(|s| s.is_p2p(channel))
        && load(node).contains(&(*space, *channel))
}

/// 채널 보기의 `p2p`·`p2p_joined`를 채웁니다(`channel.list`·`space.get`).
pub(crate) fn fill_channel_view(
    node: &Node,
    space: &SpaceId,
    state: &SpaceState,
    view: &mut ChannelView,
) {
    view.p2p = state.is_p2p(&view.channel);
    view.p2p_joined = view.p2p && p2p_joined(node, space, &view.channel);
}

/// 채널을 P2P 전용으로 둘 수 있는지. 해소기 규칙(텍스트·음성, 셀프 공간 아님, 그 채널의
/// `MANAGE_CHANNELS`)을 먼저 봐서, 거부될 이벤트를 쓰지 않게 합니다. `state`에는 그 채널이 이미 있어야 합니다.
fn check_p2p(
    state: &SpaceState,
    me: &yangpa_core::ids::UserId,
    channel: &ChannelId,
    settings: &ChannelSettings,
) -> Result<()> {
    if state.kind == SpaceKind::SelfSpace {
        return Err(Error::invalid(
            "내 기기끼리 쓰는 공간에는 P2P 전용 채널을 둘 수 없습니다",
        ));
    }
    if !matches!(settings.kind, ChannelKind::Text | ChannelKind::Voice) {
        return Err(Error::invalid("P2P 전용은 텍스트·통화 채널만 됩니다"));
    }
    if !state
        .channel_permissions(me, channel)
        .contains(Permissions::MANAGE_CHANNELS)
    {
        return Err(Error::invalid(
            "내가 관리할 수 없는 채널은 P2P 전용으로 둘 수 없습니다",
        ));
    }
    Ok(())
}

/// `channel.create`의 `p2p: true` 검사. 채널만 만들어지고 P2P 지정이 거부돼 Tor 채널로 남는 일이 없게 미리 봅니다.
pub(crate) fn check_create(
    state: &SpaceState,
    me: &yangpa_core::ids::UserId,
    channel: &ChannelId,
    settings: &ChannelSettings,
) -> Result<()> {
    let mut after = state.clone();
    after.channels.insert(*channel, settings.clone());
    check_p2p(&after, me, channel, settings)
}

/// `channel.set_p2p` 검사. 켤 때는 만들 때와 같은 규칙, 끌(Tor로 되돌릴) 때는 그 채널의 `MANAGE_CHANNELS`만 봅니다.
pub(crate) fn check_set(
    state: &SpaceState,
    me: &yangpa_core::ids::UserId,
    channel: &ChannelId,
    enabled: bool,
) -> Result<()> {
    let settings = state
        .channels
        .get(channel)
        .ok_or_else(|| Error::not_found("없는 채널"))?;
    if enabled {
        return check_p2p(state, me, channel, settings);
    }
    if !state
        .channel_permissions(me, channel)
        .contains(Permissions::MANAGE_CHANNELS)
    {
        return Err(Error::forbidden("이 채널을 관리할 권한이 없습니다"));
    }
    Ok(())
}

/// 채널 전송 방식을 바꾸는 상태 이벤트 본문(`p2p`가 참이면 P2P 전용, 거짓이면 Tor).
pub(crate) fn transport_body(channel: ChannelId, p2p: bool) -> EventBody {
    EventBody::ChannelTransportSet {
        channel,
        transport: if p2p {
            ChannelTransport::P2p
        } else {
            ChannelTransport::Tor
        },
    }
}

/// `channel.p2p_join`: 볼 수 있는 P2P 전용 채널에 이 기기로 참여합니다. 이미 참여 중이면 그대로 둡니다.
pub(crate) fn join(node: &Arc<Node>, space: SpaceId, channel: ChannelId) -> Result<()> {
    let _w = write_guard();
    // 상태는 잠금 안에서 읽습니다: 같은 때 채널이 Tor로 돌아가면 그 정리(`on_state_applied`)가 이 기록 뒤에 옵니다.
    let (state, me) = crews::member_state(node, &space)?;
    if !state.channels.contains_key(&channel) {
        return Err(Error::not_found("없는 채널"));
    }
    if !state
        .channel_permissions(&me, &channel)
        .contains(Permissions::VIEW_CHANNEL)
    {
        return Err(Error::forbidden("이 채널을 볼 권한이 없습니다"));
    }
    if !state.is_p2p(&channel) {
        return Err(Error::invalid("P2P 전용 채널이 아닙니다"));
    }
    let mut joined = load(node);
    if joined.contains(&(space, channel)) {
        return Ok(());
    }
    if joined.len() >= MAX_JOINED {
        return Err(Error::conflict(format!(
            "P2P 전용 채널은 {MAX_JOINED}개까지 참여할 수 있습니다"
        )));
    }
    joined.insert((space, channel));
    save(node, &joined)?;
    node.notify.publish(Notification::SpaceUpdated { space });
    reconcile(node);
    Ok(())
}

/// `channel.p2p_leave`: 참여를 그만둡니다. 동의를 거두는 일이라 채널·멤버 자격과 상관없이 늘 됩니다.
pub(crate) fn leave(node: &Arc<Node>, space: SpaceId, channel: ChannelId) -> Result<()> {
    let _w = write_guard();
    let mut joined = load(node);
    if !joined.remove(&(space, channel)) {
        return Ok(());
    }
    save(node, &joined)?;
    node.notify.publish(Notification::SpaceUpdated { space });
    reconcile(node);
    Ok(())
}

/// 해소된 상태가 바뀔 때(`crews::on_state_applied`): P2P 전용이 아니게 된(Tor로 돌아갔거나 지운) 채널과, 내가
/// 멤버가 아니게 된 공간의 참여 기록을 지웁니다. 다시 P2P 전용이 되거나 다시 들어오면 경고부터 다시 받습니다.
/// 그리고 멤버·권한이 바뀌었을 수 있으니 알게 된 P2P 노드를 다시 보고 스웜·안내를 맞춥니다.
pub(crate) fn on_state_applied(
    node: &Arc<Node>,
    space: &SpaceId,
    old: Option<&SpaceState>,
    new: &SpaceState,
) {
    let me = node.me().ok().map(|m| m.user());
    // 알게 된 기기의 사용자가 이 공간 멤버였거나 멤버가 됐으면(신뢰·아는 사이 여부가 바뀌었을 수 있음) 다시 맞춥니다.
    let touches = load(node).iter().any(|(s, _)| s == space)
        || node.p2p.learned.lock().values().any(|l| {
            l.channels.iter().any(|(s, _)| s == space)
                || new.is_member(&l.user)
                || old.is_some_and(|o| o.is_member(&l.user))
        });
    if let Some(old) = old {
        let left = me.is_some_and(|u| old.is_member(&u) && !new.is_member(&u));
        if left || !old.channel_transport.keys().all(|c| new.is_p2p(c)) {
            let _w = write_guard();
            let mut joined = load(node);
            let before = joined.len();
            joined.retain(|(s, c)| s != space || (!left && new.is_p2p(c)));
            if joined.len() != before
                && let Err(e) = save(node, &joined)
            {
                warn!(error = %e, "P2P 참여 목록을 정리하지 못함");
            }
        }
    }
    if touches {
        reconcile(node);
    }
}

// ───────────── P2P 스웜 ─────────────

/// Tor 밖으로 잇는 이 기기의 두 스웜.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Side {
    /// 직접 스웜: 직접 받고 직접 겁니다(IP가 드러남). 참여한 P2P 전용 채널이나 신뢰하는 사람·크루가 있는 동안.
    Direct,
    /// 출구 스웜: Tor 출구를 거쳐 나를 신뢰한 상대의 직접 스웜에만 겁니다(받지 않음, IP를 숨김).
    Exit,
}

/// 이 기기와 상대 기기 사이에 바랄 연결(내 쪽에서 본 것).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Want {
    None,
    /// 서로의 직접 스웜끼리(함께 참여한 P2P 전용 채널이 있거나 서로 신뢰).
    Direct,
    /// 내 출구 스웜에서 상대 직접 스웜으로(상대만 나를 신뢰).
    ExitOut,
}

/// 이 P2P 피어와의 연결을 닫습니다(시험용: 망이 바뀌어 끊긴 흉내).
pub(crate) fn cut(node: &Node, peer: PeerId) {
    if let Some(h) = handle_of(node, side_of(node, &peer)) {
        h.disconnect(peer);
    }
}

fn handle_of(node: &Node, side: Side) -> Option<NetHandle> {
    match side {
        Side::Direct => node.p2p.net.read().clone(),
        Side::Exit => node.p2p.exit.read().clone(),
    }
}

/// 지금 도는 직접 스웜.
fn handle(node: &Node) -> Option<NetHandle> {
    handle_of(node, Side::Direct)
}

/// 직접 스웜이 도는지.
pub(crate) fn running(node: &Node) -> bool {
    node.p2p.net.read().is_some()
}

/// 지금 쓰는(P2P 전용이고, 내가 멤버이며 볼 수 있는) 참여 채널.
fn active_joined(node: &Node) -> Joined {
    let Ok(me) = node.me() else {
        return Joined::new();
    };
    let user = me.user();
    load(node)
        .into_iter()
        .filter(|(s, c)| {
            node.space_state(s)
                .is_some_and(|st| joined_active(&st, &user, c))
                && !crate::friends::space_deactivated(node, s)
        })
        .collect()
}

fn joined_active(state: &SpaceState, me: &UserId, channel: &ChannelId) -> bool {
    state.is_p2p(channel)
        && state.is_member(me)
        && state
            .channel_permissions(me, channel)
            .contains(Permissions::VIEW_CHANNEL)
}

pub(crate) fn start(node: &Arc<Node>) {
    reconcile(node);
}

/// 노드 종료: 스웜을 모두 멈추고 P2P 인증을 잊습니다.
pub(crate) fn shutdown(node: &Node) {
    for slot in [&node.p2p.net, &node.p2p.exit] {
        if let Some(h) = slot.write().take() {
            h.shutdown();
        }
    }
    node.peers.forget_p2p(None);
    node.p2p.sides.lock().clear();
}

/// 참여 목록·신뢰·상태가 바뀌면: 필요한 스웜만 띄우고 나머지는 멈춘 뒤, 멤버 기기에게 안내를 다시 보내고 바라는
/// 연결을 겁니다.
pub(crate) fn reconcile(node: &Arc<Node>) {
    node.spawn_detached(|node| async move {
        reconcile_now(&node).await;
        prune(&node);
        announce(&node);
        connect_all(&node, false);
        // 참가를 그만뒀거나 Tor로 돌아간 채널의 직접 연결 통화에서 나옵니다.
        crate::voice::leave_blocked(&node).await;
        // 서로 신뢰가 바뀐 Tor 통화 참가자를 앱에 알립니다(그 상대와 WebRTC를 열거나 닫음).
        crate::voice::refresh_direct_all(&node);
    });
}

async fn reconcile_now(node: &Arc<Node>) {
    let _l = node.p2p.life.lock().await;
    if node.is_shutting_down() {
        return;
    }
    // 오프라인 모드에서는 직접 연결 스웜(수신 포트·UPnP·STUN)도 띄우지 않습니다.
    let offline = crate::presence::offline(node);
    let want_direct = !offline
        && (!active_joined(node).is_empty()
            || crate::trust::any(node)
            || crate::trust::own_devices(node)
            || devlink::wants_direct(node));
    let want_exit = !offline && wants_exit(node);
    for (side, want) in [(Side::Direct, want_direct), (Side::Exit, want_exit)] {
        let running = handle_of(node, side).is_some();
        if want && !running {
            if let Err(e) = start_swarm(node, side).await {
                warn!(error = %e, side = ?side, "P2P 스웜을 띄우지 못함");
            }
        } else if !want && running {
            stop_swarm(node, side);
        }
    }
}

/// 출구 스웜이 필요한지: 나를 신뢰하고 주소를 준 기기 중 출구로 걸 기기가 있음.
fn wants_exit(node: &Node) -> bool {
    let devices: Vec<DeviceId> = node.p2p.learned.lock().keys().copied().collect();
    devices.iter().any(|d| want_for(node, d) == Want::ExitOut)
}

async fn start_swarm(node: &Arc<Node>, side: Side) -> Result<()> {
    let cfg = &node.cfg;
    // 띄울 때마다 새 키: 지난번 노드나 Tor 노드와 이어 볼 수 없게.
    let keypair = Keypair::generate_ed25519();
    let mut stun_at = None;
    let mut n = match side {
        Side::Direct => {
            let mut n = NetConfig::new(NetMode::Normal, keypair, cfg.data_dir.join("p2p"));
            if !cfg.net.p2p_listen.is_empty() {
                // 정해 준 수신 주소(시험)면 바깥 STUN·UPnP에 묻지 않습니다.
                n.listen = cfg.net.p2p_listen.clone();
            } else {
                n.upnp = true;
                if let Some((port, mapped)) = probe(node).await {
                    n.listen = vec![
                        quic_addr(std::net::Ipv4Addr::UNSPECIFIED.into(), port),
                        "/ip4/0.0.0.0/tcp/0".parse().expect("고정 주소"),
                    ];
                    if let Some(m) = mapped {
                        n.external_addrs = vec![quic_addr(m.ip(), m.port())];
                    }
                }
                stun_at = Some(Instant::now());
            }
            n
        }
        Side::Exit => match node.net.tor_share() {
            // 주 스웜의 Arti를 함께 씁니다(부트스트랩·회로를 다시 만들지 않게). onion 서비스는 열지 않습니다.
            Some(share) => {
                let mut n = NetConfig::new(NetMode::Tor, keypair, cfg.data_dir.clone());
                n.onion_listen = false;
                n.tor_share = Some(share);
                n
            }
            // 직접 연결 주 스웜(`--direct`: 상시 노드·시험)에는 Arti가 없어, 받지 않는 직접 스웜으로 겁니다.
            None => {
                let mut n = NetConfig::new(NetMode::Normal, keypair, cfg.data_dir.join("p2p-exit"));
                n.listen = Vec::new();
                n
            }
        },
    };
    // 같은 망 이웃에게 알리지 않고(mDNS), DHT에는 참여하지 않습니다.
    n.mdns = false;
    n.kad_mode = KadMode::Client;
    n.request_timeout = cfg.net.request_timeout;
    // 앱의 전체 속도 표시가 직접 연결도 함께 세게.
    n.traffic = node.net.traffic();
    let (h, mut events) = match yangpa_net::spawn(n.clone()) {
        Ok(x) => x,
        // 고른 포트를 그새 다른 프로그램이 썼으면 아무 포트로(공인 주소는 모름).
        Err(e) if side == Side::Direct && stun_at.is_some() => {
            debug!(error = %e, "고른 포트를 열지 못해 아무 포트로 엶");
            n.listen = vec![
                "/ip4/0.0.0.0/udp/0/quic-v1".parse().expect("고정 주소"),
                "/ip4/0.0.0.0/tcp/0".parse().expect("고정 주소"),
            ];
            n.external_addrs.clear();
            stun_at = None;
            yangpa_net::spawn(n)?
        }
        Err(e) => return Err(e.into()),
    };
    let local = h.local_peer_id();
    if side == Side::Direct {
        *node.p2p.stun_at.lock() = stun_at;
    }
    let slot = match side {
        Side::Direct => &node.p2p.net,
        Side::Exit => &node.p2p.exit,
    };
    *slot.write() = Some(h.clone());
    // 띄우는 사이 종료가 시작됐으면(종료는 이 잠금을 기다리지 않음) 바로 멈춥니다.
    if node.is_shutting_down() {
        shutdown(node);
        return Ok(());
    }
    info!(side = ?side, "P2P 스웜을 띄움");
    let weak = Arc::downgrade(node);
    node.tasks.spawn(async move {
        while let Some(ev) = events.recv().await {
            let Some(node) = weak.upgrade() else { break };
            // 멈췄거나 다시 띄운 스웜의 이벤트는 버립니다.
            if node.is_shutting_down()
                || handle_of(&node, side).is_none_or(|x| x.local_peer_id() != local)
            {
                break;
            }
            on_event(&node, side, &h, ev);
        }
    });
    Ok(())
}

fn stop_swarm(node: &Arc<Node>, side: Side) {
    let slot = match side {
        Side::Direct => &node.p2p.net,
        Side::Exit => &node.p2p.exit,
    };
    if let Some(h) = slot.write().take() {
        h.shutdown();
    }
    node.p2p.gone.lock().retain(|(s, _)| *s != side);
    for (peer, _, _) in node.peers.p2p_peers() {
        if side_of(node, &peer) == side {
            on_disconnected(node, side, peer);
        }
    }
    node.p2p.auth_locks.lock().clear();
    info!(side = ?side, "P2P 스웜을 멈춤");
}

fn on_event(node: &Arc<Node>, side: Side, h: &NetHandle, ev: NetEvent) {
    match ev {
        NetEvent::InboundRequest { peer, req, reply } => {
            let n = node.clone();
            node.tasks.spawn(async move {
                let resp = handle_inbound(&n, side, peer, req).await;
                let _ = reply.send(resp);
            });
        }
        // P2P 전용 채널의 통화는 WebRTC라 이 스웜으로는 소리 패킷이 오지 않습니다(Tor 통화는 주 스웜).
        NetEvent::Media { .. } => {}
        // P2P 쪽 스웜에서는 gossip을 쓰지 않습니다(구독한 토픽이 없음).
        NetEvent::Gossip { from, msg_id, .. } => {
            h.report_gossip(msg_id, from, GossipVerdict::Ignore);
        }
        NetEvent::PeerConnected(peer) => {
            node.p2p.gone.lock().remove(&(side, peer));
            if learned_device(node, &peer).is_some() {
                node.spawn_detached(move |node| async move {
                    if let Err(e) = ensure_auth(&node, side, peer).await {
                        debug!(peer = %peer, error = %e, "P2P 연결 후 인증 실패");
                    }
                });
            }
        }
        NetEvent::PeerDisconnected(peer) => {
            {
                let mut gone = node.p2p.gone.lock();
                if gone.len() >= GONE_MAX {
                    gone.clear();
                }
                gone.insert((side, peer));
            }
            on_disconnected(node, side, peer);
        }
        // 주소가 바뀌었을 수 있으니 안내를 맞춥니다(바뀐 것이 없으면 보내지 않음).
        NetEvent::Status(_) if side == Side::Direct => announce(node),
        NetEvent::Status(_) => {}
    }
}

fn on_disconnected(node: &Arc<Node>, side: Side, peer: PeerId) {
    // 다른 스웜에 있는 같은 피어의 인증은 두고(상대 직접 스웜이 내 두 스웜에 함께 이어졌을 수 있음) 이 스웜 것만 지웁니다.
    if side_of(node, &peer) != side {
        return;
    }
    let was = node.peers.p2p_identity(&peer).is_some();
    node.peers.forget_p2p(Some(&peer));
    node.p2p.sides.lock().remove(&peer);
    node.p2p.auth_locks.lock().remove(&peer);
    if was {
        node.p2p
            .drops
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        volatile::on_peer_disconnected(node, peer);
        // 내 쪽이나 상대 쪽 망이 바뀌어(VPN을 켬, 와이파이를 옮김) 끊겼을 수 있습니다. 같은 노드에 다시 거는 간격
        // (`REDIAL_GAP`)을 기다리지 않고 곧 다시 겁니다. 그 사이 신뢰한 상대와는 Tor로 주고받습니다(`request`).
        if let Some((_, device)) = learned_device(node, &peer) {
            redial_soon(node, device);
        }
    }
}

/// 기억하는 끊긴 피어 수.
const GONE_MAX: usize = 4096;

/// 끊긴 직접 연결을 다시 거는 때(끊긴 뒤부터 차례로 기다림). 상대가 새 주소를 알리면 그때도 바로 겁니다([`learn`]).
const REDIAL_SOON: [Duration; 4] = [
    Duration::from_secs(1),
    Duration::from_secs(5),
    Duration::from_secs(15),
    Duration::from_secs(45),
];

fn redial_soon(node: &Arc<Node>, device: DeviceId) {
    if !node.p2p.redialing.lock().insert(device) {
        return;
    }
    node.spawn_detached(move |node| async move {
        for wait in REDIAL_SOON {
            tokio::time::sleep(wait).await;
            if node.is_shutting_down() || linked(&node, &device) {
                break;
            }
            connect_one(&node, device, true);
        }
        node.p2p.redialing.lock().remove(&device);
    });
}

/// 이 기기의 노드(직접이든 출구든)와 P2P로 인증돼 있는지.
fn linked(node: &Node, device: &DeviceId) -> bool {
    let Some((peer, exit)) = node
        .p2p
        .learned
        .lock()
        .get(device)
        .map(|l| (l.peer, l.exit_peer))
    else {
        return false;
    };
    [peer, exit]
        .into_iter()
        .flatten()
        .any(|p| node.peers.p2p_identity(&p).is_some())
}

/// P2P로 인증된 피어가 내 어느 스웜에 있는지(모르면 직접 스웜).
fn side_of(node: &Node, peer: &PeerId) -> Side {
    node.p2p
        .sides
        .lock()
        .get(peer)
        .copied()
        .unwrap_or(Side::Direct)
}

// ───────────── 안내(Tor로) ─────────────

/// 이 기기의 직접 스웜 노드(돌 때): PeerId와 알려 줄 주소(모든 인터페이스를 뜻하는 주소는 뺌).
fn my_node(node: &Node) -> Option<(PeerId, Vec<String>)> {
    let h = handle(node)?;
    let s = h.last_status();
    let mut addrs: Vec<String> = Vec::new();
    for a in s
        .external_addrs
        .iter()
        .chain(&s.relay_addrs)
        .chain(&s.listen_addrs)
    {
        if addrs.len() >= MAX_P2P_ADDRS {
            break;
        }
        let t = a.to_string();
        if dialable(a) && !addrs.contains(&t) {
            addrs.push(t);
        }
    }
    Some((h.local_peer_id(), addrs))
}

/// 걸 수 있는 직접 주소인지(onion·모든 인터페이스 주소가 아님).
fn dialable(a: &Multiaddr) -> bool {
    a.iter().all(|p| match p {
        Protocol::Ip4(ip) => !ip.is_unspecified(),
        Protocol::Ip6(ip) => !ip.is_unspecified(),
        Protocol::Onion(..) | Protocol::Onion3(_) => false,
        _ => true,
    })
}

/// Tor 출구로 걸 수 있는 주소인지(TCP만, 릴레이 경유 아님). 사설·루프백 주소는 Arti가 거절합니다.
fn exit_dialable(a: &Multiaddr) -> bool {
    dialable(a)
        && a.iter().any(|p| matches!(p, Protocol::Tcp(_)))
        && !a.iter().any(|p| matches!(p, Protocol::P2pCircuit))
}

/// 안내 하나에 실을 수 있는 채널인지: P2P 전용이고, 나와 상대 기기가 멤버이며 상대가 볼 수 있음.
fn valid_for(
    node: &Node,
    me: &UserId,
    user: &UserId,
    device: &DeviceId,
    space: &SpaceId,
    channel: &ChannelId,
) -> bool {
    node.space_state(space).is_some_and(|st| {
        st.is_p2p(channel)
            && st.is_member(me)
            && st.is_member(user)
            && st.device_belongs(user, device)
            && st
                .channel_permissions(user, channel)
                .contains(Permissions::VIEW_CHANNEL)
    }) && !crate::friends::space_deactivated(node, space)
}

/// 이 기기를 아는지: 내 활성 기기이거나, 비활성화되지 않은 공간에 나와 함께 멤버이고 그 사용자의 기기임(신뢰 안내를 받아들일
/// 근거). 내 기기는 함께 있는 공간이 없어도 내 목록으로 압니다.
fn known_device(node: &Node, me: &UserId, user: &UserId, device: &DeviceId) -> bool {
    if user == me {
        return node
            .me()
            .is_ok_and(|m| m.devices.list.contains_active(device));
    }
    node.space_ids().iter().any(|s| {
        node.space_state(s).is_some_and(|st| {
            st.is_member(me) && st.is_member(user) && st.device_belongs(user, device)
        }) && !crate::friends::space_deactivated(node, s)
    })
}

fn empty_hello() -> P2pHello {
    P2pHello {
        peer_id: Vec::new(),
        addrs: Vec::new(),
        channels: Vec::new(),
        trusted: false,
        exit_peer: Vec::new(),
        seq: 0,
    }
}

/// 보낼 안내에 순서 값을 붙입니다(밀리초 시각, 같은 밀리초면 하나 더). 비교·기록은 붙이기 전 것으로 합니다.
fn stamp(node: &Node, mut h: P2pHello) -> P2pHello {
    h.seq = next_seq(node);
    h
}

/// 다음 안내 순서 값.
fn next_seq(node: &Node) -> u64 {
    let now = yangpa_core::time::Timestamp::now().as_millis();
    let mut last = node.p2p.seq.lock();
    *last = now.max(*last + 1);
    *last
}

fn hello_is_empty(h: &P2pHello) -> bool {
    h.peer_id.is_empty() && h.exit_peer.is_empty()
}

/// 이 기기에게 보낼 안내.
///
/// - 직접 스웜 노드: 내가 상대를 신뢰하거나 상대가 볼 수 있는 내 참여 채널이 있을 때. 주소(IP)는 내가 상대를 신뢰하거나
///   상대가 그 채널 중 하나에 함께 참여한다고 알려 왔을 때만.
/// - 출구 스웜 노드: 상대만 나를 신뢰하고 주소를 줬을 때(내가 출구로 걸 테니 받아 달라는 뜻).
fn hello_for(
    node: &Node,
    user: &UserId,
    device: &DeviceId,
    joined: &Joined,
    direct: Option<&(PeerId, Vec<String>)>,
) -> P2pHello {
    let mut out = empty_hello();
    let Ok(me) = node.me() else {
        return out;
    };
    let me = me.user();
    let i_trust = crate::trust::trusts(node, user);
    if let Some((peer, addrs)) = direct {
        let channels: Vec<_> = joined
            .iter()
            .filter(|(s, c)| valid_for(node, &me, user, device, s, c))
            .take(MAX_P2P_CHANNELS)
            .copied()
            .collect();
        if i_trust || !channels.is_empty() {
            let shared = node
                .p2p
                .learned
                .lock()
                .get(device)
                .is_some_and(|l| channels.iter().any(|k| l.channels.contains(k)));
            out.peer_id = peer.to_bytes();
            out.trusted = i_trust;
            if i_trust || shared {
                out.addrs = addrs.clone();
            }
            out.channels = channels;
        }
    }
    if want_for(node, device) == Want::ExitOut
        && let Some(h) = handle_of(node, Side::Exit)
    {
        out.exit_peer = h.local_peer_id().to_bytes();
    }
    out
}

/// Tor로 인증된 피어마다, 지난번에 보낸 것과 달라졌으면 안내를 보냅니다. 빈 안내는 전에 무언가 보냈을 때만(참여·신뢰를
/// 그만둠) 보냅니다.
fn announce(node: &Arc<Node>) {
    let joined = active_joined(node);
    let mine = my_node(node);
    for (peer, user, device) in node.peers.authed_peers() {
        let hello = hello_for(node, &user, &device, &joined, mine.as_ref());
        {
            let mut sent = node.p2p.sent.lock();
            let changed = match sent.get(&peer) {
                Some((h, _)) => *h != hello,
                None => !hello_is_empty(&hello),
            };
            if !changed {
                continue;
            }
            sent.insert(peer, (hello.clone(), false));
        }
        node.spawn_detached(move |node| async move {
            send_hello(&node, peer, user, device, hello).await;
        });
    }
}

async fn send_hello(
    node: &Arc<Node>,
    peer: PeerId,
    user: UserId,
    device: DeviceId,
    hello: P2pHello,
) {
    let r = node
        .net
        .request(
            peer,
            YpRequest::P2pHello(Box::new(stamp(node, hello.clone()))),
        )
        .await;
    let delivered = match &r {
        Ok(YpResponse::P2pHello(_)) => true,
        // 안내를 모르는 예전 빌드도 답은 했으니 다시 보내지 않습니다.
        Ok(_) => true,
        Err(e) => {
            debug!(peer = %peer, error = %e, "P2P 안내를 보내지 못함");
            false
        }
    };
    if let Some(e) = node.p2p.sent.lock().get_mut(&peer)
        && e.0 == hello
    {
        e.1 = delivered;
    }
    if let Ok(YpResponse::P2pHello(theirs)) = r {
        learn(node, user, device, &theirs);
        // 상대가 함께 참여함·신뢰함을 알았으면 이제 주소나 출구 노드를 실은 안내가 나갑니다.
        announce(node);
    }
}

/// Tor로 받은 `P2pHello`: 배우고, 이 기기에게 줄 내 안내로 답합니다.
pub(crate) fn handle_hello(node: &Arc<Node>, peer: PeerId, hello: P2pHello) -> YpResponse {
    if is_p2p_peer(node, &peer) {
        return YpResponse::error(WireErrorCode::Forbidden, "P2P 연결로는 받지 않음");
    }
    let Some((user, device)) = node.peers.identity_of(&peer) else {
        return YpResponse::error(WireErrorCode::Unauthenticated, "인증 전");
    };
    if hello.addrs.len() > MAX_P2P_ADDRS || hello.channels.len() > MAX_P2P_CHANNELS {
        return YpResponse::error(WireErrorCode::BadRequest, "안내가 너무 큼");
    }
    learn(node, user, device, &hello);
    let mine = hello_for(
        node,
        &user,
        &device,
        &active_joined(node),
        my_node(node).as_ref(),
    );
    node.p2p.sent.lock().insert(peer, (mine.clone(), true));
    // 답에 싣지 못한 것(출구 스웜을 막 띄워야 함 등)은 맞춘 뒤 다시 보냅니다.
    YpResponse::P2pHello(Box::new(stamp(node, mine)))
}

/// 받은 안내를 기억합니다. 실린 채널 중 지금 맞는 것만 남기고, 직접·출구 노드가 모두 없으면 그 기기를 잊습니다.
/// 그 뒤 스웜·연결을 맞춥니다.
fn learn(node: &Arc<Node>, user: UserId, device: DeviceId, hello: &P2pHello) {
    let Ok(me) = node.me() else { return };
    let me = me.user();
    if !known_device(node, &me, &user, &device) {
        forget_device(node, &device);
        return;
    }
    // 내 안내의 답(상대가 먼저 만든 것)이 상대가 그 뒤 보낸 안내보다 늦게 닿을 수 있습니다. 옛 것은 버립니다.
    if hello.seq != 0 {
        let mut seen = node.p2p.seen.lock();
        if seen.get(&device).is_some_and(|s| hello.seq < *s) {
            debug!("늦게 닿은 옛 P2P 안내를 버림");
            return;
        }
        seen.insert(device, hello.seq);
    }
    let peer = PeerId::from_bytes(&hello.peer_id).ok();
    let exit_peer = PeerId::from_bytes(&hello.exit_peer).ok();
    if peer.is_none() && exit_peer.is_none() {
        forget_device(node, &device);
        reconcile(node);
        return;
    }
    let channels: Joined = match peer {
        Some(_) => hello
            .channels
            .iter()
            .take(MAX_P2P_CHANNELS)
            .filter(|(s, c)| valid_for(node, &me, &user, &device, s, c))
            .copied()
            .collect(),
        None => Joined::new(),
    };
    let addrs: Vec<Multiaddr> = hello
        .addrs
        .iter()
        .take(MAX_P2P_ADDRS)
        .filter_map(|a| a.parse().ok())
        .filter(dialable)
        .collect();
    let entry = Learned {
        user,
        peer,
        addrs: if peer.is_some() { addrs } else { Vec::new() },
        exit_peer,
        channels,
        trusted: peer.is_some() && hello.trusted,
    };
    let moved = {
        let learned = node.p2p.learned.lock();
        learned.get(&device).is_some_and(|o| o.addrs != entry.addrs)
    };
    if moved {
        // 상대 주소가 바뀌었으면(망을 옮김) 다시 거는 간격을 기다리지 않고 새 주소로 겁니다.
        node.p2p.dialed.lock().retain(|(d, _, _), _| *d != device);
    }
    let old = node.p2p.learned.lock().insert(device, entry);
    let old_exit = old.as_ref().and_then(|o| o.exit_peer);
    if let Some(old) = old {
        for p in [old.peer, old.exit_peer].into_iter().flatten() {
            if Some(p) != peer && Some(p) != exit_peer {
                drop_auth(node, &p);
            }
        }
    }
    // 상대 출구 노드는 이 안내보다 먼저 걸어 와 인증을 거절당했을 수 있습니다. 이쪽에서 그 연결로 인증을 걸어 봅니다
    // (상대는 건 뒤 다시 걸지 않음).
    if let Some(x) = exit_peer.filter(|x| Some(*x) != old_exit)
        && handle(node).is_some()
    {
        node.spawn_detached(move |node| async move {
            if let Err(e) = ensure_auth(&node, Side::Direct, x).await {
                debug!(peer = %x, error = %e, "출구 노드 인증 실패(아직 걸어 오지 않음)");
            }
        });
    }
    // 바라는 연결이 바뀌었을 수 있습니다(출구 스웜을 띄우거나 멈춤, 인증을 지움, 새로 걸기).
    reconcile(node);
}

fn forget_device(node: &Arc<Node>, device: &DeviceId) {
    let old = node.p2p.learned.lock().remove(device);
    if let Some(old) = old {
        for p in [old.peer, old.exit_peer].into_iter().flatten() {
            drop_auth(node, &p);
        }
    }
    node.p2p.dialed.lock().retain(|(d, _, _), _| d != device);
}

/// P2P 인증을 지웁니다(연결은 두지만 요청은 받지 않음).
fn drop_auth(node: &Arc<Node>, peer: &PeerId) {
    if node.peers.p2p_identity(peer).is_some() {
        node.peers.forget_p2p(Some(peer));
        node.p2p.sides.lock().remove(peer);
        volatile::on_peer_disconnected(node, *peer);
    }
}

/// 상태·신뢰가 바뀐 뒤: 알게 된 기기의 채널을 다시 보고, 더는 모르는 기기를 잊고, 지금 바라지 않는 P2P 인증을 지웁니다.
fn prune(node: &Arc<Node>) {
    let Ok(me) = node.me() else { return };
    let me = me.user();
    let snapshot: Vec<(DeviceId, UserId, Joined)> = node
        .p2p
        .learned
        .lock()
        .iter()
        .map(|(d, l)| (*d, l.user, l.channels.clone()))
        .collect();
    for (device, user, channels) in snapshot {
        if !known_device(node, &me, &user, &device) {
            forget_device(node, &device);
            continue;
        }
        let keep: Joined = channels
            .iter()
            .filter(|(s, c)| valid_for(node, &me, &user, &device, s, c))
            .copied()
            .collect();
        if keep != channels
            && let Some(l) = node.p2p.learned.lock().get_mut(&device)
        {
            l.channels = keep;
        }
    }
    for (peer, user, device) in node.peers.p2p_peers() {
        if !link_ok(node, side_of(node, &peer), &peer, &user, &device) {
            drop_auth(node, &peer);
        }
    }
}

/// 이 기기와 바랄 연결(내 신뢰, 상대 안내의 신뢰, 함께 참여한 채널로 정함).
fn want_for(node: &Node, device: &DeviceId) -> Want {
    let (user, trusted) = match node.p2p.learned.lock().get(device) {
        Some(l) if l.peer.is_some() => (l.user, l.trusted),
        _ => return Want::None,
    };
    let i_trust = crate::trust::trusts(node, &user);
    if !shared_with(node, device).is_empty() || (i_trust && trusted) {
        Want::Direct
    } else if trusted && !i_trust {
        Want::ExitOut
    } else {
        Want::None
    }
}

/// 바라는데 아직 인증되지 않은 연결을 겁니다. `retry`가 아니면 최근에 건 기기는 건너뜁니다.
fn connect_all(node: &Arc<Node>, retry: bool) {
    let devices: Vec<DeviceId> = node.p2p.learned.lock().keys().copied().collect();
    for device in devices {
        connect_one(node, device, retry);
    }
}

/// 이 기기의 노드에 겁니다. 이미 이어져 있거나, 걸 까닭이 없거나, `retry`가 아닌데 최근에 걸었으면 하지 않습니다.
fn connect_one(node: &Arc<Node>, device: DeviceId, retry: bool) {
    let side = match want_for(node, &device) {
        Want::Direct => Side::Direct,
        Want::ExitOut => Side::Exit,
        Want::None => return,
    };
    let (peer, addrs, user) = match node.p2p.learned.lock().get(&device) {
        Some(Learned {
            peer: Some(p),
            addrs,
            user,
            ..
        }) => (*p, addrs.clone(), *user),
        _ => return,
    };
    let addrs: Vec<Multiaddr> = match side {
        Side::Exit => addrs.into_iter().filter(exit_dialable).collect(),
        _ => addrs,
    };
    if addrs.is_empty()
        || node
            .peers
            .p2p_identity(&peer)
            .is_some_and(|_| side_of(node, &peer) == side)
    {
        return;
    }
    let Some(h) = handle_of(node, side) else {
        return;
    };
    {
        let mut dialed = node.p2p.dialed.lock();
        let now = Instant::now();
        let key = (device, side, peer);
        if let Some(at) = dialed.get(&key)
            && !retry
            && now.duration_since(*at) < REDIAL_GAP
        {
            return;
        }
        // 오래된 기록은 지웁니다(바뀐 노드마다 쌓이지 않게).
        dialed.retain(|_, at| now.duration_since(*at) < REDIAL_GAP);
        dialed.insert(key, now);
    }
    node.spawn_detached(move |node| async move {
        if let Err(e) = h.dial(peer, addrs).await {
            debug!(peer = %peer, side = ?side, error = %e, "P2P 노드에 걸지 못함");
            // 둘 다 공유기 뒤면 바로 걸리지 않습니다. Tor로 맞춰 홀펀칭합니다.
            if side == Side::Direct && starts_punch(&node, &device) {
                punch(&node, user, device).await;
            }
            return;
        }
        // 상대가 내 새 노드를 알리는 안내를 아직 받지 못했으면 인증을 거절합니다. 잠깐 뒤 몇 번 더 겁니다.
        for i in 0..AUTH_TRIES {
            match ensure_auth(&node, side, peer).await {
                Ok(_) => return,
                Err(e) if i + 1 == AUTH_TRIES => {
                    debug!(peer = %peer, side = ?side, error = %e, "P2P 인증 실패");
                }
                Err(_) => tokio::time::sleep(AUTH_RETRY).await,
            }
        }
    });
}

// ───────────── 공유기 너머: STUN·홀펀칭 ─────────────

fn quic_addr(ip: std::net::IpAddr, port: u16) -> Multiaddr {
    let mut a = Multiaddr::empty();
    a.push(match ip {
        std::net::IpAddr::V4(v4) => Protocol::Ip4(v4),
        std::net::IpAddr::V6(v6) => Protocol::Ip6(v6),
    });
    a.push(Protocol::Udp(port));
    a.push(Protocol::QuicV1);
    a
}

/// 인터넷에서 닿는 주소인지(사설·루프백·링크 로컬·CGNAT이 아님).
fn is_public(ip: &std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            let o = v4.octets();
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || (o[0] == 100 && (o[1] & 0xc0) == 64))
        }
        std::net::IpAddr::V6(v6) => !(v6.is_loopback() || v6.is_unspecified()),
    }
}

/// 직접 스웜에 쓸 UDP 포트를 고르고, 바로 그 포트에서 STUN으로 공인 주소를 묻습니다(QUIC 소켓은 STUN 답을 받아 주지
/// 않아, 열기 직전에 같은 포트로 묻습니다). 서버마다 답한 주소가 다르면 대칭형 NAT라 홀펀칭이 안 되므로 주소를
/// 알리지 않습니다. 소켓을 열 수 없으면 `None`.
async fn probe(node: &Node) -> Option<(u16, Option<std::net::SocketAddr>)> {
    let sock = tokio::net::UdpSocket::bind((std::net::Ipv4Addr::UNSPECIFIED, 0))
        .await
        .ok()?;
    let port = sock.local_addr().ok()?.port();
    let mut servers: Vec<std::net::SocketAddr> = Vec::new();
    // 내 STUN 서버 뒤로 쓰기로 한 크루의 STUN 서버.
    let mut list = crate::stun::stun_servers(node);
    for s in crate::stun::crew_stun_all(node) {
        if !list.contains(&s) {
            list.push(s);
        }
    }
    for entry in list.iter().take(STUN_ASK) {
        let Some((host, p)) = entry.rsplit_once(':') else {
            continue;
        };
        let Ok(p) = p.parse::<u16>() else { continue };
        let host = host.trim_start_matches('[').trim_end_matches(']');
        if let Ok(Ok(mut addrs)) =
            tokio::time::timeout(STUN_WAIT, tokio::net::lookup_host((host, p))).await
            && let Some(a) = addrs.find(std::net::SocketAddr::is_ipv4)
        {
            servers.push(a);
        }
    }
    let mut pending: Vec<[u8; 12]> = Vec::new();
    for s in &servers {
        let mut tid = [0u8; 12];
        rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut tid);
        if sock
            .send_to(&yangpa_sfu::stun::binding_request(tid), s)
            .await
            .is_ok()
        {
            pending.push(tid);
        }
    }
    let deadline = tokio::time::Instant::now() + STUN_WAIT;
    let mut found: Vec<std::net::SocketAddr> = Vec::new();
    let mut buf = [0u8; 512];
    while !pending.is_empty() {
        let Ok(Ok((n, _))) = tokio::time::timeout_at(deadline, sock.recv_from(&mut buf)).await
        else {
            break;
        };
        if let Some(i) = pending.iter().position(|t| {
            yangpa_sfu::stun::parse_binding_response(&buf[..n], t)
                .inspect(|a| found.push(*a))
                .is_some()
        }) {
            pending.swap_remove(i);
        }
    }
    let mapped = match found.as_slice() {
        [] => None,
        [first, rest @ ..] if rest.iter().all(|a| a == first) => Some(*first),
        _ => {
            debug!("STUN 서버마다 주소가 달라(대칭형 NAT) 공인 주소를 알리지 않음");
            None
        }
    };
    Some((port, mapped.filter(|m| is_public(&m.ip()))))
}

/// 직접 스웜의 공인 주소가 낡았는지: STUN을 썼고, 지금 연결이 없으며(공유기 대응을 붙잡는 것이 없음), 물은 지 오래됨.
fn direct_stale(node: &Node) -> bool {
    let Some(at) = *node.p2p.stun_at.lock() else {
        return false;
    };
    handle(node).is_some_and(|h| h.last_status().connections == 0) && at.elapsed() > STUN_FRESH
}

/// 홀펀칭 직전: 직접 스웜의 주소가 낡았으면 포트를 다시 열고 다시 묻습니다(새 키, 이어진 연결이 없을 때만). 그 뒤
/// 수신 주소가 나올 때까지 잠깐 기다려 지금 노드를 돌려줍니다.
async fn fresh_node(node: &Arc<Node>) -> Option<(PeerId, Vec<String>)> {
    {
        let _l = node.p2p.life.lock().await;
        if direct_stale(node) {
            stop_swarm(node, Side::Direct);
            if let Err(e) = start_swarm(node, Side::Direct).await {
                warn!(error = %e, "직접 스웜을 다시 띄우지 못함");
            }
        }
    }
    let deadline = Instant::now() + LISTEN_WAIT;
    loop {
        let mine = my_node(node)?;
        if !mine.1.is_empty() || Instant::now() >= deadline {
            return Some(mine);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// 기기 연결 링크에 실을 이 기기의 직접 스웜. 링크를 띄운 뒤 부릅니다(`devlink::wants_direct`가 스웜을 띄움).
/// 오프라인이거나 주소가 없으면 `None`이고, 새 기기는 Tor로만 닿습니다.
pub(crate) async fn link_contact(node: &Arc<Node>) -> Option<DirectContact> {
    reconcile_now(node).await;
    fresh_node(node).await?;
    let h = handle(node)?;
    let s = h.status().await;
    let test = !node.cfg.net.p2p_listen.is_empty();
    let addrs = link_addrs(s.external_addrs.iter().chain(&s.listen_addrs), test);
    (!addrs.is_empty()).then(|| DirectContact {
        peer_id: h.local_peer_id().to_bytes(),
        addrs,
    })
}

/// 기기 연결 링크에 담거나 받은 링크에서 걸 주소인지: IP 직접 주소만. 받은 링크는 조작됐을 수 있고, 이름 주소는 Tor 밖의
/// 시스템 DNS로 풀립니다.
pub(crate) fn link_dialable(a: &Multiaddr) -> bool {
    dialable(a)
        && !a.iter().any(|p| {
            matches!(
                p,
                Protocol::Dns(_)
                    | Protocol::Dns4(_)
                    | Protocol::Dns6(_)
                    | Protocol::Dnsaddr(_)
                    | Protocol::P2pCircuit
            )
        })
}

/// 링크에 담을 주소: 다른 기기가 걸 수 있는 것만, QUIC 먼저, 상한까지. 시험 노드만 루프백을 씁니다.
fn link_addrs<'a>(addrs: impl Iterator<Item = &'a Multiaddr>, loopback: bool) -> Vec<ByteBuf> {
    let usable = |a: &Multiaddr| {
        link_dialable(a)
            && a.iter().all(|p| match p {
                Protocol::Ip4(ip) => !ip.is_link_local() && (loopback || !ip.is_loopback()),
                Protocol::Ip6(ip) => loopback || !ip.is_loopback(),
                _ => true,
            })
    };
    let mut out: Vec<&Multiaddr> = Vec::new();
    for a in addrs {
        if usable(a) && !out.contains(&a) {
            out.push(a);
        }
    }
    out.sort_by_key(|a| !a.iter().any(|p| matches!(p, Protocol::QuicV1)));
    out.truncate(MAX_DIRECT_ADDRS);
    out.into_iter().map(|a| ByteBuf::from(a.to_vec())).collect()
}

/// 새 기기 쪽: 링크의 주소로 쓰던 기기의 직접 스웜에 겁니다. 이어지면 그 스웜을 돌려줍니다.
pub(crate) async fn dial_link(
    node: &Arc<Node>,
    peer: PeerId,
    addrs: Vec<Multiaddr>,
) -> Option<NetHandle> {
    reconcile_now(node).await;
    let h = handle(node)?;
    match h.dial(peer, addrs).await {
        Ok(()) => Some(h),
        Err(e) => {
            debug!(peer = %peer, error = %e, "쓰던 기기에 직접 걸지 못함");
            None
        }
    }
}

/// 홀펀칭은 기기 ID가 작은 쪽이 시작합니다(양쪽이 동시에 시작하지 않게).
fn starts_punch(node: &Node, device: &DeviceId) -> bool {
    node.me().is_ok_and(|me| me.device_id() < *device)
}

/// 홀펀칭을 시작합니다(Tor로 맞춤): 막 고친 내 노드를 보내고, 상대가 답과 함께 받는 쪽 역할로 길을 여는 동안 겁니다.
async fn punch(node: &Arc<Node>, user: UserId, device: DeviceId) {
    let Some(tor) = node.peers.authed_peer_of_device(&device) else {
        return;
    };
    let Some(mine) = fresh_node(node).await else {
        return;
    };
    let hello = hello_for(node, &user, &device, &active_joined(node), Some(&mine));
    if hello.peer_id.is_empty() || hello.addrs.is_empty() {
        return;
    }
    node.p2p.sent.lock().insert(tor, (hello.clone(), true));
    let req = YpRequest::P2pPunch(Box::new(stamp(node, hello)));
    let theirs = match node.net.request(tor, req).await {
        Ok(YpResponse::P2pPunch(h)) => h,
        Ok(r) => {
            debug!(peer = %tor, response = ?std::mem::discriminant(&r), "홀펀칭 거절");
            return;
        }
        Err(e) => {
            debug!(peer = %tor, error = %e, "홀펀칭을 맞추지 못함");
            return;
        }
    };
    learn(node, user, device, &theirs);
    let (Some(h), Some((peer, addrs))) = (handle(node), direct_target(node, &device)) else {
        return;
    };
    match h.dial(peer, addrs).await {
        Ok(()) => {
            if let Err(e) = ensure_auth(node, Side::Direct, peer).await {
                debug!(peer = %peer, error = %e, "홀펀칭 뒤 인증 실패");
            }
        }
        Err(e) => debug!(peer = %peer, error = %e, "홀펀칭 실패(Tor로 이어 씀)"),
    }
}

/// 상대 직접 스웜 노드와 주소(안내로 받은 것).
fn direct_target(node: &Node, device: &DeviceId) -> Option<(PeerId, Vec<Multiaddr>)> {
    match node.p2p.learned.lock().get(device) {
        Some(Learned {
            peer: Some(p),
            addrs,
            ..
        }) if !addrs.is_empty() => Some((*p, addrs.clone())),
        _ => None,
    }
}

/// Tor로 받은 `P2pPunch`: 상대 노드를 배우고, 막 고친 내 노드로 답한 뒤 받는 쪽 역할로 상대 주소에 길을 엽니다.
pub(crate) async fn handle_punch(node: &Arc<Node>, peer: PeerId, hello: P2pHello) -> YpResponse {
    if is_p2p_peer(node, &peer) {
        return YpResponse::error(WireErrorCode::Forbidden, "P2P 연결로는 받지 않음");
    }
    let Some((user, device)) = node.peers.identity_of(&peer) else {
        return YpResponse::error(WireErrorCode::Unauthenticated, "인증 전");
    };
    if hello.addrs.len() > MAX_P2P_ADDRS || hello.channels.len() > MAX_P2P_CHANNELS {
        return YpResponse::error(WireErrorCode::BadRequest, "안내가 너무 큼");
    }
    learn(node, user, device, &hello);
    if want_for(node, &device) != Want::Direct {
        return YpResponse::error(WireErrorCode::Forbidden, "직접 연결할 사이가 아님");
    }
    let Some(mine) = fresh_node(node).await else {
        return YpResponse::error(WireErrorCode::Internal, "직접 스웜이 꺼져 있음");
    };
    let reply = hello_for(node, &user, &device, &active_joined(node), Some(&mine));
    node.p2p.sent.lock().insert(peer, (reply.clone(), true));
    let reply = stamp(node, reply);
    if let (Some(h), Some((their, addrs))) = (handle(node), direct_target(node, &device)) {
        let udp: Vec<Multiaddr> = addrs
            .into_iter()
            .filter(|a| a.iter().any(|p| matches!(p, Protocol::QuicV1)))
            .collect();
        if !udp.is_empty() {
            // 답이 Tor로 닿는 동안 미리 보내기 시작합니다(상대 연결이 들어올 때까지 보냄). 한 번은 QUIC 핸드셰이크 시간(5초)
            // 뿐이라, Tor로 답이 늦게 닿는 경우를 위해 몇 번 이어 갑니다.
            node.spawn_detached(move |node| async move {
                for _ in 0..PUNCH_TRIES {
                    match h.dial_as_listener(their, udp.clone()).await {
                        Ok(()) => {
                            if let Err(e) = ensure_auth(&node, Side::Direct, their).await {
                                debug!(peer = %their, error = %e, "홀펀칭 뒤 인증 실패");
                            }
                            return;
                        }
                        Err(e) => debug!(peer = %their, error = %e, "홀펀칭(받는 쪽) 실패"),
                    }
                    if node.peers.p2p_identity(&their).is_some() {
                        return;
                    }
                }
            });
        }
    }
    YpResponse::P2pPunch(Box::new(reply))
}

/// 이 기기가 Tor로 (다시) 인증됐을 때(`peers::remember`): 안내를 처음부터 다시 맞춥니다(상대가 다시 떴을 수 있음).
pub(crate) fn on_peer_authed(node: &Arc<Node>, peer: PeerId, _user: &UserId) {
    node.p2p.sent.lock().remove(&peer);
    // 상대가 다시 떴으면 시계가 뒤로 갔을 수 있어 순서를 처음부터 받습니다.
    let device = node.peers.identity_of(&peer).map(|(_, d)| d);
    if let Some(device) = &device {
        node.p2p.seen.lock().remove(device);
    }
    if handle(node).is_some() || handle_of(node, Side::Exit).is_some() {
        announce(node);
    }
}

/// Tor 피어 연결이 끊겼을 때(`peers::on_disconnected`): 그 PeerId로 보낸 안내를 잊습니다. 같은 PeerId로 다시 붙으면
/// 인증 때 어차피 지우고([`on_peer_authed`]), 새 PeerId로 붙으면 옛 항목이 재시작 전까지 쌓이기만 합니다.
pub(crate) fn on_peer_disconnected(node: &Node, peer: PeerId) {
    node.p2p.sent.lock().remove(&peer);
}

/// 주기 작업(`sync_service::spawn_periodic`): 전하지 못한 안내를 다시 보내고, 끊긴 연결을 다시 걸고, P2P 피어와 함께
/// 참여한 채널을 따라잡습니다.
pub(crate) fn tick(node: &Arc<Node>) {
    node.p2p.sent.lock().retain(|_, (_, delivered)| *delivered);
    announce(node);
    connect_all(node, false);
    // 기기마다 한 번만 따라잡습니다.
    let mut seen: HashSet<DeviceId> = HashSet::new();
    for (peer, _, device) in node.peers.p2p_peers() {
        if !seen.insert(device) {
            continue;
        }
        let spaces: BTreeSet<SpaceId> = shared_with(node, &device)
            .into_iter()
            .map(|(s, _)| s)
            .collect();
        for space in spaces {
            sync_service::spawn_catch_up(node, space, peer);
        }
    }
}

// ───────────── P2P 인증 ─────────────

/// 이 P2P 쪽 PeerId를 알려 준 기기(직접 노드든 출구 노드든).
fn learned_device(node: &Node, peer: &PeerId) -> Option<(UserId, DeviceId)> {
    node.p2p
        .learned
        .lock()
        .iter()
        .find(|(_, l)| l.peer == Some(*peer) || l.exit_peer == Some(*peer))
        .map(|(d, l)| (l.user, *d))
}

/// 이 기기와 함께 참여한(둘 다 지금 쓰는) P2P 전용 채널.
fn shared_with(node: &Node, device: &DeviceId) -> Joined {
    let theirs = match node.p2p.learned.lock().get(device) {
        Some(l) => l.channels.clone(),
        None => return Joined::new(),
    };
    if theirs.is_empty() {
        return theirs;
    }
    let mine = active_joined(node);
    theirs.intersection(&mine).copied().collect()
}

fn auth_for(node: &Node, local: &PeerId, peer: &PeerId) -> Result<PeerAuth> {
    let me = node.me()?;
    Ok(PeerAuth::create(
        &me.device.signing,
        me.devices.clone(),
        &local.to_bytes(),
        &peer.to_bytes(),
    ))
}

/// 내 `side` 스웜의 이 피어가 이 기기의 연결로 맞는지.
///
/// - 직접 스웜 + 상대 직접 노드: 함께 참여한 채널이 있거나 서로 신뢰.
/// - 직접 스웜 + 상대 출구 노드: 내가 상대를 신뢰(상대가 숨은 채 내게 걸어 옴).
/// - 출구 스웜 + 상대 직접 노드: 상대가 나를 신뢰(내가 숨은 채 겁니다).
fn link_ok(node: &Node, side: Side, peer: &PeerId, user: &UserId, device: &DeviceId) -> bool {
    let (direct, exit) = match node.p2p.learned.lock().get(device) {
        Some(l) if l.user == *user => (l.peer == Some(*peer), l.exit_peer == Some(*peer)),
        _ => return false,
    };
    match side {
        Side::Direct if direct => want_for(node, device) == Want::Direct,
        Side::Direct if exit => crate::trust::trusts(node, user),
        Side::Exit if direct => want_for(node, device) == Want::ExitOut,
        _ => false,
    }
}

/// P2P 쪽 스웜의 `Auth`를 믿을 수 있는지: Tor로 인증된 그 기기가 알려 준 바로 그 노드이고, 기기가 활성이며, 이 스웜에서
/// 바라는 연결임([`link_ok`]).
fn trusted(node: &Node, side: Side, peer: &PeerId, auth: &PeerAuth) -> bool {
    matches!(
        identity::auth_status(node, auth),
        identity::AuthStatus::Active
    ) && link_ok(node, side, peer, &auth.user, &auth.device)
}

fn accept(node: &Arc<Node>, side: Side, peer: PeerId, auth: &PeerAuth) {
    // 인증을 주고받는 사이 끊겼으면(끊김 처리가 먼저 돎) 넣지 않습니다. 넣으면 다시 이어질 때까지 끊긴 연결을 인증된
    // 직접 연결로 보고 다시 걸지도 않습니다. 다시 이어지면 연결 알림에서 다시 인증합니다.
    if node.p2p.gone.lock().contains(&(side, peer)) {
        return;
    }
    node.peers.set_p2p_authed(peer, auth.user, auth.device);
    node.p2p.sides.lock().insert(peer, side);
    let spaces: BTreeSet<SpaceId> = shared_with(node, &auth.device)
        .into_iter()
        .map(|(s, _)| s)
        .collect();
    for space in spaces {
        sync_service::spawn_catch_up(node, space, peer);
    }
    // 신뢰로 일반 내용도 이 연결로 나르면 Tor 피어로 한 번 더 맞춥니다(요청은 이 연결로 갑니다).
    if let Some(tor) = normal_tor_peer(node, &peer) {
        sync_service::catch_up_peer(node, tor);
    }
}

/// 연결을 건 쪽의 P2P 인증. 이미 인증됐으면 바로 돌려줍니다.
async fn ensure_auth(node: &Arc<Node>, side: Side, peer: PeerId) -> Result<(UserId, DeviceId)> {
    if let Some(id) = node.peers.p2p_identity(&peer)
        && side_of(node, &peer) == side
    {
        return Ok(id);
    }
    let lock = node.p2p.auth_locks.lock().entry(peer).or_default().clone();
    let _g = lock.lock().await;
    if let Some(id) = node.peers.p2p_identity(&peer)
        && side_of(node, &peer) == side
    {
        return Ok(id);
    }
    let h = handle_of(node, side).ok_or_else(|| Error::Network("P2P 스웜이 꺼져 있음".into()))?;
    let local = h.local_peer_id();
    let mine = auth_for(node, &local, &peer)?;
    match h.request(peer, YpRequest::Auth(Box::new(mine))).await? {
        YpResponse::Auth(theirs) => {
            theirs
                .verify(&peer.to_bytes(), &local.to_bytes(), MAX_AUTH_SKEW_MS)
                .map_err(|_| Error::forbidden("상대 인증이 올바르지 않음"))?;
            if !trusted(node, side, &peer, &theirs) {
                return Err(Error::forbidden("이 기기와 바라는 P2P 연결이 아님"));
            }
            accept(node, side, peer, &theirs);
            Ok((theirs.user, theirs.device))
        }
        YpResponse::Error { code, .. } => Err(Error::forbidden(format!("P2P 인증 거절: {code:?}"))),
        _ => Err(Error::internal("인증 응답이 아님")),
    }
}

/// P2P 쪽 스웜에서 받은 `Auth`.
fn handle_auth(node: &Arc<Node>, side: Side, peer: PeerId, auth: PeerAuth) -> YpResponse {
    let Some(h) = handle_of(node, side) else {
        return YpResponse::error(WireErrorCode::Internal, "P2P 스웜이 꺼져 있음");
    };
    let local = h.local_peer_id();
    if auth
        .verify(&peer.to_bytes(), &local.to_bytes(), MAX_AUTH_SKEW_MS)
        .is_err()
    {
        return YpResponse::error(WireErrorCode::Unauthenticated, "인증이 올바르지 않음");
    }
    if !trusted(node, side, &peer, &auth) {
        return YpResponse::error(WireErrorCode::NotMember, "이 기기와 바라는 P2P 연결이 아님");
    }
    let reply = match auth_for(node, &local, &peer) {
        Ok(a) => a,
        Err(_) => return YpResponse::error(WireErrorCode::Internal, "신원 없음"),
    };
    accept(node, side, peer, &auth);
    YpResponse::Auth(reply)
}

// ───────────── 길 나누기 ─────────────

/// 이 피어가 P2P 쪽 스웜의 노드인지(P2P로 인증됐거나, 안내로 알게 된 노드). 둘 다 Tor 노드와 다른 임시 키라 PeerId만으로
/// 가릅니다.
pub(crate) fn is_p2p_peer(node: &Node, peer: &PeerId) -> bool {
    node.peers.p2p_identity(peer).is_some() || learned_device(node, peer).is_some()
}

/// 이 기기와 서로 신뢰하는지: 내가 그 사람을 신뢰하고, 그 기기가 안내로 나를 신뢰한다고 알려 옴(Tor 통화의 직접 연결,
/// `voice`). 안내의 사용자가 다르면(다른 사람의 기기) 아닙니다.
pub(crate) fn mutual(node: &Node, user: &UserId, device: &DeviceId) -> bool {
    let they_trust = node
        .p2p
        .learned
        .lock()
        .get(device)
        .is_some_and(|l| l.user == *user && l.trusted);
    they_trust && crate::trust::trusts(node, user)
}

/// 이 P2P 피어와의 연결로 일반 내용(공간 스트림·Tor 채널·파일·휘발성)을 받아도 되는지: 둘 중 하나라도 상대를 신뢰함.
/// 함께 참여한 채널만 있는 연결은 그 채널 내용만 나릅니다.
fn carries_normal(node: &Node, peer: &PeerId) -> bool {
    let Some((user, device)) = node.peers.p2p_identity(peer) else {
        return false;
    };
    let they_trust = node
        .p2p
        .learned
        .lock()
        .get(&device)
        .is_some_and(|l| l.trusted);
    they_trust || crate::trust::trusts(node, &user)
}

/// 일반 내용을 이 기기의 빠른 연결로 보내도 되는지: 상대가 나를 신뢰하거나, 내가 신뢰한다는 안내가 상대에게 전해졌음
/// (상대가 아직 모르면 받지 않으므로).
fn send_normal(node: &Node, device: &DeviceId, user: &UserId) -> bool {
    if node
        .p2p
        .learned
        .lock()
        .get(device)
        .is_some_and(|l| l.trusted)
    {
        return true;
    }
    crate::trust::trusts(node, user)
        && node.peers.authed_peers().iter().any(|(p, _, d)| {
            d == device
                && node
                    .p2p
                    .sent
                    .lock()
                    .get(p)
                    .is_some_and(|(h, delivered)| *delivered && h.trusted)
        })
}

/// Tor 피어(그 기기)의 빠른 연결: 일반 내용을 실어도 되는 P2P 인증 피어와 그 스웜.
fn fast_link(node: &Node, tor_peer: &PeerId) -> Option<(PeerId, Side)> {
    let (user, device) = node.peers.identity_of(tor_peer)?;
    if !send_normal(node, &device, &user) {
        return None;
    }
    let (peer, _, _) = node
        .peers
        .p2p_peers()
        .into_iter()
        .find(|(_, _, d)| *d == device)?;
    Some((peer, side_of(node, &peer)))
}

/// 데이터 요청(글·동기화·휘발성·파일)인지. 아니면 늘 그 피어의 연결·Tor로 갑니다.
fn is_data_request(req: &YpRequest) -> bool {
    matches!(
        req,
        YpRequest::PushEvents { .. }
            | YpRequest::GetHeads { .. }
            | YpRequest::GetFingerprints { .. }
            | YpRequest::GetIds { .. }
            | YpRequest::GetEvents { .. }
            | YpRequest::VolatileSnapshot { .. }
            | YpRequest::VolatilePresence { .. }
            | YpRequest::VolatileKeyWrap { .. }
            | YpRequest::BlobManifest { .. }
            | YpRequest::BlobHave { .. }
            | YpRequest::BlobChunk { .. }
    )
}

/// 이 피어에게 보내는 파일 조각 요청이 Tor를 거치지 않는 직접 연결로 가는지(함께 참가한 P2P 전용 채널, 서로 신뢰).
/// 단방향 연결은 한쪽이 Tor 출구를 거쳐 걸므로 아닙니다. 내려받기가 Tor 한도를 쓸지 정합니다.
pub(crate) fn data_goes_direct(node: &Node, peer: &PeerId) -> bool {
    let link = if is_p2p_peer(node, peer) {
        Some(*peer)
    } else {
        fast_link(node, peer).map(|(p, _)| p)
    };
    link.is_some_and(|p| match side_of(node, &p) {
        Side::Exit => false,
        Side::Direct => !node
            .p2p
            .learned
            .lock()
            .values()
            .any(|l| l.exit_peer == Some(p)),
    })
}

/// 피어에게 요청을 보냅니다. P2P 피어면 그 피어의 스웜으로. Tor 피어여도 데이터 요청이고 그 기기와 일반 내용을
/// 나르는 빠른 연결이 있으면 그쪽으로 먼저 보내고, 못 보내면 Tor로 보냅니다. 인증·친구·신호·키 갱신 등은 늘 Tor로.
pub(crate) async fn request(
    node: &Node,
    peer: PeerId,
    req: YpRequest,
) -> std::result::Result<YpResponse, NetError> {
    if is_p2p_peer(node, &peer) {
        let Some(h) = handle_of(node, side_of(node, &peer)) else {
            return Err(NetError::Shutdown);
        };
        return h.request(peer, req).await;
    }
    if is_data_request(&req)
        && let Some((fast, side)) = fast_link(node, &peer)
        && let Some(h) = handle_of(node, side)
    {
        match h.request(fast, req.clone()).await {
            Ok(YpResponse::Error {
                code: WireErrorCode::Unauthenticated,
                ..
            }) => debug!(peer = %fast, "빠른 연결 인증이 풀려 Tor로 보냄"),
            Ok(r) => return Ok(r),
            // 파일 조각이 빠른 연결의 피어당 요청 상한에 걸림(빠른 연결을 꽉 채워 쓰는 중): Tor로 돌리면 회로만 막으므로
            // 받는 쪽이 잠깐 쉬게 그대로 알립니다. 글·동기화 요청은 Tor로 넘겨 늦지 않게 합니다.
            Err(NetError::Busy) if matches!(req, YpRequest::BlobChunk { .. }) => {
                return Err(NetError::Busy);
            }
            Err(e) => debug!(peer = %fast, error = %e, "빠른 연결로 못 보내 Tor로 보냄"),
        }
    }
    node.net.request(peer, req).await
}

/// 이 사람과 지금 이어진 방식(`trust.get`). 기기 중 가장 빠른 것.
pub(crate) fn connection_mode(node: &Node, user: &UserId) -> ConnectionMode {
    fn rank(m: ConnectionMode) -> u8 {
        match m {
            ConnectionMode::P2p => 4,
            ConnectionMode::OneWayPeerExposed => 3,
            ConnectionMode::OneWayMeExposed => 2,
            ConnectionMode::Tor => 1,
            ConnectionMode::Offline => 0,
        }
    }
    let mut best = if node.peers.user_online(user) {
        ConnectionMode::Tor
    } else {
        ConnectionMode::Offline
    };
    for (peer, u, device) in node.peers.p2p_peers() {
        if u != *user {
            continue;
        }
        let exit_in = node
            .p2p
            .learned
            .lock()
            .get(&device)
            .is_some_and(|l| l.exit_peer == Some(peer));
        let m = match side_of(node, &peer) {
            Side::Exit => ConnectionMode::OneWayPeerExposed,
            Side::Direct if exit_in => ConnectionMode::OneWayMeExposed,
            Side::Direct => ConnectionMode::P2p,
        };
        if rank(m) > rank(best) {
            best = m;
        }
    }
    best
}

/// 이 이벤트가 P2P 스웜으로만 오가는지(P2P 전용 채널의 채널 스트림).
pub(crate) fn is_p2p_event(state: &SpaceState, ev: &SignedEvent) -> bool {
    matches!(ev.header.stream(), StreamId::Channel(c) if state.is_p2p(&c))
}

/// 이 스트림을 이 피어와 주고받아도 되는지: P2P 전용 채널의 채널 스트림은 그 채널에 함께 참여한 P2P 피어와만,
/// 나머지(공간 스트림·Tor 채널)는 Tor 피어나 일반 내용을 나르는 P2P 피어([`carries_normal`])와.
pub(crate) fn route_ok(
    node: &Node,
    peer: &PeerId,
    space: &SpaceId,
    state: &SpaceState,
    stream: &StreamId,
) -> bool {
    match stream {
        StreamId::Channel(c) if state.is_p2p(c) => node
            .peers
            .p2p_identity(peer)
            .is_some_and(|(_, d)| shares(node, &d, space, state, c)),
        _ => !is_p2p_peer(node, peer) || carries_normal(node, peer),
    }
}

/// 직접 연결로 받아도 되는 통화 신호: WebRTC 신호이고, 이 피어의 기기와 함께 참여한 P2P 전용 채널의 통화. 참가·키·SFU
/// 신호는 Tor로만(참가자 노드를 기록하는 신호라 연결을 섞지 않음).
fn call_signal_ok(
    node: &Node,
    peer: &PeerId,
    space: &SpaceId,
    channel: &ChannelId,
    payload: &SignalPayload,
) -> bool {
    is_webrtc_signal(payload)
        && node.space_state(space).is_some_and(|st| {
            st.is_p2p(channel)
                && node
                    .peers
                    .p2p_identity(peer)
                    .is_some_and(|(_, d)| shares(node, &d, space, &st, channel))
        })
}

fn is_webrtc_signal(p: &SignalPayload) -> bool {
    matches!(
        p,
        SignalPayload::Offer { .. }
            | SignalPayload::Answer { .. }
            | SignalPayload::Ice { .. }
            | SignalPayload::WatchScreen { .. }
    )
}

/// P2P 전용 채널 통화의 WebRTC 신호를 함께 참여한 기기와의 연결로 보냅니다. 그 채널에서는 이미 서로 IP가 보이므로 Tor를
/// 돌 까닭이 없고, Tor로 가면 신호 하나에 1초 가까이 걸려 연결이 늦었습니다. 보낼 연결이 없거나 받아 주지 않으면(예전 빌드)
/// None이라 부르는 쪽이 Tor로 보냅니다.
pub(crate) async fn request_call_signal(
    node: &Node,
    space: &SpaceId,
    channel: &ChannelId,
    device: &DeviceId,
    req: &YpRequest,
) -> Option<YpResponse> {
    let YpRequest::Signal { payload, .. } = req else {
        return None;
    };
    if !is_webrtc_signal(payload) {
        return None;
    }
    let state = node.space_state(space)?;
    if !state.is_p2p(channel) {
        return None;
    }
    let (peer, _, _) = channel_peers(node, space, &state, channel)
        .into_iter()
        .find(|(_, _, d)| d == device)?;
    match request(node, peer, req.clone()).await {
        Ok(r @ YpResponse::Ok) => Some(r),
        Ok(r) => {
            debug!(peer = %peer, response = ?r, "직접 연결이 통화 신호를 받지 않아 Tor로 보냄");
            None
        }
        Err(e) => {
            debug!(peer = %peer, error = %e, "직접 연결로 통화 신호를 못 보내 Tor로 보냄");
            None
        }
    }
}

/// 이 기기와 이 P2P 전용 채널에 함께 참여하는지(나는 지금 쓰고 있고, 상대는 안내로 알려 옴).
fn shares(
    node: &Node,
    device: &DeviceId,
    space: &SpaceId,
    state: &SpaceState,
    channel: &ChannelId,
) -> bool {
    let key = (*space, *channel);
    node.me()
        .is_ok_and(|me| joined_active(state, &me.user(), channel))
        && load(node).contains(&key)
        && node
            .p2p
            .learned
            .lock()
            .get(device)
            .is_some_and(|l| l.channels.contains(&key))
}

/// 피어가 보낸 이벤트를 받아도 되는지(`ingest`): 오가야 할 길로 온 것만. 상태를 아직 모르는 공간은 Tor 피어나 일반
/// 내용을 나르는 P2P 피어 것만.
pub(crate) fn accepts(
    node: &Node,
    peer: &PeerId,
    space: &SpaceId,
    state: Option<&SpaceState>,
    ev: &SignedEvent,
) -> bool {
    match state {
        Some(st) => route_ok(node, peer, space, st, &ev.header.stream()),
        None => !is_p2p_peer(node, peer) || carries_normal(node, peer),
    }
}

/// 이 P2P 피어가 일반 내용을 나르면 그 기기의 Tor 피어(따라잡기를 그쪽으로 돌리면 빠른 연결을 탐).
pub(crate) fn normal_tor_peer(node: &Node, peer: &PeerId) -> Option<PeerId> {
    if !carries_normal(node, peer) {
        return None;
    }
    let (_, device) = node.peers.p2p_identity(peer)?;
    node.peers.authed_peer_of_device(&device)
}

/// 이 기기가 이 채널 스트림에 써도 되는지: P2P 전용 채널이면 참여한 동안만.
pub(crate) fn may_author(node: &Node, space: &SpaceId, channel: &ChannelId) -> bool {
    match node.space_state(space) {
        Some(st) if st.is_p2p(channel) => p2p_joined(node, space, channel),
        _ => true,
    }
}

/// 전파할 이벤트를 P2P 스웜으로 갈 것과 나머지로 나눕니다(`sync_service::propagate`).
pub(crate) fn split(
    node: &Node,
    space: &SpaceId,
    events: Vec<SignedEvent>,
) -> (Vec<SignedEvent>, Vec<SignedEvent>) {
    match node.space_state(space) {
        Some(st) => events.into_iter().partition(|e| is_p2p_event(&st, e)),
        None => (Vec::new(), events),
    }
}

/// 이 피어(P2P로 인증된 기기)와 함께 참여한 이 공간의 P2P 전용 채널(휘발성 채널은 빼고: 스트림 동기화가 없음).
pub(crate) fn shared_channels(node: &Node, peer: &PeerId, space: &SpaceId) -> Vec<ChannelId> {
    let Some((_, device)) = node.peers.p2p_identity(peer) else {
        return Vec::new();
    };
    let Some(state) = node.space_state(space) else {
        return Vec::new();
    };
    shared_with(node, &device)
        .into_iter()
        .filter(|(s, c)| s == space && state.channels.get(c).is_some_and(|ch| !ch.is_volatile()))
        .map(|(_, c)| c)
        .collect()
}

/// 이 P2P 전용 채널에 함께 참여해 P2P로 인증된 피어와 그 사용자·기기(기기마다 하나).
pub(crate) fn channel_peers(
    node: &Node,
    space: &SpaceId,
    state: &SpaceState,
    channel: &ChannelId,
) -> Vec<(PeerId, UserId, DeviceId)> {
    let mut seen: HashSet<DeviceId> = HashSet::new();
    node.peers
        .p2p_peers()
        .into_iter()
        .filter(|(_, _, d)| shares(node, d, space, state, channel))
        .filter(|(_, _, d)| seen.insert(*d))
        .collect()
}

/// P2P 전용 채널의 이벤트를 함께 참여한 P2P 피어에게 보냅니다(지금 이어지지 않은 기기는 이어질 때 따라잡음).
pub(crate) fn push(node: &Arc<Node>, space: SpaceId, events: Vec<SignedEvent>) {
    let mut by_channel: BTreeMap<ChannelId, Vec<SignedEvent>> = BTreeMap::new();
    for ev in events {
        if let StreamId::Channel(c) = ev.header.stream() {
            by_channel.entry(c).or_default().push(ev);
        }
    }
    let Some(state) = node.space_state(&space) else {
        return;
    };
    for (c, evs) in by_channel {
        let evs = Arc::new(evs);
        for (peer, _, _) in channel_peers(node, &space, &state, &c) {
            let evs = evs.clone();
            node.spawn_detached(move |node| async move {
                for chunk in evs.chunks(MAX_EVENTS_PER_MESSAGE) {
                    let req = YpRequest::PushEvents {
                        space,
                        events: chunk.to_vec(),
                    };
                    match request(&node, peer, req).await {
                        Ok(YpResponse::Ok) => {}
                        Ok(r) => {
                            debug!(peer = %peer, response = ?std::mem::discriminant(&r), "P2P 전송 거절(따라잡기로 받음)");
                            return;
                        }
                        Err(e) => {
                            debug!(peer = %peer, error = %e, "P2P 전송 실패(따라잡기로 받음)");
                            return;
                        }
                    }
                }
            });
        }
    }
}

// ───────────── P2P 쪽 스웜에서 받은 요청 ─────────────

/// P2P 쪽 스웜에서 받는 요청: 인증, 기기 연결, 그리고 인증된 뒤의 데이터 요청(이벤트 전송·따라잡기·파일·휘발성 채널). 무엇을
/// 내줄지는 각 처리기가 [`route_ok`]로 가립니다. 나머지(공간·친구·통화 신호·안내 등)는 Tor로만 받습니다.
async fn handle_inbound(node: &Arc<Node>, side: Side, peer: PeerId, req: Incoming) -> YpResponse {
    if node.me.read().is_none() {
        return YpResponse::error(WireErrorCode::Unauthenticated, "이 노드에 아직 신원이 없음");
    }
    // 압축된 요청은 이 스웜에서 인증된 노드의 것만 풉니다(`inbound`와 같은 규칙).
    if req.is_packed() && (node.peers.p2p_identity(&peer).is_none() || side_of(node, &peer) != side)
    {
        return YpResponse::error(
            WireErrorCode::Unauthenticated,
            crate::inbound::PACKED_BEFORE_AUTH,
        );
    }
    // 푼 압축 요청의 예산 표식은 답을 만들 때까지 쥡니다.
    let (req, _opened) = match req.open() {
        Ok(r) => r,
        Err(resp) => return *resp,
    };
    if let YpRequest::Auth(auth) = req {
        return handle_auth(node, side, peer, *auth);
    }
    // 기기 연결은 링크의 토큰으로 가리므로 인증 전에 받습니다(직접 스웜 주소는 QR에 실려 감).
    let req = match req {
        YpRequest::DeviceLink(r) if side == Side::Direct => {
            return devlink::handle_request(node, peer, *r);
        }
        YpRequest::DeviceLinkPoll { token } if side == Side::Direct => {
            return devlink::handle_poll(node, peer, token);
        }
        req => req,
    };
    if matches!(req, YpRequest::Ping) {
        return YpResponse::Pong;
    }
    if node.peers.p2p_identity(&peer).is_none() || side_of(node, &peer) != side {
        return YpResponse::error(WireErrorCode::Unauthenticated, "인증 전");
    }
    match req {
        YpRequest::PushEvents { space, events } => {
            sync_service::handle_push(node, peer, space, events).await
        }
        YpRequest::GetHeads { space, stream } => {
            sync_service::handle_get_heads(node, peer, space, stream)
        }
        YpRequest::GetFingerprints {
            space,
            stream,
            ranges,
        } => sync_service::handle_get_fingerprints(node, peer, space, stream, ranges),
        YpRequest::GetIds {
            space,
            stream,
            range,
            limit,
        } => sync_service::handle_get_ids(node, peer, space, stream, range, limit),
        YpRequest::GetEvents { space, ids } => {
            sync_service::handle_get_events(node, peer, space, ids)
        }
        req @ (YpRequest::BlobManifest { .. }
        | YpRequest::BlobHave { .. }
        | YpRequest::BlobChunk { .. }) => blobs::handle_request(node, peer, req).await,
        YpRequest::VolatileSnapshot { space, channel } => {
            volatile::handle_snapshot(node, peer, space, channel).await
        }
        YpRequest::VolatilePresence {
            space,
            channel,
            present,
        } => volatile::handle_presence(node, peer, space, channel, present).await,
        YpRequest::VolatileKeyWrap {
            space,
            channel,
            generation,
            wrap,
            recipients,
        } => volatile::handle_key_wrap(node, peer, space, channel, generation, wrap, recipients),
        // P2P 전용 채널의 글 쓰는 중 알림(처리기가 `route_ok`로 채널을 가림).
        YpRequest::Typing { space, channel } => crate::typing::handle(node, peer, space, channel),
        YpRequest::Signal {
            space,
            channel,
            call,
            from,
            payload,
        } if call_signal_ok(node, &peer, &space, &channel, &payload) => {
            crate::voice::handle_signal(node, peer, space, channel, call, from, payload).await
        }
        _ => YpResponse::error(WireErrorCode::Forbidden, "P2P 연결에서는 받지 않는 요청"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_dials_only_ip_addresses() {
        let ok = |s: &str| link_dialable(&s.parse().unwrap());
        assert!(ok("/ip4/192.168.0.3/udp/4001/quic-v1"));
        assert!(ok("/ip6/2001:db8::1/tcp/4001"));
        assert!(ok("/ip4/127.0.0.1/tcp/4001"), "시험 노드는 루프백을 씀");
        for bad in [
            "/dns/example.com/udp/443/quic-v1",
            "/dns4/example.com/tcp/443",
            "/dns6/example.com/tcp/443",
            "/dnsaddr/example.com",
            "/ip4/0.0.0.0/tcp/4001",
            "/ip4/192.0.2.1/tcp/4001/p2p-circuit",
        ] {
            assert!(!ok(bad), "{bad}");
        }
    }
}
