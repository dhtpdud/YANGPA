//! 데몬이 쓰는 핸들([`NetHandle`])과 이벤트([`NetEvent`]).
//!
//! 핸들은 명령을 채널로 스웜 작업에 넘기기만 하므로 복제해서 어디서든 써도 됩니다. 스웜 작업이 끝나면
//! 비동기 호출은 [`NetError::Shutdown`]을 돌려줍니다.

use std::collections::VecDeque;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use libp2p::gossipsub::{MessageAcceptance, MessageId};
use libp2p::{Multiaddr, PeerId};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};
use tracing::info;
use yangpa_core::wire::{GossipMessage, GossipTopic, YpRequest, YpResponse};

use crate::NetError;
use crate::addr;
use crate::codec::Incoming;
use crate::config::{MAX_RECORD_KEY, MAX_RECORD_VALUE, NetConfig, NetMode};
use crate::tor_stream::{TorActivity, TorIsolation, TorStream};

/// 스웜 작업이 데몬에 알리는 일.
#[derive(Debug)]
pub enum NetEvent {
    /// 다른 노드의 요청. `reply`로 답하면 스웜 작업이 상대에게 보냅니다. 답하지 않고 버리면 오류 응답이 갑니다.
    /// `Ping`은 네트워크 계층이 바로 `Pong`으로 답하므로 오지 않습니다. 압축된 요청은 `peer`를 인증한 뒤에만
    /// [`Incoming::open`]으로 풉니다.
    InboundRequest {
        peer: PeerId,
        req: Incoming,
        reply: oneshot::Sender<YpResponse>,
    },
    /// gossip 메시지. 네트워크 단계 검사(토픽·허용 목록·형식)는 통과했고, 데몬이 내용을 검사한 뒤
    /// [`NetHandle::report_gossip`]으로 판정을 알려야 다른 피어에게 전달됩니다.
    Gossip {
        from: PeerId,
        msg_id: MessageId,
        /// 받은 토픽(그룹 토픽이면 데몬이 그 그룹 것만 받음).
        topic: GossipTopic,
        msg: GossipMessage,
    },
    /// 통화 미디어 프레임 묶음(`/yangpa/media/1`). 네트워크 계층은 크기만 봤고, 보낸 피어 확인·내용 검사는 데몬이 합니다.
    /// 데몬 쪽 대기열이 밀리면 버립니다(늦은 소리는 쓸모가 없음).
    Media {
        peer: PeerId,
        frames: Vec<Vec<u8>>,
    },
    PeerConnected(PeerId),
    PeerDisconnected(PeerId),
    /// 상태가 바뀌었을 때(주소·NAT·연결 수·Tor 진행률 등).
    Status(NetStatus),
}

/// gossip 메시지 판정.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GossipVerdict {
    /// 유효: 다른 피어에게 전달합니다.
    Accept,
    /// 위반(서명·권한 등): 버리고 보낸 피어를 감점합니다.
    Reject,
    /// 전달하지 않지만 감점도 하지 않습니다(이미 가진 것, 판단 불가).
    Ignore,
}

impl From<GossipVerdict> for MessageAcceptance {
    fn from(v: GossipVerdict) -> Self {
        match v {
            GossipVerdict::Accept => MessageAcceptance::Accept,
            GossipVerdict::Reject => MessageAcceptance::Reject,
            GossipVerdict::Ignore => MessageAcceptance::Ignore,
        }
    }
}

/// NAT 상태(AutoNAT).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum NatStatus {
    #[default]
    Unknown,
    /// 외부에서 직접 연결할 수 있음.
    Public,
    /// NAT·방화벽 뒤. 릴레이와 홀펀칭으로 연결받습니다.
    Private,
    /// Tor 스웜이라 검사하지 않음.
    Disabled,
}

/// 노드 상태.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetStatus {
    pub peer_id: PeerId,
    pub mode: NetMode,
    /// 실제 수신 주소(이 기기 진단용. 내부 IP가 있으므로 링크에 넣지 마세요).
    pub listen_addrs: Vec<Multiaddr>,
    /// 확인된 외부 주소(릴레이 경유 제외).
    pub external_addrs: Vec<Multiaddr>,
    /// 릴레이 경유 주소(`…/p2p-circuit/p2p/<자기 PeerId>`). 링크·위치 기록에 넣는 주소. 고른 순서의 릴레이마다
    /// 번갈아, 릴레이 안에서는 걸기 쉬운 주소(IPv4·DNS, IPv6, 그 밖의 전송 순)부터 둡니다.
    pub relay_addrs: Vec<Multiaddr>,
    /// Tor 스웜의 onion 서비스 주소(`/onion3/<주소>:<포트>`).
    pub onion_addr: Option<Multiaddr>,
    pub nat: NatStatus,
    /// 성립한 연결 수.
    pub connections: u32,
    /// 연결된 피어 수.
    pub peers: u32,
    /// Tor 부트스트랩 진행률(0~100). Arti를 아직 시작하지 않았으면 `None`.
    pub tor_bootstrap: Option<u8>,
    /// 지금 예약을 잡아 중계 주소를 내준 릴레이(고른 순서: 부트스트랩 노드, 지난번 릴레이, 나머지).
    pub relays: Vec<RelayNode>,
}

/// 예약을 잡은 릴레이 하나.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayNode {
    pub peer_id: PeerId,
}

impl NetStatus {
    pub(crate) fn initial(peer_id: PeerId, mode: NetMode) -> Self {
        NetStatus {
            peer_id,
            mode,
            listen_addrs: Vec::new(),
            external_addrs: Vec::new(),
            relay_addrs: Vec::new(),
            onion_addr: None,
            nat: if mode == NetMode::Tor {
                NatStatus::Disabled
            } else {
                NatStatus::Unknown
            },
            connections: 0,
            peers: 0,
            tor_bootstrap: None,
            relays: Vec::new(),
        }
    }

