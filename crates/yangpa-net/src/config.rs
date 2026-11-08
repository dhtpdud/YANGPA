//! 네트워크 설정과 스웜 종류(`NetMode`)별 기능 계획.

use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use libp2p::PeerId;
use libp2p::identity::Keypair;
use libp2p::multiaddr::{Multiaddr, Protocol};
use serde::{Deserialize, Serialize};

use crate::addr;

/// Kademlia 프로토콜 이름. 다른 libp2p 망의 DHT(IPFS 등)와 섞이지 않게 따로 둡니다.
pub const KAD_PROTOCOL: &str = "/yangpa/kad/1.0.0";
/// identify 프로토콜 버전 문자열.
pub const IDENTIFY_PROTOCOL: &str = "/yangpa/id/1.0.0";
/// gossip 메시지 하나의 최대 크기(압축 전). 이벤트 본문 하나(256KiB)와 묶음 여유를 감안했습니다.
/// 더 큰 묶음은 직접 스트림(`PushEvents`)으로 보냅니다.
pub const MAX_GOSSIP_MESSAGE: usize = 1024 * 1024;
/// DHT 기록 키 최대 길이(위치 기록 키는 BLAKE3 32바이트).
pub const MAX_RECORD_KEY: usize = 64;
/// DHT 기록 값 최대 길이(암호화된 위치 기록).
pub const MAX_RECORD_VALUE: usize = 8 * 1024;
/// Tor 스웜 onion 서비스의 가상 포트.
pub const DEFAULT_ONION_PORT: u16 = 4001;

/// 네트워크 모드. 모드마다 네트워크 키(PeerId)가 달라야 합니다. 사용자 기기의 주 스웜은 늘 Tor이고, 일반(직접
/// 연결)은 P2P 전용 채널의 P2P 스웜과 상시 시더·중계 서버·시험의 주 스웜(`--direct`)에만 씁니다.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum NetMode {
    /// 직접 연결(TCP·QUIC·릴레이·홀펀칭). IP가 드러나고 Arti는 쓰지 않습니다.
    #[default]
    Normal,
    /// 나가는 연결은 모두 Arti, 들어오는 연결은 onion 서비스.
    Tor,
}

/// Kademlia 역할.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum KadMode {
    /// 외부 주소가 확인되면 서버, 아니면 클라이언트(libp2p 기본 동작).
    #[default]
    Auto,
    /// 항상 서버(부트스트랩·릴레이 노드, 테스트).
    Server,
    /// 항상 클라이언트(DHT 요청에 답하지 않음).
    Client,
}

