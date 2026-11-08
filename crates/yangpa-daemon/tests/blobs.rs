//! 블롭·정책 통합 시험: 첨부 스웜(여러 시더, 손상 조각, 이어 받기), 시더 설정·메시지 제외, 프로필 이미지,
//! 공간 이모지, 링크 미리보기(SSRF 방어), 휴면 공간, 남의 블롭 ID를 베낀 메시지·프로필(제공·정리),
//! 자동 내려받기 중의 직접 요청, 보내는 쪽이 만든 썸네일·재생 길이, 범위별 속도 제한과 전송 속도.
//!
//! 고정 sleep 없이 조건이 될 때까지 기다립니다(`common::eventually`). 조각 수를 늘리려고 보내는 쪽 조각 크기를
//! 최소(16KiB)로 둡니다.

use crate::common;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use base64::Engine;
use common::{
    Cluster, TestNode, befriend, connect_both, eventually, next_notification, wait_message,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use yangpa_core::blob::MIN_CHUNK_SIZE;
use yangpa_core::body::{Attachment, Secret32};
use yangpa_core::event::{EventKind, StreamId};
use yangpa_core::ids::{BlobId, ChannelId, EventId, SpaceId};
use yangpa_core::wire::{WireErrorCode, YpRequest, YpResponse};
use yangpa_daemon::NodeConfig;
use yangpa_daemon::testkit;
use yangpa_rpc::api::{
    AttachmentInput, BlobFetchParams, BlobParams, BlobPasteDiscardParams, BlobPasteParams,
    BlobReadParams, BlobStatusView, Empty, MessageRefParams, MessageSendParams, Notification,
    OptSpaceParams, PeerSetParams, ProfileGetParams, ProfileSetParams, RateLimitParams,
    RateLimitView, RateScope, SeedExcludeParams, SeedSetParams, SpaceParams, m,
};
use yangpa_rpc::jsonrpc::{ERR_FORBIDDEN, ERR_INVALID_INPUT};
use yangpa_rpc::{Endpoint, RpcClient};

// ───────────── 도우미 ─────────────

fn small_chunks(cfg: &mut NodeConfig) {
    cfg.sharing.chunk_size = MIN_CHUNK_SIZE;
}

fn quick_dormancy(cfg: &mut NodeConfig) {
    cfg.sharing.dormant_after = Duration::from_secs(2);
    cfg.sync_interval = Duration::from_millis(300);
}

/// 시험 서버(127.0.0.1의 임의 포트)를 조회할 수 있게 주소 검사와 포트 제한을 모두 끕니다.
fn private_previews(cfg: &mut NodeConfig) {
    cfg.sharing.link_preview.allow_private_addrs = true;
    cfg.sharing.link_preview.allow_any_port = true;
}

/// 포트 제한만 끕니다(주소 검사는 그대로): 루프백 거절이 포트가 아니라 SSRF 주소 검사에서 나오는지 봅니다.
fn any_port_previews(cfg: &mut NodeConfig) {
    cfg.sharing.link_preview.allow_any_port = true;
}

/// 메시지에 그대로 실을 첨부(블롭 ID·키·크기를 마음대로).
fn raw_attachment(blob: BlobId, key: Secret32, size: u64) -> Attachment {
    Attachment {
        blob,
        key,
        name: "베낀 첨부.bin".into(),
        mime: "application/octet-stream".into(),
        size,
        width: None,
        height: None,
        duration_ms: None,
        thumbnail: Vec::new(),
    }
}

/// 블롭 요청 세 가지(매니페스트·보유 비트맵·첫 조각).
fn blob_asks(space: SpaceId, blob: BlobId) -> [YpRequest; 3] {
    [
        YpRequest::BlobManifest { space, blob },
        YpRequest::BlobHave { space, blob },
        YpRequest::BlobChunk {
            space,
            blob,
            index: 0,
        },
    ]
}

/// 내용 확인용 결정적 바이트.
fn sample(len: usize, seed: u8) -> Vec<u8> {
    common::noise(len, seed)
}

/// png 서명·IHDR과 잡음을 담은 상자 하나(형식 판별·크기 읽기용, 상자 구조는 맞아 메타데이터 지우기를 지남).
fn png(len: usize, seed: u8) -> Vec<u8> {
    let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
    v.extend_from_slice(&13u32.to_be_bytes());
    v.extend_from_slice(b"IHDR");
    v.extend_from_slice(&64u32.to_be_bytes());
    v.extend_from_slice(&48u32.to_be_bytes());
    v.extend_from_slice(&[8, 6, 0, 0, 0, 0, 0, 0, 0]);
    let rest = len.saturating_sub(v.len() + 12);
    v.extend_from_slice(&(rest as u32).to_be_bytes());
    v.extend_from_slice(b"IDAT");
    v.extend(sample(rest, seed));
    v.extend_from_slice(&[0; 4]);
    v
}

/// 진짜로 풀리는 잡음 PNG(압축이 잘 안 돼 썸네일 한도보다 큼).
fn noisy_png(width: u32, height: u32) -> Vec<u8> {
    let mut x = 0x9E37_79B9u32;
    let img = image::RgbImage::from_fn(width, height, |_, _| {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        let [r, g, b, _] = x.to_le_bytes();
        image::Rgb([r, g, b])
    });
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(img)
        .write_to(&mut out, image::ImageFormat::Png)
        .unwrap();
    out.into_inner()
}

/// 길이가 `ms`밀리초인 최소 MP4(ftyp·mdat·moov/mvhd v0, 시간 단위 1000).
fn tiny_mp4(ms: u32) -> Vec<u8> {
    fn bx(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut v = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        v.extend_from_slice(kind);
        v.extend_from_slice(body);
        v
    }
    let mut mvhd = vec![0u8; 12];
    mvhd.extend_from_slice(&1000u32.to_be_bytes());
    mvhd.extend_from_slice(&ms.to_be_bytes());
    mvhd.extend_from_slice(&[0; 80]);
    let mut v = bx(b"ftyp", b"isom\0\0\0\0isommp41");
    v.extend(bx(b"mdat", &sample(40_000, 3)));
    v.extend(bx(b"moov", &bx(b"mvhd", &mvhd)));
    v
}

fn write_file(n: &TestNode, name: &str, bytes: &[u8]) -> String {
    let dir = n.dir.join("files");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, bytes).unwrap();
    assert!(Path::new(&path).is_absolute());
    path.to_string_lossy().into_owned()
}

/// 파일 하나를 첨부해 보냅니다. (메시지, 블롭).
async fn send_file(
    n: &TestNode,
    space: SpaceId,
    channel: ChannelId,
    path: &str,
) -> (EventId, BlobId) {
    let id = n
        .ok::<m::MessageSend>(MessageSendParams {
            space,
            channel,
            text: "파일".into(),
            attachments: vec![AttachmentInput {
                path: path.into(),
                ..Default::default()
            }],
            reply_to: None,
            thread: None,
        })
        .await
        .id;
    let view = n
        .messages(space, channel)
        .await
        .into_iter()
        .find(|v| v.id == id)
        .expect("보낸 메시지");
    assert!(view.attachments[0].downloaded, "보낸 쪽은 이미 가짐");
    (id, view.attachments[0].blob)
}

async fn status(n: &TestNode, space: SpaceId, blob: BlobId) -> BlobStatusView {
    n.ok::<m::BlobStatus>(BlobParams { space, blob }).await
}

async fn wait_complete(n: &TestNode, space: SpaceId, blob: BlobId, what: &str) -> BlobStatusView {
    eventually(what, || async {
        let s = status(n, space, blob).await;
        (s.total_chunks > 0 && s.have_chunks == s.total_chunks).then_some(s)
    })
    .await
}