    /// 링크·위치 기록에 넣어도 되는 주소: 릴레이 경유 주소와 onion 주소. 자기 IP는 들어가지 않습니다.
    pub fn shareable_addrs(&self) -> Vec<Multiaddr> {
        self.relay_addrs
            .iter()
            .filter(|a| addr::is_link_safe_relay_addr(a))
            .cloned()
            .chain(self.onion_addr.clone())
            .collect()
    }
}

/// 연결된 피어 하나.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectedPeer {
    pub peer_id: PeerId,
    /// 열린 연결 수.
    pub connections: u32,
    /// 릴레이 경유 연결만 있는지(홀펀칭 전). 큰 전송은 미루는 편이 좋습니다.
    pub relayed_only: bool,
    /// 최근 왕복 시간.
    pub rtt_ms: Option<u32>,
}

/// 지금 도는 스웜의 구성(설정이 아니라 실제로 조립·활성화된 것). Tor 누출 시험이 "끄기 목록"을 구조로
/// 확인하는 데 씁니다.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetInspection {
    pub mode: NetMode,
    /// 조립한 전송(`crate::TRANSPORT_*`). Tor 스웜은 `arti`(와 `onion-listen`)뿐이어야 합니다.
    pub transports: Vec<String>,
    /// identify(수신 주소 광고).
    pub identify: bool,
    pub mdns: bool,
    pub autonat: bool,
    pub dcutr: bool,
    pub relay_client: bool,
    pub relay_server: bool,
    /// Kademlia 서버 역할(DHT 질의에 답하고 라우팅 표에 자기 주소가 실림). Tor 스웜은 꺼져야 합니다.
    pub kad_server: bool,
    /// 수신 중인 주소.
    pub listen_addrs: Vec<Multiaddr>,
    /// 다른 노드에 알리는 외부 주소(identify·Kademlia가 퍼뜨리는 주소).
    pub external_addrs: Vec<Multiaddr>,
}

pub(crate) type Reply<T> = oneshot::Sender<Result<T, NetError>>;

/// 핸들 → 스웜 작업 명령.
pub(crate) enum Command {
    Request {
        peer: PeerId,
        req: Box<YpRequest>,
        tx: Reply<YpResponse>,
    },
    /// 미디어 프레임 하나(기다리지 않음, 못 보내면 버림).
    SendMedia {
        peer: PeerId,
        frame: Vec<u8>,
    },
    Dial {
        peer: PeerId,
        addrs: Vec<Multiaddr>,
        /// 받는 쪽 역할로 겁니다(홀펀칭: 상대가 동시에 거는 동안 수신 소켓에서 상대 주소로 패킷을 보내 공유기 길을 엶).
        as_listener: bool,
        tx: Reply<()>,
    },
    /// PeerId를 모르는 주소(연락처 링크의 onion)로 연결하고, 답한 노드의 PeerId를 돌려줍니다.
    DialAddr {
        addr: Multiaddr,
        tx: Reply<PeerId>,
    },
    AddAddresses {
        peer: PeerId,
        addrs: Vec<Multiaddr>,
    },
    FindPeer {
        peer: PeerId,
        tx: Reply<Vec<Multiaddr>>,
    },
    PutRecord {
        key: Vec<u8>,
        value: Vec<u8>,
        tx: Reply<()>,
    },
    GetRecord {
        key: Vec<u8>,
        tx: Reply<Vec<Vec<u8>>>,
    },
    Subscribe(GossipTopic),
    Unsubscribe(GossipTopic),
    Publish {
        topic: GossipTopic,
        msg: Box<GossipMessage>,
        tx: Option<Reply<()>>,
    },
    ReportGossip {
        msg_id: MessageId,
        from: PeerId,
        verdict: GossipVerdict,
    },
    SetAllowed {
        topic: GossipTopic,
        peers: Vec<PeerId>,
    },
    Penalize {
        peer: PeerId,
    },
    Disconnect {
        peer: PeerId,
    },
    Status(oneshot::Sender<NetStatus>),
    Inspect(oneshot::Sender<NetInspection>),
    Peers(oneshot::Sender<Vec<ConnectedPeer>>),
    AddBootstrap(Vec<Multiaddr>),
    ListenAddrs {
        peer: PeerId,
        tx: Reply<Vec<Multiaddr>>,
    },
    /// 스웜을 내려놓고 이벤트 채널을 넘깁니다([`NetHandle::restart`]).
    Handover(HandoverRequest),
    Shutdown,
}

/// 재시작 때 옛 스웜 작업이 새 작업에 넘기는 것: 데몬으로 가는 이벤트 채널과 아직 못 보낸 이벤트.
pub(crate) struct Handover {
    pub events: mpsc::Sender<NetEvent>,
    pub queue: VecDeque<NetEvent>,
}

type ParkedSlot = Arc<Mutex<Option<Handover>>>;

const HANDOVER_PENDING: u8 = 0;
const HANDOVER_CLAIMED: u8 = 1;
const HANDOVER_CANCELLED: u8 = 2;

/// 옛 스웜 작업에 보내는 넘기기 요청. 작업이 받아들였는지와 재시작 쪽이 기다림을 그만뒀는지를 한 값으로 정해,
/// 둘이 엇갈려도 "재시작은 실패했다는데 옛 스웜은 내려감"이나 이벤트 채널을 잃는 일이 없게 합니다.
pub(crate) struct HandoverRequest {
    tx: oneshot::Sender<Handover>,
    state: Arc<AtomicU8>,
    /// 재시작 쪽이 사라져 넘기지 못한 채널을 둘 곳(다음 재시작이 이어 씀).
    fallback: ParkedSlot,
}

