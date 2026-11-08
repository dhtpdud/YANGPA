//! 연락처 링크·임시 연락처·친구 추가, 1:1 공간 만들기(protocol.md "연락처 링크").
//!
//! 1. A가 연락처 링크를 만듭니다(토큰 원문은 링크에만, 저장소에는 해시). 링크에는 A의 PeerId와 onion 주소(직접 연결
//!    주 스웜이면 릴레이 경유 주소)가 실립니다.
//! 2. B가 링크를 쓰면 A 노드에 `RedeemFriendLink{token, auth, profile}`(인증 전에도 받음). 링크의
//!    PeerId로 닿지 않으면(예전 빌드가 만든 링크라 A의 PeerId가 바뀜) PeerId 없이 onion으로 걸어 답한 노드에게
//!    보냅니다. 닿지 않으면 B는 요청을 대기열에 두고 A와 연결될 때·백오프마다 다시 보냅니다(A는 같은 사람·같은 토큰을
//!    같은 요청으로 봄).
//! 3. A는 토큰·서명을 확인하고 사람의 수락 없이 1:1 공간을 만들어(첫 상태 키 봉인 → `SpaceCreate` → 첫 채널 → 첫
//!    크루 키 갱신: 직접 인코딩으로 B 기기까지 봉인) 공간 스트림 이벤트와 임시 연락처 조건(`ContactTerms`, 링크의 만료)을 `FriendAccepted`로
//!    B에게 보냅니다. 보내지 못하면 다음 연결 때 다시 보냅니다. 둘 다 서로를 임시 연락처로 둡니다.
//! 4. B는 자기가 보낸 요청의 토큰과 맞는지, `SpaceCreate`가 링크 주인의 것인지 확인하고 그 이벤트를 시작으로 고정합니다.
//! 5. 링크가 만료되면(친구가 되지 않았으면) 양쪽이 각자 만료된 연락처로 바꾸고 그 1:1 공간을 비활성화합니다(보내기·
//!    동기화·인증 안 함, 지난 대화는 읽기만). 링크를 폐기하면 그 링크의 임시 연락처를 바로 만료하고, 상대는 다음 인증
//!    때 `Expired` 거절로 알게 됩니다. 만료된 사람이 새 링크를 쓰면 같은 1:1 공간으로 다시 임시 연락처가 됩니다.
//! 6. 임시 연락처의 친구 추가(`friend.propose`)는 `Social(Propose)`로 보내고, 받은 쪽이 같은 요청을 보냈거나 수락하면
//!    둘 다 친구가 됩니다. 친구가 되면 노드 카드(PeerId와 그 노드의 onion 주소)를 주고받습니다.

use std::sync::{Arc, Mutex, MutexGuard};

use serde::{Deserialize, Serialize};
use tracing::debug;
use yangpa_core::body::{EventBody, SharedKey, state_key_id};
use yangpa_core::device::{SignedDeviceList, SignedProfile};
use yangpa_core::event::{EventKind, KeyRef, Lane, SignedEvent, StreamId};
use yangpa_core::ids::{DeviceId, EventId, LinkToken, SpaceId, UserId};
use yangpa_core::links::{ContactInfo, FriendLink, token_hash};
use yangpa_core::model::{Member, SpaceKind, SpaceSettings};
use yangpa_core::time::Timestamp;
use yangpa_core::wire::{
    ContactTerms, FriendRedeem, MAX_EVENTS_PER_MESSAGE, OwnContact, OwnRequest, PeerAuth,
    SocialReply, SocialRequest, WireErrorCode, YpRequest, YpResponse,
};
use yangpa_crypto::{decrypt_body, keysched, open_sealed_body};
use yangpa_net::{Multiaddr, PeerId};
use yangpa_rpc::api::Notification;
use yangpa_store::{
    ContactKind, FriendLinkRecord, FriendRecord, FriendRequestRecord, RedeemOutcome,
    RequestDirection,
};
use yangpa_sync::Resolver;
use yangpa_sync::reconcile::full_range;
use yangpa_sync::state::DecodedEvent;

use crate::error::{Error, Result};
use crate::identity::Me;
use crate::ingest::{self, Source};
use crate::node::Node;
use crate::spaces::{self, NewSpace};
use crate::{identity, peers, sync_service};

const MAX_AUTH_SKEW_MS: u64 = 5 * 60 * 1000;
const PENDING_ACCEPT_PREFIX: &str = "friend.pending_accept.";
const PENDING_REDEEM_PREFIX: &str = "friend.pending_redeem.";
const DM_OFFER_PREFIX: &str = "friend.dm_offer.";
/// 이 기간 안에 전하지 못한 친구 요청은 다시 보내지 않습니다(보낸 요청 기록은 남김). 그 전에 멈추려면 상대를 차단합니다.
const REDEEM_RETRY_FOR_MS: u64 = 7 * 24 * 60 * 60 * 1000;
/// 다시 보내기 간격의 상한(처음은 `redial_interval`, 두 배씩).
const REDEEM_RETRY_MAX_MS: u64 = 30 * 60 * 1000;
/// `friend.add`의 첫 시도 제한(앱의 호출 제한 180초보다 짧게). 넘으면 대기열에 두고 `pending`.
const FIRST_REDEEM_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const FIRST_REDEEM_TIMEOUT_TOR: std::time::Duration = std::time::Duration::from_secs(120);
/// 1:1 공간의 처음 채널 이름.
const FIRST_CHANNEL: &str = "일반";
/// 친구 추가 요청 표식(`friend_requests.token_hash`). 링크 토큰 해시는 이 값이 될 수 없습니다.
const PROPOSAL: [u8; 32] = [0; 32];
/// 보낸 친구 추가 요청이 상대에게 닿았음(`payload`).
const PROPOSAL_DELIVERED: &[u8] = &[1];
/// 받은 친구 추가 요청을 거절함(`payload`). 기록을 지우면 내 다른 기기가 다시 알려 되살아납니다.
const PROPOSAL_REJECTED: &[u8] = &[2];
/// 임시 연락처 상한. 만료 없는 링크를 공개해도 아무나 1:1 공간을 끝없이 만들지 못하게 합니다.
const MAX_TEMPORARY_CONTACTS: u64 = 256;
/// 임시 연락처 만료를 확인하는 간격.
const EXPIRY_CHECK: std::time::Duration = std::time::Duration::from_secs(5);

/// 보낸 요청에 남기는 것(수락 확인·재연결용).
#[derive(Serialize, Deserialize)]
struct OutgoingPayload {
    peer: Vec<u8>,
}

/// 받은 요청에 남기는 것(수락 때 공간을 만들 재료).
#[derive(Serialize, Deserialize)]
struct IncomingPayload {
    token: LinkToken,
    peer: Vec<u8>,
    auth: PeerAuth,
}

/// 아직 링크 주인에게 전하지 못한 친구 요청. 토큰 원문이 들어 있어 암호화된 저장소에만 두고 로그에 남기지 않습니다.
#[derive(Serialize, Deserialize)]
struct PendingRedeem {
    request_id: [u8; 16],
    user: UserId,
    contact: ContactInfo,
    token: LinkToken,
    expires_at: Option<Timestamp>,
    created_at: Timestamp,
    attempts: u32,
    next_at: Timestamp,
}

fn pending_redeem_key(request_id: &[u8; 16]) -> String {
    format!("{PENDING_REDEEM_PREFIX}{}", request_id_str(request_id))
}

/// 보내는 중인 친구 요청 표식(놓으면 풀림). 같은 요청을 겹쳐 보내면 링크 주인이 둘을 동시에 받아 한쪽에 "쓸 수 없는
/// 링크"(한 번 쓰는 링크)로 답하고, 그 답을 받은 쪽이 보낸 요청을 지워 수락을 받지 못합니다.
pub(crate) struct Sending {
    node: Arc<Node>,
    id: [u8; 16],
}

impl Sending {
    fn claim(node: &Arc<Node>, id: [u8; 16]) -> Option<Self> {
        // 표식은 쥐었을 때만 만듭니다(못 쥐었는데 만들면 그 Drop이 남이 쥔 표식을 지우고, 잠금 안이면 교착).
        // 못 쥐면 "다시 보내 달라"고 남깁니다: 보내는 도중 링크 주인과 연결되면 그 시도는 앞선 다이얼 실패로 끝날 수
        // 있는데(request-response는 같은 피어의 다이얼이 하나라도 실패하면 기다리던 요청을 실패시킴), 연결 때의
        // 다시 보내기가 여기서 건너뛰어지면 다음 백오프(1~2분)까지 늦습니다.
        let mut sending = node.friend_sending.lock();
        if let Some(again) = sending.get_mut(&id) {
            *again = true;
            return None;
        }
        sending.insert(id, false);
        Some(Sending {
            node: node.clone(),
            id,
        })
    }
}

impl Drop for Sending {
    fn drop(&mut self) {
        let again = self.node.friend_sending.lock().remove(&self.id) == Some(true);
        // 닿았거나 거절됐으면 대기열 기록이 없어 다시 보내기는 곧 끝납니다.
        if again {
            let id = self.id;
            self.node.spawn_detached(move |node| async move {
                resend_redeem(&node, id).await;
            });
        }
    }
}

/// 보내는 중인 친구 수락 표식(1:1 공간별, 놓으면 풀림). 같은 수락을 겹쳐 보내면(연결 때·요청을 다시 받았을 때·
/// 처음 보낼 때가 겹침) 받는 쪽이 둘을 동시에 처리하다가 먼저 끝난 쪽이 그 공간을 겹친 공간으로 지운 뒤 늦은 쪽이
/// 거절하고, 보낸 쪽은 그 거절을 받고 수락을 되돌립니다. 그사이 다시 보내려 했으면 놓을 때 다시 봅니다(전해졌으면
/// 대기열 기록이 없어 곧 끝남).
struct AcceptSending {
    node: Arc<Node>,
    space: SpaceId,
    peer: PeerId,
}

impl AcceptSending {
    fn claim(node: &Arc<Node>, space: SpaceId, peer: PeerId) -> Option<Self> {
        let mut sending = node.accept_sending.lock();
        if let Some(again) = sending.get_mut(&space) {
            *again = true;
            return None;
        }
        sending.insert(space, false);
        Some(AcceptSending {
            node: node.clone(),
            space,
            peer,
        })
    }
}

impl Drop for AcceptSending {
    fn drop(&mut self) {
        let again = self.node.accept_sending.lock().remove(&self.space) == Some(true);
        if again {
            let peer = self.peer;
            self.node.spawn_detached(move |node| async move {
                send_pending_accepts(&node, peer).await;
            });
        }
    }
}

/// 보내지 못한 수락.
#[derive(Serialize, Deserialize)]
struct PendingAccept {
    peer: Vec<u8>,
    token: LinkToken,
    user: UserId,
    space: SpaceId,
    /// 임시 연락처 조건(연락처 링크). 예전 기록은 없음(친구).
    #[serde(default)]
    contact: Option<ContactTerms>,
    /// 원래 있던 1:1 공간으로 다시 임시 연락처가 된 것(상대가 받지 않으면 공간은 두고 만료로 되돌림).
    #[serde(default)]
    reactivated: bool,
}

/// 1:1을 크루로 바꾼 뒤 내가 새로 만들어 상대가 아직 받지 않은 1:1 공간(사용자마다 하나, `friend.open`).
#[derive(Serialize, Deserialize)]
struct DmOffer {
    space: SpaceId,
    /// 상대가 대신 쓴다고 답한 1:1 공간(내 다른 기기가 먼저 만든 것). 알게 되면 그쪽으로 옮깁니다.
    #[serde(default)]
    theirs: Option<SpaceId>,
}

/// 링크·요청 ID 표기(base58).
pub(crate) fn token_id(hash: &[u8; 32]) -> String {
    bs58::encode(hash).into_string()
}

fn parse_token_id(s: &str) -> Result<[u8; 32]> {
    bs58::decode(s.trim())
        .into_vec()
        .ok()
        .and_then(|v| v.try_into().ok())
        .ok_or_else(|| Error::invalid("token_id 형식이 잘못됨"))
}

pub(crate) fn request_id_str(id: &[u8; 16]) -> String {
    bs58::encode(id).into_string()
}

pub(crate) fn parse_request_id(s: &str) -> Result<[u8; 16]> {
    bs58::decode(s.trim())
        .into_vec()
        .ok()
        .and_then(|v| v.try_into().ok())
        .ok_or_else(|| Error::invalid("request_id 형식이 잘못됨"))
}

// ───────────── 링크 ─────────────

pub(crate) struct CreatedLink {
    pub url: String,
    pub token_id: String,
    pub expires_at: Option<Timestamp>,
}

/// 링크에 실을 내 연락처. 자기 IP는 넣지 않습니다(onion만, 직접 연결 주 스웜이면 릴레이 경유 주소만). onion은 Tor
/// 부트스트랩 전에도 키에서 나오므로 바로 넣습니다.
pub(crate) async fn my_contact(node: &Node) -> ContactInfo {
    let status = node.net.status().await;
    ContactInfo {
        peer_id: node.peer_id().to_bytes(),
        relay_addrs: status
            .relay_addrs
            .iter()
            .map(|a| a.to_string())
            .filter(|a| a.contains("/p2p-circuit"))
            .collect(),
        onion: crate::tor::device_onion(node).await,
    }
}

pub(crate) async fn create_link(
    node: &Arc<Node>,
    expires_in_secs: Option<u64>,
    single_use: bool,
) -> Result<CreatedLink> {
    let me = node.me()?;
    let now = Timestamp::now();
    let expires_at =
        expires_in_secs.map(|s| Timestamp(now.0.saturating_add(s.saturating_mul(1000))));
    let contact = my_contact(node).await;
    let mut link = FriendLink::new(me.user(), contact, expires_at, single_use);
    link.name_hint = me.profile.profile.name.chars().take(32).collect();
    let url = link.to_url()?;
    let hash = token_hash(&link.token);
    node.store.create_friend_link(&FriendLinkRecord {
        token_hash: hash,
        created_at: now,
        expires_at,
        single_use,
        uses: 0,
        revoked: false,
    })?;
    Ok(CreatedLink {
        url,
        token_id: token_id(&hash),
        expires_at,
    })
}

