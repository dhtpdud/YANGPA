//! 피어: 연결·인증(`Auth`)·노드 주소 기록.
//!
//! - 연결되면 저장소가 신원을 아는 노드(친구·멤버)에게 `Auth`를 보내고, 인증되면 따라잡기를 시작합니다.
//! - `Auth`를 받으면 그 기기가 내 기기·친구·같은 공간 멤버의 활성 기기일 때만 내 `Auth`로 답합니다. 내가 아는
//!   목록에서 폐기된 기기에는 `Revoked`로 답해 그 기기가 스스로 지우게 합니다(`identity::refuse_auth`).
//! - 친구 링크 사용(`RedeemFriendLink`)과 친구 수락(`FriendAccepted`)은 인증 전에도 받습니다(`inbound` 참고).
//! - 연결에 성공한 주소는 `peer_cache`에 남겨 재시작 때 먼저 다시 연결합니다.
//! - 인증되면 서로의 링크용 주소(onion, 직접 연결 스웜이면 릴레이 경유)를 `Social(Contact)`로 주고받고, 내 주소가
//!   바뀌면 다시 알립니다. Tor는 identify가 없어 들어온 연결의 주소를 모르므로 다시 걸 주소가 이것뿐입니다.
//! - 끊긴 친구·내 기기·작은 크루의 멤버 기기에는 주기적으로 다시 겁니다(`spawn_redial`, 기기마다 백오프). 노드를 모르는
//!   기기는 위치 기록·멤버 소개(`locator::lookup`)로 찾고 인증으로 그 기기임을 확인합니다(`reach_found`). 기기가
//!   예전에 쓰던 PeerId(업그레이드 전의 노드, 예전 빌드 친구가 알려 준 다른 모드의 노드)도 기억해 같이 걸어 봅니다
//!   (`peer.prev.<PeerId>`).
//! - 친구·내 기기끼리는 `Contact`에 노드(PeerId와 그 노드가 연결받는 onion 주소)도 실어, 상대가 PeerId를 바꿔도(예전
//!   빌드에서 업그레이드) 다시 걸 수 있게 합니다.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tracing::debug;
use yangpa_core::ids::{DeviceId, SpaceId, UserId};
use yangpa_core::model::{SpaceKind, SpaceState};
use yangpa_core::time::Timestamp;
use yangpa_core::wire::{
    MAX_CHECK_JOIN_SPACES, MAX_CONTACT_ADDRS, MAX_NODE_CONTACTS, NodeContact, PeerAuth,
    SocialRequest, WireErrorCode, YpRequest, YpResponse,
};
use yangpa_net::{Multiaddr, NetMode, NetStatus, PeerId, addr};
use yangpa_rpc::api::Notification;

use crate::error::{Error, Result};
use crate::node::Node;
use crate::{crews, friends, identity, policy, sync_service, voice, volatile};

/// 인증 시각 허용 오차.
pub(crate) const MAX_AUTH_SKEW_MS: u64 = 5 * 60 * 1000;
/// 재시작 때 다시 연결해 볼 노드 수.
const RECONNECT_LIMIT: u32 = 64;
/// 노드 하나에 남기는 주소 수(새로 들은 것 먼저).
const MAX_KEPT_ADDRS: usize = 16;
/// 다시 걸기 간격의 상한.
const MAX_REDIAL_BACKOFF: Duration = Duration::from_secs(15 * 60);
/// 한 번에 다시 걸어 보는 기기 수(Tor 스웜은 시도마다 회로를 만듦).
const MAX_REDIALS_PER_TICK: usize = 4;
/// 모르는 기기에게서 그 기기의 가입을 놓쳤는지 알아보는 간격(다시 걸기 주기의 배수, 기본 10분)과 그 사이 전체 상한.
const JOIN_PROBE_EVERY: u32 = 10;
const MAX_JOIN_PROBES: usize = 8;
/// 멤버로 몰라 거절한 연결을 기억하는 수([`PeerDirectory::note_refused`]).
const MAX_REFUSED: usize = 32;
/// 기기가 예전에 쓰던 PeerId(`peer.prev.<PeerId>` → [`PrevPeer`]).
const PREV_PEER_PREFIX: &str = "peer.prev.";

/// 기억하는 끊긴 피어 수(넘으면 비움: 그때는 늦은 인증 하나가 남을 수 있음).
const GONE_MAX: usize = 4096;

#[derive(Default)]
struct Inner {
    authed: HashMap<PeerId, (UserId, DeviceId)>,
    /// P2P 스웜(`crate::p2p`)에서 인증된 피어. 신원 확인(`identity_of`)에만 함께 쓰고, 기기의 노드·접속 상태·
    /// 주소 기록에는 쓰지 않습니다(Tor로 오갈 것이 P2P로 새지 않게).
    p2p_authed: HashMap<PeerId, (UserId, DeviceId)>,
    connected: HashSet<PeerId>,
    /// 끊긴 뒤 아직 다시 이어지지 않은 피어(끊김 처리 뒤 늦게 끝난 인증을 넣지 않으려고, [`GONE_MAX`]개까지).
    gone: HashSet<PeerId>,
    auth_locks: HashMap<PeerId, Arc<tokio::sync::Mutex<()>>>,
    /// 다시 걸 기기마다 (다음 시도 시각, 그다음 간격).
    redial: HashMap<DeviceId, (Instant, Duration)>,
    /// gossip 이웃으로 걸어 본 기기마다 (다음 시도 시각, 그다음 간격). 인증되면 지웁니다.
    neighbor_wait: HashMap<DeviceId, (Instant, Duration)>,
    /// 가입을 놓쳤는지 알아본 모르는 기기와 그 시각([`handle_check_join`]).
    join_probes: HashMap<DeviceId, Instant>,
    /// `NotMember`로 거절한 연결의 기기([`MAX_REFUSED`]개까지, 끊기거나 인증되면 지움). 상태가 따라잡혀 그 기기를 멤버로
    /// 알게 되면 이쪽에서 인증합니다([`on_state_applied`]).
    refused: HashMap<PeerId, (UserId, DeviceId)>,
    /// 마지막으로 알린 내 링크용 주소(PeerId와 함께, 바뀌면 인증된 피어에게 다시 알림).
    announced: Option<(PeerId, Vec<Multiaddr>)>,
    /// 시작할 때 다시 건 친구·내 기기 연결 중 끝나지 않은 것의 사용자별 수(앱이 '연결 중'으로 보임).
    startup: HashMap<UserId, u32>,
}

/// 기기가 예전에 쓰던 노드(업그레이드 전의 PeerId, 예전 빌드 친구가 알려 준 다른 모드의 노드). 예전 빌드는 아직
/// 모드를 바꿔 그 노드로 다시 뜰 수 있어 걸어 보고, 그 노드와 연결되면 신원을 모르는 노드라도 인증을 보냅니다.
#[derive(serde::Serialize, serde::Deserialize)]
struct PrevPeer {
    user: UserId,
    device: DeviceId,
}

fn prev_key(peer: &PeerId) -> String {
    format!("{PREV_PEER_PREFIX}{peer}")
}

/// 예전 노드([`PrevPeer`])로 기억한 PeerId가 묶인 기기.
pub(crate) fn prev_peer_device(node: &Node, peer: &PeerId) -> Option<DeviceId> {
    node.store
        .get_setting::<PrevPeer>(&prev_key(peer))
        .ok()
        .flatten()
        .map(|p| p.device)
}

