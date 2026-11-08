//! 새 기기 연결. 새 기기는 살아 있는 내 기기가 그 자리에서 띄운 링크(QR)로만 들어옵니다.
//!
//! 1. 내 기기가 토큰을 RAM에 만들어 링크를 띄웁니다([`create`]).
//! 2. 신원이 없는 새 기기가 기기 키를 만들어 그 노드에 요청합니다([`join`], `YpRequest::DeviceLink`, 인증 전). 링크의 직접
//!    주소와 Tor를 함께 걸어 먼저 닿는 길을 씁니다(같은 망이면 Tor를 기다리지 않음).
//! 3. 양쪽 화면에 같은 6자리 번호가 뜨고, 사람이 내 기기에서 승인합니다([`decide`]).
//! 4. 내 기기가 새 기기를 목록에 더해 공동 서명해 알린 뒤, 시드·목록·프로필을 새 기기의 상자 키로 봉인합니다.
//! 5. 새 기기는 승인을 물어 보다가 받으면 신원을 설치하고, 내 기기로 인증해 공간을 받습니다.
//!
//! 시드는 파일이나 문구로 기기 밖에 나가지 않습니다. 토큰은 한 요청에만 쓰이고 비교할 번호가 다르면 사람이 거절합니다.

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::future::Either;
use parking_lot::Mutex;
use yangpa_core::body::Secret32;
use yangpa_core::device::DeviceEntry;
use yangpa_core::ids::{DeviceId, LinkToken, UserId};
use yangpa_core::links::{ContactInfo, DeviceLink, DirectContact, MAX_DIRECT_ADDRS};
use yangpa_core::time::Timestamp;
use yangpa_core::wire::{
    DeviceLinkGrant, DeviceLinkRequest, WireErrorCode, YpRequest, YpResponse, device_link_code,
};
use yangpa_crypto::{DeviceKeys, Identity, keysched};
use yangpa_net::{Multiaddr, NetHandle, PeerId};
use yangpa_rpc::RpcError;
use yangpa_rpc::api::serde_util::SecretString;
use yangpa_rpc::api::{DeviceLinkOffer, DeviceLinkRequestView, DeviceLinkStatus, Notification};
use zeroize::Zeroizing;

use crate::error::{Error, Result};
use crate::node::Node;
use crate::{friends, identity, p2p, peers, sync_service};

/// 화면에 그대로 뜨는 오류. `RpcError` 문구는 앱 번역표의 키가 됩니다.
type ShownResult<T> = std::result::Result<T, RpcError>;

/// 링크가 사는 시간. 사람이 두 기기를 들고 번호를 비교할 만큼만 둡니다.
const LINK_TTL: Duration = Duration::from_secs(10 * 60);
/// 승인을 묻는 간격.
const POLL_EVERY: Duration = Duration::from_secs(1);
/// 기기 이름 상한(남의 화면에 뜨므로 긴 글로 채우지 못하게).
pub(crate) const MAX_NAME: usize = 100;
/// 승인 뒤 쓰던 기기에 Tor로 인증을 거는 횟수와 그 사이 기다림(직접으로 연결했으면 Tor는 이때 처음 쓰일 수 있음).
const FIRST_AUTH_TRIES: usize = 5;
const FIRST_AUTH_GAP: Duration = Duration::from_secs(5);

#[derive(Default)]
pub(crate) struct DeviceLinkState {
    /// 내가 띄운 링크(한 번에 하나).
    offer: Mutex<Option<Offer>>,
    /// 승인했고 새 기기가 받아 가기를 기다리는 묶음. 답이 끊겨도 다시 물으면 같은 것을 주고, 새 링크를 띄워도 남습니다.
    granted: Mutex<Vec<Granted>>,
    /// 이 새 기기가 기다리는 연결의 토큰. 비면 그만둔 것입니다.
    joining: Mutex<Option<LinkToken>>,
}

struct Offer {
    token: LinkToken,
    until: Instant,
    request: Option<Request>,
}