/// 링크를 폐기하고 그 링크로 생긴 임시 연락처를 바로 만료합니다.
pub(crate) fn revoke_link(node: &Node, id: &str) -> Result<()> {
    let hash = parse_token_id(id)?;
    if !node.store.revoke_friend_link(&hash)? {
        return Err(Error::not_found("없는 링크"));
    }
    for user in node.store.expire_link_contacts(&hash)? {
        on_contact_expired(node, &user, true);
    }
    Ok(())
}

// ───────────── 요청 보내기(B) ─────────────

pub(crate) enum AddOutcome {
    Pending(UserId),
    AlreadyFriend(UserId),
}

/// 링크 주인 노드에 닿을 수 있게 하고 보낼 노드를 돌려줍니다. 연결돼 있지 않으면 릴레이 경유·onion 주소와 전에
/// 연결했던 주소, 없으면 DHT로 겁니다. 그래도 닿지 않으면 PeerId 없이 onion으로 걸어 답한 노드를 씁니다(예전 빌드가
/// 만든 링크라 링크의 PeerId가 지금 노드와 다를 때. onion은 서비스 키가 곧 주소라 거기서 답한 노드가 링크 주인의
/// 기기입니다).
async fn reach(node: &Arc<Node>, peer: PeerId, contact: &ContactInfo) -> PeerId {
    if node.peers.is_connected(&peer) {
        return peer;
    }
    reach_known(node, peer, contact).await;
    if node.peers.is_connected(&peer) {
        return peer;
    }
    let Some(onion) = contact
        .onion
        .as_deref()
        .and_then(crate::tor::parse_onion_contact)
    else {
        return peer;
    };
    match node
        .net
        .dial_addr(yangpa_net::addr::split_peer_id(&onion).0)
        .await
    {
        Ok(found) if found != node.peer_id() => {
            if found != peer {
                debug!("링크 주인이 다른 노드로 onion에 답함(키를 바꾼 예전 빌드)");
            }
            found
        }
        Ok(_) => peer,
        Err(e) => {
            debug!(error = %e, "링크 주인의 onion에 연결 실패");
            peer
        }
    }
}

async fn reach_known(node: &Arc<Node>, peer: PeerId, contact: &ContactInfo) {
    let mut addrs: Vec<Multiaddr> = crate::tor::contact_addrs(contact);
    if let Ok(Some(rec)) = node.store.get_peer(&peer.to_bytes()) {
        for a in rec.addrs.iter().filter_map(|a| a.parse::<Multiaddr>().ok()) {
            if !addrs.contains(&a) {
                addrs.push(a);
            }
        }
    }
    if addrs.is_empty()
        && let Ok(found) = node.net.find_peer(peer).await
    {
        addrs = found;
    }
    if !addrs.is_empty()
        && let Err(e) = peers::connect(node, peer, addrs).await
    {
        debug!(error = %e, "링크 주인에게 연결 실패");
    }
}

pub(crate) fn first_try_limit(node: &Node) -> std::time::Duration {
    if crate::tor::tor_active(node) {
        FIRST_REDEEM_TIMEOUT_TOR
    } else {
        FIRST_REDEEM_TIMEOUT
    }
}

/// 연락처의 노드에 닿게 하고 그 노드를 돌려줍니다. 닿지 못하면 오류(기기 연결).
pub(crate) async fn reach_peer(
    node: &Arc<Node>,
    peer: PeerId,
    contact: &ContactInfo,
) -> Result<PeerId> {
    let found = reach(node, peer, contact).await;
    // 걸기 결과가 `PeerConnected`보다 먼저 와 `node.peers`로는 아직 모를 수 있습니다. 주소 없이 걸면 망 계층이
    // 연결돼 있을 때 바로 성공합니다.
    node.net.dial(found, Vec::new()).await?;
    Ok(found)
}

pub(crate) async fn add(node: &Arc<Node>, url: &str) -> Result<AddOutcome> {
    let link = FriendLink::parse(url).map_err(|e| Error::invalid(format!("링크: {e}")))?;
    let me = node.me()?;
    if link.user == me.user() {
        return Err(Error::invalid("내 링크입니다"));
    }
    // 임시·만료된 연락처면 새 링크로 다시 임시 연락처가 됩니다(같은 1:1 공간). 이 링크로 이미 임시 연락처가 됐으면
    // 다시 보내지 않습니다.
    let hash = token_hash(&link.token);
    if node.store.get_friend(&link.user)?.is_some_and(|f| {
        f.space.is_some()
            && (f.is_friend() || (f.kind == ContactKind::Temporary && f.link_hash == Some(hash)))
    }) {
        return Ok(AddOutcome::AlreadyFriend(link.user));
    }
    let peer = PeerId::from_bytes(&link.contact.peer_id)
        .map_err(|_| Error::invalid("링크의 노드 ID가 잘못됨"))?;

    // 같은 링크로 다시 보내면(응답을 잃음) 보낸 요청 기록 하나를 다시 씁니다(수락이 오면 하나만 남지 않게).
    let request_id: [u8; 16] = node
        .store
        .outgoing_request_by_token(&hash)?
        .map(|r| r.request_id)
        .unwrap_or_else(rand::random);
    // 대기열에 넣기 전에 쥐어, 첫 시도가 끝나기 전에 연결·주기의 다시 보내기가 같은 요청을 보내지 않게 합니다.
    let _sending = Sending::claim(node, request_id);
    node.store.put_friend_request(&FriendRequestRecord {
        request_id,
        direction: RequestDirection::Outgoing,
        user: link.user,
        profile: None,
        token_hash: hash,
        payload: postcard::to_stdvec(&OutgoingPayload {
            peer: peer.to_bytes(),
        })?,
        received_at: Timestamp::now(),
    })?;
    let now = Timestamp::now();
    let pending = PendingRedeem {
        request_id,
        user: link.user,
        contact: link.contact.clone(),
        token: link.token,
        expires_at: link.expires_at,
        created_at: now,
        attempts: 0,
        next_at: now,
    };
    node.store
        .put_setting(&pending_redeem_key(&request_id), &pending)?;
    match tokio::time::timeout(
        first_try_limit(node),
        deliver_redeem(node, peer, pending, true),
    )
    .await
    {
        Ok(Delivery::Refused(e)) => Err(e),
        // 닿았거나, 닿지 않아 대기열에 남김(요청은 닿았는데 응답만 잃었을 수도 있음. 보낸 요청을 지우면 상대가 수락해도
        // 받지 못해 한쪽만 친구가 되므로 남겨 둡니다).
        Ok(Delivery::Delivered | Delivery::Later) | Err(_) => Ok(AddOutcome::Pending(link.user)),
    }
}

enum Delivery {
    Delivered,
    /// 닿지 않음(다음 연결·백오프 때 다시).
    Later,
    /// 상대가 받지 않음(대기열을 지우고, 첫 시도면 보낸 요청도 지움).
    Refused(Error),
}

/// 대기열의 친구 요청 하나를 보냅니다(부르는 쪽이 [`Sending`]을 쥠). 인증은 매번 새로 서명합니다(시각 허용 오차가
/// 5분이고, 예전 빌드에서 업그레이드하면 내 PeerId도 바뀜). 결과에 따라 대기열 기록을 지우거나 다음 시도 시각을
/// 미룹니다.
async fn deliver_redeem(
    node: &Arc<Node>,
    peer: PeerId,
    mut p: PendingRedeem,
    first: bool,
) -> Delivery {
    let key = pending_redeem_key(&p.request_id);
    // 보내기 전에 다음 시도 시각을 미뤄 둡니다(Tor 연결은 주기보다 오래 걸릴 수 있어, 그사이 같은 요청을 또 보내지 않게).
    // 그사이 취소(`friend.reject`)·전달됐으면 되살리지 않습니다.
    p.attempts = p.attempts.saturating_add(1);
    let start = (node.cfg.redial_interval.as_millis() as u64).max(1);
    let wait = start
        .saturating_mul(1u64 << (p.attempts - 1).min(10))
        .min(REDEEM_RETRY_MAX_MS);
    p.next_at = Timestamp(Timestamp::now().0.saturating_add(wait));
    if node
        .store
        .get_setting::<serde_json::Value>(&key)
        .ok()
        .flatten()
        .is_some()
    {
        let _ = node.store.put_setting(&key, &p);
    }
    let peer = reach(node, peer, &p.contact).await;
    if peer.to_bytes() != p.contact.peer_id {
        // 링크 주인의 지금 노드로 다시 보내게 기억합니다(연결될 때 다시 보내기도 이 노드로).
        p.contact.peer_id = peer.to_bytes();
        if node
            .store
            .get_setting::<serde_json::Value>(&key)
            .ok()
            .flatten()
            .is_some()
        {
            let _ = node.store.put_setting(&key, &p);
        }
    }
    let me = match node.me() {
        Ok(m) => m,
        Err(_) => return Delivery::Later,
    };
    let auth = PeerAuth::create(
        &me.device.signing,
        me.devices.clone(),
        &node.peer_id().to_bytes(),
        &peer.to_bytes(),
    );
    let req = YpRequest::RedeemFriendLink(Box::new(FriendRedeem {
        token: p.token,
        auth,
        profile: me.profile.clone(),
    }));
    // 다시 보낸 요청이 거절되면 대기열만 지웁니다. 앞선 시도가 닿고 응답만 잃었다면 그 거절은 링크 주인이 수락하며
    // 요청을 가져간 사이의 답일 수 있어, 보낸 요청을 지우면 뒤이어 오는 수락을 받지 못합니다.
    let refuse = |node: &Node, e: Error| {
        let _ = node.store.delete_setting(&key);
        if first {
            let _ = node.store.delete_friend_request(&p.request_id);
        }
        Delivery::Refused(e)
    };
    match node.net.request(peer, req).await {
        Ok(YpResponse::FriendPending) | Ok(YpResponse::Ok) => {
            // 보낸 요청 기록은 수락(`FriendAccepted`)을 확인할 때 씁니다.
            let _ = node.store.delete_setting(&key);
            debug!("친구 요청 전달");
            Delivery::Delivered
        }
        Ok(YpResponse::Error {
            code: code @ (WireErrorCode::InvalidToken | WireErrorCode::Expired),
            ..
        }) => {
            debug!(?code, "친구 요청 거절됨");
            refuse(node, Error::invalid("링크가 만료됐거나 폐기됐습니다"))
        }
        Ok(YpResponse::Error {
            code: WireErrorCode::BadRequest,
            message,
        }) => refuse(node, Error::invalid(format!("상대가 거절함: {message}"))),
        // 바쁨·저장소 오류·인증 실패(상대 노드에 아직 신원이 없거나 시각이 어긋남) 같은 답은 지나갈 수 있어 다시 보냅니다.
        Ok(YpResponse::Error { code, .. }) => {
            debug!(?code, "친구 요청에 오류 답(다시 보냄)");
            Delivery::Later
        }
        Ok(_) => {
            debug!("친구 요청에 예상하지 못한 답(다시 보냄)");
            Delivery::Later
        }
        Err(e) => {
            debug!(error = %e, "친구 요청 전달 실패(다시 보냄)");
            Delivery::Later
        }
    }
}

fn pending_redeems(node: &Node) -> Vec<(String, PendingRedeem)> {
    let Ok(rows) = node.store.settings_with_prefix(PENDING_REDEEM_PREFIX) else {
        return Vec::new();
    };
    rows.into_iter()
        .filter_map(|(k, v)| match serde_json::from_value::<PendingRedeem>(v) {
            Ok(p) => Some((k, p)),
            Err(_) => {
                let _ = node.store.delete_setting(&k);
                None
            }
        })
        .collect()
}

fn redeem_peer(p: &PendingRedeem) -> Option<PeerId> {
    PeerId::from_bytes(&p.contact.peer_id).ok()
}

/// 이 노드로 보낼 친구 요청이 대기열에 있는지(연결될 때 `peers::on_connected`).
pub(crate) fn has_pending_redeem(node: &Node, peer: &PeerId) -> bool {
    pending_redeems(node)
        .iter()
        .any(|(_, p)| redeem_peer(p) == Some(*peer))
}

/// 이 노드와 연결됐을 때: 그 노드로 갈 대기열의 친구 요청을 시각과 상관없이 보냅니다.
pub(crate) async fn send_pending_redeems(node: &Arc<Node>, peer: PeerId) {
    for (_, p) in pending_redeems(node) {
        if redeem_peer(&p) == Some(peer) {
            resend_redeem(node, p.request_id).await;
        }
    }
}

/// 대기열의 요청을 다시 보냅니다. 이미 보내는 중이면 건너뛰고(그 시도가 끝나면 다시 보냄), 쥔 뒤 다시 읽어 그사이
/// 전달·취소된 것은 보내지 않습니다.
async fn resend_redeem(node: &Arc<Node>, request_id: [u8; 16]) {
    let Some(_sending) = Sending::claim(node, request_id) else {
        return;
    };
    let Some(p) = node
        .store
        .get_setting::<PendingRedeem>(&pending_redeem_key(&request_id))
        .ok()
        .flatten()
    else {
        return;
    };
    let Some(peer) = redeem_peer(&p) else {
        return;
    };
    if still_wanted(node, &p) {
        let _ = deliver_redeem(node, peer, p, false).await;
    }
}

/// 시험용: 대기열의 친구 요청을 모두 보내는 중으로 붙잡습니다(`testkit::hold_friend_sends`).
pub(crate) fn hold_pending_redeems(node: &Arc<Node>) -> Vec<Sending> {
    pending_redeems(node)
        .into_iter()
        .filter_map(|(_, p)| Sending::claim(node, p.request_id))
        .collect()
}

/// 시험용: 보내는 중에 다시 보내 달라고 남은 요청이 있는지.
pub(crate) fn resend_wanted(node: &Node) -> bool {
    node.friend_sending.lock().values().any(|again| *again)
}