/// 받아들인 넘기기 요청. [`Self::deliver`]로 채널을 넘깁니다.
pub(crate) struct HandoverClaim {
    tx: oneshot::Sender<Handover>,
    fallback: ParkedSlot,
}

impl HandoverRequest {
    /// 스웜 작업 쪽: 넘기기로 정합니다. 재시작 쪽이 이미 그만뒀으면 `None`이고 작업은 그대로 돕니다.
    pub(crate) fn claim(self) -> Option<HandoverClaim> {
        self.state
            .compare_exchange(
                HANDOVER_PENDING,
                HANDOVER_CLAIMED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .ok()?;
        Some(HandoverClaim {
            tx: self.tx,
            fallback: self.fallback,
        })
    }
}

impl HandoverClaim {
    /// 채널을 넘깁니다. 재시작 쪽이 사라졌으면 다음 재시작을 위해 맡겨 둡니다(데몬 수신 루프가 끝나지 않게).
    pub(crate) fn deliver(self, h: Handover) {
        if let Err(h) = self.tx.send(h) {
            put_parked(&self.fallback, Some(h));
        }
    }
}

fn put_parked(slot: &Mutex<Option<Handover>>, h: Option<Handover>) -> Option<Handover> {
    match slot.lock() {
        Ok(mut p) => std::mem::replace(&mut *p, h),
        Err(e) => std::mem::replace(&mut *e.into_inner(), h),
    }
}

/// 핸들 쪽 대기 상한. 스웜 작업이 늦어도 호출자가 영원히 기다리지 않게 합니다.
#[derive(Clone, Copy, Debug)]
pub(crate) struct HandleLimits {
    pub request: Duration,
    pub dial: Duration,
    pub kad: Duration,
}

/// 네트워크 계층 핸들. 복제해서 여러 작업에서 씁니다.
///
/// [`Self::restart`]로 스웜을 바꿔 끼워도 모든 복제본이 새 스웜을 가리킵니다.
#[derive(Clone)]
pub struct NetHandle {
    shared: Arc<Shared>,
}

struct Shared {
    cur: RwLock<Current>,
    /// 재시작이 중간에 실패해 스웜 없이 남은 이벤트 채널. 다음 재시작이 이어 씁니다(데몬 수신 루프가 끝나지 않게).
    parked: ParkedSlot,
    restart: tokio::sync::Mutex<()>,
    shut: AtomicBool,
    /// 다시 띄워도 이어 세는 바이트 수(처음 설정의 것).
    traffic: crate::Traffic,
}

/// 다른 스웜과 함께 쓰는 Arti([`NetHandle::tor_share`]). 같은 Tor 클라이언트·디렉터리 캐시를 쓰므로 부트스트랩을 다시
/// 하지 않습니다(Arti 0.47은 같은 데이터 경로로 클라이언트를 둘 만들 수 없음, [`crate::transport`]).
#[derive(Clone)]
pub struct TorShare(#[cfg(feature = "tor")] pub(crate) Arc<crate::tor::ArtiShared>);

impl std::fmt::Debug for TorShare {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TorShare")
    }
}

#[derive(Clone)]
struct Current {
    cmd: mpsc::UnboundedSender<Command>,
    local: PeerId,
    last_status: Arc<Mutex<NetStatus>>,
    limits: HandleLimits,
    #[cfg(feature = "tor")]
    arti: Option<Arc<crate::tor::ArtiShared>>,
}

impl From<crate::Started> for Current {
    fn from(s: crate::Started) -> Self {
        Current {
            cmd: s.cmd,
            local: s.local,
            last_status: s.last_status,
            limits: s.limits,
            #[cfg(feature = "tor")]
            arti: s.arti,
        }
    }
}

/// 내부 확인용 짧은 대기 상한(상태 조회 등).
const LOCAL_CALL_LIMIT: Duration = Duration::from_secs(5);
/// identify로 상대 수신 주소를 받기까지 기다리는 상한.
const IDENTIFY_WAIT: Duration = Duration::from_secs(10);
/// 옛 스웜 작업이 내려놓기를 기다리는 상한.
const HANDOVER_WAIT: Duration = Duration::from_secs(10);

impl NetHandle {
    pub(crate) fn new(started: crate::Started, traffic: crate::Traffic) -> Self {
        NetHandle {
            shared: Arc::new(Shared {
                cur: RwLock::new(Current::from(started)),
                parked: Arc::new(Mutex::new(None)),
                restart: tokio::sync::Mutex::new(()),
                shut: AtomicBool::new(false),
                traffic,
            }),
        }
    }

    /// 이 스웜이 요청·응답으로 주고받은 바이트(다른 스웜에 넘겨 함께 셀 수 있음).
    pub fn traffic(&self) -> crate::Traffic {
        self.shared.traffic.clone()
    }

    fn cur(&self) -> Current {
        match self.shared.cur.read() {
            Ok(c) => c.clone(),
            Err(e) => e.into_inner().clone(),
        }
    }

    fn limits(&self) -> HandleLimits {
        self.cur().limits
    }

    /// 이 노드의 PeerId. [`Self::restart`]로 다른 키의 설정을 띄우면 달라집니다.
    pub fn local_peer_id(&self) -> PeerId {
        self.cur().local
    }

    fn send(&self, c: Command) -> bool {
        self.cur().cmd.send(c).is_ok()
    }

    async fn call<T>(
        &self,
        limit: Duration,
        make: impl FnOnce(Reply<T>) -> Command,
    ) -> Result<T, NetError> {
        let (tx, rx) = oneshot::channel();
        if !self.send(make(tx)) {
            return Err(NetError::Shutdown);
        }
        wait(limit, rx).await
    }