/// PeerId ↔ (사용자, 기기)와 연결 상태.
#[derive(Default)]
pub(crate) struct PeerDirectory {
    inner: Mutex<Inner>,
}

impl PeerDirectory {
    /// 인증된 피어(Tor 스웜, 또는 P2P 스웜)의 사용자·기기.
    pub(crate) fn identity_of(&self, peer: &PeerId) -> Option<(UserId, DeviceId)> {
        let inner = self.inner.lock();
        inner
            .authed
            .get(peer)
            .or_else(|| inner.p2p_authed.get(peer))
            .copied()
    }

    /// P2P 스웜에서 인증된 피어의 사용자·기기.
    pub(crate) fn p2p_identity(&self, peer: &PeerId) -> Option<(UserId, DeviceId)> {
        self.inner.lock().p2p_authed.get(peer).copied()
    }

    /// P2P 스웜에서 인증된 피어 모두.
    pub(crate) fn p2p_peers(&self) -> Vec<(PeerId, UserId, DeviceId)> {
        self.inner
            .lock()
            .p2p_authed
            .iter()
            .map(|(p, (u, d))| (*p, *u, *d))
            .collect()
    }

    pub(crate) fn set_p2p_authed(&self, peer: PeerId, user: UserId, device: DeviceId) {
        let mut inner = self.inner.lock();
        // 한 기기는 P2P 노드 하나(다시 띄우면 새 키)라 예전 것은 지웁니다.
        inner.p2p_authed.retain(|_, (_, d)| *d != device);
        inner.p2p_authed.insert(peer, (user, device));
    }

    /// P2P 피어 인증을 지웁니다(`None`이면 모두, P2P 스웜을 멈출 때).
    pub(crate) fn forget_p2p(&self, peer: Option<&PeerId>) {
        let mut inner = self.inner.lock();
        match peer {
            Some(p) => {
                inner.p2p_authed.remove(p);
            }
            None => inner.p2p_authed.clear(),
        }
    }

    pub(crate) fn is_connected(&self, peer: &PeerId) -> bool {
        self.inner.lock().connected.contains(peer)
    }

    /// 이웃 고르기가 이 기기에 지금 걸어도 되는지.
    pub(crate) fn neighbor_due(&self, device: &DeviceId, now: Instant) -> bool {
        self.inner
            .lock()
            .neighbor_wait
            .get(device)
            .is_none_or(|(at, _)| *at <= now)
    }

    /// 이웃으로 걸기 시작한 기기: 인증되기 전까지 다음 시도를 다시 걸기처럼 두 배씩(최대 15분) 미룹니다. 꺼진 기기에
    /// 바퀴마다 Tor 회로를 쓰지 않게.
    pub(crate) fn neighbor_dialed(&self, device: DeviceId, now: Instant, base: Duration) {
        let mut inner = self.inner.lock();
        let e = inner.neighbor_wait.entry(device).or_insert((now, base));
        let wait = e.1.max(base);
        *e = (now + wait, (wait * 2).min(MAX_REDIAL_BACKOFF.max(base)));
    }

    /// 이 모르는 기기의 가입을 지금 알아봐도 되는지: 기기마다 `every`에 한 번, 그 사이 전체 [`MAX_JOIN_PROBES`]번.
    fn join_probe_due(&self, device: DeviceId, every: Duration) -> bool {
        let mut inner = self.inner.lock();
        inner.join_probes.retain(|_, at| at.elapsed() < every);
        if inner.join_probes.contains_key(&device) || inner.join_probes.len() >= MAX_JOIN_PROBES {
            return false;
        }
        inner.join_probes.insert(device, Instant::now());
        true
    }

    /// 멤버로 몰라 거절한 연결을 기억합니다. 끊긴 연결(끊김 처리가 먼저 돎)은 넣지 않습니다.
    fn note_refused(&self, peer: PeerId, user: UserId, device: DeviceId) {
        let mut inner = self.inner.lock();
        if (inner.gone.contains(&peer) && !inner.connected.contains(&peer))
            || inner.refused.len() >= MAX_REFUSED
        {
            return;
        }
        inner.refused.insert(peer, (user, device));
    }

    /// 거절한 연결 중 이제 `trusted`인 기기의 것을 꺼냅니다.
    fn take_refused(&self, trusted: impl Fn(&UserId, &DeviceId) -> bool) -> Vec<PeerId> {
        self.inner
            .lock()
            .refused
            .extract_if(|_, (u, d)| trusted(u, d))
            .map(|(p, _)| p)
            .collect()
    }

    /// 지금 연결 수(이웃 고르기의 상한 판단).
    /// 시작 연결이 아직 끝나지 않은 사용자.
    pub(crate) fn connecting(&self) -> Vec<UserId> {
        self.inner.lock().startup.keys().copied().collect()
    }

    pub(crate) fn connected_count(&self) -> usize {
        self.inner.lock().connected.len()
    }

    /// 지금 인증된 연결이 있는 기기의 PeerId.
    pub(crate) fn authed_peer_of_device(&self, device: &DeviceId) -> Option<PeerId> {
        self.inner
            .lock()
            .authed
            .iter()
            .find(|(_, (_, d))| d == device)
            .map(|(p, _)| *p)
    }

    /// 사용자의 기기 중 하나라도 인증된 연결이 있는지.
    pub(crate) fn user_online(&self, user: &UserId) -> bool {
        self.inner.lock().authed.values().any(|(u, _)| u == user)
    }

    pub(crate) fn authed_peers(&self) -> Vec<(PeerId, UserId, DeviceId)> {
        self.inner
            .lock()
            .authed
            .iter()
            .map(|(p, (u, d))| (*p, *u, *d))
            .collect()
    }

    /// 지금 연결된 피어(인증 여부와 무관).
    pub(crate) fn connected_peers(&self) -> Vec<PeerId> {
        self.inner.lock().connected.iter().copied().collect()
    }

    /// 폐기된 기기의 인증을 지웁니다(그 연결의 요청은 다시 인증해야 하고, 인증은 거절됨).
    pub(crate) fn forget_devices(&self, devices: &[DeviceId]) {
        if devices.is_empty() {
            return;
        }
        let mut inner = self.inner.lock();
        inner.authed.retain(|_, (_, d)| !devices.contains(d));
        inner.p2p_authed.retain(|_, (_, d)| !devices.contains(d));
    }

    /// 인증된 피어로 기억합니다. 새로 온라인이 됐으면 `Some(true)`. 인증을 주고받는 사이 연결이 끊겼으면(끊김 처리가
    /// 먼저 돎) 넣지 않고 `None`: 넣으면 끊긴 기기가 다시 연결될 때까지 온라인으로 남습니다. 연결 알림보다 먼저 끝난
    /// 인증(바쁠 때 건 쪽의 인증이 이벤트 처리보다 빠름)은 막지 않습니다. 다시 이어지면 `on_connected`가 다시 인증합니다.
    fn set_authed(&self, peer: PeerId, user: UserId, device: DeviceId) -> Option<bool> {
        let mut inner = self.inner.lock();
        if inner.gone.contains(&peer) && !inner.connected.contains(&peer) {
            return None;
        }
        let was_online = inner.authed.values().any(|(u, _)| *u == user);
        inner.authed.insert(peer, (user, device));
        inner.neighbor_wait.remove(&device);
        inner.refused.remove(&peer);
        Some(!was_online)
    }

    fn auth_lock(&self, peer: &PeerId) -> Arc<tokio::sync::Mutex<()>> {
        self.inner
            .lock()
            .auth_locks
            .entry(*peer)
            .or_default()
            .clone()
    }
}

