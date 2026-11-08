//! 스웜 작업: 스웜 하나를 소유하고 명령·네트워크 이벤트·데몬 응답을 한 루프에서 처리합니다.
//!
//! 데몬 쪽이 느려도 스웜이 멈추지 않게, 이벤트는 넘치면 내부 대기열에 두고 대기열도 차면 요청은 `Busy`로,
//! gossip은 `Ignore`로 처리합니다.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::StreamExt;
use futures::future::BoxFuture;
use futures::stream::FuturesUnordered;
use libp2p::core::ConnectedPoint;
use libp2p::core::transport::ListenerId;
use libp2p::kad::store::RecordStore;
use libp2p::kad::{
    self, GetClosestPeersError, GetRecordOk, QueryId, QueryResult, Quorum, Record, RecordKey,
};
use libp2p::multiaddr::Protocol;
use libp2p::request_response::{self, OutboundFailure, OutboundRequestId, ResponseChannel};
use libp2p::swarm::dial_opts::{DialOpts, PeerCondition};
use libp2p::swarm::{ConnectionId, DialError, SwarmEvent};
use libp2p::{Multiaddr, PeerId, Swarm, autonat, gossipsub, identify, mdns, relay};
use tokio::sync::{mpsc, oneshot};
use tokio::time::MissedTickBehavior;
use tracing::{debug, info, warn};
use yangpa_core::wire::{RPC_PROTOCOL, WireErrorCode, YpRequest, YpResponse};

use crate::NetError;
use crate::addr;
use crate::behaviour::{YpBehaviour, YpBehaviourEvent};
use crate::codec;
use crate::config::{
    FeaturePlan, KAD_PROTOCOL, MAX_RECORD_KEY, MAX_RECORD_VALUE, MAX_STICKY_ADDRS,
    MAX_STICKY_RELAYS, NetConfig, NetMode, StickyRelay,
};
use crate::gossip::{self, GossipState};
use crate::handle::{
    Command, ConnectedPeer, Handover, NatStatus, NetEvent, NetInspection, NetStatus, RelayNode,
    Reply,
};

/// 데몬에 넘기지 못한 이벤트를 쌓아 둘 수 있는 상한.
const MAX_QUEUED_EVENTS: usize = 4096;
/// 데몬이 답하지 않은 수신 요청: 피어별·전체 개수 상한.
const MAX_INBOUND_PER_PEER: usize = 32;
const MAX_INBOUND_TOTAL: usize = 1024;
/// 데몬이 답하지 않은 수신 요청의 직렬화 바이트: 피어별·전체 상한(인증 전 노드도 요청을 보낼 수 있음).
const MAX_INBOUND_BYTES_PER_PEER: usize = 32 * 1024 * 1024;
const MAX_INBOUND_BYTES_TOTAL: usize = 256 * 1024 * 1024;
/// 피어 하나에 동시에 보내 둔(응답 대기 중) 요청 상한. 연결당 스트림 상한(behaviour)의 절반이라, 상대가
/// 같은 연결로 보내는 요청과 합쳐도 스트림이 모자라 입출력 오류가 나는 일이 드뭅니다.
pub(crate) const MAX_OUTBOUND_PER_PEER: usize = 32;
/// 바쁠 때 상대에게 알려 줄 재시도 대기.
pub(crate) const BUSY_RETRY_MS: u32 = 1000;
/// 한 번에 받아들일 주소 수(명령·identify·mDNS).
const MAX_ADDRS_PER_PEER: usize = 16;
/// 릴레이 예약 실패 후 같은 릴레이를 다시 쓰기까지.
const RELAY_BACKOFF: Duration = Duration::from_secs(60);
/// 예약하려고 건 릴레이의 연결·identify를 기다리는 시간(직접 연결 스웜의 다이얼 제한 20초보다 조금 길게). 이 동안 그 몫의
/// 자리를 순위가 낮은 후보로 채우지 않습니다.
const RELAY_DIAL_WAIT: Duration = Duration::from_secs(30);
/// 기억할 릴레이 후보 수.
const MAX_RELAY_CANDIDATES: usize = 64;
/// DHT 기록을 하나 찾은 뒤 다른 값을 더 기다리는 시간.
const GET_RECORD_GRACE: Duration = Duration::from_secs(2);
/// 피어 하나가 이 노드에 저장할 수 있는 DHT 기록 수(시간당).
const MAX_PUTS_PER_PEER_PER_HOUR: u32 = 64;
/// 연결이 하나도 없을 때 부트스트랩 노드에 다시 거는 간격.
const BOOTSTRAP_REDIAL: Duration = Duration::from_secs(60);
/// 동시에 거는 PeerId 모르는 주소 다이얼(연락처 링크) 상한.
const MAX_ADDR_DIALS: usize = 16;
/// mDNS로 찾은 피어에 자동 연결할 최대 동시 다이얼 수.
const MAX_MDNS_DIALS: usize = 16;

enum KadQuery {
    FindPeer {
        target: PeerId,
        tx: Reply<Vec<Multiaddr>>,
    },
    Put {
        tx: Reply<()>,
    },
    Get {
        tx: Reply<Vec<Vec<u8>>>,
        values: Vec<Vec<u8>>,
        first_found: Option<Instant>,
    },
}

#[derive(Default)]
struct PeerConns {
    /// 릴레이도 onion도 거치지 않은 연결(Tor 스웜의 출구 노드 경유 TCP 포함).
    direct: HashSet<ConnectionId>,
    relayed: HashSet<ConnectionId>,
    /// onion 서비스를 거친 연결(건 쪽은 onion 주소, 받은 쪽은 onion 수신).
    onion: HashSet<ConnectionId>,
    /// 우리가 직접 걸어서 성공한 주소(릴레이 후보 주소로 씀).
    dialed: Option<Multiaddr>,
    rtt: Option<Duration>,
}

#[derive(Default)]
struct RelayState {
    /// 릴레이 후보와 거는 주소(`/p2p` 없이).
    candidates: HashMap<PeerId, Multiaddr>,
    active: HashMap<ListenerId, PeerId>,
    backoff: HashMap<PeerId, Instant>,
    /// 지난번에 예약했던 릴레이(저장 순서, [`NetConfig::sticky_relays`]). 순위와 다시 걸기에 씁니다.
    sticky: Vec<PeerId>,
    /// 예약하려고 걸고 있는 릴레이와 기다릴 기한. 연결되지 않은 릴레이에 바로 `listen_on`하면, 다른 동작(kad 등)이 같은
    /// 노드에 걸고 있을 때 릴레이 클라이언트의 다이얼이 거절돼 예약이 곧 닫힙니다. 그래서 먼저 걸고, identify로 릴레이임을
    /// 확인한 뒤 예약합니다.
    dialing: HashMap<PeerId, Instant>,
}

struct InboundDone {
    peer: PeerId,
    len: usize,
    channel: ResponseChannel<YpResponse>,
    result: Result<Result<YpResponse, oneshot::error::RecvError>, tokio::time::error::Elapsed>,
}

/// 데몬이 답하지 않은 수신 요청 집계.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Pending {
    count: usize,
    bytes: usize,
}

enum Action<'a> {
    Command(Option<Command>),
    Swarm(Box<SwarmEvent<YpBehaviourEvent>>),
    Inbound(Box<InboundDone>),
    Permit(Option<mpsc::Permit<'a, NetEvent>>),
    Tick,
}

pub(crate) struct Task {
    swarm: Swarm<YpBehaviour>,
    local: PeerId,
    mode: NetMode,
    plan: FeaturePlan,
    request_timeout: Duration,
    ban_duration: Duration,
    relay_enabled: bool,
    /// 스웜에 실제로 들어간 전송 이름(구조 점검용).
    transports: Vec<&'static str>,
    #[cfg(feature = "tor")]
    arti: Option<Arc<crate::tor::ArtiShared>>,

    events: mpsc::Sender<NetEvent>,
    queue: VecDeque<NetEvent>,
    last_status: Arc<Mutex<NetStatus>>,
    emitted_status: Option<NetStatus>,

    gossip: GossipState,
    pending_requests: HashMap<OutboundRequestId, (PeerId, Reply<YpResponse>)>,
    media_out: crate::media::MediaOut,
    pending_media: HashMap<OutboundRequestId, PeerId>,
    outbound_per_peer: HashMap<PeerId, usize>,
    pending_dials: HashMap<PeerId, Vec<Reply<()>>>,
    /// 명시적 연결 요청의 주소. 듣기 포트를 재사용한 연결이 전송 단계에서 실패하면 새 포트로 한 번 더 겁니다.
    /// (macOS는 방금 닫은 연결과 같은 포트 쌍이 TIME_WAIT에 있으면 `EADDRINUSE`로 거절합니다.)
    new_port_retry: HashMap<PeerId, Vec<Multiaddr>>,
    dial_errors: HashMap<PeerId, NetError>,
    queries: HashMap<QueryId, KadQuery>,
    inbound: FuturesUnordered<BoxFuture<'static, InboundDone>>,
    inbound_per_peer: HashMap<PeerId, Pending>,
    inbound_bytes: usize,
    bans: HashMap<PeerId, Instant>,
    conns: HashMap<PeerId, PeerConns>,
    nat: NatStatus,
    relay: RelayState,
    bootstrap: Vec<(Option<PeerId>, Multiaddr)>,
    put_quota: HashMap<PeerId, (Instant, u32)>,
    last_bootstrap_dial: Instant,
    ticks: u64,
    /// 연결된 피어가 identify로 알린 수신 주소(데몬이 들어온 연결의 상대 주소를 기록할 때 씀).
    identified: HashMap<PeerId, Vec<Multiaddr>>,
    addr_waiters: HashMap<PeerId, Vec<Reply<Vec<Multiaddr>>>>,
    /// PeerId를 모르고 건 주소 다이얼(연락처 링크의 onion). 연결이 서면 답한 PeerId를 돌려줍니다.
    addr_dials: HashMap<ConnectionId, Reply<PeerId>>,
}