/// 아직 보낼 요청인지. 아니면 대기열 기록을 지웁니다(취소됨, 이미 친구, 링크 만료, 보관 기간 지남).
fn still_wanted(node: &Node, p: &PendingRedeem) -> bool {
    let now = Timestamp::now();
    let cancelled = !node
        .store
        .get_friend_request(&p.request_id)
        .ok()
        .flatten()
        .is_some_and(|r| r.direction == RequestDirection::Outgoing);
    let friend = node
        .store
        .get_friend(&p.user)
        .ok()
        .flatten()
        .is_some_and(|f| f.space.is_some() && f.is_friend());
    let expired = p.expires_at.is_some_and(|t| t.0 < now.0)
        || p.created_at.0.saturating_add(REDEEM_RETRY_FOR_MS) < now.0;
    if cancelled || friend || expired {
        let _ = node
            .store
            .delete_setting(&pending_redeem_key(&p.request_id));
        return false;
    }
    true
}

/// 시각이 된 대기열의 친구 요청을 다시 보냅니다(`peers::spawn_redial`의 주기).
pub(crate) fn retry_pending_redeems(node: &Arc<Node>) {
    let now = Timestamp::now();
    for (_, p) in pending_redeems(node) {
        if p.next_at.0 > now.0 || !still_wanted(node, &p) {
            continue;
        }
        if redeem_peer(&p).is_none() {
            let _ = node
                .store
                .delete_setting(&pending_redeem_key(&p.request_id));
            continue;
        }
        let request_id = p.request_id;
        node.spawn_detached(move |node| async move {
            resend_redeem(&node, request_id).await;
        });
    }
}

/// 친구가 되거나 친구가 끝나면 그 사람과의 남은 요청과 보낼 대기열 기록을 지웁니다(서로 링크를 쓴 경우 낡은 요청이
/// 남아 수락하면 "이미 친구"가 되지 않게). 내가 수락한 쪽(`accept_claimed`)은 보낸 요청을 남깁니다: 상대도 연결이
/// 끊긴 사이나 동시에 내 요청을 수락했으면, 그 수락(`FriendAccepted`)은 보낸 요청의 토큰으로만 맞춰집니다.
fn clear_requests_with(node: &Node, user: &UserId, keep_outgoing: bool) {
    let dirs: &[RequestDirection] = if keep_outgoing {
        &[RequestDirection::Incoming]
    } else {
        &[RequestDirection::Incoming, RequestDirection::Outgoing]
    };
    for dir in dirs.iter().copied() {
        if let Ok(rows) = node.store.list_friend_requests(dir) {
            for r in rows.into_iter().filter(|r| r.user == *user) {
                let _ = node.store.delete_friend_request(&r.request_id);
            }
        }
    }
    for (k, p) in pending_redeems(node) {
        if p.user == *user {
            let _ = node.store.delete_setting(&k);
        }
    }
}

// ───────────── 요청 받기(A) ─────────────

pub(crate) fn handle_redeem(node: &Arc<Node>, peer: PeerId, r: FriendRedeem) -> YpResponse {
    let bad = |m: &str| YpResponse::error(WireErrorCode::BadRequest, m);
    let Ok(me) = node.me() else {
        return YpResponse::error(WireErrorCode::Internal, "신원 없음");
    };
    if r.auth
        .verify(
            &peer.to_bytes(),
            &node.peer_id().to_bytes(),
            MAX_AUTH_SKEW_MS,
        )
        .is_err()
    {
        return YpResponse::error(WireErrorCode::Unauthenticated, "기기 증명이 올바르지 않음");
    }
    let user = r.auth.user;
    if r.profile.user != user || r.profile.verify().is_err() {
        return bad("프로필 서명이 올바르지 않음");
    }
    if user == me.user() {
        return bad("자기 자신");
    }
    let contact = node
        .store
        .get_friend(&user)
        .ok()
        .flatten()
        .filter(|f| f.space.is_some());
    // 이 링크로 이미 수락했고 아직 전하지 못했으면(상대가 응답을 잃고 다시 보냄) 요청을 그대로 두게 하고 수락을 다시
    // 보냅니다.
    if contact.is_some()
        && pending_accept_for(node, &user).is_some_and(|p| keysched::ct_eq(&p.token.0, &r.token.0))
    {
        let node2 = node.clone();
        node.spawn_detached(move |_| async move {
            send_pending_accepts(&node2, peer).await;
        });
        return YpResponse::FriendPending;
    }
    if contact.as_ref().is_some_and(|f| f.is_friend()) {
        return bad("이미 친구");
    }
    let hash = token_hash(&r.token);
    // 이 링크로 이미 임시 연락처가 됐거나 받아들이는 중(상대가 응답을 잃고 다시 보냄): 링크 사용 횟수를 다시 쓰지
    // 않습니다. 수락은 받아들이기를 마치면(또는 이미) 보냅니다.
    if contact
        .as_ref()
        .is_some_and(|f| f.kind == ContactKind::Temporary && f.link_hash == Some(hash))
        || node.contact_accepting.lock().contains(&(user, hash))
    {
        return YpResponse::FriendPending;
    }
    // 차단한 사용자의 요청은 드러내지 않고 버립니다(모더레이션).
    if crate::moderation::is_blocked(node, &user) {
        return crate::moderation::silent_friend_refusal(node, &user, &hash);
    }
    // 같은 사람이 같은 링크로 다시 보낸 요청(응답을 잃고 다시 시도)은 링크 사용 횟수를 다시 쓰지 않고 그 요청을 갱신합니다.
    let existing = node
        .store
        .list_friend_requests(RequestDirection::Incoming)
        .ok()
        .and_then(|v| {
            v.into_iter()
                .find(|x| x.user == user && x.token_hash == hash)
        });
    if existing.is_none() {
        if contact.is_none()
            && node.store.count_temporary_contacts().unwrap_or(u64::MAX) >= MAX_TEMPORARY_CONTACTS
        {
            return bad("상대의 임시 연락처가 가득 찼습니다");
        }
        match node.store.redeem_friend_link(&hash, Timestamp::now()) {
            Ok(RedeemOutcome::Accepted) => {}
            Ok(RedeemOutcome::Expired) => {
                return YpResponse::error(WireErrorCode::Expired, "만료된 링크");
            }
            Ok(_) => return YpResponse::error(WireErrorCode::InvalidToken, "쓸 수 없는 링크"),
            Err(_) => return YpResponse::error(WireErrorCode::Internal, "저장소 오류"),
        }
    }
    let request_id: [u8; 16] = existing
        .as_ref()
        .map(|x| x.request_id)
        .unwrap_or_else(rand::random);
    let payload = match postcard::to_stdvec(&IncomingPayload {
        token: r.token,
        peer: peer.to_bytes(),
        auth: r.auth.clone(),
    }) {
        Ok(p) => p,
        Err(_) => return YpResponse::error(WireErrorCode::Internal, "직렬화 실패"),
    };
    let stored = node.store.put_friend_request(&FriendRequestRecord {
        request_id,
        direction: RequestDirection::Incoming,
        user,
        profile: Some(r.profile.clone()),
        token_hash: hash,
        payload,
        received_at: Timestamp::now(),
    });
    if stored.is_err() {
        return YpResponse::error(WireErrorCode::Internal, "저장소 오류");
    }
    let _ = identity::accept_profile(node, &r.profile);
    let _ = identity::accept_device_list(node, &r.auth.devices);
    let _ = node
        .store
        .set_peer_identity(&peer.to_bytes(), &user, &r.auth.device);
    // 사람의 수락 없이 바로 임시 연락처로 받습니다(응답이 나간 뒤 공간을 만들어 `FriendAccepted`로 보냄).
    node.spawn_detached(move |node| async move {
        if let Err(e) = accept_contact(&node, request_id).await {
            debug!(error = %e, "연락처 링크 요청을 받지 못함");
        }
    });
    YpResponse::FriendPending
}

/// 연락처 링크로 온 요청을 임시 연락처로 받습니다. 처음이면 1:1 공간을 만들고, 만료된(또는 아직 임시인) 연락처면
/// 같은 공간으로 다시 임시 연락처가 됩니다. 실패하면 요청을 남겨 상대가 다시 보낼 때 다시 받습니다.
async fn accept_contact(node: &Arc<Node>, rid: [u8; 16]) -> Result<SpaceId> {
    let rec = node
        .store
        .get_friend_request(&rid)?
        .filter(|r| r.direction == RequestDirection::Incoming && r.token_hash != PROPOSAL)
        .ok_or_else(|| Error::not_found("없는 연락처 요청"))?;
    let p: IncomingPayload = postcard::from_bytes(&rec.payload)?;
    let terms = ContactTerms {
        expires_at: node
            .store
            .get_friend_link(&rec.token_hash)?
            .and_then(|l| l.expires_at),
    };
    let existing = node
        .store
        .get_friend(&rec.user)?
        .filter(|f| f.space.is_some());
    if existing.as_ref().is_some_and(|f| f.is_friend()) {
        let _ = node.store.delete_friend_request(&rid);
        return Err(Error::conflict("이미 친구입니다"));
    }
    // 같은 요청을 겹쳐 받아 1:1 공간이 둘 생기지 않게, 공간을 만들기 전에 요청을 가져갑니다.
    let key = (rec.user, rec.token_hash);
    node.contact_accepting.lock().insert(key);
    if !node.store.delete_friend_request(&rid)? {
        node.contact_accepting.lock().remove(&key);
        return Err(Error::not_found("이미 처리한 연락처 요청"));
    }
    let r = match existing {
        Some(f) if still_one_to_one(node, &f) => reactivate(node, &rec, p, f, terms),
        _ => accept_claimed(node, &rec, p, Some(terms)).await,
    };
    if r.is_err() {
        let _ = node.store.put_friend_request(&rec);
    }
    node.contact_accepting.lock().remove(&key);
    r
}

/// 기록의 1:1 공간에 아직 둘 다 멤버인지(상대가 나갔으면 새 공간을 만듦).
fn still_one_to_one(node: &Node, f: &FriendRecord) -> bool {
    let (Some(space), Ok(me)) = (f.space, node.me()) else {
        return false;
    };
    node.space_state(&space).is_some_and(|st| {
        st.kind == SpaceKind::Space && st.is_member(&me.user()) && st.is_member(&f.user)
    })
}

/// 만료된(또는 아직 임시인) 연락처가 새 링크를 씀: 같은 1:1 공간으로 다시 임시 연락처가 되고 새 조건을 상대에게
/// `FriendAccepted`로 다시 보냅니다.
fn reactivate(
    node: &Arc<Node>,
    rec: &FriendRequestRecord,
    p: IncomingPayload,
    f: FriendRecord,
    terms: ContactTerms,
) -> Result<SpaceId> {
    let space = f.space.ok_or_else(|| Error::internal("1:1 공간 없음"))?;
    node.store.put_friend(&FriendRecord {
        kind: ContactKind::Temporary,
        expires_at: terms.expires_at,
        link_hash: Some(rec.token_hash),
        ..f
    })?;
    clear_requests_with(node, &rec.user, true);
    sync_service::refresh_space_peers(node, &space);
    node.notify.publish(Notification::SpaceUpdated { space });
    node.notify
        .publish(Notification::FriendUpdated { user: rec.user });
    queue_accept(node, p.peer, p.token, rec.user, space, Some(terms), true)?;
    Ok(space)
}

/// 상대에게 보낼 수락을 남기고 바로 보내 봅니다(닿지 않으면 다음 연결 때).
fn queue_accept(
    node: &Arc<Node>,
    peer: Vec<u8>,
    token: LinkToken,
    user: UserId,
    space: SpaceId,
    contact: Option<ContactTerms>,
    reactivated: bool,
) -> Result<()> {
    let pending = PendingAccept {
        peer: peer.clone(),
        token,
        user,
        space,
        contact,
        reactivated,
    };
    node.store
        .put_setting(&format!("{PENDING_ACCEPT_PREFIX}{space}"), &pending)?;
    if let Ok(peer) = PeerId::from_bytes(&peer) {
        let node2 = node.clone();
        node.spawn_detached(move |_| async move {
            send_pending_accepts(&node2, peer).await;
        });
    }
    Ok(())
}

/// 이 사용자에게 아직 전하지 못한 수락.
fn pending_accept_for(node: &Node, user: &UserId) -> Option<PendingAccept> {
    node.store
        .settings_with_prefix(PENDING_ACCEPT_PREFIX)
        .ok()?
        .into_iter()
        .filter_map(|(_, v)| serde_json::from_value::<PendingAccept>(v).ok())
        .find(|p| p.user == *user)
}

/// 받은 요청을 수락해 1:1 공간을 만들고 상대에게 알립니다.
pub(crate) async fn accept(node: &Arc<Node>, request_id: &str) -> Result<SpaceId> {
    let rid = parse_request_id(request_id)?;
    let rec = node
        .store
        .get_friend_request(&rid)?
        .filter(|r| r.direction == RequestDirection::Incoming)
        .ok_or_else(|| Error::not_found("없는 친구 요청"))?;
    // 임시 연락처의 친구 추가 요청: 같은 요청을 보내면 둘 다 친구가 됩니다.
    if rec.token_hash == PROPOSAL {
        let space = node
            .store
            .get_friend(&rec.user)?
            .and_then(|f| f.space)
            .ok_or_else(|| Error::not_found("연락처가 아닙니다"))?;
        propose(node, &rec.user).await?;
        return Ok(space);
    }
    // 예전 친구 링크로 받아 둔 요청(이제는 링크 요청을 바로 받으므로 업그레이드 전 것만): 친구로 받습니다.
    let p: IncomingPayload = postcard::from_bytes(&rec.payload)?;
    if node
        .store
        .get_friend(&rec.user)?
        .is_some_and(|f| f.space.is_some() && f.is_friend())
    {
        return Err(Error::conflict("이미 친구입니다"));
    }
    // 같은 요청을 동시에(두 번 눌러) 수락해 1:1 공간이 둘 생기지 않게, 공간을 만들기 전에 요청을 가져갑니다.
    if !node.store.delete_friend_request(&rid)? {
        return Err(Error::not_found("이미 처리했거나 없는 친구 요청"));
    }
    match accept_claimed(node, &rec, p, None).await {
        Ok(space) => Ok(space),
        Err(e) => {
            // 공간을 만들지 못했으면 요청을 되돌려 다시 수락할 수 있게 합니다.
            let _ = node.store.put_friend_request(&rec);
            Err(e)
        }
    }
}