/// 기기의 노드(지금 인증된 연결, 없으면 저장된 주소 기록).
pub(crate) fn peer_for_device(node: &Node, device: &DeviceId) -> Option<PeerId> {
    if let Some(p) = node.peers.authed_peer_of_device(device) {
        return Some(p);
    }
    let rec = node.store.peer_for_device(device).ok()??;
    let peer = PeerId::from_bytes(&rec.peer_id).ok()?;
    let addrs: Vec<Multiaddr> = rec.addrs.iter().filter_map(|a| a.parse().ok()).collect();
    if !addrs.is_empty() {
        node.net.add_addresses(peer, addrs);
    }
    Some(peer)
}

/// 이 `Auth`의 기기를 믿을 수 있는지: 내 기기, 같은 공간(내가 멤버인) 멤버의 활성 기기, 친구의 활성 기기, 초대 가입
/// 대기 중인 공간의 받아 준 노드·공간을 만든 사용자의 기기(`crews::trusted_for_join`, 상태를 풀기 전이라 멤버를 모름),
/// 보관 관계의 기기(`archive::trusts`: 지정된 보관 노드, 보관 쪽이면 받아 둔 공간 명단의 멤버 기기).
/// 기기는 내가 아는 그 사용자의 목록에서 활성이거나, 그 목록에서 활성인 기기가 공동 서명한 새 판에서 활성이어야
/// 합니다(`identity::auth_status`: 폐기된 기기가 신원 키로 자기를 되살린 판·새 기기를 더한 판은 받지 않음). 아직 받지
/// 않은 기기는 함께 있는 공간의 상태가 이미 그 기기를 담았을 때만(다른 멤버가 전함) 믿습니다.
fn is_trusted(node: &Node, peer: &PeerId, auth: &PeerAuth) -> bool {
    let Ok(me) = node.me() else { return false };
    // 이 기기에서 차단한 남의 기기는 어떤 관계로도 믿지 않습니다(`moderation::block_device`).
    if crate::moderation::is_device_blocked(node, &auth.device) {
        return false;
    }
    // 만료된 연락처와의 1:1 공간은 비활성화돼 믿을 근거가 되지 않습니다.
    let member = || {
        node.space_ids().iter().any(|s| {
            node.space_state(s).is_some_and(|st| {
                st.is_member(&me.user()) && st.device_belongs(&auth.user, &auth.device)
            }) && !friends::space_deactivated(node, s)
        })
    };
    match identity::auth_status(node, auth) {
        identity::AuthStatus::Active => {}
        identity::AuthStatus::Revoked(_) | identity::AuthStatus::Unknown => return false,
    }
    if auth.user == me.user() || member() {
        return true;
    }
    // 친구·임시 연락처(또는 내가 연락처 링크를 쓴 사람)의 기기: 서명된 기기 목록(신원 키로 검증됨)에 있으면 믿습니다.
    node.store
        .get_friend(&auth.user)
        .ok()
        .flatten()
        .is_some_and(|f| f.is_active())
        || friends::has_outgoing_request(node, &auth.user)
        || friends::has_pending_leave(node, &auth.user)
        || crews::trusted_for_join(node, peer, &auth.user)
        || crate::archive::trusts(node, &auth.user, &auth.device)
}

/// 내가 멤버이고 `device`도 멤버 기기로 담은 크루.
fn shared_crews(node: &Node, user: &UserId, device: &DeviceId) -> Vec<SpaceId> {
    let Ok(me) = node.me() else {
        return Vec::new();
    };
    node.space_ids()
        .into_iter()
        .filter(|s| {
            !friends::space_deactivated(node, s)
                && node.space_state(s).is_some_and(|st| {
                    st.kind == SpaceKind::Crew
                        && st.is_member(&me.user())
                        && st.device_belongs(user, device)
                })
        })
        .collect()
}

/// 찾은 기기가 내 `Auth`를 거절함: 그 기기가 꺼진 동안 들어온 내 가입을 놓쳤을 수 있습니다. 내 상태가 그 기기를 멤버로
/// 담은 크루만 지목해 확인을 청합니다(상대는 그 크루만 내게서 받고 나를 다시 인증함, [`handle_check_join`]).
async fn ask_check_join(node: &Arc<Node>, peer: PeerId, user: &UserId, device: &DeviceId) {
    let mut spaces = shared_crews(node, user, device);
    if spaces.is_empty() {
        return;
    }
    spaces.truncate(MAX_CHECK_JOIN_SPACES);
    let Ok(auth) = my_auth(node, &peer) else {
        return;
    };
    let req = YpRequest::CheckJoin {
        spaces,
        auth: Box::new(auth),
    };
    if let Err(e) = node.net.request(peer, req).await {
        debug!(peer = %peer, error = %e, "놓친 가입 확인 요청 실패");
    }
}

/// `CheckJoin`: 내가 꺼진 동안 들어온 멤버일 수 있는 기기가 크루를 지목했습니다. 그 가입을 아는 다른 멤버가 모두 꺼져
/// 있으면 그 기기에게서 말고는 알 길이 없으므로, 내 인증을 보내(상대는 나를 멤버로 알면 받아 줌) 지목된 크루 중 내가
/// 멤버인 것의 공간 스트림만 받고 다시 인증합니다. 내 다른 크루는 묻지 않습니다. 받은 이벤트는 늘 하듯 서명·해소기로
/// 검사하고, 그 기기를 믿는 것은 내 상태가 그 기기를 멤버로 담은 뒤뿐입니다. 남는 노출: 그 크루 ID와 내 주소를 아는
/// 누구나 내 인증(사용자 ID·기기 목록)을 받고, 아는 크루 ID에 내가 있는지 알 수 있습니다. 아무 피어나 일을 시키거나
/// 끝없이 묻지 못하게 기기마다 한 번, 전체 몇 번으로 묶습니다([`MAX_JOIN_PROBES`]).
pub(crate) fn handle_check_join(
    node: &Arc<Node>,
    peer: PeerId,
    spaces: &[SpaceId],
    auth: &PeerAuth,
) -> YpResponse {
    if auth
        .verify(
            &peer.to_bytes(),
            &node.peer_id().to_bytes(),
            MAX_AUTH_SKEW_MS,
        )
        .is_err()
    {
        return YpResponse::error(WireErrorCode::Unauthenticated, "인증이 올바르지 않음");
    }
    if spaces.is_empty() || spaces.len() > MAX_CHECK_JOIN_SPACES {
        return YpResponse::error(WireErrorCode::BadRequest, "지목한 크루 수가 맞지 않음");
    }
    let Ok(me) = node.me() else {
        return YpResponse::error(WireErrorCode::Internal, "신원 없음");
    };
    if auth.user == me.user()
        || !matches!(
            identity::auth_status(node, auth),
            identity::AuthStatus::Active
        )
    {
        return YpResponse::error(WireErrorCode::Forbidden, "확인할 수 없는 기기");
    }
    // 멤버인지 보기 전에 묶습니다: 아니면 아는 크루 ID마다 내가 그 크루에 있는지를 끝없이 물어볼 수 있음. 묶인 동안은
    // 멤버 여부와 무관하게 `Ok`.
    if !node
        .peers
        .join_probe_due(auth.device, node.cfg.redial_interval * JOIN_PROBE_EVERY)
    {
        return YpResponse::Ok;
    }
    let crews: Vec<SpaceId> = node
        .space_ids()
        .into_iter()
        .filter(|s| {
            spaces.contains(s)
                && !friends::space_deactivated(node, s)
                && node.space_state(s).is_some_and(|st| {
                    // 밴된 사람이 놓친 가입을 핑계로 내가 자기에게서 공간 기록을 받아 가게 하지 못하게.
                    st.kind == SpaceKind::Crew
                        && st.is_member(&me.user())
                        && !st.bans.contains_key(&auth.user)
                })
        })
        .collect();
    if crews.is_empty() {
        return YpResponse::error(WireErrorCode::NotMember, "지목한 크루의 멤버가 아님");
    }
    let (user, device) = (auth.user, auth.device);
    node.spawn_detached(move |node| async move {
        // 상대가 나를 받아 주어야 따라잡기에 답합니다. 답한 인증이 같은 기기인지만 보고 아직 기억하지 않습니다.
        let Ok(mine) = my_auth(&node, &peer) else {
            return;
        };
        match node
            .net
            .request(peer, YpRequest::Auth(Box::new(mine)))
            .await
        {
            Ok(YpResponse::Auth(theirs))
                if theirs.user == user
                    && theirs.device == device
                    && theirs
                        .verify(
                            &peer.to_bytes(),
                            &node.peer_id().to_bytes(),
                            MAX_AUTH_SKEW_MS,
                        )
                        .is_ok() => {}
            _ => return,
        }
        for space in crews {
            if let Err(e) = sync_service::pull_space_stream(&node, space, peer).await {
                debug!(peer = %peer, error = %e, "가입 확인용 공간 따라잡기 실패");
            }
        }
        if ensure_auth(&node, peer).await.is_ok() {
            debug!(peer = %peer, "놓친 가입을 확인하고 인증");
            sync_service::catch_up_peer(&node, peer);
        }
    });
    YpResponse::Ok
}