impl Task {
    pub fn new(
        swarm: Swarm<YpBehaviour>,
        cfg: &NetConfig,
        transports: Vec<&'static str>,
        #[cfg(feature = "tor")] arti: Option<Arc<crate::tor::ArtiShared>>,
        events: mpsc::Sender<NetEvent>,
        last_status: Arc<Mutex<NetStatus>>,
    ) -> Self {
        let plan = cfg.plan();
        Task {
            local: *swarm.local_peer_id(),
            swarm,
            mode: cfg.mode,
            plan,
            request_timeout: cfg.request_timeout,
            ban_duration: cfg.ban_duration,
            relay_enabled: plan.relay_client,
            transports,
            #[cfg(feature = "tor")]
            arti,
            events,
            queue: VecDeque::new(),
            last_status,
            emitted_status: None,
            gossip: GossipState::default(),
            pending_requests: HashMap::new(),
            media_out: Default::default(),
            pending_media: HashMap::new(),
            outbound_per_peer: HashMap::new(),
            pending_dials: HashMap::new(),
            new_port_retry: HashMap::new(),
            dial_errors: HashMap::new(),
            queries: HashMap::new(),
            inbound: FuturesUnordered::new(),
            inbound_per_peer: HashMap::new(),
            inbound_bytes: 0,
            bans: HashMap::new(),
            conns: HashMap::new(),
            nat: if cfg.mode == NetMode::Tor {
                NatStatus::Disabled
            } else {
                NatStatus::Unknown
            },
            relay: RelayState::default(),
            bootstrap: Vec::new(),
            put_quota: HashMap::new(),
            last_bootstrap_dial: Instant::now(),
            ticks: 0,
            identified: HashMap::new(),
            addr_waiters: HashMap::new(),
            addr_dials: HashMap::new(),
        }
    }

    /// 재시작 전 스웜이 넘긴, 아직 데몬에 못 보낸 이벤트를 이어 받습니다.
    pub fn adopt_queue(&mut self, queue: VecDeque<NetEvent>) {
        self.queue = queue;
    }

    /// 부트스트랩 노드를 등록하고 연결합니다.
    pub fn add_bootstrap(&mut self, addrs: Vec<Multiaddr>) {
        for a in addrs {
            let (base, peer) = addr::split_peer_id(&a);
            if peer == Some(self.local) || self.bootstrap.contains(&(peer, base.clone())) {
                continue;
            }
            if let Some(p) = peer {
                self.register_bootstrap_peer(p, base.clone());
            }
            self.bootstrap.push((peer, base));
        }
        self.dial_bootstrap();
        if self.bootstrap.iter().any(|(p, _)| p.is_some()) {
            let _ = self.swarm.behaviour_mut().kad.bootstrap();
        }
    }

    /// 지난번에 예약했던 릴레이를 순위에 올리고 먼저 겁니다(예약은 identify 뒤 [`Self::maintain_relays`]). Tor 스웜은
    /// 릴레이 클라이언트가 없어 아무것도 하지 않습니다.
    pub fn add_sticky_relays(&mut self, relays: Vec<StickyRelay>) {
        if !self.relay_enabled {
            return;
        }
        for r in relays.into_iter().take(MAX_STICKY_RELAYS) {
            let peer = r.peer_id;
            if peer == self.local
                || self.relay.sticky.contains(&peer)
                || self.bans.contains_key(&peer)
            {
                continue;
            }
            // 릴레이 자신의 주소만(회로·onion·`/p2p`가 붙은 것은 버림).
            let addrs: Vec<Multiaddr> = r
                .addrs
                .into_iter()
                .filter(|a| {
                    !addr::is_relayed(a) && !addr::is_onion(a) && addr::split_peer_id(a).1.is_none()
                })
                .take(MAX_STICKY_ADDRS)
                .collect();
            let Some(first) = addrs.first().cloned() else {
                continue;
            };
            self.relay.sticky.push(peer);
            for a in addrs {
                self.swarm.add_peer_address(peer, a);
            }
            self.add_relay_candidate(peer, first);
        }
        self.dial_sticky();
    }

    /// 예약하지 않은 지난번 릴레이에 겁니다(시작할 때·연결이 없다가 생겼을 때). 물러나 있어도 겁니다: 망이 없어 실패한
    /// 것일 수 있으므로.
    fn dial_sticky(&mut self) {
        let using: HashSet<PeerId> = self.relay.active.values().copied().collect();
        for peer in self.relay.sticky.clone() {
            if using.contains(&peer)
                || self.relay.dialing.contains_key(&peer)
                || self.bans.contains_key(&peer)
                || self.swarm.is_connected(&peer)
            {
                continue;
            }
            let Some(base) = self.relay.candidates.get(&peer).cloned() else {
                continue;
            };
            self.relay.backoff.remove(&peer);
            self.dial_relay(peer, base);
        }
    }

    /// 예약하려는 릴레이에 겁니다. 걸고 있는 동안(`dialing`) 그 몫의 자리를 남겨 둡니다. 걸 수 없으면 물러납니다.
    fn dial_relay(&mut self, peer: PeerId, base: Multiaddr) -> bool {
        let opts = DialOpts::peer_id(peer)
            .addresses(vec![base])
            .extend_addresses_through_behaviour()
            .build();
        match self.swarm.dial(opts) {
            // 이미 누가 걸고 있으면 그 연결을 기다립니다.
            Ok(()) | Err(DialError::DialPeerConditionFalse(_)) => {
                self.relay
                    .dialing
                    .insert(peer, Instant::now() + RELAY_DIAL_WAIT);
                true
            }
            Err(e) => {
                debug!(relay = %peer, error = %short_dial_error(&e), "릴레이 연결 실패");
                self.relay
                    .backoff
                    .insert(peer, Instant::now() + RELAY_BACKOFF);
                false
            }
        }
    }

    fn bootstrap_peers(&self) -> HashSet<PeerId> {
        self.bootstrap.iter().filter_map(|(p, _)| *p).collect()
    }

    fn register_bootstrap_peer(&mut self, peer: PeerId, base: Multiaddr) {
        self.swarm.add_peer_address(peer, base.clone());
        let b = self.swarm.behaviour_mut();
        b.kad.add_address(&peer, base.clone());
        if let Some(autonat) = b.autonat.as_mut() {
            autonat.add_server(peer, Some(base.clone()));
        }
        if self.relay_enabled && !addr::is_relayed(&base) {
            self.add_relay_candidate(peer, base);
        }
    }

    fn dial_bootstrap(&mut self) {
        self.last_bootstrap_dial = Instant::now();
        for (peer, a) in self.bootstrap.clone() {
            let res = match peer {
                Some(p) if self.swarm.is_connected(&p) => continue,
                Some(p) => self.swarm.dial(
                    DialOpts::peer_id(p)
                        .addresses(vec![a])
                        .extend_addresses_through_behaviour()
                        .build(),
                ),
                None => self.swarm.dial(a),
            };
            if let Err(e) = res {
                debug!(error = %short_dial_error(&e), "부트스트랩 노드 연결 실패");
            }
        }
    }

    pub async fn run(mut self, mut cmd_rx: mpsc::UnboundedReceiver<Command>) {
        let events_tx = self.events.clone();
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        self.update_status();
        let mut handover = None;
        loop {
            let has_queue = !self.queue.is_empty();
            let has_inbound = !self.inbound.is_empty();
            let action = tokio::select! {
                c = cmd_rx.recv() => Action::Command(c),
                ev = self.swarm.select_next_some() => Action::Swarm(Box::new(ev)),
                Some(done) = self.inbound.next(), if has_inbound => Action::Inbound(Box::new(done)),
                p = events_tx.reserve(), if has_queue => Action::Permit(p.ok()),
                _ = tick.tick() => Action::Tick,
            };
            match action {
                Action::Command(None) | Action::Command(Some(Command::Shutdown)) => break,
                Action::Command(Some(Command::Handover(req))) => {
                    // 재시작 쪽이 기다림을 그만둔 요청이면 그대로 돕니다.
                    if let Some(claim) = req.claim() {
                        handover = Some(claim);
                        break;
                    }
                }
                Action::Command(Some(c)) => self.on_command(c),
                Action::Swarm(ev) => self.on_swarm(*ev),
                Action::Inbound(done) => self.on_inbound_done(*done),
                Action::Permit(Some(permit)) => {
                    if let Some(ev) = self.queue.pop_front() {
                        permit.send(ev);
                    }
                }
                Action::Permit(None) => {
                    // 데몬이 수신기를 버렸습니다. 쌓인 이벤트는 거절 처리합니다.
                    while let Some(ev) = self.queue.pop_front() {
                        self.drop_event(ev);
                    }
                }
                Action::Tick => self.on_tick(),
            }
        }
        drop(events_tx);
        let Some(tx) = handover else {
            self.fail_all();
            info!("네트워크 작업 종료");
            return;
        };
        // 데몬이 피어별 상태를 정리하게 끊김을 알리고, 스웜(수신 소켓)을 먼저 놓은 뒤 채널을 넘깁니다.
        let connected: Vec<PeerId> = self.swarm.connected_peers().copied().collect();
        for p in connected {
            self.emit(NetEvent::PeerDisconnected(p));
        }
        self.fail_all();
        let Task {
            swarm,
            events,
            queue,
            ..
        } = self;
        drop(swarm);
        info!("네트워크 작업을 새 설정에 넘김");
        tx.deliver(Handover { events, queue });
    }

