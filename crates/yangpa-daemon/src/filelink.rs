//! Tor 파일 회로: 큰 첨부를 Tor로 받을 때 조각 요청을 글·동기화와 다른 Tor 회로 여럿으로 나릅니다.
//!
//! 주 스웜은 상대 기기마다 연결(= Tor 회로) 하나로 글·동기화·파일을 함께 나르므로, 파일 조각을 많이 겹쳐 보내면 글이
//! 조각 뒤에 밀립니다. 그래서 Tor로 받을 때는 조각 요청을 피어당 3개로 묶어 왔고, 회로 하나의 속도가 끝이었습니다.
//!
//! 받는 쪽은 큰 첨부를 받을 때만 파일 회로를 설정한 수(1~[`MAX_CIRCUITS`])만큼 엽니다. 회로마다 작은 스웜 하나(띄울 때마다 새 임시 키,
//! 받지 않고 걸기만 함, Arti는 주 스웜 것을 함께 쓰되 연결마다 격리 토큰이 달라 같은 onion 주소로 걸어도 회로가
//! 따로)를 띄워 상대 주 스웜의 onion 주소에 겁니다. onion 주소·PeerId는 그대로 하나입니다. 이어지면
//! [`YpRequest::FileLink`]로 그 임시 PeerId를 내 기기 키로 서명해 알리고, 상대는 같은 기기가 주 스웜으로 이미 인증돼
//! 있을 때만 받아 들여 그 연결에서는 블롭 요청만 그 기기의 주 연결인 것처럼 답합니다(그 밖의 요청은 거절). 상대는
//! 임시 PeerId로 자기 요청을 보내지 않습니다.
//!
//! 회로마다 스웜을 따로 두는 까닭: libp2p는 같은 피어의 연결들에 요청을 번갈아 실을 뿐 고를 수 없어, 한 스웜에 연결만
//! 더하면 멈춘 회로에 실린 조각이 시간 초과까지 묶였습니다(실제 Tor 망에서 이어진 동안 약 200KiB/s였지만 멈춘 회로
//! 때문에 평균은 주 연결보다 느림). 회로를 하나씩 다루면 받는 쪽 조각 스케줄러가 회로를 받을 곳 하나로 보고 빠른
//! 회로에 더 맡기고, 늦은 조각은 다른 회로에 겹쳐 요청하며, 실패가 잦은 회로를 뺍니다(`blobs::fetch`).
//!
//! 링크가 하나도 없으면 조각은 주 연결로 갑니다(예전 빌드 상대도 그대로 받음). 일정 시간 쓰지 않으면 스웜을 멈춥니다.
//!
//! 선택 기능입니다(설정 [`SETTING`]은 상대 하나에 더 여는 회로 수 0~4, 기본 0 = 끔, 사용자 결정 2026-10-22·10-24). 파일
//! 회로 하나마다 내주는 쪽 onion 서비스도 랑데부 지점으로 새 회로를 만드므로, 회로를 자주·많이 만들수록 가드 찾기
//! 공격에 더 드러납니다(Arti vanguards가 줄임). 그래서 1 이상으로 둔 기기만 파일 회로를 열고 들어온 파일 링크를 받습니다.
//! 양쪽이 모두 1 이상이어야 쓰입니다(여는 수는 받는 쪽 설정).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tracing::{debug, info, warn};
use yangpa_core::wire::{PeerAuth, WireErrorCode, YpRequest, YpResponse};
use yangpa_net::{
    GossipVerdict, KadMode, Keypair, Multiaddr, NetConfig, NetError, NetEvent, NetHandle, NetMode,
    PeerId,
};
use yangpa_sync::swarm::{Bitmap, ChunkScheduler};

use crate::node::Node;
use crate::peers::MAX_AUTH_SKEW_MS;