fn my_auth(node: &Node, peer: &PeerId) -> Result<PeerAuth> {
    let me = node.me()?;
    Ok(PeerAuth::create(
        &me.device.signing,
        me.devices.clone(),
        &node.peer_id().to_bytes(),
        &peer.to_bytes(),
    ))
}

fn remember(node: &Arc<Node>, peer: &PeerId, auth: &PeerAuth) {
    let _ = identity::accept_device_list(node, &auth.devices);
    // 기기가 다른 PeerId로 옮겨 왔으면(예전 빌드에서 업그레이드 등) 예전 노드를 기억해 둡니다. 저장소는 기기를 노드
    // 하나에만 묶습니다.
    if let Ok(Some(old)) = node.store.peer_for_device(&auth.device)
        && old.peer_id != peer.to_bytes()
        && !old.addrs.is_empty()
        && let Ok(old_peer) = PeerId::from_bytes(&old.peer_id)
    {
        let prev = PrevPeer {
            user: auth.user,
            device: auth.device,
        };
        let _ = node.store.put_setting(&prev_key(&old_peer), &prev);
    }
    let _ = node.store.delete_setting(&prev_key(peer));
    let _ = node
        .store
        .set_peer_identity(&peer.to_bytes(), &auth.user, &auth.device);
    if node
        .peers
        .set_authed(*peer, auth.user, auth.device)
        .is_none()
    {
        return;
    }
    // 믿기로 한 뒤 여기까지 오는 사이 이 기기를 폐기한 목록이 들어왔으면(그 목록의 `forget_revoked`가 먼저 돌아 위에서
    // 다시 넣은 셈) 지금 잊습니다. 목록은 저장한 뒤 잊으므로 여기서 다시 읽으면 놓치지 않습니다.
    if matches!(
        identity::auth_status(node, auth),
        identity::AuthStatus::Revoked(_)
    ) {
        node.peers.forget_devices(&[auth.device]);
        return;
    }
    // 이 기기가 멤버인 공간의 gossip 허용 목록에 새 PeerId를 넣습니다(상태가 바뀔 때만 다시 세우면 늦게 인증된
    // 멤버의 gossip이 무시되고 그 피어가 감점됨).
    for space in node.space_ids() {
        if node
            .space_state(&space)
            .is_some_and(|st| st.device_belongs(&auth.user, &auth.device))
        {
            sync_service::refresh_space_peers(node, &space);
        }
    }
    // 내 표시 상태를 보내고, 상대 상태를 받으면(옛 빌드는 잠시 뒤) 접속을 알립니다.
    crate::presence::on_authed(node, *peer, auth.user, auth.device);
    // 상대 수신 주소 기록(들어온 연결도 재시작 뒤 다시 걸게)·통화 재알림.
    crate::locator::on_peer_authed(node, peer, &auth.user, &auth.device);
    // 함께 참여한 P2P 전용 채널이 있으면 P2P 노드를 알립니다.
    crate::p2p::on_peer_authed(node, *peer, &auth.user);
    // 전하지 못한 친구 추가 요청.
    friends::on_peer_authed(node, &auth.user);
}

/// 만료된 연락처처럼 더는 믿지 않게 된 사용자의 인증을 지웁니다(다른 공간을 함께 쓰면 그대로 둠). 그 연결의 요청은
/// 다시 인증해야 하고, 인증은 거절됩니다.
pub(crate) fn drop_untrusted_user(node: &Node, user: &UserId) {
    let Ok(me) = node.me() else { return };
    if me.user() == *user {
        return;
    }
    let devices: Vec<DeviceId> = node
        .peers
        .authed_peers()
        .into_iter()
        .filter(|(_, u, d)| {
            u == user
                && !node.space_ids().iter().any(|s| {
                    node.space_state(s)
                        .is_some_and(|st| st.is_member(&me.user()) && st.device_belongs(u, d))
                        && !friends::space_deactivated(node, s)
                })
                && !node
                    .store
                    .get_friend(u)
                    .ok()
                    .flatten()
                    .is_some_and(|f| f.is_active())
        })
        .map(|(_, _, d)| d)
        .collect();
    node.peers.forget_devices(&devices);
    for d in devices {
        crate::presence::on_gone(node, *user, d);
    }
}

/// 연결을 건 쪽의 인증. 이미 인증됐으면 바로 돌려줍니다.
pub(crate) async fn ensure_auth(node: &Arc<Node>, peer: PeerId) -> Result<(UserId, DeviceId)> {
    if let Some(id) = node.peers.identity_of(&peer) {
        return Ok(id);
    }
    let lock = node.peers.auth_lock(&peer);
    let _g = lock.lock().await;
    if let Some(id) = node.peers.identity_of(&peer) {
        return Ok(id);
    }
    let auth = my_auth(node, &peer)?;
    let contact = friends::peer_contact(node, &peer);
    match node
        .net
        .request(peer, YpRequest::Auth(Box::new(auth)))
        .await?
    {
        YpResponse::Auth(theirs) => {
            theirs
                .verify(
                    &peer.to_bytes(),
                    &node.peer_id().to_bytes(),
                    MAX_AUTH_SKEW_MS,
                )
                .map_err(|_| Error::forbidden("상대 인증이 올바르지 않음"))?;
            if !is_trusted(node, &peer, &theirs) {
                return Err(Error::forbidden("친구·멤버가 아닌 기기"));
            }
            remember(node, &peer, &theirs);
            // 보관 관계로만 믿는 상대와는 프로필 등을 주고받지 않습니다.
            if !crate::archive::archive_only(node, &theirs.user, &theirs.device) {
                identity::on_authenticated(node, peer, theirs.user, theirs.device);
            }
            Ok((theirs.user, theirs.device))
        }
        YpResponse::Revoked { devices } => {
            identity::on_revoked(node, peer, &devices);
            Err(Error::forbidden("이 기기는 폐기됐습니다"))
        }
        YpResponse::Error { code, .. } => {
            if code == WireErrorCode::NotMember {
                rejected_as_stranger(node, &peer);
            }
            // 상대 쪽에서 연락처 링크가 만료됨: 이쪽 임시 연락처도 만료합니다.
            if code == WireErrorCode::Expired {
                friends::on_expired_by_peer(node, &peer, contact);
            }
            Err(Error::forbidden(format!("인증 거절: {code:?}")))
        }
        _ => Err(Error::internal("인증 응답이 아님")),
    }
}

