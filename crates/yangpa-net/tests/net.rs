//! 여러 노드를 127.0.0.1에 띄워 요청-응답·gossip·DHT·차단을 확인합니다.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use yangpa_core::device::DeviceSignKeys;
use yangpa_core::event::{EVENT_VERSION, EventHeader, EventKind, KeyRef, SignedEvent};
use yangpa_core::ids::{ChannelId, ChannelRootId, DeviceId, EventId, SpaceId, UserId};
use yangpa_core::sign::SigningKeypair;
use yangpa_core::wire::{
    GossipMessage, GossipTopic, MAX_WIRE_MESSAGE, WireErrorCode, YpRequest, YpResponse,
};
use yangpa_net::{
    GossipVerdict, KadMode, Keypair, Multiaddr, NetConfig, NetError, NetEvent, NetHandle, NetMode,
    PeerId, RelayNode, StickyRelay,
};

/// 데몬 흉내: 요청에 대한 답. `None`이면 답하지 않고 `reply`를 버립니다.
type Responder = Arc<dyn Fn(&YpRequest) -> Option<YpResponse> + Send + Sync>;
/// 데몬 흉내(직접 처리): 요청과 답할 채널을 받아 늦게 답하거나 붙잡아 둘 수 있습니다.
type Handler = Arc<dyn Fn(YpRequest, oneshot::Sender<YpResponse>) + Send + Sync>;

#[derive(Debug)]
enum Seen {
    Gossip { from: PeerId, msg: GossipMessage },
    Media { from: PeerId, frames: Vec<Vec<u8>> },
    Connected(PeerId),
    Disconnected(PeerId),
}

struct Node {
    h: NetHandle,
    peer: PeerId,
    addr: Multiaddr,
    seen: mpsc::UnboundedReceiver<Seen>,
    _daemon: JoinHandle<()>,
    _dir: tempfile::TempDir,
}

impl Node {
    fn full_addr(&self) -> Multiaddr {
        self.addr.clone().with_p2p(self.peer).unwrap()
    }

    /// 지금까지 쌓인 알림을 비웁니다.
    fn drain(&mut self) {
        while self.seen.try_recv().is_ok() {}
    }

    /// 조건에 맞는 gossip을 기다립니다.
    async fn wait_gossip(
        &mut self,
        limit: Duration,
        mut pred: impl FnMut(&PeerId, &GossipMessage) -> bool,
    ) -> bool {
        let deadline = tokio::time::Instant::now() + limit;
        loop {
            match tokio::time::timeout_at(deadline, self.seen.recv()).await {
                Ok(Some(Seen::Gossip { from, msg })) if pred(&from, &msg) => return true,
                Ok(Some(_)) => continue,
                Ok(None) | Err(_) => return false,
            }
        }
    }

    /// 연결(`true`) 또는 끊김(`false`) 알림을 기다립니다.
    async fn wait_link(&mut self, peer: PeerId, connected: bool, limit: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + limit;
        loop {
            match tokio::time::timeout_at(deadline, self.seen.recv()).await {
                Ok(Some(Seen::Connected(p))) if p == peer && connected => return true,
                Ok(Some(Seen::Disconnected(p))) if p == peer && !connected => return true,
                Ok(Some(_)) => continue,
                Ok(None) | Err(_) => return false,
            }
        }
    }
}

fn default_responder() -> Responder {
    Arc::new(|_| Some(YpResponse::Ok))
}

async fn node(responder: Responder, tweak: impl FnOnce(&mut NetConfig)) -> Node {
    let handler: Handler = Arc::new(move |req, reply| {
        if let Some(r) = responder(&req) {
            let _ = reply.send(r);
        }
    });
    node_with(handler, tweak).await
}

