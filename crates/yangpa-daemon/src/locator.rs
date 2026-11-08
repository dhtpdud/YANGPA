//! 위치 기록: 하루 단위 키로 암호화한 {PeerId, 링크용 주소}를 DHT에 올리고 친구·멤버 기기를 찾습니다.
//!
//! - DHT 키 = BLAKE3 derive_key("yangpa/v1/locator", 기기 ID ‖ UTC 일 번호 ‖ mode). 값 = XChaCha20-Poly1305(
//!   HKDF(기기 ID, "yangpa/v1/locator", "yangpa/v1/locator-key" ‖ 일 번호 ‖ mode), AD = DHT 키, Padmé(기록)).
//!   기록 = 기기 키로 서명한 {사용자, 기기, 일 번호, PeerId, 주소, 올린 시각}. 기기 ID를 아는 친구·멤버만 키를
//!   만들고 열 수 있고, 날마다 키가 바뀌어 같은 기기를 오래 이어 볼 수 없습니다.
//! - `mode`(올리는 스웜 종류: 직접 연결 0, Tor 1)마다 키가 다릅니다. 같은 키에 직접 연결 PeerId(IP로 접속)와 Tor
//!   PeerId가 차례로 올라오면, 그 키 가까이 있는 DHT 노드가 둘을 같은 기기로 이을 수 있기 때문입니다. 사용자 기기는
//!   Tor로만 올리고 직접 연결 주 스웜(`--direct`)만 0으로 올립니다(예전 빌드의 일반 모드 기록도 0). 찾는 쪽은 둘 다 봅니다.
//! - 주소에는 자기 IP를 넣지 않고 릴레이 경유 주소·onion 주소만 넣습니다(`NetStatus::shareable_addrs`). 직접 주소는
//!   찾은 PeerId로 Kademlia에서 찾습니다(`find_peer`, DHT 서버 역할을 하는 노드만 라우팅 표에 주소가 있음).
//! - 시작하면 연결이 생기는 대로 올리고, 실패하면 물러났다 다시, 성공하면 30분마다·날짜·PeerId·주소가 바뀔 때 다시
//!   올립니다.
//! - 인증된 피어가 identify로 알린 수신 주소를 `peer_cache`에 남겨, 들어온 연결의 상대에게도 재시작 뒤 다시 겁니다.
//! - 멤버 소개(`FindDevice`): 위치 기록으로 못 찾으면 그 기기와 같은 공간에 있는, 지금 인증된 멤버 몇 곳에 묻습니다.
//!   Tor 기기는 모두 DHT 클라이언트이고 앱은 부트스트랩을 넘기지 않아 실제로는 기록을 받아 둘 노드가 없으므로, 만난 적
//!   없는 크루 멤버는 이것으로만 서로를 찾습니다. 위치 기록도 기기 ID를 아는 멤버면 열 수 있어 더 드러나는 것은 없습니다.
//! - 멤버 노드 목록(`GetMemberNodes`): 큰 크루의 gossip 이웃이 모자라면 연결된 멤버에게 그가 최근에 본 멤버 노드를
//!   묻습니다([`crate::neighbors`]). 소개받은 노드는 지금 꺼져 있어도 확인 전 기록(`peer.intro.<기기 ID>`)으로 남겨,
//!   소개해 준 멤버가 꺼진 뒤 그 기기가 켜져도 다시 걸 수 있게 합니다. 쓰기 전에는 늘 인증으로 그 기기인지 봅니다.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use rand::seq::SliceRandom;
use serde::{Deserialize, Serialize};
use tracing::debug;
use yangpa_core::ids::{DeviceId, SpaceId, UserId};
use yangpa_core::sign::{Sig, SignDomain};
use yangpa_core::time::Timestamp;
use yangpa_core::wire::{MAX_MEMBER_NODES, MemberNode, WireErrorCode, YpRequest, YpResponse};
use yangpa_crypto::{DeviceKeys, Secret32};
use yangpa_net::{Multiaddr, NetMode, PeerId};

use crate::node::Node;
use crate::{friends, peers, voice};