/// [`crate::spawn`]에 넘기는 설정.
#[derive(Clone)]
pub struct NetConfig {
    pub mode: NetMode,
    /// 직접 연결 스웜의 수신 주소. Tor 스웜은 무시하고 onion 서비스만 엽니다.
    pub listen: Vec<Multiaddr>,
    /// 부트스트랩 노드(`/p2p/<id>` 포함 권장). 릴레이 후보·AutoNAT 서버로도 씁니다.
    pub bootstrap: Vec<Multiaddr>,
    /// 스웜 종류마다 다른 네트워크 키(P2P 스웜은 띄울 때마다 새 키).
    pub keypair: Keypair,
    /// 같은 네트워크 탐색(Tor 스웜은 항상 끔. 데몬은 `--direct` 주 스웜에서만 켬).
    pub mdns: bool,
    /// 공유기 UPnP로 수신 포트를 엽니다(직접 연결 스웜만). 기본은 꺼짐(시험이 공유기를 건드리지 않게)이고 데몬의 P2P
    /// 스웜이 켭니다. Tor 스웜은 항상 끔.
    pub upnp: bool,
    /// 다른 노드를 위한 릴레이 서버 역할(Tor 스웜은 항상 끔).
    pub relay_server: bool,
    /// 동시에 유지할 최대 연결 수.
    pub max_connections: u32,
    /// Arti 상태·캐시(`<data_dir>/arti`).
    pub data_dir: PathBuf,
    /// 요청 하나의 응답 대기 시간.
    pub request_timeout: Duration,
    /// `penalize` 후 차단 유지 시간.
    pub ban_duration: Duration,
    /// Tor 스웜 onion 서비스 가상 포트.
    pub onion_port: u16,
    /// Kademlia 역할(Tor 스웜이면 무시하고 클라이언트).
    pub kad_mode: KadMode,
    /// 공인 주소를 아는 부트스트랩·릴레이 노드가 미리 알리는 외부 주소. 릴레이 서버는 확인된 외부 주소가
    /// 있어야 예약을 받습니다. Tor 스웜은 무시합니다.
    pub external_addrs: Vec<Multiaddr>,
    /// 지난번에 예약했던 릴레이(앞일수록 먼저). 남이 기억한 중계 주소(`…/p2p/<릴레이>/p2p-circuit/p2p/<나>`)가 다시
    /// 켠 뒤에도 맞도록 시작할 때 먼저 걸어 다시 예약합니다. 데몬은 이제 채우지 않습니다(P2P 스웜은 띄울 때마다 키가
    /// 바뀌어 지난 중계 주소가 맞지 않음). Tor 스웜은 무시합니다.
    pub sticky_relays: Vec<StickyRelay>,
    /// Tor 스웜이 onion 서비스로 받을지(기본 켬). 끄면 나가는 연결만 Arti로 하는 스웜(데몬의 출구 스웜)입니다.
    pub onion_listen: bool,
    /// 다른 스웜의 Arti를 함께 씁니다([`crate::NetHandle::tor_share`]). Tor 스웜만 보고, 있으면 `data_dir`의 Arti
    /// 경로는 쓰지 않습니다.
    pub tor_share: Option<crate::handle::TorShare>,
    /// 이 스웜의 Arti 연결마다 회로를 따로 만듭니다(Tor 스웜만). 같은 onion 주소로 걸어도 주 스웜이나 이 스웜의 다른
    /// 연결과 회로를 같이 쓰지 않습니다(데몬의 파일 스웜: 파일 조각이 글과 같은 회로에서 밀리지 않고, Tor 스트림 하나의
    /// 흐름 창에 묶이지 않게 여러 회로로 나눔).
    pub tor_isolated: bool,
    /// Tor 브리지(검열 우회). Arti를 새로 만들거나 이어 쓸 때 이 값으로 맞춥니다. `tor_share`로 받은 Arti는
    /// 건드리지 않습니다(넘겨준 스웜의 설정을 따름).
    pub tor_bridges: Option<TorBridges>,
    /// 요청·응답 바이트를 셀 곳. 데몬은 모든 스웜에 같은 값을 넘겨 전체 속도를 잽니다.
    pub traffic: crate::Traffic,
}

/// Tor 브리지 설정([`NetConfig::tor_bridges`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TorBridges {
    /// 브리지 줄(`obfs4 1.2.3.4:443 <지문> cert=… iat-mode=0`, `snowflake …`, `webtunnel …`).
    pub lines: Vec<String>,
    /// 브리지 전송(obfs4·snowflake·webtunnel·meek_lite)을 맡는 PT 실행 파일(lyrebird).
    pub lyrebird: PathBuf,
}