    fn fail_all(&mut self) {
        self.outbound_per_peer.clear();
        self.media_out = Default::default();
        self.pending_media.clear();
        for (_, (_, tx)) in self.pending_requests.drain() {
            let _ = tx.send(Err(NetError::Shutdown));
        }
        for (_, txs) in self.pending_dials.drain() {
            for tx in txs {
                let _ = tx.send(Err(NetError::Shutdown));
            }
        }
        for (_, txs) in self.addr_waiters.drain() {
            for tx in txs {
                let _ = tx.send(Err(NetError::Shutdown));
            }
        }
        for (_, q) in self.queries.drain() {
            match q {
                KadQuery::FindPeer { tx, .. } => drop(tx.send(Err(NetError::Shutdown))),
                KadQuery::Put { tx } => drop(tx.send(Err(NetError::Shutdown))),
                KadQuery::Get { tx, .. } => drop(tx.send(Err(NetError::Shutdown))),
            }
        }
    }

    // ---- 이벤트 전달 ----

    fn emit(&mut self, ev: NetEvent) {
        if self.queue.is_empty() {
            match self.events.try_send(ev) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(ev)) => self.queue.push_back(ev),
                Err(mpsc::error::TrySendError::Closed(ev)) => self.drop_event(ev),
            }
        } else if self.queue.len() < MAX_QUEUED_EVENTS {
            self.queue.push_back(ev);
        } else {
            warn!("이벤트 대기열이 가득 참");
            self.drop_event(ev);
        }
    }

    /// 전달하지 못한 이벤트 정리. 요청은 `reply`가 버려지면서 오류 응답이 가고, gossip은 무시 판정을 알립니다.
    fn drop_event(&mut self, ev: NetEvent) {
        if let NetEvent::Gossip { from, msg_id, .. } = ev {
            self.swarm
                .behaviour_mut()
                .gossip
                .report_message_validation_result(
                    &msg_id,
                    &from,
                    gossipsub::MessageAcceptance::Ignore,
                );
        }
    }

    // ---- 명령 ----

    /// 지금 스웜의 구성(전송·동작·주소)을 그대로 읽어 냅니다. 설정이 아니라 살아 있는 스웜을 봅니다.
    fn inspect(&self) -> NetInspection {
        let b = self.swarm.behaviour();
        NetInspection {
            mode: self.mode,
            transports: self.transports.iter().map(|t| t.to_string()).collect(),
            identify: b.identify.is_enabled(),
            mdns: b.mdns.is_enabled(),
            autonat: b.autonat.is_enabled(),
            dcutr: b.dcutr.is_enabled(),
            relay_client: b.relay_client.is_enabled(),
            relay_server: b.relay_server.is_enabled(),
            kad_server: b.kad.mode() == kad::Mode::Server,
            listen_addrs: self.swarm.listeners().cloned().collect(),
            external_addrs: self.swarm.external_addresses().cloned().collect(),
        }
    }

    fn on_command(&mut self, c: Command) {
        match c {
            Command::Request { peer, req, tx } => self.cmd_request(peer, *req, tx),
            Command::SendMedia { peer, frame } => {
                if peer != self.local
                    && self.swarm.is_connected(&peer)
                    && !self.bans.contains_key(&peer)
                {
                    let batch = self.media_out.push(peer, frame);
                    self.send_media_batch(peer, batch);
                }
            }
            Command::Dial {
                peer,
                addrs,
                as_listener,
                tx,
            } => self.cmd_dial(peer, addrs, as_listener, tx),
            Command::DialAddr { addr, tx } => self.cmd_dial_addr(addr, tx),
            Command::AddAddresses { peer, addrs } => {
                if peer != self.local {
                    for a in addrs
                        .into_iter()
                        .take(MAX_ADDRS_PER_PEER)
                        .filter_map(|a| normalize_addr(a, &peer))
                    {
                        self.swarm.add_peer_address(peer, a);
                    }
                }
            }
            Command::FindPeer { peer, tx } => {
                let id = self.swarm.behaviour_mut().kad.get_closest_peers(peer);
                self.queries
                    .insert(id, KadQuery::FindPeer { target: peer, tx });
            }
            Command::PutRecord { key, value, tx } => {
                if let Err(e) = crate::handle::validate_record(&key, Some(&value)) {
                    let _ = tx.send(Err(e));
                    return;
                }
                let record = Record::new(RecordKey::new(&key), value);
                match self
                    .swarm
                    .behaviour_mut()
                    .kad
                    .put_record(record, Quorum::One)
                {
                    Ok(id) => {
                        self.queries.insert(id, KadQuery::Put { tx });
                    }
                    Err(e) => {
                        let _ = tx.send(Err(NetError::Kad(e.to_string())));
                    }
                }
            }
            Command::GetRecord { key, tx } => {
                if let Err(e) = crate::handle::validate_record(&key, None) {
                    let _ = tx.send(Err(e));
                    return;
                }
                let id = self
                    .swarm
                    .behaviour_mut()
                    .kad
                    .get_record(RecordKey::new(&key));
                self.queries.insert(
                    id,
                    KadQuery::Get {
                        tx,
                        values: Vec::new(),
                        first_found: None,
                    },
                );
            }
            Command::Subscribe(t) => {
                self.gossip.subscribe(t);
                let topic = gossip::topic(&t);
                let g = &mut self.swarm.behaviour_mut().gossip;
                if let Err(e) = g.subscribe(&topic) {
                    warn!(error = ?e, "gossip 구독 실패");
                }
                if let Err(e) = g.set_topic_params(topic, gossip::topic_score_params()) {
                    warn!(error = e, "gossip 토픽 점수 설정 실패");
                }
                self.refresh_all_gossip_scores();
            }
            Command::Unsubscribe(t) => {
                self.gossip.unsubscribe(&t);
                self.swarm
                    .behaviour_mut()
                    .gossip
                    .unsubscribe(&gossip::topic(&t));
                self.refresh_all_gossip_scores();
            }
            Command::Publish { topic: t, msg, tx } => {
                // 닫힌 스웜: 허용 목록이 없으면 누가 토픽을 구독했든 걸러 낼 기준이 없으므로 보내지 않습니다.
                let res = if self.gossip.has_allowed(&t) {
                    gossip::encode_outgoing(&t.space(), &msg)
                } else {
                    Err(NetError::InvalidInput(
                        "허용 피어 목록이 없는 토픽에는 gossip을 보낼 수 없음".into(),
                    ))
                };
                let res = res.and_then(|data| {
                    match self
                        .swarm
                        .behaviour_mut()
                        .gossip
                        .publish(gossip::topic(&t), data)
                    {
                        Ok(_) | Err(gossipsub::PublishError::Duplicate) => Ok(()),
                        Err(e) => Err(map_publish_error(e)),
                    }
                });
                match tx {
                    Some(tx) => drop(tx.send(res)),
                    None => {
                        if let Err(e) = res {
                            debug!(error = %e, "gossip 보내기 실패");
                        }
                    }
                }
            }
            Command::ReportGossip {
                msg_id,
                from,
                verdict,
            } => {
                self.swarm
                    .behaviour_mut()
                    .gossip
                    .report_message_validation_result(&msg_id, &from, verdict.into());
            }
            Command::SetAllowed { topic, peers } => {
                // 멤버 여부뿐 아니라 '허용되지 않은 토픽 구독' 여부도 바뀌므로 연결된 피어 전부를 다시 셉니다.
                self.gossip.set_allowed(topic, peers);
                self.refresh_all_gossip_scores();
            }
            Command::Penalize { peer } => self.cmd_penalize(peer),
            Command::Disconnect { peer } => {
                let _ = self.swarm.disconnect_peer_id(peer);
            }
            Command::Status(tx) => {
                let s = self.update_status();
                let _ = tx.send(s);
            }
            Command::Inspect(tx) => {
                let _ = tx.send(self.inspect());
            }
            Command::Peers(tx) => {
                let list = self
                    .conns
                    .iter()
                    .map(|(p, c)| ConnectedPeer {
                        peer_id: *p,
                        connections: (c.direct.len() + c.relayed.len() + c.onion.len()) as u32,
                        relayed_only: c.direct.is_empty() && c.onion.is_empty(),
                        rtt_ms: c.rtt.map(|d| d.as_millis().min(u32::MAX as u128) as u32),
                    })
                    .collect();
                let _ = tx.send(list);
            }
            Command::AddBootstrap(addrs) => {
                let usable: Vec<Multiaddr> = addrs
                    .into_iter()
                    .take(MAX_ADDRS_PER_PEER)
                    .filter(|a| crate::config::bootstrap_usable(self.mode, &self.plan, a))
                    .collect();
                self.add_bootstrap(usable);
            }
            Command::ListenAddrs { peer, tx } => {
                if let Some(a) = self.identified.get(&peer) {
                    let _ = tx.send(Ok(a.clone()));
                } else if !self.plan.identify || !self.swarm.is_connected(&peer) {
                    let _ = tx.send(Ok(Vec::new()));
                } else {
                    let waiters = self.addr_waiters.entry(peer).or_default();
                    waiters.retain(|w| !w.is_closed());
                    waiters.push(tx);
                }
            }
            Command::Handover(_) | Command::Shutdown => {}
        }
    }

    fn cmd_request(&mut self, peer: PeerId, req: YpRequest, tx: Reply<YpResponse>) {
        if peer == self.local {
            let _ = tx.send(Err(NetError::InvalidInput(
                "자기 자신에게 요청할 수 없음".into(),
            )));
            return;
        }
        if self.bans.contains_key(&peer) {
            let _ = tx.send(Err(NetError::Banned));
            return;
        }
        let in_flight = self.outbound_per_peer.entry(peer).or_default();
        if *in_flight >= MAX_OUTBOUND_PER_PEER {
            let _ = tx.send(Err(NetError::Busy));
            return;
        }
        *in_flight += 1;
        let id = self
            .swarm
            .behaviour_mut()
            .rpc
            .send_request(&peer, codec::WireRequest::outbound(req));
        self.pending_requests.insert(id, (peer, tx));
    }

    fn cmd_dial(&mut self, peer: PeerId, addrs: Vec<Multiaddr>, as_listener: bool, tx: Reply<()>) {
        if peer == self.local {
            let _ = tx.send(Err(NetError::InvalidInput(
                "자기 자신에게 연결할 수 없음".into(),
            )));
            return;
        }
        if self.bans.contains_key(&peer) {
            let _ = tx.send(Err(NetError::Banned));
            return;
        }
        let addrs: Vec<Multiaddr> = addrs
            .into_iter()
            .take(MAX_ADDRS_PER_PEER)
            .filter_map(|a| normalize_addr(a, &peer))
            .collect();
        for a in &addrs {
            self.swarm.add_peer_address(peer, a.clone());
        }
        if self.swarm.is_connected(&peer) {
            let _ = tx.send(Ok(()));
            return;
        }
        let opts = if as_listener {
            // 홀펀칭은 받은 주소에만(QUIC 수신 소켓에서 보냄). 이미 걸고 있어도 따로 시도합니다.
            DialOpts::peer_id(peer)
                .condition(PeerCondition::Always)
                .addresses(addrs.clone())
                .override_role()
                .build()
        } else {
            DialOpts::peer_id(peer)
                .addresses(addrs.clone())
                .extend_addresses_through_behaviour()
                .build()
        };
        match self.swarm.dial(opts) {
            Ok(()) | Err(DialError::DialPeerConditionFalse(_)) => {
                if !addrs.is_empty() && !as_listener {
                    self.new_port_retry.insert(peer, addrs);
                }
                self.pending_dials.entry(peer).or_default().push(tx)
            }
            Err(e) => {
                let _ = tx.send(Err(map_dial_error(&e)));
            }
        }
    }

    /// PeerId를 모르는 주소로 겁니다. onion 주소는 서비스 키가 곧 주소라, 그 주소에서 답한 노드가 링크를 만든
    /// 기기입니다(그 PeerId로 인증을 이어 갑니다).
    fn cmd_dial_addr(&mut self, addr: Multiaddr, tx: Reply<PeerId>) {
        let (base, peer) = addr::split_peer_id(&addr);
        if peer == Some(self.local) {
            let _ = tx.send(Err(NetError::InvalidInput(
                "자기 자신에게 연결할 수 없음".into(),
            )));
            return;
        }
        self.addr_dials.retain(|_, t| !t.is_closed());
        if self.addr_dials.len() >= MAX_ADDR_DIALS {
            let _ = tx.send(Err(NetError::Busy));
            return;
        }
        let opts = DialOpts::unknown_peer_id().address(base).build();
        let id = opts.connection_id();
        match self.swarm.dial(opts) {
            Ok(()) => {
                self.addr_dials.insert(id, tx);
            }
            Err(e) => {
                let _ = tx.send(Err(map_dial_error(&e)));
            }
        }
    }

    /// 포트를 재사용한 연결이 전송 단계에서 실패했으면 새 로컬 포트로 한 번 다시 겁니다. 다시 걸었으면 `true`.
    ///
    /// 다른 노드가 답한 경우(`WrongPeerId`)도 한 번 다시 겁니다: 주소록에 남은 상대의 옛 주소(재시작 전 포트)를 이제
    /// 다른 노드가 쓰면, 그쪽 연결이 먼저 맺어져 받은 주소로는 닿을 수 있는데도 걸기가 통째로 실패합니다. 다시 걸 때는
    /// 받은 주소만 씁니다.
    fn retry_dial_with_new_port(&mut self, peer: PeerId, error: &DialError) -> bool {
        if !matches!(
            error,
            DialError::Transport(_) | DialError::WrongPeerId { .. }
        ) || self.swarm.is_connected(&peer)
        {
            return false;
        }
        let Some(addrs) = self.new_port_retry.remove(&peer) else {
            return false;
        };
        if !self.pending_dials.contains_key(&peer) {
            return false;
        }
        let opts = DialOpts::peer_id(peer)
            .addresses(addrs)
            .allocate_new_port()
            .build();
        match self.swarm.dial(opts) {
            Ok(()) => {
                debug!(%peer, "새 포트로 다시 연결 시도");
                true
            }
            Err(_) => false,
        }
    }

    fn cmd_penalize(&mut self, peer: PeerId) {
        if peer == self.local {
            return;
        }
        self.bans.insert(peer, Instant::now() + self.ban_duration);
        let b = self.swarm.behaviour_mut();
        // 차단 목록에 넣으면 기존 연결이 닫히고 새 연결도 거절됩니다.
        b.blocked.block_peer(peer);
        b.kad.remove_peer(&peer);
        self.relay.candidates.remove(&peer);
        self.relay.dialing.remove(&peer);
        self.new_port_retry.remove(&peer);
        if let Some(txs) = self.pending_dials.remove(&peer) {
            for tx in txs {
                let _ = tx.send(Err(NetError::Banned));
            }
        }
    }

    // ---- 스웜 이벤트 ----

    fn on_swarm(&mut self, ev: SwarmEvent<YpBehaviourEvent>) {
        match ev {
            SwarmEvent::Behaviour(b) => self.on_behaviour(b),
            SwarmEvent::ConnectionEstablished {
                peer_id,
                connection_id,
                endpoint,
                num_established,
                ..
            } => {
                let relayed = endpoint.is_relayed();
                let onion = is_onion_endpoint(&endpoint);
                let entry = self.conns.entry(peer_id).or_default();
                if relayed {
                    entry.relayed.insert(connection_id);
                } else if onion {
                    entry.onion.insert(connection_id);
                } else {
                    entry.direct.insert(connection_id);
                    if endpoint.is_dialer() {
                        entry.dialed = Some(endpoint.get_remote_address().clone());
                    }
                }
                self.dial_errors.remove(&peer_id);
                self.new_port_retry.remove(&peer_id);
                if let Some(tx) = self.addr_dials.remove(&connection_id) {
                    let _ = tx.send(Ok(peer_id));
                }
                if let Some(txs) = self.pending_dials.remove(&peer_id) {
                    for tx in txs {
                        let _ = tx.send(Ok(()));
                    }
                }
                // 주소에 PeerId 없이 등록한 부트스트랩 노드.
                if endpoint.is_dialer() {
                    let remote = endpoint.get_remote_address().clone();
                    if let Some(i) = self
                        .bootstrap
                        .iter()
                        .position(|(p, a)| p.is_none() && *a == remote)
                    {
                        self.bootstrap[i].0 = Some(peer_id);
                        self.register_bootstrap_peer(peer_id, remote);
                        let _ = self.swarm.behaviour_mut().kad.bootstrap();
                    }
                }
                // 연결이 하나도 없다가 생김(시작·망이 돌아옴): 예약이 모자라면 지난번 릴레이부터 다시 겁니다.
                if num_established.get() == 1
                    && self.swarm.network_info().num_peers() == 1
                    && self.relay.active.len() < self.relay_target()
                {
                    self.dial_sticky();
                }
                if num_established.get() == 1 {
                    self.refresh_gossip_score(&peer_id);
                    self.emit(NetEvent::PeerConnected(peer_id));
                }
            }
            SwarmEvent::ConnectionClosed {
                peer_id,
                connection_id,
                num_established,
                ..
            } => {
                if let Some(c) = self.conns.get_mut(&peer_id) {
                    c.direct.remove(&connection_id);
                    c.relayed.remove(&connection_id);
                    c.onion.remove(&connection_id);
                    if c.direct.is_empty() && c.relayed.is_empty() && c.onion.is_empty() {
                        self.conns.remove(&peer_id);
                    }
                }
                if num_established == 0 {
                    self.conns.remove(&peer_id);
                    self.identified.remove(&peer_id);
                    self.media_out.forget(&peer_id);
                    if let Some(txs) = self.addr_waiters.remove(&peer_id) {
                        for tx in txs {
                            let _ = tx.send(Ok(Vec::new()));
                        }
                    }
                    self.emit(NetEvent::PeerDisconnected(peer_id));
                }
            }
            SwarmEvent::OutgoingConnectionError {
                connection_id,
                peer_id: None,
                error,
            } => {
                if let Some(tx) = self.addr_dials.remove(&connection_id) {
                    debug!(error = %short_dial_error(&error), "주소 연결 실패");
                    let _ = tx.send(Err(map_dial_error(&error)));
                }
            }
            SwarmEvent::OutgoingConnectionError {
                peer_id: Some(peer),
                error,
                ..
            } => {
                if self.retry_dial_with_new_port(peer, &error) {
                    return;
                }
                // 예약하려고 걸던 릴레이에 닿지 못함: 자리를 풀고 다음 후보로.
                if !self.swarm.is_connected(&peer) && self.relay.dialing.remove(&peer).is_some() {
                    self.relay
                        .backoff
                        .insert(peer, Instant::now() + RELAY_BACKOFF);
                    self.maintain_relays();
                }
                let e = map_dial_error(&error);
                debug!(%peer, error = %short_dial_error(&error), "연결 실패");
                if self.swarm.is_connected(&peer) {
                    if let Some(txs) = self.pending_dials.remove(&peer) {
                        for tx in txs {
                            let _ = tx.send(Ok(()));
                        }
                    }
                } else {
                    if let Some(txs) = self.pending_dials.remove(&peer) {
                        for tx in txs {
                            let _ = tx.send(Err(e.clone()));
                        }
                    }
                    if self.dial_errors.len() > 1024 {
                        self.dial_errors.clear();
                    }
                    self.dial_errors.insert(peer, e);
                }
            }
            SwarmEvent::NewListenAddr { address, .. } => {
                debug!(
                    relayed = addr::is_relayed(&address),
                    onion = addr::is_onion(&address),
                    "수신 주소 추가"
                );
            }
            SwarmEvent::ListenerClosed {
                listener_id,
                reason,
                ..
            } => {
                if let Some(relay_peer) = self.relay.active.remove(&listener_id) {
                    debug!(%relay_peer, ok = reason.is_ok(), "릴레이 예약 종료");
                    self.relay
                        .backoff
                        .insert(relay_peer, Instant::now() + RELAY_BACKOFF);
                    // identify로 알게 된 릴레이가 예약을 거절했거나 사라졌으면 후보에서 뺍니다(다시 알려 오면 됨).
                    // 부트스트랩 노드와 지난번 릴레이는 남깁니다: 갱신 시간 초과(잠자기 뒤 등)도 `Err`로 오므로.
                    let keep = self.bootstrap_peers().contains(&relay_peer)
                        || self.relay.sticky.contains(&relay_peer);
                    if reason.is_err() && !keep {
                        self.relay.candidates.remove(&relay_peer);
                    }
                } else if let Err(e) = reason {
                    warn!(error = %e, "수신 중단");
                }
            }
            SwarmEvent::ListenerError { error, .. } => debug!(error = %error, "수신 오류"),
            SwarmEvent::IncomingConnectionError { error, .. } => {
                debug!(error = %error, "들어온 연결 실패")
            }
            _ => {}
        }
    }

    fn on_behaviour(&mut self, ev: YpBehaviourEvent) {
        match ev {
            YpBehaviourEvent::Rpc(e) => self.on_rpc(e),
            YpBehaviourEvent::Media(e) => self.on_media(e),
            YpBehaviourEvent::Gossip(e) => self.on_gossip(e),
            YpBehaviourEvent::Kad(e) => self.on_kad(e),
            YpBehaviourEvent::Ping(e) => {
                if let Ok(rtt) = e.result
                    && let Some(c) = self.conns.get_mut(&e.peer)
                {
                    c.rtt = Some(rtt);
                }
            }
            YpBehaviourEvent::Identify(e) => self.on_identify(e),
            YpBehaviourEvent::RelayClient(e) => match e {
                relay::client::Event::ReservationReqAccepted {
                    relay_peer_id,
                    renewal,
                    ..
                } => {
                    if !renewal {
                        info!(relay = %relay_peer_id, "릴레이 예약 성립");
                    }
                }
                other => debug!(event = ?other, "릴레이 클라이언트"),
            },
            YpBehaviourEvent::RelayServer(e) => debug!(event = ?e, "릴레이 서버"),
            YpBehaviourEvent::Dcutr(e) => match e.result {
                Ok(_) => info!(peer = %e.remote_peer_id, "홀펀칭으로 직접 연결"),
                Err(err) => debug!(peer = %e.remote_peer_id, error = %err, "홀펀칭 실패"),
            },
            YpBehaviourEvent::Autonat(e) => {
                if let autonat::Event::StatusChanged { new, .. } = e {
                    self.nat = match new {
                        autonat::NatStatus::Public(_) => NatStatus::Public,
                        autonat::NatStatus::Private => NatStatus::Private,
                        autonat::NatStatus::Unknown => NatStatus::Unknown,
                    };
                    info!(nat = ?self.nat, "NAT 상태 변경");
                    self.maintain_relays();
                }
            }
            YpBehaviourEvent::Mdns(e) => self.on_mdns(e),
            // 열린 외부 주소는 동작이 스웜에 직접 확인시킵니다. 주소(IP)는 로그에 남기지 않습니다.
            YpBehaviourEvent::Upnp(e) => match e {
                libp2p::upnp::Event::NewExternalAddr { .. } => info!("UPnP로 수신 포트를 엶"),
                libp2p::upnp::Event::ExpiredExternalAddr { .. } => debug!("UPnP 포트 갱신 실패"),
                libp2p::upnp::Event::GatewayNotFound => debug!("UPnP 공유기를 찾지 못함"),
                libp2p::upnp::Event::NonRoutableGateway => {
                    debug!("UPnP 공유기가 공인망에 바로 있지 않음")
                }
            },
            YpBehaviourEvent::Limits(v)
            | YpBehaviourEvent::Memory(v)
            | YpBehaviourEvent::Blocked(v)
            | YpBehaviourEvent::Addrbook(v) => match v {},
        }
    }

    // ---- 요청-응답 ----

    fn on_rpc(&mut self, ev: request_response::Event<codec::WireRequest, YpResponse>) {
        match ev {
            request_response::Event::Message { peer, message, .. } => match message {
                request_response::Message::Request {
                    request, channel, ..
                } => self.on_inbound_request(peer, request.req, request.len, channel),
                request_response::Message::Response {
                    request_id,
                    response,
                } => {
                    if let Some(tx) = self.take_pending_request(&request_id) {
                        let _ = tx.send(Ok(response));
                    }
                }
            },
            request_response::Event::OutboundFailure {
                peer,
                request_id,
                error,
                ..
            } => {
                if let Some(tx) = self.take_pending_request(&request_id) {
                    let e = match error {
                        OutboundFailure::DialFailure => self
                            .dial_errors
                            .remove(&peer)
                            .unwrap_or_else(|| NetError::Dial("연결할 수 없음".into())),
                        OutboundFailure::Timeout => NetError::Timeout,
                        OutboundFailure::ConnectionClosed => NetError::ConnectionClosed,
                        OutboundFailure::UnsupportedProtocols => NetError::Unsupported,
                        OutboundFailure::Io(e) => match codec::as_too_large(&e) {
                            Some((size, max)) => NetError::TooLarge { size, max },
                            None => NetError::Io(e.kind().to_string()),
                        },
                    };
                    let _ = tx.send(Err(e));
                }
            }
            request_response::Event::InboundFailure { peer, error, .. } => {
                debug!(%peer, error = %error, "수신 요청 처리 실패");
            }
            request_response::Event::ResponseSent { .. } => {}
        }
    }

    // ---- 통화 미디어 ----

    fn send_media_batch(&mut self, peer: PeerId, batch: Option<Vec<Vec<u8>>>) {
        let Some(batch) = batch else { return };
        let id = self.swarm.behaviour_mut().media.send_request(&peer, batch);
        self.pending_media.insert(id, peer);
    }

    fn media_done(&mut self, id: &OutboundRequestId) {
        if let Some(peer) = self.pending_media.remove(id) {
            let next = self.media_out.done(&peer);
            if self.swarm.is_connected(&peer) {
                self.send_media_batch(peer, next);
            } else {
                self.media_out.forget(&peer);
            }
        }
    }

    fn on_media(&mut self, ev: request_response::Event<Vec<Vec<u8>>, ()>) {
        match ev {
            request_response::Event::Message { peer, message, .. } => match message {
                request_response::Message::Request {
                    request, channel, ..
                } => {
                    let _ = self.swarm.behaviour_mut().media.send_response(channel, ());
                    // 데몬 쪽이 밀려 있으면 버립니다(대기열을 RPC·gossip 이벤트에 남김).
                    if !request.is_empty() && self.queue.len() < MAX_QUEUED_EVENTS / 4 {
                        self.emit(NetEvent::Media {
                            peer,
                            frames: request,
                        });
                    }
                }
                request_response::Message::Response { request_id, .. } => {
                    self.media_done(&request_id)
                }
            },
            request_response::Event::OutboundFailure { request_id, .. } => {
                self.media_done(&request_id)
            }
            request_response::Event::InboundFailure { .. }
            | request_response::Event::ResponseSent { .. } => {}
        }
    }

    /// 응답·실패가 온 요청을 꺼내고 피어별 동시 요청 수를 줄입니다.
    fn take_pending_request(&mut self, id: &OutboundRequestId) -> Option<Reply<YpResponse>> {
        let (peer, tx) = self.pending_requests.remove(id)?;
        if let Some(n) = self.outbound_per_peer.get_mut(&peer) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                self.outbound_per_peer.remove(&peer);
            }
        }
        Some(tx)
    }

    fn on_inbound_request(
        &mut self,
        peer: PeerId,
        req: codec::Incoming,
        len: usize,
        channel: ResponseChannel<YpResponse>,
    ) {
        // 살아 있는지 확인하는 요청은 데몬까지 가지 않고 바로 답합니다.
        if matches!(req.plain(), Some(YpRequest::Ping)) {
            let _ = self
                .swarm
                .behaviour_mut()
                .rpc
                .send_response(channel, YpResponse::Pong);
            return;
        }
        let per_peer = self
            .inbound_per_peer
            .get(&peer)
            .copied()
            .unwrap_or_default();
        if per_peer.count >= MAX_INBOUND_PER_PEER
            || per_peer.bytes.saturating_add(len) > MAX_INBOUND_BYTES_PER_PEER
            || self.inbound.len() >= MAX_INBOUND_TOTAL
            || self.inbound_bytes.saturating_add(len) > MAX_INBOUND_BYTES_TOTAL
            || self.queue.len() >= MAX_QUEUED_EVENTS
            || self.events.is_closed()
        {
            let _ = self.swarm.behaviour_mut().rpc.send_response(
                channel,
                YpResponse::Busy {
                    retry_after_ms: BUSY_RETRY_MS,
                },
            );
            return;
        }
        let (tx, rx) = oneshot::channel();
        let entry = self.inbound_per_peer.entry(peer).or_default();
        entry.count += 1;
        entry.bytes += len;
        self.inbound_bytes += len;
        let deadline = daemon_deadline(self.request_timeout);
        self.inbound.push(Box::pin(async move {
            let result = tokio::time::timeout(deadline, rx).await;
            InboundDone {
                peer,
                len,
                channel,
                result,
            }
        }));
        self.emit(NetEvent::InboundRequest {
            peer,
            req,
            reply: tx,
        });
    }

    fn on_inbound_done(&mut self, done: InboundDone) {
        if let Some(p) = self.inbound_per_peer.get_mut(&done.peer) {
            p.count = p.count.saturating_sub(1);
            p.bytes = p.bytes.saturating_sub(done.len);
            if p.count == 0 {
                self.inbound_per_peer.remove(&done.peer);
            }
        }
        self.inbound_bytes = self.inbound_bytes.saturating_sub(done.len);
        let resp = match done.result {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => YpResponse::error(WireErrorCode::Internal, "요청을 처리하지 못함"),
            Err(_) => YpResponse::error(WireErrorCode::Internal, "처리 시간 초과"),
        };
        // 상대가 이미 끊었으면 보낼 곳이 없습니다.
        let _ = self
            .swarm
            .behaviour_mut()
            .rpc
            .send_response(done.channel, resp);
    }

    // ---- gossip ----

    fn on_gossip(&mut self, ev: gossipsub::Event) {
        match ev {
            gossipsub::Event::Message {
                propagation_source,
                message_id,
                message,
            } => match self.gossip.check(&propagation_source, &message) {
                Ok((topic, msg)) => self.emit(NetEvent::Gossip {
                    from: propagation_source,
                    msg_id: message_id,
                    topic,
                    msg,
                }),
                Err(acceptance) => {
                    if acceptance == gossipsub::MessageAcceptance::Reject {
                        debug!(peer = %propagation_source, "gossip 메시지 거부");
                    }
                    self.swarm
                        .behaviour_mut()
                        .gossip
                        .report_message_validation_result(
                            &message_id,
                            &propagation_source,
                            acceptance,
                        );
                }
            },
            gossipsub::Event::Subscribed { peer_id, .. }
            | gossipsub::Event::Unsubscribed { peer_id, .. } => self.refresh_gossip_score(&peer_id),
            gossipsub::Event::SlowPeer { peer_id, .. } => {
                debug!(peer = %peer_id, "gossip 느린 피어")
            }
            _ => {}
        }
    }

    /// 닫힌 스웜: 어느 공간에도 없거나 허용되지 않은 공간의 토픽을 구독한 피어는 점수를 크게 깎아
    /// 메시·전달·보내기 대상에서 뺍니다.
    fn refresh_gossip_score(&mut self, peer: &PeerId) {
        if !self.swarm.is_connected(peer) {
            return;
        }
        let g = &self.swarm.behaviour().gossip;
        let topics: Vec<gossipsub::TopicHash> = g
            .all_peers()
            .find(|(p, _)| *p == peer)
            .map(|(_, t)| t.into_iter().cloned().collect())
            .unwrap_or_default();
        let score = self.gossip.score_for(peer, &topics);
        self.swarm
            .behaviour_mut()
            .gossip
            .set_application_score(peer, score);
    }

    fn refresh_all_gossip_scores(&mut self) {
        let gossip = &mut self.gossip;
        let scores: Vec<(PeerId, f64)> = self
            .swarm
            .behaviour()
            .gossip
            .all_peers()
            .map(|(p, topics)| (*p, gossip.score_for(p, topics)))
            .collect();
        let g = &mut self.swarm.behaviour_mut().gossip;
        for (p, score) in scores {
            g.set_application_score(&p, score);
        }
    }

    // ---- Kademlia ----

    fn on_kad(&mut self, ev: kad::Event) {
        match ev {
            kad::Event::InboundRequest {
                request:
                    kad::InboundRequest::PutRecord {
                        source,
                        record: Some(record),
                        ..
                    },
            } => self.on_inbound_put(source, record),
            kad::Event::OutboundQueryProgressed {
                id, result, step, ..
            } => self.on_query(id, result, step.last),
            kad::Event::RoutingUpdated {
                peer, addresses, ..
            } => {
                // onion으로 건 연결의 주소는 라우팅 표에 두지 않습니다(DHT 질의 답으로 새지 않게).
                let onion: Vec<Multiaddr> = addresses
                    .iter()
                    .filter(|a| addr::is_onion(a))
                    .cloned()
                    .collect();
                for a in onion {
                    self.swarm.behaviour_mut().kad.remove_address(&peer, &a);
                }
            }
            _ => {}
        }
    }

    fn on_inbound_put(&mut self, source: PeerId, mut record: Record) {
        let key_len = record.key.as_ref().len();
        if key_len == 0
            || key_len > MAX_RECORD_KEY
            || record.value.is_empty()
            || record.value.len() > MAX_RECORD_VALUE
        {
            debug!(peer = %source, "규격에 맞지 않는 DHT 기록 무시");
            return;
        }
        let now = Instant::now();
        let quota = self.put_quota.entry(source).or_insert((now, 0));
        if now.duration_since(quota.0) > Duration::from_secs(3600) {
            *quota = (now, 0);
        }
        if quota.1 >= MAX_PUTS_PER_PEER_PER_HOUR {
            debug!(peer = %source, "DHT 기록 저장 한도 초과");
            return;
        }
        quota.1 += 1;
        // 올린 노드를 저장해 두면 날마다 바뀌는 위치 기록 키가 같은 PeerId로 이어지므로 지웁니다.
        record.publisher = None;
        if let Err(e) = self.swarm.behaviour_mut().kad.store_mut().put(record) {
            debug!(error = %e, "DHT 기록 저장 실패");
        }
    }

    fn on_query(&mut self, id: QueryId, result: QueryResult, last: bool) {
        match result {
            QueryResult::GetClosestPeers(r) => {
                let Some(KadQuery::FindPeer { target, tx }) = self.queries.remove(&id) else {
                    return;
                };
                let peers = match r {
                    Ok(ok) => ok.peers,
                    Err(GetClosestPeersError::Timeout { peers, .. }) => peers,
                };
                let mut found: Vec<Multiaddr> = Vec::new();
                for p in peers.into_iter().filter(|p| p.peer_id == target) {
                    for a in p
                        .addrs
                        .into_iter()
                        .take(MAX_ADDRS_PER_PEER)
                        .filter_map(|a| normalize_addr(a, &target))
                    {
                        if !found.contains(&a) {
                            found.push(a);
                        }
                    }
                }
                for a in &found {
                    self.swarm.add_peer_address(target, a.clone());
                }
                for a in self.swarm.behaviour_mut().addrbook.get(&target) {
                    let (base, _) = addr::split_peer_id(&a);
                    if !found.contains(&base) && !found.contains(&a) {
                        found.push(base);
                    }
                }
                let _ = tx.send(if found.is_empty() {
                    Err(NetError::NotFound)
                } else {
                    Ok(found)
                });
            }
            QueryResult::PutRecord(r) => {
                let Some(KadQuery::Put { tx }) = self.queries.remove(&id) else {
                    return;
                };
                let res = match r {
                    Ok(_) => Ok(()),
                    Err(kad::PutRecordError::Timeout { .. }) => Err(NetError::Timeout),
                    Err(e) => Err(NetError::Kad(e.to_string())),
                };
                let _ = tx.send(res);
            }
            QueryResult::GetRecord(r) => {
                if let Some(KadQuery::Get {
                    values,
                    first_found,
                    ..
                }) = self.queries.get_mut(&id)
                    && let Ok(GetRecordOk::FoundRecord(rec)) = &r
                {
                    let v = &rec.record.value;
                    if !v.is_empty()
                        && v.len() <= MAX_RECORD_VALUE
                        && !values.contains(v)
                        && values.len() < 64
                    {
                        values.push(v.clone());
                    }
                    first_found.get_or_insert_with(Instant::now);
                }
                if last {
                    let Some(KadQuery::Get { tx, values, .. }) = self.queries.remove(&id) else {
                        return;
                    };
                    let res = if !values.is_empty() {
                        Ok(values)
                    } else {
                        match r {
                            Err(kad::GetRecordError::Timeout { .. }) => Err(NetError::Timeout),
                            _ => Err(NetError::NotFound),
                        }
                    };
                    let _ = tx.send(res);
                }
            }
            _ => {}
        }
    }

    // ---- identify · mDNS ----

    fn on_identify(&mut self, ev: identify::Event) {
        let identify::Event::Received { peer_id, info, .. } = ev else {
            return;
        };
        let supports_kad = info.protocols.iter().any(|p| p.as_ref() == KAD_PROTOCOL);
        let yangpa = info.protocols.iter().any(|p| p.as_ref() == RPC_PROTOCOL);
        let is_relay = info.protocols.contains(&relay::HOP_PROTOCOL_NAME);
        let addrs = heard_listen_addrs(info.listen_addrs, &peer_id);
        for a in &addrs {
            self.swarm.add_peer_address(peer_id, a.clone());
            if supports_kad {
                self.swarm
                    .behaviour_mut()
                    .kad
                    .add_address(&peer_id, a.clone());
            }
        }
        if self.swarm.is_connected(&peer_id) {
            if let Some(txs) = self.addr_waiters.remove(&peer_id) {
                for tx in txs {
                    let _ = tx.send(Ok(addrs.clone()));
                }
            }
            self.identified.insert(peer_id, addrs.clone());
        }
        // 예약하려고 걸던 릴레이가 연결됨: 자리 맡기를 끝냅니다. 더는 릴레이가 아니면 지난번 릴레이에서도 잊습니다.
        if self.relay.dialing.remove(&peer_id).is_some() {
            if self.relay.sticky.contains(&peer_id) {
                self.relay.backoff.remove(&peer_id);
            }
            if !is_relay {
                self.relay.sticky.retain(|p| *p != peer_id);
                if !self.bootstrap_peers().contains(&peer_id) {
                    self.relay.candidates.remove(&peer_id);
                }
                self.maintain_relays();
            }
        }
        // 릴레이는 YANGPA 노드와 설정으로 받은 노드(부트스트랩·지난번 릴레이)만 씁니다. 아무 libp2p 릴레이(IPFS 등
        // 공개망 노드)나 쓰지 않습니다.
        let known =
            self.bootstrap_peers().contains(&peer_id) || self.relay.sticky.contains(&peer_id);
        if is_relay && (yangpa || known) && self.relay_enabled && !self.bans.contains_key(&peer_id)
        {
            let dialed = self.conns.get(&peer_id).and_then(|c| c.dialed.clone());
            let candidate = dialed.or_else(|| addrs.iter().find(|a| !addr::is_relayed(a)).cloned());
            if let Some(a) = candidate {
                self.add_relay_candidate(peer_id, addr::split_peer_id(&a).0);
                self.maintain_relays();
            }
        }
    }

    fn on_mdns(&mut self, ev: mdns::Event) {
        let mdns::Event::Discovered(list) = ev else {
            return;
        };
        let mut to_dial = HashSet::new();
        for (peer, a) in list.into_iter().take(256) {
            if peer == self.local || self.bans.contains_key(&peer) {
                continue;
            }
            self.swarm.add_peer_address(peer, a);
            if !self.swarm.is_connected(&peer) && to_dial.len() < MAX_MDNS_DIALS {
                to_dial.insert(peer);
            }
        }
        for peer in to_dial {
            if let Err(e) = self.swarm.dial(peer) {
                debug!(%peer, error = %short_dial_error(&e), "mDNS 피어 연결 실패");
            }
        }
    }

    // ---- 릴레이 ----

    fn add_relay_candidate(&mut self, peer: PeerId, base: Multiaddr) {
        if peer == self.local {
            return;
        }
        if self.relay.candidates.len() >= MAX_RELAY_CANDIDATES
            && !self.relay.candidates.contains_key(&peer)
        {
            // identify로 알게 된 후보로 가득 차도 부트스트랩 노드는 들어가야 합니다.
            let mine = self.bootstrap_peers();
            if !mine.contains(&peer) {
                return;
            }
            let using: HashSet<PeerId> = self.relay.active.values().copied().collect();
            let Some(evict) = self
                .relay
                .candidates
                .keys()
                .find(|p| !mine.contains(p) && !using.contains(p))
                .copied()
            else {
                return;
            };
            self.relay.candidates.remove(&evict);
        }
        self.relay.candidates.insert(peer, base);
    }

    /// 지금 쓸 수 있는 후보인지(물러나 있거나 차단되지 않은 것).
    fn relay_usable(&self, peer: &PeerId) -> bool {
        !self.relay.backoff.contains_key(peer) && !self.bans.contains_key(peer)
    }

    fn relay_target(&self) -> usize {
        if !self.relay_enabled {
            0
        } else if self.nat == NatStatus::Private {
            // NAT 뒤에서는 릴레이 하나가 끊겨도 연락이 끊기지 않게 둘.
            2
        } else {
            // 공인 주소가 있어도 링크·위치 기록에는 릴레이 경유 주소만 넣으므로 하나는 유지합니다.
            1
        }
    }

    fn maintain_relays(&mut self) {
        let target = self.relay_target();
        let now = Instant::now();
        self.relay.backoff.retain(|_, until| *until > now);
        let mine = self.bootstrap_peers();
        let sticky = self.relay.sticky.clone();
        let rank = |p: &PeerId| relay_rank(p, &mine, &sticky);
        // 줄일 때(공인 주소)는 순위가 가장 낮은 예약부터 내려놓습니다.
        while self.relay.active.len() > target {
            let Some(id) = self
                .relay
                .active
                .iter()
                .max_by_key(|(_, p)| rank(p))
                .map(|(id, _)| *id)
            else {
                break;
            };
            self.relay.active.remove(&id);
            self.swarm.remove_listener(id);
        }
        let mut left = target.saturating_sub(self.relay.active.len());
        if left == 0 {
            return;
        }
        let using: HashSet<PeerId> = self.relay.active.values().copied().collect();
        let mut picks: Vec<(PeerId, Multiaddr)> = self
            .relay
            .candidates
            .iter()
            .filter(|(p, _)| {
                !using.contains(p) && !self.relay.dialing.contains_key(p) && self.relay_usable(p)
            })
            .map(|(p, a)| (*p, a.clone()))
            .collect();
        // 링크에 들어간 중계 주소가 오래 가도록 순위대로 고릅니다. 같은 순위끼리는 HashMap 순서 그대로 둡니다(모든
        // 사용자가 같은 릴레이로 몰리지 않게).
        picks.sort_by_key(|(p, _)| rank(p));
        let dialing: Vec<_> = self.relay.dialing.keys().map(&rank).collect();
        for (peer, base) in picks {
            // 걸고 있는, 순위가 더 높은 릴레이의 몫은 남겨 둡니다.
            if left <= dialing.iter().filter(|w| **w < rank(&peer)).count() {
                break;
            }
            left -= 1;
            if !self.conns.get(&peer).is_some_and(|c| !c.direct.is_empty()) {
                // 연결부터 합니다(예약은 identify 뒤).
                if !self.dial_relay(peer, base) {
                    left += 1;
                }
                continue;
            }
            let Ok(with_peer) = base.with_p2p(peer) else {
                self.relay.candidates.remove(&peer);
                left += 1;
                continue;
            };
            match self.swarm.listen_on(with_peer.with(Protocol::P2pCircuit)) {
                Ok(id) => {
                    debug!(relay = %peer, "릴레이 예약 요청");
                    self.relay.active.insert(id, peer);
                }
                Err(e) => {
                    debug!(relay = %peer, error = %e, "릴레이 수신 실패");
                    self.relay.backoff.insert(peer, now + RELAY_BACKOFF);
                    left += 1;
                }
            }
        }
    }

    // ---- 주기 작업 ----

    fn on_tick(&mut self) {
        self.ticks += 1;
        let now = Instant::now();

        let expired: Vec<PeerId> = self
            .bans
            .iter()
            .filter(|(_, until)| **until <= now)
            .map(|(p, _)| *p)
            .collect();
        for p in expired {
            self.bans.remove(&p);
            self.swarm.behaviour_mut().blocked.unblock_peer(p);
        }

        // 기록을 하나 찾았으면 쿼리 전체(최대 수십 초)를 기다리지 않고 잠깐 뒤 끝냅니다.
        let done: Vec<QueryId> = self
            .queries
            .iter()
            .filter_map(|(id, q)| match q {
                KadQuery::Get {
                    first_found: Some(t),
                    ..
                } if now.duration_since(*t) >= GET_RECORD_GRACE => Some(*id),
                _ => None,
            })
            .collect();
        for id in done {
            if let Some(mut q) = self.swarm.behaviour_mut().kad.query_mut(&id) {
                q.finish();
            }
        }

        if self.ticks.is_multiple_of(5) {
            self.relay.dialing.retain(|_, until| *until > now);
            self.maintain_relays();
        }
        if self.swarm.network_info().num_peers() == 0
            && !self.bootstrap.is_empty()
            && now.duration_since(self.last_bootstrap_dial) >= BOOTSTRAP_REDIAL
        {
            self.dial_bootstrap();
        }
        if self.ticks.is_multiple_of(600) {
            self.put_quota
                .retain(|_, (t, _)| now.duration_since(*t) <= Duration::from_secs(3600));
        }

        let status = self.update_status();
        if self.emitted_status.as_ref() != Some(&status) {
            self.emitted_status = Some(status.clone());
            self.emit(NetEvent::Status(status));
        }
    }

    fn update_status(&mut self) -> NetStatus {
        let status = self.compute_status();
        match self.last_status.lock() {
            Ok(mut s) => *s = status.clone(),
            Err(e) => *e.into_inner() = status.clone(),
        }
        status
    }

    fn compute_status(&self) -> NetStatus {
        let mut listen_addrs: Vec<Multiaddr> = Vec::new();
        for a in self.swarm.listeners() {
            if !listen_addrs.contains(a) {
                listen_addrs.push(a.clone());
            }
        }
        let relay_addrs: Vec<Multiaddr> = listen_addrs
            .iter()
            .filter(|a| addr::is_relayed(a))
            .map(|a| match a.iter().last() {
                Some(Protocol::P2p(_)) => a.clone(),
                _ => a.clone().with(Protocol::P2p(self.local)),
            })
            .filter(addr::is_link_safe_relay_addr)
            .collect();
        let mine = self.bootstrap_peers();
        // 친구·링크에는 앞 몇 개만 가므로, 고른 순서의 릴레이마다 걸기 쉬운 주소를 번갈아 둡니다.
        let mut order: Vec<PeerId> = self.relay.active.values().copied().collect();
        order.sort_by_key(|p| (relay_rank(p, &mine, &self.relay.sticky), *p));
        order.dedup();
        let relay_addrs = addr::interleave_relay_addrs(relay_addrs, &order);
        let onion_addr = listen_addrs.iter().find(|a| addr::is_onion(a)).cloned();
        let external_addrs = self
            .swarm
            .external_addresses()
            .filter(|a| !addr::is_relayed(a))
            .cloned()
            .collect();
        let info = self.swarm.network_info();
        let mut relays: Vec<RelayNode> = Vec::new();
        for peer_id in relay_addrs.iter().filter_map(addr::relay_peer) {
            if !relays.iter().any(|r| r.peer_id == peer_id) {
                relays.push(RelayNode { peer_id });
            }
        }
        #[cfg(feature = "tor")]
        let tor_bootstrap = self.arti.as_ref().and_then(|a| a.progress());
        #[cfg(not(feature = "tor"))]
        let tor_bootstrap = None;
        NetStatus {
            peer_id: self.local,
            mode: self.mode,
            listen_addrs,
            external_addrs,
            relay_addrs,
            onion_addr,
            nat: self.nat,
            connections: info.connection_counters().num_established(),
            peers: info.num_peers() as u32,
            tor_bootstrap,
            relays,
        }
    }
}