/// 기록 형식 판.
const RECORD_VERSION: u8 = 1;
/// 기록에 넣는 주소 수.
const MAX_RECORD_ADDRS: usize = 8;
/// 같은 내용이라도 다시 올리는 간격(DHT 기록은 저장한 노드가 사라지면 함께 사라짐).
const REFRESH: Duration = Duration::from_secs(30 * 60);
/// 올린 뒤 바뀐 것이 없는지 보는 간격.
const CHECK_INTERVAL: Duration = Duration::from_secs(60);
/// 연결이 없거나 신원이 없을 때 다시 볼 간격.
const NOT_READY_WAIT: Duration = Duration::from_secs(2);
const MIN_BACKOFF: Duration = Duration::from_millis(500);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// 조회 결과를 다시 쓰는 시간(같은 기기를 짧은 사이에 거듭 찾지 않게). 못 찾은 결과는 더 짧게.
const FOUND_TTL: Duration = Duration::from_secs(60);
const MISS_TTL: Duration = Duration::from_secs(15);
const MAX_CACHE: usize = 1024;
/// identify로 받은 상대 수신 주소 중 기록할 수.
const MAX_RECORDED_ADDRS: usize = 16;
/// 멤버 소개를 물어보는 멤버 수(Tor 왕복마다 회로를 씀).
const MAX_INTRODUCERS: usize = 3;
/// 소개받은 노드(확인 전, `peer.intro.<기기 ID>` → [`Intro`]).
const INTRO_PREFIX: &str = "peer.intro.";
/// 남겨 두는 소개 수(넘으면 오래된 것부터 지움).
const MAX_INTROS: usize = 256;
/// 같은 공간의 멤버 노드 목록을 다시 묻는 간격(다시 걸기 주기의 배수, 기본 10분).
const MEMBER_NODES_EVERY: u32 = 10;

/// 위치 기록 영역의 노드 수준 상태(`Node::locator`).
#[derive(Default)]
pub struct LocatorState {
    /// `&Node`만 받는 고리(`on_peer_authed`)에서 작업을 띄우려고 둡니다.
    node: OnceLock<Weak<Node>>,
    republish: tokio::sync::Notify,
    published: Mutex<Option<Published>>,
    cache: Mutex<HashMap<DeviceId, (Instant, Option<Found>)>>,
    /// 기록된 노드로 보내기가 실패해 위치 기록으로 찾은 노드에 보낸 횟수(진단·시험용).
    reroutes: AtomicU64,
    /// 시험용: identify로 들은 주소를 기록하지 않음(identify가 없는 Tor 스웜 흉내).
    skip_identify: AtomicBool,
    /// 공간마다 마지막으로 멤버 노드 목록을 물은 때.
    member_nodes_asked: Mutex<HashMap<SpaceId, Instant>>,
}

type Found = (PeerId, Vec<Multiaddr>);

/// 소개받았지만 아직 인증으로 확인하지 않은 노드. 소개에는 서명이 없어 다시 걸 후보로만 씁니다.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Intro {
    pub user: UserId,
    pub device: DeviceId,
    peer: String,
    addrs: Vec<String>,
    at: Timestamp,
}

impl Intro {
    /// 걸 노드와 주소(받을 때와 같은 규칙으로 다시 거름). 걸 주소가 없으면 `None`.
    pub(crate) fn found(&self) -> Option<Found> {
        let peer: PeerId = self.peer.parse().ok()?;
        let addrs = peers::contact_addrs_from(&peer, &self.addrs);
        (!addrs.is_empty()).then_some((peer, addrs))
    }
}

#[derive(Clone)]
struct Published {
    day: u32,
    mode: NetMode,
    peer: PeerId,
    addrs: Vec<Multiaddr>,
    at: Instant,
}

/// 서명하는 기록 본문.
#[derive(Serialize, Deserialize)]
struct RecordBody {
    v: u8,
    user: UserId,
    device: DeviceId,
    day: u32,
    peer: Vec<u8>,
    addrs: Vec<String>,
    published_at: Timestamp,
}

#[derive(Serialize, Deserialize)]
struct SignedRecord {
    body: Vec<u8>,
    sig: Sig,
}

/// 찾아서 연 기록.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct OpenedRecord {
    pub user: UserId,
    pub peer: PeerId,
    pub addrs: Vec<Multiaddr>,
    pub published_at: Timestamp,
}

/// UTC 일 번호.
pub(crate) fn today() -> u32 {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    (secs / 86_400) as u32
}

/// 키에 넣는 스웜 종류 표시(`mode`).
fn mode_tag(mode: NetMode) -> u8 {
    match mode {
        NetMode::Normal => 0,
        NetMode::Tor => 1,
    }
}

/// 찾을 때 보는 스웜 종류(상대가 Tor 기기든 직접 연결 노드든 찾게 둘 다).
const MODES: [NetMode; 2] = [NetMode::Normal, NetMode::Tor];

