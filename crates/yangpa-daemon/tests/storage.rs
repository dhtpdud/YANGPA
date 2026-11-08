//! 저장 공간·미디어 시험(X4): 받은 첨부의 저장 공간 정리(LRU·고정·내가 올린 것·올리는 중인 것·다시 받기),
//! 자동화 토큰의 한도 줄이기 확인, 헤드리스 시더의 미리 받기, 앱이 넘긴 동영상·음성 썸네일·길이 검사(형식이 맞지
//! 않으면 힌트만 버림), 확정 기기 후보 목록, 비운 첨부를 같은 파일로 다시 잇기, 라이브러리 탭 목록과 이 기기에서 지우기.
//!
//! 고정 sleep 없이 조건이 될 때까지 기다립니다(`common::eventually`). 정리 순서는 마지막 접근 시각을 직접
//! 정하고, "방금 읽어 내주는 중"도 시험 고리로 붙잡아 두어 시계에 기대지 않습니다.

use crate::common;

use base64::Engine;
use common::{Cluster, TestNode, befriend, connect_both, eventually, wait_message};
use yangpa_core::ids::{BlobId, ChannelId, EventId, SpaceId};
use yangpa_core::wire::{YpRequest, YpResponse};
use yangpa_daemon::{ApiHandler, CallCtx, testkit};
use yangpa_rpc::api::{
    ANONYMOUS_USER, AttachmentInput, BlobCacheLimitParams, BlobCacheView, BlobFetchParams,
    BlobParams, BlobReadParams, BlobRelinkParams, BlobStatusView, ChannelUpdateParams, Empty,
    IdParams, LibraryListParams, LibraryPage, MemberSetRolesParams, MessageSendParams, Method,
    OptSpaceParams, RoleCreateParams, Scope, SeedExcludeParams, SeedSetParams, SpaceParams,
    TokenCreateParams, m,
};
use yangpa_rpc::jsonrpc::{ERR_CONFIRMATION_REQUIRED, ERR_INVALID_INPUT, ERR_NOT_FOUND};
use yangpa_rpc::{RpcError, RpcHandler};

// ───────────── 도우미 ─────────────

/// 내용 확인용 결정적 바이트.
fn sample(len: usize, seed: u8) -> Vec<u8> {
    common::noise(len, seed)
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
    v.extend(bx(b"mdat", &sample(20_000, 3)));
    v.extend(bx(b"moov", &bx(b"mvhd", &mvhd)));
    v
}

/// 실제로 풀리는 PNG.
fn png(w: u32, h: u32) -> Vec<u8> {
    let img = image::RgbImage::from_fn(w, h, |x, y| {
        image::Rgb([(x % 256) as u8, (y % 256) as u8, 90])
    });
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(img)
        .write_to(&mut out, image::ImageFormat::Png)
        .unwrap();
    out.into_inner()
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn write_file(n: &TestNode, name: &str, bytes: &[u8]) -> String {
    let dir = n.dir.join("files");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, bytes).unwrap();
    path.to_string_lossy().into_owned()
}

fn send_params(
    space: SpaceId,
    channel: ChannelId,
    attachments: Vec<AttachmentInput>,
) -> MessageSendParams {
    MessageSendParams {
        space,
        channel,
        text: "파일".into(),
        attachments,
        reply_to: None,
        thread: None,
    }
}

/// 파일 하나를 첨부해 보냅니다. (메시지, 블롭). 보낸 쪽의 메시지 목록을 보므로 그 채널을 연 것이 됩니다.
async fn send_file(
    n: &TestNode,
    space: SpaceId,
    channel: ChannelId,
    name: &str,
    bytes: &[u8],
) -> (EventId, BlobId) {
    let path = write_file(n, name, bytes);
    let id = n
        .ok::<m::MessageSend>(send_params(
            space,
            channel,
            vec![AttachmentInput {
                path,
                ..Default::default()
            }],
        ))
        .await
        .id;
    let view = n
        .messages(space, channel)
        .await
        .into_iter()
        .find(|v| v.id == id)
        .expect("보낸 메시지");
    (id, view.attachments[0].blob)
}

async fn status(n: &TestNode, space: SpaceId, blob: BlobId) -> BlobStatusView {
    n.ok::<m::BlobStatus>(BlobParams { space, blob }).await
}