/// 데몬이 수신 요청에 답해야 하는 시간. 요청 스트림 전체(읽기 + 응답 쓰기)의 제한 시간보다 짧아야
/// 시간을 넘겼을 때 상대가 맨 시간 초과 대신 오류 응답을 받습니다.
fn daemon_deadline(request_timeout: Duration) -> Duration {
    request_timeout
        .saturating_sub(request_timeout / 4)
        .max(Duration::from_millis(100))
}

/// 릴레이 순위(작을수록 먼저): 부트스트랩 노드(`--bootstrap`, 한도 없는 중계), 지난번에 예약했던 릴레이(저장 순서:
/// 친구가 기억한 내 중계 주소), 나머지(identify로 알게 된 YANGPA 릴레이).
fn relay_rank(p: &PeerId, mine: &HashSet<PeerId>, sticky: &[PeerId]) -> (bool, usize) {
    (
        !mine.contains(p),
        sticky.iter().position(|s| s == p).unwrap_or(usize::MAX),
    )
}

/// onion 서비스를 거친 연결인지: 건 쪽은 onion 주소로 걸었고, 받은 쪽은 onion 수신(로컬 주소가 onion)으로 받았습니다.
fn is_onion_endpoint(endpoint: &ConnectedPoint) -> bool {
    match endpoint {
        ConnectedPoint::Dialer { address, .. } => addr::is_onion(address),
        ConnectedPoint::Listener { local_addr, .. } => addr::is_onion(local_addr),
    }
}