/// DHT 키.
pub(crate) fn record_key(device: &DeviceId, day: u32, mode: NetMode) -> Vec<u8> {
    let mut h = blake3::Hasher::new_derive_key("yangpa/v1/locator");
    h.update(device.as_bytes());
    h.update(&day.to_le_bytes());
    h.update(&[mode_tag(mode)]);
    h.finalize().as_bytes().to_vec()
}

fn record_secret(device: &DeviceId, day: u32, mode: NetMode) -> Secret32 {
    let mut info = b"yangpa/v1/locator-key".to_vec();
    info.extend_from_slice(&day.to_le_bytes());
    info.push(mode_tag(mode));
    yangpa_crypto::derive(device.as_bytes(), b"yangpa/v1/locator", &info)
}

/// 기록을 만들어 암호화합니다(DHT 값).
pub(crate) fn seal_record(
    user: UserId,
    keys: &DeviceKeys,
    day: u32,
    mode: NetMode,
    peer: &PeerId,
    addrs: &[Multiaddr],
) -> crate::error::Result<Vec<u8>> {
    let device = keys.device_id();
    let body = RecordBody {
        v: RECORD_VERSION,
        user,
        device,
        day,
        peer: peer.to_bytes(),
        addrs: addrs
            .iter()
            .take(MAX_RECORD_ADDRS)
            .map(|a| a.to_string())
            .collect(),
        published_at: Timestamp::now(),
    };
    let body = postcard::to_stdvec(&body)?;
    let sig = keys.signing.sign(SignDomain::Locator, &body);
    let signed = postcard::to_stdvec(&SignedRecord { body, sig })?;
    let key = record_key(&device, day, mode);
    Ok(yangpa_crypto::seal(
        &record_secret(&device, day, mode),
        &key,
        &yangpa_crypto::pad(&signed),
    ))
}

/// DHT 값을 엽니다. 기기 ID를 모르면(다른 기기 ID로) 열 수 없고, 서명·기기·날짜·모드가 맞아야 합니다.
pub(crate) fn open_record(
    device: &DeviceId,
    day: u32,
    mode: NetMode,
    value: &[u8],
) -> Option<OpenedRecord> {
    let key = record_key(device, day, mode);
    let padded = yangpa_crypto::open(&record_secret(device, day, mode), &key, value).ok()?;
    let signed: SignedRecord = postcard::from_bytes(yangpa_crypto::unpad(&padded).ok()?).ok()?;
    yangpa_core::sign::verify(
        device.as_bytes(),
        SignDomain::Locator,
        &signed.body,
        &signed.sig,
    )
    .ok()?;
    let body: RecordBody = postcard::from_bytes(&signed.body).ok()?;
    if body.v != RECORD_VERSION || body.device != *device || body.day != day {
        return None;
    }
    Some(OpenedRecord {
        user: body.user,
        peer: PeerId::from_bytes(&body.peer).ok()?,
        addrs: body
            .addrs
            .iter()
            .take(MAX_RECORD_ADDRS)
            .filter_map(|a| a.parse().ok())
            .collect(),
        published_at: body.published_at,
    })
}

pub(crate) fn start(node: &Arc<Node>) {
    let _ = node.locator.node.set(Arc::downgrade(node));
    let weak = Arc::downgrade(node);
    node.tasks.spawn(publish_loop(weak));
}

enum Step {
    Done,
    NotReady,
    Failed,
}

async fn publish_loop(weak: Weak<Node>) {
    let mut backoff = MIN_BACKOFF;
    loop {
        let Some(node) = weak.upgrade() else { return };
        let wait = match publish_step(&node).await {
            Step::Done => {
                backoff = MIN_BACKOFF;
                CHECK_INTERVAL
            }
            Step::NotReady => NOT_READY_WAIT,
            Step::Failed => {
                let b = backoff;
                backoff = (backoff * 2).min(MAX_BACKOFF);
                b
            }
        };
        let notify = async {
            node.locator.republish.notified().await;
        };
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = notify => backoff = MIN_BACKOFF,
        }
    }
}