/// 다 받을 때까지(받기 시작 전에는 그 공간의 블롭으로 등록되지 않아 `blob.status`가 실패할 수 있음).
async fn wait_complete(n: &TestNode, space: SpaceId, blob: BlobId, what: &str) {
    eventually(what, || async {
        let s = n
            .call::<m::BlobStatus>(BlobParams { space, blob })
            .await
            .ok()?;
        (s.total_chunks > 0 && s.have_chunks == s.total_chunks).then_some(())
    })
    .await;
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

fn stored(n: &TestNode, blob: &BlobId) -> u64 {
    testkit::blobs::stored_bytes(&n.node, blob).unwrap_or(0)
}

async fn set_limit(n: &TestNode, limit: u64) -> BlobCacheView {
    n.ok::<m::BlobCacheSetLimit>(BlobCacheLimitParams {
        limit_bytes: Some(limit),
        ..Default::default()
    })
    .await
}

/// 남(`from`)이 본 `to`의 보유 비트맵에 켜진 조각 수. 제공하지 않거나 없다고 답하면 `None`.
async fn have_bits(from: &TestNode, to: &TestNode, space: SpaceId, blob: BlobId) -> Option<u32> {
    match testkit::net_request(&from.node, to.peer(), YpRequest::BlobHave { space, blob })
        .await
        .expect("요청 전송")
    {
        YpResponse::Have { bitmap, .. } => Some(bitmap.iter().map(|b| b.count_ones()).sum()),
        _ => None,
    }
}

fn code<T: std::fmt::Debug>(r: Result<T, RpcError>) -> i64 {
    r.expect_err("오류여야 합니다").code
}

/// 소유자가 만든 자동화 토큰(AI)으로 인증한 호출 맥락.
async fn automation(n: &TestNode, scopes: Vec<Scope>) -> CallCtx {
    let r = n
        .ok::<m::TokenCreate>(TokenCreateParams {
            name: "정리 봇".into(),
            scopes,
            spaces: Vec::new(),
            automation: Some(true),
        })
        .await;
    let auth = ApiHandler::new(n.node.clone())
        .authenticate(r.token.expose())
        .await
        .expect("새 토큰으로 인증");
    CallCtx::from_auth(auth)
}

/// 이 맥락(토큰)으로 부릅니다.
async fn call_as<M: Method>(
    n: &TestNode,
    ctx: &CallCtx,
    p: M::Params,
) -> Result<M::Result, RpcError> {
    let v = serde_json::to_value(&p).expect("매개변수 직렬화");
    let out = n.node.call(ctx, M::NAME, v).await?;
    Ok(serde_json::from_value(out).expect("결과 형식"))
}

/// 사람 확인이 필요하다는 오류에서 확인 요청 ID를 꺼냅니다.
fn confirmation_id<T: std::fmt::Debug>(r: Result<T, RpcError>) -> String {
    let e = r.expect_err("사람 확인이 필요해야 함");
    assert_eq!(e.code, ERR_CONFIRMATION_REQUIRED, "{e:?}");
    e.data
        .as_ref()
        .and_then(|d| d.get("confirmation_id"))
        .and_then(|v| v.as_str())
        .expect("confirmation_id")
        .to_owned()
}

/// 두 노드 크루: 연결·인증하고 같은 크루 키 순번이 될 때까지. `b_opens`이면 B도 첫 채널을 엽니다.
async fn crew(cl: &Cluster, b_opens: bool) -> (TestNode, TestNode, SpaceId, ChannelId) {
    let a = cl.node("a").await;
    let b = cl.node("b").await;
    let (space, channel) = testkit::blobs::create_group_space(&a.node, &[&b.node])
        .await
        .expect("크루 공간");
    connect_both(&a, &b).await;
    testkit::authenticate(&a.node, b.peer())
        .await
        .expect("인증");
    eventually("같은 크루 키 순번", || async {
        let ea = testkit::crew_key_seq(&a.node, &space)?;
        let eb = testkit::crew_key_seq(&b.node, &space)?;
        (ea == eb).then_some(())
    })
    .await;
    a.messages(space, channel).await;
    if b_opens {
        b.messages(space, channel).await;
    }
    (a, b, space, channel)
}

// ───────────── 1. 저장 공간 정리 ─────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cache_evicts_oldest_and_keeps_pinned_and_own() {
    let cl = Cluster::new();
    let (a, b, space, channel) = crew(&cl, true).await;

    // A가 같은 크기의 파일 넷을 보내고 B는 자동으로 받습니다. B도 자기 파일을 하나 올립니다.
    let mut files = Vec::new();
    for i in 1..=4u8 {
        let bytes = sample(40_000, i);
        let (id, blob) = send_file(&a, space, channel, &format!("f{i}.bin"), &bytes).await;
        files.push((id, blob, bytes));
    }
    for (_, blob, _) in &files {
        wait_complete(&b, space, *blob, "B가 자동으로 받음").await;
    }
    let (_, own) = send_file(&b, space, channel, "내 파일.bin", &sample(40_000, 9)).await;
    let [f1, f2, f3, f4] = [0, 1, 2, 3].map(|i| files[i].1);
    let s = stored(&b, &f1);
    assert!(s >= 40_000, "조각 바이트: {s}");
    for f in [f2, f3, f4] {
        assert_eq!(stored(&b, &f), s, "같은 크기");
    }
    // 정리 순서를 정합니다(오래된 것부터 f1, f2, f3, f4). 내 파일은 가장 오래됐어도 남아야 합니다.
    for (blob, at) in [
        (own, 100),
        (f1, 1_000),
        (f2, 2_000),
        (f3, 3_000),
        (f4, 4_000),
    ] {
        assert!(testkit::blobs::set_last_access(&b.node, &blob, at).unwrap());
    }

    // f2를 고정합니다.
    b.ok::<m::BlobPin>(BlobParams { space, blob: f2 }).await;
    let pins = b.ok::<m::BlobPins>(OptSpaceParams { space: None }).await;
    assert_eq!(pins.len(), 1);
    assert_eq!((pins[0].blob, pins[0].space), (f2, space));
    assert!(pins[0].downloaded);
    let st = status(&b, space, f2).await;
    assert!(st.pinned && !st.kept, "{st:?}");
    let st = status(&b, space, own).await;
    assert!(st.kept && !st.pinned, "{st:?}");
    let v = b.ok::<m::BlobCacheStatus>(Empty).await;
    assert_eq!((v.cache_count, v.cache_bytes), (3, 3 * s), "{v:?}");
    assert_eq!((v.pinned_count, v.pinned_bytes), (1, s), "{v:?}");
    assert!(v.kept_count >= 1 && v.kept_bytes >= s, "{v:?}");
    assert_eq!(v.default_limit_bytes, v.limit_bytes);

    // 한도 안이면 비우지 않고, 넘으면 오래된 것부터 비웁니다(고정·내 파일은 그대로).
    let half = s / 2;
    let v = set_limit(&b, 3 * s + half).await;
    assert_eq!((v.cache_count, v.evicted_count), (3, 0), "{v:?}");
    let v = set_limit(&b, 2 * s + half).await;
    assert_eq!((v.cache_count, v.evicted_count), (2, 1), "{v:?}");
    assert_eq!(stored(&b, &f1), 0, "가장 오래된 f1을 비움");
    let v = set_limit(&b, s + half).await;
    assert_eq!((v.cache_count, v.evicted_count), (1, 2), "{v:?}");
    assert_eq!(stored(&b, &f3), 0, "고정한 f2는 건너뛰고 f3을 비움");
    assert_eq!((stored(&b, &f2), stored(&b, &f4)), (s, s));
    assert!(stored(&b, &own) > 0, "내가 올린 파일은 남김");
    assert_eq!(v.evicted_bytes, 2 * s);

    // 비운 블롭은 남에게 조각이 없다고 답합니다(빈 비트맵, 또는 키를 확인할 조각이 없어 '없는 블롭').
    assert_eq!(have_bits(&a, &b, space, f1).await.unwrap_or(0), 0);
    assert!(have_bits(&a, &b, space, f4).await.unwrap_or(0) > 0);
    let st = status(&b, space, f1).await;
    assert_eq!(st.have_chunks, 0, "{st:?}");

    // 새 첨부를 자동으로 받을 때 자리를 만듭니다(f4를 비움).
    let (_, f5) = send_file(&a, space, channel, "f5.bin", &sample(40_000, 5)).await;
    wait_complete(&b, space, f5, "B가 자리를 만들고 받음").await;
    assert_eq!(stored(&b, &f4), 0, "f4를 비우고 f5를 받음");
    assert_eq!(stored(&b, &f2), s, "고정은 그대로");

    // 비운 첨부는 다시 받을 수 있습니다.
    set_limit(&b, 100 * s).await;
    b.ok::<m::BlobFetch>(BlobFetchParams {
        space,
        blob: f1,
        priority: Some(200),
    })
    .await;
    wait_complete(&b, space, f1, "비운 f1을 다시 받음").await;
    assert_eq!(read_blob(&b, space, f1, files[0].0).await, files[0].2);
    // 읽으면 잠시(30초) 내주는 중으로 기록합니다. 느린 기계에서도 30초가 지나 버리지 않게 시험 동안 붙잡아 둡니다.
    assert!(testkit::blobs::serving_recorded(&b.node, &f1), "읽은 기록");
    testkit::blobs::hold_serving(&b.node, &f1, true);

    // 모두 비우기: 방금 읽은 f1(재생·내주는 중으로 봄), 고정, 내 파일은 남깁니다.
    let r = b.ok::<m::BlobCacheClear>(Empty).await;
    assert_eq!((r.evicted_count, r.freed_bytes), (1, s), "{r:?}");
    assert_eq!(stored(&b, &f5), 0);
    assert_eq!(stored(&b, &f1), s, "방금 읽은 블롭은 남김");
    assert_eq!(stored(&b, &f2), s);
    assert!(stored(&b, &own) > 0);

    // 고정을 풀면 정리 대상이 됩니다. 고정하지 않은 블롭은 풀 수 없습니다.
    b.ok::<m::BlobUnpin>(BlobParams { space, blob: f2 }).await;
    assert!(
        b.ok::<m::BlobPins>(OptSpaceParams { space: None })
            .await
            .is_empty()
    );
    assert_eq!(
        code(b.call::<m::BlobUnpin>(BlobParams { space, blob: f2 }).await),
        ERR_NOT_FOUND
    );
    let r = b.ok::<m::BlobCacheClear>(Empty).await;
    assert_eq!(r.evicted_count, 1, "{r:?}");
    assert_eq!(stored(&b, &f2), 0);
    assert_eq!(stored(&b, &f1), s, "아직 내주는 중");
    assert!(stored(&b, &own) > 0, "내가 올린 파일은 끝까지 남김");
    // 내주기가 끝나면 비울 수 있습니다.
    testkit::blobs::hold_serving(&b.node, &f1, false);
    let r = b.ok::<m::BlobCacheClear>(Empty).await;
    assert_eq!((r.evicted_count, r.freed_bytes), (1, s), "{r:?}");
    assert_eq!(stored(&b, &f1), 0);
    assert!(stored(&b, &own) > 0, "내가 올린 파일은 끝까지 남김");

    // 정리가 목록을 읽은 뒤에 고정해도(정리와 고정이 겹침) 고정한 첨부는 비우지 않습니다.
    b.ok::<m::BlobFetch>(BlobFetchParams {
        space,
        blob: f3,
        priority: Some(200),
    })
    .await;
    wait_complete(&b, space, f3, "비운 f3을 다시 받음").await;
    eventually("받기 작업이 끝남", || async {
        (!testkit::blobs::download_active(&b.node, &f3)).then_some(())
    })
    .await;
    assert_eq!(
        testkit::blobs::clear_pinning_midway(&b.node, space, f3),
        (0, 0)
    );
    assert_eq!(stored(&b, &f3), s, "고정한 f3은 남김");
    assert!(status(&b, space, f3).await.pinned);
    b.ok::<m::BlobUnpin>(BlobParams { space, blob: f3 }).await;
    let r = b.ok::<m::BlobCacheClear>(Empty).await;
    assert_eq!(
        (r.evicted_count, r.freed_bytes),
        (1, s),
        "고정을 풀면 비움: {r:?}"
    );
    // 모르는 블롭은 고정할 수 없습니다.
    assert_eq!(
        code(
            b.call::<m::BlobPin>(BlobParams {
                space,
                blob: BlobId([7; 32]),
            })
            .await
        ),
        ERR_NOT_FOUND
    );

    // 한도를 기본값으로 되돌립니다.
    let v = b
        .ok::<m::BlobCacheSetLimit>(BlobCacheLimitParams {
            reset: true,
            ..Default::default()
        })
        .await;
    assert_eq!(v.limit_bytes, v.default_limit_bytes);

    // 확정 기기 후보: 소유자 기기(지금 확정 기기)와 관리자로 올린 B의 기기.
    let (da, db) = (
        testkit::device_id(&a.node).unwrap(),
        testkit::device_id(&b.node).unwrap(),
    );
    let c = a
        .ok::<m::CrewCommitterCandidates>(SpaceParams { space })
        .await;
    assert_eq!(c.len(), 1, "{c:?}");
    assert!(c[0].device == da && c[0].current && c[0].mine && c[0].online);
    let admin = a
        .ok::<m::RoleCreate>(RoleCreateParams {
            space,
            name: "관리".into(),
            color: None,
            permissions: vec!["administrator".into()],
            rank: Some(10),
        })
        .await
        .role;
    a.ok::<m::MemberSetRoles>(MemberSetRolesParams {
        space,
        user: b.user(),
        roles: vec![admin],
    })
    .await;
    let c = a
        .ok::<m::CrewCommitterCandidates>(SpaceParams { space })
        .await;
    assert_eq!(
        c.iter().map(|x| x.device).collect::<Vec<_>>(),
        vec![da, db],
        "지금 확정 기기가 먼저"
    );
    assert!(!c[1].current && !c[1].mine && c[1].online, "{c:?}");
    let seen_by_b = eventually("B도 후보 둘을 봄", || async {
        let c = b
            .ok::<m::CrewCommitterCandidates>(SpaceParams { space })
            .await;
        (c.len() == 2).then_some(c)
    })
    .await;
    assert!(seen_by_b[0].current && !seen_by_b[0].mine);
    assert!(seen_by_b[1].mine && seen_by_b[1].device == db);
}