/// 1:1 공간을 만들어 상대를 친구(`contact`가 없을 때) 또는 임시 연락처로 둡니다.
async fn accept_claimed(
    node: &Arc<Node>,
    rec: &FriendRequestRecord,
    p: IncomingPayload,
    contact: Option<ContactTerms>,
) -> Result<SpaceId> {
    let me = node.me()?;
    let their_name = rec
        .profile
        .as_ref()
        .map(|p| p.profile.name.clone())
        .unwrap_or_else(|| rec.user.short());
    let spec = dm_spec(&me, rec.user, &their_name, p.auth.devices.clone());
    let (space, _evs, report) = spaces::create_space(node, spec).await?;
    ingest::after_unlock(node, space, &report);
    let settled = {
        let _g = contact_guard();
        let prev = node.store.get_friend(&rec.user).ok().flatten();
        let dup = duplicate_space(node, prev.as_ref(), &space);
        let keep = dup.map_or(space, |d| d.min(space));
        node.store
            .put_friend(&FriendRecord {
                kind: if contact.is_some() {
                    ContactKind::Temporary
                } else {
                    ContactKind::Friend
                },
                expires_at: contact.as_ref().and_then(|c| c.expires_at),
                link_hash: contact.as_ref().map(|_| rec.token_hash),
                verified: prev.as_ref().is_some_and(|f| f.verified),
                ..FriendRecord::friend(rec.user, Some(keep))
            })
            .map(|_| (keep, dup))
    };
    let (keep, dup) = match settled {
        Ok(x) => x,
        Err(e) => {
            let l = ingest::lock_space(node, space).await;
            let _ = spaces::discard_space_locked(node, &l, &space);
            return Err(e.into());
        }
    };
    clear_requests_with(node, &rec.user, true);
    sync_service::refresh_space_peers(node, &keep);
    node.notify.publish(Notification::FriendAccepted {
        user: rec.user,
        space: keep,
    });
    // 겹쳐 지울 공간이어도 수락은 보냅니다: 상대도 같은 규칙으로 고르고, 기다리던 보낸 요청을 정리합니다.
    queue_accept(node, p.peer, p.token, rec.user, space, contact, false)?;
    if let Some(d) = dup {
        discard_duplicate(node, if keep == d { space } else { d }).await;
    }
    Ok(keep)
}

/// 연락처 기록의 읽기-고르기-쓰기를 한 번에 하나만(받은 수락과 내가 만든 1:1 공간이 엇갈려 서로를 덮어쓰지 않게).
static CONTACT_WRITE: Mutex<()> = Mutex::new(());

fn contact_guard() -> MutexGuard<'static, ()> {
    CONTACT_WRITE.lock().unwrap_or_else(|e| e.into_inner())
}

/// 이 사용자와의 기록이 `new`가 아닌 살아 있는 1:1 공간을 가리키면 그 공간. 서로 상대 링크를 동시에 쓰면 양쪽이
/// 각자 1:1 공간을 만들고 상대 것도 받으므로, 둘 다 ID가 작은 쪽을 남기고 다른 하나를 지워 한 대화로 맞춥니다.
fn duplicate_space(node: &Node, prev: Option<&FriendRecord>, new: &SpaceId) -> Option<SpaceId> {
    prev.filter(|f| f.is_active() && still_one_to_one(node, f))
        .and_then(|f| f.space)
        .filter(|s| s != new)
}

/// 겹친 1:1 공간 하나를 이 기기에서 지웁니다. 내가 만들어 아직 상대에게 수락을 전하지 못한 공간이면, 전한 뒤에
/// 지웁니다([`send_pending_accepts`]).
async fn discard_duplicate(node: &Arc<Node>, space: SpaceId) {
    if node
        .store
        .get_setting::<serde_json::Value>(&format!("{PENDING_ACCEPT_PREFIX}{space}"))
        .ok()
        .flatten()
        .is_some()
    {
        return;
    }
    debug!(space = %space.short(), "겹친 1:1 공간 지움");
    let l = ingest::lock_space(node, space).await;
    if let Err(e) = spaces::discard_space_locked(node, &l, &space) {
        debug!(error = %e, "겹친 1:1 공간 정리 실패");
    }
    drop(l);
    node.notify.publish(Notification::SpaceUpdated { space });
}

/// 나와 `user` 둘만의 1:1 공간.
fn dm_spec(me: &Me, user: UserId, their_name: &str, devices: SignedDeviceList) -> NewSpace {
    NewSpace {
        kind: SpaceKind::Space,
        settings: SpaceSettings {
            name: format!("{} · {}", me.profile.profile.name, their_name)
                .chars()
                .take(100)
                .collect(),
            description: String::new(),
            icon: None,
            icon_key: None,
            allow_external_emoji: false,
            strict_rotation: false,
        },
        everyone: yangpa_sync::state::FRIEND_SPACE_EVERYONE,
        members: vec![member(me.user()), member(user)],
        devices: vec![me.devices.clone(), devices],
        first_channel: Some(FIRST_CHANNEL.into()),
    }
}

fn member(user: UserId) -> Member {
    Member {
        user,
        roles: Vec::new(),
        nickname: None,
        automation: false,
    }
}

pub(crate) fn reject(node: &Node, request_id: &str) -> Result<()> {
    let rid = parse_request_id(request_id)?;
    let _ = node.store.delete_setting(&pending_redeem_key(&rid));
    // 임시 연락처의 요청은 거절 표시로 남겨 내 다른 기기에 전합니다.
    if let Some(r) = node
        .store
        .get_friend_request(&rid)?
        .filter(|r| r.direction == RequestDirection::Incoming && r.token_hash == PROPOSAL)
    {
        if rejected(&r) {
            return Err(Error::not_found("없는 친구 요청"));
        }
        let user = r.user;
        node.store.put_friend_request(&FriendRequestRecord {
            payload: PROPOSAL_REJECTED.to_vec(),
            ..r
        })?;
        node.notify.publish(Notification::FriendUpdated { user });
        return Ok(());
    }
    if !node.store.delete_friend_request(&rid)? {
        return Err(Error::not_found("없는 친구 요청"));
    }
    Ok(())
}

/// 이 피어에게 보내지 못한 수락을 다시 보냅니다(연결될 때마다).
pub(crate) async fn send_pending_accepts(node: &Arc<Node>, peer: PeerId) {
    if node.accept_hold.load(std::sync::atomic::Ordering::Acquire) {
        return;
    }
    let Ok(rows) = node.store.settings_with_prefix(PENDING_ACCEPT_PREFIX) else {
        return;
    };
    let Ok(me) = node.me() else { return };
    for (key, value) in rows {
        let Ok(p) = serde_json::from_value::<PendingAccept>(value) else {
            let _ = node.store.delete_setting(&key);
            continue;
        };
        if p.peer != peer.to_bytes() {
            continue;
        }
        // 이미 보내는 중이면 건너뜁니다(그 시도가 끝나면 다시 봄). 쥔 뒤 다시 읽어 그사이 전해진 것은 보내지 않습니다.
        let Some(_sending) = AcceptSending::claim(node, p.space, peer) else {
            continue;
        };
        let Ok(Some(p)) = node.store.get_setting::<PendingAccept>(&key) else {
            continue;
        };
        let Some(req) = accept_request(node, &me, &p) else {
            continue;
        };
        match node.net.request(peer, req).await {
            Ok(YpResponse::Ok) => {
                let _ = node.store.delete_setting(&key);
                debug!(space = %p.space.short(), "친구 수락 전달");
                let superseded = node
                    .store
                    .get_friend(&p.user)
                    .ok()
                    .flatten()
                    .is_some_and(|f| f.is_active() && f.space.is_some_and(|s| s != p.space));
                if superseded {
                    // 겹친 공간 중 지우기로 한 쪽: 상대도 이제 알았으므로 지웁니다.
                    discard_duplicate(node, p.space).await;
                    continue;
                }
                // 이제 서로 친구라 인증하고 따라잡을 수 있습니다.
                if peers::ensure_auth(node, peer).await.is_ok() {
                    sync_service::spawn_catch_up(node, p.space, peer);
                }
            }
            Ok(YpResponse::Error { code, .. }) => {
                debug!(?code, "상대가 친구 수락을 거절함");
                if matches!(
                    code,
                    WireErrorCode::InvalidToken | WireErrorCode::BadRequest
                ) {
                    // 상대는 이 요청을 모르거나(취소·응답 유실 뒤 삭제) 받지 않으므로, 이쪽만 친구로 남지 않게 되돌립니다.
                    let _ = node.store.delete_setting(&key);
                    rollback_accept(node, &p).await;
                }
            }
            Ok(_) => {}
            Err(e) => debug!(error = %e, "친구 수락 전달 실패(다음 연결 때 다시)"),
        }
    }
}

/// 남은 수락으로 보낼 `FriendAccepted`. 받는 쪽 상한에 맞춥니다(시작 이벤트가 앞쪽에 있고, 나머지는 인증 뒤
/// 따라잡기로 받음).
fn accept_request(node: &Node, me: &Me, p: &PendingAccept) -> Option<YpRequest> {
    let events = node
        .store
        .stream_events(
            &p.space,
            StreamId::Space,
            full_range(u64::MAX - 1),
            MAX_EVENTS_PER_MESSAGE as u32,
        )
        .ok()?;
    Some(YpRequest::FriendAccepted {
        token: p.token,
        space: p.space,
        events,
        profile: Some(Box::new(me.profile.clone())),
        contact: p.contact.clone(),
    })
}

/// 시험용: 그 1:1 공간의 남은 수락으로 보낼 요청과 받을 피어(`testkit::pending_accept_request`).
pub(crate) fn pending_accept_request(node: &Node, space: &SpaceId) -> Option<(PeerId, YpRequest)> {
    let p: PendingAccept = node
        .store
        .get_setting(&format!("{PENDING_ACCEPT_PREFIX}{space}"))
        .ok()??;
    let me = node.me().ok()?;
    let peer = PeerId::from_bytes(&p.peer).ok()?;
    Some((peer, accept_request(node, &me, &p)?))
}

/// 시험용: 친구 수락 보내기를 붙잡거나(`true`) 풀어 남은 수락을 보냅니다.
pub(crate) fn hold_accepts(node: &Arc<Node>, hold: bool) {
    node.accept_hold
        .store(hold, std::sync::atomic::Ordering::Release);
    if hold {
        return;
    }
    let peers: std::collections::BTreeSet<Vec<u8>> = node
        .store
        .settings_with_prefix(PENDING_ACCEPT_PREFIX)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(_, v)| serde_json::from_value::<PendingAccept>(v).ok())
        .map(|p| p.peer)
        .collect();
    for peer in peers.iter().filter_map(|p| PeerId::from_bytes(p).ok()) {
        let node2 = node.clone();
        node.spawn_detached(move |_| async move {
            send_pending_accepts(&node2, peer).await;
        });
    }
}

/// 상대가 받지 않은 수락을 되돌립니다: 친구 기록과 1:1 공간을 지웁니다. 다시 임시 연락처가 된 것이면 지난 대화가
/// 있으므로 공간은 두고 만료된 연락처로 돌립니다.
async fn rollback_accept(node: &Arc<Node>, p: &PendingAccept) {
    // 기록을 읽고 바꾸는 사이 받은 수락(`handle_accepted`)이 기록을 다른 1:1 공간으로 옮겼으면 그 기록은 건드리지 않습니다:
    // 연락처 기록의 읽기-고르기-쓰기는 한 번에 하나만(`contact_guard`).
    if p.reactivated {
        let expired = {
            let _g = contact_guard();
            match node.store.get_friend(&p.user) {
                Ok(Some(f)) if f.space == Some(p.space) && f.kind == ContactKind::Temporary => node
                    .store
                    .set_contact_kind(&p.user, ContactKind::Expired, f.expires_at, f.link_hash)
                    .unwrap_or(false),
                _ => false,
            }
        };
        if expired {
            on_contact_expired(node, &p.user, false);
        }
        return;
    }
    {
        let _g = contact_guard();
        if node
            .store
            .get_friend(&p.user)
            .ok()
            .flatten()
            .is_some_and(|f| f.space == Some(p.space))
        {
            let _ = node.store.remove_friend(&p.user);
        }
    }
    let l = ingest::lock_space(node, p.space).await;
    if let Err(e) = spaces::discard_space_locked(node, &l, &p.space) {
        debug!(error = %e, "받지 않은 친구 공간 정리 실패");
    }
    drop(l);
    node.notify
        .publish(Notification::SpaceUpdated { space: p.space });
}

/// 받은 `FriendAccepted`의 내용.
pub(crate) struct Accepted {
    pub token: LinkToken,
    pub space: SpaceId,
    pub events: Vec<SignedEvent>,
    pub profile: Option<SignedProfile>,
    pub contact: Option<ContactTerms>,
}