/// 올릴 내용이 바뀌었거나 오래됐으면 올립니다.
async fn publish_step(node: &Arc<Node>) -> Step {
    let Ok(me) = node.me() else {
        return Step::NotReady;
    };
    let status = node.net.last_status();
    if status.peers == 0 || node.net.is_closed() {
        return Step::NotReady;
    }
    let day = today();
    let mode = status.mode;
    let peer = status.peer_id;
    let addrs: Vec<Multiaddr> = status
        .shareable_addrs()
        .into_iter()
        .take(MAX_RECORD_ADDRS)
        .collect();
    let fresh = node.locator.published.lock().as_ref().is_some_and(|p| {
        p.day == day
            && p.mode == mode
            && p.peer == peer
            && p.addrs == addrs
            && p.at.elapsed() < REFRESH
    });
    if fresh {
        return Step::Done;
    }
    let value = match seal_record(me.user(), &me.device, day, mode, &peer, &addrs) {
        Ok(v) => v,
        Err(e) => {
            debug!(error = %e, "위치 기록을 만들지 못함");
            return Step::Failed;
        }
    };
    match node
        .net
        .put_record(record_key(&me.device_id(), day, mode), value)
        .await
    {
        Ok(()) => {
            debug!(relay_addrs = addrs.len(), "위치 기록 올림");
            *node.locator.published.lock() = Some(Published {
                day,
                mode,
                peer,
                addrs,
                at: Instant::now(),
            });
            Step::Done
        }
        Err(e) => {
            debug!(error = %e, "위치 기록을 올리지 못함");
            Step::Failed
        }
    }
}

/// 기기의 찾기 결과 캐시를 지웁니다(시험에서 DHT에 기록을 새로 넣은 뒤).
pub(crate) fn forget_cached(node: &Node, device: &DeviceId) {
    node.locator.cache.lock().remove(device);
}

/// 위치 기록으로 찾은 노드에 보내기가 성공했음을 셉니다(`sync_service::push_to`).
pub(crate) fn note_reroute(node: &Node) {
    node.locator.reroutes.fetch_add(1, Ordering::Relaxed);
}

/// [`note_reroute`] 횟수. 시험·진단용.
pub(crate) fn reroutes(node: &Node) -> u64 {
    node.locator.reroutes.load(Ordering::Relaxed)
}

/// 마지막으로 올린 기록의 (일 번호, PeerId). 시험·진단용.
pub(crate) fn published(node: &Node) -> Option<(u32, PeerId)> {
    node.locator
        .published
        .lock()
        .as_ref()
        .map(|p| (p.day, p.peer))
}

/// 기기의 지금 노드와 주소를 찾습니다: 위치 기록(오늘·어제·내일) → 없으면 마지막으로 알던 PeerId로 →
/// 주소가 없으면 Kademlia `find_peer` → 그래도 없으면 멤버 소개([`ask_members`]). 이 기기가 쓸 수 있는 주소가 하나도
/// 없으면 `None`. 멤버 소개는 서명이 없으므로 부르는 쪽이 인증으로 그 기기임을 확인합니다(`peers::reach_found`).
pub(crate) async fn lookup(
    node: &Arc<Node>,
    user: &UserId,
    device: &DeviceId,
) -> Option<(PeerId, Vec<Multiaddr>)> {
    if node.me().is_ok_and(|m| m.device_id() == *device) {
        return None;
    }
    if let Some((at, found)) = node.locator.cache.lock().get(device).cloned() {
        let ttl = if found.is_some() { FOUND_TTL } else { MISS_TTL };
        if at.elapsed() < ttl {
            return found;
        }
    }
    let found = lookup_uncached(node, user, device).await;
    let mut cache = node.locator.cache.lock();
    if cache.len() >= MAX_CACHE {
        cache.retain(|_, (at, _)| at.elapsed() < FOUND_TTL);
        if cache.len() >= MAX_CACHE {
            cache.clear();
        }
    }
    cache.insert(*device, (Instant::now(), found.clone()));
    found
}

async fn lookup_uncached(node: &Arc<Node>, user: &UserId, device: &DeviceId) -> Option<Found> {
    let record = find_record(node, device).await.filter(|r| r.user == *user);
    let known = match record {
        Some(r) => Some((r.peer, r.addrs)),
        // 기록이 없으면(아직 못 올렸거나 DHT에서 사라짐) 마지막으로 알던 PeerId로 찾아 봅니다.
        None => node
            .store
            .peer_for_device(device)
            .ok()
            .flatten()
            .and_then(|rec| PeerId::from_bytes(&rec.peer_id).ok())
            .map(|p| (p, Vec::new())),
    };
    if let Some((peer, mut addrs)) = known {
        if peer == node.peer_id() {
            return None;
        }
        if addrs.is_empty() {
            addrs = node.net.find_peer(peer).await.unwrap_or_default();
        }
        if !addrs.is_empty() {
            debug!(%peer, addrs = addrs.len(), "위치 기록으로 기기를 찾음");
            return Some((peer, addrs));
        }
    }
    if let Some(found) = ask_members(node, user, device).await {
        return Some(found);
    }
    // 물어볼 멤버가 지금 없으면 전에 소개받아 둔 노드로(그 기기가 이제 켜졌을 수 있음).
    load_intro(node, device)
        .filter(|i| i.user == *user)
        .and_then(|i| i.found())
}

