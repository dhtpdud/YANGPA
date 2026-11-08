//! Tor 누출 시험(오프라인, 데몬 전체): Tor로 도는 노드가 데몬의 어느 경로로도 127.0.0.1의 미끼(honeypot)
//! TCP·UDP 수신기나 직접 연결(`--direct`) 친구 노드에 직접 연결하지 않는지 봅니다.
//!
//! 경로: 직접 연결로 쓰던 기기를 Tor로 다시 띄운 뒤의 재연결·메시지 전파, 부트스트랩 주소(시작 설정), 평문 주소가
//! 든 친구 링크, 위치 기록 조회와 기록에 든 평문 주소로의 연결(DHT 로컬 저장소에 미끼 주소가 든 기록을 심어 둠),
//! 링크 미리보기, 블롭 내려받기, 일반 채널 통화(Tor 통화). 같은 미끼가 직접 연결 노드에서는 실제로 잡힌다는 것(양성
//! 대조)을 먼저 확인합니다.
//!
//! 고정 대기 없이: 각 동작의 결과와 Arti 경유 시도 수(`tor_activity`)로 'Tor 경로를 탔다'를 확인한 뒤 미끼에
//! 표지 연결·데이터그램을 보내 그보다 앞서 들어온 것을 셉니다(수락 대기열·UDP 수신은 도착 순서대로).
//! Arti는 루프백·사설 주소를 망에 나가기 전에 거절하므로 Tor 망 없이 끝납니다.
//! 실제 Tor 망에서 패킷을 잡아 보는 시험은 `scripts/tor-leak-check.sh`(`.github/workflows/tor-leak.yml`)입니다.

use crate::common;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use common::{Cluster, TestNode, befriend, eventually};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::mpsc;
use yangpa_core::ids::{BlobId, SpaceId};
use yangpa_core::links::FriendLink;
use yangpa_daemon::testkit;
use yangpa_daemon::testkit::net as tnet;
use yangpa_daemon::{Node, NodeConfig};
use yangpa_net::{Multiaddr, PeerId, TRANSPORT_ARTI_ALL, TRANSPORT_ONION_LISTEN};
use yangpa_rpc::api::{
    AttachmentInput, BlobFetchParams, BlobParams, Empty, FriendLinkCreateParams, MessageListParams,
    MessageSendParams, NetMode, PeerSetParams, SpaceChannelParams, UrlParams, m,
};

/// 표지·양성 대조를 기다리는 상한(진행 보장용 안전망).
const LIVENESS: Duration = Duration::from_secs(60);