async fn read_blob(n: &TestNode, space: SpaceId, blob: BlobId, key_from: EventId) -> Vec<u8> {
    let r = n
        .ok::<m::BlobRead>(BlobReadParams {
            space,
            blob,
            key_from,
            index: None,
            offset: None,
            len: None,
            binary: None,
        })
        .await;
    base64::engine::general_purpose::STANDARD
        .decode(r.data_base64.expect("base64 결과"))
        .unwrap()
}

/// 세 노드 크루: 서로 연결·인증하고 모두 첫 채널을 연(메시지 목록을 본) 상태.
async fn crew(
    cl: &Cluster,
    tweak: fn(&mut NodeConfig),
) -> (TestNode, TestNode, TestNode, SpaceId, ChannelId) {
    let a = cl.node_with("a", tweak).await;
    let b = cl.node_with("b", tweak).await;
    let c = cl.node_with("c", tweak).await;
    let (space, channel) = testkit::blobs::create_group_space(&a.node, &[&b.node, &c.node])
        .await
        .expect("크루 공간");
    connect_both(&a, &b).await;
    connect_both(&a, &c).await;
    connect_both(&b, &c).await;
    for (x, y) in [(&a, &b), (&a, &c), (&b, &c)] {
        testkit::authenticate(&x.node, y.peer())
            .await
            .expect("인증");
    }
    eventually("모두 같은 크루 키 순번", || async {
        let ea = testkit::crew_key_seq(&a.node, &space)?;
        let eb = testkit::crew_key_seq(&b.node, &space)?;
        let ec = testkit::crew_key_seq(&c.node, &space)?;
        (ea == eb && eb == ec).then_some(())
    })
    .await;
    for n in [&a, &b, &c] {
        n.messages(space, channel).await;
    }
    (a, b, c, space, channel)
}

async fn net_request(from: &TestNode, to: &TestNode, req: YpRequest) -> YpResponse {
    testkit::net_request(&from.node, to.peer(), req)
        .await
        .expect("요청 전송")
}

fn refused_with(r: &YpResponse, code: WireErrorCode) -> bool {
    matches!(r, YpResponse::Error { code: c, .. } if *c == code)
}

// ───────────── 1. 여러 시더에게서 받기 ─────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attachment_downloads_from_several_seeders() {
    let cl = Cluster::new();
    let (a, b, c, space, channel) = crew(&cl, small_chunks).await;
    // C는 B가 다 받을 때까지 조각을 요청하지 않게 멈춰 둡니다(두 시더를 모두 알고 시작하게).
    testkit::blobs::hold_downloads(&c.node, Some(0));
    let file = sample(190_000, 1);
    let path = write_file(&a, "사진.bin", &file);
    let (id, blob) = send_file(&a, space, channel, &path).await;

    // B는 자동으로 받습니다(크루 채널을 열어 둠, 자동 다운로드 한도 안).
    wait_message(
        &b,
        space,
        channel,
        id,
        "B가 첨부를 자동으로 받음",
        |v| v.attachments.first().is_some_and(|x| x.downloaded),
    )
    .await;
    let sb = status(&b, space, blob).await;
    assert_eq!(sb.size, file.len() as u64);
    assert!(sb.total_chunks >= 10, "조각이 여럿이어야 함: {sb:?}");
    assert_eq!(read_blob(&b, space, blob, id).await, file);
    let from_b = testkit::blobs::fetch_stats(&b.node, &blob);
    assert_eq!(from_b.chunks_from.get(&a.peer()), Some(&sb.total_chunks));

    // C는 A와 B가 모두 가졌다는 것을 안 뒤에 풀어 줍니다.
    eventually("C가 시더 둘을 앎", || async {
        (testkit::blobs::seeders_seen(&c.node, &blob) >= 2).then_some(())
    })
    .await;
    testkit::blobs::hold_downloads(&c.node, None);
    let sc = wait_complete(&c, space, blob, "C가 다 받음").await;
    assert_eq!(sc.size, file.len() as u64);
    let from_c = testkit::blobs::fetch_stats(&c.node, &blob);
    assert!(
        from_c.chunks_from.get(&a.peer()).copied().unwrap_or(0) > 0
            && from_c.chunks_from.get(&b.peer()).copied().unwrap_or(0) > 0,
        "C는 A와 B 모두에게서 받아야 함: {from_c:?}"
    );
    assert_eq!(from_c.chunks_from.values().sum::<u32>(), sc.total_chunks);
    assert_eq!(from_c.bad_chunks, 0);
    eventually("C의 내려받기 표시가 꺼짐", || async {
        (!status(&c, space, blob).await.downloading).then_some(())
    })
    .await;

    // 바이너리 프레임 읽기(로컬 API 서버).
    let endpoint = Endpoint::in_dir(&c.dir);
    let server = yangpa_rpc::serve(
        endpoint.clone(),
        Arc::new(yangpa_daemon::ApiHandler::new(c.node.clone())),
        c.node.event_hub(),
    )
    .await
    .expect("로컬 API 열기");
    let token = yangpa_rpc::endpoint::read_token_file(
        &c.node
            .data_dir()
            .join(yangpa_rpc::endpoint::OWNER_TOKEN_FILE),
    )
    .expect("소유자 토큰");
    let (client, _) = RpcClient::connect_with_token(&endpoint, token.expose())
        .await
        .expect("토큰 인증");
    let bytes = client
        .blob_read(BlobReadParams {
            space,
            blob,
            key_from: id,
            index: None,
            offset: Some(1000),
            len: Some(50_000),
            binary: None,
        })
        .await
        .expect("바이너리 읽기");
    assert_eq!(&bytes[..], &file[1000..51_000]);
    drop(client);
    server.shutdown();
    server.wait().await;

    // 저장: 덮어쓰지 않음.
    let out = c.dir.join("files").join("저장본.bin");
    std::fs::create_dir_all(out.parent().unwrap()).unwrap();
    let save = yangpa_rpc::api::BlobSaveParams {
        space,
        blob,
        key_from: id,
        path: out.to_string_lossy().into_owned(),
        allow_volatile: false,
    };
    c.ok::<m::BlobSave>(save.clone()).await;
    assert_eq!(std::fs::read(&out).unwrap(), file);
    // 받은 파일이라 Office·SmartScreen이 검사하도록 "인터넷에서 받음" 표시가 붙습니다(주소는 적지 않음).
    #[cfg(windows)]
    assert_eq!(
        std::fs::read_to_string(format!("{}:Zone.Identifier", out.display())).unwrap(),
        "[ZoneTransfer]\r\nZoneId=3\r\n"
    );
    assert!(
        c.call::<m::BlobSave>(save).await.is_err(),
        "있는 파일은 덮어쓰지 않음"
    );
}

