//! 통화 SFU 호스트 실행기: `yangpa-sfu` 엔진(sans-IO)을 UDP 소켓·시계·통화 신호 경로에 잇습니다.
//!
//! - 엔진은 작업 하나가 혼자 쥐고 명령 채널로만 만집니다. 소켓은 맡을 통화가 처음 생길 때 엽니다. Tor로 도는 노드는
//!   아예 맡지 않아(실제 IP가 참가자에게 드러나므로), 실제로 맡는 것은 직접 연결 주 스웜(`--direct`)의 상시 노드뿐입니다.
//!   멈출 때는 실행기가 소켓을 닫고 끝날 때까지 기다립니다(STUN 모으기 중이어도 바로 그만둠).
//! - 수용 인원은 노드 전체 몫입니다. 통화마다 알리는 자리는 먼저 연 다른 통화방이 잡아 둔 자리를 뺀 값이고, 맡은
//!   통화방이 바뀌면 다시 알립니다(나중에 연 통화가 비켜 감).
//! - 호스트 후보는 노드 수신 주소의 IP(또는 `--sfu-bind`), 서버 반사 후보는 `--sfu-announce`와 STUN 서버 목록
//!   (`net.stun_servers`)으로 얻습니다. TURN은 쓰지 않습니다.
//! - 참가자에게 가는 신호는 기기마다 순서대로 보냅니다(재협상 제안이 앞지르지 않게).
//! - 로그에 SDP·후보를 남기지 않습니다(ICE 비밀번호·지문·IP가 들어 있음).