/// 파일 회로를 쓰는 블롭의 최소 조각 수(256KiB 조각이면 1MiB 넘음). 작은 첨부는 회로를 만드는 몇 초가 더 깁니다.
pub(crate) const MIN_CHUNKS: usize = 5;
/// 상대 하나에 여는 파일 회로 수 상한(설정 0~이 값). 회로 하나는 Tor 스트림 하나의 흐름 창(500셀, 약 243KiB)과 경로의
/// 릴레이에 묶이므로 여럿으로 나눕니다. 회로마다 Tor 망에 부하가 더해지므로 넷까지만.
pub(crate) const MAX_CIRCUITS: u8 = 4;
/// 받을 곳(주 연결 또는 회로) 하나에 겹쳐 보낼 조각 요청 수와 블롭 하나 전체 상한(파일 회로가 있을 때).
pub(crate) const WINDOW_PER_ROUTE: u32 = 3;
pub(crate) const WINDOW_TOTAL: u32 = 16;
/// 이만큼 쓰지 않으면 파일 스웜을 멈춥니다.
const IDLE_STOP: Duration = Duration::from_secs(120);
/// 잇지 못한 회로를 다시 걸어 보기까지.
const RETRY_GAP: Duration = Duration::from_secs(60);
/// 받아 둘 들어온 파일 링크 수 상한.
const MAX_INBOUND: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Link {
    Connecting,
    Ready,
    Failed(Instant),
}

#[derive(Default)]
pub(crate) struct FileLinkState {
    /// 회로 번호(1부터) → 그 회로의 스웜.
    swarms: Mutex<HashMap<u8, NetHandle>>,
    /// 스웜 띄우기를 하나씩.
    life: tokio::sync::Mutex<()>,
    /// 받는 쪽: (상대 주 PeerId, 회로 번호) → 링크 상태.
    links: Mutex<HashMap<(PeerId, u8), Link>>,
    /// 내주는 쪽: 들어온 파일 스웜 PeerId → 그 기기의 주 PeerId.
    inbound: Mutex<HashMap<PeerId, PeerId>>,
    last_used: Mutex<Option<Instant>>,
    /// 내주는 쪽: 파일 링크로 답한 조각 수(시험용).
    served_chunks: AtomicU64,
}

/// 회로 수 설정 키(`u8` 0~[`MAX_CIRCUITS`], 없으면 0). 예전 빌드는 `bool`로 적었습니다(켬 = 4).
pub(crate) const SETTING: &str = "net.file_circuits";

/// 상대 하나에 더 여는 파일 회로 수(0이면 끔).
pub(crate) fn circuits(node: &Node) -> u8 {
    match node.store.get_setting::<serde_json::Value>(SETTING) {
        Ok(Some(serde_json::Value::Bool(on))) => {
            if on {
                MAX_CIRCUITS
            } else {
                0
            }
        }
        Ok(Some(v)) => v
            .as_u64()
            .map_or(0, |n| n.min(u64::from(MAX_CIRCUITS)) as u8),
        Ok(None) => 0,
        // 못 읽으면 회로를 늘리지 않는 쪽으로.
        Err(e) => {
            warn!(error = %e, "파일 회로 설정을 읽지 못함");
            0
        }
    }
}

/// 파일 회로를 쓰는지(1개 이상).
pub(crate) fn enabled(node: &Node) -> bool {
    circuits(node) > 0
}

/// 회로 수를 저장합니다. 0이면 이어 둔 파일 스웜을 멈추고 들어온 파일 링크도 지우고, 줄이면 넘는 번호의 회로를 닫습니다.
pub(crate) fn set_circuits(node: &Node, n: u8) -> crate::error::Result<()> {
    if n > MAX_CIRCUITS {
        return Err(crate::error::Error::invalid(format!(
            "파일 회로는 0~{MAX_CIRCUITS}개입니다"
        )));
    }
    node.store.put_setting(SETTING, &n)?;
    if n == 0 {
        shutdown(node);
        return Ok(());
    }
    node.filelink.links.lock().retain(|(_, c), _| *c <= n);
    node.filelink.swarms.lock().retain(|c, h| {
        let keep = *c <= n;
        if !keep {
            h.shutdown();
        }
        keep
    });
    Ok(())
}

/// 이 블롭을 파일 회로로 받을지: 켰고, Tor로 돌고, 큰 블롭.
pub(crate) fn wanted(node: &Node, chunks: usize) -> bool {
    chunks >= MIN_CHUNKS && crate::tor::tor_active(node) && enabled(node)
}