struct Request {
    peer: PeerId,
    entry: DeviceEntry,
    nonce: [u8; 16],
    code: String,
    contested: bool,
    answer: Answer,
}

enum Answer {
    Waiting,
    Rejected,
    /// 승인을 눌러 내 목록에 넣는 중(이때 링크를 끝내면 목록에 든 기기가 묶음을 못 받습니다).
    Approving,
}

struct Granted {
    token: LinkToken,
    peer: PeerId,
    until: Instant,
    /// 봉인한 [`DeviceLinkGrant`].
    sealed: Vec<u8>,
}

/// 인증 전 상대가 보낸 토큰을 비교합니다.
fn same_token(a: &LinkToken, b: &LinkToken) -> bool {
    keysched::ct_eq(&a.0, &b.0)
}

fn grant_ad(user: &UserId, device: &DeviceId) -> Vec<u8> {
    let mut ad = b"yangpa/v3/device-link-grant".to_vec();
    ad.extend_from_slice(user.as_bytes());
    ad.extend_from_slice(device.as_bytes());
    ad
}

// ───────────── 내 기기 쪽 ─────────────

/// 새 링크를 띄웁니다(앞서 띄운 링크는 끝남).
pub(crate) async fn create(node: &Arc<Node>) -> ShownResult<DeviceLinkOffer> {
    let me = node.me()?;
    if me.devices.list.is_revoked(&me.device_id()) {
        return Err(RpcError::forbidden("이 기기는 이미 폐기됐습니다"));
    }
    let mut link = DeviceLink::new(me.user(), friends::my_contact(node).await, None);
    {
        let mut offer = node.devlink.offer.lock();
        if approving(&offer) {
            return Err(RpcError::conflict("새 기기를 승인하는 중입니다"));
        }
        *offer = Some(Offer {
            token: link.token,
            until: Instant::now() + LINK_TTL,
            request: None,
        });
    }
    // 링크가 끝나면 이 일로 띄운 직접 스웜을 내립니다.
    node.spawn_detached(|node| async move {
        tokio::time::sleep(LINK_TTL).await;
        p2p::reconcile(&node);
    });
    link.direct = p2p::link_contact(node).await;
    let url = link.to_url().map_err(Error::from)?;
    let qr = crate::qr::encode(&url)?;
    Ok(DeviceLinkOffer {
        url: SecretString::new(url),
        qr,
        expires_at: Timestamp(
            Timestamp::now()
                .0
                .saturating_add(LINK_TTL.as_millis() as u64),
        ),
    })
}

/// 직접 스웜을 띄워 둘지: 링크를 띄웠거나 찍어서 연결을 기다리는 동안.
pub(crate) fn wants_direct(node: &Node) -> bool {
    node.devlink.joining.lock().is_some()
        || node
            .devlink
            .offer
            .lock()
            .as_ref()
            .is_some_and(|o| o.until > Instant::now())
}

fn approving(offer: &Option<Offer>) -> bool {
    offer
        .as_ref()
        .and_then(|o| o.request.as_ref())
        .is_some_and(|r| matches!(r.answer, Answer::Approving))
}

fn view(r: &Request) -> DeviceLinkRequestView {
    DeviceLinkRequestView {
        name: r.entry.name.clone(),
        code: r.code.clone(),
        contested: r.contested,
    }
}

/// 승인을 기다리는 요청.
pub(crate) fn pending(node: &Node) -> Option<DeviceLinkRequestView> {
    let offer = node.devlink.offer.lock();
    let o = offer.as_ref().filter(|o| o.until > Instant::now())?;
    o.request
        .as_ref()
        .filter(|r| matches!(r.answer, Answer::Waiting))
        .map(view)
}

/// 띄운 링크를 끝냅니다.
pub(crate) fn cancel(node: &Arc<Node>) {
    {
        let mut offer = node.devlink.offer.lock();
        if !approving(&offer) {
            *offer = None;
        }
    }
    p2p::reconcile(node);
}

