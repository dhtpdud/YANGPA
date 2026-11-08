//! 임시 소켓 위에서 클라이언트와 서버를 맞물려 시험합니다.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::Engine;
use bytes::Bytes;
use futures::StreamExt;
use serde_json::{Value, json};
use yangpa_core::ids::{ChannelId, DeviceId, EventId, SpaceId, UserId};
use yangpa_core::model::SpaceKind;
use yangpa_core::time::Timestamp;
use yangpa_rpc::api::{self, Empty, MessageView, Notification, NotifyView, Scope, SpaceSummary, m};
use yangpa_rpc::jsonrpc::{
    ERR_AUTH_REQUIRED, ERR_CONFIRMATION_REQUIRED, ERR_NOT_FOUND, ERR_OUT_OF_SCOPE, INVALID_PARAMS,
    METHOD_NOT_FOUND,
};
use yangpa_rpc::server::{
    AuthInfo, ConnCtx, EventHub, RpcHandler, RpcOutput, ServerConfig, ServerHandle,
};
use yangpa_rpc::{ClientError, Endpoint, RpcClient, RpcError};

const S1: SpaceId = SpaceId([1; 16]);
const S2: SpaceId = SpaceId([2; 16]);
const CH: ChannelId = ChannelId([3; 16]);

struct Mock {
    in_flight: AtomicUsize,
    max_in_flight: AtomicUsize,
    /// `message.list`를 시작한 횟수.
    starts: AtomicUsize,
    disconnects: AtomicUsize,
}

impl Mock {
    fn new() -> Arc<Mock> {
        Arc::new(Mock {
            in_flight: AtomicUsize::new(0),
            max_in_flight: AtomicUsize::new(0),
            starts: AtomicUsize::new(0),
            disconnects: AtomicUsize::new(0),
        })
    }
}

fn message(text: &str, volatile: bool) -> MessageView {
    MessageView {
        id: EventId::hash_of(text.as_bytes()),
        author: UserId([4; 32]),
        author_device: DeviceId([5; 32]),
        created_at: Timestamp(1),
        lamport: 1,
        text: text.into(),
        attachments: vec![],
        embeds: vec![],
        reply_to: None,
        reactions: vec![],
        edited_at: None,
        deleted: false,
        volatile,
        expires_at: None,
        thread: None,
        space: None,
        channel: None,
        author_blocked: false,
        anonymous: false,
        anon_name: None,
        mine: false,
        mentions_me: false,
    }
}

fn info(name: &str, scopes: Vec<Scope>, spaces: Vec<SpaceId>, automation: bool) -> AuthInfo {
    AuthInfo {
        token_id: name.into(),
        token_name: name.into(),
        scopes,
        spaces,
        automation,
    }
}

#[async_trait]
impl RpcHandler for Mock {
    async fn authenticate(&self, token: &str) -> Option<AuthInfo> {
        match token {
            "owner" => Some(info("owner", vec![Scope::Owner], vec![], false)),
            "reader" => Some(info("reader", vec![Scope::Read], vec![], false)),
            "ai" => Some(info(
                "ai",
                vec![Scope::Read, Scope::Write, Scope::Admin],
                vec![],
                true,
            )),
            "crew" => Some(info(
                "crew",
                vec![Scope::Read, Scope::Write],
                vec![S1],
                false,
            )),
            _ => None,
        }
    }