/// 멤버 소개: 그 기기와 같은 공간(내가 멤버이고 비활성화되지 않은)의 멤버 기기 중 지금 인증된 곳에
/// [`MAX_INTRODUCERS`]곳까지 차례로 묻습니다. 받은 주소는 `Social(Contact)`와 같은 규칙으로 거릅니다(평문 IP 버림).
async fn ask_members(node: &Arc<Node>, user: &UserId, device: &DeviceId) -> Option<Found> {
    let me = node.me().ok()?.user();
    let spaces: Vec<(SpaceId, Arc<yangpa_core::model::SpaceState>)> = node
        .space_ids()
        .into_iter()
        .filter(|s| !friends::space_deactivated(node, s))
        .filter_map(|s| node.space_state(&s).map(|st| (s, st)))
        .filter(|(_, st)| {
            st.is_member(&me) && st.is_member(user) && st.device_belongs(user, device)
        })
        .collect();
    if spaces.is_empty() {
        return None;
    }
    let mut asked = 0;
    for (p, u, d) in node.peers.authed_peers() {
        if asked >= MAX_INTRODUCERS {
            break;
        }
        if d == *device {
            continue;
        }
        let Some((space, _)) = spaces
            .iter()
            .find(|(_, st)| st.is_member(&u) && st.device_belongs(&u, &d))
        else {
            continue;
        };
        asked += 1;
        let req = YpRequest::FindDevice {
            space: *space,
            user: *user,
            device: *device,
        };
        match node.net.request(p, req).await {
            Ok(YpResponse::DeviceNode { peer_id, addrs }) => {
                let Ok(peer) = PeerId::from_bytes(&peer_id) else {
                    continue;
                };
                let addrs = peers::contact_addrs_from(&peer, &addrs);
                if peer != node.peer_id() && !addrs.is_empty() {
                    debug!(%peer, addrs = addrs.len(), "멤버 소개로 기기를 찾음");
                    remember_intros(node, [(*user, *device, peer, &addrs[..])]);
                    return Some((peer, addrs));
                }
            }
            Ok(_) => {}
            Err(e) => debug!(peer = %p, error = %e, "멤버 소개 요청 실패"),
        }
    }
    None
}

/// `FindDevice`에 답합니다: 요청 기기와 물은 기기가 모두 이 공간(내가 멤버이고 비활성화되지 않은)의 활성 멤버
/// 기기일 때만, 아는 그 기기의 노드(지금 인증된 연결, 없으면 노드 기록)와 링크용 주소를 줍니다.
pub(crate) fn handle_find_device(
    node: &Node,
    peer: PeerId,
    space: SpaceId,
    user: UserId,
    device: DeviceId,
) -> YpResponse {
    let Some((ru, rd)) = node.peers.identity_of(&peer) else {
        return YpResponse::error(WireErrorCode::Unauthenticated, "인증되지 않음");
    };
    let Ok(me) = node.me() else {
        return YpResponse::error(WireErrorCode::Internal, "신원 없음");
    };
    let shared = !friends::space_deactivated(node, &space)
        && node.space_state(&space).is_some_and(|st| {
            st.is_member(&me.user())
                && st.is_member(&ru)
                && st.device_belongs(&ru, &rd)
                && st.is_member(&user)
                && st.device_belongs(&user, &device)
        });
    if !shared {
        return YpResponse::error(WireErrorCode::NotMember, "같은 공간의 멤버 기기가 아님");
    }
    let Some(found) = peers::peer_for_device(node, &device).filter(|p| *p != peer) else {
        return YpResponse::error(WireErrorCode::NotFound, "모르는 기기");
    };
    let addrs = peers::link_addrs_of(node, &found);
    if addrs.is_empty() {
        return YpResponse::error(WireErrorCode::NotFound, "알려 줄 주소가 없음");
    }
    YpResponse::DeviceNode {
        peer_id: found.to_bytes(),
        addrs,
    }
}