/// 이 상대들과 파일 회로를 준비합니다(없거나 실패한 지 오래된 회로를 겁니다).
pub(crate) fn prepare(node: &Arc<Node>, peers: &[PeerId]) {
    let now = Instant::now();
    *node.filelink.last_used.lock() = Some(now);
    let mut start = Vec::new();
    let n = circuits(node);
    {
        let mut links = node.filelink.links.lock();
        for p in peers {
            for c in 1..=n {
                let go = match links.get(&(*p, c)) {
                    None => true,
                    Some(Link::Failed(at)) => now.duration_since(*at) >= RETRY_GAP,
                    Some(_) => false,
                };
                if go {
                    links.insert((*p, c), Link::Connecting);
                    start.push((*p, c));
                }
            }
        }
    }
    for (peer, c) in start {
        node.spawn_detached(move |node| async move {
            let ok = match connect(&node, peer, c).await {
                Ok(()) => true,
                Err(e) => {
                    debug!(%peer, circuit = c, error = %e, "파일 회로를 잇지 못함");
                    false
                }
            };
            let mut links = node.filelink.links.lock();
            // 그사이 스웜이 멈췄으면(링크를 지움) 남기지 않습니다.
            if let Some(l) = links.get_mut(&(peer, c)) {
                *l = if ok {
                    Link::Ready
                } else {
                    Link::Failed(Instant::now())
                };
            }
        });
    }
}

/// 이 상대와 이어진 파일 회로 번호들.
pub(crate) fn ready_circuits(node: &Node, peer: &PeerId) -> Vec<u8> {
    let links = node.filelink.links.lock();
    (1..=MAX_CIRCUITS)
        .filter(|c| links.get(&(*peer, *c)) == Some(&Link::Ready))
        .collect()
}

/// 파일 회로 `circuit`으로 보냅니다. 그 회로가 끊겼으면 실패로 표시하고 오류를 돌려줍니다(받는 쪽 스케줄러가 다른
/// 곳에서 다시 받음).
pub(crate) async fn request(
    node: &Node,
    peer: PeerId,
    circuit: u8,
    req: YpRequest,
) -> Result<YpResponse, NetError> {
    let ready = node.filelink.links.lock().get(&(peer, circuit)) == Some(&Link::Ready);
    let handle = ready
        .then(|| node.filelink.swarms.lock().get(&circuit).cloned())
        .flatten();
    let Some(h) = handle else {
        return Err(NetError::Shutdown);
    };
    *node.filelink.last_used.lock() = Some(Instant::now());
    let r = h.request(peer, req).await;
    match &r {
        // 이 스웜의 피어당 요청 상한: 잠깐 쉬면 됨.
        Err(NetError::Busy) => {}
        Err(e) => {
            debug!(%peer, circuit, error = %e, "파일 회로로 못 보냄");
            mark_failed(node, &peer, circuit);
        }
        Ok(YpResponse::Error {
            code: WireErrorCode::Unauthenticated,
            ..
        }) => mark_failed(node, &peer, circuit),
        Ok(_) => {}
    }
    r
}

fn mark_failed(node: &Node, peer: &PeerId, circuit: u8) {
    if let Some(l) = node.filelink.links.lock().get_mut(&(*peer, circuit)) {
        *l = Link::Failed(Instant::now());
    }
}

/// 주 스웜이 아는 상대 주소(들어온 연결이면 기록해 둔 주소).
async fn addrs_of(node: &Node, peer: PeerId) -> Vec<Multiaddr> {
    let mut addrs = node.net.peer_listen_addrs(peer).await;
    if let Ok(Some(rec)) = node.store.get_peer(&peer.to_bytes()) {
        for a in rec.addrs.iter().filter_map(|a| a.parse::<Multiaddr>().ok()) {
            if !addrs.contains(&a) {
                addrs.push(a);
            }
        }
    }
    addrs
}

async fn connect(node: &Arc<Node>, peer: PeerId, circuit: u8) -> crate::error::Result<()> {
    let h = swarm(node, circuit).await?;
    let addrs = addrs_of(node, peer).await;
    if addrs.is_empty() {
        return Err(crate::error::Error::NotFound("상대 주소".into()));
    }
    h.dial(peer, addrs).await?;
    let me = node.me()?;
    let auth = PeerAuth::create(
        &me.device.signing,
        me.devices.clone(),
        &h.local_peer_id().to_bytes(),
        &peer.to_bytes(),
    );
    match h.request(peer, YpRequest::FileLink(Box::new(auth))).await? {
        YpResponse::Ok => Ok(()),
        // 예전 빌드 상대는 이 요청을 모름: 주 연결로 받습니다.
        _ => Err(crate::error::Error::Conflict("파일 링크 거절".into())),
    }
}