/// 시험용: 기한을 지난 것으로 만듭니다.
pub(crate) fn expire(node: &Node) {
    if let Some(o) = node.devlink.offer.lock().as_mut() {
        o.until = Instant::now();
    }
}

/// 사람의 판단. 승인이면 새 기기를 내 목록에 넣은 뒤 봉인한 묶음을 답으로 둡니다. 링크는 여기서 끝납니다.
pub(crate) async fn decide(node: &Arc<Node>, approve: bool) -> ShownResult<()> {
    let (token, peer, entry) = {
        let mut offer = node.devlink.offer.lock();
        let o = offer
            .as_mut()
            .filter(|o| o.until > Instant::now())
            .ok_or_else(no_request)?;
        let token = o.token;
        let r = o
            .request
            .as_mut()
            .filter(|r| matches!(r.answer, Answer::Waiting))
            .ok_or_else(no_request)?;
        if !approve {
            r.answer = Answer::Rejected;
            return Ok(());
        }
        r.answer = Answer::Approving;
        (token, r.peer, r.entry.clone())
    };
    let sealed = seal_grant(node, entry);
    let mut offer = node.devlink.offer.lock();
    let sealed = match sealed {
        Ok(sealed) => sealed,
        Err(e) => {
            if let Some(r) = offer.as_mut().and_then(|o| o.request.as_mut()) {
                r.answer = Answer::Waiting;
            }
            return Err(e.into());
        }
    };
    *offer = None;
    drop(offer);
    let now = Instant::now();
    let mut granted = node.devlink.granted.lock();
    granted.retain(|g| g.until > now);
    // 새 기기가 답을 받아 갈 시간을 줍니다.
    granted.push(Granted {
        token,
        peer,
        until: now + LINK_TTL,
        sealed,
    });
    Ok(())
}

fn no_request() -> RpcError {
    RpcError::not_found("승인을 기다리는 기기가 없습니다")
}

/// 새 기기를 내 목록에 넣고 시드·목록·프로필을 그 기기의 상자 키로 봉인합니다.
fn seal_grant(node: &Arc<Node>, entry: DeviceEntry) -> Result<Vec<u8>> {
    let devices = identity::add_device(node, entry.clone())?;
    let me = node.me()?;
    let grant = DeviceLinkGrant {
        seed: Secret32(*me.identity.signing.seed()),
        devices,
        profile: me.profile.clone(),
    };
    let body = Zeroizing::new(
        postcard::to_stdvec(&grant).map_err(|_| Error::internal("연결 묶음 직렬화 실패"))?,
    );
    tracing::info!(device = %entry.device.short(), "새 기기 연결을 승인함");
    Ok(yangpa_crypto::seal_to(
        &[(entry.device, entry.box_key.clone())],
        &grant_ad(&me.user(), &entry.device),
        &body,
    ))
}

/// 들어온 연결 요청(인증 전). 토큰은 한 기기만 씁니다. 다른 기기가 또 쓰면 QR이 샌 것이니 사람에게 알립니다.
pub(crate) fn handle_request(node: &Node, peer: PeerId, req: DeviceLinkRequest) -> YpResponse {
    let entry = req.entry;
    if !entry.keys_match() || entry.revoked_at.is_some() || entry.name.chars().count() > MAX_NAME {
        return YpResponse::error(WireErrorCode::BadRequest, "기기 항목이 올바르지 않음");
    }
    let mut offer = node.devlink.offer.lock();
    let Some(o) = offer
        .as_mut()
        .filter(|o| same_token(&o.token, &req.token) && o.until > Instant::now())
    else {
        return YpResponse::error(WireErrorCode::Expired, "기기 연결 링크가 끝남");
    };
    if let Some(r) = o.request.as_mut() {
        if matches!(r.answer, Answer::Rejected) {
            return YpResponse::error(WireErrorCode::Expired, "기기 연결 링크가 끝남");
        }
        if r.entry.device == entry.device {
            return YpResponse::DeviceLinkOpened { nonce: r.nonce };
        }
        r.contested = true;
        tracing::warn!("기기 연결 링크를 다른 기기가 또 씀");
        node.notify
            .publish(Notification::DeviceLinkRequest(view(r)));
        return YpResponse::error(WireErrorCode::Forbidden, "다른 기기가 먼저 요청함");
    }
    let nonce: [u8; 16] = rand::random();
    let r = Request {
        peer,
        code: device_link_code(&o.token, &nonce, &entry),
        entry: DeviceEntry {
            added_at: Timestamp::now(),
            ..entry
        },
        nonce,
        contested: false,
        answer: Answer::Waiting,
    };
    node.notify
        .publish(Notification::DeviceLinkRequest(view(&r)));
    o.request = Some(r);
    YpResponse::DeviceLinkOpened { nonce }
}