    /// 요청을 보내고 응답을 기다립니다. 연결이 없으면 알려진 주소(이전 `dial`·`add_addresses`·mDNS·DHT)로
    /// 연결합니다.
    pub async fn request(&self, peer: PeerId, req: YpRequest) -> Result<YpResponse, NetError> {
        self.call(self.limits().request, |tx| Command::Request {
            peer,
            req: Box::new(req),
            tx,
        })
        .await
    }

    /// 통화 미디어 프레임을 보냅니다(`/yangpa/media/1`). 기다리지 않고, 밀리면 오래된 프레임부터 버립니다. 이미 연결된
    /// 피어에게만 쓰세요(연결이 없으면 요청이 다이얼을 시도하다 실패함).
    pub fn send_media(&self, peer: PeerId, frame: Vec<u8>) {
        self.send(Command::SendMedia { peer, frame });
    }

    /// 주어진 주소(와 이미 아는 주소)로 연결합니다. 이미 연결돼 있으면 바로 성공합니다.
    pub async fn dial(&self, peer: PeerId, addrs: Vec<Multiaddr>) -> Result<(), NetError> {
        self.call(self.limits().dial, |tx| Command::Dial {
            peer,
            addrs,
            as_listener: false,
            tx,
        })
        .await
    }

    /// 홀펀칭의 받는 쪽: 상대가 같은 때 `dial`로 걸어 오는 동안, 수신 소켓에서 상대 주소로 패킷을 보내 이 기기 공유기에
    /// 길을 엽니다(QUIC). 상대 연결이 들어오면 성공합니다. 시각 맞추기는 부르는 쪽이 합니다(데몬은 Tor로 맞춤).
    pub async fn dial_as_listener(
        &self,
        peer: PeerId,
        addrs: Vec<Multiaddr>,
    ) -> Result<(), NetError> {
        self.call(self.limits().dial, |tx| Command::Dial {
            peer,
            addrs,
            as_listener: true,
            tx,
        })
        .await
    }

    /// 피어 주소를 기억해 둡니다(다음 요청이 자동 연결할 때 씀). 위치 기록·캐시에서 찾은 주소용.
    pub fn add_addresses(&self, peer: PeerId, addrs: Vec<Multiaddr>) {
        self.send(Command::AddAddresses { peer, addrs });
    }

    /// Kademlia로 피어 주소를 찾습니다.
    pub async fn find_peer(&self, peer: PeerId) -> Result<Vec<Multiaddr>, NetError> {
        self.call(self.limits().kad, |tx| Command::FindPeer { peer, tx })
            .await
    }

    /// DHT에 기록을 올립니다(위치 기록: 암호화된 값). 키 최대 64바이트, 값 최대 8KiB.
    pub async fn put_record(&self, key: Vec<u8>, value: Vec<u8>) -> Result<(), NetError> {
        validate_record(&key, Some(&value))?;
        self.call(self.limits().kad, |tx| Command::PutRecord {
            key,
            value,
            tx,
        })
        .await
    }

    /// DHT에서 기록을 찾습니다. 서로 다른 값이 여럿이면 모두 돌려줍니다.
    pub async fn get_record(&self, key: Vec<u8>) -> Result<Vec<Vec<u8>>, NetError> {
        validate_record(&key, None)?;
        self.call(self.limits().kad, |tx| Command::GetRecord { key, tx })
            .await
    }

    /// gossip 토픽을 구독합니다. 허용 목록([`Self::set_allowed_peers`])에 있는 피어의 메시지만 받습니다.
    pub fn subscribe(&self, topic: GossipTopic) {
        self.send(Command::Subscribe(topic));
    }

    pub fn unsubscribe(&self, topic: GossipTopic) {
        self.send(Command::Unsubscribe(topic));
    }

    /// gossip으로 보냅니다(결과를 기다리지 않음).
    pub fn publish(&self, topic: GossipTopic, msg: GossipMessage) {
        self.send(Command::Publish {
            topic,
            msg: Box::new(msg),
            tx: None,
        });
    }

    /// gossip으로 보내고 결과를 기다립니다. 받을 피어가 없으면 [`NetError::NoPeers`]이므로 직접 전송으로
    /// 대신할 수 있습니다.
    pub async fn try_publish(
        &self,
        topic: GossipTopic,
        msg: GossipMessage,
    ) -> Result<(), NetError> {
        self.call(LOCAL_CALL_LIMIT, |tx| Command::Publish {
            topic,
            msg: Box::new(msg),
            tx: Some(tx),
        })
        .await
    }

    /// [`NetEvent::Gossip`] 판정을 알립니다. `from`은 이벤트의 `from` 그대로.
    pub fn report_gossip(&self, msg_id: MessageId, from: PeerId, accept: GossipVerdict) {
        self.send(Command::ReportGossip {
            msg_id,
            from,
            verdict: accept,
        });
    }

    /// 닫힌 스웜: 이 토픽의 gossip을 주고받을 멤버 노드. 어느 토픽에도 없는 피어는 메시에 들이지 않습니다.
    pub fn set_allowed_peers(&self, topic: GossipTopic, peers: Vec<PeerId>) {
        self.send(Command::SetAllowed { topic, peers });
    }

    /// 위반한 피어를 끊고 일정 시간(`NetConfig::ban_duration`) 차단합니다. `reason`은 로그용 짧은 설명이며
    /// 메시지 내용을 넣지 마세요.
    pub fn penalize(&self, peer: PeerId, reason: &str) {
        info!(%peer, reason, "피어 차단");
        self.send(Command::Penalize { peer });
    }