async fn node_with(handler: Handler, tweak: impl FnOnce(&mut NetConfig)) -> Node {
    // `RUST_LOG=yangpa_net=debug`로 실패 원인을 볼 수 있습니다.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = NetConfig::new(NetMode::Normal, Keypair::generate_ed25519(), dir.path());
    cfg.listen = vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()];
    cfg.mdns = false;
    cfg.kad_mode = KadMode::Server;
    tweak(&mut cfg);
    let (h, mut events) = yangpa_net::spawn(cfg).unwrap();
    let peer = h.local_peer_id();

    let (seen_tx, seen) = mpsc::unbounded_channel();
    let dh = h.clone();
    let daemon = tokio::spawn(async move {
        while let Some(ev) = events.recv().await {
            match ev {
                // 시험 노드는 상대를 모두 믿고 압축된 요청도 풉니다(인증 확인은 데몬 몫).
                NetEvent::InboundRequest { req, reply, .. } => match req.open() {
                    Ok((req, _)) => handler(req, reply),
                    Err(resp) => drop(reply.send(*resp)),
                },
                NetEvent::Gossip {
                    from, msg_id, msg, ..
                } => {
                    dh.report_gossip(msg_id, from, GossipVerdict::Accept);
                    let _ = seen_tx.send(Seen::Gossip { from, msg });
                }
                NetEvent::Media { peer, frames } => {
                    let _ = seen_tx.send(Seen::Media { from: peer, frames });
                }
                NetEvent::PeerConnected(p) => drop(seen_tx.send(Seen::Connected(p))),
                NetEvent::PeerDisconnected(p) => drop(seen_tx.send(Seen::Disconnected(p))),
                NetEvent::Status(_) => {}
            }
        }
    });

    let mut addr = None;
    for _ in 0..100 {
        let s = h.status().await;
        if let Some(a) = s
            .listen_addrs
            .into_iter()
            .find(|a| a.to_string().contains("/tcp/"))
        {
            addr = Some(a);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Node {
        h,
        peer,
        addr: addr.expect("수신 주소"),
        seen,
        _daemon: daemon,
        _dir: dir,
    }
}

/// [`signed_events`]의 작성 기기(채널 이벤트는 짧은 서명이라 확인에 키가 필요).
fn events_author() -> SigningKeypair {
    SigningKeypair::from_seed(&[9; 32])
}

fn signed_events(space: SpaceId, n: usize, body_len: usize) -> Vec<SignedEvent> {
    events_with(space, n, |i| {
        let mut body = vec![0u8; body_len.max(8)];
        body[..8].copy_from_slice(&(i as u64).to_le_bytes());
        body
    })
}

/// 줄지 않는 본문(실제 본문은 암호문이라 압축되지 않음).
fn noise_events(space: SpaceId, n: usize, body_len: usize) -> Vec<SignedEvent> {
    events_with(space, n, |i| {
        let mut body = vec![0u8; body_len];
        blake3::Hasher::new()
            .update(&(i as u64).to_le_bytes())
            .finalize_xof()
            .fill(&mut body);
        body
    })
}

fn events_with(space: SpaceId, n: usize, body: impl Fn(usize) -> Vec<u8>) -> Vec<SignedEvent> {
    let dev = events_author();
    (0..n)
        .map(|i| {
            let header = EventHeader {
                v: EVENT_VERSION,
                space,
                channel: Some(ChannelId::random()),
                kind: EventKind::Message,
                author: DeviceId(dev.public()),
                user: UserId([1; 32]),
                parents: vec![],
                lamport: i as u64 + 1,
                created_at: yangpa_core::Timestamp(1),
                key: KeyRef::Record {
                    root: ChannelRootId::random(),
                },
                body_hash: [0; 32],
                body_len: 0,
                scope: None,
                state_anchor: Vec::new(),
            };
            SignedEvent::sign(header, body(i), &dev).unwrap()
        })
        .collect()
}

#[tokio::test]
async fn request_response_roundtrip_and_size_limit() {
    const BODY: usize = 256 * 1024;
    let responder: Responder = Arc::new(|req| {
        Some(match req {
            YpRequest::GetEvents { space, ids } => {
                YpResponse::Events(signed_events(*space, ids.len(), BODY))
            }
            _ => YpResponse::error(WireErrorCode::BadRequest, "지원하지 않음"),
        })
    });
    let a = node(default_responder(), |_| {}).await;
    let b = node(responder, |_| {}).await;

    a.h.dial(b.peer, vec![b.addr.clone()]).await.unwrap();
    assert_eq!(
        a.h.request(b.peer, YpRequest::Ping).await.unwrap(),
        YpResponse::Pong
    );

    // 한도(8MiB) 가까운 응답: 256KiB 본문 30개 ≈ 7.5MiB.
    let space = SpaceId::random();
    let ids: Vec<EventId> = (0..30u8).map(|i| EventId::hash_of(&[i])).collect();
    let res =
        a.h.request(b.peer, YpRequest::GetEvents { space, ids })
            .await
            .unwrap();
    let YpResponse::Events(evs) = res else {
        panic!("이벤트 응답이어야 함: {res:?}")
    };
    assert_eq!(evs.len(), 30);
    let total: usize = evs
        .iter()
        .map(|e| e.body.as_ref().map_or(0, Vec::len))
        .sum();
    assert!(total > MAX_WIRE_MESSAGE * 9 / 10 && total < MAX_WIRE_MESSAGE);
    let keys = DeviceSignKeys::of(&events_author());
    for e in &evs {
        e.verify_with(&keys).unwrap();
    }

    // 한도를 넘는 응답은 잘리지 않고 오류 응답으로 바뀐다.
    let ids: Vec<EventId> = (0..33u8).map(|i| EventId::hash_of(&[i])).collect();
    let res =
        a.h.request(b.peer, YpRequest::GetEvents { space, ids })
            .await
            .unwrap();
    assert!(
        matches!(
            res,
            YpResponse::Error {
                code: WireErrorCode::Internal,
                ..
            }
        ),
        "{res:?}"
    );

    // 주소를 모르는 피어.
    let err =
        a.h.request(PeerId::random(), YpRequest::Ping)
            .await
            .unwrap_err();
    assert!(
        matches!(err, NetError::NoAddresses | NetError::Dial(_)),
        "{err:?}"
    );

    // 상태: 연결 수와 수신 주소.
    let s = a.h.status().await;
    assert_eq!(s.peer_id, a.peer);
    assert!(s.connections >= 1 && s.peers >= 1);
    assert!(
        a.h.peers()
            .await
            .iter()
            .any(|p| p.peer_id == b.peer && !p.relayed_only)
    );
}

/// 통화 미디어 프레임은 연결된 피어에게 한 방향으로 가고(묶여 올 수 있음), 연결이 없는 피어에게는 보내지 않습니다.
#[tokio::test]
async fn media_frames_reach_connected_peer() {
    let a = node(default_responder(), |_| {}).await;
    let mut b = node(default_responder(), |_| {}).await;
    // 연결 전에는 버림(다이얼하지 않음).
    a.h.send_media(b.peer, vec![0]);
    a.h.dial(b.peer, vec![b.addr.clone()]).await.unwrap();
    for i in 1..=20u8 {
        a.h.send_media(b.peer, vec![i; 100]);
    }
    let mut got = Vec::new();
    while got.len() < 20 {
        let ev = tokio::time::timeout(Duration::from_secs(10), b.seen.recv())
            .await
            .expect("미디어 프레임이 와야 함")
            .unwrap();
        if let Seen::Media { from, frames } = ev {
            assert_eq!(from, a.peer);
            got.extend(frames);
        }
    }
    // 동시에 보낸 요청끼리는 순서가 바뀔 수 있습니다(받는 앱이 번호로 맞춤).
    let mut firsts: Vec<u8> = got.iter().map(|f| f[0]).collect();
    firsts.sort();
    assert_eq!(
        firsts,
        (1..=20).collect::<Vec<u8>>(),
        "모두 오고, 연결 전 프레임은 없음"
    );
}

#[tokio::test]
async fn daemon_dropping_reply_sends_error() {
    let a = node(default_responder(), |_| {}).await;
    // b의 데몬은 답하지 않고 reply를 버린다.
    let b = node(Arc::new(|_| None), |_| {}).await;
    a.h.dial(b.peer, vec![b.addr.clone()]).await.unwrap();
    let res =
        a.h.request(
            b.peer,
            YpRequest::GetEvents {
                space: SpaceId::random(),
                ids: vec![],
            },
        )
        .await;
    assert!(
        matches!(
            res,
            Ok(YpResponse::Error {
                code: WireErrorCode::Internal,
                ..
            })
        ),
        "{res:?}"
    );
    // Ping은 데몬을 거치지 않는다.
    assert_eq!(
        a.h.request(b.peer, YpRequest::Ping).await.unwrap(),
        YpResponse::Pong
    );
    b.h.shutdown();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        b.h.request(a.peer, YpRequest::Ping).await,
        Err(NetError::Shutdown)
    );
}