/// 새 기기의 물음. 요청을 보낸 그 노드에게만 답합니다.
pub(crate) fn handle_poll(node: &Node, peer: PeerId, token: LinkToken) -> YpResponse {
    let now = Instant::now();
    if let Some(g) = node
        .devlink
        .granted
        .lock()
        .iter()
        .find(|g| same_token(&g.token, &token) && g.peer == peer && g.until > now)
    {
        return YpResponse::DeviceLinkGrant(g.sealed.clone());
    }
    let offer = node.devlink.offer.lock();
    let Some(r) = offer
        .as_ref()
        .filter(|o| same_token(&o.token, &token) && o.until > now)
        .and_then(|o| o.request.as_ref())
        .filter(|r| r.peer == peer)
    else {
        return YpResponse::error(WireErrorCode::Expired, "기기 연결 링크가 끝남");
    };
    match r.answer {
        Answer::Waiting | Answer::Approving => YpResponse::DeviceLinkOpened { nonce: r.nonce },
        Answer::Rejected => YpResponse::error(WireErrorCode::Forbidden, "거절됨"),
    }
}

// ───────────── 새 기기 쪽 ─────────────

/// 링크로 연결을 청합니다. 이 기기 화면에 띄울 번호를 돌려주고, 승인을 뒤에서 기다려 `device.link_result`로 알립니다.
pub(crate) async fn join(node: &Arc<Node>, url: &str) -> ShownResult<String> {
    if node.me.read().is_some() {
        return Err(RpcError::conflict("이미 신원이 있습니다"));
    }
    let link = DeviceLink::parse(url).map_err(|_| bad_link())?;
    let peer = PeerId::from_bytes(&link.contact.peer_id).map_err(|_| bad_link())?;
    let device = DeviceKeys::generate();
    let entry = identity::device_entry(node, &device, Timestamp::now());
    // 이 값이 있는 동안 직접 스웜이 뜹니다.
    *node.devlink.joining.lock() = Some(link.token);
    let opened = open(node, &link, peer, &entry).await;
    if *node.devlink.joining.lock() != Some(link.token) {
        return Err(unreachable_device());
    }
    let (route, nonce) = match opened {
        Ok(x) => x,
        Err(e) => {
            abort(node);
            return Err(e);
        }
    };
    let code = device_link_code(&link.token, &nonce, &entry);
    node.spawn_detached(move |node| async move {
        let status = wait_grant(&node, &link, &route, device).await;
        if let Some(status) = status {
            *node.devlink.joining.lock() = None;
            node.notify
                .publish(Notification::DeviceLinkResult { status });
        }
        p2p::reconcile(&node);
    });
    Ok(code)
}

/// 쓰던 기기에 닿아 연결을 청합니다.
async fn open(
    node: &Arc<Node>,
    link: &DeviceLink,
    peer: PeerId,
    entry: &DeviceEntry,
) -> ShownResult<(Route, [u8; 16])> {
    let route = reach(node, link, peer)
        .await
        .ok_or_else(unreachable_device)?;
    let req = YpRequest::DeviceLink(Box::new(DeviceLinkRequest {
        token: link.token,
        entry: entry.clone(),
    }));
    match route.net.request(route.peer, req).await {
        Ok(YpResponse::DeviceLinkOpened { nonce }) => Ok((route, nonce)),
        Ok(YpResponse::Error {
            code: WireErrorCode::Forbidden,
            ..
        }) => Err(RpcError::forbidden("다른 기기가 이 링크를 먼저 썼습니다")),
        Ok(YpResponse::Error { .. }) => Err(RpcError::forbidden(
            "기기 연결 링크가 끝났습니다. 쓰던 기기에서 QR을 새로 띄우세요",
        )),
        Ok(_) | Err(_) => Err(unreachable_device()),
    }
}