    /// 이 피어와의 연결을 모두 닫습니다(차단하지 않아 다시 걸 수 있음). 망이 바뀌어 연결이 끊긴 것을 흉내 낼 때 씁니다.
    pub fn disconnect(&self, peer: PeerId) {
        self.send(Command::Disconnect { peer });
    }

    /// 현재 상태. 스웜 작업이 응답하지 않으면 마지막으로 알려진 상태를 돌려줍니다.
    pub async fn status(&self) -> NetStatus {
        let (tx, rx) = oneshot::channel();
        if self.send(Command::Status(tx))
            && let Ok(Ok(s)) = tokio::time::timeout(LOCAL_CALL_LIMIT, rx).await
        {
            return s;
        }
        self.last_status()
    }

    /// 스웜 작업이 마지막으로 계산한 상태(기다리지 않음). 길어도 1초 전 상태입니다.
    pub fn last_status(&self) -> NetStatus {
        let cur = self.cur();
        cur.last_status
            .lock()
            .map(|s| s.clone())
            .unwrap_or_else(|e| e.into_inner().clone())
    }

    /// 지금 스웜의 실제 구성(전송·켜진 동작·알리는 주소). 스웜 작업이 없으면 `None`.
    pub async fn inspect(&self) -> Option<NetInspection> {
        let (tx, rx) = oneshot::channel();
        if !self.send(Command::Inspect(tx)) {
            return None;
        }
        tokio::time::timeout(LOCAL_CALL_LIMIT, rx).await.ok()?.ok()
    }

    /// 지금 스웜이 쓰는 Arti의 경유 다이얼·앱 스트림 시도 수. 스웜을 다시 띄워도 데이터 경로가 같으면 이어서 셉니다.
    /// Arti가 없는 빌드·설정이면 0.
    pub fn tor_activity(&self) -> TorActivity {
        #[cfg(feature = "tor")]
        if let Some(a) = &self.cur().arti {
            return a.activity();
        }
        TorActivity::default()
    }

    /// Tor 브리지를 바꿉니다(Arti가 떠 있으면 바로, 함께 쓰는 스웜에도). 다시 띄울 때도 쓰려면
    /// [`NetConfig::tor_bridges`]도 같이 바꾸세요. Arti가 없는 스웜이면 아무것도 하지 않습니다.
    pub fn set_tor_bridges(&self, bridges: Option<crate::TorBridges>) -> Result<(), NetError> {
        #[cfg(feature = "tor")]
        if let Some(a) = &self.cur().arti {
            a.set_bridges(bridges)
                .map_err(|e| NetError::Tor(e.to_string()))?;
        }
        #[cfg(not(feature = "tor"))]
        let _ = bridges;
        Ok(())
    }

    /// Tor 부트스트랩 (진행률 0~100, 회로를 만들 수 있는지). Arti를 아직 시작하지 않았으면 `None`.
    pub fn tor_bootstrap(&self) -> Option<(u8, bool)> {
        #[cfg(feature = "tor")]
        if let Some(a) = &self.cur().arti {
            return a.progress().map(|p| (p, p == 100));
        }
        None
    }

    /// 이 스웜의 Arti를 다른 스웜이 함께 쓰도록 넘깁니다([`NetConfig::tor_share`]). Arti가 없으면 `None`.
    pub fn tor_share(&self) -> Option<TorShare> {
        #[cfg(feature = "tor")]
        {
            self.cur().arti.map(TorShare)
        }
        #[cfg(not(feature = "tor"))]
        {
            None
        }
    }

    /// libp2p 밖의 TCP 스트림을 Arti(Tor)로 엽니다(Tor로 도는 기기의 링크 미리보기). 이름(`host`)은 출구 노드가 풀어
    /// 로컬 DNS 조회가 없고, `xxx.onion`은 onion 서비스로 갑니다. 루프백·사설 주소·`localhost`는 Arti가 거절합니다.
    /// 같은 `isolation`끼리만 회로를 같이 쓰며 libp2p 연결과는 나눕니다. 시간 제한은 부르는 쪽이 둡니다.
    pub async fn tor_connect(
        &self,
        host: &str,
        port: u16,
        isolation: TorIsolation,
    ) -> Result<TorStream, NetError> {
        #[cfg(feature = "tor")]
        {
            let Some(arti) = self.cur().arti else {
                return Err(NetError::Tor("이 네트워크 설정에는 Arti가 없음".into()));
            };
            match arti.connect_stream(host, port, isolation).await {
                Ok(inner) => Ok(TorStream { inner }),
                // 오류 문구에 대상 주소가 들어갈 수 있어(메시지 내용) 종류만 남깁니다.
                Err(e) => Err(NetError::Tor(tor_error_kind(&e).into())),
            }
        }
        #[cfg(not(feature = "tor"))]
        {
            let _ = (host, port, isolation);
            Err(NetError::Tor("tor 기능 없이 빌드됨".into()))
        }
    }

    /// 상대가 identify로 알린 수신 주소(`/p2p` 없이). 연결된 피어면 identify를 받을 때까지 잠깐 기다리고,
    /// 연결이 없거나 identify를 쓰지 않는 스웜(Tor)이면 빈 목록입니다. 들어온 연결의 상대 주소를 기록해
    /// 재시작 뒤 다시 거는 데 씁니다.
    pub async fn peer_listen_addrs(&self, peer: PeerId) -> Vec<Multiaddr> {
        self.call(IDENTIFY_WAIT, |tx| Command::ListenAddrs { peer, tx })
            .await
            .unwrap_or_default()
    }