#[tokio::test]
async fn gossip_reaches_members_and_rejects_outsiders() {
    let mut a = node(default_responder(), |_| {}).await;
    let mut b = node(default_responder(), |_| {}).await;
    let mut c = node(default_responder(), |_| {}).await;
    b.h.dial(a.peer, vec![a.addr.clone()]).await.unwrap();
    c.h.dial(a.peer, vec![a.addr.clone()]).await.unwrap();

    let space = SpaceId::random();
    let all = vec![a.peer, b.peer, c.peer];
    for n in [&a, &b, &c] {
        n.h.set_allowed_peers(GossipTopic::Crew(space), all.clone());
        n.h.subscribe(GossipTopic::Crew(space));
    }
    // 구독 교환과 메시 구성(하트비트 1초)을 기다립니다.
    tokio::time::sleep(Duration::from_secs(3)).await;

    // b가 올린 메시지가 a와 c에 도착한다(a는 검증 후 Accept하면 메시로 다시 전달).
    let mut delivered = false;
    for round in 0..10u64 {
        let msg = GossipMessage::Events {
            space,
            events: signed_events(space, 1, 16 + round as usize),
        };
        if b.h
            .try_publish(GossipTopic::Crew(space), msg.clone())
            .await
            .is_err()
        {
            tokio::time::sleep(Duration::from_millis(500)).await;
            continue;
        }
        let at_a = a
            .wait_gossip(Duration::from_secs(3), |from, m| {
                *from == b.peer && *m == msg
            })
            .await;
        // DHT 자동 부트스트랩으로 b와 c도 직접 연결될 수 있으므로 c는 누가 전달했든 받으면 됩니다.
        let at_c = c
            .wait_gossip(Duration::from_secs(3), |_, m| *m == msg)
            .await;
        if at_a && at_c {
            delivered = true;
            break;
        }
    }
    assert!(delivered, "b의 메시지가 a·c에 도착");

    // 1) c를 이 공간에서만 뺀다(다른 공간 멤버라 연결·점수는 유지). c가 구독을 풀고(허용되지 않은 토픽
    //    구독은 그 자체로 그레이리스트 대상) 이 공간 토픽으로 보내면 수동 검증 단계에서 거부된다.
    let other_space = SpaceId::random();
    for n in [&a, &b] {
        n.h.set_allowed_peers(GossipTopic::Crew(other_space), all.clone());
        n.h.set_allowed_peers(GossipTopic::Crew(space), vec![a.peer, b.peer]);
    }
    c.h.unsubscribe(GossipTopic::Crew(space));
    tokio::time::sleep(Duration::from_millis(1000)).await;
    a.drain();
    b.drain();
    // c는 여전히 자기가 멤버라고 믿고 보낸다(구독하지 않은 토픽이라 fanout으로).
    let outsider = GossipMessage::Events {
        space,
        events: signed_events(space, 1, 99),
    };
    let _ =
        c.h.try_publish(GossipTopic::Crew(space), outsider.clone())
            .await;
    assert!(
        !a.wait_gossip(Duration::from_secs(3), |_, m| *m == outsider)
            .await,
        "a는 허용 목록 밖의 메시지를 넘기면 안 됨"
    );
    assert!(
        !b.wait_gossip(Duration::from_secs(1), |_, m| *m == outsider)
            .await,
        "b에도 전달되면 안 됨"
    );

    // 2) 어느 공간에도 없으면 그레이리스트: 메시지를 아예 처리하지 않는다.
    for n in [&a, &b] {
        n.h.set_allowed_peers(GossipTopic::Crew(other_space), vec![]);
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    let outsider2 = GossipMessage::Events {
        space,
        events: signed_events(space, 1, 100),
    };
    let _ =
        c.h.try_publish(GossipTopic::Crew(space), outsider2.clone())
            .await;
    assert!(
        !a.wait_gossip(Duration::from_secs(2), |_, m| *m == outsider2)
            .await
    );
    assert!(
        !b.wait_gossip(Duration::from_secs(1), |_, m| *m == outsider2)
            .await
    );

    // 멤버끼리는 계속 동작한다.
    let msg = GossipMessage::Events {
        space,
        events: signed_events(space, 1, 7),
    };
    let mut ok = false;
    for _ in 0..5 {
        if b.h
            .try_publish(GossipTopic::Crew(space), msg.clone())
            .await
            .is_ok()
            && a.wait_gossip(Duration::from_secs(3), |from, m| {
                *from == b.peer && *m == msg
            })
            .await
        {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(ok, "멤버 사이 gossip");

    // 형식이 잘못된 메시지(공간 불일치)는 보내기 전에 거절.
    let wrong = GossipMessage::Events {
        space: SpaceId::random(),
        events: signed_events(space, 1, 8),
    };
    assert!(matches!(
        b.h.try_publish(GossipTopic::Crew(space), wrong).await,
        Err(NetError::InvalidInput(_))
    ));
}

/// 닫힌 스웜의 보내는 쪽: 다른 공간의 멤버(연결·점수는 정상)가 이 공간 토픽을 구독해도 메시지를 받지 못하고,
/// 허용 목록에 넣으면 바로 받습니다.
#[tokio::test]
async fn gossip_not_leaked_to_foreign_space_subscribers() {
    let mut a = node(default_responder(), |_| {}).await;
    let b = node(default_responder(), |_| {}).await;
    let mut c = node(default_responder(), |_| {}).await;
    b.h.dial(a.peer, vec![a.addr.clone()]).await.unwrap();
    c.h.dial(a.peer, vec![a.addr.clone()]).await.unwrap();
    c.h.dial(b.peer, vec![b.addr.clone()]).await.unwrap();

    let space = SpaceId::random();
    let shared = SpaceId::random();
    let all = vec![a.peer, b.peer, c.peer];
    // 허용 목록 없이 구독만 한 공간에는 보낼 수 없다.
    a.h.subscribe(GossipTopic::Crew(space));
    let early = GossipMessage::Events {
        space,
        events: signed_events(space, 1, 5),
    };
    assert!(matches!(
        a.h.try_publish(GossipTopic::Crew(space), early).await,
        Err(NetError::InvalidInput(_))
    ));
    for n in [&a, &b] {
        n.h.set_allowed_peers(GossipTopic::Crew(shared), all.clone());
        n.h.set_allowed_peers(GossipTopic::Crew(space), vec![a.peer, b.peer]);
        n.h.subscribe(GossipTopic::Crew(space));
    }
    // c는 이 공간의 멤버가 아니지만(다른 공간 멤버) 토픽 이름을 알고 구독한다.
    c.h.set_allowed_peers(GossipTopic::Crew(space), all.clone());
    c.h.subscribe(GossipTopic::Crew(space));
    tokio::time::sleep(Duration::from_secs(3)).await;
    c.drain();

    let mut at_a = false;
    for round in 0..5u64 {
        let msg = GossipMessage::Events {
            space,
            events: signed_events(space, 1, 32 + round as usize),
        };
        if b.h
            .try_publish(GossipTopic::Crew(space), msg.clone())
            .await
            .is_err()
        {
            tokio::time::sleep(Duration::from_millis(500)).await;
            continue;
        }
        at_a = a
            .wait_gossip(Duration::from_secs(3), |_, m| *m == msg)
            .await;
        if at_a {
            break;
        }
    }
    assert!(at_a, "멤버 a는 받아야 함");
    assert!(
        !c.wait_gossip(Duration::from_secs(2), |_, _| true).await,
        "멤버가 아닌 c에게 새면 안 됨"
    );

    // c를 허용 목록에 넣으면 점수가 풀려 받기 시작한다.
    for n in [&a, &b] {
        n.h.set_allowed_peers(GossipTopic::Crew(space), all.clone());
    }
    tokio::time::sleep(Duration::from_secs(2)).await;
    let mut at_c = false;
    for round in 0..10u64 {
        let msg = GossipMessage::Events {
            space,
            events: signed_events(space, 1, 64 + round as usize),
        };
        if b.h
            .try_publish(GossipTopic::Crew(space), msg.clone())
            .await
            .is_ok()
            && c.wait_gossip(Duration::from_secs(2), |_, m| *m == msg)
                .await
        {
            at_c = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(at_c, "허용 목록에 들어간 c는 받아야 함");
}

/// 데몬이 늦으면 상대는 맨 시간 초과가 아니라 오류 응답을 받는다.
#[tokio::test]
async fn slow_daemon_yields_error_response_before_timeout() {
    let timeout = Duration::from_secs(2);
    let a = node(default_responder(), |c| c.request_timeout = timeout).await;
    let slow: Handler = Arc::new(|_req, reply| {
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(3)).await;
            let _ = reply.send(YpResponse::Ok);
        });
    });
    let b = node_with(slow, |c| c.request_timeout = timeout).await;
    a.h.dial(b.peer, vec![b.addr.clone()]).await.unwrap();
    let started = tokio::time::Instant::now();
    let res =
        a.h.request(
            b.peer,
            YpRequest::GetEvents {
                space: SpaceId::random(),
                ids: vec![],
            },
        )
        .await;
    assert!(
        matches!(
            res,
            Ok(YpResponse::Error {
                code: WireErrorCode::Internal,
                ..
            })
        ),
        "{res:?}"
    );
    assert!(started.elapsed() < timeout);
}

/// 보내는 쪽은 피어당 동시 요청 수를, 받는 쪽은 피어당 대기 바이트를 제한한다.
#[tokio::test]
async fn request_backpressure_limits() {
    let held: Arc<Mutex<Vec<oneshot::Sender<YpResponse>>>> = Arc::default();
    let hold = held.clone();
    let holder: Handler = Arc::new(move |_req, reply| hold.lock().unwrap().push(reply));
    let a = node(default_responder(), |_| {}).await;
    let b = node_with(holder, |_| {}).await;
    a.h.dial(b.peer, vec![b.addr.clone()]).await.unwrap();

    // 1) 답이 오지 않는 요청이 피어당 32개면 그 이상은 보내지 않고 바로 Busy.
    let space = SpaceId::random();
    let calls: Vec<_> = (0..40)
        .map(|_| {
            let h = a.h.clone();
            tokio::spawn(async move {
                h.request(b.peer, YpRequest::GetEvents { space, ids: vec![] })
                    .await
            })
        })
        .collect();
    for _ in 0..100 {
        if held.lock().unwrap().len() >= 32 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(held.lock().unwrap().len(), 32);
    // 붙잡은 답을 버리면 기다리던 요청은 오류 응답을 받는다.
    held.lock().unwrap().clear();
    let (mut busy, mut internal) = (0, 0);
    for c in calls {
        match c.await.unwrap() {
            Err(NetError::Busy) => busy += 1,
            Ok(YpResponse::Error {
                code: WireErrorCode::Internal,
                ..
            }) => internal += 1,
            other => panic!("{other:?}"),
        }
    }
    assert_eq!((busy, internal), (8, 32));

    // 2) 받는 쪽: 피어 하나가 쌓아 둔 요청 바이트가 32MiB를 넘으면 Busy로 답한다(약 7.5MiB × 5).
    let big = || YpRequest::PushEvents {
        space,
        events: noise_events(space, 30, 256 * 1024),
    };
    let calls: Vec<_> = (0..5)
        .map(|_| {
            let h = a.h.clone();
            let req = big();
            tokio::spawn(async move { h.request(b.peer, req).await })
        })
        .collect();
    // 붙잡힌 4개는 끝나지 않으므로 가장 먼저 끝나는 하나가 Busy여야 한다.
    let (first, _, rest) =
        tokio::time::timeout(Duration::from_secs(30), futures::future::select_all(calls))
            .await
            .expect("하나는 바로 Busy로 끝나야 함");
    assert!(
        matches!(first, Ok(Ok(YpResponse::Busy { .. }))),
        "{first:?}"
    );
    assert_eq!(held.lock().unwrap().len(), 4);
    held.lock().unwrap().clear();
    for c in rest {
        let r = c.await.unwrap();
        assert!(
            matches!(
                r,
                Ok(YpResponse::Error {
                    code: WireErrorCode::Internal,
                    ..
                })
            ),
            "{r:?}"
        );
    }
}

#[tokio::test]
async fn kad_put_get_and_find_peer() {
    let a = node(default_responder(), |_| {}).await;
    let boot = a.full_addr();
    let b = node(default_responder(), |c| c.bootstrap = vec![boot.clone()]).await;
    let c = node(default_responder(), |c| c.bootstrap = vec![boot.clone()]).await;

    let key = blake3::hash(b"yangpa/v1/locator test").as_bytes().to_vec();
    let value = vec![42u8; 600];
    let mut put = Err(NetError::Timeout);
    for _ in 0..20 {
        put = b.h.put_record(key.clone(), value.clone()).await;
        if put.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    put.unwrap();

    let mut got = Vec::new();
    for _ in 0..20 {
        if let Ok(v) = c.h.get_record(key.clone()).await {
            got = v;
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert_eq!(got, vec![value]);

    // 없는 키.
    assert_eq!(c.h.get_record(vec![7; 32]).await, Err(NetError::NotFound));
    // 크기 검사.
    assert!(matches!(
        b.h.put_record(key.clone(), vec![0; 9000]).await,
        Err(NetError::TooLarge { .. })
    ));

    // c가 b의 주소를 DHT로 찾는다.
    let mut found = Vec::new();
    for _ in 0..20 {
        if let Ok(addrs) = c.h.find_peer(b.peer).await {
            found = addrs;
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(found.contains(&b.addr), "찾은 주소: {found:?}");
    assert_eq!(
        c.h.request(b.peer, YpRequest::Ping).await.unwrap(),
        YpResponse::Pong
    );
}

#[tokio::test]
async fn penalize_disconnects_and_bans() {
    let mut a = node(default_responder(), |c| {
        c.ban_duration = Duration::from_secs(2)
    })
    .await;
    let b = node(default_responder(), |_| {}).await;
    b.h.dial(a.peer, vec![a.addr.clone()]).await.unwrap();
    assert!(
        a.wait_link(b.peer, true, Duration::from_secs(5)).await,
        "연결 알림"
    );
    assert_eq!(
        b.h.request(a.peer, YpRequest::Ping).await.unwrap(),
        YpResponse::Pong
    );

    a.h.penalize(b.peer, "테스트");
    assert!(
        a.wait_link(b.peer, false, Duration::from_secs(5)).await,
        "차단하면 연결이 끊김"
    );
    assert_eq!(
        a.h.request(b.peer, YpRequest::Ping).await,
        Err(NetError::Banned)
    );
    assert_eq!(
        a.h.dial(b.peer, vec![b.addr.clone()]).await,
        Err(NetError::Banned)
    );
    // b가 다시 걸어도 a가 거절한다.
    assert!(b.h.request(a.peer, YpRequest::Ping).await.is_err());
    assert!(!a.h.peers().await.iter().any(|p| p.peer_id == b.peer));

    // 차단 시간이 지나면 다시 연결된다.
    tokio::time::sleep(Duration::from_secs(3)).await;
    a.h.dial(b.peer, vec![b.addr.clone()]).await.unwrap();
    assert_eq!(
        a.h.request(b.peer, YpRequest::Ping).await.unwrap(),
        YpResponse::Pong
    );
}

#[tokio::test]
async fn relay_reservation_gives_link_safe_address() {
    // r: 공인 주소를 아는 릴레이 서버. 릴레이는 확인된 외부 주소가 있어야 예약을 받으므로 미리 정한 포트로 띄웁니다.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let r_addr: Multiaddr = format!("/ip4/127.0.0.1/tcp/{port}").parse().unwrap();
    let r = node(default_responder(), |c| {
        c.listen = vec![r_addr.clone()];
        c.external_addrs = vec![r_addr.clone()];
        c.relay_server = true;
    })
    .await;
    // n: 릴레이를 부트스트랩으로 알고 예약하는 노드.
    let n = node(default_responder(), |c| c.bootstrap = vec![r.full_addr()]).await;
    let s = wait_status(&n.h, Duration::from_secs(10), |s| !s.relay_addrs.is_empty()).await;
    let shared = s.shareable_addrs();
    assert!(!shared.is_empty(), "릴레이 예약 주소가 생겨야 함");
    assert_eq!(s.relays, vec![RelayNode { peer_id: r.peer }]);
    for x in &shared {
        assert!(yangpa_net::addr::is_link_safe_relay_addr(x), "{x}");
        assert!(
            x.to_string()
                .ends_with(&format!("/p2p-circuit/p2p/{}", n.peer)),
            "{x}"
        );
        // 회로 앞은 릴레이 주소, 뒤는 PeerId뿐(자기 직접 주소 없음).
        assert!(
            x.to_string()
                .starts_with(&format!("{r_addr}/p2p/{}/p2p-circuit", r.peer)),
            "{x}"
        );
    }

    // d는 릴레이 경유 주소만 알고 n에 연결해 요청한다.
    let d = node(default_responder(), |_| {}).await;
    d.h.dial(n.peer, shared.clone()).await.unwrap();
    assert_eq!(
        d.h.request(n.peer, YpRequest::Ping).await.unwrap(),
        YpResponse::Pong
    );
}

/// Tor 모드 설정이 실제 스웜에서 지켜지는지(네트워크 성공은 필요 없음).
#[cfg(feature = "tor")]
#[tokio::test]
async fn tor_mode_spawn_exposes_no_ip() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = NetConfig::new(NetMode::Tor, Keypair::generate_ed25519(), dir.path());
    cfg.mdns = true;
    cfg.relay_server = true;
    cfg.listen = vec!["/ip4/0.0.0.0/tcp/0".parse().unwrap()];
    cfg.external_addrs = vec!["/ip4/1.2.3.4/tcp/4001".parse().unwrap()];
    let (h, mut ev) = yangpa_net::spawn(cfg).unwrap();
    tokio::spawn(async move { while ev.recv().await.is_some() {} });
    let mut s = h.status().await;
    for _ in 0..100 {
        if s.tor_bootstrap.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        s = h.status().await;
    }
    assert_eq!(s.mode, NetMode::Tor);
    assert_eq!(s.nat, yangpa_net::NatStatus::Disabled);
    assert!(s.external_addrs.is_empty());
    assert!(s.relay_addrs.is_empty());
    for a in &s.listen_addrs {
        assert!(
            yangpa_net::addr::is_onion(a),
            "Tor 모드 수신 주소는 onion뿐: {a}"
        );
    }
    assert!(
        s.tor_bootstrap.is_some(),
        "onion 서비스 때문에 Arti가 시작돼야 함"
    );
    // onion 키는 로컬에서 만들어지므로 Tor 망 없이도 주소가 나오고, 공유할 주소는 그것 하나뿐이어야 한다.
    for _ in 0..100 {
        if s.onion_addr.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        s = h.status().await;
    }
    let onion = s.onion_addr.clone().expect("onion 주소가 나와야 함");
    assert!(yangpa_net::addr::is_onion(&onion), "{onion}");
    assert_eq!(s.shareable_addrs(), vec![onion]);
    // Tor 스웜은 IP 주소로 직접 나가지 않는다(UDP는 Tor로 걸 수 없음).
    let err = h
        .dial(
            PeerId::random(),
            vec!["/ip4/127.0.0.1/udp/1/quic-v1".parse().unwrap()],
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, NetError::Dial(_) | NetError::NoAddresses),
        "{err:?}"
    );
    h.shutdown();
}

/// 실제 Tor 네트워크가 필요합니다: `cargo test -p yangpa-net -- --ignored tor_onion`.
/// Tor 스웜 하나가 onion 서비스를 열고, 다른 Tor 스웜이 그 onion 주소로 연결해 요청합니다(직접 연결 스웜은 Arti를
/// 띄우지 않아 onion에 걸지 못함).
#[cfg(feature = "tor")]
#[tokio::test]
#[ignore]
async fn tor_onion_service_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = NetConfig::new(NetMode::Tor, Keypair::generate_ed25519(), dir.path());
    let (tor, mut ev) = yangpa_net::spawn(cfg).unwrap();
    tokio::spawn(async move { while ev.recv().await.is_some() {} });
    let mut onion = None;
    for _ in 0..600 {
        if let Some(a) = tor.status().await.onion_addr {
            onion = Some(a);
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    let onion = onion.expect("onion 서비스 주소");
    // onion 서비스 설명자가 퍼질 시간을 줍니다.
    tokio::time::sleep(Duration::from_secs(30)).await;

    let dir2 = tempfile::tempdir().unwrap();
    let cfg2 = NetConfig::new(NetMode::Tor, Keypair::generate_ed25519(), dir2.path());
    let (client, mut ev2) = yangpa_net::spawn(cfg2).unwrap();
    tokio::spawn(async move { while ev2.recv().await.is_some() {} });
    let mut ok = false;
    for _ in 0..5 {
        if client
            .dial(tor.local_peer_id(), vec![onion.clone()])
            .await
            .is_ok()
        {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
    assert!(ok, "onion 주소로 연결");
    assert_eq!(
        client
            .request(tor.local_peer_id(), YpRequest::Ping)
            .await
            .unwrap(),
        YpResponse::Pong
    );
}

/// 들어온 연결의 상대가 identify로 알린 수신 주소를 데몬이 받아 갈 수 있다(재시작 뒤 다시 걸 때 씀).
#[tokio::test]
async fn inbound_peer_listen_addrs_via_identify() {
    let mut a = node(default_responder(), |_| {}).await;
    let b = node(default_responder(), |_| {}).await;
    b.h.dial(a.peer, vec![a.addr.clone()]).await.unwrap();
    assert!(a.wait_link(b.peer, true, Duration::from_secs(10)).await);
    let addrs = a.h.peer_listen_addrs(b.peer).await;
    assert!(addrs.contains(&b.addr), "{addrs:?}");
    // 연결이 없는 피어는 기다리지 않고 빈 목록.
    assert!(a.h.peer_listen_addrs(PeerId::random()).await.is_empty());
}

/// 주소록에 남은 옛 주소에서 다른 노드가 답해도, 받은 올바른 주소로 다시 걸어 연결한다(재시작한 상대의 옛 포트를
/// 다른 노드가 쓰는 경우).
#[tokio::test]
async fn stale_address_answered_by_other_node_does_not_fail_dial() {
    for _ in 0..4 {
        let a = node(default_responder(), |_| {}).await;
        let b = node(default_responder(), |_| {}).await;
        let c = node(default_responder(), |_| {}).await;
        // 옛 주소(지금은 C가 씀)로 걸던 중에 올바른 주소로 다시 걸면, 진행 중인 걸기에 합쳐져 그 결과를 기다립니다.
        // 옛 주소에서 C가 답해 실패해도 받은 올바른 주소로 다시 걸어 둘 다 연결된다.
        let (stale, fresh) = tokio::join!(
            a.h.dial(b.peer, vec![c.addr.clone()]),
            a.h.dial(b.peer, vec![b.addr.clone()])
        );
        fresh.unwrap();
        stale.unwrap();
        assert_eq!(
            a.h.request(b.peer, YpRequest::Ping).await.unwrap(),
            YpResponse::Pong
        );
    }
}

/// 실행 중에 부트스트랩 노드를 더하면 바로 연결한다.
#[tokio::test]
async fn add_bootstrap_at_runtime() {
    let mut a = node(default_responder(), |_| {}).await;
    let b = node(default_responder(), |_| {}).await;
    b.h.add_bootstrap(vec![a.full_addr()]);
    assert!(a.wait_link(b.peer, true, Duration::from_secs(10)).await);
    // 같은 주소를 또 더해도 문제없다.
    b.h.add_bootstrap(vec![a.full_addr()]);
    assert_eq!(
        b.h.request(a.peer, YpRequest::Ping).await.unwrap(),
        YpResponse::Pong
    );
}

/// 스웜을 다른 키·포트로 바꿔 띄워도 핸들 복제본과 이벤트 수신기가 그대로 이어진다.
#[tokio::test]
async fn restart_swaps_swarm_and_keeps_event_stream() {
    let requests = Arc::new(Mutex::new(0usize));
    let counter = requests.clone();
    let handler: Handler = Arc::new(move |_req, reply| {
        *counter.lock().unwrap() += 1;
        let _ = reply.send(YpResponse::Ok);
    });
    let mut a = node_with(handler, |_| {}).await;
    let b = node(default_responder(), |_| {}).await;
    let copy = a.h.clone();
    b.h.dial(a.peer, vec![a.addr.clone()]).await.unwrap();
    assert!(a.wait_link(b.peer, true, Duration::from_secs(10)).await);

    let old_peer = a.peer;
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = NetConfig::new(NetMode::Normal, Keypair::generate_ed25519(), dir.path());
    cfg.listen = vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()];
    cfg.mdns = false;
    let new_peer = cfg.keypair.public().to_peer_id();
    a.h.restart(cfg).await.unwrap();
    // 옛 연결이 끊겼다고 같은 수신기로 알린다.
    assert!(a.wait_link(b.peer, false, Duration::from_secs(10)).await);
    assert_eq!(copy.local_peer_id(), new_peer);
    assert_ne!(new_peer, old_peer);
    assert!(!copy.is_closed());

    let mut new_addr = None;
    for _ in 0..100 {
        if let Some(x) = copy.status().await.listen_addrs.first().cloned() {
            new_addr = Some(x);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let new_addr = new_addr.expect("새 수신 주소");
    assert_ne!(new_addr, a.addr);
    assert_eq!(copy.last_status().peer_id, new_peer);

    // 새 스웜이 받은 요청도 같은 데몬 루프로 온다.
    b.h.dial(new_peer, vec![new_addr]).await.unwrap();
    let before = *requests.lock().unwrap();
    let req = YpRequest::GetEvents {
        space: SpaceId::random(),
        ids: Vec::new(),
    };
    assert_eq!(b.h.request(new_peer, req).await.unwrap(), YpResponse::Ok);
    assert_eq!(*requests.lock().unwrap(), before + 1);
    // 옛 PeerId로는 더 이상 연결되지 않는다.
    assert!(b.h.request(old_peer, YpRequest::Ping).await.is_err());

    copy.shutdown();
    for _ in 0..100 {
        if a.h.is_closed() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(a.h.is_closed());
    assert_eq!(
        a.h.request(b.peer, YpRequest::Ping).await,
        Err(NetError::Shutdown)
    );
    assert_eq!(
        a.h.restart(NetConfig::new(
            NetMode::Normal,
            Keypair::generate_ed25519(),
            dir.path()
        ))
        .await,
        Err(NetError::Shutdown)
    );
}

/// 일반 모드에서 Tor 모드로 실행 중에 바꾸면 IP 수신 주소가 사라진다(Tor 망 연결은 필요 없음).
#[cfg(feature = "tor")]
#[tokio::test]
async fn restart_into_tor_mode_drops_ip_listeners() {
    let a = node(default_responder(), |_| {}).await;
    assert_eq!(a.h.last_status().mode, NetMode::Normal);
    let dir = tempfile::tempdir().unwrap();
    let cfg = NetConfig::new(NetMode::Tor, Keypair::generate_ed25519(), dir.path());
    a.h.restart(cfg).await.unwrap();
    let mut s = a.h.status().await;
    for _ in 0..100 {
        if s.onion_addr.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        s = a.h.status().await;
    }
    assert_eq!(s.mode, NetMode::Tor);
    assert!(s.onion_addr.is_some(), "onion 주소가 나와야 함");
    for x in &s.listen_addrs {
        assert!(yangpa_net::addr::is_onion(x), "{x}");
    }
    // 옛 수신 포트는 닫혔다.
    let port = a
        .addr
        .iter()
        .find_map(|p| match p {
            libp2p::multiaddr::Protocol::Tcp(port) => Some(port),
            _ => None,
        })
        .unwrap();
    assert!(
        tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_err()
    );
    a.h.shutdown();
}

/// 실행 중에 네트워크를 다시 띄우면 같은 데이터 경로의 Arti 상태를 이어 쓴다. 새로 만들면 Arti 0.47이 놓지 않은
/// 옛 클라이언트의 잠금 때문에 새 클라이언트가 읽기 전용으로 떠 부트스트랩이 0%에서 멈춘다(실제 Tor 망에서만
/// 보이므로 여기서는 다이얼 수가 이어지는지로 같은 상태인지 봄).
#[cfg(feature = "tor")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_keeps_arti_client() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = NetConfig::new(NetMode::Tor, Keypair::generate_ed25519(), dir.path());
    let (h, mut ev) = yangpa_net::spawn(cfg).unwrap();
    tokio::spawn(async move { while ev.recv().await.is_some() {} });
    let onion: Multiaddr = "/onion3/duckduckgogg42xjoc72x3sjasowoarfbgcmvfimaftt6twagswzczad:4001"
        .parse()
        .unwrap();
    let _ = tokio::time::timeout(
        Duration::from_secs(2),
        h.dial(PeerId::random(), vec![onion]),
    )
    .await;
    let before = h.tor_activity().dials;
    assert!(before >= 1, "onion 주소는 Arti로 감");

    h.restart(NetConfig::new(
        NetMode::Tor,
        Keypair::generate_ed25519(),
        dir.path(),
    ))
    .await
    .unwrap();
    assert_eq!(h.inspect().await.expect("구성 점검").mode, NetMode::Tor);
    assert!(
        h.tor_activity().dials >= before,
        "같은 데이터 경로면 Arti 상태를 이어 써야 함"
    );

    // 데이터 경로가 다르면 새로 만듭니다.
    let other = tempfile::tempdir().unwrap();
    h.restart(NetConfig::new(
        NetMode::Tor,
        Keypair::generate_ed25519(),
        other.path(),
    ))
    .await
    .unwrap();
    assert_eq!(h.tor_activity().dials, 0);
    h.shutdown();
}

/// Tor 모드 스웜을 띄워 onion 주소가 나올 때까지 기다립니다.
#[cfg(feature = "tor")]
async fn spawn_tor(dir: &std::path::Path, key: &Keypair, wait: Duration) -> (NetHandle, Multiaddr) {
    let cfg = NetConfig::new(NetMode::Tor, key.clone(), dir);
    let (h, mut ev) = yangpa_net::spawn(cfg).unwrap();
    tokio::spawn(async move { while ev.recv().await.is_some() {} });
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        if let Some(a) = h.status().await.onion_addr {
            return (h, a);
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "onion 주소가 나와야 함"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[cfg(feature = "tor")]
async fn shutdown_and_wait(h: &NetHandle) {
    h.shutdown();
    for _ in 0..100 {
        if h.is_closed() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(h.is_closed());
}

/// 같은 프로세스에서 Tor 모드 스웜을 내리고 같은 데이터 경로로 다시 띄워도 같은 onion 주소를 알린다(Tor 망 없이:
/// 주소는 서비스 신원 키에서 나오고 서비스는 부트스트랩 뒤에 뜨므로, 여기서는 서비스를 띄우지 않음).
/// 서비스가 실제로 다시 뜨는지는 `tor_mode_restart_in_process_relaunches_onion`(실제 Tor 망)이 봅니다.
#[cfg(feature = "tor")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tor_mode_restart_in_process_keeps_onion_address() {
    let dir = tempfile::tempdir().unwrap();
    let key = Keypair::generate_ed25519();
    let (h1, first) = spawn_tor(dir.path(), &key, Duration::from_secs(30)).await;
    shutdown_and_wait(&h1).await;
    let (h2, second) = spawn_tor(dir.path(), &key, Duration::from_secs(30)).await;
    assert_eq!(first, second, "같은 키면 같은 onion 주소");
    shutdown_and_wait(&h2).await;
}

/// 실제 Tor 네트워크가 필요합니다: `cargo test -p yangpa-net -- --ignored relaunches_onion`.
/// 같은 프로세스에서 onion 서비스를 띄운 Tor 모드 스웜을 내리고 같은 데이터 경로로 다시 띄우면, 서비스가 다시 떠서
/// 그 onion 주소로 연결해 요청할 수 있다(Arti가 이전 서비스의 상태 잠금을 놓아야 함).
#[cfg(feature = "tor")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "실제 Tor 망 접속이 필요하고 onion 서비스 게시에 몇 분 걸릴 수 있음"]
async fn tor_mode_restart_in_process_relaunches_onion() {
    let dir = tempfile::tempdir().unwrap();
    let key = Keypair::generate_ed25519();
    let normal = node(default_responder(), |_| {}).await;
    for round in 0..2 {
        let (h, onion) = spawn_tor(dir.path(), &key, Duration::from_secs(600)).await;
        let mut ok = false;
        for _ in 0..30 {
            if normal
                .h
                .dial(h.local_peer_id(), vec![onion.clone()])
                .await
                .is_ok()
            {
                ok = true;
                break;
            }
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
        assert!(ok, "{round}번째 onion 서비스로 연결");
        assert_eq!(
            normal
                .h
                .request(h.local_peer_id(), YpRequest::Ping)
                .await
                .unwrap(),
            YpResponse::Pong
        );
        shutdown_and_wait(&h).await;
    }
}

/// YANGPA 노드가 아닌 libp2p 릴레이(공개망 노드 같은). 미리 정한 포트로 띄워 외부 주소를 압니다(릴레이는 외부 주소가
/// 있어야 예약을 받음).
#[derive(libp2p::swarm::NetworkBehaviour)]
struct PlainRelay {
    relay: libp2p::relay::Behaviour,
    identify: libp2p::identify::Behaviour,
}

/// 릴레이 흉내를 띄워 주소(`/p2p/<id>` 포함)를 돌려줍니다.
fn spawn_plain_relay() -> (PeerId, Multiaddr) {
    use futures::StreamExt;
    use libp2p::{identify, noise, tcp, yamux};
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let addr: Multiaddr = format!("/ip4/127.0.0.1/tcp/{port}").parse().unwrap();
    let mut swarm = libp2p::SwarmBuilder::with_new_identity()
        .with_tokio()
        .with_tcp(
            tcp::Config::default(),
            noise::Config::new,
            yamux::Config::default,
        )
        .unwrap()
        .with_behaviour(|key| PlainRelay {
            relay: libp2p::relay::Behaviour::new(key.public().to_peer_id(), Default::default()),
            identify: identify::Behaviour::new(identify::Config::new(
                "/plain-relay/0.1.0".into(),
                key.public(),
            )),
        })
        .unwrap()
        .build();
    let peer = *swarm.local_peer_id();
    swarm.listen_on(addr.clone()).unwrap();
    swarm.add_external_address(addr.clone());
    tokio::spawn(async move {
        loop {
            swarm.select_next_some().await;
        }
    });
    (peer, addr.with_p2p(peer).unwrap())
}

/// TLS만 아는 libp2p 노드. `dial`을 주면 그 주소로 걸어 옵니다.
fn spawn_tls_only_peer(dial: Option<Multiaddr>) -> (PeerId, Multiaddr) {
    use futures::StreamExt;
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let addr: Multiaddr = format!("/ip4/127.0.0.1/tcp/{port}").parse().unwrap();
    let mut swarm = libp2p::SwarmBuilder::with_new_identity()
        .with_tokio()
        .with_tcp(
            libp2p::tcp::Config::default(),
            libp2p::tls::Config::new,
            libp2p::yamux::Config::default,
        )
        .unwrap()
        .with_behaviour(|_| libp2p::ping::Behaviour::default())
        .unwrap()
        .build();
    let peer = *swarm.local_peer_id();
    swarm.listen_on(addr.clone()).unwrap();
    if let Some(to) = dial {
        swarm.dial(to).unwrap();
    }
    tokio::spawn(async move {
        loop {
            swarm.select_next_some().await;
        }
    });
    (peer, addr)
}

/// 직접 TCP는 TLS(X25519MLKEM768)로 잇는다: Noise로 물러나면 TLS만 아는 노드와는 어느 쪽으로도 이어지지 않는다.
#[tokio::test]
async fn direct_tcp_speaks_tls_both_ways() {
    let mut n = node(default_responder(), |_| {}).await;
    let (inbound, _) = spawn_tls_only_peer(Some(n.addr.clone()));
    assert!(
        n.wait_link(inbound, true, Duration::from_secs(10)).await,
        "TLS로 걸어 온 연결을 받아야 함"
    );
    let (outbound, addr) = spawn_tls_only_peer(None);
    n.h.dial(outbound, vec![addr])
        .await
        .expect("TLS로 걸어야 함");
}

async fn wait_status(
    h: &NetHandle,
    limit: Duration,
    mut pred: impl FnMut(&yangpa_net::NetStatus) -> bool,
) -> yangpa_net::NetStatus {
    let deadline = tokio::time::Instant::now() + limit;
    loop {
        let s = h.status().await;
        if pred(&s) || tokio::time::Instant::now() >= deadline {
            return s;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn sticky(peer: PeerId, addr: &Multiaddr) -> StickyRelay {
    StickyRelay {
        peer_id: peer,
        addrs: vec![yangpa_net::addr::split_peer_id(addr).0],
    }
}

/// 지난번 릴레이는 저장 순서대로 예약한다. 뒤의 릴레이(r2)가 먼저 identify를 보내 와도, 앞의 릴레이(r1)에 걸고 있는
/// 동안에는 r2로 자리를 채우지 않아야 한다.
#[tokio::test]
async fn sticky_relays_are_reserved_in_saved_order() {
    let (r1, r1_addr) = spawn_plain_relay();
    let (r2, r2_addr) = spawn_plain_relay();
    let n = node(default_responder(), |c| {
        c.sticky_relays = vec![sticky(r1, &r1_addr), sticky(r2, &r2_addr)];
    })
    .await;
    let s = wait_status(&n.h, Duration::from_secs(20), |s| !s.relays.is_empty()).await;
    assert_eq!(
        s.relays,
        vec![RelayNode { peer_id: r1 }],
        "앞의 지난번 릴레이를 먼저 예약해야 함: {:?}",
        s.relay_addrs
    );
    assert!(
        s.shareable_addrs()
            .iter()
            .all(|a| a.to_string().starts_with(&r1_addr.to_string())),
        "{:?}",
        s.relay_addrs
    );
}

/// 지난번 릴레이가 사라졌으면 기다리지 않고(다이얼 실패) 다음 후보를 예약한다.
#[tokio::test]
async fn dead_sticky_relay_releases_slot() {
    let (r2, r2_addr) = spawn_plain_relay();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let dead: Multiaddr = format!("/ip4/127.0.0.1/tcp/{port}").parse().unwrap();
    let n = node(default_responder(), |c| {
        c.sticky_relays = vec![sticky(PeerId::random(), &dead), sticky(r2, &r2_addr)];
    })
    .await;
    let s = wait_status(&n.h, Duration::from_secs(15), |s| !s.relays.is_empty()).await;
    assert_eq!(s.relays, vec![RelayNode { peer_id: r2 }], "{s:?}");
}

/// YANGPA 노드가 아닌 릴레이(공개망 노드 등)는 identify로 릴레이라고 알려 와도, 부트스트랩·지난번 릴레이로 받지
/// 않았으면 쓰지 않는다. 부트스트랩으로 넣으면 쓴다.
#[tokio::test]
async fn plain_relay_is_used_only_when_configured() {
    let (p, p_addr) = spawn_plain_relay();
    let n = node(default_responder(), |_| {}).await;
    n.h.dial(p, vec![yangpa_net::addr::split_peer_id(&p_addr).0])
        .await
        .unwrap();
    // identify를 받으면 릴레이 후보 판단도 끝납니다. 예약이 생기지 않는지 잠깐 더 봅니다.
    assert!(!n.h.peer_listen_addrs(p).await.is_empty());
    tokio::time::sleep(Duration::from_secs(2)).await;
    let s = n.h.status().await;
    assert!(s.relays.is_empty() && s.relay_addrs.is_empty(), "{s:?}");

    n.h.add_bootstrap(vec![p_addr.clone()]);
    let s = wait_status(&n.h, Duration::from_secs(15), |s| !s.relays.is_empty()).await;
    assert_eq!(s.relays, vec![RelayNode { peer_id: p }], "{s:?}");
}