use std::collections::{BTreeSet, HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use futures::future::join_all;
use parking_lot::Mutex;
use rand::RngCore;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;
use tracing::{debug, info, warn};
use yangpa_core::ids::DeviceId;
use yangpa_core::wire::{SfuCaps, SignalPayload};
use yangpa_net::{NatStatus, PeerId};
use yangpa_sfu::{Candidate, Output, RoomId, SessionInfo, Sfu, SfuConfig, SfuError, SfuEvent};

use crate::config::SfuHostMode;
use crate::node::Node;
use crate::{stun, tor, voice};

/// 소켓을 열지 못한 뒤 다시 맡겠다고 나서기까지 기다리는 시간.
const FAIL_BACKOFF: Duration = Duration::from_secs(60);
/// STUN 응답을 기다리는 시간(첫 세션 응답이 이만큼 늦을 수 있음).
const STUN_WAIT: Duration = Duration::from_millis(1500);
/// 세션 제안보다 먼저 온 ICE 후보를 기기마다 이만큼 잡아 둠.
const EARLY_ICE: usize = 16;
/// 받는 UDP 데이터그램 크기 상한(MTU보다 넉넉히).
const MAX_DATAGRAM: usize = 2048;
/// RTT를 이 단위로 묶어 비교(조금씩 흔들리는 값으로 호스트가 바뀌지 않게).
pub(crate) const RTT_BUCKET_MS: u32 = 50;
/// 멈출 때 실행기가 끝나기를 기다리는 한도(보통은 바로 끝남. 한도는 멈춤 경로가 묶이지 않게 하는 안전장치).
const STOP_WAIT: Duration = Duration::from_secs(10);

/// SFU 실행기 상태(`VoiceState` 안).
#[derive(Default)]
pub struct SfuState {
    runner: Mutex<Option<RunnerHandle>>,
    /// 맡은 통화방(엔진에 보낸 것과 같음).
    rooms: Mutex<Rooms>,
    failed_at: Mutex<Option<Instant>>,
}

/// 맡은 통화방: 연 순서와 들어올 수 있는 기기.
#[derive(Default)]
struct Rooms {
    next: u64,
    map: HashMap<RoomId, (u64, BTreeSet<DeviceId>)>,
}

impl Rooms {
    /// `room`에 알릴 수용 인원: `capacity`에서 그보다 먼저 연 통화방(맡지 않은 통화면 모든 통화방)이 잡아 둔
    /// 자리(들어올 수 있는 기기 수)를 뺀 값. 먼저 연 통화가 자리를 지키고 나중에 연 통화가 비켜 가게 합니다.
    fn capacity_for(&self, capacity: u16, room: &RoomId) -> u16 {
        let mine = self.map.get(room).map(|(order, _)| *order);
        let reserved: usize = self
            .map
            .iter()
            .filter(|(r, (order, _))| *r != room && mine.is_none_or(|m| *order < m))
            .map(|(_, (_, allowed))| allowed.len())
            .sum();
        capacity.saturating_sub(u16::try_from(reserved).unwrap_or(u16::MAX))
    }
}

/// 돌고 있는 실행기.
struct RunnerHandle {
    tx: mpsc::UnboundedSender<Cmd>,
    /// 보내거나 놓으면 실행기가 UDP를 더 쓰지 않고 바로 끝납니다.
    cancel: oneshot::Sender<()>,
    /// 실행기가 소켓을 닫고 끝나면 닫힘.
    done: oneshot::Receiver<()>,
    /// 미디어 소켓 주소.
    addrs: Vec<SocketAddr>,
}

enum Cmd {
    Allow(RoomId, BTreeSet<DeviceId>),
    Close(RoomId),
    Signal {
        room: RoomId,
        from: DeviceId,
        payload: SignalPayload,
        reply: oneshot::Sender<Result<(), SfuError>>,
    },
    Sessions {
        room: RoomId,
        reply: oneshot::Sender<Vec<SessionInfo>>,
    },
}

fn state(node: &Node) -> &SfuState {
    &node.voice.sfu
}

/// 지금 SFU 호스트를 맡을 수 있는지. Tor로 도는 노드(시험의 `simulate_tor` 포함)는 설정과 상관없이 맡지 않습니다.
pub(crate) fn can_host(node: &Node) -> bool {
    node.cfg.sfu.host != SfuHostMode::Never
        && !tor::tor_active(node)
        && state(node)
            .failed_at
            .lock()
            .is_none_or(|t| t.elapsed() >= FAIL_BACKOFF)
}

/// 참가하지 않은 통화에도 호스트 후보로 나서는지(헤드리스 시더).
pub(crate) fn prefers_hosting(node: &Node) -> bool {
    node.cfg.sfu.host == SfuHostMode::Prefer && can_host(node)
}

/// 호스트 선출에 쓸 이 노드의 조건. `call_peers`는 그 통화 참가자의 노드입니다. 맡을 수 없으면 `None`.
pub(crate) async fn caps(
    node: &Node,
    room: RoomId,
    call_peers: &BTreeSet<PeerId>,
) -> Option<SfuCaps> {
    if !can_host(node) {
        return None;
    }
    let status = node.net.last_status();
    let connected = node.net.peers().await;
    let mut relayed = 0u16;
    let mut rtts: Vec<u32> = Vec::new();
    for p in call_peers {
        match connected.iter().find(|c| c.peer_id == *p) {
            // 지금 연결이 없으면 직접 닿는지 모르므로 릴레이와 같이 셉니다.
            None => relayed = relayed.saturating_add(1),
            Some(c) => {
                if c.relayed_only {
                    relayed = relayed.saturating_add(1);
                }
                if let Some(r) = c.rtt_ms {
                    rtts.push(r);
                }
            }
        }
    }
    rtts.sort_unstable();
    let cfg = &node.cfg.sfu;
    Some(SfuCaps {
        preferred: cfg.host == SfuHostMode::Prefer,
        public: status.nat == NatStatus::Public || !cfg.announce.is_empty(),
        relayed,
        rtt_ms: rtts.get(rtts.len() / 2).copied(),
        capacity: state(node)
            .rooms
            .lock()
            .capacity_for(cfg.capacity.max(1), &room),
    })
}

/// 맡을 통화방을 맞춥니다. `allowed`가 있으면 열거나 들어올 기기를 바꾸고, `None`이면 닫습니다.
pub(crate) fn sync_room(node: &Arc<Node>, room: RoomId, allowed: Option<BTreeSet<DeviceId>>) {
    let st = state(node);
    let Some(allowed) = allowed.filter(|_| can_host(node)) else {
        if st.rooms.lock().map.remove(&room).is_some() {
            if let Some(h) = st.runner.lock().as_ref() {
                let _ = h.tx.send(Cmd::Close(room));
            }
            debug!("SFU 통화방 닫음");
            // 비운 자리를 다른 통화에 다시 알립니다.
            voice::caps_changed(node);
        }
        return;
    };
    {
        let mut rooms = st.rooms.lock();
        let rooms = &mut *rooms;
        let order = match rooms.map.get(&room) {
            Some((_, a)) if *a == allowed => return,
            Some((order, _)) => *order,
            None => {
                rooms.next += 1;
                rooms.next
            }
        };
        rooms.map.insert(room, (order, allowed.clone()));
    }
    let Some(tx) = ensure_runner(node) else {
        st.rooms.lock().map.remove(&room);
        // 맡을 수 없게 됐음을 알려 키 관리 기기가 다른 호스트를 고르게 합니다.
        voice::caps_changed(node);
        return;
    };
    let _ = tx.send(Cmd::Allow(room, allowed));
    // 잡아 둔 자리가 바뀌었으니 다른 통화에 알리는 수용 인원도 다시 잽니다.
    voice::caps_changed(node);
}

/// 이 노드가 그 통화의 SFU를 맡고 있는지.
pub(crate) fn is_hosting(node: &Node, room: &RoomId) -> bool {
    state(node).rooms.lock().map.contains_key(room)
}

/// 참가자 기기 `from`의 SFU 신호를 엔진에 넘깁니다. 부르는 쪽이 `from`이 그 통화 참가자인지 먼저 확인합니다.
pub(crate) async fn handle_signal(
    node: &Node,
    room: RoomId,
    from: DeviceId,
    payload: SignalPayload,
) -> Result<(), SfuError> {
    if !is_hosting(node, &room) {
        return Err(SfuError::NoRoom);
    }
    let tx = state(node)
        .runner
        .lock()
        .as_ref()
        .map(|h| h.tx.clone())
        .ok_or(SfuError::NoRoom)?;
    let (reply, rx) = oneshot::channel();
    tx.send(Cmd::Signal {
        room,
        from,
        payload,
        reply,
    })
    .map_err(|_| SfuError::NoRoom)?;
    rx.await.unwrap_or(Err(SfuError::NoRoom))
}

/// 모든 통화방과 소켓을 닫습니다(시험에서 Tor처럼 보게 함). 실행기가 소켓을 닫고 끝날 때까지
/// 기다리므로, 돌아온 뒤에는 실제 IP로 나가는 SFU의 UDP(미디어·STUN)가 없습니다.
pub(crate) async fn stop(node: &Node) {
    let st = state(node);
    let rooms: Vec<RoomId> = st.rooms.lock().map.drain().map(|(r, _)| r).collect();
    let Some(h) = st.runner.lock().take() else {
        return;
    };
    for r in rooms {
        let _ = h.tx.send(Cmd::Close(r));
    }
    // 닫기 명령(참가자에게 `SfuClose`)만 처리하고 끝나라고 알립니다.
    let _ = h.cancel.send(());
    if tokio::time::timeout(STOP_WAIT, h.done).await.is_err() {
        warn!("SFU 실행기가 제때 끝나지 않음");
    }
    info!("SFU 호스트 멈춤");
}

fn ensure_runner(node: &Arc<Node>) -> Option<mpsc::UnboundedSender<Cmd>> {
    let st = state(node);
    let mut runner = st.runner.lock();
    if let Some(h) = runner.as_ref().filter(|h| !h.tx.is_closed()) {
        return Some(h.tx.clone());
    }
    let cfg = &node.cfg.sfu;
    let ips = bind_ips(node);
    let mut sockets = Vec::new();
    for ip in &ips {
        match bind(*ip, cfg.port) {
            Ok(s) => sockets.push(s),
            Err(e) => debug!(error = %e, "SFU 소켓을 열지 못함"),
        }
    }
    if sockets.is_empty() {
        warn!("SFU 미디어 소켓을 하나도 열지 못해 호스트를 맡지 않음");
        *st.failed_at.lock() = Some(Instant::now());
        return None;
    }
    let addrs: Vec<SocketAddr> = sockets.iter().filter_map(|s| s.local_addr().ok()).collect();
    let stun = stun::stun_servers(node);
    let (tx, rx) = mpsc::unbounded_channel();
    let (cancel, cancel_rx) = oneshot::channel();
    let (done_tx, done) = oneshot::channel();
    let config = SfuConfig {
        capacity: usize::from(cfg.capacity.max(1)),
        ..SfuConfig::default()
    };
    let announce = cfg.announce.clone();
    let weak = Arc::downgrade(node);
    node.tasks.spawn(async move {
        run(weak, sockets, announce, stun, config, rx, cancel_rx).await;
        // 소켓을 다 닫은 뒤에 끝났다고 알립니다.
        drop(done_tx);
    });
    info!("SFU 호스트 시작");
    *runner = Some(RunnerHandle {
        tx: tx.clone(),
        cancel,
        done,
        addrs,
    });
    Some(tx)
}

fn bind(ip: IpAddr, port: u16) -> std::io::Result<UdpSocket> {
    let s = std::net::UdpSocket::bind(SocketAddr::new(ip, port))?;
    s.set_nonblocking(true)?;
    UdpSocket::from_std(s)
}

/// 미디어 소켓을 열 IP: 설정값, 없으면 노드 수신 주소의 IP(루프백은 다른 주소가 없을 때만).
fn bind_ips(node: &Node) -> Vec<IpAddr> {
    let mut ips: Vec<IpAddr> = if node.cfg.sfu.bind.is_empty() {
        node.net
            .last_status()
            .listen_addrs
            .iter()
            .filter_map(|a| ip_of(&a.to_string()))
            .filter(usable_ip)
            .collect()
    } else {
        node.cfg.sfu.bind.clone()
    };
    ips.sort();
    ips.dedup();
    if node.cfg.sfu.bind.is_empty() && ips.iter().any(|ip| !ip.is_loopback()) {
        ips.retain(|ip| !ip.is_loopback());
    }
    ips
}

/// `/ip4/<ip>/…` 꼴 multiaddr의 IP(릴레이 경유 주소는 릴레이의 IP라 뺌).
fn ip_of(addr: &str) -> Option<IpAddr> {
    if addr.contains("/p2p-circuit") {
        return None;
    }
    let mut parts = addr.split('/').skip(1);
    match (parts.next()?, parts.next()?) {
        ("ip4" | "ip6", ip) => ip.parse().ok(),
        _ => None,
    }
}

fn usable_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => !v.is_unspecified() && !v.is_link_local() && !v.is_multicast(),
        // 링크 로컬 IPv6는 범위 ID가 있어야 해서 후보로 쓰지 않음.
        IpAddr::V6(v) => {
            !v.is_unspecified() && !v.is_multicast() && (v.segments()[0] & 0xffc0) != 0xfe80
        }
    }
}