/// `GetMemberNodes`에 답합니다: 검사는 `FindDevice`와 같고, 7일 안에 연결에 성공한 이 공간 멤버 기기(요청 기기·나 빼고)의
/// 노드를 섞어 [`MAX_MEMBER_NODES`]개까지 줍니다. 지금 꺼진 기기도 넣습니다(켜지면 요청 기기가 걺).
pub(crate) fn handle_get_member_nodes(node: &Node, peer: PeerId, space: SpaceId) -> YpResponse {
    let Some((ru, rd)) = node.peers.identity_of(&peer) else {
        return YpResponse::error(WireErrorCode::Unauthenticated, "인증되지 않음");
    };
    let Ok(me) = node.me() else {
        return YpResponse::error(WireErrorCode::Internal, "신원 없음");
    };
    let Some(st) = node.space_state(&space).filter(|st| {
        !friends::space_deactivated(node, &space)
            && st.is_member(&me.user())
            && st.is_member(&ru)
            && st.device_belongs(&ru, &rd)
    }) else {
        return YpResponse::error(WireErrorCode::NotMember, "같은 공간의 멤버 기기가 아님");
    };
    let mut rows = match node.store.identified_peers() {
        Ok(r) => r,
        Err(e) => {
            debug!(error = %e, "노드 기록을 읽지 못함");
            return YpResponse::error(WireErrorCode::Internal, "노드 기록을 읽지 못함");
        }
    };
    let since = Timestamp::now()
        .0
        .saturating_sub(crate::neighbors::RECENT.as_millis() as u64);
    rows.retain(|r| {
        r.last_success.is_some_and(|t| t.0 >= since)
            && match (r.user, r.device) {
                (Some(u), Some(d)) => {
                    d != rd && d != me.device_id() && st.is_member(&u) && st.device_belongs(&u, &d)
                }
                _ => false,
            }
    });
    // 최근 순으로 주면 물을 수 있는 멤버마다 누가 언제 접속했는지 순위가 새므로 섞어서 줍니다.
    rows.shuffle(&mut rand::thread_rng());
    let nodes = rows
        .into_iter()
        .filter_map(|r| {
            let (user, device) = (r.user?, r.device?);
            let found = PeerId::from_bytes(&r.peer_id).ok()?;
            let addrs = peers::link_addrs_of(node, &found);
            (found != peer && !addrs.is_empty()).then_some(MemberNode {
                user,
                device,
                peer_id: r.peer_id,
                addrs,
            })
        })
        .take(MAX_MEMBER_NODES)
        .collect();
    YpResponse::MemberNodes(nodes)
}

/// 이번에 그 공간의 멤버 노드 목록을 물어도 되는지(다시 걸기 주기의 [`MEMBER_NODES_EVERY`]배마다 한 번).
pub(crate) fn member_nodes_due(node: &Node, space: &SpaceId) -> bool {
    let every = node.cfg.redial_interval * MEMBER_NODES_EVERY;
    node.locator
        .member_nodes_asked
        .lock()
        .get(space)
        .is_none_or(|at| at.elapsed() >= every)
}

/// 연결된 멤버 한 곳에 멤버 노드 목록을 묻고, 받은 것을 소개로 남깁니다(걸기는 다음 이웃 고르기에서).
pub(crate) async fn ask_member_nodes(node: &Arc<Node>, space: SpaceId, peer: PeerId) {
    node.locator
        .member_nodes_asked
        .lock()
        .insert(space, Instant::now());
    let nodes = match node
        .net
        .request(peer, YpRequest::GetMemberNodes { space })
        .await
    {
        Ok(YpResponse::MemberNodes(n)) => n,
        Ok(_) => return,
        Err(e) => {
            debug!(peer = %peer, error = %e, "멤버 노드 목록 요청 실패");
            return;
        }
    };
    let (Ok(me), Some(st)) = (node.me(), node.space_state(&space)) else {
        return;
    };
    let found: Vec<(UserId, DeviceId, PeerId, Vec<Multiaddr>)> = nodes
        .into_iter()
        .take(MAX_MEMBER_NODES)
        .filter(|n| {
            n.device != me.device_id()
                && st.is_member(&n.user)
                && st.device_belongs(&n.user, &n.device)
        })
        .filter_map(|n| {
            let p = PeerId::from_bytes(&n.peer_id).ok()?;
            let addrs = peers::contact_addrs_from(&p, &n.addrs);
            (p != node.peer_id() && !addrs.is_empty()).then_some((n.user, n.device, p, addrs))
        })
        .collect();
    debug!(%space, nodes = found.len(), "멤버 노드 목록을 받음");
    remember_intros(node, found.iter().map(|(u, d, p, a)| (*u, *d, *p, &a[..])));
}

fn intro_key(device: &DeviceId) -> String {
    format!("{INTRO_PREFIX}{device}")
}