/// 올리는 중인(아직 메시지가 없는) 첨부는 암호화하는 동안에도 정리 대상이 아닙니다. 첫 조각을 쓴 뒤 멈춰 두고
/// 모두 비우기·디스크 부족 정리를 돌려도 조각이 남아, 보내기가 끝나면 받는 쪽이 원본을 그대로 받습니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn upload_in_progress_is_never_evicted() {
    let cl = Cluster::new();
    let (a, b, space, channel) = crew(&cl, true).await;
    // 조각 셋(256KiB 조각).
    let bytes = sample(600 * 1024, 7);
    let path = write_file(&a, "큰 파일.bin", &bytes);
    let pause = testkit::blobs::pause_uploads(&a.node);
    let send = a.call::<m::MessageSend>(send_params(
        space,
        channel,
        vec![AttachmentInput {
            path,
            ..Default::default()
        }],
    ));
    let squeeze = async {
        eventually("첫 조각을 쓰고 멈춤", || async {
            pause.reached().then_some(())
        })
        .await;
        let cleared = a.ok::<m::BlobCacheClear>(Empty).await;
        testkit::blobs::set_disk_free(&a.node, Some(0));
        let enforced = testkit::blobs::enforce_cache_now(&a.node);
        testkit::blobs::set_disk_free(&a.node, None);
        pause.release();
        (cleared, enforced)
    };
    let (sent, (cleared, enforced)) = tokio::join!(send, squeeze);
    assert_eq!(cleared.evicted_count, 0, "{cleared:?}");
    assert_eq!(enforced, (0, 0));
    let id = sent.expect("보내기").id;
    let blob = a
        .messages(space, channel)
        .await
        .into_iter()
        .find(|v| v.id == id)
        .expect("보낸 메시지")
        .attachments[0]
        .blob;
    let st = status(&a, space, blob).await;
    assert!(
        st.total_chunks == 3 && st.have_chunks == 3 && st.kept,
        "{st:?}"
    );
    wait_complete(&b, space, blob, "B가 다 받음").await;
    assert_eq!(read_blob(&b, space, blob, id).await, bytes);
}