/// 받은 `FriendAccepted`(B). 인증 전에도 받습니다. 조건(`contact`)이 있으면 임시 연락처, 없으면(예전 친구 링크) 친구로
/// 둡니다. 만료된 연락처가 새 링크를 쓴 것이면 같은 공간으로 다시 임시 연락처가 됩니다.
pub(crate) async fn handle_accepted(node: &Arc<Node>, peer: PeerId, a: Accepted) -> YpResponse {
    let Accepted {
        token,
        space,
        events,
        profile,
        contact,
    } = a;
    let hash = token_hash(&token);
    let Ok(me) = node.me() else {
        return YpResponse::error(WireErrorCode::Internal, "신원 없음");
    };
    let out = match node.store.outgoing_request_by_token(&hash) {
        Ok(Some(r)) => r,
        Ok(None) => {
            // 이미 처리한 수락의 재전송.
            let user = events
                .iter()
                .find(|e| e.header.kind == EventKind::SpaceCreate)
                .map(|e| e.header.user);
            return handled_resend(node, &me, user.as_ref(), &space, &events);
        }
        Err(_) => return YpResponse::error(WireErrorCode::Internal, "저장소 오류"),
    };
    let user = out.user;
    // 저장·런타임 같은 흔적을 남기기 전에, 링크 주인이 나와 둘만의 공간을 만든 것인지 먼저 확인합니다
    // (인증 전에 오므로 아무나 공간·보류 이벤트를 쌓지 못하게).
    let (genesis_id, author_device) = match precheck_accept(&me, &user, &space, &events) {
        Ok(x) => x,
        Err(why) => return YpResponse::error(WireErrorCode::BadRequest, why),
    };
    // 있던 공간인지 보는 것부터 적용 결과를 확인하기까지 그 공간 잠금을 쥡니다: 같은 수락이 겹쳐 오면 먼저 끝난 쪽이
    // 그 공간을 겹친 공간으로 지울 수 있는데, 그사이 늦은 쪽이 지워진 공간을 다시 만들거나 상태가 없다고 거절하면 보낸
    // 쪽이 수락을 되돌립니다.
    let l = ingest::lock_space(node, space).await;
    // 기다리는 사이 같은 수락을 먼저 처리했으면(요청을 지움) 재전송처럼 답합니다.
    if !matches!(node.store.outgoing_request_by_token(&hash), Ok(Some(_))) {
        drop(l);
        return handled_resend(node, &me, Some(&user), &space, &events);
    }
    let report = match ingest_dm_locked(node, &l, peer, &user, space, events, genesis_id) {
        Ok(r) => r,
        Err((code, why)) => return YpResponse::error(code, why),
    };
    drop(l);
    ingest::after_unlock(node, space, &report);
    if let Some(p) = &profile
        && p.user == user
    {
        let _ = identity::accept_profile(node, p);
    }
    let (prev, saved) = {
        let _g = contact_guard();
        let prev = node.store.get_friend(&user).ok().flatten();
        let dup = duplicate_space(node, prev.as_ref(), &space);
        let keep = dup.map_or(space, |d| d.min(space));
        let saved = node
            .store
            .put_friend(&FriendRecord {
                verified: prev.as_ref().is_some_and(|f| f.verified),
                kind: if contact.is_some() {
                    ContactKind::Temporary
                } else {
                    ContactKind::Friend
                },
                expires_at: contact.as_ref().and_then(|c| c.expires_at),
                link_hash: contact.as_ref().map(|_| hash),
                ..FriendRecord::friend(user, Some(keep))
            })
            .map(|_| (keep, dup));
        (prev, saved)
    };
    let Ok((keep, dup)) = saved else {
        return YpResponse::error(WireErrorCode::Internal, "저장소 오류");
    };
    if prev.is_some() {
        node.notify.publish(Notification::FriendUpdated { user });
    }
    let _ = node.store.delete_friend_request(&out.request_id);
    clear_requests_with(node, &user, false);
    let _ = node
        .store
        .set_peer_identity(&peer.to_bytes(), &user, &author_device);
    if let Some(d) = dup {
        discard_duplicate(node, if keep == d { space } else { d }).await;
    }
    let space = keep;
    sync_service::refresh_space_peers(node, &space);
    node.notify
        .publish(Notification::FriendAccepted { user, space });
    // 응답이 나간 뒤 인증하고 채널 스트림을 따라잡습니다. 연결은 친구가 되기 전에 맺어 노드 카드를 아직 주고받지
    // 않았으므로 지금 주고받습니다(못 만난 동안 다시 찾는 데 씀).
    node.spawn_detached(move |node| async move {
        if peers::ensure_auth(&node, peer).await.is_ok() {
            sync_service::spawn_catch_up(&node, space, peer);
            peers::exchange_contact(&node, peer).await;
        }
    });
    YpResponse::Ok
}

/// 쥔 그 공간 잠금 안에서 받은 1:1 공간의 시작 묶음(`precheck_accept`를 거친 것)을 적용하고, `user`가 소유한 나와 둘만의
/// 공간이 됐는지 봅니다. 아니면 이번에 새로 만든 흔적을 지우고(원래 있던 공간은 건드리지 않음) 답할 오류를 돌려줍니다.
fn ingest_dm_locked(
    node: &Arc<Node>,
    l: &ingest::Locked<'_>,
    peer: PeerId,
    user: &UserId,
    space: SpaceId,
    events: Vec<SignedEvent>,
    genesis_id: EventId,
) -> std::result::Result<ingest::IngestReport, (WireErrorCode, &'static str)> {
    let me = node
        .me()
        .map_err(|_| (WireErrorCode::Internal, "신원 없음"))?;
    let existed = node.spaces.read().contains_key(&space)
        || spaces::load_genesis(node, &space).ok().flatten().is_some();
    if let Some(existing) = spaces::load_genesis(node, &space).ok().flatten()
        && existing != genesis_id
    {
        return Err((WireErrorCode::BadRequest, "다른 시작 이벤트"));
    }
    if spaces::ensure_runtime(node, &space, Some(genesis_id)).is_err() {
        return Err((WireErrorCode::Internal, "공간을 만들 수 없음"));
    }
    let applied = ingest::ingest_locked(node, l, space, events, Source::Bootstrap(peer));
    let ok = applied.is_ok()
        && node.space_state(&space).is_some_and(|s| {
            s.kind == SpaceKind::Space
                && s.is_member(&me.user())
                && s.is_member(user)
                && s.owner == *user
        });
    match applied {
        Ok(r) if ok => Ok(r),
        applied => {
            if let Err(e) = &applied {
                debug!(error = %e, "친구 공간 적용 실패");
            }
            if !existed {
                let _ = spaces::discard_space_locked(node, l, &space);
            }
            Err(if applied.is_err() {
                (WireErrorCode::Internal, "적용 실패")
            } else {
                (WireErrorCode::BadRequest, "1:1 공간이 아님")
            })
        }
    }
}

/// 요청이 이미 없는 `FriendAccepted`(먼저 온 같은 수락을 처리했거나, 요청을 취소함)에 답합니다. 처리한 것이면 `Ok`:
/// 그 공간이 연락처 기록의 공간이거나, 겹친 1:1 공간이라 지운 것(기록이 그 사람과의 ID가 더 작은 다른 1:1 공간을
/// 가리킴, [`duplicate_space`]의 규칙)입니다. 뒤의 경우는 공간 주인이 만든 나와의 1:1 공간인지 확인한 뒤에만
/// 답합니다(인증 전이라 아무나 사용자 ID를 적어 그 사람과의 연락처 여부를 떠보지 못하게). 나머지는 모르는 요청입니다:
/// 보낸 쪽은 수락을 되돌립니다.
fn handled_resend(
    node: &Node,
    me: &Me,
    user: Option<&UserId>,
    space: &SpaceId,
    events: &[SignedEvent],
) -> YpResponse {
    let done = node.store.friend_by_space(space).ok().flatten().is_some()
        || user.is_some_and(|user| {
            node.store.get_friend(user).ok().flatten().is_some_and(|f| {
                f.is_active()
                    && still_one_to_one(node, &f)
                    && f.space.is_some_and(|keep| keep < *space)
            }) && precheck_accept(me, user, space, events).is_ok()
        });
    if done {
        YpResponse::Ok
    } else {
        YpResponse::error(WireErrorCode::InvalidToken, "모르는 요청")
    }
}

/// 받은 수락 묶음을 저장하기 전에 확인합니다. 공간 스트림 이벤트만, 상한 이하, 그리고 링크 주인이 만든 시작 이벤트가
/// 하나 있어야 합니다. 시작 이벤트는 나에게 봉인된 키 묶음의 상태 키로 열어 버리는 해소기에 넣어 보고(서명된 기기 목록
/// 확인 포함), 링크 주인이 소유한 나와 둘만의 1:1 공간인지 봅니다. 시작 이벤트 ID와 만든 기기를 돌려줍니다.
fn precheck_accept(
    me: &Me,
    user: &UserId,
    space: &SpaceId,
    events: &[SignedEvent],
) -> std::result::Result<(EventId, DeviceId), &'static str> {
    if events.is_empty() || events.len() > MAX_EVENTS_PER_MESSAGE {
        return Err("이벤트 수가 잘못됨");
    }
    if events
        .iter()
        .any(|e| e.header.space != *space || e.header.stream() != StreamId::Space)
    {
        return Err("공간 스트림이 아닌 이벤트");
    }
    let creates: Vec<&SignedEvent> = events
        .iter()
        .filter(|e| e.header.kind == EventKind::SpaceCreate)
        .collect();
    let [genesis] = creates.as_slice() else {
        return Err("공간 생성 이벤트가 하나가 아님");
    };
    if genesis.header.user != *user || genesis.verify().is_err() {
        return Err("링크 주인의 공간 생성 이벤트가 아님");
    }
    let KeyRef::State(kid) = genesis.header.key else {
        return Err("공간 생성 이벤트의 키 형식이 잘못됨");
    };
    let key = events
        .iter()
        .filter(|e| {
            e.header.kind == EventKind::KeyShare
                && e.header.key == KeyRef::Sealed
                && e.verify().is_ok()
        })
        .filter_map(|e| {
            open_sealed_body(
                &me.device_id(),
                &me.device.boxkey,
                &e.header,
                e.body.as_deref()?,
            )
            .ok()
        })
        .find_map(|b| match b {
            EventBody::KeyShare(ks) => ks.keys.into_iter().find_map(|k| match k {
                SharedKey::State { id, key } if id == kid && state_key_id(space, &key) == kid => {
                    Some(key)
                }
                _ => None,
            }),
            _ => None,
        })
        .ok_or("공간 생성 이벤트를 열 키가 없음")?;
    let body = genesis
        .body
        .as_deref()
        .ok_or("공간 생성 이벤트 본문 없음")
        .and_then(|b| {
            decrypt_body(&key, &genesis.header, b).map_err(|_| "공간 생성 이벤트 복호화 실패")
        })?;
    let mut resolver = Resolver::with_genesis(genesis.id());
    resolver.insert_events(vec![DecodedEvent {
        id: genesis.id(),
        header: genesis.header.clone(),
        body,
    }]);
    let st = resolver.state().ok_or("공간 생성 이벤트가 해소되지 않음")?;
    let ok = st.kind == SpaceKind::Space
        && st.owner == *user
        && st.members.len() == 2
        && st.is_member(&me.user())
        && st.is_member(user)
        && st.device_belongs(user, &genesis.header.author);
    if !ok {
        return Err("1:1 공간이 아님");
    }
    Ok((genesis.id(), genesis.header.author))
}

/// 연락처 링크를 쓴 사용자(그 사람의 `Auth`를 받아 줄지 판단). 친구 추가 요청은 세지 않습니다(임시 연락처가
/// 만료되면 믿지 않음).
pub(crate) fn has_outgoing_request(node: &Node, user: &UserId) -> bool {
    node.store
        .list_friend_requests(RequestDirection::Outgoing)
        .map(|v| {
            v.iter()
                .any(|r| r.user == *user && r.token_hash != PROPOSAL)
        })
        .unwrap_or(false)
}

// ───────────── 임시 연락처 만료 ─────────────

/// 임시 연락처의 만료를 주기적으로 확인합니다(양쪽이 각자 링크의 만료 시각으로).
pub(crate) fn spawn_contact_expiry(node: &Arc<Node>) {
    let weak = Arc::downgrade(node);
    node.tasks.spawn(async move {
        let mut tick = tokio::time::interval(EXPIRY_CHECK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let Some(node) = weak.upgrade() else { break };
            if node.is_shutting_down() {
                break;
            }
            expire_due(&node);
        }
    });
}

/// 만료 시각이 지난 임시 연락처를 만료합니다.
pub(crate) fn expire_due(node: &Node) {
    match node.store.expire_due_contacts(Timestamp::now()) {
        Ok(users) => {
            for u in users {
                on_contact_expired(node, &u, true);
            }
        }
        Err(e) => debug!(error = %e, "임시 연락처 만료 확인 실패"),
    }
}

/// 임시 연락처가 만료됨: 친구 추가 요청을 지우고, 다른 공간을 함께 쓰지 않으면 인증을 풀고, 1:1 공간을 비활성화합니다.
/// `tell`이면 인증을 풀기 전에 상대 기기에 알립니다(링크를 폐기하면 상대는 만료 시각만으로는 모름).
fn on_contact_expired(node: &Node, user: &UserId, tell: bool) {
    debug!("임시 연락처 만료");
    clear_proposals(node, user);
    if tell {
        let mut peers: Vec<PeerId> = node
            .peers
            .authed_peers()
            .into_iter()
            .filter(|(_, u, _)| u == user)
            .map(|(p, _, _)| p)
            .collect();
        // 내 쪽 인증이 아직 안 끝난 기기(링크를 쓴 직후)에도 알립니다. 상대는 나를 이미 인증했을 수 있어, 다음 인증
        // 거절로 알 때까지 오래 걸립니다(받는 쪽은 자기 인증만 봄).
        if let Ok(Some(f)) = node.store.get_friend(user)
            && let Some(st) = f.space.and_then(|s| node.space_state(&s))
        {
            for d in st.member_active_devices(user) {
                if let Some(p) = peers::peer_for_device(node, &d.device)
                    && !peers.contains(&p)
                {
                    peers.push(p);
                }
            }
        }
        let net = node.net.clone();
        node.tasks.spawn(async move {
            for peer in peers {
                let req = YpRequest::Social(SocialRequest::ContactExpired);
                if let Err(e) = net.request(peer, req).await {
                    debug!(error = %e, "연락처 만료를 알리지 못함(상대는 다음 인증 때 앎)");
                }
            }
        });
    }
    peers::drop_untrusted_user(node, user);
    if let Ok(Some(f)) = node.store.get_friend(user)
        && let Some(space) = f.space
    {
        sync_service::refresh_space_peers(node, &space);
        node.notify.publish(Notification::SpaceUpdated { space });
    }
    node.notify
        .publish(Notification::FriendUpdated { user: *user });
}