/// 줄어드는 첨부(글)는 OpenZL로 압축해 조각 수가 줄고, 받는 쪽은 원래 내용 그대로 읽고(범위 읽기 포함) 저장합니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn compressible_attachment_is_sent_compressed() {
    let cl = Cluster::new();
    let (a, b, _c, space, channel) = crew(&cl, small_chunks).await;
    let mut file = Vec::new();
    for i in 0..8000 {
        file.extend(format!("{i}번째 줄: 양파 메신저 기록 내보내기\n").as_bytes());
    }
    let path = write_file(&a, "기록.txt", &file);
    let (id, blob) = send_file(&a, space, channel, &path).await;
    wait_message(
        &b,
        space,
        channel,
        id,
        "B가 첨부를 자동으로 받음",
        |v| v.attachments.first().is_some_and(|x| x.downloaded),
    )
    .await;
    let sb = status(&b, space, blob).await;
    assert_eq!(sb.size, file.len() as u64);
    let raw_chunks = file.len().div_ceil(MIN_CHUNK_SIZE as usize - 16) as u32;
    assert!(
        sb.total_chunks * 5 < raw_chunks,
        "압축돼 조각이 적어야 함: {sb:?}, 그대로면 {raw_chunks}개"
    );
    assert_eq!(read_blob(&b, space, blob, id).await, file);
    let part = b
        .ok::<m::BlobRead>(BlobReadParams {
            space,
            blob,
            key_from: id,
            index: None,
            offset: Some(100_000),
            len: Some(5000),
            binary: None,
        })
        .await;
    let part = base64::engine::general_purpose::STANDARD
        .decode(part.data_base64.unwrap())
        .unwrap();
    assert_eq!(part, file[100_000..105_000]);
    let out = b.dir.join("저장한 기록.txt");
    b.ok::<m::BlobSave>(yangpa_rpc::api::BlobSaveParams {
        space,
        blob,
        key_from: id,
        path: out.to_string_lossy().into_owned(),
        allow_volatile: false,
    })
    .await;
    assert_eq!(std::fs::read(&out).unwrap(), file);
}

// ───────────── 2. 손상된 조각 ─────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn corrupted_chunks_are_refetched() {
    let cl = Cluster::new();
    let (a, b, c, space, channel) = crew(&cl, small_chunks).await;
    testkit::blobs::hold_downloads(&c.node, Some(0));
    let file = sample(150_000, 2);
    let path = write_file(&a, "문서.bin", &file);
    let (id, blob) = send_file(&a, space, channel, &path).await;
    wait_message(&b, space, channel, id, "B가 받음", |v| {
        v.attachments.first().is_some_and(|x| x.downloaded)
    })
    .await;
    // B는 이제 보내는 조각을 망가뜨립니다.
    testkit::blobs::corrupt_uploads(&b.node, true);
    eventually("C가 시더 둘을 앎", || async {
        (testkit::blobs::seeders_seen(&c.node, &blob) >= 2).then_some(())
    })
    .await;
    testkit::blobs::hold_downloads(&c.node, None);
    wait_complete(&c, space, blob, "C가 손상 조각을 버리고 다 받음").await;
    let st = testkit::blobs::fetch_stats(&c.node, &blob);
    assert!(st.bad_chunks >= 1, "B의 조각은 검증에 실패해야 함: {st:?}");
    assert_eq!(
        st.chunks_from.get(&b.peer()),
        None,
        "망가진 조각은 저장하지 않음"
    );
    assert_eq!(read_blob(&c, space, blob, id).await, file);
    // 몇 번의 검증 실패로는 연결을 끊지 않습니다(이 블롭에서만 뺌).
    let pong = net_request(&c, &b, YpRequest::Ping).await;
    assert!(matches!(pong, YpResponse::Pong), "{pong:?}");
}

// ───────────── 3. 시더 끄기 ─────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn seeder_off_refuses_peers() {
    let cl = Cluster::new();
    let a = cl.node_with("a", small_chunks).await;
    let b = cl.node("b").await;
    let space = befriend(&a, &b).await;
    let channel = a.first_channel(space).await;
    a.ok::<m::SeedSet>(SeedSetParams {
        enabled: Some(false),
        ..Default::default()
    })
    .await;
    let got = a.ok::<m::SeedGet>(OptSpaceParams { space: None }).await;
    assert!(!got.enabled);
    let file = sample(60_000, 3);
    let path = write_file(&a, "끈 시더.bin", &file);
    let (id, blob) = send_file(&a, space, channel, &path).await;
    wait_message(
        &b,
        space,
        channel,
        id,
        "B가 메시지를 받음(전파는 시더 설정과 무관)",
        |_| true,
    )
    .await;

    for req in [
        YpRequest::BlobManifest { space, blob },
        YpRequest::BlobHave { space, blob },
        YpRequest::BlobChunk {
            space,
            blob,
            index: 0,
        },
    ] {
        let r = net_request(&b, &a, req).await;
        assert!(refused_with(&r, WireErrorCode::NotSeeded), "{r:?}");
    }
    // 이 공간에서 쓰이지 않는 블롭은 있는지도 알리지 않습니다.
    let r = net_request(
        &b,
        &a,
        YpRequest::BlobManifest {
            space,
            blob: BlobId([7; 32]),
        },
    )
    .await;
    assert!(refused_with(&r, WireErrorCode::NotFound), "{r:?}");
    assert_ne!(
        status(&b, space, blob).await.have_chunks,
        status(&a, space, blob).await.have_chunks
    );

    // 다시 켜면 직접 요청으로 받습니다.
    a.ok::<m::SeedSet>(SeedSetParams {
        enabled: Some(true),
        ..Default::default()
    })
    .await;
    b.ok::<m::BlobFetch>(BlobFetchParams {
        space,
        blob,
        priority: Some(200),
    })
    .await;
    wait_complete(&b, space, blob, "시더를 켠 뒤 받음").await;
    assert_eq!(read_blob(&b, space, blob, id).await, file);
}

// ───────────── 4. 메시지 단위 제외 ─────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn excluded_message_is_not_seeded() {
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let b = cl.node("b").await;
    let space = befriend(&a, &b).await;
    let channel = a.first_channel(space).await;
    // B는 직접 요청할 때만 받습니다.
    b.ok::<m::PeerSet>(PeerSetParams {
        auto_download_max_bytes: Some(0),
        ..Default::default()
    })
    .await;
    let file = sample(40_000, 4);
    let path = write_file(&a, "제외.bin", &file);
    let (id, blob) = send_file(&a, space, channel, &path).await;
    a.ok::<m::SeedExcludeMessage>(SeedExcludeParams {
        space,
        channel,
        id,
        excluded: true,
    })
    .await;
    let got = a
        .ok::<m::SeedGet>(OptSpaceParams { space: Some(space) })
        .await;
    assert_eq!(got.excluded_events, vec![id]);
    wait_message(&b, space, channel, id, "B가 메시지를 받음", |v| {
        !v.attachments.is_empty() && !v.attachments[0].downloaded
    })
    .await;
    for req in [
        YpRequest::BlobManifest { space, blob },
        YpRequest::BlobChunk {
            space,
            blob,
            index: 0,
        },
    ] {
        let r = net_request(&b, &a, req).await;
        assert!(refused_with(&r, WireErrorCode::NotSeeded), "{r:?}");
    }

    a.ok::<m::SeedExcludeMessage>(SeedExcludeParams {
        space,
        channel,
        id,
        excluded: false,
    })
    .await;
    b.ok::<m::BlobFetch>(BlobFetchParams {
        space,
        blob,
        priority: None,
    })
    .await;
    wait_complete(&b, space, blob, "제외를 풀고 받음").await;
    assert_eq!(read_blob(&b, space, blob, id).await, file);
}

// ───────────── 5. 재시작 뒤 이어 받기 ─────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn partial_download_resumes_after_restart() {
    let cl = Cluster::new();
    let a = cl.node_with("a", small_chunks).await;
    let mut b = cl.node("b").await;
    let space = befriend(&a, &b).await;
    let channel = a.first_channel(space).await;
    const BEFORE: u32 = 5;
    testkit::blobs::hold_downloads(&b.node, Some(BEFORE));
    let file = sample(190_000, 5);
    let path = write_file(&a, "큰 파일.bin", &file);
    let (id, blob) = send_file(&a, space, channel, &path).await;
    let total = eventually("B가 일부만 받음", || async {
        let (have, total) = testkit::blobs::chunk_progress(&b.node, &blob)?;
        (have == BEFORE).then_some(total)
    })
    .await;
    assert!(total > BEFORE + 3, "조각이 충분히 많아야 함: {total}");

    b.restart().await;
    wait_complete(&b, space, blob, "재시작 뒤 이어 받음").await;
    let st = testkit::blobs::fetch_stats(&b.node, &blob);
    assert_eq!(
        st.chunks_from.values().sum::<u32>(),
        total - BEFORE,
        "받은 조각은 다시 받지 않음: {st:?}"
    );
    assert_eq!(read_blob(&b, space, blob, id).await, file);
}