    /// 실행 중에 부트스트랩 노드를 더합니다(이 스웜에서 걸 수 없는 주소는 무시). 바로 연결을 시도합니다. 사용자
    /// 설정은 없어졌고 시험이 노드를 DHT·릴레이 노드에 이을 때 씁니다(데몬은 시작 옵션 `--bootstrap`만 봄).
    pub fn add_bootstrap(&self, addrs: Vec<Multiaddr>) {
        self.send(Command::AddBootstrap(addrs));
    }

    /// PeerId를 모르는 주소(연락처 링크의 onion)로 연결하고 답한 노드의 PeerId를 돌려줍니다. onion 주소는 서비스
    /// 키로 상대를 확인하므로, 그 주소에서 답한 노드가 그 기기입니다.
    pub async fn dial_addr(&self, addr: Multiaddr) -> Result<PeerId, NetError> {
        self.call(self.limits().dial, |tx| Command::DialAddr { addr, tx })
            .await
    }

    /// 이 기기의 onion 주소(`xxxx.onion`). 망 없이 키에서 정해지고 처음이면 키를 만듭니다(onion 서비스가 아직 뜨기
    /// 전에도). Arti가 없는 빌드·설정(직접 연결 스웜)이면 `None`.
    pub async fn device_onion_host(&self) -> Option<String> {
        #[cfg(feature = "tor")]
        {
            let arti = self.cur().arti?;
            match tokio::task::spawn_blocking(move || arti.device_onion_host()).await {
                Ok(Ok(h)) => return Some(h),
                Ok(Err(e)) => tracing::warn!(error = %e, "onion 주소를 읽지 못함"),
                Err(e) => tracing::warn!(error = %e, "onion 주소 작업 실패"),
            }
        }
        None
    }

    /// onion 서비스 키를 지웁니다(폐기된 기기의 데이터를 지울 때). 다음에 띄우는 서비스는 새 주소를 씁니다.
    pub fn forget_onion_keys(&self) {
        #[cfg(feature = "tor")]
        if let Some(a) = self.cur().arti {
            a.forget_onion_keys();
        }
    }

    /// 스웜을 새 설정으로 바꿔 띄웁니다(데몬은 실행 중에 스웜 종류를 바꾸지 않아 지금은 시험만 씀). 옛 연결은 모두
    /// 끊기고 끊김 이벤트가 가며, 부르는 쪽은 같은 이벤트 수신기로 새 스웜의 이벤트를 계속 받습니다. 구독·허용 목록·
    /// 주소록은 새 스웜에 없으므로 다시 알려야 합니다.
    ///
    /// 새 설정을 만들 수 없으면(Tor 기능 없음 등) 옛 스웜을 그대로 두고 오류입니다. 옛 스웜을 내린 뒤 새 스웜이
    /// 수신을 열지 못하면 네트워크가 꺼진 채로 오류를 돌려줍니다(옛 설정으로 몰래 돌아가지 않음). 그 뒤에도
    /// 다시 `restart`할 수 있습니다.
    pub async fn restart(&self, cfg: NetConfig) -> Result<(), NetError> {
        self.restart_until(cfg, tokio::time::sleep(HANDOVER_WAIT))
            .await
    }

    /// [`Self::restart`]. `give_up`이 끝날 때까지 옛 스웜 작업이 넘기기를 받아들이지 않으면 옛 스웜을 그대로 두고
    /// [`NetError::Timeout`]입니다. 이미 받아들였으면(스웜을 내려놓는 중) 끝까지 기다려 이어 씁니다.
    async fn restart_until(
        &self,
        mut cfg: NetConfig,
        give_up: impl Future<Output = ()>,
    ) -> Result<(), NetError> {
        cfg.traffic = self.shared.traffic.clone();
        let _guard = self.shared.restart.lock().await;
        if self.shared.shut.load(Ordering::Acquire) {
            return Err(NetError::Shutdown);
        }
        // 같은 데이터 경로의 Arti는 새 스웜이 이어 씁니다(`transport::build`). 새로 만들면 옛 클라이언트의 잠금 때문에
        // 부트스트랩이 멈춥니다.
        let prepared = crate::prepare(
            &cfg,
            #[cfg(feature = "tor")]
            self.cur().arti,
        )?;
        let handover = match put_parked(&self.shared.parked, None) {
            Some(h) => h,
            None => {
                let (tx, mut rx) = oneshot::channel();
                let state = Arc::new(AtomicU8::new(HANDOVER_PENDING));
                let req = HandoverRequest {
                    tx,
                    state: state.clone(),
                    fallback: self.shared.parked.clone(),
                };
                if !self.send(Command::Handover(req)) {
                    return Err(NetError::Shutdown);
                }
                let got = tokio::select! {
                    biased;
                    r = &mut rx => Some(r),
                    _ = give_up => None,
                };
                let got = match got {
                    Some(r) => r,
                    None => {
                        let cancelled = state
                            .compare_exchange(
                                HANDOVER_PENDING,
                                HANDOVER_CANCELLED,
                                Ordering::AcqRel,
                                Ordering::Acquire,
                            )
                            .is_ok();
                        if cancelled {
                            // 옛 스웜 작업은 이 요청을 받아들이지 않고 그대로 돕니다.
                            return Err(NetError::Timeout);
                        }
                        // 이미 넘기기로 정했으니 스웜을 내려놓는 대로 옵니다.
                        rx.await
                    }
                };
                match got {
                    Ok(h) => h,
                    Err(_) => {
                        // 작업이 넘기지 못하고 끝났으면 맡겨 둔 채널이 있을 수 있습니다.
                        match put_parked(&self.shared.parked, None) {
                            Some(h) => h,
                            None => return Err(NetError::Shutdown),
                        }
                    }
                }
            }
        };
        match crate::launch(prepared, &cfg, handover) {
            Ok(started) => {
                let next = Current::from(started);
                match self.shared.cur.write() {
                    Ok(mut c) => *c = next,
                    Err(e) => *e.into_inner() = next,
                }
                // 재시작 중에 종료를 불렀으면 새 스웜도 바로 닫습니다.
                if self.shared.shut.load(Ordering::Acquire) {
                    self.send(Command::Shutdown);
                }
                Ok(())
            }
            Err((e, handover)) => {
                if !self.shared.shut.load(Ordering::Acquire) {
                    put_parked(&self.shared.parked, Some(handover));
                }
                Err(e)
            }
        }
    }