/// 회로 `circuit`의 스웜(없으면 띄움).
async fn swarm(node: &Arc<Node>, circuit: u8) -> crate::error::Result<NetHandle> {
    let _l = node.filelink.life.lock().await;
    if let Some(h) = node.filelink.swarms.lock().get(&circuit).cloned() {
        return Ok(h);
    }
    if node.is_shutting_down() {
        return Err(crate::error::Error::ShuttingDown);
    }
    let cfg = &node.cfg;
    // 띄울 때마다 새 키: 지난번 파일 스웜이나 주 스웜과 이어 볼 수 없게.
    let keypair = Keypair::generate_ed25519();
    let mut n = match node.net.tor_share() {
        // 주 스웜의 Arti를 함께 쓰되 회로는 따로(같은 onion 주소로 걸어도 새 회로).
        Some(share) => {
            let mut n = NetConfig::new(NetMode::Tor, keypair, cfg.data_dir.clone());
            n.onion_listen = false;
            n.tor_share = Some(share);
            n.tor_isolated = true;
            n
        }
        // 직접 연결 주 스웜(`--direct`: 시험)에는 Arti가 없어, 받지 않는 직접 스웜으로 겁니다. Tor로 도는 기기에서
        // Arti를 못 얻었을 때 직접 스웜으로 물러나면 IP가 드러나므로 그때는 열지 않습니다.
        None if cfg.net.direct => {
            let mut n = NetConfig::new(
                NetMode::Normal,
                keypair,
                cfg.data_dir.join(format!("file-link-{circuit}")),
            );
            n.listen = Vec::new();
            n
        }
        None => return Err(crate::error::Error::Network("Tor를 쓸 수 없음".into())),
    };
    n.mdns = false;
    n.kad_mode = KadMode::Client;
    n.request_timeout = cfg.net.request_timeout;
    // 앱의 전체 속도 표시가 파일 회로도 함께 세게.
    n.traffic = node.net.traffic();
    let (h, mut events) = yangpa_net::spawn(n)?;
    let local = h.local_peer_id();
    node.filelink.swarms.lock().insert(circuit, h.clone());
    info!(circuit, "파일 스웜을 띄움");
    let weak = Arc::downgrade(node);
    let eh = h.clone();
    node.tasks.spawn(async move {
        while let Some(ev) = events.recv().await {
            let Some(node) = weak.upgrade() else { break };
            if node.is_shutting_down()
                || node
                    .filelink
                    .swarms
                    .lock()
                    .get(&circuit)
                    .is_none_or(|x| x.local_peer_id() != local)
            {
                break;
            }
            match ev {
                // 걸기만 하는 스웜이라 받은 요청은 답하지 않습니다(상대가 이 임시 PeerId로 요청할 일이 없음).
                NetEvent::InboundRequest { reply, .. } => {
                    let _ = reply.send(YpResponse::error(
                        WireErrorCode::Unauthenticated,
                        "파일 스웜은 요청을 받지 않음",
                    ));
                }
                NetEvent::Gossip { from, msg_id, .. } => {
                    eh.report_gossip(msg_id, from, GossipVerdict::Ignore);
                }
                NetEvent::PeerDisconnected(p) => {
                    node.filelink.links.lock().remove(&(p, circuit));
                }
                _ => {}
            }
        }
    });
    Ok(h)
}

/// 주기 작업: 오래 쓰지 않은 파일 스웜을 멈춥니다.
pub(crate) fn tick(node: &Arc<Node>) {
    let idle = node
        .filelink
        .last_used
        .lock()
        .is_none_or(|t| t.elapsed() >= IDLE_STOP);
    if idle && !node.filelink.swarms.lock().is_empty() {
        stop(node);
        info!("쓰지 않는 파일 스웜을 멈춤");
    }
}

fn stop(node: &Node) {
    for (_, h) in node.filelink.swarms.lock().drain() {
        h.shutdown();
    }
    node.filelink.links.lock().clear();
}

/// 노드 종료.
pub(crate) fn shutdown(node: &Node) {
    stop(node);
    node.filelink.inbound.lock().clear();
}

/// (이어 둔 회로 수, 들어온 링크 수, 도는 스웜 수). 시험용.
pub(crate) fn counts(node: &Node) -> (usize, usize, usize) {
    let ready = node
        .filelink
        .links
        .lock()
        .values()
        .filter(|l| **l == Link::Ready)
        .count();
    (
        ready,
        node.filelink.inbound.lock().len(),
        node.filelink.swarms.lock().len(),
    )
}

// ───────────── 받는 쪽: 길 나누기 ─────────────