/// 인증으로 확인한 노드가 묶인 기기인지(기기 칸은 인증 때만 채워짐, `Store::set_peer_identity`).
fn verified(node: &Node, device: &DeviceId) -> bool {
    node.store.peer_for_device(device).ok().flatten().is_some()
}

/// 소개받은 노드를 확인 전 기록으로 남깁니다. 확인된 노드가 있는 기기는 남기지 않습니다(서명 없는 소개로 덮지 않게).
fn remember_intros<'a>(
    node: &Node,
    found: impl IntoIterator<Item = (UserId, DeviceId, PeerId, &'a [Multiaddr])>,
) {
    let at = Timestamp::now();
    for (user, device, peer, addrs) in found {
        if verified(node, &device) {
            continue;
        }
        let intro = Intro {
            user,
            device,
            peer: peer.to_string(),
            addrs: addrs.iter().map(|a| a.to_string()).collect(),
            at,
        };
        if let Err(e) = node.store.put_setting(&intro_key(&device), &intro) {
            debug!(error = %e, "소개를 저장하지 못함");
        }
    }
    let Ok(rows) = node.store.settings_with_prefix(INTRO_PREFIX) else {
        return;
    };
    if rows.len() > MAX_INTROS {
        let mut rows: Vec<(String, Timestamp)> = rows
            .into_iter()
            .map(|(k, v)| {
                let at = serde_json::from_value::<Intro>(v).map_or(Timestamp(0), |i| i.at);
                (k, at)
            })
            .collect();
        rows.sort_by_key(|(_, at)| *at);
        for (k, _) in &rows[..rows.len() - MAX_INTROS] {
            let _ = node.store.delete_setting(k);
        }
    }
}

fn load_intro(node: &Node, device: &DeviceId) -> Option<Intro> {
    if verified(node, device) {
        return None;
    }
    node.store.get_setting(&intro_key(device)).ok().flatten()
}

/// 아직 확인하지 않은 소개 모두. 그 사이 인증으로 확인된 기기의 소개는 지웁니다.
pub(crate) fn intros(node: &Node) -> Vec<Intro> {
    let Ok(rows) = node.store.settings_with_prefix(INTRO_PREFIX) else {
        return Vec::new();
    };
    rows.into_iter()
        .filter_map(|(k, v)| {
            let i = serde_json::from_value::<Intro>(v).ok()?;
            if verified(node, &i.device) {
                let _ = node.store.delete_setting(&k);
                return None;
            }
            Some(i)
        })
        .collect()
}

/// 소개받은 노드가 다른 기기였으면 그 소개를 지웁니다.
pub(crate) fn forget_intro(node: &Node, device: &DeviceId) {
    let _ = node.store.delete_setting(&intro_key(device));
}

/// 오늘·어제·내일 키로(두 스웜 종류 모두) 찾아 가장 최근에 올린 기록을 고릅니다(기기마다 날짜 경계가 조금씩 다를 수 있음).
pub(crate) async fn find_record(node: &Node, device: &DeviceId) -> Option<OpenedRecord> {
    let today = today();
    let days = [today, today.saturating_sub(1), today.saturating_add(1)];
    let keys = days
        .iter()
        .flat_map(|day| MODES.iter().map(move |mode| (*day, *mode)));
    let lookups = keys.map(|(day, mode)| async move {
        let values = node
            .net
            .get_record(record_key(device, day, mode))
            .await
            .unwrap_or_default();
        values
            .iter()
            .filter_map(|v| open_record(device, day, mode, v))
            .collect::<Vec<_>>()
    });
    futures::future::join_all(lookups)
        .await
        .into_iter()
        .flatten()
        .max_by_key(|r| r.published_at)
}

/// 인증된 피어(`peers::remember`의 끝): 상대가 identify로 알린 수신 주소를 기록하고(들어온 연결이라도 재시작 뒤
/// 다시 걸 수 있게), 통화 중이면 상대에게 다시 알립니다.
pub(crate) fn on_peer_authed(node: &Node, peer: &PeerId, user: &UserId, device: &DeviceId) {
    let Some(node) = node.locator.node.get().and_then(Weak::upgrade) else {
        return;
    };
    let (peer, user, device) = (*peer, *user, *device);
    node.spawn_detached(move |node| async move {
        record_listen_addrs(&node, peer).await;
    });
    voice::on_peer_authed(&node, peer, user, device);
}