// shortcut: Tor Browser 16.0(tor-expert-bundle-windows-x86_64-16.0, pt_config.json, 2026-10-28)의 내장 브리지를 그대로
// 옮긴 것이라 Tor 프로젝트가 바꾸면 낡습니다. 내장 브리지가 막히기 시작하면 새 판의 pt_config.json으로 바꾸세요.
/// 내장 obfs4 브리지.
pub const BUILTIN_OBFS4: [&str; 7] = [
    "obfs4 37.218.245.14:38224 D9A82D2F9C2F65A18407B1D2B764F130847F8B5D cert=bjRaMrr1BRiAW8IE9U5z27fQaYgOhX1UCmOpg2pFpoMvo6ZgQMzLsaTzzQNTlm7hNcb+Sg iat-mode=0",
    "obfs4 209.148.46.65:443 74FAD13168806246602538555B5521A0383A1875 cert=ssH+9rP8dG2NLDN2XuFw63hIO/9MNNinLmxQDpVa+7kTOa9/m+tGWT1SmSYpQ9uTBGa6Hw iat-mode=0",
    "obfs4 146.57.248.225:22 10A6CD36A537FCE513A322361547444B393989F0 cert=K1gDtDAIcUfeLqbstggjIw2rtgIKqdIhUlHp82XRqNSq/mtAjp1BIC9vHKJ2FAEpGssTPw iat-mode=0",
    "obfs4 45.145.95.6:27015 C5B7CD6946FF10C5B3E89691A7D3F2C122D2117C cert=TD7PbUO0/0k6xYHMPW3vJxICfkMZNdkRrb63Zhl5j9dW3iRGiCx0A7mPhe5T2EDzQ35+Zw iat-mode=0",
    "obfs4 51.222.13.177:80 5EDAC3B810E12B01F6FD8050D2FD3E277B289A08 cert=2uplIpLQ0q9+0qMFrK5pkaYRDOe460LL9WHBvatgkuRr/SL31wBOEupaMMJ6koRE6Ld0ew iat-mode=0",
    "obfs4 212.83.43.95:443 BFE712113A72899AD685764B211FACD30FF52C31 cert=ayq0XzCwhpdysn5o0EyDUbmSOx3X/oTEbzDMvczHOdBJKlvIdHHLJGkZARtT4dcBFArPPg iat-mode=1",
    "obfs4 212.83.43.74:443 39562501228A4D5E27FCA4C0C81A01EE23AE3EE4 cert=PBwr+S8JTVZo6MPdHnkTwXJPILWADLqfMGoVvhZClMq/Urndyd42BwX9YFJHZnBB3H0XCw iat-mode=1",
];
/// 내장 Snowflake 브리지(브로커는 CDN 도메인 프론팅, 프록시와는 WebRTC).
pub const BUILTIN_SNOWFLAKE: [&str; 2] = [
    "snowflake 192.0.2.3:80 2B280B23E1107BB62ABFC40DDCC8824814F80A72 fingerprint=2B280B23E1107BB62ABFC40DDCC8824814F80A72 url=https://1098762253.rsc.cdn77.org/ fronts=app.datapacket.com,www.datapacket.com ice=stun:stun.epygi.com:3478,stun:stun.uls.co.za:3478,stun:stun.voipgate.com:3478,stun:stun.mixvoip.com:3478,stun:stun.telnyx.com:3478,stun:stun.hot-chilli.net:3478,stun:stun.fitauto.ru:3478,stun:stun.m-online.net:3478 utls-imitate=hellorandomizedalpn",
    "snowflake 192.0.2.4:80 8838024498816A039FCBBAB14E6F40A0843051FA fingerprint=8838024498816A039FCBBAB14E6F40A0843051FA url=https://1098762253.rsc.cdn77.org/ fronts=app.datapacket.com,www.datapacket.com ice=stun:stun.epygi.com:3478,stun:stun.uls.co.za:3478,stun:stun.voipgate.com:3478,stun:stun.mixvoip.com:3478,stun:stun.telnyx.com:3478,stun:stun.hot-chilli.net:3478,stun:stun.fitauto.ru:3478,stun:stun.m-online.net:3478 utls-imitate=hellorandomizedalpn",
];

/// 기억해 둘 지난번 릴레이 수(예약은 많아야 둘이라, 잠깐 실패한 것까지 넉넉히).
pub const MAX_STICKY_RELAYS: usize = 4;
/// 지난번 릴레이 하나에 저장해 두고 걸어 볼 주소 수.
pub const MAX_STICKY_ADDRS: usize = 2;

/// 지난번에 예약했던 릴레이 하나([`NetConfig::sticky_relays`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StickyRelay {
    pub peer_id: PeerId,
    /// 릴레이에 거는 주소(`/p2p` 없이, 앞일수록 먼저).
    pub addrs: Vec<Multiaddr>,
}