/// 한도를 낮추거나 최소 여유를 올리면 받은 첨부를 지울 수 있어, 자동화 토큰(AI)이 그렇게 바꾸면 사람 확인을
/// 받습니다. 늘리는 것은 바로 됩니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn automation_lowering_storage_budget_needs_human() {
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let bot = automation(&a, vec![Scope::Settings]).await;
    let start = a.ok::<m::BlobCacheStatus>(Empty).await;
    let (limit, min_free) = (start.limit_bytes, start.min_free_bytes);

    // 한도 늘리기(최소 여유는 그대로)는 바로.
    let v = call_as::<m::BlobCacheSetLimit>(
        &a,
        &bot,
        BlobCacheLimitParams {
            limit_bytes: Some(limit * 2),
            min_free_bytes: Some(min_free),
            ..Default::default()
        },
    )
    .await
    .expect("늘리기는 바로");
    assert_eq!((v.limit_bytes, v.min_free_bytes), (limit * 2, min_free));

    // 한도 줄이기, 최소 여유 늘리기, (늘려 둔 값에서) 기본값으로 되돌리기는 모두 확인을 받고, 그 전에는 그대로입니다.
    for p in [
        BlobCacheLimitParams {
            limit_bytes: Some(1024 * 1024),
            ..Default::default()
        },
        BlobCacheLimitParams {
            min_free_bytes: Some(min_free + 1024 * 1024),
            ..Default::default()
        },
        BlobCacheLimitParams {
            reset: true,
            ..Default::default()
        },
    ] {
        confirmation_id(call_as::<m::BlobCacheSetLimit>(&a, &bot, p).await);
    }
    let v = a.ok::<m::BlobCacheStatus>(Empty).await;
    assert_eq!((v.limit_bytes, v.min_free_bytes), (limit * 2, min_free));
    // 모두 비우기도 언제나 확인을 받습니다.
    confirmation_id(call_as::<m::BlobCacheClear>(&a, &bot, Empty).await);

    // 사람(소유자)이 승인하면 그 값으로 바뀝니다. 요약에는 무엇이 줄어드는지 보입니다.
    let list = a.ok::<m::ConfirmList>(Empty).await;
    assert_eq!(list.len(), 4, "{list:?}");
    let lower = list
        .iter()
        .find(|c| {
            c.method == "blob.cache_set_limit"
                && c.params.get("limit_bytes").and_then(|v| v.as_u64()) == Some(1024 * 1024)
        })
        .expect("한도 줄이기 확인 요청");
    assert!(
        lower.summary.contains("저장 공간 줄이기") && lower.summary.contains("한도 1MiB"),
        "{}",
        lower.summary
    );
    let first = lower.id.clone();
    a.ok::<m::ConfirmApprove>(IdParams { id: first }).await;
    let v = a.ok::<m::BlobCacheStatus>(Empty).await;
    assert_eq!(v.limit_bytes, 1024 * 1024, "{v:?}");
    // 사람이 직접 줄이면 확인 없이 바로 됩니다.
    let v = set_limit(&a, 512 * 1024).await;
    assert_eq!(v.limit_bytes, 512 * 1024);
}