/// identify로 들은 상대 수신 주소(상한까지). 상대는 주소를 순서 없이(HashSet) 알려 주므로, 인터페이스가 많아
/// 상한에서 잘릴 때 IP가 바뀌어도 닿는 onion·릴레이 주소가 빠지지 않게 앞에 둡니다(이 순서로 기록하고 다시 겁니다).
fn heard_listen_addrs(mut listen: Vec<Multiaddr>, peer: &PeerId) -> Vec<Multiaddr> {
    // 릴레이 경유·onion 먼저, 그 안에서는 우리가 걸 수 있는 주소부터(한도 안에 못 거는 전송만 남지 않게).
    listen.sort_by_key(|a| {
        (
            !(addr::is_onion(a) || addr::is_relayed(a)),
            addr::relay_dial_rank(a),
        )
    });
    listen
        .into_iter()
        .take(MAX_ADDRS_PER_PEER)
        .filter_map(|a| normalize_addr(a, peer))
        .collect()
}

/// 주소 끝의 `/p2p/<id>`가 다른 피어를 가리키면 버리고, 맞으면 뗀 주소를 돌려줍니다.
fn normalize_addr(a: Multiaddr, peer: &PeerId) -> Option<Multiaddr> {
    if a.is_empty() {
        return None;
    }
    match addr::split_peer_id(&a) {
        (base, Some(p)) if p == *peer && !base.is_empty() => Some(base),
        (_, Some(p)) if p == *peer => None,
        // 릴레이 주소 `…/p2p/<릴레이>/p2p-circuit`는 끝이 p2p가 아니므로 그대로.
        (_, Some(_)) => None,
        (base, None) => Some(base),
    }
}