impl NetConfig {
    /// 스웜 종류에 맞는 기본값으로 설정을 만듭니다.
    pub fn new(mode: NetMode, keypair: Keypair, data_dir: impl Into<PathBuf>) -> Self {
        let listen = match mode {
            NetMode::Normal => vec![
                "/ip4/0.0.0.0/udp/0/quic-v1".parse().expect("고정 주소"),
                "/ip4/0.0.0.0/tcp/0".parse().expect("고정 주소"),
            ],
            NetMode::Tor => Vec::new(),
        };
        NetConfig {
            mode,
            listen,
            bootstrap: Vec::new(),
            keypair,
            mdns: mode == NetMode::Normal,
            upnp: false,
            relay_server: false,
            max_connections: 256,
            data_dir: data_dir.into(),
            request_timeout: match mode {
                NetMode::Normal => Duration::from_secs(30),
                NetMode::Tor => Duration::from_secs(90),
            },
            ban_duration: Duration::from_secs(10 * 60),
            onion_port: DEFAULT_ONION_PORT,
            kad_mode: KadMode::Auto,
            external_addrs: Vec::new(),
            sticky_relays: Vec::new(),
            onion_listen: true,
            tor_share: None,
            tor_isolated: false,
            tor_bridges: None,
            traffic: crate::Traffic::default(),
        }
    }

    /// 스웜 종류에 따라 실제로 켜는 기능. 켜 둔 값이라도 Tor 스웜에서 위험한 것은 끕니다.
    pub fn plan(&self) -> FeaturePlan {
        let tor_available = cfg!(feature = "tor");
        match self.mode {
            // 직접 연결 스웜은 Arti를 띄우지 않습니다: 직접 연결과 onion이 한 스웜에 섞여 IP와 onion이 이어지지 않게.
            NetMode::Normal => FeaturePlan {
                tcp: true,
                quic: true,
                system_dns: true,
                onion_dial: false,
                tor_dial_all: false,
                onion_listen: false,
                identify: true,
                mdns: self.mdns,
                upnp: self.upnp,
                autonat: true,
                dcutr: true,
                relay_client: true,
                relay_server: self.relay_server,
                kad_client_only: self.kad_mode == KadMode::Client,
            },
            NetMode::Tor => FeaturePlan {
                tcp: false,
                quic: false,
                system_dns: false,
                onion_dial: tor_available,
                tor_dial_all: tor_available,
                onion_listen: tor_available && self.onion_listen,
                identify: false,
                mdns: false,
                upnp: false,
                autonat: false,
                dcutr: false,
                relay_client: false,
                relay_server: false,
                kad_client_only: true,
            },
        }
    }

    /// 실제로 `listen_on`할 주소. 직접 연결 스웜은 onion 주소를 빼고, Tor 스웜은 onion 서비스 표식 하나뿐입니다.
    pub fn effective_listen_addrs(&self) -> Vec<Multiaddr> {
        let plan = self.plan();
        match self.mode {
            NetMode::Normal => self
                .listen
                .iter()
                .filter(|a| !addr::is_onion(a))
                .filter(|a| {
                    let udp = addr::is_udp(a);
                    let tcp = a.iter().any(|p| matches!(p, Protocol::Tcp(_)));
                    addr::is_relayed(a) || (udp && plan.quic) || (tcp && plan.tcp)
                })
                .cloned()
                .collect(),
            NetMode::Tor if plan.onion_listen => vec![addr::onion_listen_marker(self.onion_port)],
            NetMode::Tor => Vec::new(),
        }
    }

    /// 부트스트랩 주소 중 이 스웜에서 걸 수 있는 것.
    pub fn effective_bootstrap(&self) -> Vec<Multiaddr> {
        let plan = self.plan();
        self.bootstrap
            .iter()
            .filter(|a| bootstrap_usable(self.mode, &plan, a))
            .cloned()
            .collect()
    }

    /// Arti 상태 디렉터리.
    pub fn arti_state_dir(&self) -> PathBuf {
        self.data_dir.join("arti").join("state")
    }