/// 이 노드의 사용자와의 연락처 기록.
pub(crate) fn peer_contact(node: &Node, peer: &PeerId) -> Option<FriendRecord> {
    let user = node.store.get_peer(&peer.to_bytes()).ok().flatten()?.user?;
    node.store.get_friend(&user).ok().flatten()
}

/// 상대가 내 인증을 `Expired`로 거절함(상대 쪽 링크가 만료·폐기됨): 이쪽 임시 연락처도 만료합니다. `sent`는 인증을
/// 보내기 전의 기록([`peer_contact`]): 그사이 새 링크로 다시 임시 연락처가 됐으면 늦게 처리한 거절이라 두고, 아직
/// 만료라면 다음 인증이 다시 알려 줍니다.
pub(crate) fn on_expired_by_peer(node: &Node, peer: &PeerId, sent: Option<FriendRecord>) {
    let expired = {
        let _g = contact_guard();
        match peer_contact(node, peer) {
            Some(f) if f.kind == ContactKind::Temporary && sent.as_ref() == Some(&f) => node
                .store
                .set_contact_kind(&f.user, ContactKind::Expired, f.expires_at, f.link_hash)
                .unwrap_or(false)
                .then_some(f.user),
            _ => None,
        }
    };
    if let Some(user) = expired {
        on_contact_expired(node, &user, false);
    }
}

/// 상대가 나와의 임시 연락처를 만료했다고 알림(`SocialRequest::ContactExpired`): 내 인증을 다시 보내 상대가 `Expired`로
/// 거절하면 이쪽도 만료합니다(`peers::reconfirm_auth`). 알림을 늦게 처리하면 그사이 새 링크로 다시 만난 연락처를 만료하므로
/// 상대의 지금 답으로 정합니다. 친구는 그대로 둡니다.
pub(crate) fn handle_contact_expired(node: &Arc<Node>, peer: PeerId, user: UserId) -> YpResponse {
    if node
        .store
        .get_friend(&user)
        .ok()
        .flatten()
        .is_some_and(|f| f.kind == ContactKind::Temporary)
    {
        node.spawn_detached(move |node| async move {
            peers::reconfirm_auth(&node, peer).await;
        });
    }
    YpResponse::Social(SocialReply::default())
}

/// 만료된 연락처인지(그 사람의 인증을 `Expired`로 거절).
pub(crate) fn is_expired_contact(node: &Node, user: &UserId) -> bool {
    node.store
        .get_friend(user)
        .ok()
        .flatten()
        .is_some_and(|f| f.kind == ContactKind::Expired)
}

/// 비활성화된 1:1 공간인지: 상대가 만료된 연락처인 둘만의 공간. 보내기·동기화·인증 근거로 쓰지 않고 지난 대화만
/// 읽습니다. 기록이 없는 공간(수락하며 만드는 중, 연결 중)은 비활성화하지 않습니다.
pub(crate) fn space_deactivated(node: &Node, space: &SpaceId) -> bool {
    let Some(st) = node.space_state(space) else {
        return false;
    };
    if st.kind != SpaceKind::Space {
        return false;
    }
    let Ok(me) = node.me() else { return false };
    if !st.is_member(&me.user()) {
        return false;
    }
    let Some(other) = st.members.keys().find(|u| **u != me.user()) else {
        return false;
    };
    node.store
        .get_friend(other)
        .ok()
        .flatten()
        .is_some_and(|f| !f.is_active() && f.space == Some(*space))
}

// ───────────── 친구 추가(임시 연락처 → 친구) ─────────────

pub(crate) enum ProposeOutcome {
    /// 상대의 수락을 기다림.
    Requested,
    Friends,
}

/// 친구 추가 요청의 확인-기록-친구로 올리기를 한 번에 하나만. 둘이 동시에 누르면 받은 요청 기록과 친구로 올리기가
/// 엇갈려 친구가 된 뒤에도 받은 요청이 남을 수 있었습니다(네트워크를 기다리는 동안에는 잡지 않음).
static PROPOSALS: Mutex<()> = Mutex::new(());

fn proposals_guard() -> MutexGuard<'static, ()> {
    PROPOSALS.lock().unwrap_or_else(|e| e.into_inner())
}

fn proposal(node: &Node, dir: RequestDirection, user: &UserId) -> Option<FriendRequestRecord> {
    node.store
        .list_friend_requests(dir)
        .ok()?
        .into_iter()
        .find(|r| r.user == *user && r.token_hash == PROPOSAL)
}

fn clear_proposals(node: &Node, user: &UserId) {
    for dir in [RequestDirection::Incoming, RequestDirection::Outgoing] {
        if let Some(r) = proposal(node, dir, user) {
            let _ = node.store.delete_friend_request(&r.request_id);
        }
    }
}

/// 보낸 친구 추가 요청이 있는지(`friend.list`의 `proposed`).
pub(crate) fn proposed(node: &Node, user: &UserId) -> bool {
    proposal(node, RequestDirection::Outgoing, user).is_some()
}

/// 거절해서 표시만 남은 받은 요청인지(`friend.requests`에 보이지 않음).
pub(crate) fn rejected(r: &FriendRequestRecord) -> bool {
    r.token_hash == PROPOSAL && r.payload == PROPOSAL_REJECTED
}

/// 그 사람이 보낸 친구 추가 요청의 상태(내 다른 기기에게).
pub(crate) fn own_request(node: &Node, user: &UserId) -> OwnRequest {
    match proposal(node, RequestDirection::Incoming, user) {
        None => OwnRequest::None,
        Some(r) if rejected(&r) => OwnRequest::Rejected,
        Some(_) => OwnRequest::Pending,
    }
}

fn incoming_proposal(node: &Node, user: UserId, payload: &[u8]) -> FriendRequestRecord {
    FriendRequestRecord {
        request_id: rand::random(),
        direction: RequestDirection::Incoming,
        user,
        profile: node.store.get_profile(&user).ok().flatten(),
        token_hash: PROPOSAL,
        payload: payload.to_vec(),
        received_at: Timestamp::now(),
    }
}

/// 받은 친구 추가 요청을 저장하고 앱에 알립니다. 저장하지 못하면 거짓.
fn receive_proposal(node: &Node, user: UserId) -> bool {
    let r = incoming_proposal(node, user, &[]);
    if node.store.put_friend_request(&r).is_err() {
        return false;
    }
    node.notify.publish(Notification::FriendRequest {
        request_id: request_id_str(&r.request_id),
        user,
        profile: r.profile.map(|p| p.profile).unwrap_or_default(),
    });
    true
}

/// `friend.propose`: 임시 연락처에게 친구 추가를 요청합니다. 상대가 먼저 요청했으면(또는 상대가 이미 나를 친구로
/// 보면) 바로 친구가 됩니다. 닿지 않으면 요청을 남겨 다음 인증 때 보냅니다.
pub(crate) async fn propose(node: &Arc<Node>, user: &UserId) -> Result<ProposeOutcome> {
    let f = node
        .store
        .get_friend(user)?
        .ok_or_else(|| Error::not_found("연락처가 아닙니다"))?;
    match f.kind {
        ContactKind::Friend => return Ok(ProposeOutcome::Friends),
        ContactKind::Expired => return Err(Error::invalid("만료된 연락처입니다")),
        ContactKind::Temporary => {}
    }
    let accepted = {
        let _g = proposals_guard();
        let accepted = proposal(node, RequestDirection::Incoming, user).is_some();
        if proposal(node, RequestDirection::Outgoing, user).is_none() {
            node.store.put_friend_request(&FriendRequestRecord {
                request_id: rand::random(),
                direction: RequestDirection::Outgoing,
                user: *user,
                profile: None,
                token_hash: PROPOSAL,
                payload: Vec::new(),
                received_at: Timestamp::now(),
            })?;
        }
        if accepted {
            // 상대가 먼저 요청함: 이쪽은 바로 친구.
            become_friend(node, user, false);
        }
        accepted
    };
    if accepted {
        // 상대에게도 알려 친구로 만듭니다(닿지 않으면 다음 인증 때).
        let node2 = node.clone();
        let user = *user;
        node.spawn_detached(move |_| async move {
            deliver_proposal(&node2, &user).await;
        });
        return Ok(ProposeOutcome::Friends);
    }
    Ok(match deliver_proposal(node, user).await {
        Some(true) => ProposeOutcome::Friends,
        _ => ProposeOutcome::Requested,
    })
}

/// 보낸 친구 추가 요청을 그 사용자의 인증된 기기에 보냅니다. 닿으면 답의 `friends`를 돌려줍니다.
async fn deliver_proposal(node: &Arc<Node>, user: &UserId) -> Option<bool> {
    let peers: Vec<PeerId> = node
        .peers
        .authed_peers()
        .into_iter()
        .filter(|(_, u, _)| u == user)
        .map(|(p, _, _)| p)
        .collect();
    for peer in peers {
        match node
            .net
            .request(peer, YpRequest::Social(SocialRequest::Propose))
            .await
        {
            Ok(YpResponse::Social(reply)) => {
                let _g = proposals_guard();
                if reply.friends {
                    become_friend(node, user, true);
                } else if let Some(mut r) = proposal(node, RequestDirection::Outgoing, user) {
                    r.payload = PROPOSAL_DELIVERED.to_vec();
                    let _ = node.store.put_friend_request(&r);
                }
                return Some(reply.friends);
            }
            Ok(YpResponse::Error { code, .. }) => {
                debug!(?code, "친구 추가 요청을 상대가 받지 않음");
            }
            Ok(_) => {}
            Err(e) => debug!(error = %e, "친구 추가 요청 전달 실패(다음 인증 때 다시)"),
        }
    }
    None
}

/// 인증됐을 때: 이 사용자에게 아직 전하지 못한 친구 추가 요청·새 1:1 공간을 보냅니다.
pub(crate) fn on_peer_authed(node: &Arc<Node>, user: &UserId) {
    if dm_offer(node, user).is_some() {
        let user = *user;
        node.spawn_detached(move |node| async move {
            deliver_dm_offer(&node, &user).await;
        });
    }
    let pending = proposal(node, RequestDirection::Outgoing, user)
        .is_some_and(|r| r.payload.as_slice() != PROPOSAL_DELIVERED);
    let wanted = node.store.get_friend(user).ok().flatten().is_some_and(|f| {
        f.kind == ContactKind::Temporary
            || (f.is_friend() && proposal(node, RequestDirection::Outgoing, user).is_some())
    });
    if pending && wanted {
        let user = *user;
        node.spawn_detached(move |node| async move {
            deliver_proposal(&node, &user).await;
        });
    }
}

/// 받은 `Social(Propose)`. 이미 친구이거나 내가 먼저 요청했으면 둘 다 친구, 아니면 요청을 받아 둡니다(`friend.request`).
/// 거절한 사람이 다시 보내도 표시가 남아 있어 알리지 않습니다.
pub(crate) fn handle_propose(node: &Arc<Node>, _peer: PeerId, user: UserId) -> YpResponse {
    let _g = proposals_guard();
    let Some(f) = node.store.get_friend(&user).ok().flatten() else {
        return YpResponse::error(WireErrorCode::NotMember, "연락처가 아님");
    };
    let friends = match f.kind {
        ContactKind::Expired => {
            return YpResponse::error(WireErrorCode::Expired, "만료된 연락처");
        }
        ContactKind::Friend => {
            clear_proposals(node, &user);
            true
        }
        ContactKind::Temporary => {
            if proposal(node, RequestDirection::Outgoing, &user).is_some() {
                become_friend(node, &user, true);
                true
            } else {
                if proposal(node, RequestDirection::Incoming, &user).is_none()
                    && !receive_proposal(node, user)
                {
                    return YpResponse::error(WireErrorCode::Internal, "저장소 오류");
                }
                false
            }
        }
    };
    YpResponse::Social(SocialReply {
        friends,
        ..Default::default()
    })
}

/// 임시 연락처를 친구로 올립니다: 링크 흔적·요청을 지우고 알리고, 인증된 기기와 노드 카드를 주고받습니다.
/// `clear_outgoing`이 거짓이면 보낸 요청은 남겨 상대에게 전할 때까지 다시 보냅니다. [`proposals_guard`]를 잡고 부릅니다.
fn become_friend(node: &Arc<Node>, user: &UserId, clear_outgoing: bool) {
    let Some(was) = node.store.get_friend(user).ok().flatten() else {
        return;
    };
    if clear_outgoing && let Some(r) = proposal(node, RequestDirection::Outgoing, user) {
        let _ = node.store.delete_friend_request(&r.request_id);
    }
    if was.is_friend()
        || !node
            .store
            .set_contact_kind(user, ContactKind::Friend, None, None)
            .unwrap_or(false)
    {
        return;
    }
    debug!("임시 연락처가 친구가 됨");
    if let Some(r) = proposal(node, RequestDirection::Incoming, user) {
        let _ = node.store.delete_friend_request(&r.request_id);
    }
    node.notify
        .publish(Notification::FriendUpdated { user: *user });
    for (peer, u, _) in node.peers.authed_peers() {
        if u == *user {
            node.spawn_detached(move |node| async move {
                peers::exchange_contact(&node, peer).await;
            });
        }
    }
}

/// 내 다른 기기가 알려 준 연락처 기록과 그 사람이 보낸 친구 추가 요청을 맞춥니다.
pub(crate) fn merge_own_contact(node: &Arc<Node>, c: &OwnContact) {
    merge_contact_record(node, c);
    merge_own_request(node, c);
}