fn map_dial_error(e: &DialError) -> NetError {
    match e {
        DialError::NoAddresses => NetError::NoAddresses,
        DialError::LocalPeerId { .. } => {
            NetError::InvalidInput("자기 자신에게 연결할 수 없음".into())
        }
        DialError::Denied { .. } => NetError::Dial("연결이 거절됨(한도 또는 차단)".into()),
        DialError::WrongPeerId { .. } => NetError::Dial("상대 키가 기대와 다름".into()),
        DialError::Aborted => NetError::Dial("연결 시도가 중단됨".into()),
        DialError::DialPeerConditionFalse(_) => NetError::Dial("이미 연결 중".into()),
        DialError::Transport(_) => NetError::Dial("모든 주소에 연결하지 못함".into()),
    }
}

/// 로그용 다이얼 오류 요약(주소는 넣지 않음).
fn short_dial_error(e: &DialError) -> &'static str {
    match e {
        DialError::NoAddresses => "주소 없음",
        DialError::LocalPeerId { .. } => "자기 자신",
        DialError::Denied { .. } => "거절됨",
        DialError::WrongPeerId { .. } => "다른 키",
        DialError::Aborted => "중단",
        DialError::DialPeerConditionFalse(_) => "이미 연결 중",
        DialError::Transport(_) => "전송 실패",
    }
}