/// 소켓 하나의 서버 반사 주소를 STUN으로 알아냅니다(응답이 없으면 빈 목록). 보낸 요청의 트랜잭션 ID를 `sent`에
/// 더합니다(늦게 온 응답을 엔진에 넘기지 않게).
async fn reflexive(
    sock: &UdpSocket,
    servers: &[SocketAddr],
    sent: &Mutex<HashSet<[u8; 12]>>,
) -> Vec<SocketAddr> {
    let Ok(local) = sock.local_addr() else {
        return Vec::new();
    };
    let mut pending = Vec::new();
    for s in servers.iter().filter(|s| s.is_ipv4() == local.is_ipv4()) {
        let mut tid = [0u8; 12];
        rand::rngs::OsRng.fill_bytes(&mut tid);
        if sock
            .send_to(&yangpa_sfu::stun::binding_request(tid), s)
            .await
            .is_ok()
        {
            sent.lock().insert(tid);
            pending.push(tid);
        }
    }
    let deadline = tokio::time::Instant::now() + STUN_WAIT;
    let mut found = Vec::new();
    let mut buf = [0u8; 512];
    while !pending.is_empty() {
        let Ok(Ok((n, _))) = tokio::time::timeout_at(deadline, sock.recv_from(&mut buf)).await
        else {
            break;
        };
        if let Some(i) = pending.iter().position(|t| {
            yangpa_sfu::stun::parse_binding_response(&buf[..n], t)
                .inspect(|a| {
                    if !found.contains(a) {
                        found.push(*a);
                    }
                })
                .is_some()
        }) {
            pending.swap_remove(i);
        }
    }
    found
}