/// 127.0.0.1의 같은 포트 번호에 TCP·UDP로 묶인 미끼.
struct Honeypot {
    port: u16,
    tcp_seen: mpsc::UnboundedReceiver<SocketAddr>,
    udp_seen: mpsc::UnboundedReceiver<SocketAddr>,
    _tasks: Vec<tokio::task::JoinHandle<()>>,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Hits {
    tcp: usize,
    udp: usize,
}

impl Honeypot {
    async fn bind() -> Self {
        for _ in 0..32 {
            let tcp = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
            let port = tcp.local_addr().unwrap().port();
            let Ok(udp) = UdpSocket::bind(("127.0.0.1", port)).await else {
                continue;
            };
            let (ttx, tcp_seen) = mpsc::unbounded_channel();
            let (utx, udp_seen) = mpsc::unbounded_channel();
            let t1 = tokio::spawn(async move {
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

    fn tcp(&self, peer: PeerId) -> String {
        format!("/ip4/127.0.0.1/tcp/{}/p2p/{peer}", self.port)
    }

    /// 위치 기록에 넣을 평문 주소(TCP·이름·QUIC).
    fn record_addrs(&self) -> Vec<Multiaddr> {
        [
            format!("/ip4/127.0.0.1/tcp/{}", self.port),
            format!("/dns4/localhost/tcp/{}", self.port),
            format!("/ip4/127.0.0.1/udp/{}/quic-v1", self.port),
        ]
        .iter()
        .map(|a| a.parse().unwrap())
        .collect()
    }

    /// 표지 앞에 들어온 낯선 연결·데이터그램 수.
    async fn flush(&mut self) -> Hits {
        let mut hits = Hits::default();
        let sentinel = TcpStream::connect(("127.0.0.1", self.port)).await.unwrap();
        let me = sentinel.local_addr().unwrap();
        loop {
            let from = tokio::time::timeout(LIVENESS, self.tcp_seen.recv())
                .await
                .expect("TCP 표지")
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
                .expect("UDP 표지")
                .unwrap();
            if from == me {
                break;
            }
            hits.udp += 1;
        }
        hits
    }

    async fn wait_tcp(&mut self, what: &str) {
        tokio::time::timeout(LIVENESS, self.tcp_seen.recv())
            .await
            .unwrap_or_else(|_| panic!("양성 대조 실패: {what}"))
            .expect("미끼 작업");
    }
}

/// 미리보기가 (직접 조회일 때) 로컬 미끼도 조회하게 합니다.
fn leak_tweak(cfg: &mut NodeConfig) {
    cfg.sharing.link_preview.allow_private_addrs = true;
    cfg.sharing.link_preview.allow_any_port = true;
}

/// [`leak_tweak`]에 더해 Tor로 띄웁니다.
fn leak_tweak_tor(cfg: &mut NodeConfig) {
    leak_tweak(cfg);
    cfg.net.direct = false;
}

/// `t`의 스웜이 Tor 끄기 목록을 지키는지(구조).
async fn assert_tor_structure(t: &TestNode) {
    let i = tnet::inspect(&t.node).await.expect("구성 점검");
    assert_eq!(i.mode, yangpa_net::NetMode::Tor);
    assert_eq!(
        i.transports,
        vec![
            TRANSPORT_ARTI_ALL.to_string(),
            TRANSPORT_ONION_LISTEN.to_string()
        ]
    );
    assert!(!i.identify && !i.mdns && !i.autonat && !i.dcutr);
    assert!(!i.relay_client && !i.relay_server && !i.kad_server);
    assert!(i.external_addrs.is_empty(), "{:?}", i.external_addrs);
    for a in &i.listen_addrs {
        assert!(yangpa_net::addr::is_onion(a), "{a}");
    }
    let s = t.ok::<m::NetStatusGet>(Empty).await;
    assert_eq!(s.mode, NetMode::Tor);
    for x in s.listen_addrs.iter().chain(&s.external_addrs) {
        assert!(x.starts_with("/onion3/"), "Tor 모드에서 알리는 주소: {x}");
    }
}

/// 직접 연결로 쓰던 기기를 Tor로 다시 띄우면(예전 빌드의 일반 모드에서 업그레이드) 데몬의 어느 경로로도
/// 미끼·친구 노드에 직접 닿지 않는다(기록해 둔 친구의 평문 주소가 있어도).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tor_mode_daemon_paths_never_touch_clearnet() {
    let cl = Cluster::new();
    let mut pot = Honeypot::bind().await;
    let a = cl.node("a").await;
    let mut b = cl.node_with("b", leak_tweak).await;
    let c = cl.node("c").await;
    let space = befriend(&a, &b).await;
    let channel = a.first_channel(space).await;
    // B는 첨부를 자동으로 받지 않습니다(6에서 Tor로 직접 달라고 할 때까지 이벤트만 가짐).
    b.ok::<m::PeerSet>(PeerSetParams {
        auto_download_max_bytes: Some(0),
        ..Default::default()
    })
    .await;

    // A가 첨부를 보냅니다. B는 받지 않고 이벤트만 가집니다.
    let file = a.dir.join("files");
    std::fs::create_dir_all(&file).unwrap();
    let path = file.join("자료.bin");
    std::fs::write(&path, vec![7u8; 64 * 1024]).unwrap();
    let sent = a
        .ok::<m::MessageSend>(MessageSendParams {
            space,
            channel,
            text: "첨부".into(),
            attachments: vec![AttachmentInput {
                path: path.to_string_lossy().into_owned(),
                ..Default::default()
            }],
            reply_to: None,
            thread: None,
        })
        .await
        .id;
    let blob = attachment_of(&a, space, channel, sent).await;
    eventually("B가 첨부 메시지를 받음", || async {
        testkit::has_body(&b.node, &sent).then_some(())
    })
    .await;

    // B는 직접 연결일 때 미끼를 전혀 모릅니다(미끼에 남은 직접 연결을 Tor 누출로 잘못 세지 않게).
    // 혼자 쓰는 크루(미리보기·통화용)는 미리 만들어 둡니다.
    let crew = tnet::create_crew(&b.node, &[]).await.expect("크루");
    let text_channel = b.first_channel(crew).await;
    let voice = tnet::create_voice_channel(&b.node, crew, "음성")
        .await
        .expect("통화 채널");
    let link = format!("http://127.0.0.1:{}/page", pot.port);

    // Tor로 다시 띄움.
    b.restart_with(leak_tweak_tor).await;
    eventually("onion 주소", || async {
        b.ok::<m::NetStatusGet>(Empty).await.onion_addr.map(|_| ())
    })
    .await;
    let tor_peer = b.peer();
    assert_tor_structure(&b).await;
    let start = tnet::tor_activity(&b.node);

    // A 기기로 서명한, 미끼 주소가 든 위치 기록을 B의 DHT에 심습니다. 1)의 전송이 실패하면 B는 이 기록으로 다시
    // 찾아 걸고(`push_to`), 4)에서는 직접 찾아 겁니다. 어느 쪽이든 Arti로만 나가야 합니다.
    let a_dev = testkit::device_id(&a.node).expect("기기");
    let planted_peer = PeerId::random();
    let planted = pot.record_addrs();
    tnet::plant_locator_record(&a.node, &b.node, planted_peer, &planted)
        .await
        .expect("위치 기록 심기");

    // 1) 재연결·전파: 기록해 둔 A의 127.0.0.1 주소로 메시지를 보내려 함.
    let id = b.send(space, channel, "Tor로 보냄").await;
    assert!(testkit::has_body(&b.node, &id));

    // 2) 평문 릴레이 주소가 든 친구 링크(C의 링크를 미끼 주소로 바꿈).
    let created = c
        .ok::<m::FriendLinkCreate>(FriendLinkCreateParams {
            expires_in_secs: Some(3600),
            single_use: Some(true),
        })
        .await;
    let mut fl = FriendLink::parse(&created.url).expect("링크");
    fl.contact.relay_addrs = vec![format!(
        "/ip4/127.0.0.1/tcp/{}/p2p/{}/p2p-circuit",
        pot.port,
        PeerId::random()
    )];
    let url = fl.to_url().expect("링크");
    let _ = b.call::<m::FriendAdd>(UrlParams { url }).await;

    // 3) 위치 기록 조회(A의 기기)와 기록에 든 주소로 연결: 기록은 찾지만 연결은 Arti로만 시도해 실패한다.
    let found = tnet::locator_lookup(&b.node, &a.user(), &a_dev)
        .await
        .expect("심은 위치 기록을 찾음");
    assert_eq!(found, (planted_peer, planted.clone()));
    let before = tnet::tor_activity(&b.node).dials;
    assert!(
        b.node.connect(found.0, found.1).await.is_err(),
        "Tor 모드에서 위치 기록의 로컬 주소로 연결됨"
    );
    assert!(
        tnet::tor_activity(&b.node).dials > before,
        "위치 기록 주소로의 연결이 Arti를 거치지 않음"
    );

    // 4) 링크 미리보기는 Arti로만: 미끼(로컬 주소)는 Arti가 거절해 미리보기가 없다.
    let before = tnet::tor_activity(&b.node).streams;
    let hidden = b.send(crew, text_channel, &format!("Tor {link}")).await;
    let embeds = b
        .ok::<m::MessageList>(MessageListParams {
            space: crew,
            channel: text_channel,
            before: None,
            limit: Some(50),
            thread: None,
        })
        .await
        .messages
        .into_iter()
        .find(|v| v.id == hidden)
        .expect("보낸 메시지")
        .embeds
        .len();
    assert_eq!(embeds, 0);
    assert!(
        tnet::tor_activity(&b.node).streams > before,
        "Tor 모드 미리보기가 Arti를 거치지 않음"
    );

    // 5) 블롭 내려받기(A에게서).
    b.ok::<m::BlobFetch>(BlobFetchParams {
        space,
        blob,
        priority: Some(255),
    })
    .await;

    // 6) 통화: Tor로 도는 기기의 일반 채널 통화는 WebRTC 없이 Tor 통화(ICE 서버 없음)이고, 소리 패킷은 노드 연결로만
    //    갑니다(아래 미끼 검사가 그 밖의 길이 없었음을 봄).
    let j = b
        .ok::<m::VoiceJoin>(SpaceChannelParams {
            space: crew,
            channel: voice,
        })
        .await;
    assert_eq!(j.topology, yangpa_rpc::api::Topology::Tor);
    assert!(j.ice_servers.is_empty(), "Tor 통화에 ICE 서버를 줌");
    b.ok::<m::VoiceMedia>(yangpa_rpc::api::VoiceMediaParams {
        space: crew,
        channel: voice,
        call: j.call,
        data: "AAEC".into(),
        to: None,
    })
    .await;

    // 장벽: 위 경로들이 실제로 Arti 다이얼을 냈다(조용히 버려진 것이 아니라).
    eventually("Arti 경유 다이얼", || async {
        let act = tnet::tor_activity(&b.node);
        (act.dials >= start.dials + 3).then_some(())
    })
    .await;
    assert_eq!(pot.flush().await, Hits::default(), "Tor 모드가 미끼에 닿음");

    // A·C는 B의 Tor 노드를 본 적이 없다(직접 연결이 없었음).
    for n in [&a, &c] {
        let seen = n.ok::<m::NetPeers>(Empty).await;
        assert!(
            !seen.iter().any(|p| p.peer_id == tor_peer.to_string()),
            "{}가 Tor 모드 B와 직접 연결됨: {seen:?}",
            n.name
        );
    }
    let s = b
        .call::<m::BlobStatus>(BlobParams { space, blob })
        .await
        .ok();
    assert!(
        s.is_none_or(|s| s.total_chunks == 0 || s.have_chunks < s.total_chunks),
        "Tor 모드에서 로컬 주소로 첨부를 받음"
    );
    assert_tor_structure(&b).await;
}

/// 양성 대조: 같은 설정의 직접 연결 노드(`--direct`)는 같은 경로(부트스트랩·링크 미리보기)로 미끼에 실제로 닿는다.
/// 미끼가 잡지 못해서 Tor 시험이 통과하는 일이 없게 합니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn honeypot_catches_normal_mode_daemon() {
    let cl = Cluster::new();
    let mut pot = Honeypot::bind().await;
    let n = cl.node_with("n", leak_tweak).await;
    n.add_bootstrap(&pot.tcp(PeerId::random()));
    pot.wait_tcp("일반 모드 부트스트랩").await;
    let crew = tnet::create_crew(&n.node, &[]).await.expect("크루");
    let ch = n.first_channel(crew).await;
    n.send(crew, ch, &format!("http://127.0.0.1:{}/page", pot.port))
        .await;
    pot.wait_tcp("일반 모드 링크 미리보기").await;
    assert_eq!(tnet::tor_activity(&n.node).streams, 0);
    let i = tnet::inspect(&n.node).await.expect("구성 점검");
    assert!(i.transports.iter().any(|t| t == "tcp"));

    // 위치 기록에 든 평문 주소로의 연결(따로 둔 미끼라 부트스트랩 재시도와 섞이지 않음).
    let mut pot2 = Honeypot::bind().await;
    let m_node = cl.node("m").await;
    let m_dev = testkit::device_id(&m_node.node).expect("기기");
    let planted_peer = PeerId::random();
    let planted = pot2.record_addrs();
    tnet::plant_locator_record(&m_node.node, &n.node, planted_peer, &planted)
        .await
        .expect("위치 기록 심기");
    let found = tnet::locator_lookup(&n.node, &m_node.user(), &m_dev)
        .await
        .expect("심은 위치 기록을 찾음");
    assert_eq!(found, (planted_peer, planted));
    let _ = n.node.connect(found.0, found.1).await;
    pot2.wait_tcp("일반 모드 위치 기록 주소").await;
}

/// 처음부터 Tor로 띄운 노드는 설정의 평문 부트스트랩 주소로도 직접 나가지 않는다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tor_start_with_clearnet_bootstrap_stays_on_tor() {
    let cl = Cluster::new();
    let mut pot = Honeypot::bind().await;
    let mut cfg = NodeConfig::for_test(cl.dir("t"));
    cfg.net.direct = false;
    cfg.net.mdns = true;
    cfg.net.bootstrap = vec![
        pot.tcp(PeerId::random()).parse().unwrap(),
        format!("/dns4/localhost/tcp/{}/p2p/{}", pot.port, PeerId::random())
            .parse()
            .unwrap(),
        format!(
            "/ip4/127.0.0.1/udp/{}/quic-v1/p2p/{}",
            pot.port,
            PeerId::random()
        )
        .parse()
        .unwrap(),
    ];
    let node: Arc<Node> = Node::start(cfg).await.expect("노드 시작");
    eventually("부트스트랩 다이얼이 Arti로 감", || async {
        (tnet::tor_activity(&node).dials >= 2).then_some(())
    })
    .await;
    let i = tnet::inspect(&node).await.expect("구성 점검");
    assert_eq!(i.mode, yangpa_net::NetMode::Tor);
    assert_eq!(
        i.transports,
        vec![
            TRANSPORT_ARTI_ALL.to_string(),
            TRANSPORT_ONION_LISTEN.to_string()
        ]
    );
    assert!(!i.mdns && !i.identify && !i.kad_server);
    assert_eq!(pot.flush().await, Hits::default(), "Tor 모드가 미끼에 닿음");
    node.shutdown().await;
}

async fn attachment_of(
    n: &TestNode,
    space: SpaceId,
    channel: yangpa_core::ids::ChannelId,
    id: yangpa_core::ids::EventId,
) -> BlobId {
    n.messages(space, channel)
        .await
        .into_iter()
        .find(|v| v.id == id)
        .and_then(|v| v.attachments.first().map(|a| a.blob))
        .expect("첨부")
}

fn tor_with_slow_preview(c: &mut NodeConfig) {
    c.net.direct = false;
    // Tor 첫 회로는 느릴 수 있어 이 시험에서만 넉넉히 둡니다.
    c.sharing.link_preview.timeout = Duration::from_secs(120);
}

/// 실제 Tor 망이 필요합니다: `cargo test -p yangpa-daemon --test tor_leak -- --ignored real_tor_link_preview`.
/// Tor로 도는 노드의 링크 미리보기가 Arti(출구 노드)로 https 페이지를 받아 만들어진다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "실제 Tor 망 접속이 필요함"]
async fn real_tor_link_preview() {
    let cl = Cluster::new();
    let t = cl.node_with("t", tor_with_slow_preview).await;
    eventually("Tor 부트스트랩", || async {
        let s = t.ok::<m::NetStatusGet>(Empty).await;
        (s.bootstrap_progress == Some(100)).then_some(())
    })
    .await;
    let crew = tnet::create_crew(&t.node, &[]).await.expect("크루");
    let ch = t.first_channel(crew).await;
    let id = t.send(crew, ch, "https://example.com/").await;
    let v = t
        .messages(crew, ch)
        .await
        .into_iter()
        .find(|v| v.id == id)
        .expect("보낸 메시지");
    assert_eq!(v.embeds.len(), 1, "Tor 경유 미리보기가 만들어져야 함");
    assert!(tnet::tor_activity(&t.node).streams >= 1);
}