async fn record_listen_addrs(node: &Node, peer: PeerId) {
    if node.locator.skip_identify.load(Ordering::Relaxed) {
        return;
    }
    let heard = node.net.peer_listen_addrs(peer).await;
    if heard.is_empty() {
        return;
    }
    let mut all: Vec<String> = heard
        .iter()
        .take(MAX_RECORDED_ADDRS)
        .map(|a| a.to_string())
        .collect();
    if let Ok(Some(rec)) = node.store.get_peer(&peer.to_bytes()) {
        for a in rec.addrs {
            if !all.contains(&a) {
                all.push(a);
            }
        }
    }
    if let Err(e) = node
        .store
        .record_peer_success(&peer.to_bytes(), &all, Timestamp::now())
    {
        debug!(error = %e, "피어 주소 기록 실패");
    }
}

/// 시험용: identify로 들은 주소를 기록하지 않게 합니다(Tor 스웜처럼 `Social(Contact)`로만 주소를 알게).
pub(crate) fn skip_identify_addrs(node: &Node) {
    node.locator.skip_identify.store(true, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;
    use yangpa_crypto::Identity;

    fn keys() -> (UserId, DeviceKeys) {
        (Identity::generate().user_id(), DeviceKeys::generate())
    }

    #[test]
    fn record_roundtrip_and_privacy() {
        let (user, dev) = keys();
        let peer = PeerId::random();
        let relay = PeerId::random();
        let circuit: Multiaddr =
            format!("/ip4/1.2.3.4/tcp/4001/p2p/{relay}/p2p-circuit/p2p/{peer}")
                .parse()
                .unwrap();
        let day = 20_000;
        let n = NetMode::Normal;
        let v = seal_record(user, &dev, day, n, &peer, std::slice::from_ref(&circuit)).unwrap();
        let opened = open_record(&dev.device_id(), day, n, &v).unwrap();
        assert_eq!(opened.peer, peer);
        assert_eq!(opened.user, user);
        assert_eq!(opened.addrs, vec![circuit]);
        // 암호문에는 PeerId·주소가 보이지 않는다.
        let pid = peer.to_bytes();
        assert!(!v.windows(pid.len()).any(|w| w == pid.as_slice()));
        assert!(!v.windows(7).any(|w| w == b"1.2.3.4"));
        // 다른 날짜·다른 기기·사용자 ID로는 열 수 없다.
        assert!(open_record(&dev.device_id(), day + 1, n, &v).is_none());
        assert!(open_record(&DeviceId::random(), day, n, &v).is_none());
        assert!(open_record(&DeviceId(user.0), day, n, &v).is_none());
        // 키는 날짜마다 다르다.
        assert_ne!(
            record_key(&dev.device_id(), day, n),
            record_key(&dev.device_id(), day + 1, n)
        );
    }

    /// 같은 날이라도 스웜 종류마다 DHT 키·암호 키가 달라, 한 기기의 직접 연결 기록과 Tor 기록을 같은 키로 이을 수 없다.
    #[test]
    fn modes_use_separate_keys() {
        let (user, dev) = keys();
        let device = dev.device_id();
        let day = 20_002;
        assert_ne!(
            record_key(&device, day, NetMode::Normal),
            record_key(&device, day, NetMode::Tor)
        );
        let peer = PeerId::random();
        let v = seal_record(user, &dev, day, NetMode::Tor, &peer, &[]).unwrap();
        assert!(open_record(&device, day, NetMode::Normal, &v).is_none());
        assert_eq!(
            open_record(&device, day, NetMode::Tor, &v).map(|r| r.peer),
            Some(peer)
        );
    }

    #[test]
    fn forged_record_rejected() {
        // 기기 ID를 아는 다른 사람이 같은 키로 기록을 만들어도 서명이 맞지 않으면 버린다.
        let (user, victim) = keys();
        let (_, attacker) = keys();
        let day = 20_001;
        let device = victim.device_id();
        let body = RecordBody {
            v: RECORD_VERSION,
            user,
            device,
            day,
            peer: PeerId::random().to_bytes(),
            addrs: Vec::new(),
            published_at: Timestamp::now(),
        };
        let body = postcard::to_stdvec(&body).unwrap();
        let sig = attacker.signing.sign(SignDomain::Locator, &body);
        let signed = postcard::to_stdvec(&SignedRecord { body, sig }).unwrap();
        let n = NetMode::Normal;
        let v = yangpa_crypto::seal(
            &record_secret(&device, day, n),
            &record_key(&device, day, n),
            &yangpa_crypto::pad(&signed),
        );
        assert!(open_record(&device, day, n, &v).is_none());
    }
}