/// 새 기기가 쓰던 기기에 닿은 길. 연결 요청과 승인 묻기는 모두 이 길로 갑니다(쓰던 기기는 요청을 보낸 노드에게만 답함).
struct Route {
    net: NetHandle,
    peer: PeerId,
    /// 직접 주소(비었으면 Tor).
    addrs: Vec<Multiaddr>,
}

/// 직접 주소와 Tor를 함께 걸어 먼저 닿는 길을 돌려줍니다. 직접으로 닿아도 Tor는 이어서 겁니다: 승인 뒤 첫 인증이 Tor로
/// 가므로 번호를 비교하는 동안 미리 이어 둡니다.
async fn reach(node: &Arc<Node>, link: &DeviceLink, peer: PeerId) -> Option<Route> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let contact = link.contact.clone();
    node.spawn_detached(move |node| async move {
        let limit = friends::first_try_limit(&node);
        let reached = friends::reach_peer(&node, peer, &contact);
        if let Ok(Ok(found)) = tokio::time::timeout(limit, reached).await {
            let _ = tx.send(found);
        }
    });
    let tor = async {
        rx.await.ok().map(|peer| Route {
            net: node.net.clone(),
            peer,
            addrs: Vec::new(),
        })
    };
    let direct = async {
        match &link.direct {
            Some(contact) => reach_direct(node, contact).await,
            None => None,
        }
    };
    let (tor, direct) = (std::pin::pin!(tor), std::pin::pin!(direct));
    match futures::future::select(direct, tor).await {
        Either::Left((Some(route), _)) | Either::Right((Some(route), _)) => Some(route),
        Either::Left((None, tor)) => tor.await,
        Either::Right((None, direct)) => direct.await,
    }
}

async fn reach_direct(node: &Arc<Node>, contact: &DirectContact) -> Option<Route> {
    let peer = PeerId::from_bytes(&contact.peer_id).ok()?;
    let addrs: Vec<Multiaddr> = contact
        .addrs
        .iter()
        .take(MAX_DIRECT_ADDRS)
        .filter_map(|a| Multiaddr::try_from(a.to_vec()).ok())
        .filter(p2p::link_dialable)
        .collect();
    if addrs.is_empty() {
        return None;
    }
    let net = p2p::dial_link(node, peer, addrs.clone()).await?;
    Some(Route { net, peer, addrs })
}

fn bad_link() -> RpcError {
    RpcError::invalid_input("기기 연결 링크가 올바르지 않습니다")
}

fn unreachable_device() -> RpcError {
    RpcError::network("쓰던 기기에 닿지 못했습니다. 켜져 있는지 확인하고 다시 시도하세요")
}

/// 이 기기의 연결 요청을 그만둡니다.
pub(crate) fn abort(node: &Arc<Node>) {
    *node.devlink.joining.lock() = None;
    p2p::reconcile(node);
}