// ───────────── 6. 프로필 이미지 ─────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn avatar_is_visible_to_friend() {
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let b = cl.node("b").await;
    let image = png(30_000, 6);
    let path = write_file(&a, "아바타.png", &image);
    let mine = a
        .ok::<m::ProfileSet>(ProfileSetParams {
            avatar_path: Some(path),
            ..Default::default()
        })
        .await;
    let avatar = mine.avatar.expect("아바타 블롭");
    // 키는 로컬 API JSON에 나오지 않습니다.
    let json = serde_json::to_value(&mine).unwrap();
    assert!(json["avatar_key"].is_null(), "{json}");
    // 이미지가 아닌 파일은 거절.
    let txt = write_file(&a, "아님.txt", b"not an image");
    assert!(
        a.call::<m::ProfileSet>(ProfileSetParams {
            avatar_path: Some(txt),
            ..Default::default()
        })
        .await
        .is_err()
    );

    let space = befriend(&a, &b).await;
    let theirs = b
        .ok::<m::ProfileGet>(ProfileGetParams {
            user: Some(a.user()),
        })
        .await;
    assert_eq!(theirs.avatar, Some(avatar));
    b.ok::<m::BlobFetch>(BlobFetchParams {
        space,
        blob: avatar,
        priority: None,
    })
    .await;
    wait_complete(&b, space, avatar, "B가 A의 아바타를 받음").await;
    // 프로필 이미지는 `key_from`과 무관하게 서명된 프로필의 키로 엽니다.
    assert_eq!(read_blob(&b, space, avatar, EventId([0; 32])).await, image);
    // 친구가 아닌 C: 인증 전에는 거절됩니다.
    let c = cl.node("c").await;
    connect_both(&a, &c).await;
    let ask = |space| YpRequest::BlobManifest {
        space,
        blob: avatar,
    };
    let r = net_request(&c, &a, ask(space)).await;
    assert!(
        refused_with(&r, WireErrorCode::Unauthenticated)
            || refused_with(&r, WireErrorCode::NotMember),
        "{r:?}"
    );
    // 크루를 함께 써 인증된 C도 A–B 공간의 멤버가 아니면 그 공간으로는 받지 못합니다.
    let (crew, _) = testkit::blobs::create_group_space(&a.node, &[&c.node])
        .await
        .expect("크루 공간");
    testkit::authenticate(&c.node, a.peer())
        .await
        .expect("크루 멤버로 인증");
    let r = net_request(&c, &a, ask(space)).await;
    assert!(refused_with(&r, WireErrorCode::NotMember), "{r:?}");
    // 같은 크루 멤버에게는 그 크루 공간으로 프로필 이미지를 내줍니다.
    let r = net_request(&c, &a, ask(crew)).await;
    assert!(matches!(r, YpResponse::Manifest(_)), "{r:?}");
}

/// 친구가 된 뒤 바꾼 아바타: 받는 쪽은 `profile.updated`를 받고, 첨부 자동 받기 한도(Tor 기본 2MiB)보다 프로필 이미지
/// 상한(8MiB)이 커도 따로 요청하지 않고 새 사진을 받습니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn avatar_change_reaches_friend_under_tor_download_limit() {
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let b = cl.node("b").await;
    let space = befriend(&a, &b).await;
    b.ok::<m::PeerSet>(PeerSetParams {
        auto_download_max_bytes: Some(2 * 1024 * 1024),
        ..Default::default()
    })
    .await;
    let mut rx = b.subscribe();
    let image = png(30_000, 7);
    let path = write_file(&a, "새 아바타.png", &image);
    let avatar = a
        .ok::<m::ProfileSet>(ProfileSetParams {
            avatar_path: Some(path),
            ..Default::default()
        })
        .await
        .avatar
        .expect("아바타 블롭");
    let user = a.user();
    next_notification(&mut rx, "B의 profile.updated", |n| {
        matches!(n, Notification::ProfileUpdated { user: u } if *u == user).then_some(())
    })
    .await;
    let theirs = b
        .ok::<m::ProfileGet>(ProfileGetParams { user: Some(user) })
        .await;
    assert_eq!(theirs.avatar, Some(avatar));
    // blob.fetch 없이 받습니다.
    wait_complete(&b, space, avatar, "B가 A의 새 아바타를 스스로 받음").await;
    assert_eq!(read_blob(&b, space, avatar, EventId([0; 32])).await, image);
}

/// 전역 송신 상한·동시 전송 수를 바꾸면, 크루 화면의 스위치로 복사돼 이전 전역 값을 그대로 가진 공간 설정도 따라
/// 바뀝니다. 공간에서 따로 정한 값은 그대로입니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn global_rate_follows_into_untouched_space_policies() {
    const MIB: u64 = 1024 * 1024;
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let b = cl.node("b").await;
    let c = cl.node("c").await;
    let copied = befriend(&a, &b).await;
    let own = befriend(&a, &c).await;
    a.ok::<m::SeedSet>(SeedSetParams {
        space: Some(copied),
        enabled: Some(true),
        ..Default::default()
    })
    .await;
    a.ok::<m::SeedSet>(SeedSetParams {
        space: Some(own),
        upload_limit_bps: Some(Some(MIB)),
        ..Default::default()
    })
    .await;
    a.ok::<m::SeedSet>(SeedSetParams {
        upload_limit_bps: Some(Some(100 * MIB)),
        max_concurrent: Some(8),
        ..Default::default()
    })
    .await;
    let get = |space| a.ok::<m::SeedGet>(OptSpaceParams { space: Some(space) });
    let p = get(copied).await;
    assert_eq!((p.upload_limit_bps, p.max_concurrent), (Some(100 * MIB), 8));
    let p = get(own).await;
    assert_eq!((p.upload_limit_bps, p.max_concurrent), (Some(MIB), 8));
}

/// 예전 기본 대역폭(시더 송신 2MiB/s·Tor 256KiB/s, Tor 피어 수신 512KiB/s)이 그대로 저장된 설정은 다음 시작 때 새
/// 기본값(전역)과 그 전역 값(공간)으로 옮겨집니다. 따로 정한 값은 그대로입니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn legacy_default_rates_migrate_on_start() {
    const MIB: u64 = 1024 * 1024;
    let cl = Cluster::new();
    let mut a = cl.node("a").await;
    let b = cl.node("b").await;
    let c = cl.node("c").await;
    let legacy = befriend(&a, &b).await;
    let own = befriend(&a, &c).await;
    a.ok::<m::SeedSet>(SeedSetParams {
        space: Some(own),
        upload_limit_bps: Some(Some(3 * MIB)),
        ..Default::default()
    })
    .await;
    testkit::plant_legacy_rates(&a.node, &legacy).expect("예전 설정 심기");
    a.restart().await;
    let seed = |space| a.ok::<m::SeedGet>(OptSpaceParams { space: Some(space) });
    assert_eq!(seed(legacy).await.upload_limit_bps, Some(10 * MIB));
    assert_eq!(seed(own).await.upload_limit_bps, Some(3 * MIB));
    let peer = a.ok::<m::PeerGet>(OptSpaceParams { space: None }).await;
    assert_eq!(peer.download_limit_bps, Some(10 * MIB));
}