/// 조각을 받을 곳: (상대 기기의 주 PeerId, 길). 길 [`MAIN`]은 주 연결, 1부터는 그 상대와의 파일 회로. 받는 쪽 조각
/// 스케줄러는 길마다 따로 받을 곳으로 봐 빠른 회로에 더 맡기고, 늦은 조각은 다른 길에 겹쳐 요청합니다. 일반 첨부
/// (`blobs::fetch`)와 휘발성 첨부(`volatile::attach`)가 함께 씁니다.
pub(crate) type Route = (PeerId, u8);
pub(crate) const MAIN: u8 = 0;

/// 파일 회로를 열 상대 수 상한(블롭 하나). 상대마다 회로가 설정한 수만큼 생기므로 크루의 모든 멤버에게 열지
/// 않습니다.
const MAX_PEERS: usize = 3;

/// 파일 회로를 열 상대: 조각을 가졌다고 답한 상대 중 몇(늘 같은 순서로 골라 고를 때마다 바뀌지 않게). 직접 연결로
/// 닿는 상대는 뺍니다(받기 쪽 겹침 방식을 다시 정하기 전에 그 상대의 답이 먼저 와도 회로를 열지 않게 여기서 봄).
pub(crate) fn targets(node: &Node, seeders: &HashSet<PeerId>) -> Vec<PeerId> {
    let mut v: Vec<PeerId> = seeders
        .iter()
        .copied()
        .filter(|p| !crate::p2p::data_goes_direct(node, p))
        .collect();
    v.sort();
    v.truncate(MAX_PEERS);
    v
}

/// 조각 요청 하나를 그 길로 보냅니다.
pub(crate) async fn send(
    node: &Node,
    route: Route,
    req: YpRequest,
) -> Result<YpResponse, NetError> {
    match route {
        (peer, MAIN) => crate::p2p::request(node, peer, req).await,
        (peer, circuit) => request(node, peer, circuit, req).await,
    }
}

/// 상대마다 알게 된 보유 비트맵을 지금 이어진 길들에 나눠 둡니다. 파일 회로가 하나라도 있으면 주 연결에는 맡기지
/// 않습니다(글이 조각 뒤에 밀리지 않게). 회로가 모두 끊기면 다시 주 연결로.
pub(crate) struct Routes {
    total: u32,
    /// 상대 → (마지막 보유 비트맵, 지금 맡기는 길들).
    peers: HashMap<PeerId, (Bitmap, Vec<u8>)>,
}

impl Routes {
    pub(crate) fn new(total: u32) -> Self {
        Routes {
            total,
            peers: HashMap::new(),
        }
    }

    /// 상대의 보유 비트맵이 바뀜.
    pub(crate) fn set_have(&mut self, sched: &mut ChunkScheduler<Route>, peer: PeerId, bm: Bitmap) {
        let ways = self
            .peers
            .get(&peer)
            .map(|(_, w)| w.clone())
            .unwrap_or_else(|| vec![MAIN]);
        for w in &ways {
            let _ = sched.set_peer_have((peer, *w), bm.clone());
        }
        self.peers.insert(peer, (bm, ways));
    }

    /// 받기 루프에서 요청을 내기 전마다: 겹침 수를 정하고(`blobs::Pacer`: 직접 연결이면 잰 속도, Tor면 한도), 파일
    /// 회로가 이어져 있으면 그 길들로 나눠 회로별 한도를 씁니다. 직접 연결로 받으면 파일 회로를 쓰지 않습니다.
    pub(crate) fn steer(
        &mut self,
        node: &Node,
        pacer: &mut crate::blobs::Pacer,
        sched: &mut ChunkScheduler<Route>,
        file_link: bool,
        now: u64,
    ) {
        let files = file_link && !pacer.is_direct();
        let circuits = self.sync(node, sched, files, pacer.direct_peers());
        pacer.apply(
            sched,
            now,
            circuits.then_some((WINDOW_PER_ROUTE, WINDOW_TOTAL)),
        );
    }