/// 승인을 물어 봅니다. 그만뒀으면 `None`.
async fn wait_grant(
    node: &Arc<Node>,
    link: &DeviceLink,
    route: &Route,
    device: DeviceKeys,
) -> Option<DeviceLinkStatus> {
    let deadline = Instant::now() + LINK_TTL;
    loop {
        tokio::time::sleep(POLL_EVERY).await;
        if *node.devlink.joining.lock() != Some(link.token) {
            return None;
        }
        if Instant::now() > deadline {
            return Some(DeviceLinkStatus::Expired);
        }
        let poll = YpRequest::DeviceLinkPoll { token: link.token };
        match route.net.request(route.peer, poll).await {
            Ok(YpResponse::DeviceLinkOpened { .. }) => {}
            Ok(YpResponse::DeviceLinkGrant(sealed)) => {
                return Some(match finish(node, link, device, &sealed) {
                    Ok(()) => DeviceLinkStatus::Approved,
                    Err(e) => {
                        tracing::warn!(error = %e, "받은 기기 연결 묶음을 쓰지 못함");
                        DeviceLinkStatus::Failed
                    }
                });
            }
            Ok(YpResponse::Error {
                code: WireErrorCode::Forbidden,
                ..
            }) => return Some(DeviceLinkStatus::Rejected),
            Ok(YpResponse::Error { .. }) => return Some(DeviceLinkStatus::Expired),
            Ok(_) => {}
            // 연결이 끊겼으면 다시 겁니다(Tor 회로는 기다리는 동안 끊길 수 있음).
            Err(e) => {
                tracing::debug!(error = %e, "기기 연결 승인 묻기 실패");
                if route.addrs.is_empty() {
                    let _ = friends::reach_peer(node, route.peer, &link.contact).await;
                } else {
                    let _ = route.net.dial(route.peer, route.addrs.clone()).await;
                }
            }
        }
    }
}

/// 받은 묶음을 풀어 확인하고 신원을 설치한 뒤, 승인한 기기와 인증해 첫 동기화를 시작합니다.
fn finish(node: &Arc<Node>, link: &DeviceLink, device: DeviceKeys, sealed: &[u8]) -> Result<()> {
    let id = device.device_id();
    let body = yangpa_crypto::open_sealed(&id, &device.boxkey, &grant_ad(&link.user, &id), sealed)?;
    let g: DeviceLinkGrant =
        postcard::from_bytes(&body).map_err(|_| Error::invalid("기기 연결 묶음 형식 오류"))?;
    let identity_keys = Identity::from_seed(&g.seed.0);
    let list = &g.devices.list;
    if identity_keys.user_id() != link.user
        || list.user != link.user
        || g.profile.user != link.user
        || !list.contains_active(&id)
    {
        return Err(Error::invalid("기기 연결 묶음이 링크와 맞지 않음"));
    }
    identity::check_list_shape(&g.devices)?;
    g.devices.verify()?;
    g.profile.verify()?;
    identity::install_linked(
        node,
        identity_keys,
        device,
        g.devices.clone(),
        g.profile.clone(),
    )?;
    crate::p2p::reconcile(node);
    identity::spawn_first_sync(node);
    let (peer, contact) = (
        PeerId::from_bytes(&link.contact.peer_id),
        link.contact.clone(),
    );
    if let Ok(peer) = peer {
        node.spawn_detached(move |node| async move { first_auth(&node, peer, &contact).await });
    }
    Ok(())
}

/// 승인한 기기와 Tor로 인증해 따라잡습니다. 연결은 신원이 없을 때 이어졌으므로 `on_connected`가 인증을 걸지 않았습니다.
/// 이 인증 뒤 안내(`P2pHello`)로 직접 잇고, 내용은 그 연결로 옵니다.
async fn first_auth(node: &Arc<Node>, peer: PeerId, contact: &ContactInfo) {
    for _ in 0..FIRST_AUTH_TRIES {
        let reached = friends::reach_peer(node, peer, contact);
        match tokio::time::timeout(friends::first_try_limit(node), reached).await {
            Ok(Ok(found)) => match peers::ensure_auth(node, found).await {
                Ok(_) => return sync_service::catch_up_peer(node, found),
                Err(e) => tracing::debug!(peer = %found, error = %e, "연결된 기기의 첫 인증 실패"),
            },
            Ok(Err(e)) => tracing::debug!(peer = %peer, error = %e, "연결된 기기에 닿지 못함"),
            Err(_) => tracing::debug!(peer = %peer, "연결된 기기에 닿는 시간 초과"),
        }
        tokio::time::sleep(FIRST_AUTH_GAP).await;
    }
}