/// 상대가 나를 더는 친구·멤버로 보지 않음(`NotMember`): 그 사람에게 전할 친구 삭제는 이미 반영됐으므로 지웁니다.
fn rejected_as_stranger(node: &Node, peer: &PeerId) {
    if let Some(user) = node
        .store
        .get_peer(&peer.to_bytes())
        .ok()
        .flatten()
        .and_then(|r| r.user)
    {
        friends::clear_pending_leave(node, &user);
    }
}

/// 이미 인증된 연결이라도 내 `Auth`를 다시 보내 상대가 아직 나를 받아 주는지 봅니다(상대가 먼저 걸어 와 나만 상대를
/// 받아 준 경우, 친구 삭제 전달용). 받아 주면 다시 기억하고 true. `NotMember`로 거절하면 전할 친구 삭제를 지웁니다.
pub(crate) async fn reconfirm_auth(node: &Arc<Node>, peer: PeerId) -> bool {
    let lock = node.peers.auth_lock(&peer);
    let _g = lock.lock().await;
    let Ok(auth) = my_auth(node, &peer) else {
        return false;
    };
    let contact = friends::peer_contact(node, &peer);
    match node
        .net
        .request(peer, YpRequest::Auth(Box::new(auth)))
        .await
    {
        Ok(YpResponse::Auth(theirs)) => {
            let ok = theirs
                .verify(
                    &peer.to_bytes(),
                    &node.peer_id().to_bytes(),
                    MAX_AUTH_SKEW_MS,
                )
                .is_ok()
                && is_trusted(node, &peer, &theirs);
            if ok {
                remember(node, &peer, &theirs);
            }
            ok
        }
        Ok(YpResponse::Error {
            code: WireErrorCode::NotMember,
            ..
        }) => {
            rejected_as_stranger(node, &peer);
            false
        }
        // `ensure_auth`와 같이: 이쪽만 상대를 인증된 피어로 알 때 상대의 만료를 여기서만 알게 됩니다.
        Ok(YpResponse::Error {
            code: WireErrorCode::Expired,
            ..
        }) => {
            friends::on_expired_by_peer(node, &peer, contact);
            false
        }
        Ok(YpResponse::Revoked { devices }) => {
            identity::on_revoked(node, peer, &devices);
            false
        }
        _ => false,
    }
}

/// 받은 `Auth`.
pub(crate) fn handle_auth(node: &Arc<Node>, peer: PeerId, auth: PeerAuth) -> YpResponse {
    if auth
        .verify(
            &peer.to_bytes(),
            &node.peer_id().to_bytes(),
            MAX_AUTH_SKEW_MS,
        )
        .is_err()
    {
        return YpResponse::error(WireErrorCode::Unauthenticated, "인증이 올바르지 않음");
    }
    if !is_trusted(node, &peer, &auth) {
        if friends::is_expired_contact(node, &auth.user) {
            return YpResponse::error(WireErrorCode::Expired, "만료된 연락처");
        }
        let refused = identity::refuse_auth(node, &auth);
        if matches!(
            refused,
            YpResponse::Error {
                code: WireErrorCode::NotMember,
                ..
            }
        ) {
            node.peers.note_refused(peer, auth.user, auth.device);
            // 믿을지 본 뒤 기록하기 전에 상태가 따라잡혔으면 그 적용은 이 기록을 보지 못했습니다.
            if is_trusted(node, &peer, &auth) {
                auth_refused(node, node.peers.take_refused(|_, d| *d == auth.device));
            }
        }
        return refused;
    }
    let reply = match my_auth(node, &peer) {
        Ok(a) => a,
        Err(_) => return YpResponse::error(WireErrorCode::Internal, "신원 없음"),
    };
    remember(node, &peer, &auth);
    // 상대가 먼저 걸었어도 이쪽도 따라잡고, 전하지 못한 친구 삭제를 전합니다(건 쪽만 하면 상대가 먼저 걸 때 영영 못 전함).
    sync_service::catch_up_peer(node, peer);
    if !crate::archive::archive_only(node, &auth.user, &auth.device) {
        identity::on_auth_accepted(node, peer, auth.user);
    }
    YpResponse::Auth(reply)
}

/// 공간 상태가 바뀜(`crews::on_state_applied`): `NotMember`로 거절한 연결의 기기가 이제 이 공간 멤버 기기면 이쪽에서
/// 인증합니다. 막 들어온 멤버를 소개받아 걸어 온 쪽은 그 가입이 아직 이쪽에 닿지 않아 거절되면, 연결이 남은 동안 다시
/// 걸지 않고(`neighbors::plan`) 놓친 가입 확인도 기기마다 드물게만 되므로 가입을 받은 이쪽이 먼저 걸어야 곧 이어집니다.
pub(crate) fn on_state_applied(node: &Arc<Node>, state: &SpaceState) {
    if !node.me().is_ok_and(|m| state.is_member(&m.user())) {
        return;
    }
    let due = node
        .peers
        .take_refused(|u, d| state.is_member(u) && state.device_belongs(u, d));
    auth_refused(node, due);
}

/// 거절했던 연결에 내 `Auth`를 보내 인증하고 따라잡습니다(`ensure_auth`가 다시 믿을지 봄).
fn auth_refused(node: &Arc<Node>, peers: Vec<PeerId>) {
    for peer in peers {
        node.spawn_detached(move |node| async move {
            match ensure_auth(&node, peer).await {
                Ok(_) => sync_service::catch_up_peer(&node, peer),
                Err(e) => debug!(peer = %peer, error = %e, "거절했던 기기 인증 실패"),
            }
        });
    }
}

pub(crate) fn on_connected(node: &Arc<Node>, peer: PeerId) {
    {
        let mut inner = node.peers.inner.lock();
        inner.connected.insert(peer);
        inner.gone.remove(&peer);
    }
    let known = node
        .store
        .get_peer(&peer.to_bytes())
        .ok()
        .flatten()
        .is_some_and(|r| r.user.is_some());
    if node.me.read().is_none() {
        return;
    }
    // 내가 보낸 친구 요청 중 아직 전하지 못한 것(링크 주인은 신원을 모르는 노드라 아래에서 끝남).
    if friends::has_pending_redeem(node, &peer) {
        node.spawn_detached(move |node| async move {
            friends::send_pending_redeems(&node, peer).await;
        });
    }
    // 아는 기기가 예전에 쓰던 노드면 그 기기로 보고 인증합니다(예전 노드로 다시 뜬 예전 빌드 친구).
    let known = known
        || node
            .store
            .get_setting::<PrevPeer>(&prev_key(&peer))
            .ok()
            .flatten()
            .is_some();
    if !known {
        return;
    }
    node.spawn_detached(move |node| async move {
        // 친구 수락은 인증 전에도 받으므로 먼저 보냅니다(상대가 아직 나를 친구로 모를 수 있음).
        friends::send_pending_accepts(&node, peer).await;
        match ensure_auth(&node, peer).await {
            Ok(_) => sync_service::catch_up_peer(&node, peer),
            Err(e) => debug!(peer = %peer, error = %e, "연결 후 인증 실패"),
        }
    });
}