    async fn handle(
        &self,
        ctx: &ConnCtx,
        method: &str,
        params: Value,
    ) -> Result<RpcOutput, RpcError> {
        match method {
            "system.version" => RpcOutput::json(&api::VersionResult {
                version: "0.1.0".into(),
                protocol: yangpa_rpc::PROTOCOL_VERSION,
            }),
            "space.list" => {
                let all = vec![
                    SpaceSummary {
                        space: S1,
                        kind: SpaceKind::Crew,
                        name: "게임 모임".into(),
                        icon: None,
                        unread: 0,
                        mention: 0,
                        dormant: false,
                        member_count: 3,
                    },
                    SpaceSummary {
                        space: S2,
                        kind: SpaceKind::Space,
                        name: "친구".into(),
                        icon: None,
                        unread: 1,
                        mention: 0,
                        dormant: false,
                        member_count: 2,
                    },
                ];
                let visible: Vec<_> = all
                    .into_iter()
                    .filter(|s| ctx.allows_space(&s.space))
                    .collect();
                RpcOutput::json(&visible)
            }
            "message.list" => {
                let p: api::MessageListParams = api::parse_params(params)?;
                self.starts.fetch_add(1, Ordering::SeqCst);
                let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                self.max_in_flight.fetch_max(now, Ordering::SeqCst);
                // limit을 지연(ms)으로 써서 응답 순서를 뒤섞습니다.
                tokio::time::sleep(Duration::from_millis(u64::from(p.limit.unwrap_or(0)))).await;
                self.in_flight.fetch_sub(1, Ordering::SeqCst);
                RpcOutput::json(&api::MessageListResult {
                    messages: vec![message(&format!("m{}", p.limit.unwrap_or(0)), false)],
                    gaps: vec![],
                })
            }
            "message.send" => {
                let p: api::MessageSendParams = api::parse_params(params)?;
                let msg = message(&p.text, false);
                let id = msg.id;
                ctx.hub().publish(Notification::MessageNew {
                    space: p.space,
                    channel: p.channel,
                    message: msg,
                    notify: NotifyView {
                        title: "t".into(),
                        body: Some(p.text.clone()),
                        sender: None,
                    },
                });
                RpcOutput::json(&api::EventRef { id })
            }
            "member.kick" => {
                if ctx.needs_confirmation(method) {
                    return Err(RpcError::confirmation_required("c-1"));
                }
                Ok(RpcOutput::empty())
            }
            "blob.read" => {
                let p: api::BlobReadParams = api::parse_params(params)?;
                let len = p.len.unwrap_or(16) as usize;
                let data: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
                if p.binary == Some(true) {
                    Ok(RpcOutput::Binary(Bytes::from(data)))
                } else {
                    RpcOutput::json(&api::BlobReadResult {
                        data_base64: Some(base64::engine::general_purpose::STANDARD.encode(data)),
                    })
                }
            }
            _ => Err(RpcError::not_found("시험용 핸들러에 없음")),
        }
    }

    async fn on_disconnect(&self, _ctx: &ConnCtx) {
        self.disconnects.fetch_add(1, Ordering::SeqCst);
    }
}

async fn start(config: ServerConfig) -> (tempfile::TempDir, ServerHandle, Arc<Mock>, EventHub) {
    let dir = tempfile::tempdir().unwrap();
    let ep = Endpoint::in_dir(&dir.path().join("rt"));
    let mock = Mock::new();
    let hub = EventHub::new();
    let handle = yangpa_rpc::serve_with(ep, mock.clone(), hub.clone(), config)
        .await
        .unwrap();
    (dir, handle, mock, hub)
}

async fn client(handle: &ServerHandle, token: Option<&str>) -> RpcClient {
    let c = RpcClient::connect(handle.endpoint())
        .await
        .unwrap()
        .with_timeout(Duration::from_secs(10));
    if let Some(t) = token {
        c.hello(t).await.unwrap();
    }
    c
}

#[tokio::test]
async fn auth_is_required_first() {
    let (_dir, handle, _mock, _hub) = start(ServerConfig::default()).await;
    let c = client(&handle, None).await;

    // 인증 없이 되는 것은 system.version뿐.
    let v = c.request::<m::SystemVersion>(Empty).await.unwrap();
    assert_eq!(v.protocol, yangpa_rpc::PROTOCOL_VERSION);
    let e = c.request::<m::SpaceList>(Empty).await.unwrap_err();
    assert_eq!(e.code(), Some(ERR_AUTH_REQUIRED));

    let e = c.hello("wrong").await.unwrap_err();
    assert_eq!(e.code(), Some(ERR_AUTH_REQUIRED));
    let hello = c.hello("reader").await.unwrap();
    assert_eq!(hello.scopes, vec![Scope::Read]);
    assert!(!hello.automation);
    assert_eq!(c.request::<m::SpaceList>(Empty).await.unwrap().len(), 2);
    // 두 번째 hello는 거절.
    assert!(c.hello("owner").await.is_err());
    handle.shutdown();
}

#[tokio::test]
async fn repeated_auth_failures_close_connection() {
    let config = ServerConfig {
        max_failed_auth: 2,
        ..Default::default()
    };
    let (_dir, handle, _mock, _hub) = start(config).await;
    let c = client(&handle, None).await;
    assert!(c.hello("x").await.is_err());
    assert!(c.hello("y").await.is_err());
    // 연결이 닫혀 다음 호출은 실패합니다.
    let e = c.request::<m::SystemVersion>(Empty).await.unwrap_err();
    assert!(matches!(e, ClientError::Closed), "{e:?}");
}