/// 내 다른 기기가 알려 준 연락처 기록을 맞춥니다(protocol.md "내 다른 기기로 전하기"). 꺼져 있던 기기의 옛 기록이 올 수 있어
/// 되돌아가지 않는 규칙으로 정합니다: 친구가 늘 이기고, 임시·만료는 같은 링크면 만료가, 다른 링크면 다시 만난 임시 연락처가
/// 이깁니다. 없는 기록은 둘만의 1:1 공간이 선 뒤에만 만듭니다(크루를 가리키는 기록의 삭제는 다른 기기에 전해지지 않음). 만료된
/// 연락처는 만들지 않습니다(이 기기에서 지운 것일 수 있음).
fn merge_contact_record(node: &Arc<Node>, c: &OwnContact) {
    let Ok(me) = node.me() else { return };
    let guard = contact_guard();
    let Ok(local) = node.store.get_friend(&c.user) else {
        return;
    };
    let with_terms = |base: FriendRecord| FriendRecord {
        kind: c.kind,
        expires_at: c.expires_at,
        link_hash: c.link_hash,
        ..base
    };
    match (local, c.kind) {
        (None, ContactKind::Friend | ContactKind::Temporary) => {
            let one_to_one = node
                .space_state(&c.space)
                .is_some_and(|st| identity::is_friend_space_with(&me, &st, &c.user));
            let record = with_terms(FriendRecord::friend(c.user, Some(c.space)));
            if one_to_one && node.store.put_friend(&record).is_ok() {
                drop(guard);
                sync_service::refresh_space_peers(node, &c.space);
                // `friend.accepted`는 앱이 "연결이 끝났다"고 띄우므로, 다른 기기에서 한 일에는 쓰지 않습니다.
                node.notify
                    .publish(Notification::FriendUpdated { user: c.user });
            }
        }
        (Some(f), ContactKind::Friend) if !f.is_friend() => {
            drop(guard);
            // 다른 기기가 요청을 수락함: 상대가 이 기기로만 닿을 수 있어 이 기기도 같은 답을 보냅니다.
            if f.kind == ContactKind::Temporary
                && proposal(node, RequestDirection::Incoming, &c.user).is_some()
            {
                let user = c.user;
                node.spawn_detached(move |node| async move {
                    let _ = propose(&node, &user).await;
                });
            } else {
                become_friend(node, &c.user, true);
            }
            reopened(node, &f);
        }
        (Some(f), ContactKind::Temporary)
            if f.kind == ContactKind::Expired && f.link_hash != c.link_hash =>
        {
            if node.store.put_friend(&with_terms(f.clone())).is_ok() {
                drop(guard);
                reopened(node, &f);
                node.notify
                    .publish(Notification::FriendUpdated { user: c.user });
            }
        }
        (Some(f), ContactKind::Expired)
            if f.kind == ContactKind::Temporary && f.link_hash == c.link_hash =>
        {
            let expired = node
                .store
                .set_contact_kind(&c.user, ContactKind::Expired, f.expires_at, f.link_hash)
                .unwrap_or(false);
            drop(guard);
            if expired {
                on_contact_expired(node, &c.user, false);
            }
        }
        _ => {}
    }
}

/// 내 다른 기기가 받은 친구 추가 요청을 맞춥니다. 상대는 닿는 기기 하나에만 보내고 기기마다 켜진 때가 달라, 되돌아가지 않게
/// 거절이 기다림을 이깁니다. 같은 링크의 임시 연락처에만 씁니다(만료 뒤 다시 만났으면 옛 요청).
fn merge_own_request(node: &Arc<Node>, c: &OwnContact) {
    if c.request == OwnRequest::None || crate::moderation::is_blocked(node, &c.user) {
        return;
    }
    let _g = proposals_guard();
    let same_contact = node
        .store
        .get_friend(&c.user)
        .ok()
        .flatten()
        .is_some_and(|f| f.kind == ContactKind::Temporary && f.link_hash == c.link_hash);
    let have = proposal(node, RequestDirection::Incoming, &c.user);
    if !same_contact || have.as_ref().is_some_and(rejected) {
        return;
    }
    if c.request == OwnRequest::Rejected {
        let r = incoming_proposal(node, c.user, PROPOSAL_REJECTED);
        if node.store.put_friend_request(&r).is_ok() {
            node.notify
                .publish(Notification::FriendUpdated { user: c.user });
        }
    } else if have.is_none() {
        if proposal(node, RequestDirection::Outgoing, &c.user).is_some() {
            // 이 기기도 요청했으니 `handle_propose`처럼 친구입니다.
            become_friend(node, &c.user, true);
        } else {
            receive_proposal(node, c.user);
        }
    }
}

/// 만료됐던 연락처가 다시 쓰이게 됨: 비활성화됐던 1:1 공간을 다시 동기화합니다.
fn reopened(node: &Node, was: &FriendRecord) {
    if was.kind == ContactKind::Expired
        && let Some(space) = was.space
    {
        sync_service::refresh_space_peers(node, &space);
        node.notify.publish(Notification::SpaceUpdated { space });
    }
}

// ───────────── 크루로 바꾼 뒤 다시 1:1 ─────────────
//
// 1:1 공간을 크루로 바꾸면 그 공간은 크루로 남고 친구 기록은 그 크루를 가리킵니다. 대화를 열면(`friend.open`) 둘만의
// 1:1 공간을 새로 만들어 기록을 옮기고, 상대의 인증된 기기에 `Social(DmSpace)`로 보냅니다(닿지 않으면 다음 인증 때). 받는
// 쪽도 기록이 크루를 가리키면 그 공간으로 옮깁니다. 서로 닿지 않을 때 둘 다 열어 따로 만들었으면 양쪽 모두 ID가 작은 쪽을
// 고릅니다. 내 다른 기기는 받아들인 공간이 상태를 세울 때 따라옵니다(`identity::restore_friend`).

fn dm_offer_key(user: &UserId) -> String {
    format!("{DM_OFFER_PREFIX}{user}")
}

fn dm_offer(node: &Node, user: &UserId) -> Option<DmOffer> {
    node.store.get_setting(&dm_offer_key(user)).ok().flatten()
}

/// 기록의 1:1 공간이 더는 1:1이 아님: 크루로 바뀌었거나, 그 크루에서 나가 이 기기에서 지움(지우면 저장소가 기록의 공간을
/// 비움).
fn no_longer_dm(node: &Node, space: Option<SpaceId>) -> bool {
    space.is_none_or(|s| {
        node.space_state(&s)
            .is_some_and(|st| st.kind == SpaceKind::Crew)
    })
}

/// `friend.open`: 이 연락처와 대화할 1:1 공간. 기록의 공간이 더는 1:1이 아니면([`no_longer_dm`]) 새로 만듭니다. 만료된
/// 연락처는 지난 공간 그대로(읽기만).
pub(crate) async fn open(node: &Arc<Node>, user: &UserId) -> Result<SpaceId> {
    let me = node.me()?;
    let f = node
        .store
        .get_friend(user)?
        .ok_or_else(|| Error::not_found("연락처가 아닙니다"))?;
    if !f.is_active() || !no_longer_dm(node, f.space) {
        return f
            .space
            .ok_or_else(|| Error::not_found("1:1 공간이 아직 없습니다"));
    }
    let devices = node
        .store
        .get_device_list(user)?
        .ok_or_else(|| Error::not_found("상대 기기 목록을 모릅니다"))?;
    let spec = dm_spec(&me, *user, &identity::display_name(node, user), devices);
    let (new, _evs, report) = spaces::create_space(node, spec).await?;
    ingest::after_unlock(node, new, &report);
    // 만드는 사이 기록이 다른 공간으로 옮겨 갔으면(상대가 보낸 공간을 받음, 두 번 누름) 그쪽을 씁니다.
    let settled = (|| -> Result<SpaceId> {
        let _g = contact_guard();
        let cur = node
            .store
            .get_friend(user)?
            .ok_or_else(|| Error::not_found("연락처가 아닙니다"))?;
        if cur.space != f.space {
            return cur
                .space
                .ok_or_else(|| Error::not_found("1:1 공간이 아직 없습니다"));
        }
        // 기록보다 먼저 남겨, 기록만 옮기고 보낼 것을 잃지 않게 합니다.
        node.store.put_setting(
            &dm_offer_key(user),
            &DmOffer {
                space: new,
                theirs: None,
            },
        )?;
        node.store.put_friend(&FriendRecord {
            space: Some(new),
            ..cur
        })?;
        Ok(new)
    })();
    if !matches!(settled, Ok(s) if s == new) {
        let l = ingest::lock_space(node, new).await;
        let _ = spaces::discard_space_locked(node, &l, &new);
        drop(l);
        return settled;
    }
    debug!(space = %new.short(), "크루로 바꾼 상대와 새 1:1 공간");
    sync_service::refresh_space_peers(node, &new);
    node.notify
        .publish(Notification::FriendUpdated { user: *user });
    let user = *user;
    node.spawn_detached(move |node| async move {
        deliver_dm_offer(&node, &user).await;
    });
    Ok(new)
}

/// 상대가 아직 받지 않은 새 1:1 공간을 그 사람의 인증된 기기에 보냅니다. 기록이 이미 다른 공간으로 옮겨 갔으면 지웁니다.
async fn deliver_dm_offer(node: &Arc<Node>, user: &UserId) {
    let Some(o) = dm_offer(node, user) else {
        return;
    };
    let current = node
        .store
        .get_friend(user)
        .ok()
        .flatten()
        .is_some_and(|f| f.is_active() && f.space == Some(o.space));
    if !current {
        let _ = node.store.delete_setting(&dm_offer_key(user));
        return;
    }
    let Ok(events) = node.store.stream_events(
        &o.space,
        StreamId::Space,
        full_range(u64::MAX - 1),
        MAX_EVENTS_PER_MESSAGE as u32,
    ) else {
        return;
    };
    let peers: Vec<PeerId> = node
        .peers
        .authed_peers()
        .into_iter()
        .filter(|(_, u, _)| u == user)
        .map(|(p, _, _)| p)
        .collect();
    for peer in peers {
        let req = YpRequest::Social(SocialRequest::DmSpace {
            space: o.space,
            events: events.clone(),
        });
        match identity::request_after_auth(node, peer, req).await {
            Ok(YpResponse::Ok) => {
                if dm_offer(node, user).is_some_and(|x| x.space == o.space) {
                    let _ = node.store.delete_setting(&dm_offer_key(user));
                }
                sync_service::spawn_catch_up(node, o.space, peer);
            }
            Ok(YpResponse::Social(r)) => {
                if let Some((theirs, _)) = r.friend_space {
                    on_their_dm(node, user, o.space, theirs);
                }
            }
            Ok(YpResponse::Error { code, .. }) => {
                debug!(?code, "새 1:1 공간을 상대가 받지 않음");
            }
            Ok(_) => {}
            Err(e) => debug!(error = %e, "새 1:1 공간 전달 실패(다음 인증 때 다시)"),
        }
    }
}

/// 상대가 내 새 1:1 공간 대신 `theirs`를 쓴다고 답함(내 다른 기기가 먼저 만들었거나, 둘이 따로 만들어 상대 것의 ID가 작음).
/// 기억해 두고, 이미 아는 둘만의 공간이면 바로 옮깁니다(아니면 그 공간을 받을 때 [`follow_dm`]).
fn on_their_dm(node: &Arc<Node>, user: &UserId, mine: SpaceId, theirs: SpaceId) {
    {
        let _g = contact_guard();
        let Some(o) = dm_offer(node, user).filter(|o| o.space == mine && o.space != theirs) else {
            return;
        };
        let _ = node.store.put_setting(
            &dm_offer_key(user),
            &DmOffer {
                theirs: Some(theirs),
                ..o
            },
        );
    }
    let Ok(me) = node.me() else { return };
    let known = node.space_state(&theirs).is_some_and(|st| {
        st.kind == SpaceKind::Space
            && st.members.len() == 2
            && st.is_member(&me.user())
            && st.is_member(user)
    });
    if known && follow_dm(node, user, theirs) {
        node.notify
            .publish(Notification::FriendUpdated { user: *user });
    }
}

/// 기록을 그 사람과의 1:1 공간 `new`로 옮길지. 기록의 공간이 더는 1:1이 아니면 옮깁니다. 내가 만들어 상대가 아직 받지
/// 않은 공간을 가리키면 상대가 쓴다고 답한 공간이거나 ID가 더 작을 때만 옮깁니다(둘이 따로 만들어도 양쪽이 같은 쪽을 고름).
/// 상대가 이미 받은 1:1 공간은 바꾸지 않습니다(친구가 새 공간을 보내 지난 대화를 가리지 못하게).
fn prefers_dm(node: &Node, f: &FriendRecord, new: &SpaceId) -> bool {
    if f.space == Some(*new) || !f.is_active() {
        return false;
    }
    no_longer_dm(node, f.space)
        || f.space.is_some_and(|cur| {
            dm_offer(node, &f.user)
                .is_some_and(|o| o.space == cur && (o.theirs == Some(*new) || *new < cur))
        })
}

/// 둘만의 공간임을 확인한 그 사람과의 1:1 공간 `new`를 알게 됨: [`prefers_dm`]이면 기록을 옮기고 true. 옮기기 전 공간이
/// 내가 만들어 상대가 받지 않은 것이면 보낼 것을 지우고, 메시지가 없으면 공간도 지웁니다.
pub(crate) fn follow_dm(node: &Arc<Node>, user: &UserId, new: SpaceId) -> bool {
    let old = {
        let _g = contact_guard();
        let Some(f) = node.store.get_friend(user).ok().flatten() else {
            return false;
        };
        if !prefers_dm(node, &f, &new) {
            return false;
        }
        let old = f.space;
        if node
            .store
            .put_friend(&FriendRecord {
                space: Some(new),
                ..f
            })
            .is_err()
        {
            return false;
        }
        old
    };
    debug!(space = %new.short(), "친구 기록이 새 1:1 공간으로 옮겨 감");
    if let Some(old) = old
        && dm_offer(node, user).is_some_and(|o| o.space == old)
    {
        let _ = node.store.delete_setting(&dm_offer_key(user));
        discard_if_empty(node, old);
    }
    true
}

