//! Tor 모드 누출 시험(오프라인): 127.0.0.1의 미끼(honeypot) TCP·UDP 수신기에 Tor 모드 스웜이 어떤 경로로도
//! 직접 연결하지 않는지, 스웜 구성(전송·동작)이 Tor 모드 끄기 목록을 지키는지 봅니다.
//!
//! - 미끼가 실제로 잡는다는 것을 먼저 일반 모드로 확인합니다(양성 대조).
//! - 동기화는 고정 대기 없이 합니다: 각 동작의 결과(다이얼 실패 등)를 기다리고, Arti 경유 시도 수가 늘었는지로
//!   'Tor 경로를 탔다'를 확인한 뒤, 미끼에 표지 연결·데이터그램을 보내 그보다 앞서 들어온 것이 있는지 셉니다
//!   (수락 대기열과 UDP 수신은 도착 순서대로라 표지가 보이면 앞선 것은 모두 보인 것).
//! - Arti는 루프백·사설 주소를 망에 나가기 전에 거절하므로 Tor 망 없이도 끝납니다.

#![cfg(feature = "tor")]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::mpsc;
use yangpa_core::wire::YpRequest;
use yangpa_net::{
    KadMode, Keypair, Multiaddr, NetConfig, NetError, NetEvent, NetHandle, NetMode, PeerId,
    TRANSPORT_ARTI_ALL, TRANSPORT_ONION_LISTEN, TorIsolation,
};

/// 이벤트·표지를 기다리는 상한(진행 보장용 안전망, 동기화 수단이 아님).
const LIVENESS: Duration = Duration::from_secs(60);

/// 127.0.0.1의 같은 포트 번호에 TCP·UDP로 묶인 미끼.
struct Honeypot {
    port: u16,
    tcp_seen: mpsc::UnboundedReceiver<SocketAddr>,
    udp_seen: mpsc::UnboundedReceiver<SocketAddr>,
    _tasks: Vec<tokio::task::JoinHandle<()>>,
}

/// 표지 앞에 들어온 낯선 연결·데이터그램 수.
#[derive(Debug, Default, PartialEq, Eq)]
struct Hits {
    tcp: usize,
    udp: usize,
}

impl Honeypot {
    async fn bind() -> Self {
        // UDP 포트가 다른 프로그램에 잡혀 있으면 새 포트로 다시 고릅니다.
        for _ in 0..32 {
            let tcp = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
            let port = tcp.local_addr().unwrap().port();
            let Ok(udp) = UdpSocket::bind(("127.0.0.1", port)).await else {
                continue;
            };
            let (ttx, tcp_seen) = mpsc::unbounded_channel();
            let (utx, udp_seen) = mpsc::unbounded_channel();
            let t1 = tokio::spawn(async move {
                // 연결은 받자마자 닫습니다(상대 주소만 기록).
                while let Ok((_s, from)) = tcp.accept().await {
                    if ttx.send(from).is_err() {
                        break;
                    }
                }
            });
            let udp = Arc::new(udp);
            let t2 = tokio::spawn(async move {
                let mut buf = vec![0u8; 65536];
                while let Ok((_, from)) = udp.recv_from(&mut buf).await {
                    if utx.send(from).is_err() {
                        break;
                    }
                }
            });
            return Honeypot {
                port,
                tcp_seen,
                udp_seen,
                _tasks: vec![t1, t2],
            };
        }
        panic!("미끼 포트를 고르지 못함");
    }

    fn tcp(&self) -> Multiaddr {
        format!("/ip4/127.0.0.1/tcp/{}", self.port).parse().unwrap()
    }

    fn quic(&self) -> Multiaddr {
        format!("/ip4/127.0.0.1/udp/{}/quic-v1", self.port)
            .parse()
            .unwrap()
    }

    /// 미끼를 가리키는 모든 꼴의 주소(IP·이름·IPv6·QUIC·릴레이 회로).
    fn every_addr(&self, relay: PeerId) -> Vec<Multiaddr> {
        let p = self.port;
        [
            format!("/ip4/127.0.0.1/tcp/{p}"),
            format!("/dns4/localhost/tcp/{p}"),
            format!("/dns/localhost/tcp/{p}"),
            format!("/ip6/::1/tcp/{p}"),
            format!("/ip4/127.0.0.1/udp/{p}/quic-v1"),
            format!("/ip4/127.0.0.1/tcp/{p}/p2p/{relay}/p2p-circuit"),
        ]
        .iter()
        .map(|s| s.parse().unwrap())
        .collect()
    }

