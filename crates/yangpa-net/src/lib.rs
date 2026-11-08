//! 양파 메신저 네트워크 계층(libp2p 0.57).
//!
//! - 스웜 종류([`NetMode`]): Tor(앱 사용자 기기의 주 스웜), 직접 연결(데몬의 P2P 스웜과 `--direct` 주 스웜)
//! - 전송: 직접 연결 스웜은 QUIC, TCP(TLS, 남의 노드에만 Noise + yamux), DNS, 릴레이 회로. Tor 스웜은 Arti뿐(나가는 연결 전부, onion 수신)
//! - 요청-응답 `/yangpa/rpc/5`(OpenZL로 압축한 CBOR, 압축 전 최대 8MiB), GossipSub(닫힌 스웜·수동 검증), Kademlia `/yangpa/kad/1.0.0`
//! - identify, ping, 릴레이 서버(선택)·클라이언트, DCUtR, AutoNAT, mDNS(선택), UPnP(선택), 연결 제한
//! - 직접 연결 스웜의 릴레이는 부트스트랩 노드, 지난번 릴레이, identify로 릴레이를 맡는다고 알린 YANGPA 노드만 씀(공개망
//!   노드는 쓰지 않음)
//!
//! [`spawn`]이 스웜 작업을 띄우고 [`NetHandle`]과 [`NetEvent`] 수신기를 돌려줍니다.

pub mod addr;
mod behaviour;
mod codec;
pub mod config;
mod error;
mod gossip;
mod handle;
pub mod media;
mod task;
#[cfg(feature = "tor")]
mod tor;
mod tor_stream;
mod traffic;
mod transport;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use libp2p::Swarm;
use tokio::sync::mpsc;

pub use codec::{Incoming, Opened};
pub use config::{
    FeaturePlan, KadMode, MAX_GOSSIP_MESSAGE, NetConfig, NetMode, StickyRelay, TorBridges,
};
pub use error::NetError;
#[cfg(feature = "tor")]
pub use tor::check_bridge_line;

/// Tor 없는 빌드에는 브리지가 없습니다.
#[cfg(not(feature = "tor"))]
pub fn check_bridge_line(_line: &str) -> Result<(), String> {
    Err("이 빌드는 Tor를 지원하지 않습니다".into())
}
pub use handle::{
    ConnectedPeer, GossipVerdict, NatStatus, NetEvent, NetHandle, NetInspection, NetStatus,
    RelayNode, TorShare,
};
pub use libp2p::gossipsub::MessageId;
pub use libp2p::identity::Keypair;
pub use libp2p::multiaddr::Protocol;
pub use libp2p::{Multiaddr, PeerId};
pub use tor_stream::{TorActivity, TorIsolation, TorStream};
pub use traffic::Traffic;
pub use transport::{
    TRANSPORT_ARTI_ALL, TRANSPORT_ONION_LISTEN, TRANSPORT_QUIC, TRANSPORT_RELAY,
    TRANSPORT_SYSTEM_DNS, TRANSPORT_TCP,
};

/// 데몬에 이벤트를 넘기는 채널 크기. 넘치면 스웜 작업이 내부 대기열에 잠시 둡니다.
pub const EVENT_CHANNEL_CAPACITY: usize = 1024;