// ───────────── 2. 헤드리스 시더의 미리 받기 ─────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn prefetch_fetches_unopened_channels_within_budget() {
    let cl = Cluster::new();
    // B는 채널을 열지 않습니다(메시지 목록을 보지 않음).
    let (a, b, space, channel) = crew(&cl, false).await;
    let (id1, o1) = send_file(&a, space, channel, "o1.bin", &sample(60_000, 1)).await;
    let (id2, o2) = send_file(&a, space, channel, "o2.bin", &sample(60_000, 2)).await;
    for (id, blob) in [(id1, o1), (id2, o2)] {
        eventually("B가 열지 않은 채널의 첨부를 미뤄 둠", || async {
            (testkit::get_event(&b.node, &id).unwrap().is_some()
                && testkit::blobs::want_deferred(&b.node, &blob) == Some(true))
            .then_some(())
        })
        .await;
    }

    // 한도는 하나만 들어갈 만큼. 미리 받기를 켜면 최근 것(o2)부터 남은 자리 안에서만 받습니다.
    set_limit(&b, 100_000).await;
    b.ok::<m::SeedSet>(SeedSetParams {
        prefetch: Some(true),
        ..Default::default()
    })
    .await;
    assert!(
        b.ok::<m::SeedGet>(OptSpaceParams { space: None })
            .await
            .prefetch
    );
    wait_complete(&b, space, o2, "B가 최근 첨부를 미리 받음").await;
    assert_eq!(
        testkit::blobs::backfill_now(&b.node),
        0,
        "자리가 없어 더 넣지 않음"
    );
    assert_eq!(testkit::blobs::stored_bytes(&b.node, &o1), None);
    assert_eq!(testkit::blobs::want_deferred(&b.node, &o1), Some(true));
    let v = b.ok::<m::BlobCacheStatus>(Empty).await;
    assert_eq!(v.cache_count, 1, "{v:?}");
    assert_eq!(v.evicted_count, 0, "미리 받기는 다른 첨부를 비우지 않음");

    // 한도를 늘리면 남은 것도 받습니다.
    set_limit(&b, 10 * 1024 * 1024).await;
    assert!(testkit::blobs::backfill_now(&b.node) >= 1);
    wait_complete(&b, space, o1, "B가 지난 첨부를 마저 받음").await;

    // 새 메시지의 첨부는 열지 않은 채널이어도 미루지 않고 받습니다.
    let (_, n1) = send_file(&a, space, channel, "n1.bin", &sample(30_000, 3)).await;
    wait_complete(&b, space, n1, "B가 새 첨부를 바로 받음").await;

    // 따라잡기로 들어온 지난 메시지(작성 시각이 오래된 것)의 첨부는 바로 받지 않고 미뤄 둡니다. 한도가 찬 시더가
    // 지난 첨부를 받고 비우기를 되풀이하지 않게, 보충이 남은 자리만큼만 넣습니다.
    testkit::blobs::set_prefetch_live_ms(&b.node, Some(0));
    let (id_old, old) = send_file(&a, space, channel, "old.bin", &sample(30_000, 5)).await;
    eventually("지난 메시지의 첨부는 미룸", || async {
        (testkit::get_event(&b.node, &id_old).unwrap().is_some()
            && testkit::blobs::want_deferred(&b.node, &old) == Some(true))
        .then_some(())
    })
    .await;
    testkit::blobs::set_prefetch_live_ms(&b.node, None);
    assert!(testkit::blobs::backfill_now(&b.node) >= 1);
    wait_complete(&b, space, old, "보충이 지난 첨부를 받음").await;

    // 받아 둔 것은 열지 않은 채널이어도 남에게 내줍니다.
    for blob in [o1, o2, n1] {
        assert!(have_bits(&a, &b, space, blob).await.unwrap_or(0) > 0);
    }
    // 끄면 새 첨부는 다시 미룹니다.
    b.ok::<m::SeedSet>(SeedSetParams {
        prefetch: Some(false),
        ..Default::default()
    })
    .await;
    let (id3, n2) = send_file(&a, space, channel, "n2.bin", &sample(30_000, 4)).await;
    eventually("끈 뒤에는 미룸", || async {
        (testkit::get_event(&b.node, &id3).unwrap().is_some()
            && testkit::blobs::want_deferred(&b.node, &n2) == Some(true))
        .then_some(())
    })
    .await;
}