async fn gather(
    sockets: &[Arc<UdpSocket>],
    locals: &[SocketAddr],
    announce: &[IpAddr],
    stun: &[String],
    sent: &Mutex<HashSet<[u8; 12]>>,
) -> Vec<Candidate> {
    let mut out = Vec::new();
    for l in locals {
        match Candidate::host(*l, "udp") {
            Ok(c) => out.push(c),
            Err(e) => debug!(error = %e, "SFU 호스트 후보를 만들지 못함"),
        }
        for ip in announce.iter().filter(|ip| ip.is_ipv4() == l.is_ipv4()) {
            if *ip == l.ip() {
                continue;
            }
            if let Ok(c) = Candidate::server_reflexive(SocketAddr::new(*ip, l.port()), *l, "udp") {
                out.push(c);
            }
        }
    }
    let mut servers = Vec::new();
    // 목록은 `stun::normalize`를 거친 `host:port`(IPv6는 `[v6]:port`)라 그대로 찾아볼 수 있습니다.
    for target in stun {
        if let Ok(addrs) = tokio::net::lookup_host(target.as_str()).await {
            servers.extend(addrs);
        }
    }
    if servers.is_empty() {
        return out;
    }
    let found = join_all(sockets.iter().map(|s| reflexive(s, &servers, sent))).await;
    for (l, mapped) in locals.iter().zip(found) {
        for m in mapped.into_iter().filter(|m| m != l) {
            if let Ok(c) = Candidate::server_reflexive(m, *l, "udp") {
                out.push(c);
            }
        }
    }
    out
}