    /// 연결된 피어 목록.
    pub async fn peers(&self) -> Vec<ConnectedPeer> {
        let (tx, rx) = oneshot::channel();
        if !self.send(Command::Peers(tx)) {
            return Vec::new();
        }
        tokio::time::timeout(LOCAL_CALL_LIMIT, rx)
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default()
    }

    /// 스웜 작업을 끝냅니다. 모든 연결이 닫히고 이후 호출은 [`NetError::Shutdown`]입니다.
    pub fn shutdown(&self) {
        self.shared.shut.store(true, Ordering::Release);
        self.send(Command::Shutdown);
        drop(put_parked(&self.shared.parked, None));
    }

    /// 스웜 작업이 끝났는지(재시작이 실패해 스웜 없이 남은 경우 포함).
    pub fn is_closed(&self) -> bool {
        self.cur().cmd.is_closed()
    }
}

/// Arti 오류의 짧은 분류(주소 없이).
#[cfg(feature = "tor")]
fn tor_error_kind(e: &crate::tor::TorTransportError) -> &'static str {
    use crate::tor::TorTransportError as E;
    match e {
        E::Arti(a) => {
            use arti_client::{ErrorKind as K, HasKind};
            match a.kind() {
                K::ForbiddenStreamTarget => "Tor로 연결할 수 없는 대상(로컬·사설 주소 등)",
                K::RemoteNetworkTimeout | K::TorNetworkTimeout => "Tor 연결 시간 초과",
                K::BootstrapRequired | K::TorAccessFailed => "Tor 망에 접속하지 못함",
                _ => "Tor 스트림을 열지 못함",
            }
        }
        E::Addr(_) => "Tor 주소 형식 오류",
        _ => "Tor 스트림을 열지 못함",
    }
}

async fn wait<T>(
    limit: Duration,
    rx: impl Future<Output = Result<Result<T, NetError>, oneshot::error::RecvError>>,
) -> Result<T, NetError> {
    match tokio::time::timeout(limit, rx).await {
        Ok(Ok(r)) => r,
        Ok(Err(_)) => Err(NetError::Shutdown),
        Err(_) => Err(NetError::Timeout),
    }
}