// ───────────── 3. 앱이 넘긴 동영상·음성 썸네일·길이 ─────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn media_hints_are_validated_and_delivered() {
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let b = cl.node("b").await;
    let space = befriend(&a, &b).await;
    let channel = a.first_channel(space).await;

    let mut webm = vec![0x1A, 0x45, 0xDF, 0xA3];
    webm.extend(sample(30_000, 1));
    let mut ogg = b"OggS".to_vec();
    ogg.extend(sample(20_000, 2));
    let thumb = png(64, 36);
    let webm_path = write_file(&a, "clip.webm", &webm);
    let ogg_path = write_file(&a, "voice.ogg", &ogg);
    let mp4_path = write_file(&a, "clip.mp4", &tiny_mp4(12_500));
    let photo_path = write_file(&a, "photo.png", &png(100, 50));

    let id = a
        .ok::<m::MessageSend>(send_params(
            space,
            channel,
            vec![
                AttachmentInput {
                    path: webm_path.clone(),
                    thumbnail_base64: Some(b64(&thumb)),
                    width: Some(1920),
                    height: Some(1080),
                    duration_ms: Some(5_000),
                    ..Default::default()
                },
                AttachmentInput {
                    path: ogg_path.clone(),
                    duration_ms: Some(90_000),
                    ..Default::default()
                },
                // 데몬이 파일에서 읽은 길이가 앱 값보다 우선합니다.
                AttachmentInput {
                    path: mp4_path,
                    thumbnail_base64: Some(b64(&thumb)),
                    duration_ms: Some(9_999),
                    ..Default::default()
                },
                // 이미지는 데몬이 직접 만들므로 앱 값은 무시합니다.
                AttachmentInput {
                    path: photo_path,
                    width: Some(9),
                    height: Some(9),
                    ..Default::default()
                },
            ],
        ))
        .await
        .id;

    // 받는 쪽은 원본 없이 썸네일·크기·길이를 봅니다.
    let view = wait_message(
        &b,
        space,
        channel,
        id,
        "B가 첨부 정보를 받음",
        |v| v.attachments.len() == 4,
    )
    .await;
    let [vid, aud, mp4, img] = [0, 1, 2, 3].map(|i| view.attachments[i].clone());
    assert_eq!(vid.mime, "video/webm");
    assert_eq!(
        (vid.width, vid.height, vid.duration_ms),
        (Some(1920), Some(1080), Some(5_000))
    );
    let t = base64::engine::general_purpose::STANDARD
        .decode(vid.thumbnail_base64.as_deref().expect("썸네일"))
        .unwrap();
    // 데몬이 JPEG로 다시 만든 것(원래 PNG 그대로가 아님).
    let t = image::load_from_memory_with_format(&t, image::ImageFormat::Jpeg).unwrap();
    assert_eq!((t.width(), t.height()), (64, 36));
    assert_eq!(aud.mime, "audio/ogg");
    assert_eq!((aud.width, aud.duration_ms), (None, Some(90_000)));
    assert!(aud.thumbnail_base64.is_none());
    assert_eq!(mp4.duration_ms, Some(12_500));
    assert!(mp4.thumbnail_base64.is_some());
    assert_eq!((img.width, img.height), (Some(100), Some(50)));
    assert!(!vid.downloaded || vid.size == webm.len() as u64);

    // 파일 내용으로 가린 형식과 힌트가 맞지 않으면(앱과 데몬의 판별이 다를 수 있음) 힌트만 버리고 보냅니다.
    // 머리말 없는 AAC(ADTS)는 데몬도 음성으로 가려 길이를 싣습니다.
    let pdf_path = write_file(&a, "doc.pdf", b"%PDF-1.4 not really");
    let mut adts = vec![0xFF, 0xF1, 0x50, 0x80];
    adts.extend(sample(10_000, 4));
    let aac_path = write_file(&a, "song.aac", &adts);
    let id = a
        .ok::<m::MessageSend>(send_params(
            space,
            channel,
            vec![
                AttachmentInput {
                    path: pdf_path,
                    duration_ms: Some(1_000),
                    thumbnail_base64: Some(b64(&thumb)),
                    ..Default::default()
                },
                // 음성에는 화면 크기가 없어 크기만 버리고 길이는 싣습니다.
                AttachmentInput {
                    path: ogg_path.clone(),
                    width: Some(10),
                    height: Some(10),
                    duration_ms: Some(3_000),
                    ..Default::default()
                },
                AttachmentInput {
                    path: aac_path,
                    duration_ms: Some(4_000),
                    ..Default::default()
                },
            ],
        ))
        .await
        .id;
    let view = wait_message(
        &b,
        space,
        channel,
        id,
        "B가 힌트를 버린 첨부를 받음",
        |v| v.attachments.len() == 3,
    )
    .await;
    let [pdf, ogg2, aac] = [0, 1, 2].map(|i| view.attachments[i].clone());
    assert_eq!(pdf.mime, "application/pdf");
    assert_eq!((pdf.duration_ms, pdf.width), (None, None));
    assert!(pdf.thumbnail_base64.is_none());
    assert_eq!(
        (ogg2.width, ogg2.height, ogg2.duration_ms),
        (None, None, Some(3_000))
    );
    assert_eq!(
        (aac.mime.as_str(), aac.duration_ms),
        ("audio/aac", Some(4_000))
    );

    // 잘못 만든 힌트 값은 보내기 전에 거절합니다(메시지가 생기지 않음).
    let before = a.messages(space, channel).await.len();
    let gif = b"GIF89a\x01\x00\x01\x00\x00\x00\x00;".to_vec();
    let wide = png(1_000, 10);
    let bad = [
        AttachmentInput {
            path: webm_path.clone(),
            thumbnail_base64: Some(b64(&gif)),
            ..Default::default()
        },
        AttachmentInput {
            path: webm_path.clone(),
            thumbnail_base64: Some(b64(&wide)),
            ..Default::default()
        },
        AttachmentInput {
            path: webm_path.clone(),
            thumbnail_base64: Some(b64(&vec![0u8; 70_000])),
            ..Default::default()
        },
        AttachmentInput {
            path: webm_path.clone(),
            width: Some(100),
            ..Default::default()
        },
        AttachmentInput {
            path: webm_path,
            duration_ms: Some(8 * 24 * 60 * 60 * 1000),
            ..Default::default()
        },
        AttachmentInput {
            path: ogg_path,
            thumbnail_base64: Some("이건 base64가 아님".into()),
            ..Default::default()
        },
    ];
    for inp in bad {
        let what = format!("{inp:?}");
        let r = a
            .call::<m::MessageSend>(send_params(space, channel, vec![inp]))
            .await;
        assert_eq!(
            code(r),
            ERR_INVALID_INPUT,
            "{}",
            &what[..what.len().min(200)]
        );
    }
    assert_eq!(a.messages(space, channel).await.len(), before);
}