    /// 지금까지 들어온 낯선 TCP 연결·UDP 데이터그램 수. 표지를 보내 그보다 앞선 것을 모두 셉니다.
    async fn flush(&mut self) -> Hits {
        let mut hits = Hits::default();
        let sentinel = TcpStream::connect(("127.0.0.1", self.port)).await.unwrap();
        let me = sentinel.local_addr().unwrap();
        loop {
            let from = tokio::time::timeout(LIVENESS, self.tcp_seen.recv())
                .await
                .expect("TCP 표지가 와야 함")
                .unwrap();
            if from == me {
                break;
            }
            hits.tcp += 1;
        }
        drop(sentinel);
        let s = UdpSocket::bind(("127.0.0.1", 0)).await.unwrap();
        let me = s.local_addr().unwrap();
        s.send_to(b"sentinel", ("127.0.0.1", self.port))
            .await
            .unwrap();
        loop {
            let from = tokio::time::timeout(LIVENESS, self.udp_seen.recv())
                .await
                .expect("UDP 표지가 와야 함")
                .unwrap();
            if from == me {
                break;
            }
            hits.udp += 1;
        }
        hits
    }

    /// 낯선 TCP 연결 하나를 기다립니다(양성 대조).
    async fn wait_tcp(&mut self) -> bool {
        matches!(
            tokio::time::timeout(LIVENESS, self.tcp_seen.recv()).await,
            Ok(Some(_))
        )
    }

    /// 낯선 UDP 데이터그램 하나를 기다립니다(양성 대조).
    async fn wait_udp(&mut self) -> bool {
        matches!(
            tokio::time::timeout(LIVENESS, self.udp_seen.recv()).await,
            Ok(Some(_))
        )
    }
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
}

/// 이벤트를 비우기만 하는 데몬 흉내를 붙여 스웜을 띄웁니다.
fn spawn(cfg: NetConfig) -> NetHandle {
    let (h, mut ev) = yangpa_net::spawn(cfg).unwrap();
    tokio::spawn(async move {
        while let Some(e) = ev.recv().await {
            // 받은 요청은 답하지 않고 버립니다(이 시험에서는 들어올 일이 없음).
            if let NetEvent::InboundRequest { reply, .. } = e {
                drop(reply);
            }
        }
    });
    h
}

/// Tor 모드 설정에 일반 모드에서만 의미 있는 값을 일부러 잔뜩 넣습니다(모두 무시돼야 함).
fn hostile_tor_cfg(dir: &std::path::Path, pot: &Honeypot, relay: PeerId) -> NetConfig {
    let mut cfg = NetConfig::new(NetMode::Tor, Keypair::generate_ed25519(), dir);
    cfg.mdns = true;
    cfg.relay_server = true;
    cfg.kad_mode = KadMode::Server;
    cfg.listen = vec![
        "/ip4/127.0.0.1/tcp/0".parse().unwrap(),
        "/ip4/127.0.0.1/udp/0/quic-v1".parse().unwrap(),
    ];
    cfg.external_addrs = vec![pot.tcp(), pot.quic()];
    cfg.bootstrap = pot
        .every_addr(relay)
        .into_iter()
        .map(|a| a.with_p2p(relay).unwrap_or_else(|a| a))
        .collect();
    cfg
}