/// DHT 기록 크기 검사.
pub(crate) fn validate_record(key: &[u8], value: Option<&[u8]>) -> Result<(), NetError> {
    if key.is_empty() || key.len() > MAX_RECORD_KEY {
        return Err(NetError::InvalidInput("DHT 키 길이가 범위를 벗어남".into()));
    }
    if let Some(v) = value {
        if v.is_empty() {
            return Err(NetError::InvalidInput("DHT 값이 비어 있음".into()));
        }
        if v.len() > MAX_RECORD_VALUE {
            return Err(NetError::TooLarge {
                size: v.len(),
                max: MAX_RECORD_VALUE,
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_limits() {
        assert!(validate_record(&[], None).is_err());
        assert!(validate_record(&[0; 65], None).is_err());
        assert!(validate_record(&[0; 32], Some(&[])).is_err());
        assert_eq!(
            validate_record(&[0; 32], Some(&[0; MAX_RECORD_VALUE + 1])),
            Err(NetError::TooLarge {
                size: MAX_RECORD_VALUE + 1,
                max: MAX_RECORD_VALUE
            })
        );
        assert!(validate_record(&[0; 32], Some(&[1; 100])).is_ok());
    }

    #[test]
    fn shareable_addrs_exclude_direct_ips() {
        let me = PeerId::random();
        let relay = PeerId::random();
        let mut s = NetStatus::initial(me, NetMode::Normal);
        s.listen_addrs = vec!["/ip4/192.168.0.2/tcp/4001".parse().unwrap()];
        s.external_addrs = vec!["/ip4/8.8.8.8/tcp/4001".parse().unwrap()];
        let circuit: Multiaddr = format!("/ip4/1.2.3.4/tcp/4001/p2p/{relay}/p2p-circuit/p2p/{me}")
            .parse()
            .unwrap();
        // 회로 뒤에 IP가 붙은 이상한 주소는 걸러야 한다.
        let bad: Multiaddr = format!("/ip4/1.2.3.4/tcp/4001/p2p/{relay}/p2p-circuit/ip4/10.0.0.1")
            .parse()
            .unwrap();
        s.relay_addrs = vec![circuit.clone(), bad];
        assert_eq!(s.shareable_addrs(), vec![circuit]);
        let json = serde_json::to_string(&s).unwrap();
        let back: NetStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }

    #[tokio::test]
    async fn closed_handle_reports_shutdown() {
        let (tx, rx) = mpsc::unbounded_channel();
        drop(rx);
        let me = PeerId::random();
        let h = NetHandle::new(fake_started(tx, me), crate::Traffic::default());
        assert_eq!(
            h.request(PeerId::random(), YpRequest::Ping).await,
            Err(NetError::Shutdown)
        );
        assert_eq!(h.status().await.peer_id, me);
        assert!(h.peers().await.is_empty());
        assert!(h.is_closed());
    }

    #[tokio::test]
    async fn offline_handle_never_connects() {
        let cfg = local_cfg(std::path::Path::new("."));
        let me = cfg.keypair.public().to_peer_id();
        let (h, mut events) = crate::spawn_offline(&cfg);
        assert_eq!(h.local_peer_id(), me);
        assert!(h.is_closed());
        assert_eq!(
            h.request(PeerId::random(), YpRequest::Ping).await,
            Err(NetError::Shutdown)
        );
        assert!(h.status().await.listen_addrs.is_empty());
        assert!(events.recv().await.is_none());
    }

    /// 스웜 작업 대신 명령을 직접 받는 핸들.
    fn fake_handle() -> (NetHandle, mpsc::UnboundedReceiver<Command>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            NetHandle::new(
                fake_started(tx, PeerId::random()),
                crate::Traffic::default(),
            ),
            rx,
        )
    }

    fn fake_started(cmd: mpsc::UnboundedSender<Command>, me: PeerId) -> crate::Started {
        crate::Started {
            cmd,
            local: me,
            last_status: Arc::new(Mutex::new(NetStatus::initial(me, NetMode::Normal))),
            limits: HandleLimits {
                request: Duration::from_secs(1),
                dial: Duration::from_secs(1),
                kad: Duration::from_secs(1),
            },
            #[cfg(feature = "tor")]
            arti: None,
        }
    }

    fn local_cfg(dir: &std::path::Path) -> NetConfig {
        let mut cfg = NetConfig::new(
            NetMode::Normal,
            libp2p::identity::Keypair::generate_ed25519(),
            dir,
        );
        cfg.listen = vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()];
        cfg.mdns = false;
        cfg
    }

    fn handover_with(events: mpsc::Sender<NetEvent>, gone: PeerId) -> Handover {
        Handover {
            events,
            queue: VecDeque::from([NetEvent::PeerDisconnected(gone)]),
        }
    }

    async fn expect_disconnected(rx: &mut mpsc::Receiver<NetEvent>, gone: PeerId) {
        match tokio::time::timeout(Duration::from_secs(10), rx.recv()).await {
            Ok(Some(NetEvent::PeerDisconnected(p))) => assert_eq!(p, gone),
            _ => panic!("넘긴 이벤트가 같은 수신기로 와야 함"),
        }
    }

    /// 옛 스웜 작업이 받기 전에 기다림을 그만두면 시간 초과이고, 늦게 닿은 요청은 받아들이지 않아 옛 스웜이 그대로 돈다.
    #[tokio::test]
    async fn handover_given_up_before_claim_keeps_old_swarm() {
        let (h, mut cmd_rx) = fake_handle();
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            h.restart_until(local_cfg(dir.path()), std::future::ready(()))
                .await,
            Err(NetError::Timeout)
        );
        let Some(Command::Handover(req)) = cmd_rx.recv().await else {
            panic!("넘기기 요청");
        };
        assert!(req.claim().is_none(), "그만둔 요청을 받아들이면 안 됨");
        assert!(!h.is_closed(), "옛 스웜이 그대로여야 함");
    }

    /// 받아들인 뒤 넘기기가 기다림 상한보다 늦어도 재시작은 그 채널을 이어 쓰고, 쌓인 이벤트도 같은 수신기로 간다.
    #[tokio::test]
    async fn handover_claimed_then_slow_still_completes() {
        let (h, mut cmd_rx) = fake_handle();
        let (ev_tx, mut ev_rx) = mpsc::channel(8);
        let (claimed_tx, claimed_rx) = oneshot::channel::<()>();
        let (go_tx, go_rx) = oneshot::channel::<()>();
        let gone = PeerId::random();
        let fake = tokio::spawn(async move {
            let Some(Command::Handover(req)) = cmd_rx.recv().await else {
                panic!("넘기기 요청");
            };
            let claim = req.claim().expect("받아들임");
            claimed_tx.send(()).unwrap();
            // 재시작 쪽이 기다림 상한을 넘긴 뒤에야 넘깁니다(스웜을 내려놓는 데 오래 걸림).
            go_rx.await.unwrap();
            claim.deliver(handover_with(ev_tx, gone));
        });
        let give_up = async move {
            claimed_rx.await.unwrap();
            go_tx.send(()).unwrap();
        };
        let dir = tempfile::tempdir().unwrap();
        h.restart_until(local_cfg(dir.path()), give_up)
            .await
            .expect("재시작");
        fake.await.unwrap();
        expect_disconnected(&mut ev_rx, gone).await;
        assert!(!h.is_closed());
        h.shutdown();
    }

    /// 넘기기를 받아들인 뒤 재시작 쪽이 사라져도 이벤트 채널을 잃지 않고, 다음 재시작이 이어 쓴다.
    #[tokio::test]
    async fn handover_to_vanished_restart_is_kept_for_next() {
        let (h, mut cmd_rx) = fake_handle();
        let (ev_tx, mut ev_rx) = mpsc::channel(8);
        let gone = PeerId::random();
        let dir = tempfile::tempdir().unwrap();
        let h2 = h.clone();
        let cfg = local_cfg(dir.path());
        let restarting =
            tokio::spawn(async move { h2.restart_until(cfg, std::future::pending()).await });
        let Some(Command::Handover(req)) = cmd_rx.recv().await else {
            panic!("넘기기 요청");
        };
        let claim = req.claim().expect("받아들임");
        restarting.abort();
        assert!(restarting.await.is_err(), "재시작 작업이 취소돼야 함");
        claim.deliver(handover_with(ev_tx, gone));
        h.restart(local_cfg(dir.path()))
            .await
            .expect("맡겨 둔 채널로 재시작");
        expect_disconnected(&mut ev_rx, gone).await;
        h.shutdown();
    }
}