// ───────────── 다시 잇기 ─────────────

/// 비운 첨부를 같은 파일로 다시 채움: 크기·내용이 다르면(해시가 다름) 아무것도 쓰지 않고 거절, 같으면 받지 않고 곧바로
/// 다 차고 고정되며 남에게 다시 내줍니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn relink_refills_cleared_attachment_only_from_same_file() {
    let cl = Cluster::new();
    let (a, b, space, channel) = crew(&cl, true).await;
    let bytes = sample(300_000, 9);
    let (id, blob) = send_file(&a, space, channel, "원본.bin", &bytes).await;
    wait_complete(&b, space, blob, "B가 받음").await;
    b.ok::<m::BlobCacheClear>(Empty).await;
    assert_eq!(stored(&b, &blob), 0, "받은 첨부를 비움");

    let relink = |path: String, message: EventId| BlobRelinkParams {
        space,
        channel,
        message,
        blob,
        path,
    };
    let shorter = write_file(&b, "짧음.bin", &bytes[..1000]);
    assert_eq!(
        code(b.call::<m::BlobRelink>(relink(shorter, id)).await),
        ERR_INVALID_INPUT
    );
    let mut changed = bytes.clone();
    changed[123_456] ^= 1;
    let changed = write_file(&b, "다름.bin", &changed);
    assert_eq!(
        code(b.call::<m::BlobRelink>(relink(changed, id)).await),
        ERR_INVALID_INPUT
    );
    assert_eq!(stored(&b, &blob), 0, "다른 파일로는 아무것도 쓰지 않음");
    let same = write_file(&b, "옮긴 원본.bin", &bytes);
    assert_eq!(
        code(
            b.call::<m::BlobRelink>(relink("옮긴 원본.bin".into(), id))
                .await
        ),
        ERR_INVALID_INPUT,
        "상대 경로"
    );
    let plain = a.send(space, channel, "글만").await;
    wait_message(&b, space, channel, plain, "B가 글을 받음", |_| true).await;
    assert_eq!(
        code(b.call::<m::BlobRelink>(relink(same.clone(), plain)).await),
        ERR_NOT_FOUND,
        "그 첨부가 없는 메시지"
    );

    let st = b.ok::<m::BlobRelink>(relink(same, id)).await;
    assert!(
        st.total_chunks > 0 && st.have_chunks == st.total_chunks,
        "{st:?}"
    );
    assert!(st.pinned, "다시 이은 첨부는 고정");
    assert_eq!(read_blob(&b, space, blob, id).await, bytes);
    assert!(
        have_bits(&a, &b, space, blob).await.unwrap_or(0) > 0,
        "다시 내줌"
    );
}

/// 기본은 메타데이터를 지우고 보냄: 받은 원본·썸네일(원본을 그대로 싣는 작은 사진)에 EXIF가 없고, 원본 파일로도
/// 다시 이을 수 있음(같은 방식으로 지운 내용이 맞음). `keep_metadata`면 그대로.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attachments_lose_metadata_by_default_and_relink_from_original() {
    let cl = Cluster::new();
    let (a, b, space, channel) = crew(&cl, true).await;
    let mut plain = Vec::new();
    image::codecs::jpeg::JpegEncoder::new(&mut plain)
        .encode_image(&image::RgbImage::from_pixel(
            16,
            8,
            image::Rgb([9, 99, 199]),
        ))
        .unwrap();
    let body = b"Exif\0\0MM\0\x2A\0\0\0\x08\0\0\0\0\0\0GPS-SECRET";
    let mut photo = plain[..2].to_vec();
    photo.extend_from_slice(&[0xFF, 0xE1]);
    photo.extend_from_slice(&(body.len() as u16 + 2).to_be_bytes());
    photo.extend_from_slice(body);
    photo.extend_from_slice(&plain[2..]);
    let secret = |v: &[u8]| v.windows(10).any(|w| w == b"GPS-SECRET");

    let (id, blob) = send_file(&a, space, channel, "사진.jpg", &photo).await;
    wait_complete(&b, space, blob, "B가 받음").await;
    // B가 받은 것은 비운 뒤 원본 파일로 다시 이어 확인합니다(내용은 보낸 A에서 읽음).
    b.ok::<m::BlobCacheClear>(Empty).await;
    assert_eq!(stored(&b, &blob), 0);
    let got = read_blob(&a, space, blob, id).await;
    assert!(!secret(&got) && got.len() < photo.len());
    image::load_from_memory(&got).unwrap();
    let view = wait_message(&b, space, channel, id, "B가 첨부를 봄", |v| {
        !v.attachments.is_empty()
    })
    .await;
    let att = &view.attachments[0];
    assert_eq!(att.size, got.len() as u64);
    let thumb = base64::engine::general_purpose::STANDARD
        .decode(att.thumbnail_base64.as_deref().expect("썸네일"))
        .unwrap();
    assert!(!secret(&thumb));

    let original = write_file(&b, "원본 사진.jpg", &photo);
    let st = b
        .ok::<m::BlobRelink>(BlobRelinkParams {
            space,
            channel,
            message: id,
            blob,
            path: original,
        })
        .await;
    assert!(st.total_chunks > 0 && st.have_chunks == st.total_chunks);
    assert_eq!(read_blob(&b, space, blob, id).await, got);

    let path = write_file(&a, "그대로.jpg", &photo);
    let kept = a
        .ok::<m::MessageSend>(send_params(
            space,
            channel,
            vec![AttachmentInput {
                path,
                keep_metadata: true,
                ..Default::default()
            }],
        ))
        .await
        .id;
    let view = wait_message(&a, space, channel, kept, "보냄", |_| true).await;
    let blob = view.attachments[0].blob;
    assert!(secret(&read_blob(&a, space, blob, kept).await));
}

// ───────────── 라이브러리 탭 ─────────────