#[tokio::test]
async fn scope_denial_and_unknown_method() {
    let (_dir, handle, _mock, _hub) = start(ServerConfig::default()).await;
    let c = client(&handle, Some("reader")).await;
    let params = api::MessageSendParams {
        space: S1,
        channel: CH,
        text: "x".into(),
        attachments: vec![],
        reply_to: None,
        thread: None,
    };
    let e = c.request::<m::MessageSend>(params).await.unwrap_err();
    assert_eq!(e.code(), Some(ERR_OUT_OF_SCOPE));
    assert_eq!(e.rpc().unwrap().data.as_ref().unwrap()["required"], "write");

    let e = c.call_value("no.such", Value::Null).await.unwrap_err();
    assert_eq!(e.code(), Some(METHOD_NOT_FOUND));
    // 표에는 있지만 핸들러가 모르는 메서드는 핸들러 오류 그대로.
    let e = c.request::<m::NetPeers>(Empty).await.unwrap_err();
    assert_eq!(e.code(), Some(ERR_NOT_FOUND));
    // 잘못된 인자.
    let e = c
        .call_value("message.list", json!({"space": 1}))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Some(INVALID_PARAMS));
}

#[tokio::test]
async fn space_restricted_token() {
    let (_dir, handle, _mock, _hub) = start(ServerConfig::default()).await;
    let c = client(&handle, Some("crew")).await;
    let list = c.request::<m::SpaceList>(Empty).await.unwrap();
    assert_eq!(list.iter().map(|s| s.space).collect::<Vec<_>>(), vec![S1]);
    let p = |space| api::MessageListParams {
        space,
        channel: CH,
        before: None,
        limit: Some(0),
        thread: None,
    };
    assert!(c.request::<m::MessageList>(p(S1)).await.is_ok());
    assert_eq!(
        c.request::<m::MessageList>(p(S2)).await.unwrap_err().code(),
        Some(ERR_OUT_OF_SCOPE)
    );
    // 전역 작업(크루 만들기)은 거부.
    let e = c
        .request::<m::CrewCreate>(api::CrewCreateParams {
            name: "새 크루".into(),
            description: None,
        })
        .await
        .unwrap_err();
    assert_eq!(e.code(), Some(ERR_OUT_OF_SCOPE));
}

#[tokio::test]
async fn automation_destructive_goes_to_confirmation() {
    let (_dir, handle, _mock, _hub) = start(ServerConfig::default()).await;
    let ai = client(&handle, Some("ai")).await;
    let kick = api::MemberKickParams {
        space: S1,
        user: UserId([9; 32]),
        reason: None,
    };
    let e = ai.request::<m::MemberKick>(kick.clone()).await.unwrap_err();
    assert_eq!(e.code(), Some(ERR_CONFIRMATION_REQUIRED));
    assert_eq!(e.confirmation_id(), Some("c-1"));
    // 사람 전용 메서드는 범위가 있어도 거부.
    let e = ai
        .request::<m::ConfirmApprove>(api::IdParams { id: "c-1".into() })
        .await
        .unwrap_err();
    assert_eq!(e.code(), Some(ERR_OUT_OF_SCOPE));

    let human = client(&handle, Some("owner")).await;
    human.request::<m::MemberKick>(kick).await.unwrap();
}