/// 미끼가 실제로 잡는지: 일반 모드 스웜은 TCP·QUIC·이름 주소로 미끼에 닿는다.
#[tokio::test]
async fn honeypot_catches_normal_mode_dials() {
    init_tracing();
    let mut pot = Honeypot::bind().await;
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = NetConfig::new(NetMode::Normal, Keypair::generate_ed25519(), dir.path());
    cfg.listen = vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()];
    cfg.mdns = false;
    let h = spawn(cfg);

    let h2 = h.clone();
    let tcp = pot.tcp();
    tokio::spawn(async move { h2.dial(PeerId::random(), vec![tcp]).await });
    assert!(
        pot.wait_tcp().await,
        "일반 모드 TCP 다이얼이 미끼에 닿아야 함"
    );

    let h2 = h.clone();
    let quic = pot.quic();
    tokio::spawn(async move { h2.dial(PeerId::random(), vec![quic]).await });
    assert!(
        pot.wait_udp().await,
        "일반 모드 QUIC 다이얼이 미끼에 닿아야 함"
    );

    let h2 = h.clone();
    let name: Multiaddr = format!("/dns4/localhost/tcp/{}", pot.port).parse().unwrap();
    tokio::spawn(async move { h2.dial(PeerId::random(), vec![name]).await });
    assert!(
        pot.wait_tcp().await,
        "일반 모드 이름 주소가 시스템 DNS로 풀려 미끼에 닿아야 함"
    );

    // 일반 모드 구성은 실제로 켜져 있어야 한다(아래 Tor 모드 점검이 의미 있게).
    let i = h.inspect().await.expect("구성 점검");
    assert!(i.identify && i.autonat && i.dcutr && i.relay_client);
    assert!(i.transports.iter().any(|t| t == "tcp"));
    assert!(i.transports.iter().any(|t| t == "quic"));
    assert_eq!(
        h.tor_activity().dials,
        0,
        "일반 모드 IP 다이얼은 Arti를 쓰지 않음"
    );
    h.shutdown();
}

/// Tor 모드 스웜은 직접 다이얼·부트스트랩·주소 캐시·DHT 질의·앱 스트림 어느 경로로도 미끼에 닿지 않는다.
#[tokio::test]
async fn tor_mode_never_touches_local_honeypot() {
    init_tracing();
    let mut pot = Honeypot::bind().await;
    let dir = tempfile::tempdir().unwrap();
    let boot = PeerId::random();
    let h = spawn(hostile_tor_cfg(dir.path(), &pot, boot));

    // 1) 부트스트랩: 시작할 때 건 다이얼의 결과를 같은 피어에 대한 다이얼로 기다립니다.
    let err = tokio::time::timeout(LIVENESS, h.dial(boot, Vec::new()))
        .await
        .expect("부트스트랩 다이얼이 끝나야 함")
        .unwrap_err();
    assert!(
        matches!(err, NetError::Dial(_) | NetError::NoAddresses),
        "{err:?}"
    );

    // 2) 실행 중에 추가한 부트스트랩.
    let boot2 = PeerId::random();
    h.add_bootstrap(
        pot.every_addr(boot2)
            .into_iter()
            .map(|a| a.with_p2p(boot2).unwrap_or_else(|a| a))
            .collect(),
    );
    let _ = tokio::time::timeout(LIVENESS, h.dial(boot2, Vec::new()))
        .await
        .expect("추가 부트스트랩 다이얼이 끝나야 함");

    // 3) 직접 다이얼(모든 꼴의 주소).
    let target = PeerId::random();
    let err = tokio::time::timeout(LIVENESS, h.dial(target, pot.every_addr(boot)))
        .await
        .expect("다이얼이 끝나야 함")
        .unwrap_err();
    assert!(
        matches!(err, NetError::Dial(_) | NetError::NoAddresses),
        "{err:?}"
    );

    // 4) 주소 캐시에 넣어 둔 뒤 요청(요청이 알아서 다이얼).
    let cached = PeerId::random();
    h.add_addresses(cached, pot.every_addr(boot));
    let r = tokio::time::timeout(LIVENESS, h.request(cached, YpRequest::Ping))
        .await
        .expect("요청이 끝나야 함");
    assert!(r.is_err(), "{r:?}");

    // 5) DHT 질의·기록(부트스트랩 피어 주소로 다이얼).
    let _ = tokio::time::timeout(LIVENESS, h.find_peer(PeerId::random())).await;
    let _ = tokio::time::timeout(
        LIVENESS,
        h.put_record(b"yangpa/v1/leak-test".to_vec(), b"x".to_vec()),
    )
    .await;

    // 6) libp2p 밖 앱 스트림(링크 미리보기 경로): 로컬 이름·주소는 Arti가 거절하고 오류에 주소가 없다.
    for host in [
        "127.0.0.1",
        "localhost",
        "LOCALHOST",
        "::1",
        "10.0.0.1",
        "0.0.0.0",
    ] {
        let err =
            tokio::time::timeout(LIVENESS, h.tor_connect(host, pot.port, TorIsolation::new()))
                .await
                .expect("Tor 스트림 시도가 끝나야 함")
                .unwrap_err();
        let msg = err.to_string();
        assert!(matches!(err, NetError::Tor(_)), "{err:?}");
        assert!(
            !msg.contains(&pot.port.to_string()) && !msg.contains(host),
            "오류에 대상이 드러나면 안 됨: {msg}"
        );
    }

    // 모든 TCP 꼴 주소가 실제로 Arti 경로를 탔다(조용히 버려진 것이 아니라).
    let act = h.tor_activity();
    assert!(act.dials >= 4, "Arti 다이얼 수: {act:?}");
    assert_eq!(act.streams, 6, "{act:?}");

    assert_eq!(pot.flush().await, Hits::default(), "Tor 모드가 미끼에 닿음");
    h.shutdown();
}