fn map_publish_error(e: gossipsub::PublishError) -> NetError {
    match e {
        gossipsub::PublishError::NoPeersSubscribedToTopic => NetError::NoPeers,
        gossipsub::PublishError::MessageTooLarge => NetError::TooLarge {
            size: crate::config::MAX_GOSSIP_MESSAGE + 1,
            max: crate::config::MAX_GOSSIP_MESSAGE,
        },
        gossipsub::PublishError::AllQueuesFull(_) => NetError::Busy,
        other => NetError::Io(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_deadline_leaves_room_for_error_response() {
        for t in [1u64, 2, 30, 90] {
            let t = Duration::from_secs(t);
            let d = daemon_deadline(t);
            assert!(d < t && d >= t / 2, "{t:?} -> {d:?}");
        }
        assert_eq!(daemon_deadline(Duration::ZERO), Duration::from_millis(100));
    }

    #[test]
    fn relay_rank_orders_mine_sticky_other() {
        let (m, s0, s1, other, other2) = (
            PeerId::random(),
            PeerId::random(),
            PeerId::random(),
            PeerId::random(),
            PeerId::random(),
        );
        let mine: HashSet<PeerId> = [m].into();
        let sticky = [s0, s1];
        let r = |p: &PeerId| relay_rank(p, &mine, &sticky);
        assert!(r(&m) < r(&s0) && r(&s0) < r(&s1) && r(&s1) < r(&other));
        assert_eq!(r(&other), r(&other2));
    }

    /// 지난번 릴레이에 걸고 있는 동안에는 순위가 낮은 후보로 자리를 채우지 않고, 닿지 못하면 다음 지난번 릴레이부터
    /// 겁니다(연결이 없는 후보는 바로 예약하지 않고 먼저 겁니다).
    #[tokio::test]
    async fn dialing_sticky_relay_holds_its_slot() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = NetConfig::new(
            NetMode::Normal,
            libp2p::identity::Keypair::generate_ed25519(),
            dir.path(),
        );
        cfg.mdns = false;
        let p = crate::prepare(
            &cfg,
            #[cfg(feature = "tor")]
            None,
        )
        .unwrap();
        let (tx, _rx) = mpsc::channel(8);
        let st = Arc::new(Mutex::new(NetStatus::initial(
            cfg.keypair.public().to_peer_id(),
            cfg.mode,
        )));
        let mut t = Task::new(
            p.swarm,
            &cfg,
            p.transports,
            #[cfg(feature = "tor")]
            None,
            tx,
            st,
        );
        let (s0, s1, other) = (PeerId::random(), PeerId::random(), PeerId::random());
        // 시험망 주소(192.0.2.0/24). 스웜을 돌리지 않으므로 실제로 나가지는 않습니다.
        let base: Multiaddr = "/ip4/192.0.2.1/tcp/4001".parse().unwrap();
        for p in [s0, s1, other] {
            t.relay.candidates.insert(p, base.clone());
        }
        t.relay.sticky = vec![s0, s1];
        t.relay.dialing.insert(s0, Instant::now() + RELAY_DIAL_WAIT);
        t.maintain_relays();
        assert!(t.relay.active.is_empty());
        assert_eq!(
            t.relay.dialing.keys().copied().collect::<Vec<_>>(),
            vec![s0],
            "걸고 있는 지난번 릴레이의 자리를 낮은 순위에 내주면 안 됨"
        );

        t.relay.dialing.clear();
        t.relay.backoff.insert(s0, Instant::now() + RELAY_BACKOFF);
        t.maintain_relays();
        assert!(
            t.relay.active.is_empty(),
            "연결이 없으면 예약 대신 먼저 겁니다"
        );
        assert!(t.relay.dialing.contains_key(&s1) && !t.relay.dialing.contains_key(&other));
    }

    #[test]
    fn heard_addrs_keep_onion_and_relay_within_cap() {
        let me = PeerId::random();
        let relay = PeerId::random();
        let onion: Multiaddr =
            "/onion3/vww6ybal4bd7szmgncyruucpgfkqahzddi37ktceo3ah7ngmcopnpyyd:4001"
                .parse()
                .unwrap();
        let circuit: Multiaddr = format!("/ip4/1.2.3.4/tcp/1/p2p/{relay}/p2p-circuit")
            .parse()
            .unwrap();
        let mut listen: Vec<Multiaddr> = (0..MAX_ADDRS_PER_PEER as u16 + 4)
            .map(|i| format!("/ip4/10.0.0.1/tcp/{}", 1000 + i).parse().unwrap())
            .collect();
        listen.push(circuit.clone());
        listen.push(onion.clone());
        let heard = heard_listen_addrs(listen, &me);
        assert_eq!(heard.len(), MAX_ADDRS_PER_PEER);
        assert!(heard[..2].contains(&onion) && heard[..2].contains(&circuit));
    }

    #[test]
    fn normalize_addr_checks_peer_suffix() {
        let me = PeerId::random();
        let other = PeerId::random();
        let base: Multiaddr = "/ip4/1.2.3.4/tcp/1".parse().unwrap();
        assert_eq!(normalize_addr(base.clone(), &me), Some(base.clone()));
        assert_eq!(
            normalize_addr(base.clone().with(Protocol::P2p(me)), &me),
            Some(base.clone())
        );
        assert_eq!(
            normalize_addr(base.clone().with(Protocol::P2p(other)), &me),
            None
        );
        assert_eq!(normalize_addr(Multiaddr::empty(), &me), None);
        let circuit = base.with(Protocol::P2p(other)).with(Protocol::P2pCircuit);
        assert_eq!(normalize_addr(circuit.clone(), &me), Some(circuit));
    }
}