// ───────────── 7. 공간 이모지 ─────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn space_emoji_is_fetchable_by_member() {
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let b = cl.node("b").await;
    let space = befriend(&a, &b).await;
    // 옛 상한(256KiB)보다 큰 이모지도 받습니다(크기 한도는 크루가 정할 때만).
    let image = png(1_200_000, 7);
    let (blob, ev) = testkit::blobs::add_emoji(&a.node, space, "양파", &image)
        .await
        .expect("이모지 추가");
    eventually("B가 이모지를 앎", || async {
        testkit::space_state(&b.node, &space)?
            .emojis
            .values()
            .any(|e| e.blob == blob)
            .then_some(())
    })
    .await;
    // 정리 주기가 공간 이모지를 받을 블롭으로 넣습니다(자동 다운로드). 피어 설정을 다시 걸러도 크기 한도 없는 이모지는 받을
    // 블롭에서 빠지지 않습니다(1.2MB는 여러 조각이라 그 사이에 다 받을 수 없음).
    testkit::blobs::sweep_now(&b.node);
    testkit::blobs::peer_policy_changed_now(&b.node);
    assert!(
        testkit::blobs::want_deferred(&b.node, &blob).is_some(),
        "피어 설정을 다시 걸러도 이모지는 받을 블롭에 남음"
    );
    wait_complete(&b, space, blob, "B가 이모지 이미지를 받음").await;
    assert_eq!(read_blob(&b, space, blob, ev).await, image);
    assert_eq!(status(&b, space, blob).await.size, image.len() as u64);
}

// ───────────── 8. 링크 미리보기 ─────────────

const PAGE: &str = "<!doctype html><html><head>\
    <meta property=\"og:title\" content=\"양파 미리보기\">\
    <meta property=\"og:description\" content=\"설명 &amp; 요약\">\
    <meta property=\"og:image\" content=\"/img.png\">\
    <title>대체 제목</title></head><body><p>본문</p></body></html>";
const BIG_PAGE: &str = "<!doctype html><html><head>\
    <meta property=\"og:title\" content=\"큰 그림\">\
    <meta property=\"og:image\" content=\"/big.png\">\
    </head><body></body></html>";

/// 127.0.0.1의 작은 HTTP 서버. 받은 요청 수를 셉니다.
async fn preview_server(hits: Arc<AtomicUsize>, image: Vec<u8>, big: Vec<u8>) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = listener.accept().await {
            let hits = hits.clone();
            let image = image.clone();
            let big = big.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 1024];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    match s.read(&mut tmp).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buf.extend_from_slice(&tmp[..n]),
                    }
                }
                hits.fetch_add(1, Ordering::SeqCst);
                let req = String::from_utf8_lossy(&buf).into_owned();
                let path = req.split_whitespace().nth(1).unwrap_or("/").to_owned();
                let (status, ct, body) = match path.as_str() {
                    "/page" => (
                        "200 OK",
                        "text/html; charset=utf-8",
                        PAGE.as_bytes().to_vec(),
                    ),
                    "/img.png" => ("200 OK", "image/png", image),
                    "/big" => ("200 OK", "text/html", BIG_PAGE.as_bytes().to_vec()),
                    "/big.png" => ("200 OK", "image/png", big),
                    _ => ("404 Not Found", "text/plain", b"none".to_vec()),
                };
                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: {ct}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = s.write_all(head.as_bytes()).await;
                let _ = s.write_all(&body).await;
                let _ = s.shutdown().await;
            });
        }
    });
    port
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn link_preview_respects_ssrf_guard() {
    let cl = Cluster::new();
    let a = cl.node_with("a", private_previews).await;
    let b = cl.node_with("b", any_port_previews).await;
    let space = befriend(&a, &b).await;
    let channel = a.first_channel(space).await;
    let hits = Arc::new(AtomicUsize::new(0));
    let image = png(3_000, 8);
    let big = noisy_png(600, 400);
    assert!(big.len() > 64 * 1024);
    let port = preview_server(hits.clone(), image.clone(), big).await;
    let link = format!("http://127.0.0.1:{port}/page");

    // 시험용으로 사설 주소를 허용한 A는 미리보기를 만듭니다.
    let id = a.send(space, channel, &format!("이것 보세요 {link}")).await;
    let mine = a
        .messages(space, channel)
        .await
        .into_iter()
        .find(|v| v.id == id)
        .unwrap();
    assert_eq!(mine.embeds.len(), 1, "{mine:?}");
    let e = &mine.embeds[0];
    assert_eq!(e.url, link);
    assert_eq!(e.title, "양파 미리보기");
    assert_eq!(e.description, "설명 & 요약");
    let thumb = base64::engine::general_purpose::STANDARD
        .decode(e.thumbnail_base64.as_deref().expect("썸네일"))
        .unwrap();
    assert_eq!(thumb, image);
    assert!(hits.load(Ordering::SeqCst) >= 2, "페이지와 이미지를 조회함");
    // 썸네일 한도(64KiB)보다 큰 그림은 줄인 JPEG로 싣습니다.
    let big_id = a
        .send(space, channel, &format!("http://127.0.0.1:{port}/big"))
        .await;
    let view = a
        .messages(space, channel)
        .await
        .into_iter()
        .find(|v| v.id == big_id)
        .unwrap();
    let small = base64::engine::general_purpose::STANDARD
        .decode(
            view.embeds[0]
                .thumbnail_base64
                .as_deref()
                .expect("줄인 썸네일"),
        )
        .unwrap();
    assert!(small.len() <= 64 * 1024);
    let shrunk = image::load_from_memory_with_format(&small, image::ImageFormat::Jpeg).unwrap();
    assert!(shrunk.width() <= 320 && shrunk.height() <= 320);
    // 받는 쪽은 사이트에 접속하지 않고 미리보기를 봅니다.
    let before = hits.load(Ordering::SeqCst);
    wait_message(
        &b,
        space,
        channel,
        id,
        "B가 미리보기와 함께 받음",
        |v| v.embeds.first().is_some_and(|e| e.title == "양파 미리보기"),
    )
    .await;

    // 포트 제한만 끈 B는 루프백 주소를 조회하지 않습니다(포트가 아니라 SSRF 주소 검사가 거절).
    let id2 = b.send(space, channel, &format!("나도 {link}")).await;
    let theirs = b
        .messages(space, channel)
        .await
        .into_iter()
        .find(|v| v.id == id2)
        .unwrap();
    assert!(theirs.embeds.is_empty(), "{theirs:?}");
    assert_eq!(
        hits.load(Ordering::SeqCst),
        before,
        "B는 서버에 접속하지 않음"
    );
    // 꺾쇠로 감싼 링크는 미리보기를 만들지 않습니다.
    let id3 = a.send(space, channel, &format!("<{link}>")).await;
    let quiet = a
        .messages(space, channel)
        .await
        .into_iter()
        .find(|v| v.id == id3)
        .unwrap();
    assert!(quiet.embeds.is_empty());
    assert_eq!(hits.load(Ordering::SeqCst), before);
}