/// Tor 모드 스웜 구성: 전송은 Arti(+onion 수신)뿐이고 identify·mDNS·AutoNAT·DCUtR·릴레이·Kademlia 서버가 꺼져 있으며
/// 알리는 외부 주소가 없다. 설정에 반대 값을 넣어도 마찬가지.
#[tokio::test]
async fn tor_mode_swarm_structure() {
    init_tracing();
    let pot = Honeypot::bind().await;
    let dir = tempfile::tempdir().unwrap();
    let h = spawn(hostile_tor_cfg(dir.path(), &pot, PeerId::random()));
    let i = h.inspect().await.expect("구성 점검");
    assert_tor_structure(&i);
    // onion 주소가 나온 뒤에도 수신 주소는 onion뿐(Tor 망 없이 로컬 키로 나옴).
    let deadline = tokio::time::Instant::now() + LIVENESS;
    loop {
        if h.status().await.onion_addr.is_some() {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "onion 주소");
        // onion 키는 배경 작업이 만들므로 상태를 짧은 간격으로 다시 봅니다(조건 확인용 폴링).
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let i = h.inspect().await.expect("구성 점검");
    assert_tor_structure(&i);
    let s = h.status().await;
    assert!(s.external_addrs.is_empty() && s.relay_addrs.is_empty());
    assert_eq!(s.shareable_addrs().len(), 1);
    assert!(yangpa_net::addr::is_onion(&s.shareable_addrs()[0]));
    h.shutdown();
}

/// 실행 중에 일반 → Tor 모드로 바꾸면 스웜이 통째로 다시 조립돼 IP 전송이 남지 않는다.
#[tokio::test]
async fn restart_into_tor_mode_rebuilds_transports() {
    init_tracing();
    let mut pot = Honeypot::bind().await;
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = NetConfig::new(NetMode::Normal, Keypair::generate_ed25519(), dir.path());
    cfg.listen = vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()];
    cfg.mdns = false;
    let h = spawn(cfg);
    let before = h.inspect().await.expect("구성 점검");
    assert!(before.transports.iter().any(|t| t == "tcp"));

    let tor_dir = tempfile::tempdir().unwrap();
    let boot = PeerId::random();
    h.restart(hostile_tor_cfg(tor_dir.path(), &pot, boot))
        .await
        .unwrap();
    let i = h.inspect().await.expect("구성 점검");
    assert_tor_structure(&i);
    let _ = tokio::time::timeout(LIVENESS, h.dial(boot, pot.every_addr(boot)))
        .await
        .expect("다이얼이 끝나야 함");
    assert!(h.tor_activity().dials >= 1);
    assert_eq!(pot.flush().await, Hits::default(), "재시작 뒤 미끼에 닿음");
    h.shutdown();
}

fn assert_tor_structure(i: &yangpa_net::NetInspection) {
    assert_eq!(i.mode, NetMode::Tor);
    assert_eq!(
        i.transports,
        vec![
            TRANSPORT_ARTI_ALL.to_string(),
            TRANSPORT_ONION_LISTEN.to_string()
        ],
        "Tor 모드 전송은 Arti뿐"
    );
    assert!(!i.identify, "identify(주소 광고)는 꺼져야 함");
    assert!(!i.mdns, "mDNS는 꺼져야 함");
    assert!(!i.autonat, "AutoNAT은 꺼져야 함");
    assert!(!i.dcutr, "DCUtR은 꺼져야 함");
    assert!(!i.relay_client && !i.relay_server, "릴레이는 꺼져야 함");
    assert!(
        !i.kad_server,
        "Kademlia는 클라이언트 전용(자기 주소를 싣지 않음)"
    );
    assert!(i.external_addrs.is_empty(), "{:?}", i.external_addrs);
    for a in &i.listen_addrs {
        assert!(yangpa_net::addr::is_onion(a), "{a}");
    }
}