    /// Arti 캐시 디렉터리.
    pub fn arti_cache_dir(&self) -> PathBuf {
        self.data_dir.join("arti").join("cache")
    }

    /// 미리 알릴 외부 주소. Tor 스웜은 자기 주소를 알리지 않으므로 비어 있습니다.
    pub fn effective_external_addrs(&self) -> Vec<Multiaddr> {
        match self.mode {
            NetMode::Normal => self
                .external_addrs
                .iter()
                .filter(|a| !addr::is_onion(a))
                .cloned()
                .collect(),
            NetMode::Tor => Vec::new(),
        }
    }

    /// 다이얼(전송 계층 연결 + 보안·다중화 협상) 제한 시간.
    pub fn dial_timeout(&self) -> Duration {
        match self.mode {
            NetMode::Normal => Duration::from_secs(20),
            NetMode::Tor => TOR_DIAL_TIMEOUT,
        }
    }
}

/// 이 스웜에서 걸 수 있는 부트스트랩 주소인지(Tor 스웜은 Arti로 걸 수 있는 TCP·onion 주소만).
pub(crate) fn bootstrap_usable(mode: NetMode, plan: &FeaturePlan, a: &Multiaddr) -> bool {
    match mode {
        NetMode::Tor => plan.tor_dial_all && addr::tor_target(a).is_some(),
        NetMode::Normal => !addr::is_onion(a) || plan.onion_dial,
    }
}

/// Arti 경유 연결 제한 시간(첫 연결은 Tor 부트스트랩까지 포함).
pub const TOR_DIAL_TIMEOUT: Duration = Duration::from_secs(120);

impl fmt::Debug for NetConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // 키는 출력하지 않고 PeerId만 보입니다.
        f.debug_struct("NetConfig")
            .field("mode", &self.mode)
            .field("peer_id", &self.keypair.public().to_peer_id())
            .field("listen", &self.listen)
            .field("bootstrap", &self.bootstrap)
            .field("mdns", &self.mdns)
            .field("upnp", &self.upnp)
            .field("relay_server", &self.relay_server)
            .field("max_connections", &self.max_connections)
            .field("kad_mode", &self.kad_mode)
            .field("external_addrs", &self.external_addrs)
            .field("sticky_relays", &self.sticky_relays)
            .field("onion_listen", &self.onion_listen)
            .field("tor_share", &self.tor_share.is_some())
            .field("tor_isolated", &self.tor_isolated)
            // 브리지 줄은 비공개 브리지 주소라 로그에 남기지 않습니다.
            .field(
                "tor_bridges",
                &self.tor_bridges.as_ref().map(|b| b.lines.len()),
            )
            .finish_non_exhaustive()
    }
}