pub(crate) fn on_disconnected(node: &Arc<Node>, peer: PeerId) {
    let gone = {
        let mut inner = node.peers.inner.lock();
        inner.connected.remove(&peer);
        if inner.gone.len() >= GONE_MAX {
            inner.gone.clear();
        }
        inner.gone.insert(peer);
        inner.auth_locks.remove(&peer);
        inner.refused.remove(&peer);
        inner.authed.remove(&peer)
    };
    if let Some((user, device)) = gone {
        crate::presence::on_gone(node, user, device);
    }
    volatile::on_peer_disconnected(node, peer);
    voice::on_peer_disconnected(node, peer);
    crate::p2p::on_peer_disconnected(node, peer);
}

/// 주소로 연결하고, 성공하면 그 주소를 기록합니다.
pub(crate) async fn connect(node: &Node, peer: PeerId, addrs: Vec<Multiaddr>) -> Result<()> {
    node.net.dial(peer, addrs.clone()).await?;
    let mut all: Vec<String> = addrs.iter().map(|a| a.to_string()).collect();
    if let Ok(Some(rec)) = node.store.get_peer(&peer.to_bytes()) {
        for a in rec.addrs {
            if !all.contains(&a) {
                all.push(a);
            }
        }
    }
    node.store
        .record_peer_success(&peer.to_bytes(), &all, Timestamp::now())?;
    Ok(())
}

/// 재시작 때 최근에 연결했던 노드에 다시 연결합니다.
pub(crate) fn reconnect_known(node: &Arc<Node>) {
    let Ok(recent) = node.store.recent_peers(RECONNECT_LIMIT) else {
        return;
    };
    for rec in recent {
        let Ok(peer) = PeerId::from_bytes(&rec.peer_id) else {
            continue;
        };
        let addrs: Vec<Multiaddr> = rec.addrs.iter().filter_map(|a| a.parse().ok()).collect();
        if addrs.is_empty() || peer == node.peer_id() {
            continue;
        }
        // 친구·내 기기 노드는 인증(온라인 표시)까지 시작 연결로 셉니다. 저장소는 기기를 노드 하나에만 묶습니다.
        let close = rec.user.filter(|u| close_contact(node, u));
        if let Some(u) = close {
            *node.peers.inner.lock().startup.entry(u).or_default() += 1;
        }
        node.spawn_detached(move |node| async move {
            match connect(&node, peer, addrs).await {
                Ok(()) if close.is_some() => {
                    let _ = ensure_auth(&node, peer).await;
                }
                Ok(()) => {}
                Err(e) => debug!(peer = %peer, error = %e, "다시 연결 실패"),
            }
            if let Some(u) = close {
                {
                    let mut inner = node.peers.inner.lock();
                    if let Some(n) = inner.startup.get_mut(&u) {
                        *n -= 1;
                        if *n == 0 {
                            inner.startup.remove(&u);
                        }
                    }
                }
                node.notify
                    .publish(Notification::NetStatus(crate::api::system::net_status_for(
                        &node,
                        &node.net.last_status(),
                    )));
            }
        });
    }
}

// ───────────── 링크용 주소 주고받기 ─────────────

/// 내 링크용 주소(릴레이 경유·onion, 자기 IP 없음).
pub(crate) fn my_contact_addrs(node: &Node) -> Vec<String> {
    node.net
        .last_status()
        .shareable_addrs()
        .iter()
        .take(MAX_CONTACT_ADDRS)
        .map(|a| a.to_string())
        .collect()
}

/// `Social(Contact)`로 받은 주소 중 걸어도 되는 것: 링크에 넣는 것과 같은 규칙으로 릴레이 경유 주소(회로 뒤에는 그
/// 피어의 `/p2p`만)와 onion 주소 하나짜리만. 평문 IP 주소를 받으면 인증된 피어가 내 노드를 내부망 아무 곳에나 걸게
/// 할 수 있으므로 버립니다.
pub(crate) fn contact_addrs_from(peer: &PeerId, raw: &[String]) -> Vec<Multiaddr> {
    let mut out: Vec<Multiaddr> = Vec::new();
    for s in raw.iter().take(MAX_CONTACT_ADDRS) {
        if s.len() > 512 {
            continue;
        }
        let a = if let Ok(a) = s.parse::<Multiaddr>()
            && addr::is_link_safe_relay_addr(&a)
        {
            a
        } else if let Some(a) = crate::tor::parse_onion_contact(s) {
            a
        } else {
            continue;
        };
        if addr::split_peer_id(&a).1.is_some_and(|p| p != *peer) || out.contains(&a) {
            continue;
        }
        out.push(a);
    }
    out
}

/// 노드 기록의 주소 중 남에게 알려도 되는 링크용 주소(받을 때와 같은 규칙, [`MAX_CONTACT_ADDRS`]개까지).
pub(crate) fn link_addrs_of(node: &Node, peer: &PeerId) -> Vec<String> {
    let raw = match node.store.get_peer(&peer.to_bytes()) {
        Ok(Some(rec)) => rec.addrs,
        _ => return Vec::new(),
    };
    let mut out: Vec<String> = Vec::new();
    for a in raw
        .iter()
        .flat_map(|s| contact_addrs_from(peer, std::slice::from_ref(s)))
    {
        let a = a.to_string();
        if !out.contains(&a) {
            out.push(a);
        }
    }
    out.truncate(MAX_CONTACT_ADDRS);
    out
}

/// 인증된 피어가 알려 준 링크용 주소를 기록하고 돌려줍니다(다시 걸 때 씀). 여기서 걸지는 않습니다.
pub(crate) fn record_contact(node: &Node, peer: &PeerId, raw: &[String]) -> Vec<Multiaddr> {
    let fresh = contact_addrs_from(peer, raw);
    keep_addrs(node, peer, &fresh);
    fresh
}

/// 노드의 주소 기록 앞에 새 주소를 더합니다.
fn keep_addrs(node: &Node, peer: &PeerId, fresh: &[Multiaddr]) {
    if fresh.is_empty() {
        return;
    }
    let mut all: Vec<String> = fresh.iter().map(|a| a.to_string()).collect();
    if let Ok(Some(rec)) = node.store.get_peer(&peer.to_bytes()) {
        for a in rec.addrs {
            if !all.contains(&a) {
                all.push(a);
            }
        }
    }
    all.truncate(MAX_KEPT_ADDRS);
    if let Err(e) = node
        .store
        .record_peer_success(&peer.to_bytes(), &all, Timestamp::now())
    {
        debug!(error = %e, "알려 준 주소 기록 실패");
    }
}

/// 노드 카드(`NodeContact`)를 주고받는 상대인지: 내 다른 기기와 친구만(임시 연락처·크루 멤버·보관 노드에는 주지 않음).
pub(crate) fn close_contact(node: &Node, user: &UserId) -> bool {
    node.me().is_ok_and(|m| m.user() == *user)
        || node
            .store
            .get_friend(user)
            .ok()
            .flatten()
            .is_some_and(|f| f.is_friend())
}