/// 내가 STUN 서버에 보낸 요청의 응답인지. 참가자가 SFU의 ICE 연결 확인에 답한 응답은 엔진에 넘겨야 합니다
/// (버리면 SFU 쪽 확인이 하나도 성공하지 못해 참가자의 확인 요청에만 기대게 됨).
fn is_gather_response(data: &[u8], sent: &HashSet<[u8; 12]>) -> bool {
    yangpa_sfu::stun::binding_response_id(data).is_some_and(|t| sent.contains(&t))
}

/// 실행기: 엔진·소켓·시계를 돌립니다. 그만두라는 알림(`cancel`)을 받거나 명령 채널이 닫히면(노드 종료) 끝나고,
/// 끝나기 전에 소켓을 닫습니다.
async fn run(
    weak: Weak<Node>,
    sockets: Vec<UdpSocket>,
    announce: Vec<IpAddr>,
    stun: Vec<String>,
    config: SfuConfig,
    mut rx: mpsc::UnboundedReceiver<Cmd>,
    mut cancel: oneshot::Receiver<()>,
) {
    let sockets: Vec<Arc<UdpSocket>> = sockets.into_iter().map(Arc::new).collect();
    let locals: Vec<SocketAddr> = sockets.iter().filter_map(|s| s.local_addr().ok()).collect();
    if locals.len() != sockets.len() {
        return;
    }
    let mut sfu = Sfu::new(config);
    let sent = Mutex::new(HashSet::new());
    // STUN 서버를 기다리는 동안에도 멈추면 바로 그만둡니다(Tor로 바뀐 뒤 실제 IP로 STUN을 보내지 않게).
    let candidates = tokio::select! {
        biased;
        _ = &mut cancel => return,
        c = gather(&sockets, &locals, &announce, &stun, &sent) => c,
    };
    let sent = sent.into_inner();
    for c in candidates {
        sfu.add_local_candidate(c);
    }
    debug!(
        sockets = sockets.len(),
        candidates = sfu.local_candidates().len(),
        "SFU 후보 준비"
    );
    let (dtx, mut drx) = mpsc::channel::<(usize, SocketAddr, Vec<u8>)>(1024);
    let mut recv = JoinSet::new();
    for (i, s) in sockets.iter().enumerate() {
        let (s, dtx) = (s.clone(), dtx.clone());
        recv.spawn(async move {
            let mut buf = vec![0u8; MAX_DATAGRAM];
            loop {
                match s.recv_from(&mut buf).await {
                    Ok((n, from)) => {
                        if dtx.send((i, from, buf[..n].to_vec())).await.is_err() {
                            return;
                        }
                    }
                    // Windows는 보낸 곳이 닫히면(ICMP) 다음 받기에서 오류를 돌려줍니다. 소켓은 그대로 씀.
                    Err(e) => {
                        debug!(error = %e, "SFU 소켓 받기 오류");
                        tokio::task::yield_now().await;
                    }
                }
            }
        });
    }
    drop(dtx);
    let mut r = Runner {
        weak,
        sockets,
        locals,
        outboxes: HashMap::new(),
        allowed: HashMap::new(),
        early_ice: HashMap::new(),
    };
    loop {
        let wake = sfu
            .poll_timeout()
            .map(|t| t.max(Instant::now() + Duration::from_millis(1)));
        let sleep = async {
            match wake {
                Some(t) => tokio::time::sleep_until(tokio::time::Instant::from_std(t)).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            biased;
            _ = &mut cancel => {
                // 남은 명령(닫기)만 처리해 참가자에게 알리고, UDP는 더 보내지 않습니다.
                while let Ok(c) = rx.try_recv() {
                    r.command(&mut sfu, c);
                }
                r.drain(&mut sfu, false);
                break;
            }
            cmd = rx.recv() => match cmd {
                Some(c) => r.command(&mut sfu, c),
                None => break,
            },
            Some((i, from, data)) = drx.recv() => {
                if !is_gather_response(&data, &sent) {
                    sfu.handle_receive(Instant::now(), from, r.locals[i], &data);
                }
            }
            _ = sleep => sfu.handle_timeout(Instant::now()),
        }
        r.drain(&mut sfu, true);
    }
    // 받기 작업이 소켓을 놓을 때까지 기다린 뒤 소켓을 닫습니다.
    recv.abort_all();
    while recv.join_next().await.is_some() {}
    drop(r);
    debug!("SFU 실행기 끝");
}

struct Runner {
    weak: Weak<Node>,
    sockets: Vec<Arc<UdpSocket>>,
    locals: Vec<SocketAddr>,
    /// 참가자마다 순서대로 신호를 보내는 작업.
    outboxes: HashMap<(RoomId, DeviceId), mpsc::UnboundedSender<SignalPayload>>,
    allowed: HashMap<RoomId, BTreeSet<DeviceId>>,
    /// 세션 제안보다 먼저 온 ICE 후보.
    early_ice: HashMap<(RoomId, DeviceId), Vec<SignalPayload>>,
}

impl Runner {
    fn command(&mut self, sfu: &mut Sfu, cmd: Cmd) {
        let now = Instant::now();
        match cmd {
            Cmd::Allow(room, devices) => {
                self.early_ice
                    .retain(|(r, d), _| *r != room || devices.contains(d));
                sfu.set_allowed(now, room, devices.iter().copied());
                self.allowed.insert(room, devices);
            }
            Cmd::Close(room) => {
                sfu.close_room(now, room);
                self.allowed.remove(&room);
                self.early_ice.retain(|(r, _), _| *r != room);
            }
            Cmd::Signal {
                room,
                from,
                payload,
                reply,
            } => {
                let res = self.signal(sfu, now, room, from, payload);
                let _ = reply.send(res);
            }
            Cmd::Sessions { room, reply } => {
                let _ = reply.send(sfu.sessions(&room));
            }
        }
    }

    fn signal(
        &mut self,
        sfu: &mut Sfu,
        now: Instant,
        room: RoomId,
        from: DeviceId,
        payload: SignalPayload,
    ) -> Result<(), SfuError> {
        match payload {
            p @ SignalPayload::SfuIce { .. } => {
                let keep = p.clone();
                match sfu.handle_signal(now, room, from, p) {
                    // 세션 제안이 아직 오지 않았을 수 있습니다(앱이 신호를 겹쳐 보냄). 제안 뒤에 다시 넣습니다.
                    Err(SfuError::StaleSession) => {
                        let list = self.early_ice.entry((room, from)).or_default();
                        if list.len() < EARLY_ICE {
                            list.push(keep);
                        }
                        Ok(())
                    }
                    r => r,
                }
            }
            SignalPayload::SfuOffer {
                session,
                sdp,
                tracks,
            } => {
                let p = SignalPayload::SfuOffer {
                    session,
                    sdp,
                    tracks,
                };
                let r = sfu.handle_signal(now, room, from, p);
                if r.is_ok()
                    && let Some(list) = self.early_ice.remove(&(room, from))
                {
                    for c in list {
                        if matches!(c, SignalPayload::SfuIce { session: s, .. } if s == session) {
                            let _ = sfu.handle_signal(now, room, from, c);
                        }
                    }
                }
                r
            }
            p => sfu.handle_signal(now, room, from, p),
        }
    }

    /// 엔진 출력을 내보냅니다. `udp`가 거짓이면(멈추는 중) UDP는 버리고 신호만 보냅니다.
    fn drain(&mut self, sfu: &mut Sfu, udp: bool) {
        while let Some(o) = sfu.poll_output() {
            match o {
                Output::Transmit {
                    source,
                    destination,
                    contents,
                } => {
                    if !udp {
                        continue;
                    }
                    if let Some(i) = self.locals.iter().position(|l| *l == source) {
                        // 소켓이 꽉 차면 버립니다(UDP, 받는 쪽이 재전송·키프레임 요청으로 복구).
                        let _ = self.sockets[i].try_send_to(&contents, destination);
                    }
                }
                Output::Signal { room, to, payload } => self.deliver(room, to, payload),
                Output::Event(SfuEvent::Connected { device, .. }) => {
                    debug!(device = %device.short(), "SFU 세션 연결됨");
                }
                Output::Event(SfuEvent::Left { room, device }) => {
                    debug!(device = %device.short(), "SFU 세션 끝남");
                    if !self.allowed.get(&room).is_some_and(|a| a.contains(&device)) {
                        // 남은 신호(닫기)를 다 보낸 뒤 그 기기의 보내기 작업이 끝납니다.
                        self.outboxes.remove(&(room, device));
                    }
                }
            }
        }
    }

    fn deliver(&mut self, room: RoomId, to: DeviceId, payload: SignalPayload) {
        if let Some(tx) = self.outboxes.get(&(room, to))
            && tx.send(payload.clone()).is_ok()
        {
            return;
        }
        let Some(node) = self.weak.upgrade() else {
            return;
        };
        let (tx, mut rx) = mpsc::unbounded_channel::<SignalPayload>();
        let weak = self.weak.clone();
        node.tasks.spawn(async move {
            while let Some(p) = rx.recv().await {
                let Some(node) = weak.upgrade() else { return };
                voice::sfu_deliver(&node, room, to, p).await;
            }
        });
        let _ = tx.send(payload);
        self.outboxes.insert((room, to), tx);
    }
}

/// 시험용 들여다보기.
pub(crate) mod inspect {
    use super::*;

    /// 그 통화방의 SFU 세션(맡지 않으면 빈 목록).
    pub(crate) async fn sessions(node: &Node, room: RoomId) -> Vec<SessionInfo> {
        let Some(tx) = state(node).runner.lock().as_ref().map(|h| h.tx.clone()) else {
            return Vec::new();
        };
        let (reply, rx) = oneshot::channel();
        if tx.send(Cmd::Sessions { room, reply }).is_err() {
            return Vec::new();
        }
        rx.await.unwrap_or_default()
    }

    /// 미디어 소켓 주소(실행기가 없으면 빈 목록).
    pub(crate) fn addrs(node: &Node) -> Vec<SocketAddr> {
        state(node)
            .runner
            .lock()
            .as_ref()
            .map(|h| h.addrs.clone())
            .unwrap_or_default()
    }

    pub(crate) fn hosted_rooms(node: &Node) -> Vec<RoomId> {
        state(node).rooms.lock().map.keys().copied().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_listen_ips() {
        assert_eq!(
            ip_of("/ip4/192.0.2.5/udp/4001/quic-v1"),
            Some("192.0.2.5".parse().unwrap())
        );
        assert_eq!(ip_of("/ip6/::1/tcp/4001"), Some("::1".parse().unwrap()));
        assert_eq!(
            ip_of("/ip4/198.51.100.1/tcp/4001/p2p/12D3KooW/p2p-circuit"),
            None
        );
        assert_eq!(ip_of("/dns4/example.org/tcp/1"), None);
        assert!(!usable_ip(&"0.0.0.0".parse().unwrap()));
        assert!(!usable_ip(&"fe80::1".parse().unwrap()));
        assert!(usable_ip(&"10.0.0.2".parse().unwrap()));
    }

    /// STUN 서버에 보낸 요청의 응답만 거르고, 참가자가 SFU의 ICE 연결 확인에 답한 바인딩 응답은 엔진에 넘깁니다.
    #[test]
    fn only_gather_responses_are_filtered() {
        let response = |tid: [u8; 12]| {
            let mut b = vec![0x01, 0x01, 0x00, 0x00, 0x21, 0x12, 0xA4, 0x42];
            b.extend_from_slice(&tid);
            b
        };
        let sent: HashSet<[u8; 12]> = [[1; 12]].into_iter().collect();
        assert!(is_gather_response(&response([1; 12]), &sent));
        assert!(
            !is_gather_response(&response([2; 12]), &sent),
            "ICE 확인 응답"
        );
        assert!(!is_gather_response(&response([1; 12]), &HashSet::new()));
        let request = yangpa_sfu::stun::binding_request([1; 12]);
        assert!(!is_gather_response(&request, &sent));
        assert!(!is_gather_response(&[0x80, 0x60, 0, 1], &sent), "RTP");
    }

    #[test]
    fn capacity_is_shared_with_older_rooms() {
        let room = |n: u8| {
            (
                yangpa_core::ids::SpaceId::from_bytes([n; 16]),
                yangpa_core::ids::ChannelId::from_bytes([n; 16]),
            )
        };
        let devices = |n: u8| -> BTreeSet<DeviceId> {
            (0..n).map(|i| DeviceId::from_bytes([i; 32])).collect()
        };
        let mut rooms = Rooms::default();
        assert_eq!(rooms.capacity_for(25, &room(1)), 25);
        rooms.map.insert(room(1), (1, devices(15)));
        // 맡은 통화방 자신의 자리는 빼지 않음.
        assert_eq!(rooms.capacity_for(25, &room(1)), 25);
        // 아직 맡지 않은 통화에는 남은 자리만.
        assert_eq!(rooms.capacity_for(25, &room(2)), 10);
        rooms.map.insert(room(2), (2, devices(12)));
        // 먼저 연 통화방은 그대로, 나중에 연 통화방은 넘친 만큼 줄어 비켜 감.
        assert_eq!(rooms.capacity_for(25, &room(1)), 25);
        assert_eq!(rooms.capacity_for(25, &room(2)), 10);
        assert_eq!(rooms.capacity_for(25, &room(3)), 0);
        rooms.map.remove(&room(1));
        assert_eq!(rooms.capacity_for(25, &room(2)), 25);
    }
}