async fn library(n: &TestNode, p: LibraryListParams) -> LibraryPage {
    n.ok::<m::LibraryList>(p).await
}

/// 주고받은 첨부가 쪽마다 나오고(썸네일은 고른 글만), 공유 중단·익명 가림·토큰 범위가 채팅 목록과 같고, 이 기기에서
/// 지운 첨부는 메시지가 남아 다시 받을 수 있습니다(내가 올린 것도 지울 수 있고, 자동화 토큰은 사람 확인).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn library_lists_files_and_evicts_local_data() {
    let cl = Cluster::new();
    let (a, b, space, channel) = crew(&cl, true).await;
    let (m1, f1) = send_file(&a, space, channel, "사진.png", &png(64, 48)).await;
    let (m2, f2) = send_file(&b, space, channel, "메모.bin", &sample(30_000, 3)).await;
    wait_complete(&b, space, f1, "B가 A의 사진을 받음").await;
    let plain = a.send(space, channel, "글만").await;
    wait_message(&b, space, channel, plain, "B가 글을 받음", |_| true).await;

    // 전체 목록: 첨부를 실은 글만, 썸네일 없이, 공간·채널을 채움.
    let page = library(&b, LibraryListParams::default()).await;
    assert_eq!(page.total, 2);
    let mut ids: Vec<_> = page.items.iter().map(|i| i.message.id).collect();
    ids.sort();
    let mut want = vec![m1, m2];
    want.sort();
    assert_eq!(ids, want);
    for it in &page.items {
        assert_eq!(
            (it.message.space, it.message.channel),
            (Some(space), Some(channel))
        );
        assert!(it.message.attachments[0].thumbnail_base64.is_none());
        assert!(!it.excluded);
        assert!(it.received_at.0 > 0);
    }
    let get = |p: &LibraryPage, id: EventId| p.items.iter().find(|i| i.message.id == id).cloned();
    assert!(get(&page, m2).unwrap().message.mine);
    assert!(get(&page, m1).unwrap().message.attachments[0].downloaded);

    // 쪽 나누기, 고른 글만 썸네일과 함께.
    let first = library(
        &b,
        LibraryListParams {
            limit: Some(1),
            ..Default::default()
        },
    )
    .await;
    let second = library(
        &b,
        LibraryListParams {
            offset: Some(1),
            limit: Some(1),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(
        (first.items.len(), second.items.len(), first.total),
        (1, 1, 2)
    );
    assert_ne!(first.items[0].message.id, second.items[0].message.id);
    let picked = library(
        &b,
        LibraryListParams {
            messages: Some(vec![m1, plain]),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(picked.items.len(), 1, "첨부가 없는 글은 빠짐");
    assert!(
        picked.items[0].message.attachments[0]
            .thumbnail_base64
            .is_some()
    );

    // 다른 공간으로 제한한 토큰에는 이 공간의 파일이 나오지 않습니다.
    let (elsewhere, _) = testkit::blobs::create_group_space(&b.node, &[])
        .await
        .expect("B 혼자인 크루");
    let r = b
        .ok::<m::TokenCreate>(TokenCreateParams {
            name: "다른 공간".into(),
            scopes: vec![Scope::Read],
            spaces: vec![elsewhere],
            automation: None,
        })
        .await;
    let other = CallCtx::from_auth(
        ApiHandler::new(b.node.clone())
            .authenticate(r.token.expose())
            .await
            .expect("새 토큰으로 인증"),
    );
    let p = call_as::<m::LibraryList>(&b, &other, LibraryListParams::default())
        .await
        .unwrap();
    assert_eq!((p.total, p.items.len()), (0, 0));

    // 공유 중단이 목록에 보입니다.
    b.ok::<m::SeedExcludeMessage>(SeedExcludeParams {
        space,
        channel,
        id: m1,
        excluded: true,
    })
    .await;
    let page = library(&b, LibraryListParams::default()).await;
    assert!(get(&page, m1).unwrap().excluded);
    assert!(!get(&page, m2).unwrap().excluded);

    // 익명 채널이 되면 글쓴이를 가립니다(채팅 목록과 같음).
    let mut up = ChannelUpdateParams::new(space, channel);
    up.anonymous = Some(true);
    a.ok::<m::ChannelUpdate>(up).await;
    eventually("B가 익명 설정을 받음", || async {
        let p = library(&b, LibraryListParams::default()).await;
        let v = get(&p, m1)?.message;
        (v.author == ANONYMOUS_USER && v.anon_name.is_some()).then_some(())
    })
    .await;

    // 이 기기에서 지우기: 받은 첨부.
    b.ok::<m::BlobEvict>(BlobParams { space, blob: f1 }).await;
    assert_eq!(status(&b, space, f1).await.have_chunks, 0);
    let page = library(&b, LibraryListParams::default()).await;
    assert!(
        !get(&page, m1).unwrap().message.attachments[0].downloaded,
        "글은 남고 받지 않은 것으로"
    );
    b.ok::<m::BlobFetch>(BlobFetchParams {
        space,
        blob: f1,
        priority: Some(200),
    })
    .await;
    wait_complete(&b, space, f1, "지운 첨부를 다시 받음").await;

    // 내가 올린 첨부도 지울 수 있고, 모르는 블롭은 1004, 자동화 토큰은 사람 확인을 거칩니다.
    let bot = automation(&b, vec![Scope::Write]).await;
    let r = call_as::<m::BlobEvict>(&b, &bot, BlobParams { space, blob: f2 }).await;
    confirmation_id(r);
    b.ok::<m::BlobEvict>(BlobParams { space, blob: f2 }).await;
    assert_eq!(stored(&b, &f2), 0);
    assert_eq!(
        code(
            b.call::<m::BlobEvict>(BlobParams {
                space,
                blob: BlobId([5; 32]),
            })
            .await
        ),
        ERR_NOT_FOUND
    );
}