/// 친구·내 기기가 알려 준 그 기기의 노드 카드를 기록합니다(지금 빌드는 하나, 예전 빌드는 두 모드의 노드를 보냄).
/// 지금 노드는 onion 주소를 더하고, 다른 노드는 그 기기가 예전에 쓰던 노드(`PrevPeer`)로 두고 onion 주소를 남겨,
/// 예전 빌드인 상대가 다른 노드로 다시 떠도 걸게 합니다. 다른 기기에 묶인 노드와 내 노드는 건드리지 않습니다.
pub(crate) fn record_node_contacts(
    node: &Node,
    peer: &PeerId,
    user: UserId,
    device: DeviceId,
    nodes: &[NodeContact],
) {
    for n in nodes.iter().take(MAX_NODE_CONTACTS) {
        let Ok(p) = PeerId::from_bytes(&n.peer_id) else {
            continue;
        };
        if p == node.peer_id() {
            continue;
        }
        if p != *peer {
            if let Ok(Some(rec)) = node.store.get_peer(&p.to_bytes())
                && rec.user.is_some()
                && (rec.user, rec.device) != (Some(user), Some(device))
            {
                continue;
            }
            let prev = PrevPeer { user, device };
            let _ = node.store.put_setting(&prev_key(&p), &prev);
        }
        let onion: Vec<Multiaddr> = n
            .onion
            .as_deref()
            .filter(|s| s.len() <= 512)
            .and_then(crate::tor::parse_onion_contact)
            .filter(|a| addr::split_peer_id(a).1.is_none_or(|x| x == p))
            .into_iter()
            .collect();
        keep_addrs(node, &p, &onion);
    }
}

/// 내 링크용 주소를 보내고 상대 것을 받아 기록합니다(친구·내 기기와는 노드 카드도). 옛 빌드는 이 요청을 풀지
/// 못해 오류로 답합니다.
pub(crate) async fn exchange_contact(node: &Arc<Node>, peer: PeerId) {
    let close = node
        .peers
        .identity_of(&peer)
        .filter(|(u, _)| close_contact(node, u));
    let nodes = match close {
        Some(_) => crate::tor::node_contacts(node).await,
        None => Vec::new(),
    };
    let req = YpRequest::Social(SocialRequest::Contact {
        addrs: my_contact_addrs(node),
        nodes,
    });
    match node.net.request(peer, req).await {
        Ok(YpResponse::Social(r)) => {
            record_contact(node, &peer, &r.addrs);
            if let Some((user, device)) = close {
                record_node_contacts(node, &peer, user, device, &r.nodes);
            }
        }
        Ok(_) => debug!(peer = %peer, "주소 교환에 다른 응답"),
        Err(e) => debug!(peer = %peer, error = %e, "주소 교환 실패"),
    }
}

/// 네트워크 상태가 바뀔 때(`NetEvent::Status`): 내 링크용 주소가 바뀌었으면 인증된 피어에게 다시 알립니다. 직접 연결
/// 주 스웜(`--direct`)은 릴레이 예약보다 인증이 먼저 끝나 그때는 알릴 주소가 없을 수 있어, 예약이 잡히면 여기서 알립니다.
pub(crate) fn on_status(node: &Arc<Node>, s: &NetStatus) {
    let addrs = s.shareable_addrs();
    {
        let mut inner = node.peers.inner.lock();
        let key = (s.peer_id, addrs.clone());
        if inner.announced.as_ref() == Some(&key) {
            return;
        }
        inner.announced = Some(key);
    }
    if addrs.is_empty() {
        return;
    }
    for (peer, user, device) in node.peers.authed_peers() {
        if crate::archive::archive_only(node, &user, &device) {
            continue;
        }
        node.spawn_detached(move |node| async move {
            exchange_contact(&node, peer).await;
        });
    }
}

// ───────────── 끊긴 기기 다시 걸기 ─────────────