    /// 이어진 파일 회로에 맞춰 맡길 길을 고칩니다. `files`가 거짓이면(직접 연결로 받음) 주 연결만. 파일 회로가 하나라도
    /// 있으면 `true`.
    fn sync(
        &mut self,
        node: &Node,
        sched: &mut ChunkScheduler<Route>,
        files: bool,
        direct: &[PeerId],
    ) -> bool {
        let mut any = false;
        for (peer, (bm, ways)) in self.peers.iter_mut() {
            // 실패가 잦아 스케줄러가 뺀 회로는 이어져 있어도 없는 것으로 봅니다(모두 빠지면 주 연결로). 직접 연결로
            // 닿는 상대는 주 연결로만 받습니다.
            let circuits: Vec<u8> = if files && !direct.contains(peer) {
                ready_circuits(node, peer)
                    .into_iter()
                    .filter(|c| !sched.is_excluded(&(*peer, *c)))
                    .collect()
            } else {
                Vec::new()
            };
            let want: Vec<u8> = if circuits.is_empty() {
                vec![MAIN]
            } else {
                any = true;
                circuits
            };
            if *ways == want {
                continue;
            }
            for w in ways.iter().filter(|w| !want.contains(w)) {
                let _ = sched.set_peer_have((*peer, *w), Bitmap::new(self.total));
            }
            for w in want.iter().filter(|w| !ways.contains(w)) {
                let _ = sched.set_peer_have((*peer, *w), bm.clone());
            }
            *ways = want;
        }
        any
    }
}

// ───────────── 내주는 쪽 ─────────────

/// [`YpRequest::FileLink`]: 같은 기기가 주 스웜으로 인증돼 있으면 이 PeerId를 그 기기의 파일 링크로 받습니다.
pub(crate) fn handle_link(node: &Node, peer: PeerId, auth: PeerAuth) -> YpResponse {
    // 끈 기기는 받지 않습니다(내 onion 서비스가 회로를 더 만들게 되므로 내가 고름). 받는 쪽은 주 연결로 받습니다.
    if !enabled(node) {
        return YpResponse::error(WireErrorCode::Forbidden, "파일 회로를 쓰지 않음");
    }
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
    if !matches!(
        crate::identity::auth_status(node, &auth),
        crate::identity::AuthStatus::Active
    ) {
        return YpResponse::error(WireErrorCode::Unauthenticated, "활성 기기가 아님");
    }
    let Some(main) = node
        .peers
        .authed_peer_of_device(&auth.device)
        .filter(|m| node.peers.identity_of(m) == Some((auth.user, auth.device)))
    else {
        return YpResponse::error(
            WireErrorCode::Unauthenticated,
            "주 연결로 인증된 기기가 아님",
        );
    };
    let mut inbound = node.filelink.inbound.lock();
    // 한 기기가 자리를 다 차지하지 못하게 기기마다 회로 수의 두 배까지(다시 띄운 스웜의 옛 링크가 아직 남았을 때 여유).
    let of_device = inbound.values().filter(|m| **m == main).count();
    if !inbound.contains_key(&peer)
        && (inbound.len() >= MAX_INBOUND || of_device >= 2 * usize::from(MAX_CIRCUITS))
    {
        return YpResponse::Busy {
            retry_after_ms: 60_000,
        };
    }
    inbound.insert(peer, main);
    YpResponse::Ok
}

/// 파일 링크로 들어온 요청: 블롭 요청만 그 기기(`main`)의 주 연결에서 온 것처럼 답합니다. 일반 첨부와 RAM의 휘발성
/// 첨부 모두 `blobs::handle_request`가 같은 권한 검사로 답합니다.
pub(crate) async fn serve(node: &Arc<Node>, main: PeerId, req: YpRequest) -> YpResponse {
    match req {
        req @ (YpRequest::BlobManifest { .. }
        | YpRequest::BlobHave { .. }
        | YpRequest::BlobChunk { .. }) => {
            let chunk = matches!(req, YpRequest::BlobChunk { .. });
            let r = crate::blobs::handle_request(node, main, req).await;
            if chunk && matches!(r, YpResponse::Chunk { .. }) {
                node.filelink.served_chunks.fetch_add(1, Ordering::Relaxed);
            }
            r
        }
        _ => YpResponse::error(
            WireErrorCode::Unauthenticated,
            "파일 회로에서는 블롭 요청만 받음",
        ),
    }
}

/// 파일 링크로 답한 조각 수. 시험용.
pub(crate) fn served_chunks(node: &Node) -> u64 {
    node.filelink.served_chunks.load(Ordering::Relaxed)
}

/// 들어온 연결이 파일 링크면 그 기기의 주 PeerId.
pub(crate) fn main_peer_of(node: &Node, peer: &PeerId) -> Option<PeerId> {
    node.filelink.inbound.lock().get(peer).copied()
}

/// 주 스웜에서 끊긴 피어: 그 파일 링크, 또는 그 기기를 주 PeerId로 둔 파일 링크를 지웁니다.
pub(crate) fn on_disconnected(node: &Node, peer: &PeerId) {
    let mut inbound = node.filelink.inbound.lock();
    inbound.remove(peer);
    inbound.retain(|_, main| main != peer);
}