/// 스웜 종류별로 켜고 끄는 구성 요소. [`NetConfig::plan`]이 만듭니다.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FeaturePlan {
    /// TCP 직접 전송.
    pub tcp: bool,
    /// QUIC 직접 전송.
    pub quic: bool,
    /// 시스템 DNS로 `/dns` 주소 해석(Tor 스웜은 DNS 누출을 막기 위해 끔).
    pub system_dns: bool,
    /// Arti로 onion 주소에 접속(Tor 스웜, `tor` 기능이 있을 때).
    pub onion_dial: bool,
    /// 모든 나가는 연결을 Arti로(Tor 스웜).
    pub tor_dial_all: bool,
    /// onion 서비스로 수신(Tor 스웜).
    pub onion_listen: bool,
    /// identify(주소 광고). Tor 스웜은 끔.
    pub identify: bool,
    pub mdns: bool,
    /// 공유기 UPnP 포트 열기(직접 연결 스웜에서 켰을 때).
    pub upnp: bool,
    pub autonat: bool,
    pub dcutr: bool,
    pub relay_client: bool,
    pub relay_server: bool,
    /// Kademlia를 클라이언트로만(Tor 스웜: DHT에 자기 주소가 올라가지 않게).
    pub kad_client_only: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tor_mode_disables_leaky_features() {
        let mut cfg = NetConfig::new(NetMode::Tor, Keypair::generate_ed25519(), "/tmp/x");
        // 켜 두었어도 Tor 스웜에서는 꺼져야 한다.
        cfg.mdns = true;
        cfg.upnp = true;
        cfg.relay_server = true;
        cfg.kad_mode = KadMode::Server;
        cfg.listen = vec![
            "/ip4/0.0.0.0/tcp/4001".parse().unwrap(),
            "/ip4/0.0.0.0/udp/4001/quic-v1".parse().unwrap(),
        ];
        let plan = cfg.plan();
        assert!(!plan.tcp && !plan.quic && !plan.system_dns);
        assert!(!plan.identify && !plan.mdns && !plan.upnp && !plan.autonat && !plan.dcutr);
        assert!(!plan.relay_client && !plan.relay_server);
        assert!(plan.kad_client_only);
        cfg.external_addrs = vec!["/ip4/1.2.3.4/tcp/4001".parse().unwrap()];
        assert!(cfg.effective_external_addrs().is_empty());
        let listen = cfg.effective_listen_addrs();
        assert!(listen.iter().all(|a| {
            !a.iter()
                .any(|p| matches!(p, Protocol::Tcp(_) | Protocol::Udp(_)))
        }));
        if cfg!(feature = "tor") {
            assert!(plan.onion_listen && plan.onion_dial && plan.tor_dial_all);
            assert_eq!(listen, vec![addr::onion_listen_marker(DEFAULT_ONION_PORT)]);
        } else {
            assert!(listen.is_empty());
        }
    }

    #[test]
    fn normal_mode_defaults() {
        let cfg = NetConfig::new(NetMode::Normal, Keypair::generate_ed25519(), "/tmp/x");
        let plan = cfg.plan();
        assert!(
            plan.tcp
                && plan.quic
                && plan.identify
                && plan.autonat
                && plan.dcutr
                && plan.relay_client
        );
        assert!(plan.mdns);
        assert!(!plan.relay_server && !plan.tor_dial_all && !plan.onion_listen);
        // 시험이 공유기를 건드리지 않게 기본은 꺼짐.
        assert!(!plan.upnp);
        // 직접 연결 스웜은 Arti를 전혀 쓰지 않습니다.
        assert!(!plan.onion_dial);
        assert_eq!(cfg.effective_listen_addrs().len(), 2);
    }

    #[test]
    fn tor_exit_only_swarm_has_no_listener() {
        let mut cfg = NetConfig::new(NetMode::Tor, Keypair::generate_ed25519(), "/tmp/x");
        cfg.onion_listen = false;
        let plan = cfg.plan();
        assert!(!plan.onion_listen && !plan.tcp && !plan.quic && !plan.identify);
        assert_eq!(plan.tor_dial_all, cfg!(feature = "tor"));
        assert!(cfg.effective_listen_addrs().is_empty());
    }

    #[test]
    fn tor_bootstrap_filters_udp() {
        let mut cfg = NetConfig::new(NetMode::Tor, Keypair::generate_ed25519(), "/tmp/x");
        cfg.bootstrap = vec![
            "/ip4/1.2.3.4/udp/4001/quic-v1".parse().unwrap(),
            "/dns4/boot.example.org/tcp/4001".parse().unwrap(),
        ];
        let b = cfg.effective_bootstrap();
        if cfg!(feature = "tor") {
            assert_eq!(
                b,
                vec![
                    "/dns4/boot.example.org/tcp/4001"
                        .parse::<Multiaddr>()
                        .unwrap()
                ]
            );
        } else {
            assert!(b.is_empty());
        }
    }

    #[test]
    fn debug_hides_key() {
        let cfg = NetConfig::new(NetMode::Normal, Keypair::generate_ed25519(), "/tmp/x");
        let s = format!("{cfg:?}");
        assert!(s.contains("peer_id"));
        assert!(!s.contains("secret"));
    }
}