/// 끊긴 친구(1:1 공간)·내 다른 기기·작은 크루의 멤버 기기에 `redial_interval`마다 다시 겁니다. 기기마다 간격을 두
/// 배씩(최대 15분) 늘리고 인증되면 처음으로 돌립니다. 큰 크루 멤버는 gossip 이웃([`crate::neighbors`], 후보가 모자라면
/// 멤버 노드 목록으로 채움)과 보낼 것이 있을 때(`sync_service::push_to`) 찾습니다. 보내지 못한 친구 요청도 여기서 다시
/// 보냅니다.
pub(crate) fn spawn_redial(node: &Arc<Node>) {
    let weak = Arc::downgrade(node);
    let interval = node.cfg.redial_interval;
    node.tasks.spawn(async move {
        let mut tick = tokio::time::interval(interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tick.tick().await;
        loop {
            tick.tick().await;
            let Some(node) = weak.upgrade() else { break };
            if node.is_shutting_down() {
                break;
            }
            redial_tick(&node);
            crate::neighbors::tick(&node);
            crate::chantopics::spawn_poll_tips(&node);
            friends::retry_pending_redeems(&node);
        }
    });
}

/// 다시 걸 기기: 휴면이 아닌 1:1 공간의 상대 기기, 내 다른 기기, 작은 크루(기기마다 직접 보내는 크기,
/// `push_max_devices` 이하)의 멤버 기기. 크루 멤버는 위치 기록이 없는 Tor 기기라도 멤버 소개로 찾아 미리 이어 둬야
/// 보낼 것이 없어도 서로 온라인으로 보고, 둘을 이어 주던 멤버가 꺼져도 주고받습니다.
fn redial_targets(node: &Arc<Node>) -> HashMap<DeviceId, UserId> {
    let mut out = HashMap::new();
    let Ok(me) = node.me() else { return out };
    for space in node.space_ids() {
        if policy::is_dormant(node, &space) {
            continue;
        }
        let Some(st) = node.space_state(&space) else {
            continue;
        };
        if !st.is_member(&me.user()) || friends::space_deactivated(node, &space) {
            continue;
        }
        let all = st.kind == SpaceKind::Space
            || (st.kind == SpaceKind::Crew
                && node.sync.index.get(&space, &st).active_count() <= node.cfg.push_max_devices);
        for (&user, d) in st.active_devices() {
            let device = d.device;
            if device == me.device_id() || !(all || user == me.user()) {
                continue;
            }
            if crate::archive::archive_only(node, &user, &device) {
                continue;
            }
            out.insert(device, user);
        }
    }
    out
}

fn redial_tick(node: &Arc<Node>) {
    if node.net.is_closed() {
        return;
    }
    let status = node.net.last_status();
    let mut base = node.cfg.redial_interval;
    if status.mode == NetMode::Tor {
        base *= 2;
    }
    let targets = redial_targets(node);
    let now = Instant::now();
    let mut due = Vec::new();
    {
        let mut inner = node.peers.inner.lock();
        let online: HashSet<DeviceId> = inner.authed.values().map(|(_, d)| *d).collect();
        inner
            .redial
            .retain(|d, _| targets.contains_key(d) && !online.contains(d));
        for (device, user) in &targets {
            if online.contains(device) || due.len() >= MAX_REDIALS_PER_TICK {
                continue;
            }
            let e = inner.redial.entry(*device).or_insert((now, base));
            if e.0 > now {
                continue;
            }
            let wait = e.1.max(base);
            *e = (now + wait, (wait * 2).min(MAX_REDIAL_BACKOFF.max(base)));
            due.push((*user, *device));
        }
    }
    for (user, device) in due {
        node.spawn_detached(move |node| async move {
            redial_device(&node, user, device).await;
        });
    }
}

/// 기기 하나에 다시 겁니다: 지금 묶인 노드, 그 기기가 예전에 쓰던 노드([`PrevPeer`]), 그래도 안 되면 위치 기록.
async fn redial_device(node: &Arc<Node>, user: UserId, device: DeviceId) {
    let mut candidates: Vec<PeerId> = Vec::new();
    if let Ok(Some(rec)) = node.store.peer_for_device(&device)
        && let Ok(p) = PeerId::from_bytes(&rec.peer_id)
    {
        candidates.push(p);
    }
    if let Ok(rows) = node.store.settings_with_prefix(PREV_PEER_PREFIX) {
        for (key, value) in rows {
            let Ok(prev) = serde_json::from_value::<PrevPeer>(value) else {
                continue;
            };
            if prev.device == device
                && let Ok(p) = key[PREV_PEER_PREFIX.len()..].parse::<PeerId>()
                && !candidates.contains(&p)
            {
                candidates.push(p);
            }
        }
    }
    for peer in candidates {
        if peer == node.peer_id() || node.peers.authed_peer_of_device(&device).is_some() {
            return;
        }
        if node.peers.is_connected(&peer) {
            // 연결은 있는데 인증되지 않은 노드(연결될 때는 신원을 몰랐음): 연결 이벤트가 다시 오지 않으므로 여기서 인증합니다.
            match ensure_auth(node, peer).await {
                Ok(_) => {
                    sync_service::catch_up_peer(node, peer);
                    return;
                }
                Err(e) => debug!(peer = %peer, error = %e, "다시 건 노드 인증 실패"),
            }
            continue;
        }
        let addrs: Vec<Multiaddr> = node
            .store
            .get_peer(&peer.to_bytes())
            .ok()
            .flatten()
            .map(|r| r.addrs.iter().filter_map(|a| a.parse().ok()).collect())
            .unwrap_or_default();
        if addrs.is_empty() {
            continue;
        }
        // 연결되면 `on_connected`가 인증·따라잡기를 합니다(예전 노드도 아는 노드로 봄).
        match connect(node, peer, addrs).await {
            Ok(()) => return,
            Err(e) => debug!(peer = %peer, error = %e, "다시 걸기 실패"),
        }
    }
    if node.peers.authed_peer_of_device(&device).is_some() {
        return;
    }
    if let Some(peer) = reach_found(node, &user, &device).await {
        sync_service::catch_up_peer(node, peer);
    }
}

/// 위치 기록·멤버 소개(`locator::lookup`)로 찾은 노드에 걸고 인증해, 찾던 기기일 때만 그 노드를 돌려줍니다. 찾은 새
/// PeerId는 저장소가 모르는 노드라 연결 이벤트가 인증하지 않고, 멤버 소개는 서명이 없어 인증으로 기기를 확인합니다.
pub(crate) async fn reach_found(
    node: &Arc<Node>,
    user: &UserId,
    device: &DeviceId,
) -> Option<PeerId> {
    let (peer, addrs) = crate::locator::lookup(node, user, device).await?;
    reach(node, peer, addrs, user, device).await
}

/// 찾았거나 소개받은 노드에 걸고 인증해, 그 기기일 때만 노드를 돌려줍니다. 다른 기기였으면 그 소개를 지우고, 나를
/// 거절했으면 놓친 가입 확인을 청합니다(확인되면 상대가 걸어 와 인증).
pub(crate) async fn reach(
    node: &Arc<Node>,
    peer: PeerId,
    addrs: Vec<Multiaddr>,
    user: &UserId,
    device: &DeviceId,
) -> Option<PeerId> {
    if !node.peers.is_connected(&peer)
        && let Err(e) = connect(node, peer, addrs).await
    {
        debug!(peer = %peer, error = %e, "찾은 주소로 연결 실패");
        return None;
    }
    match ensure_auth(node, peer).await {
        Ok((_, d)) if d == *device => Some(peer),
        Ok(_) => {
            debug!(peer = %peer, "찾은 노드가 다른 기기");
            crate::locator::forget_intro(node, device);
            None
        }
        Err(e) => {
            debug!(peer = %peer, error = %e, "찾은 노드 인증 실패");
            ask_check_join(node, peer, user, device).await;
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_probes_are_limited_per_device_and_in_total() {
        let dir = PeerDirectory::default();
        let every = Duration::from_secs(600);
        let dev = |i: u8| DeviceId::from_bytes([i; 32]);
        assert!(dir.join_probe_due(dev(0), every));
        assert!(
            !dir.join_probe_due(dev(0), every),
            "같은 기기를 곧바로 다시"
        );
        for i in 1..MAX_JOIN_PROBES as u8 {
            assert!(dir.join_probe_due(dev(i), every));
        }
        assert!(!dir.join_probe_due(dev(200), every), "전체 상한을 넘음");
        // 간격이 지나면 다시 됩니다.
        assert!(dir.join_probe_due(dev(0), Duration::ZERO));
    }

    #[test]
    fn refused_peers_are_capped_and_taken_once() {
        let dir = PeerDirectory::default();
        let user = UserId([1; 32]);
        let dev = |i: u8| DeviceId::from_bytes([i; 32]);
        let peers: Vec<PeerId> = (0..=MAX_REFUSED).map(|_| PeerId::random()).collect();
        for (i, p) in peers.iter().enumerate() {
            dir.note_refused(*p, user, dev(i as u8));
        }
        assert_eq!(dir.inner.lock().refused.len(), MAX_REFUSED, "상한을 넘음");
        assert_eq!(dir.take_refused(|_, d| *d == dev(0)), vec![peers[0]]);
        assert!(
            dir.take_refused(|_, d| *d == dev(0)).is_empty(),
            "두 번 꺼냄"
        );
        // 다른 길로 인증되면 지웁니다.
        dir.set_authed(peers[1], user, dev(1));
        assert!(dir.take_refused(|_, d| *d == dev(1)).is_empty());
    }

    #[test]
    fn contact_addrs_keep_only_link_safe() {
        let me = PeerId::random();
        let other = PeerId::random();
        let relay = PeerId::random();
        let circuit = format!("/ip4/1.2.3.4/tcp/4001/p2p/{relay}/p2p-circuit/p2p/{me}");
        let bare_circuit = format!("/ip4/1.2.3.4/tcp/4001/p2p/{relay}/p2p-circuit");
        let onion_host = format!("{}.onion", "a".repeat(56));
        let raw = vec![
            circuit.clone(),
            bare_circuit.clone(),
            format!("{onion_host}:4001"),
            // 평문 IP, 다른 노드로 끝나는 회로, 회로 뒤 IP, onion 뒤 평문 주소, 형식 오류는 버림.
            "/ip4/192.168.0.2/tcp/1".into(),
            format!("/ip4/1.2.3.4/tcp/4001/p2p/{relay}/p2p-circuit/p2p/{other}"),
            format!("/ip4/1.2.3.4/tcp/4001/p2p/{relay}/p2p-circuit/ip4/10.0.0.1/tcp/22"),
            format!("/onion3/{}:4001/ip4/10.0.0.1/tcp/22", "a".repeat(56)),
            "아무거나".into(),
            circuit.clone(),
        ];
        let got: Vec<String> = contact_addrs_from(&me, &raw)
            .iter()
            .map(|a| a.to_string())
            .collect();
        assert_eq!(got.len(), 3, "{got:?}");
        assert_eq!(got[0], circuit);
        assert_eq!(got[1], bare_circuit);
        assert!(got[2].starts_with("/onion3/"));
        // 앞의 8개만 봄.
        let many: Vec<String> = (0..20)
            .map(|_| circuit.clone())
            .chain([bare_circuit])
            .collect();
        assert_eq!(contact_addrs_from(&me, &many).len(), 1);
    }
}