// ───────────── 9. 휴면 공간 ─────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dormant_crew_is_skipped_until_opened() {
    let cl = Cluster::new();
    let a = cl.node_with("a", quick_dormancy).await;
    let b = cl.node_with("b", quick_dormancy).await;
    // 1:1 스페이스(친구 DM)와 A가 만든(A가 확정 기기인) 크루.
    let dm = befriend(&a, &b).await;
    let dm_channel = a.first_channel(dm).await;
    let (crew, channel) = testkit::blobs::create_group_space(&a.node, &[&b.node])
        .await
        .expect("크루 공간");
    testkit::authenticate(&b.node, a.peer())
        .await
        .expect("인증");
    // A는 크루 채널을 열어 둡니다(기본 시더 범위는 '본인이 연 채널만').
    a.messages(crew, channel).await;

    eventually("B의 크루가 휴면이 됨", || async {
        testkit::blobs::is_dormant(&b.node, &crew).then_some(())
    })
    .await;
    // A가 전파 없이 씁니다. 휴면인 B의 크루는 가져오지 않고, 1:1 스페이스는 열지 않아도 주기 따라잡기로 가져옵니다.
    let evs = testkit::send_unpropagated(&a.node, crew, channel, "잠든 사이에")
        .await
        .unwrap();
    let id = evs.last().unwrap().id();
    let evs = testkit::send_unpropagated(&a.node, dm, dm_channel, "DM은 놓치지 않음")
        .await
        .unwrap();
    let dm_id = evs.last().unwrap().id();
    eventually("B가 DM을 열지 않고도 따라잡음", || async {
        testkit::get_event(&b.node, &dm_id)
            .unwrap()
            .is_some()
            .then_some(())
    })
    .await;
    let skips = testkit::blobs::dormant_skips(&b.node);
    eventually(
        "주기 작업이 휴면 공간을 여러 번 건너뜀",
        || async { (testkit::blobs::dormant_skips(&b.node) >= skips + 3).then_some(()) },
    )
    .await;
    assert!(
        testkit::get_event(&b.node, &id).unwrap().is_none(),
        "휴면 중에는 따라잡지 않음"
    );
    assert!(testkit::blobs::is_dormant(&b.node, &crew));
    // 그동안 확정 기기(A)의 크루와 양쪽의 1:1 스페이스는 휴면이 되지 않았습니다.
    assert!(
        !testkit::blobs::is_dormant(&a.node, &crew),
        "확정 기기의 크루"
    );
    assert!(
        !testkit::blobs::is_dormant(&a.node, &dm) && !testkit::blobs::is_dormant(&b.node, &dm),
        "1:1 스페이스"
    );
    assert_eq!(
        testkit::blobs::dormant_skips(&a.node),
        0,
        "A는 건너뛴 공간이 없음"
    );

    // 열면(메시지 목록) 깨어나 따라잡습니다.
    wait_message(&b, crew, channel, id, "연 뒤 따라잡음", |v| {
        v.text == "잠든 사이에"
    })
    .await;
    assert!(!testkit::blobs::is_dormant(&b.node, &crew));
    let cov = b.ok::<m::SeedCoverage>(SpaceParams { space: crew }).await;
    assert_eq!(cov.len(), 1);
    assert!(cov[0].online_seeders >= 1, "{cov:?}");

    // 다시 휴면이 된 크루도, 열지 않은 채널까지 제공하도록 시더 범위를 정하면(상시 시더) 열지 않아도 깨어납니다.
    eventually("B의 크루가 다시 휴면이 됨", || async {
        testkit::blobs::is_dormant(&b.node, &crew).then_some(())
    })
    .await;
    let evs = testkit::send_unpropagated(&a.node, crew, channel, "시더는 깨어 있음")
        .await
        .unwrap();
    let id2 = evs.last().unwrap().id();
    b.ok::<m::SeedSet>(SeedSetParams {
        space: Some(crew),
        all_channels: Some(true),
        ..Default::default()
    })
    .await;
    eventually("상시 시더의 크루가 깨어나 따라잡음", || async {
        (!testkit::blobs::is_dormant(&b.node, &crew)
            && testkit::get_event(&b.node, &id2).unwrap().is_some())
        .then_some(())
    })
    .await;
}

/// 휴면 공간에는 바뀐 프로필을 내지 않지만 건너뛴 채로 끝나지 않습니다. 깨어나면 한 번만 냅니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn profile_changed_while_dormant_goes_out_once_after_waking() {
    let cl = Cluster::new();
    let a = cl.node_with("a", quick_dormancy).await;
    let mut b = cl.node_with("b", quick_dormancy).await;
    let c = cl.node_with("c", quick_dormancy).await;
    let (crew, channel) = testkit::blobs::create_group_space(&a.node, &[&b.node, &c.node])
        .await
        .expect("크루 공간");
    connect_both(&a, &b).await;
    testkit::authenticate(&b.node, a.peer())
        .await
        .expect("인증");
    eventually("B의 크루가 휴면이 됨", || async {
        testkit::blobs::is_dormant(&b.node, &crew).then_some(())
    })
    .await;

    let carried = |n: &TestNode, user| {
        testkit::stream_events(&n.node, &crew, StreamId::Space)
            .expect("이벤트")
            .iter()
            .filter(|e| e.header.kind == EventKind::MemberProfile && e.header.user == user)
            .count()
    };
    let user = b.user();
    let before = carried(&b, user);
    b.ok::<m::ProfileSet>(ProfileSetParams {
        name: Some("잠든 사이에 바꿈".into()),
        ..Default::default()
    })
    .await;
    testkit::spread_cards(&b.node).await;
    assert_eq!(carried(&b, user), before, "휴면 공간에는 내지 않음");

    b.messages(crew, channel).await;
    assert!(!testkit::blobs::is_dormant(&b.node, &crew));
    testkit::spread_cards(&b.node).await;
    eventually(
        "깨어난 뒤 A가 B의 프로필 이벤트를 받음",
        || async { (carried(&a, user) == before + 1).then_some(()) },
    )
    .await;

    b.restart().await;
    b.messages(crew, channel).await;
    testkit::spread_cards(&b.node).await;
    assert_eq!(carried(&b, user), before + 1, "실린 판을 다시 내면 안 됨");
}

// ───────────── 10. 남의 블롭 ID를 베낀 메시지·프로필 ─────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn copied_blob_ids_are_not_served() {
    let cl = Cluster::new();
    let a = cl.node_with("a", small_chunks).await;
    let b = cl.node("b").await;
    let c = cl.node("c").await;
    // A–C(S1)에서 보낸 첨부를, S1 멤버가 아닌 B가 A–B(S2)로 받으려 합니다.
    let s1 = befriend(&a, &c).await;
    let s2 = befriend(&a, &b).await;
    let ch1 = a.first_channel(s1).await;
    let ch2 = a.first_channel(s2).await;
    let file = sample(60_000, 10);
    let path = write_file(&a, "비밀.bin", &file);
    let (orig, blob) = send_file(&a, s1, ch1, &path).await;
    let real_key = testkit::blobs::attachment_key(&a.node, &orig, &blob).expect("첨부 키");
    for req in blob_asks(s2, blob) {
        let r = net_request(&b, &a, req).await;
        assert!(refused_with(&r, WireErrorCode::NotFound), "{r:?}");
    }

    // B가 S2에 그 블롭 ID를 실은 메시지를 보냅니다: 틀린 키, 그리고 (빼돌렸다고 치고) 진짜 키.
    // 블롭은 A가 S1에서 올린 것이라 S2로는 내주지 않고, 있는지도 알리지 않습니다.
    for key in [Secret32::random(), real_key.clone()] {
        let id = testkit::blobs::send_attachment(
            &b.node,
            s2,
            ch2,
            raw_attachment(blob, key, file.len() as u64),
        )
        .await
        .expect("베낀 첨부 보내기");
        wait_message(&a, s2, ch2, id, "A가 B의 메시지를 받음", |_| true).await;
        for req in blob_asks(s2, blob) {
            let r = net_request(&b, &a, req).await;
            assert!(refused_with(&r, WireErrorCode::NotFound), "{r:?}");
        }
    }
    // 그 블롭을 자기 아바타라고 주장해도 A가 프로필 이미지로 받은 블롭이 아니라 내주지 않습니다.
    testkit::blobs::claim_avatar(&a.node, &b.node, blob, Some(real_key.clone()))
        .expect("아바타 주장");
    for req in blob_asks(s2, blob) {
        let r = net_request(&b, &a, req).await;
        assert!(refused_with(&r, WireErrorCode::NotFound), "{r:?}");
    }

    // 같은 공간 안: A가 원래 메시지를 제공 범위에서 빼면, C가 키를 모른 채 같은 블롭 ID를 실은 새 메시지로
    // 그 제외를 돌아가지 못합니다(키가 맞지 않는 기록은 남기지도 않음).
    a.ok::<m::SeedExcludeMessage>(SeedExcludeParams {
        space: s1,
        channel: ch1,
        id: orig,
        excluded: true,
    })
    .await;
    let forged = testkit::blobs::send_attachment(
        &c.node,
        s1,
        ch1,
        raw_attachment(blob, Secret32::random(), file.len() as u64),
    )
    .await
    .expect("베낀 첨부 보내기");
    wait_message(&a, s1, ch1, forged, "A가 C의 메시지를 받음", |_| {
        true
    })
    .await;
    let refs = testkit::blobs::verified_refs(&a.node, &blob);
    assert!(refs.contains(&(s1, orig)), "{refs:?}");
    assert!(!refs.iter().any(|(_, e)| *e == forged), "{refs:?}");
    for req in blob_asks(s1, blob) {
        let r = net_request(&c, &a, req).await;
        assert!(refused_with(&r, WireErrorCode::NotSeeded), "{r:?}");
    }
}