#[tokio::test]
async fn concurrent_requests_are_multiplexed() {
    let config = ServerConfig {
        max_concurrent_requests: 8,
        ..Default::default()
    };
    let (_dir, handle, mock, _hub) = start(config).await;
    let c = client(&handle, Some("reader")).await;
    let started = Instant::now();
    let mut tasks = Vec::new();
    for i in 0..24u32 {
        let c = c.clone();
        tasks.push(tokio::spawn(async move {
            // 지연이 다양해서 응답이 요청 순서와 다르게 옵니다.
            let delay = 50 + (i * 37) % 100;
            let p = api::MessageListParams {
                space: S1,
                channel: CH,
                before: None,
                limit: Some(delay),
                thread: None,
            };
            let r = c.request::<m::MessageList>(p).await.unwrap();
            assert_eq!(r.messages[0].text, format!("m{delay}"));
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    let elapsed = started.elapsed();
    // 순차라면 24 × 평균 100ms ≈ 2.4초. 동시 8개면 훨씬 짧습니다.
    assert!(elapsed < Duration::from_millis(1500), "{elapsed:?}");
    let max = mock.max_in_flight.load(Ordering::SeqCst);
    assert!(max > 1, "동시 처리되지 않음");
    assert!(max <= 8, "연결당 동시 요청 상한 초과: {max}");
}

#[tokio::test]
async fn notifications_are_batched_and_filtered() {
    let (_dir, handle, _mock, hub) = start(ServerConfig::default()).await;
    let c = client(&handle, Some("owner")).await;
    let mut sub = c.subscribe(vec!["message".into()]).await.unwrap();
    assert_eq!(hub.subscriber_count(), 1);
    assert!(hub.has_subscribers("message.new"));
    assert!(!hub.has_subscribers("presence"));

    // 주제가 다른 알림은 오지 않습니다.
    hub.publish(Notification::Presence {
        user: UserId([1; 32]),
        online: true,
        away: false,
        last_seen: None,
    });
    for i in 0..10 {
        hub.publish(Notification::MessageExpired {
            space: S1,
            channel: CH,
            ids: vec![EventId([i; 32])],
        });
    }
    let mut got = Vec::new();
    let mut batches = 0;
    while got.len() < 10 {
        let batch = tokio::time::timeout(Duration::from_secs(5), sub.next())
            .await
            .unwrap()
            .unwrap();
        batches += 1;
        got.extend(batch);
    }
    assert_eq!(got.len(), 10);
    assert!(got.iter().all(|n| n.kind() == "message.expired"));
    // 한 프레임(16ms) 안에 낸 알림은 한두 묶음으로 옵니다.
    assert!(batches <= 2, "묶음 수 {batches}");

    // 핸들러가 허브로 낸 알림도 같은 경로로 옵니다.
    let params = api::MessageSendParams {
        space: S1,
        channel: CH,
        text: "안녕".into(),
        attachments: vec![],
        reply_to: None,
        thread: None,
    };
    c.request::<m::MessageSend>(params).await.unwrap();
    let batch = tokio::time::timeout(Duration::from_secs(5), sub.next())
        .await
        .unwrap()
        .unwrap();
    let Notification::MessageNew { message, .. } = &batch[0] else {
        panic!("{batch:?}")
    };
    assert_eq!(message.text, "안녕");

    // 공간 제한 토큰은 다른 공간의 알림을 받지 않습니다.
    let crew = client(&handle, Some("crew")).await;
    let mut crew_sub = crew.subscribe(vec![]).await.unwrap();
    hub.publish(Notification::SpaceUpdated { space: S2 });
    hub.publish(Notification::SpaceUpdated { space: S1 });
    let batch = tokio::time::timeout(Duration::from_secs(5), crew_sub.next())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(batch, vec![Notification::SpaceUpdated { space: S1 }]);

    // 구독을 버리면 서버 쪽 구독도 사라집니다.
    drop(sub);
    drop(crew_sub);
    let deadline = Instant::now() + Duration::from_secs(5);
    while hub.subscriber_count() > 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(hub.subscriber_count(), 0);
}

#[tokio::test]
async fn slow_client_gets_dropped_notice() {
    let config = ServerConfig {
        notification_queue: 4,
        ..Default::default()
    };
    let (_dir, handle, _mock, hub) = start(config).await;
    let c = client(&handle, Some("owner")).await;
    let mut sub = c.subscribe(vec!["space".into()]).await.unwrap();
    // 쓰기 작업이 돌 틈 없이 한꺼번에 내면 큐(4)를 넘친 알림은 버려집니다. 데몬은 막히지 않습니다.
    let started = Instant::now();
    let mut delivered = 0;
    for _ in 0..1000 {
        delivered += hub.publish(Notification::SpaceUpdated { space: S1 });
    }
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(delivered < 1000);
    hub.publish(Notification::SpaceUpdated { space: S1 });
    tokio::time::sleep(Duration::from_millis(50)).await;
    hub.publish(Notification::SpaceUpdated { space: S1 });
    let mut saw_dropped = 0u64;
    let deadline = Instant::now() + Duration::from_secs(5);
    while saw_dropped == 0 && Instant::now() < deadline {
        let Ok(Some(batch)) = tokio::time::timeout(Duration::from_secs(2), sub.next()).await else {
            break;
        };
        for n in batch {
            if let Notification::EventsDropped { count } = n {
                saw_dropped += count;
            }
        }
    }
    assert!(saw_dropped > 0, "버린 알림 수를 알려야 함");
}

#[tokio::test]
async fn binary_frames_for_blob_read() {
    let (_dir, handle, _mock, _hub) = start(ServerConfig::default()).await;
    let c = client(&handle, Some("reader")).await;
    let params = api::BlobReadParams {
        space: S1,
        blob: yangpa_core::ids::BlobId([7; 32]),
        key_from: EventId([8; 32]),
        index: None,
        offset: None,
        len: Some(3 * 1024 * 1024),
        binary: None,
    };
    let data = c.blob_read(params.clone()).await.unwrap();
    assert_eq!(data.len(), 3 * 1024 * 1024);
    assert!(
        data.iter()
            .enumerate()
            .take(1000)
            .all(|(i, b)| *b == (i % 251) as u8)
    );
    // base64 JSON으로 와도 같은 결과.
    let small = api::BlobReadParams {
        len: Some(100),
        ..params
    };
    let r = c.request::<m::BlobRead>(small.clone()).await.unwrap();
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(r.data_base64.unwrap())
        .unwrap();
    assert_eq!(decoded.len(), 100);
    let via_binary = c
        .call_binary(
            "blob.read",
            api::BlobReadParams {
                binary: None,
                ..small
            },
        )
        .await
        .unwrap();
    assert_eq!(&via_binary[..], &decoded[..]);
}

#[cfg(unix)]
#[tokio::test]
async fn socket_file_permissions_and_stale_cleanup() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rt").join("yangpad.sock");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    // 비정상 종료한 데몬이 남긴 소켓 파일.
    drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
    assert!(path.exists());

    let mock = Mock::new();
    let ep = Endpoint::new(&path);
    let handle = yangpa_rpc::serve(ep.clone(), mock.clone(), EventHub::new())
        .await
        .unwrap();
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);

    // 실행 중인 데몬이 있으면 두 번째 서버는 실패합니다.
    let err = yangpa_rpc::serve(ep.clone(), mock.clone(), EventHub::new())
        .await
        .err()
        .unwrap();
    assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse);

    // 소켓이 아닌 파일은 지우지 않습니다.
    let not_socket = dir.path().join("file.sock");
    std::fs::write(&not_socket, b"keep").unwrap();
    assert!(
        yangpa_rpc::serve(Endpoint::new(&not_socket), mock.clone(), EventHub::new())
            .await
            .is_err()
    );
    assert_eq!(std::fs::read(&not_socket).unwrap(), b"keep");

    let c = client(&handle, Some("owner")).await;
    handle.shutdown();
    handle.wait().await;
    assert!(!path.exists(), "종료하면 소켓 파일을 지움");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !c.is_closed() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(c.is_closed());
    // 두 번째 서버가 실행 여부를 확인하려고 붙었던 연결 + 클라이언트 연결.
    let deadline = Instant::now() + Duration::from_secs(5);
    while mock.disconnects.load(Ordering::SeqCst) < 2 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(mock.disconnects.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn in_memory_connection() {
    let (a, b) = tokio::io::duplex(64 * 1024);
    let mock = Mock::new();
    tokio::spawn(yangpa_rpc::server::serve_connection(
        b,
        mock,
        EventHub::new(),
        ServerConfig::default(),
    ));
    let c = RpcClient::from_stream(a);
    c.hello("owner").await.unwrap();
    let r: api::EventRef = c
        .call(
            "message.send",
            json!({"space": S1.to_string(), "channel": CH.to_string(), "text": "x"}),
        )
        .await
        .unwrap();
    assert_eq!(r.id, EventId::hash_of(b"x"));
}

#[tokio::test]
async fn subscription_outlives_client() {
    let (_dir, handle, _mock, hub) = start(ServerConfig::default()).await;
    let c = client(&handle, Some("owner")).await;
    let mut sub = c.subscribe(vec![]).await.unwrap();
    // 클라이언트를 먼저 버려도 구독은 계속 알림을 받고,
    drop(c);
    hub.publish(Notification::SpaceUpdated { space: S1 });
    let batch = tokio::time::timeout(Duration::from_secs(5), sub.next())
        .await
        .expect("클라이언트를 버린 뒤에도 알림이 와야 함")
        .unwrap();
    assert_eq!(batch, vec![Notification::SpaceUpdated { space: S1 }]);
    // 연결이 닫히면 스트림이 끝납니다(영원히 기다리지 않음).
    handle.shutdown();
    let ended = tokio::time::timeout(Duration::from_secs(5), async {
        while sub.next().await.is_some() {}
    })
    .await;
    assert!(ended.is_ok(), "연결이 닫혔는데 구독 스트림이 끝나지 않음");
}

#[tokio::test]
async fn positional_params_cannot_bypass_space_restriction() {
    let (_dir, handle, _mock, _hub) = start(ServerConfig::default()).await;
    let c = client(&handle, Some("crew")).await;
    // 공간 제한 토큰(S1만)이 배열 인자로 S2를 넣으면 범위 검사를 피해 갈 수 있었습니다.
    for method in ["seed.get", "peer.get", "message.search"] {
        let e = c
            .call_value(method, json!([S2.to_string(), null, "q"]))
            .await
            .unwrap_err();
        assert_eq!(e.code(), Some(INVALID_PARAMS), "{method}");
    }
}

#[tokio::test]
async fn unauthenticated_connections_are_limited() {
    use yangpa_rpc::server::serve_connection;

    // 인증 전에는 큰 프레임을 받지 않습니다.
    let (a, b) = tokio::io::duplex(1 << 20);
    let mock = Mock::new();
    let task = tokio::spawn(serve_connection(
        b,
        mock.clone(),
        EventHub::new(),
        ServerConfig::default(),
    ));
    let c = RpcClient::from_stream(a).with_timeout(Duration::from_secs(5));
    let pad = "x".repeat(100 * 1024);
    let e = c
        .call_value("system.version", json!({ "pad": pad }))
        .await
        .unwrap_err();
    assert!(matches!(e, ClientError::Closed), "{e:?}");
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();

    // 인증 뒤에는 같은 크기도 됩니다.
    let (a, b) = tokio::io::duplex(1 << 20);
    tokio::spawn(serve_connection(
        b,
        mock.clone(),
        EventHub::new(),
        ServerConfig::default(),
    ));
    let c = RpcClient::from_stream(a).with_timeout(Duration::from_secs(5));
    c.hello("owner").await.unwrap();
    let r: api::EventRef = c
        .call(
            "message.send",
            json!({"space": S1.to_string(), "channel": CH.to_string(), "text": pad}),
        )
        .await
        .unwrap();
    assert_eq!(r.id, EventId::hash_of(pad.as_bytes()));

    // 인증하지 않고 머무는 연결은 시간이 지나면 닫힙니다.
    let config = ServerConfig {
        auth_timeout: Duration::from_millis(100),
        ..Default::default()
    };
    let (a, b) = tokio::io::duplex(1 << 16);
    let task = tokio::spawn(serve_connection(b, mock.clone(), EventHub::new(), config));
    let idle = RpcClient::from_stream(a);
    idle.request::<m::SystemVersion>(Empty).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("인증 시간 초과로 연결이 끝나야 함")
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !idle.is_closed() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(idle.is_closed());
}

#[tokio::test]
async fn connection_ends_when_client_stops_reading() {
    use futures::SinkExt;
    use tokio_util::codec::FramedWrite;
    use yangpa_rpc::framing::{Frame, FrameCodec};
    use yangpa_rpc::jsonrpc::Request;
    use yangpa_rpc::server::serve_connection;

    // 응답을 읽지 않는 클라이언트: 쓰기 시간 초과 뒤에는 읽기 쪽도 끝나야 합니다
    // (그러지 않으면 결과를 돌려줄 수 없는 요청을 계속 실행함).
    let config = ServerConfig {
        write_timeout: Duration::from_millis(100),
        ..Default::default()
    };
    let (a, b) = tokio::io::duplex(64);
    let mock = Mock::new();
    let task = tokio::spawn(serve_connection(b, mock.clone(), EventHub::new(), config));
    let (_unread, wr) = tokio::io::split(a);
    let mut sink = FramedWrite::new(wr, FrameCodec::new());
    sink.send(Frame::json(&Request::new(1i64, "auth.hello", json!({"token": "owner"}))).unwrap())
        .await
        .unwrap();
    for i in 2..12i64 {
        sink.send(Frame::json(&Request::new(i, "system.version", Value::Null)).unwrap())
            .await
            .unwrap();
    }
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("쓰기가 막힌 연결이 끝나지 않음")
        .unwrap();
    assert_eq!(mock.disconnects.load(Ordering::SeqCst), 1);
}

fn list_request(id: i64, delay_ms: u32) -> yangpa_rpc::framing::Frame {
    let p = api::MessageListParams {
        space: S1,
        channel: CH,
        before: None,
        limit: Some(delay_ms),
        thread: None,
    };
    let req =
        yangpa_rpc::jsonrpc::Request::new(id, "message.list", serde_json::to_value(p).unwrap());
    yangpa_rpc::framing::Frame::json(&req).unwrap()
}

#[tokio::test]
async fn unread_responses_hold_request_slots() {
    use futures::SinkExt;
    use tokio_util::codec::FramedWrite;
    use yangpa_rpc::framing::{Frame, FrameCodec};
    use yangpa_rpc::jsonrpc::Request;
    use yangpa_rpc::server::serve_connection;

    // 응답을 읽지 않으면 소켓에 쓰지 못한 응답이 요청 자리를 쥐어, 대기열·쓰기 묶음이 커도 새 요청을 시작하지 않습니다
    // (자리를 대기열에 넣을 때 돌려주면 대기열·묶음만큼 응답이 메모리에 쌓임).
    let config = ServerConfig {
        max_concurrent_requests: 2,
        write_timeout: Duration::from_secs(30),
        ..Default::default()
    };
    let (a, b) = tokio::io::duplex(64);
    let mock = Mock::new();
    tokio::spawn(serve_connection(b, mock.clone(), EventHub::new(), config));
    let (_unread, wr) = tokio::io::split(a);
    let mut sink = FramedWrite::new(wr, FrameCodec::new());
    sink.send(
        Frame::json(&Request::new(
            1i64,
            "auth.hello",
            json!({"token": "reader"}),
        ))
        .unwrap(),
    )
    .await
    .unwrap();
    // 서버가 더 읽지 않으면 보내기도 막히므로 따로 보냅니다.
    tokio::spawn(async move {
        for i in 2..22i64 {
            if sink.send(list_request(i, 0)).await.is_err() {
                break;
            }
        }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let starts = mock.starts.load(Ordering::SeqCst);
    assert!(
        (1..=2).contains(&starts),
        "읽지 않는 연결에서 시작한 요청: {starts}"
    );
}

#[tokio::test]
async fn in_flight_requests_keep_the_connection_slot() {
    // 끊어도 처리 중인 요청이 끝날 때까지 연결 자리를 돌려주지 않습니다(끊고 다시 붙기로 연결 수 상한을 넘지 못하게).
    let config = ServerConfig {
        max_connections: 1,
        ..Default::default()
    };
    let (_dir, handle, mock, _hub) = start(config).await;
    let c = client(&handle, Some("reader")).await;
    let slow = api::MessageListParams {
        space: S1,
        channel: CH,
        before: None,
        limit: Some(1500),
        thread: None,
    };
    let call = c.request::<m::MessageList>(slow);
    assert!(
        tokio::time::timeout(Duration::from_millis(200), call)
            .await
            .is_err()
    );
    assert_eq!(mock.in_flight.load(Ordering::SeqCst), 1);
    drop(c);
    let deadline = Instant::now() + Duration::from_secs(5);
    while mock.disconnects.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(mock.disconnects.load(Ordering::SeqCst), 1);

    assert!(
        client_or_none(&handle).await.is_none(),
        "처리 중인 요청이 있는데 자리를 내줌"
    );

    let deadline = Instant::now() + Duration::from_secs(5);
    while mock.in_flight.load(Ordering::SeqCst) > 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // 요청이 끝나면 자리가 납니다(마지막 복제본이 놓이는 사이를 조금 기다림).
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(c) = client_or_none(&handle).await {
            c.request::<m::SystemVersion>(Empty).await.unwrap();
            break;
        }
        assert!(
            Instant::now() < deadline,
            "요청이 끝났는데 자리가 나지 않음"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn client_or_none(handle: &ServerHandle) -> Option<RpcClient> {
    let c = RpcClient::connect(handle.endpoint())
        .await
        .ok()?
        .with_timeout(Duration::from_secs(5));
    c.hello("reader").await.ok()?;
    Some(c)
}