/// 기록이 옮겨 간 뒤 남은 내 새 1:1 공간을 지웁니다. 메시지가 있으면 내가 쓴 글이 사라지지 않게 둡니다.
fn discard_if_empty(node: &Arc<Node>, space: SpaceId) {
    node.spawn_detached(move |node| async move {
        let l = ingest::lock_space(&node, space).await;
        let unused = node
            .store
            .events_by_lane(&space, &[Lane::Message])
            .is_ok_and(|v| v.is_empty())
            && node.store.friend_by_space(&space).ok().flatten().is_none();
        if unused {
            if let Err(e) = spaces::discard_space_locked(&node, &l, &space) {
                debug!(error = %e, "남은 1:1 공간 정리 실패");
            }
            drop(l);
            node.notify.publish(Notification::SpaceUpdated { space });
        }
    });
}

/// 받은 `Social(DmSpace)`(인증된 연락처의 기기). 기록을 그 공간으로 옮기게 되면 받아들이고 `Ok`, 아니면 내가 쓰는 1:1
/// 공간으로 답합니다(내가 만들어 아직 전하지 못한 것이면 지금 보냄: 상대도 같은 규칙으로 고름).
pub(crate) async fn handle_dm_space(
    node: &Arc<Node>,
    peer: PeerId,
    user: UserId,
    space: SpaceId,
    events: Vec<SignedEvent>,
) -> YpResponse {
    let Ok(me) = node.me() else {
        return YpResponse::error(WireErrorCode::Internal, "신원 없음");
    };
    let Some(f) = node
        .store
        .get_friend(&user)
        .ok()
        .flatten()
        .filter(|f| f.is_active())
    else {
        return YpResponse::error(WireErrorCode::NotMember, "연락처가 아님");
    };
    let mine = |node: &Arc<Node>| {
        if dm_offer(node, &user).is_some() {
            node.spawn_detached(move |node| async move {
                deliver_dm_offer(&node, &user).await;
            });
        }
        YpResponse::Social(SocialReply {
            friend_space: identity::friend_space(node, &me, &user),
            ..Default::default()
        })
    };
    if f.space != Some(space) && !prefers_dm(node, &f, &space) {
        return mine(node);
    }
    let genesis_id = match precheck_accept(&me, &user, &space, &events) {
        Ok((g, _)) => g,
        Err(why) => return YpResponse::error(WireErrorCode::BadRequest, why),
    };
    let l = ingest::lock_space(node, space).await;
    let report = match ingest_dm_locked(node, &l, peer, &user, space, events, genesis_id) {
        Ok(r) => r,
        Err((code, why)) => return YpResponse::error(code, why),
    };
    drop(l);
    ingest::after_unlock(node, space, &report);
    if follow_dm(node, &user, space) {
        node.notify.publish(Notification::FriendUpdated { user });
    }
    // 확인하는 사이 기록이 다른 공간으로 옮겨 갔으면 받은 공간은 쓰지 않습니다.
    if node.store.friend_by_space(&space).ok().flatten().is_none() {
        discard_if_empty(node, space);
        return mine(node);
    }
    sync_service::refresh_space_peers(node, &space);
    sync_service::spawn_catch_up(node, space, peer);
    YpResponse::Ok
}

// ───────────── 친구 삭제 ─────────────
//
// 끊으면 양쪽 모두 연락처와 1:1 공간(대화·파일·키)을 지웁니다([`cut`]). 공간이 사라져 이벤트로는 전할 수 없으므로(상대가
// 못 받은 앞선 이벤트를 내줄 곳이 없음) 상대에게는 `Social(Cut)`을([`deliver_leaves`]), 내 다른 기기에는 `Social(Devices)`의
// `cut`을 보냅니다. 지금 닿지 않으면 다음에 인증될 때 전합니다. 지운 공간은 묘비를 남겨 옛 소식으로 되살아나지 않게 합니다.

const LEAVING_PREFIX: &str = "friend.leaving.";
/// 이 기간 안에 전하지 못한 끊기는 포기합니다(그동안만 그 사람의 인증을 받아 줌).
const LEAVING_EXPIRY_MS: u64 = 30 * 24 * 60 * 60 * 1000;
/// 끊긴 1:1 공간의 묘비.
const CUT_PREFIX: &str = "friend.cut.";

/// 아직 상대에게 전하지 못한 끊기.
#[derive(Serialize, Deserialize)]
struct Leaving {
    space: SpaceId,
    /// 끊은 시각(예전 기록은 0이라 다음 확인 때 만료로 지워짐).
    #[serde(default)]
    at: Timestamp,
}

fn leaving_key(user: &UserId) -> String {
    format!("{LEAVING_PREFIX}{user}")
}

fn cut_key(space: &SpaceId) -> String {
    format!("{CUT_PREFIX}{space}")
}

/// 끊어서 지운 1:1 공간인지.
pub(crate) fn is_cut(node: &Node, space: &SpaceId) -> bool {
    matches!(node.store.get_setting::<bool>(&cut_key(space)), Ok(Some(_)))
}

/// 끊어서 지운 1:1 공간들(내 다른 기기에게 전함).
pub(crate) fn cut_spaces(node: &Node) -> Vec<SpaceId> {
    node.store
        .settings_with_prefix(CUT_PREFIX)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(k, _)| k.strip_prefix(CUT_PREFIX)?.parse().ok())
        .collect()
}

/// 1:1 공간과 상대(`users`)의 연락처를 이 기기에서 지웁니다. 크루에서도 만나지 않는 사람이면 남은 정보도 지웁니다.
async fn cut(node: &Arc<Node>, space: SpaceId, users: &[UserId]) -> Result<()> {
    node.store.put_setting(&cut_key(&space), &true)?;
    for user in users {
        if node
            .store
            .get_friend(user)?
            .is_some_and(|f| f.space == Some(space))
        {
            node.store.remove_friend(user)?;
            clear_requests_with(node, user, false);
            crate::trust::forget_user(node, user);
        }
        if dm_offer(node, user).is_some_and(|o| o.space == space) {
            node.store.delete_setting(&dm_offer_key(user))?;
        }
    }
    let l = ingest::lock_space(node, space).await;
    spaces::discard_space_locked(node, &l, &space)?;
    drop(l);
    identity::forget_adoption(node, &space);
    for user in users {
        // 전할 끊기가 남았으면 기기 목록을 둡니다: 없으면 그 사람의 폐기된 기기도 인증을 받습니다(전한 뒤 지움).
        if !has_pending_leave(node, user) {
            identity::forget_stranger(node, user);
        }
        node.notify
            .publish(Notification::FriendUpdated { user: *user });
    }
    node.notify.publish(Notification::SpaceUpdated { space });
    debug!(space = %space.short(), "1:1 공간 끊음");
    Ok(())
}

/// 내 기기가 끊었다고 알린 공간이나 한 사람만 남은 1:1 공간을 지웁니다. 그사이 크루로 바꿨으면 그대로 둡니다.
pub(crate) async fn take_cut(node: &Arc<Node>, space: SpaceId) {
    let Ok(me) = node.me() else { return };
    let state = node.space_state(&space);
    let gone = is_cut(node, &space) && !node.spaces.read().contains_key(&space);
    if gone || state.as_ref().is_some_and(|st| st.kind != SpaceKind::Space) {
        return;
    }
    let mut users: Vec<UserId> = state
        .iter()
        .flat_map(|st| st.members.keys())
        .filter(|u| **u != me.user())
        .copied()
        .collect();
    if let Ok(Some(f)) = node.store.friend_by_space(&space)
        && !users.contains(&f.user)
    {
        users.push(f.user);
    }
    if let Err(e) = cut(node, space, &users).await {
        debug!(error = %e, "끊긴 1:1 공간 정리 실패");
    }
}

/// 시작할 때 한 사람만 남은 1:1 공간을 지웁니다(1:1 공간은 둘로 시작하므로 누가 나간 것). 끊어도 공간을 남기던 때의 것이나
/// 지우기 전에 꺼진 것입니다.
pub(crate) fn spawn_cut_sweep(node: &Arc<Node>) {
    let left: Vec<SpaceId> = node
        .space_ids()
        .into_iter()
        .filter(|s| {
            node.space_state(s)
                .is_some_and(|st| st.kind == SpaceKind::Space && st.members.len() < 2)
        })
        .collect();
    if !left.is_empty() {
        node.spawn_detached(move |node| async move {
            for space in left {
                take_cut(&node, space).await;
            }
        });
    }
}

/// `friend.remove`: 친구 관계를 끊고 양쪽의 1:1 공간을 지웁니다.
pub(crate) async fn remove(node: &Arc<Node>, user: &UserId) -> Result<()> {
    let friend = node
        .store
        .get_friend(user)?
        .ok_or_else(|| Error::not_found("친구가 아닙니다"))?;
    let dm = friend.space.filter(|s| {
        node.space_state(s)
            .is_some_and(|st| st.kind == SpaceKind::Space)
    });
    let Some(space) = dm else {
        // 1:1을 크루로 바꿨으면 크루는 그대로 두고 연락처만 지웁니다.
        node.store.remove_friend(user)?;
        clear_requests_with(node, user, false);
        crate::trust::forget_user(node, user);
        node.notify
            .publish(Notification::FriendUpdated { user: *user });
        return Ok(());
    };
    // 만료된 연락처는 상대도 이미 끊긴 것을 알고 비활성화돼 전할 수 없습니다.
    if friend.is_active() {
        node.store.put_setting(
            &leaving_key(user),
            &Leaving {
                space,
                at: Timestamp::now(),
            },
        )?;
    }
    cut(node, space, std::slice::from_ref(user)).await?;
    for (peer, u, _) in node.peers.authed_peers() {
        if u == *user {
            deliver_leaves(node, peer, &u).await;
        }
    }
    debug!("친구 삭제");
    Ok(())
}

/// 이 사용자에게 전할 끊기가 남았는지(전할 때까지, 기한 안에서만 상대 기기의 인증을 받아 줌).
pub(crate) fn has_pending_leave(node: &Node, user: &UserId) -> bool {
    let key = leaving_key(user);
    let Ok(Some(l)) = node.store.get_setting::<Leaving>(&key) else {
        return false;
    };
    if l.at.0.saturating_add(LEAVING_EXPIRY_MS) < Timestamp::now().0 {
        let _ = node.store.delete_setting(&key);
        return false;
    }
    true
}

/// 상대가 이미 나를 친구·멤버로 보지 않으면(인증 거절) 전할 끊기를 지웁니다.
pub(crate) fn clear_pending_leave(node: &Node, user: &UserId) {
    if node.me().is_ok_and(|m| m.user() == *user) {
        return;
    }
    if node
        .store
        .delete_setting(&leaving_key(user))
        .unwrap_or(false)
    {
        debug!("상대가 이미 친구 삭제를 반영함");
    }
}

/// 인증된 상대 기기에 남은 끊기를 전합니다.
pub(crate) async fn deliver_leaves(node: &Arc<Node>, peer: PeerId, user: &UserId) {
    let key = leaving_key(user);
    let Ok(Some(l)) = node.store.get_setting::<Leaving>(&key) else {
        return;
    };
    let req = YpRequest::Social(SocialRequest::Cut { space: l.space });
    // 상대가 먼저 걸어 온 경우 상대가 내 `Auth` 답을 처리할 때까지 잠시 다시 보냅니다.
    let mut r = identity::request_after_auth(node, peer, req.clone()).await;
    // 그래도 인증 전이라면 상대가 내 `Auth`를 받아 주지 않은 것: 다시 보내 확인하고(이미 끊긴 것을 반영했으면 지움),
    // 받아 주면 한 번 더 전합니다.
    if matches!(
        r,
        Ok(YpResponse::Error {
            code: WireErrorCode::Unauthenticated,
            ..
        })
    ) && peers::reconfirm_auth(node, peer).await
    {
        r = node.net.request(peer, req).await.map_err(Into::into);
    }
    match r {
        // 상대가 이미 공간을 모르거나 나를 멤버로 보지 않아도 더 전할 것이 없습니다.
        Ok(
            YpResponse::Ok
            | YpResponse::Error {
                code: WireErrorCode::NotMember | WireErrorCode::NotFound,
                ..
            },
        ) => {
            let _ = node.store.delete_setting(&key);
            // 전하려고 연결한 사이 다시 받은 프로필·기기 목록.
            identity::forget_stranger(node, user);
        }
        Ok(_) => {}
        Err(e) => debug!(error = %e, "친구 삭제 전달 실패(다음 인증 때 다시)"),
    }
}

/// 받은 `Social(Cut)`: 상대가 끊었습니다. 그 사람과의 1:1 공간만 지웁니다.
pub(crate) async fn handle_cut(node: &Arc<Node>, user: UserId, space: SpaceId) -> YpResponse {
    let Ok(me) = node.me() else {
        return YpResponse::error(WireErrorCode::Internal, "신원 없음");
    };
    let Some(st) = node.space_state(&space) else {
        return YpResponse::error(WireErrorCode::NotFound, "없는 공간");
    };
    if user == me.user() || st.kind != SpaceKind::Space || !st.is_member(&user) {
        return YpResponse::error(WireErrorCode::NotMember, "1:1 공간의 상대가 아님");
    }
    match cut(node, space, &[user]).await {
        Ok(()) => YpResponse::Ok,
        Err(e) => {
            debug!(error = %e, "받은 친구 삭제 반영 실패");
            YpResponse::error(WireErrorCode::Internal, "지우지 못함")
        }
    }
}

/// 1:1 공간에서 누가 나갔으면 끊긴 것입니다(`Social(Cut)`이 없던 때의 나가기, 공간 나가기). 적용 잠금 안이라 지우기는 뒤로 넘깁니다.
pub(crate) fn on_state_applied(
    node: &Arc<Node>,
    space: &SpaceId,
    old: Option<&yangpa_core::model::SpaceState>,
    new: &yangpa_core::model::SpaceState,
) {
    let Some(old) = old else { return };
    let Ok(me) = node.me() else { return };
    if new.kind != SpaceKind::Space || old.members.keys().all(|u| new.is_member(u)) {
        return;
    }
    let users: Vec<UserId> = old
        .members
        .keys()
        .filter(|u| **u != me.user())
        .copied()
        .collect();
    let space = *space;
    node.spawn_detached(move |node| async move {
        if let Err(e) = cut(&node, space, &users).await {
            debug!(error = %e, "끊긴 1:1 공간 정리 실패");
        }
    });
}