// ───────────── 11. 정리가 남의 메시지로 내 블롭을 지우지 않음 ─────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deleted_copies_do_not_remove_avatar_or_emoji() {
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let b = cl.node("b").await;
    let image = png(20_000, 11);
    let path = write_file(&a, "아바타.png", &image);
    let avatar = a
        .ok::<m::ProfileSet>(ProfileSetParams {
            avatar_path: Some(path),
            ..Default::default()
        })
        .await
        .avatar
        .expect("아바타 블롭");
    let space = befriend(&a, &b).await;
    let channel = a.first_channel(space).await;
    let (emoji, _) = testkit::blobs::add_emoji(&a.node, space, "양파", &png(5_000, 12))
        .await
        .expect("이모지 추가");
    let emoji_key = eventually("B가 이모지를 앎", || async {
        testkit::space_state(&b.node, &space)?
            .emojis
            .values()
            .find(|e| e.blob == emoji)
            .map(|e| e.key.clone())
    })
    .await;
    // 친구인 B는 A의 서명된 프로필에서 아바타 키를 압니다(키까지 맞는 베낀 첨부).
    let avatar_key =
        testkit::blobs::known_image_key(&b.node, &a.user(), &avatar).expect("아바타 키");
    let mut copies = Vec::new();
    for (blob, key) in [(avatar, avatar_key), (emoji, emoji_key)] {
        let id =
            testkit::blobs::send_attachment(&b.node, space, channel, raw_attachment(blob, key, 1))
                .await
                .expect("베낀 첨부 보내기");
        copies.push(id);
    }
    for id in &copies {
        wait_message(&a, space, channel, *id, "A가 B의 메시지를 받음", |_| true).await;
        b.ok::<m::MessageDelete>(MessageRefParams {
            space,
            channel,
            id: *id,
        })
        .await;
    }
    eventually("A가 B의 삭제를 받음", || async {
        copies
            .iter()
            .all(|id| testkit::blobs::is_deleted(&a.node, id))
            .then_some(())
    })
    .await;
    let before = (
        testkit::blobs::chunk_progress(&a.node, &avatar),
        testkit::blobs::chunk_progress(&a.node, &emoji),
    );
    assert!(before.0.is_some() && before.1.is_some(), "{before:?}");
    testkit::blobs::cleanup_now(&a.node);
    assert_eq!(
        (
            testkit::blobs::chunk_progress(&a.node, &avatar),
            testkit::blobs::chunk_progress(&a.node, &emoji),
        ),
        before,
        "아바타·이모지는 남아야 함"
    );

    // 내 첨부는 그 메시지를 지우면 정리됩니다.
    let path = write_file(&a, "지울 파일.bin", &sample(20_000, 13));
    let (id, blob) = send_file(&a, space, channel, &path).await;
    assert!(testkit::blobs::chunk_progress(&a.node, &blob).is_some());
    a.ok::<m::MessageDelete>(MessageRefParams { space, channel, id })
        .await;
    testkit::blobs::cleanup_now(&a.node);
    assert_eq!(testkit::blobs::chunk_progress(&a.node, &blob), None);
}

// ───────────── 12. 자동 내려받기 중의 직접 요청 ─────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manual_fetch_during_automatic_attempt_is_kept() {
    let cl = Cluster::new();
    let a = cl.node_with("a", small_chunks).await;
    let b = cl.node("b").await;
    let space = befriend(&a, &b).await;
    let channel = a.first_channel(space).await;
    // A는 처음엔 제공하지 않아, B의 자동 내려받기가 매니페스트를 기다리며 돌게 합니다.
    a.ok::<m::SeedSet>(SeedSetParams {
        enabled: Some(false),
        ..Default::default()
    })
    .await;
    let file = sample(150_000, 14);
    let (blob, key) = testkit::blobs::import_blob(&a.node, space, &file).expect("블롭 저장");
    // 적힌 크기(1000바이트)보다 훨씬 큰 블롭: 자동 내려받기는 매니페스트를 받으면 포기합니다.
    let id =
        testkit::blobs::send_attachment(&a.node, space, channel, raw_attachment(blob, key, 1_000))
            .await
            .expect("첨부 보내기");
    eventually("B의 자동 내려받기가 돎", || async {
        testkit::blobs::download_active(&b.node, &blob).then_some(())
    })
    .await;
    // 그 사이 사용자가 직접 요청합니다. 자동 시도가 포기해도 직접 요청은 남아 끝까지 받아야 합니다.
    b.ok::<m::BlobFetch>(BlobFetchParams {
        space,
        blob,
        priority: Some(200),
    })
    .await;
    a.ok::<m::SeedSet>(SeedSetParams {
        enabled: Some(true),
        ..Default::default()
    })
    .await;
    wait_complete(&b, space, blob, "직접 요청으로 받음").await;
    assert_eq!(read_blob(&b, space, blob, id).await, file);
}

// ───────────── 13. 썸네일·재생 길이 ─────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attachments_carry_thumbnail_and_duration() {
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let b = cl.node("b").await;
    let space = befriend(&a, &b).await;
    let channel = a.first_channel(space).await;
    let photo = noisy_png(600, 400);
    assert!(photo.len() > 64 * 1024, "원본을 그대로 실을 수 없는 크기");
    let photo_path = write_file(&a, "photo.png", &photo);
    let video_path = write_file(&a, "clip.mp4", &tiny_mp4(12_500));
    let id = a
        .ok::<m::MessageSend>(MessageSendParams {
            space,
            channel,
            text: "사진과 영상".into(),
            attachments: vec![
                AttachmentInput {
                    path: photo_path,
                    ..Default::default()
                },
                AttachmentInput {
                    path: video_path,
                    ..Default::default()
                },
            ],
            reply_to: None,
            thread: None,
        })
        .await
        .id;
    // 받는 쪽은 원본 없이 썸네일·크기·길이를 봅니다.
    let view = wait_message(
        &b,
        space,
        channel,
        id,
        "B가 첨부 정보를 받음",
        |v| v.attachments.len() == 2,
    )
    .await;
    let (img, vid) = (&view.attachments[0], &view.attachments[1]);
    assert_eq!((img.width, img.height), (Some(600), Some(400)));
    let thumb = base64::engine::general_purpose::STANDARD
        .decode(img.thumbnail_base64.as_deref().expect("썸네일"))
        .unwrap();
    assert!(thumb.len() <= 64 * 1024);
    let t = image::load_from_memory_with_format(&thumb, image::ImageFormat::Jpeg).unwrap();
    assert_eq!((t.width(), t.height()), (320, 213));
    assert_eq!(vid.mime, "video/mp4");
    assert_eq!(vid.duration_ms, Some(12_500));
    assert!(vid.thumbnail_base64.is_none());
}