/// 스웜 작업을 띄웁니다. tokio 런타임 안에서 불러야 합니다.
///
/// 수신 주소를 열지 못하면(포트 사용 중 등) 오류입니다. Tor 스웜의 onion 서비스는 백그라운드에서 뜨고, 진행 상황은
/// [`NetStatus::tor_bootstrap`]과 [`NetStatus::onion_addr`]로 확인합니다.
pub fn spawn(cfg: NetConfig) -> Result<(NetHandle, mpsc::Receiver<NetEvent>), NetError> {
    let prepared = prepare(
        &cfg,
        #[cfg(feature = "tor")]
        None,
    )?;
    let (ev_tx, ev_rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
    let handover = handle::Handover {
        events: ev_tx,
        queue: VecDeque::new(),
    };
    let started = launch(prepared, &cfg, handover).map_err(|(e, _)| e)?;
    Ok((NetHandle::new(started, cfg.traffic.clone()), ev_rx))
}

/// 망 없이 핸들만 만듭니다(데몬의 오프라인 모드). 스웜·Arti·수신 소켓을 띄우지 않아 아무 데도 연결하지 않고, 모든
/// 호출은 [`NetError::Shutdown`]입니다. 이벤트 수신기는 바로 끝납니다. PeerId는 `cfg`의 키 그대로입니다.
pub fn spawn_offline(cfg: &NetConfig) -> (NetHandle, mpsc::Receiver<NetEvent>) {
    let local = cfg.keypair.public().to_peer_id();
    let (cmd, _) = mpsc::unbounded_channel();
    let (_, ev_rx) = mpsc::channel(1);
    let started = Started {
        cmd,
        local,
        last_status: Arc::new(Mutex::new(handle::NetStatus::initial(local, cfg.mode))),
        limits: handle::HandleLimits {
            request: cfg.request_timeout,
            dial: cfg.dial_timeout(),
            kad: cfg.request_timeout,
        },
        #[cfg(feature = "tor")]
        arti: None,
    };
    (NetHandle::new(started, cfg.traffic.clone()), ev_rx)
}

/// 수신을 열기 전까지의 준비(전송·동작 조립). 실패해도 지금 도는 스웜에는 영향이 없습니다.
///
/// `arti`는 지금 스웜의 Arti 상태입니다. 같은 데이터 경로면 새 스웜도 그것을 이어 씁니다([`transport::build`]).
pub(crate) struct Prepared {
    swarm: Swarm<behaviour::YpBehaviour>,
    transports: Vec<&'static str>,
    #[cfg(feature = "tor")]
    arti: Option<Arc<tor::ArtiShared>>,
}

pub(crate) fn prepare(
    cfg: &NetConfig,
    #[cfg(feature = "tor")] arti: Option<Arc<tor::ArtiShared>>,
) -> Result<Prepared, NetError> {
    tokio::runtime::Handle::try_current()
        .map_err(|_| NetError::InvalidInput("tokio 런타임 안에서 불러야 함".into()))?;
    let local = cfg.keypair.public().to_peer_id();
    let built = transport::build(
        cfg,
        #[cfg(feature = "tor")]
        arti,
    )?;
    let behaviour = behaviour::YpBehaviour::new(cfg, built.relay_client)?;
    let swarm = Swarm::new(
        built.transport,
        behaviour,
        local,
        libp2p::swarm::Config::with_tokio_executor()
            .with_idle_connection_timeout(Duration::from_secs(120))
            // 요청마다 프로토콜 확인을 기다리지 않고 요청을 바로 실어 왕복 한 번을 아낌(Tor에서 약 1초). 받는 쪽은 그대로.
            .with_substream_upgrade_protocol_override(libp2p::core::upgrade::Version::V1Lazy),
    );
    Ok(Prepared {
        swarm,
        transports: built.names,
        #[cfg(feature = "tor")]
        arti: built.arti,
    })
}

pub(crate) struct Started {
    pub cmd: mpsc::UnboundedSender<handle::Command>,
    pub local: PeerId,
    pub last_status: Arc<Mutex<NetStatus>>,
    pub limits: handle::HandleLimits,
    /// 이 스웜의 Arti(앱 스트림·활동 수, Tor 스웜만).
    #[cfg(feature = "tor")]
    pub arti: Option<Arc<tor::ArtiShared>>,
}

/// 수신을 열고 스웜 작업을 띄웁니다. 실패하면 이벤트 채널을 돌려줘 다음 시도가 이어 쓰게 합니다.
#[allow(clippy::result_large_err)]
pub(crate) fn launch(
    prepared: Prepared,
    cfg: &NetConfig,
    handover: handle::Handover,
) -> Result<Started, (NetError, handle::Handover)> {
    let Prepared {
        mut swarm,
        transports,
        #[cfg(feature = "tor")]
        arti,
    } = prepared;
    let local = *swarm.local_peer_id();
    for a in cfg.effective_listen_addrs() {
        if let Err(e) = swarm.listen_on(a) {
            return Err((NetError::Transport(e.to_string()), handover));
        }
    }
    for a in cfg.effective_external_addrs() {
        swarm.add_external_address(a);
    }

    let last_status = Arc::new(Mutex::new(NetStatus::initial(local, cfg.mode)));
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let mut task = task::Task::new(
        swarm,
        cfg,
        transports,
        #[cfg(feature = "tor")]
        arti.clone(),
        handover.events,
        last_status.clone(),
    );
    task.adopt_queue(handover.queue);
    task.add_bootstrap(cfg.effective_bootstrap());
    task.add_sticky_relays(cfg.sticky_relays.clone());

    let kad_timeout = if cfg.mode == NetMode::Tor {
        Duration::from_secs(120)
    } else {
        Duration::from_secs(45)
    };
    let dial_wait = cfg.dial_timeout();
    let limits = handle::HandleLimits {
        request: cfg.request_timeout + dial_wait + Duration::from_secs(5),
        dial: dial_wait + Duration::from_secs(5),
        kad: kad_timeout + Duration::from_secs(5),
    };
    tokio::spawn(task.run(cmd_rx));
    Ok(Started {
        cmd: cmd_tx,
        local,
        last_status,
        limits,
        #[cfg(feature = "tor")]
        arti,
    })
}