/// 클립보드에서 붙여넣은 이미지(경로 없음)를 조각으로 올려 보내면 받는 쪽은 파일 첨부와 똑같이 받습니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pasted_bytes_send_like_a_file() {
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let b = cl.node("b").await;
    let space = befriend(&a, &b).await;
    let channel = a.first_channel(space).await;
    let photo = noisy_png(300, 200);
    let b64 = |b: &[u8]| base64::engine::general_purpose::STANDARD.encode(b);
    let (head, tail) = photo.split_at(photo.len() / 2);
    let first = a
        .ok::<m::BlobPaste>(BlobPasteParams {
            paste: None,
            size: Some(photo.len() as u64),
            data_base64: b64(head),
        })
        .await;
    // 다 올리기 전에는 보낼 수 없습니다.
    let send = |paste: String| MessageSendParams {
        space,
        channel,
        text: "붙여넣기".into(),
        attachments: vec![AttachmentInput {
            paste: Some(paste),
            name: Some("image.png".into()),
            ..Default::default()
        }],
        reply_to: None,
        thread: None,
    };
    let e = a
        .call::<m::MessageSend>(send(first.paste.clone()))
        .await
        .unwrap_err();
    assert_eq!(e.code, ERR_INVALID_INPUT);
    let done = a
        .ok::<m::BlobPaste>(BlobPasteParams {
            paste: Some(first.paste.clone()),
            size: None,
            data_base64: b64(tail),
        })
        .await;
    assert_eq!(done.received, photo.len() as u64);
    let id = a.ok::<m::MessageSend>(send(first.paste.clone())).await.id;
    let view = wait_message(
        &b,
        space,
        channel,
        id,
        "B가 붙여넣은 첨부를 받음",
        |v| v.attachments.len() == 1,
    )
    .await;
    let att = &view.attachments[0];
    assert_eq!(
        (att.name.as_str(), att.mime.as_str()),
        ("image.png", "image/png")
    );
    assert_eq!((att.width, att.height), (Some(300), Some(200)));
    b.ok::<m::BlobFetch>(BlobFetchParams {
        space,
        blob: att.blob,
        priority: None,
    })
    .await;
    wait_complete(&b, space, att.blob, "B가 다 받음").await;
    assert_eq!(read_blob(&b, space, att.blob, id).await, photo);
    // 지운 뒤에는 다시 쓸 수 없습니다.
    a.ok::<m::BlobPasteDiscard>(BlobPasteDiscardParams {
        paste: first.paste.clone(),
    })
    .await;
    let e = a
        .call::<m::MessageSend>(send(first.paste))
        .await
        .unwrap_err();
    assert_eq!(e.code, yangpa_rpc::jsonrpc::ERR_NOT_FOUND);
}

// ───────────── 14. 범위별 속도 제한·전송 속도 ─────────────

/// 받는 쪽의 사람별 받기 제한과 보내는 쪽의 채널별 올리기 제한이 첨부 속도를 묶고, 받는 동안 보내는 쪽
/// `transfer.rates`에 받는 사람과 속도가 보입니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn scoped_rate_limits_throttle_and_rates_show_downloader() {
    const RATE: u64 = 32 * 1024;
    // 버스트(1초 분량)를 빼고 4초쯤 걸리는 크기.
    const SIZE: usize = 160 * 1024;
    let cl = Cluster::new();
    let a = cl.node_with("a", small_chunks).await;
    let b = cl.node_with("b", small_chunks).await;
    let space = befriend(&a, &b).await;
    let channel = a.first_channel(space).await;
    a.messages(space, channel).await;
    b.messages(space, channel).await;

    let from_a = RateScope::User { user: a.user() };
    let limit = |scope, up_bps, down_bps| RateLimitParams {
        scope,
        up_bps,
        down_bps,
    };
    // 0은 받지 않음으로 저장됩니다.
    b.ok::<m::RateSetLimit>(limit(from_a, None, Some(0))).await;
    assert_eq!(
        b.ok::<m::RateLimits>(Empty).await,
        vec![RateLimitView {
            scope: from_a,
            up_bps: None,
            down_bps: Some(0),
        }]
    );
    b.ok::<m::RateSetLimit>(limit(from_a, None, Some(RATE)))
        .await;
    assert_eq!(
        b.ok::<m::RateLimits>(Empty).await,
        vec![RateLimitView {
            scope: from_a,
            up_bps: None,
            down_bps: Some(RATE),
        }]
    );

    let started = Instant::now();
    let path = write_file(&a, "느린 받기.bin", &sample(SIZE, 7));
    let (id, blob) = send_file(&a, space, channel, &path).await;
    eventually("A의 전송 속도에 B가 보임", || async {
        let r = a.ok::<m::TransferRatesGet>(Empty).await;
        (r.up_bps > 0
            && r.uploads
                .iter()
                .any(|u| u.space == space && u.blob == blob && u.user == b.user() && u.bps > 0))
        .then_some(())
    })
    .await;
    wait_message(&b, space, channel, id, "B가 첨부 메시지를 받음", |_| true).await;
    wait_complete(&b, space, blob, "B가 제한 속도로 다 받음").await;
    let took = started.elapsed();
    assert!(
        took >= Duration::from_millis(2_500),
        "받기 제한이 걸리지 않음: {took:?}"
    );
    eventually("B의 받기 속도", || async {
        (b.ok::<m::TransferRatesGet>(Empty).await.down_bps > 0).then_some(())
    })
    .await;
    assert_eq!(read_blob(&b, space, blob, id).await.len(), SIZE);

    // 지우면 목록이 빕니다. 이번엔 A가 이 채널의 올리기를 제한합니다.
    b.ok::<m::RateSetLimit>(limit(from_a, None, None)).await;
    assert!(b.ok::<m::RateLimits>(Empty).await.is_empty());
    a.ok::<m::RateSetLimit>(limit(
        RateScope::Channel { space, channel },
        Some(RATE),
        None,
    ))
    .await;
    let started = Instant::now();
    let path = write_file(&a, "느린 올리기.bin", &sample(SIZE, 8));
    let (id, blob) = send_file(&a, space, channel, &path).await;
    // 메시지가 오기 전에는 B가 그 블롭을 몰라 상태를 물을 수 없습니다.
    wait_message(
        &b,
        space,
        channel,
        id,
        "B가 두 번째 첨부 메시지를 받음",
        |_| true,
    )
    .await;
    wait_complete(&b, space, blob, "B가 A의 올리기 제한 속도로 다 받음").await;
    let took = started.elapsed();
    assert!(
        took >= Duration::from_millis(2_500),
        "올리기 제한이 걸리지 않음: {took:?}"
    );

    // B가 이 채널의 받기를 0으로 막으면 메시지는 오지만 첨부는 자동으로도 직접으로도 받지 않습니다.
    b.ok::<m::RateSetLimit>(limit(RateScope::Channel { space, channel }, None, Some(0)))
        .await;
    let path = write_file(&a, "안 받기.bin", &sample(SIZE, 9));
    let (id, blob) = send_file(&a, space, channel, &path).await;
    wait_message(
        &b,
        space,
        channel,
        id,
        "B가 세 번째 첨부 메시지를 받음",
        |_| true,
    )
    .await;
    let e = b
        .call::<m::BlobFetch>(BlobFetchParams {
            space,
            blob,
            priority: None,
        })
        .await
        .expect_err("받기 0이면 직접 받기도 거절");
    assert_eq!(e.code, ERR_FORBIDDEN, "{}", e.message);
}
