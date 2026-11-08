//! 휘발성 채널 첨부(X1) 통합 시험: 주고받기·읽기·저장 확인, 메시지 삭제·퇴장·만료 때 RAM 비우기, 디스크·임시
//! 파일에 남지 않음(만료 전후), 늦은 입장자, 보낸 기기가 나간 뒤, 크기 한도·RAM 예산, 차단한 사용자, 묶지 못한
//! 첨부의 메시지는 나가지 않음, 받다 멈춘 첨부의 예산·포기, 일반 첨부와 나눠 쓰는 내려받기 자리, Tor 파일 회로.
//!
//! 세 노드를 한 프로세스에 띄워 크루 하나를 함께 쓰고, 고정 sleep 없이 조건이 될 때까지 기다립니다. 조각 수를
//! 늘리려고 조각 크기를 최소(16KiB)로 둡니다. 실패 메시지에는 표식을 넣지 않습니다(시험 출력도 디스크로 갈 수 있음).

use crate::common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use base64::Engine;
use common::{Cluster, TestNode, connect_both, eventually, next_notification, wait_message};
use serde_json::json;
use yangpa_core::blob::MIN_CHUNK_SIZE;
use yangpa_core::ids::{BlobId, ChannelId, EventId, SpaceId};
use yangpa_core::model::{ChannelSettings, ChatTtl, HistoryVisibility, JoinMode, SecurityLevel};
use yangpa_core::wire::{WireErrorCode, YpRequest, YpResponse};
use yangpa_daemon::testkit::volatile as vt;
use yangpa_daemon::{CallCtx, Node, NodeConfig, testkit};
use yangpa_rpc::api::FileCircuitsParams;
use yangpa_rpc::api::{
    AttachmentInput, AttachmentView, BlobFetchParams, BlobParams, BlobPasteDiscardParams,
    BlobPasteParams, BlobReadParams, BlobReadResult, BlobRelinkParams, BlobSaveParams,
    BlobStatusView, ChannelSetP2pParams, LeakcheckParams, MessageListParams, MessageRefParams,
    MessageSendParams, Notification, PeerSetParams, Scope, SpaceChannelParams, SpaceParams,
    UserParams, m,
};
use yangpa_rpc::jsonrpc::{
    ERR_CONFLICT, ERR_INVALID_INPUT, ERR_NOT_FOUND, ERR_OUT_OF_SCOPE, ERR_VOLATILE_READ_DENIED,
};
use yangpa_rpc::{AuthInfo, RpcError};

// ───────────── 도우미 ─────────────

type Tweak = fn(&mut NodeConfig);

fn small_chunks(cfg: &mut NodeConfig) {
    cfg.sharing.chunk_size = MIN_CHUNK_SIZE;
}

/// 세 사람 크루. 노드를 먼저 두어 시험 경로(`Cluster`)보다 먼저 정리되게 합니다.
struct Crew {
    nodes: Vec<TestNode>,
    space: SpaceId,
    cluster: Cluster,
}

impl Crew {
    fn a(&self) -> &TestNode {
        &self.nodes[0]
    }
    fn b(&self) -> &TestNode {
        &self.nodes[1]
    }
    fn c(&self) -> &TestNode {
        &self.nodes[2]
    }
    fn all(&self) -> Vec<&TestNode> {
        self.nodes.iter().collect()
    }
    /// 노드 데이터 경로 밖의 시험용 경로.
    fn scratch(&self, name: &str) -> PathBuf {
        let dir = self.cluster.dir(name);
        std::fs::create_dir_all(&dir).expect("시험 경로");
        dir
    }
}

async fn crew() -> Crew {
    crew_with([small_chunks, small_chunks, small_chunks]).await
}

/// 서로 연결·인증한 세 노드와 `a`가 만든 크루.
async fn crew_with(tweaks: [Tweak; 3]) -> Crew {
    let cluster = Cluster::new();
    let mut nodes = Vec::new();
    for (name, tweak) in ["a", "b", "c"].into_iter().zip(tweaks) {
        nodes.push(cluster.node_with(name, tweak).await);
    }
    for i in 0..nodes.len() {
        for j in i + 1..nodes.len() {
            connect_both(&nodes[i], &nodes[j]).await;
        }
    }
    let others: Vec<Arc<Node>> = nodes[1..].iter().map(|n| n.node.clone()).collect();
    let space = vt::create_group_space(&nodes[0].node, &others)
        .await
        .expect("크루 만들기");
    for i in 0..nodes.len() {
        for j in i + 1..nodes.len() {
            testkit::authenticate(&nodes[i].node, nodes[j].peer())
                .await
                .expect("멤버끼리 인증");
        }
    }
    Crew {
        nodes,
        space,
        cluster,
    }
}

fn settings(mode: JoinMode, ttl: ChatTtl, history: HistoryVisibility) -> ChannelSettings {
    let mut s = ChannelSettings::text("휘발성 첨부");
    s.security = SecurityLevel::Volatile;
    s.join_mode = mode;
    s.ttl = ttl;
    s.history = history;
    // 켜 두어도 휘발성 채널에서는 미리보기를 만들지 않아야 합니다.
    s.link_previews = true;
    s
}

/// 휘발성 채널을 만들고 세 노드 모두에 보일 때까지 기다립니다.
async fn volatile_channel(c: &Crew, s: ChannelSettings) -> ChannelId {
    let ch = vt::create_channel_with(&c.a().node, c.space, s)
        .await
        .expect("채널 만들기");
    for n in c.all() {
        eventually("채널이 보임", || async {
            n.ok::<m::ChannelList>(yangpa_rpc::api::SpaceParams { space: c.space })
                .await
                .into_iter()
                .any(|v| v.channel == ch)
                .then_some(())
        })
        .await;
    }
    ch
}

/// 주어진 노드들이 같은 세션 키 세대로 보내게 될 때까지 기다립니다.
async fn converged(nodes: &[&TestNode], space: SpaceId, ch: ChannelId) {
    eventually("세션 키 세대가 맞춰짐", || async {
        let gens: Vec<Option<u64>> = nodes
            .iter()
            .map(|n| vt::volatile_current_generation(&n.node, &space, &ch))
            .collect();
        let first = gens[0]?;
        gens.iter().all(|g| *g == Some(first)).then_some(())
    })
    .await;
}

async fn join(n: &TestNode, space: SpaceId, channel: ChannelId) {
    n.ok::<m::ChannelJoin>(SpaceChannelParams { space, channel })
        .await;
}

async fn leave(n: &TestNode, space: SpaceId, channel: ChannelId) {
    n.ok::<m::ChannelLeave>(SpaceChannelParams { space, channel })
        .await;
}

fn marker(prefix: &str) -> String {
    format!(
        "{prefix}-{:016x}{:016x}",
        rand::random::<u64>(),
        rand::random::<u64>()
    )
}

/// 결정적 바이트. `marker`를 주면 처음·조각 경계 근처·끝에 넣습니다(조각 경계에 걸친 표식도 찾는지 보려고).
fn content(len: usize, seed: u8, marker: Option<&str>) -> Vec<u8> {
    let mut v: Vec<u8> = common::noise(len, seed);
    if let Some(mk) = marker {
        let mk = mk.as_bytes();
        let edge = MIN_CHUNK_SIZE as usize - 16 - mk.len() / 2;
        for at in [0, edge, len - mk.len()] {
            v[at..at + mk.len()].copy_from_slice(mk);
        }
    }
    v
}

/// 노드 데이터 경로 밖에 원본 파일을 씁니다.
fn write_source(c: &Crew, name: &str, bytes: &[u8]) -> PathBuf {
    let path = c.scratch("src").join(name);
    std::fs::write(&path, bytes).expect("원본 쓰기");
    path
}

fn input(path: &Path, name: Option<&str>) -> AttachmentInput {
    serde_json::from_value(json!({ "path": path.to_string_lossy(), "name": name }))
        .expect("첨부 입력")
}

async fn send_file(
    n: &TestNode,
    space: SpaceId,
    channel: ChannelId,
    text: &str,
    path: &Path,
    name: Option<&str>,
) -> Result<EventId, RpcError> {
    n.call::<m::MessageSend>(MessageSendParams {
        space,
        channel,
        text: text.into(),
        attachments: vec![input(path, name)],
        reply_to: None,
        thread: None,
    })
    .await
    .map(|r| r.id)
}

async fn attachment(n: &TestNode, space: SpaceId, ch: ChannelId, id: EventId) -> AttachmentView {
    wait_message(n, space, ch, id, "첨부 메시지", |v| {
        v.attachments.len() == 1
    })
    .await
    .attachments[0]
        .clone()
}

async fn wait_downloaded(n: &TestNode, space: SpaceId, ch: ChannelId, id: EventId) {
    wait_message(n, space, ch, id, "첨부를 다 받음", |v| {
        v.attachments.first().is_some_and(|a| a.downloaded)
    })
    .await;
}

async fn status(n: &TestNode, space: SpaceId, blob: BlobId) -> Result<BlobStatusView, RpcError> {
    n.call::<m::BlobStatus>(BlobParams { space, blob }).await
}

fn read_params(space: SpaceId, blob: BlobId, key_from: EventId) -> BlobReadParams {
    BlobReadParams {
        space,
        blob,
        key_from,
        index: None,
        offset: None,
        len: None,
        binary: None,
    }
}

async fn read_all(n: &TestNode, space: SpaceId, blob: BlobId, key_from: EventId) -> Vec<u8> {
    let r = n
        .ok::<m::BlobRead>(read_params(space, blob, key_from))
        .await;
    base64::engine::general_purpose::STANDARD
        .decode(r.data_base64.unwrap_or_default())
        .expect("base64")
}

fn save_params(space: SpaceId, blob: BlobId, key_from: EventId, path: &Path) -> BlobSaveParams {
    serde_json::from_value(json!({
        "space": space,
        "blob": blob,
        "key_from": key_from,
        "path": path.to_string_lossy(),
    }))
    .expect("저장 매개변수")
}

fn automation() -> CallCtx {
    CallCtx::from_auth(AuthInfo {
        token_id: "bot".into(),
        token_name: "자동화 시험".into(),
        scopes: vec![Scope::Read, Scope::Write],
        spaces: Vec::new(),
        automation: true,
    })
}

async fn call_as(
    n: &TestNode,
    ctx: &CallCtx,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, RpcError> {
    n.node.call(ctx, method, params).await
}

fn code<T: std::fmt::Debug>(r: Result<T, RpcError>) -> i64 {
    r.expect_err("오류여야 함").code
}

/// RAM 저장소에 첨부가 하나도 없고(잡아 둔 예산 포함) 받는 중인 것도 없는지.
fn ram_empty(n: &TestNode) -> bool {
    let s = vt::volatile_stats(&n.node);
    s.blobs == 0 && s.blob_bytes == 0 && s.blob_reserved == 0 && vt::volatile_fetching(&n.node) == 0
}

async fn ask(from: &TestNode, to: &TestNode, req: YpRequest) -> YpResponse {
    testkit::net_request(&from.node, to.peer(), req)
        .await
        .expect("요청")
}

fn is_not_found(r: &YpResponse) -> bool {
    matches!(
        r,
        YpResponse::Error {
            code: WireErrorCode::NotFound,
            ..
        }
    )
}

/// `root` 아래(깊이 `depth`까지) 파일 중 `needle`을 담은 것. `since`를 주면 그 뒤에 바뀐 파일만 봅니다.
fn files_containing(
    root: &Path,
    needle: &[u8],
    since: Option<SystemTime>,
    depth: u32,
    out: &mut Vec<PathBuf>,
) {
    let Ok(rd) = std::fs::read_dir(root) else {
        return;
    };
    for entry in rd.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        let path = entry.path();
        if ft.is_dir() {
            // 다른 시험이 남긴 tempfile 폴더(PC 러너의 임시 폴더에 수만 개)는 건너뜁니다. 다 훑으면 수 분이 걸립니다.
            if let Some(t) = since
                && entry.file_name().to_string_lossy().starts_with(".tmp")
                && entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .is_ok_and(|m| m < t)
            {
                continue;
            }
            if depth > 0 {
                files_containing(&path, needle, since, depth - 1, out);
            }
            continue;
        }
        if !ft.is_file() {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if meta.len() > 256 * 1024 * 1024 {
            continue;
        }
        if let Some(t) = since
            && meta.modified().map_or(true, |m| m < t)
        {
            continue;
        }
        if let Ok(data) = std::fs::read(&path)
            && data.windows(needle.len()).any(|w| w == needle)
        {
            out.push(path);
        }
    }
}

/// 표식이 어느 노드의 디스크(복호화한 DB·블롭 포함, `diag.leakcheck`)·데이터 경로 파일·시험 중에 생긴 임시 파일
/// 어디에도 없는지.
async fn assert_no_trace(c: &Crew, markers: &[&str], since: SystemTime, when: &str) {
    for n in c.all() {
        for mk in markers {
            let r = n
                .ok::<m::DiagLeakcheck>(LeakcheckParams {
                    marker: (*mk).to_owned(),
                })
                .await;
            assert!(
                r.clean && r.findings.is_empty(),
                "{when}: {} 디스크에 휘발성 첨부 내용이 있음: {:?}",
                n.name,
                r.findings
            );
            let mut hits = Vec::new();
            files_containing(&n.dir, mk.as_bytes(), None, 16, &mut hits);
            assert!(hits.is_empty(), "{when}: {} 데이터 경로: {hits:?}", n.name);
        }
    }
    for mk in markers {
        let mut hits = Vec::new();
        files_containing(
            &std::env::temp_dir(),
            mk.as_bytes(),
            Some(since),
            8,
            &mut hits,
        );
        assert!(hits.is_empty(), "{when}: 임시 파일: {hits:?}");
    }
}

// ───────────── 시험 ─────────────

/// 주고받기·읽기·자동화 거부·저장 확인, 미리보기 없음, 메시지를 지우면 모든 노드의 RAM이 비고 아무도 내주지 않음.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn round_trip_read_save_and_delete_wipes_ram() {
    let c = crew().await;
    let (a, b, cc) = (c.a(), c.b(), c.c());
    let ch = volatile_channel(
        &c,
        settings(JoinMode::Always, ChatTtl::None, HistoryVisibility::Private),
    )
    .await;
    converged(&c.all(), c.space, ch).await;

    let bytes = content(100 * 1024 + 123, 1, None);
    let src = write_source(&c, "자료.bin", &bytes);
    let id = send_file(
        a,
        c.space,
        ch,
        "첨부와 링크 https://example.com/",
        &src,
        None,
    )
    .await
    .expect("휘발성 첨부 보내기");
    // 보내는 쪽은 RAM에 올린 것을 이미 가졌고, 링크 미리보기는 만들지 않습니다.
    let view = wait_message(a, c.space, ch, id, "보낸 메시지", |_| true).await;
    assert!(view.volatile);
    assert!(view.embeds.is_empty(), "휘발성 채널에 링크 미리보기가 생김");
    let att = &view.attachments[0];
    assert!(att.downloaded);
    assert_eq!(att.size, bytes.len() as u64);
    assert_eq!(att.name, "자료.bin");
    let blob = att.blob;
    let info = vt::volatile_blob_info(&a.node, &c.space, &blob).expect("보낸 쪽 RAM");
    assert_eq!(info.owner, Some(id));
    assert_eq!(info.have, info.total);
    assert!(info.total > 1, "조각 여러 개");
    // 원본 파일이 없어져도 RAM에서 내줍니다(보낼 때 다 읽음).
    std::fs::remove_file(&src).expect("원본 지우기");
    // 휘발성 채널의 첨부는 같은 파일로도 다시 이을 수 없습니다(디스크에 두지 않음).
    let again = write_source(&c, "자료.bin", &bytes);
    let e = a
        .call::<m::BlobRelink>(BlobRelinkParams {
            space: c.space,
            channel: ch,
            message: id,
            blob,
            path: again.to_string_lossy().into_owned(),
        })
        .await
        .expect_err("휘발성 첨부 다시 잇기");
    assert_eq!(e.code, ERR_INVALID_INPUT);
    std::fs::remove_file(&again).expect("원본 지우기");

    for n in [b, cc] {
        wait_downloaded(n, c.space, ch, id).await;
        assert_eq!(read_all(n, c.space, blob, id).await, bytes, "{}", n.name);
        let v = wait_message(n, c.space, ch, id, "받은 메시지", |_| true).await;
        assert!(v.embeds.is_empty());
        assert_eq!(v.attachments[0].name, "자료.bin");
        let s = vt::volatile_stats(&n.node);
        assert_eq!(s.blobs, 1);
        assert!(s.blob_bytes as u64 > bytes.len() as u64);
        assert!(s.blob_reserved >= s.blob_bytes);
        let st = status(n, c.space, blob).await.expect("상태");
        assert_eq!(st.have_chunks, st.total_chunks);
        assert_eq!(st.size, bytes.len() as u64);
    }
    // 조각 하나·범위 읽기도 일반 첨부와 같습니다.
    let mut one = read_params(c.space, blob, id);
    one.offset = Some(10);
    one.len = Some(20);
    let r = b.ok::<m::BlobRead>(one).await;
    let part = base64::engine::general_purpose::STANDARD
        .decode(r.data_base64.unwrap_or_default())
        .unwrap();
    assert_eq!(part, bytes[10..30]);
    // 다른 메시지를 키 출처로 주면 찾지 못합니다.
    let other = EventId::from_bytes([9; 32]);
    assert_eq!(
        code(
            b.call::<m::BlobRead>(read_params(c.space, blob, other))
                .await
        ),
        ERR_NOT_FOUND
    );

    // 자동화 토큰: 휘발성 읽기를 허용하기 전에는 1007, 저장은 허용과 무관하게 사람만.
    let bot = automation();
    let read = json!({ "space": c.space, "blob": blob, "key_from": id });
    assert_eq!(
        code(call_as(b, &bot, "blob.read", read.clone()).await),
        ERR_VOLATILE_READ_DENIED
    );
    assert_eq!(
        code(
            call_as(
                b,
                &bot,
                "blob.status",
                json!({ "space": c.space, "blob": blob })
            )
            .await
        ),
        ERR_VOLATILE_READ_DENIED
    );
    let saved = c.scratch("saved").join("받은 자료.bin");
    let mut save = serde_json::to_value(save_params(c.space, blob, id, &saved)).unwrap();
    save["allow_volatile"] = json!(true);
    assert_eq!(
        code(call_as(b, &bot, "blob.save", save).await),
        ERR_OUT_OF_SCOPE
    );
    // 사람: 확인 없이 저장하면 1010, 확인하면 저장(덮어쓰지 않음).
    assert_eq!(
        code(
            b.call::<m::BlobSave>(save_params(c.space, blob, id, &saved))
                .await
        ),
        ERR_CONFLICT
    );
    assert!(!saved.exists());
    let mut ok = save_params(c.space, blob, id, &saved);
    ok.allow_volatile = true;
    b.ok::<m::BlobSave>(ok.clone()).await;
    assert_eq!(std::fs::read(&saved).unwrap(), bytes);
    #[cfg(windows)]
    assert_eq!(
        std::fs::read_to_string(format!("{}:Zone.Identifier", saved.display())).unwrap(),
        "[ZoneTransfer]\r\nZoneId=3\r\n",
        "받은 파일 표시"
    );
    assert_eq!(code(b.call::<m::BlobSave>(ok).await), ERR_CONFLICT);

    // 보낸 사람이 지우면 모든 노드의 RAM에서 첨부가 사라지고, 아무도 내주지 않습니다.
    a.ok::<m::MessageDelete>(MessageRefParams {
        space: c.space,
        channel: ch,
        id,
    })
    .await;
    for n in c.all() {
        eventually("RAM이 비워짐", || async { ram_empty(n).then_some(()) }).await;
        assert!(vt::volatile_blob_info(&n.node, &c.space, &blob).is_none());
        assert_eq!(code(status(n, c.space, blob).await), ERR_NOT_FOUND);
    }
    for (from, to) in [(b, a), (a, b), (b, cc)] {
        for req in [
            YpRequest::BlobManifest {
                space: c.space,
                blob,
            },
            YpRequest::BlobChunk {
                space: c.space,
                blob,
                index: 0,
            },
        ] {
            let r = ask(from, to, req).await;
            assert!(is_not_found(&r), "{} → {}: {r:?}", from.name, to.name);
        }
    }
}

/// 표식을 담은 첨부(이름에도 표식)를 보내고, 만료 전후로 어느 노드의 디스크·임시 파일에도 남지 않는지 봅니다.
/// 만료되면 모든 노드의 RAM이 비고 아무도 내주지 않습니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn marker_never_reaches_disk_and_expiry_wipes_ram() {
    // 파일 시각 해상도가 거친 파일 시스템을 감안해 조금 앞에서 잽니다.
    let since = SystemTime::now() - Duration::from_secs(2);
    let c = crew().await;
    let ch = volatile_channel(
        &c,
        settings(
            JoinMode::Always,
            ChatTtl::Seconds(10),
            HistoryVisibility::Private,
        ),
    )
    .await;
    converged(&c.all(), c.space, ch).await;
    let mut rxs: Vec<_> = c.all().iter().map(|n| n.subscribe()).collect();

    let body_marker = marker("VATT");
    let name_marker = marker("VNAME");
    let bytes = content(80 * 1024, 2, Some(&body_marker));
    let src = write_source(&c, "secret.bin", &bytes);
    let name = format!("{name_marker}.bin");
    let id = send_file(c.a(), c.space, ch, "", &src, Some(&name))
        .await
        .expect("휘발성 첨부 보내기");
    drop(bytes);
    std::fs::remove_file(&src).expect("원본 지우기");
    let blob = attachment(c.a(), c.space, ch, id).await.blob;

    // 받는 쪽이 다 받을 때까지(느린 기계에서 그 전에 만료되면 그대로 진행).
    for n in [c.b(), c.c()] {
        let got = eventually("다 받거나 만료됨", || async {
            match n
                .messages(c.space, ch)
                .await
                .into_iter()
                .find(|v| v.id == id)
            {
                Some(v) if v.attachments.first().is_some_and(|a| a.downloaded) => Some(true),
                Some(_) => None,
                None => Some(false),
            }
        })
        .await;
        if got {
            let info = vt::volatile_blob_info(&n.node, &c.space, &blob);
            assert!(info.is_none_or(|i| i.have == i.total));
        }
    }
    assert_no_trace(&c, &[&body_marker, &name_marker], since, "만료 전").await;

    for rx in rxs.iter_mut() {
        next_notification(rx, "만료 알림", |n| match n {
            Notification::MessageExpired { channel, ids, .. }
                if *channel == ch && ids.contains(&id) =>
            {
                Some(())
            }
            _ => None,
        })
        .await;
    }
    for n in c.all() {
        eventually("만료 뒤 RAM이 비워짐", || async {
            ram_empty(n).then_some(())
        })
        .await;
        assert_eq!(code(status(n, c.space, blob).await), ERR_NOT_FOUND);
    }
    for (from, to) in [(c.b(), c.a()), (c.a(), c.b()), (c.b(), c.c())] {
        let r = ask(
            from,
            to,
            YpRequest::BlobManifest {
                space: c.space,
                blob,
            },
        )
        .await;
        assert!(is_not_found(&r), "만료 뒤 내줌: {r:?}");
    }
    assert_no_trace(&c, &[&body_marker, &name_marker], since, "만료 뒤").await;
}

/// 클립보드에서 붙여넣은 바이트(`blob.paste`)도 휘발성 채널에서는 RAM에만 있고 디스크에 닿지 않습니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pasted_volatile_attachment_stays_in_ram() {
    let since = SystemTime::now() - Duration::from_secs(2);
    let c = crew().await;
    let ch = volatile_channel(
        &c,
        settings(
            JoinMode::Always,
            ChatTtl::Seconds(3600),
            HistoryVisibility::Private,
        ),
    )
    .await;
    converged(&c.all(), c.space, ch).await;
    let body_marker = marker("VPASTE");
    let bytes = content(40 * 1024, 7, Some(&body_marker));
    let paste = c
        .a()
        .ok::<m::BlobPaste>(BlobPasteParams {
            paste: None,
            size: Some(bytes.len() as u64),
            data_base64: base64::engine::general_purpose::STANDARD.encode(&bytes),
        })
        .await
        .paste;
    let id = c
        .a()
        .ok::<m::MessageSend>(MessageSendParams {
            space: c.space,
            channel: ch,
            text: String::new(),
            attachments: vec![AttachmentInput {
                paste: Some(paste.clone()),
                name: Some("clip.bin".into()),
                ..Default::default()
            }],
            reply_to: None,
            thread: None,
        })
        .await
        .id;
    c.a()
        .ok::<m::BlobPasteDiscard>(BlobPasteDiscardParams { paste })
        .await;
    let blob = attachment(c.b(), c.space, ch, id).await.blob;
    wait_downloaded(c.b(), c.space, ch, id).await;
    assert_eq!(read_all(c.b(), c.space, blob, id).await, bytes);
    assert_no_trace(&c, &[&body_marker], since, "붙여넣은 휘발성 첨부").await;
}

/// 늦은 입장자는 메시지가 살아 있는 동안 남은 참가자에게서 받습니다(보낸 사람이 나간 뒤에도). 참가자가 아닌 기기는
/// 받을 수 없고, 나가면 그 기기의 RAM이 바로 비워집니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn late_joiner_fetches_from_remaining_participant() {
    let c = crew().await;
    let (a, b, cc) = (c.a(), c.b(), c.c());
    let ch = volatile_channel(
        &c,
        settings(
            JoinMode::JoinLeave,
            ChatTtl::Seconds(3600),
            HistoryVisibility::Public,
        ),
    )
    .await;
    join(a, c.space, ch).await;
    join(b, c.space, ch).await;
    converged(&[a, b], c.space, ch).await;

    let bytes = content(70 * 1024, 3, None);
    let src = write_source(&c, "늦게.bin", &bytes);
    let id = send_file(a, c.space, ch, "입장 전 첨부", &src, None)
        .await
        .expect("보내기");
    let blob = attachment(a, c.space, ch, id).await.blob;
    wait_downloaded(b, c.space, ch, id).await;

    // 아직 참가하지 않은 c에게는 없는 블롭과 같은 답입니다.
    let r = ask(
        cc,
        b,
        YpRequest::BlobManifest {
            space: c.space,
            blob,
        },
    )
    .await;
    assert!(is_not_found(&r), "참가자가 아닌 기기에 내줌: {r:?}");

    // 보낸 사람이 나가면 그 기기의 RAM은 바로 비고, 메시지는 b에게 남습니다(퇴장 시 삭제 설정이 아님).
    leave(a, c.space, ch).await;
    assert!(ram_empty(a));
    assert!(vt::volatile_blob_info(&a.node, &c.space, &blob).is_none());

    join(cc, c.space, ch).await;
    wait_message(cc, c.space, ch, id, "스냅숏으로 받은 메시지", |_| true).await;
    wait_downloaded(cc, c.space, ch, id).await;
    assert_eq!(read_all(cc, c.space, blob, id).await, bytes);
    assert_eq!(
        vt::volatile_blob_info(&cc.node, &c.space, &blob).and_then(|i| i.owner),
        Some(id)
    );

    leave(b, c.space, ch).await;
    assert!(ram_empty(b));
    assert_eq!(read_all(cc, c.space, blob, id).await, bytes);
}

/// P2P 전용 휘발성 채널의 첨부는 P2P 연결로 받습니다. 매니페스트도 P2P 연결로 물어야 합니다(참가자가 P2P 피어라
/// Tor 스웜으로는 닿지 않아, 예전에는 받기를 시작하지 못하고 다시 시도만 반복했음).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn p2p_volatile_attachment_fetches_over_p2p_link() {
    let c = crew().await;
    let (a, b) = (c.a(), c.b());
    let ch = volatile_channel(
        &c,
        settings(
            JoinMode::JoinLeave,
            ChatTtl::Seconds(3600),
            HistoryVisibility::Public,
        ),
    )
    .await;
    a.ok::<m::ChannelSetP2p>(ChannelSetP2pParams {
        space: c.space,
        channel: ch,
        enabled: true,
    })
    .await;
    for n in [a, b] {
        eventually("P2P 전용 채널이 됨", || async {
            testkit::space_state(&n.node, &c.space)?
                .is_p2p(&ch)
                .then_some(())
        })
        .await;
        n.ok::<m::ChannelP2pJoin>(SpaceChannelParams {
            space: c.space,
            channel: ch,
        })
        .await;
    }
    eventually("A와 B가 P2P 스웜에서 서로 인증", || async {
        (testkit::p2p_peers(&a.node).len() == 1 && testkit::p2p_peers(&b.node).len() == 1)
            .then_some(())
    })
    .await;
    join(a, c.space, ch).await;
    join(b, c.space, ch).await;
    converged(&[a, b], c.space, ch).await;

    let bytes = content(300 * 1024, 9, None);
    let src = write_source(&c, "직접.bin", &bytes);
    let id = send_file(a, c.space, ch, "P2P 첨부", &src, None)
        .await
        .expect("보내기");
    let blob = attachment(b, c.space, ch, id).await.blob;
    wait_downloaded(b, c.space, ch, id).await;
    assert_eq!(read_all(b, c.space, blob, id).await, bytes);
}

/// 보낸 기기가 꺼져도 가진 참가자에게서 받습니다. 아무도 가지지 않으면 기다리는 동안 RAM 예산을 붙잡지 않고
/// 읽기·저장은 깔끔한 오류입니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sender_offline_and_no_seeder() {
    let c = crew().await;
    let (a, b, cc) = (c.a(), c.b(), c.c());
    let ch = volatile_channel(
        &c,
        settings(JoinMode::Always, ChatTtl::None, HistoryVisibility::Private),
    )
    .await;
    converged(&c.all(), c.space, ch).await;
    // b는 자동으로 받지 않습니다.
    b.ok::<m::PeerSet>(PeerSetParams {
        auto_download_max_bytes: Some(0),
        ..Default::default()
    })
    .await;

    let mut rx_b = b.subscribe();
    let bytes = content(90 * 1024, 4, None);
    let src = write_source(&c, "x.bin", &bytes);
    let x = send_file(a, c.space, ch, "x", &src, None)
        .await
        .expect("보내기");
    let blob_x = attachment(a, c.space, ch, x).await.blob;
    wait_downloaded(cc, c.space, ch, x).await;
    next_notification(&mut rx_b, "새 메시지", |n| match n {
        Notification::MessageNew { message, .. } if message.id == x => Some(()),
        _ => None,
    })
    .await;
    assert_eq!(vt::volatile_fetching(&b.node), 0, "자동 한도 밖인데 받음");
    assert!(vt::volatile_blob_info(&b.node, &c.space, &blob_x).is_none());

    // 보낸 기기가 꺼져도(그 RAM은 사라짐) c가 가진 것을 받습니다.
    a.shutdown().await;
    b.ok::<m::BlobFetch>(BlobFetchParams {
        space: c.space,
        blob: blob_x,
        priority: None,
    })
    .await;
    wait_downloaded(b, c.space, ch, x).await;
    assert_eq!(read_all(b, c.space, blob_x, x).await, bytes);

    // 아무도 가지지 않은 첨부: c가 보내고 꺼집니다.
    let mut rx_b = b.subscribe();
    let lonely = content(40 * 1024, 5, None);
    let src = write_source(&c, "y.bin", &lonely);
    let y = send_file(cc, c.space, ch, "y", &src, None)
        .await
        .expect("보내기");
    let blob_y = attachment(cc, c.space, ch, y).await.blob;
    next_notification(&mut rx_b, "새 메시지", |n| match n {
        Notification::MessageNew { message, .. } if message.id == y => Some(()),
        _ => None,
    })
    .await;
    cc.shutdown().await;
    b.ok::<m::BlobFetch>(BlobFetchParams {
        space: c.space,
        blob: blob_y,
        priority: None,
    })
    .await;
    let st = status(b, c.space, blob_y).await.expect("상태");
    assert_eq!(st.seeders_online, 0);
    assert_eq!(st.have_chunks, 0);
    assert!(st.downloading, "다시 시도하며 기다림");
    // 매니페스트를 준 참가자가 없으니 예산도 잡지 않습니다.
    assert!(vt::volatile_blob_info(&b.node, &c.space, &blob_y).is_none());
    assert_eq!(
        vt::volatile_stats(&b.node).blob_reserved,
        vt::volatile_stats(&b.node).blob_bytes
    );
    assert_eq!(
        code(b.call::<m::BlobRead>(read_params(c.space, blob_y, y)).await),
        ERR_NOT_FOUND
    );
    let mut save = save_params(c.space, blob_y, y, &c.scratch("saved").join("y.bin"));
    save.allow_volatile = true;
    assert_eq!(code(b.call::<m::BlobSave>(save).await), ERR_NOT_FOUND);
    // 받은 첨부는 그대로 읽힙니다.
    assert_eq!(read_all(b, c.space, blob_x, x).await, bytes);
}

fn sender_limits(cfg: &mut NodeConfig) {
    small_chunks(cfg);
    cfg.volatile.max_attachment_bytes = 64 * 1024;
    cfg.volatile.max_attachment_ram_bytes = 100 * 1024;
}

fn tiny_ram(cfg: &mut NodeConfig) {
    small_chunks(cfg);
    cfg.volatile.max_attachment_ram_bytes = 32 * 1024;
}

fn tiny_attachments(cfg: &mut NodeConfig) {
    small_chunks(cfg);
    cfg.volatile.max_attachment_bytes = 32 * 1024;
}

/// 크기 한도(1005)와 RAM 예산(1010): 보내기·자동 받기·직접 받기 모두 깔끔하게 거절하고 아무것도 남기지 않습니다.
/// 예산은 첨부가 사라지면 돌아옵니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn size_limit_and_ram_budget_refuse_cleanly() {
    let c = crew_with([sender_limits, tiny_ram, tiny_attachments]).await;
    let (a, b, cc) = (c.a(), c.b(), c.c());
    let ch = volatile_channel(
        &c,
        settings(JoinMode::Always, ChatTtl::None, HistoryVisibility::Private),
    )
    .await;
    converged(&c.all(), c.space, ch).await;

    // 한 첨부 한도를 넘는 파일.
    let big = write_source(&c, "big.bin", &content(100 * 1024, 6, None));
    assert_eq!(
        code(send_file(a, c.space, ch, "", &big, None).await),
        ERR_INVALID_INPUT
    );
    assert!(ram_empty(a));

    let mut rxs: Vec<_> = [b, cc].iter().map(|n| n.subscribe()).collect();
    let first = content(60 * 1024, 7, None);
    let src = write_source(&c, "first.bin", &first);
    let id = send_file(a, c.space, ch, "", &src, None)
        .await
        .expect("예산 안");
    let blob = attachment(a, c.space, ch, id).await.blob;
    for rx in rxs.iter_mut() {
        next_notification(rx, "새 메시지", |n| match n {
            Notification::MessageNew { message, .. } if message.id == id => Some(()),
            _ => None,
        })
        .await;
    }
    // b: 예산보다 커서 자동 받기를 그만두고(예산을 잡지 않음), 직접 받기는 1010.
    eventually("b가 받기를 그만둠", || async {
        (vt::volatile_fetching(&b.node) == 0).then_some(())
    })
    .await;
    assert!(ram_empty(b));
    let fetch = BlobFetchParams {
        space: c.space,
        blob,
        priority: None,
    };
    assert_eq!(
        code(b.call::<m::BlobFetch>(fetch.clone()).await),
        ERR_CONFLICT
    );
    let st = status(b, c.space, blob).await.expect("상태");
    assert_eq!(st.have_chunks, 0);
    assert!(!st.downloading);
    // c: 한 첨부 한도를 넘어 자동으로 받지 않고, 직접 받기는 1005.
    assert!(ram_empty(cc));
    assert_eq!(
        code(cc.call::<m::BlobFetch>(fetch.clone()).await),
        ERR_INVALID_INPUT
    );
    assert!(ram_empty(cc));

    // 보내는 쪽 예산: 첫 첨부가 살아 있는 동안 두 번째는 1010, 아무것도 남지 않음.
    let second = write_source(&c, "second.bin", &content(60 * 1024, 8, None));
    assert_eq!(
        code(send_file(a, c.space, ch, "", &second, None).await),
        ERR_CONFLICT
    );
    assert_eq!(vt::volatile_stats(&a.node).blobs, 1);
    // 첫 메시지를 지우면 예산이 돌아옵니다.
    a.ok::<m::MessageDelete>(MessageRefParams {
        space: c.space,
        channel: ch,
        id,
    })
    .await;
    assert!(ram_empty(a));
    send_file(a, c.space, ch, "", &second, None)
        .await
        .expect("예산이 돌아옴");
    assert_eq!(vt::volatile_stats(&a.node).blobs, 1);
}

/// 차단한 사용자의 첨부는 자동으로 받지 않고, 차단한 사용자에게는 내주지 않습니다(정책으로 뺀 데이터와 같은 답).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn blocked_user_is_neither_fetched_nor_served() {
    let c = crew().await;
    let (a, b, cc) = (c.a(), c.b(), c.c());
    let ch = volatile_channel(
        &c,
        settings(JoinMode::Always, ChatTtl::None, HistoryVisibility::Private),
    )
    .await;
    converged(&c.all(), c.space, ch).await;
    cc.ok::<m::UserBlock>(UserParams { user: b.user() }).await;

    let from_b = content(50 * 1024, 9, None);
    let src = write_source(&c, "from-b.bin", &from_b);
    let z = send_file(b, c.space, ch, "", &src, None)
        .await
        .expect("보내기");
    let blob_z = attachment(b, c.space, ch, z).await.blob;
    // 차단한 사람의 메시지는 알림 없이 목록에만(접혀서) 있습니다. 차단하지 않은 a가 다 받을 때까지 c는 받기를
    // 시작하지 않습니다.
    wait_message(cc, c.space, ch, z, "차단한 사람의 메시지", |_| {
        true
    })
    .await;
    wait_downloaded(a, c.space, ch, z).await;
    assert_eq!(vt::volatile_fetching(&cc.node), 0);
    assert!(vt::volatile_blob_info(&cc.node, &c.space, &blob_z).is_none());

    let from_a = content(50 * 1024, 10, None);
    let src = write_source(&c, "from-a.bin", &from_a);
    let w = send_file(a, c.space, ch, "", &src, None)
        .await
        .expect("보내기");
    let blob_w = attachment(a, c.space, ch, w).await.blob;
    for n in [b, cc] {
        wait_downloaded(n, c.space, ch, w).await;
    }
    let manifest = YpRequest::BlobManifest {
        space: c.space,
        blob: blob_w,
    };
    let r = ask(b, cc, manifest.clone()).await;
    assert!(
        matches!(
            r,
            YpResponse::Error {
                code: WireErrorCode::NotSeeded,
                ..
            }
        ),
        "차단한 사람에게 내줌: {r:?}"
    );
    let r = ask(b, a, manifest).await;
    assert!(matches!(r, YpResponse::Manifest(_)), "{r:?}");
}

/// 메시지 보내기 결과는 일반 첨부와 같은 형식이지만 블롭은 디스크 블롭 저장소에 들어가지 않습니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn volatile_blob_stays_out_of_disk_blob_store() {
    let c = crew().await;
    let (a, b) = (c.a(), c.b());
    let ch = volatile_channel(
        &c,
        settings(JoinMode::Always, ChatTtl::None, HistoryVisibility::Private),
    )
    .await;
    converged(&c.all(), c.space, ch).await;
    let bytes = content(40 * 1024, 11, None);
    let src = write_source(&c, "store.bin", &bytes);
    let id = send_file(a, c.space, ch, "", &src, None)
        .await
        .expect("보내기");
    let blob = attachment(a, c.space, ch, id).await.blob;
    wait_downloaded(b, c.space, ch, id).await;
    for n in [a, b] {
        assert!(
            testkit::blobs::chunk_progress(&n.node, &blob).is_none(),
            "{}: 디스크 블롭 저장소에 있음",
            n.name
        );
    }
    // 읽은 결과 형식(JSON base64)도 일반 첨부와 같습니다.
    let out = b
        .node
        .call(
            &CallCtx::owner(),
            "blob.read",
            json!({ "space": c.space, "blob": blob, "key_from": id }),
        )
        .await
        .expect("읽기");
    let r: BlobReadResult = serde_json::from_value(out).expect("형식");
    assert!(r.data_base64.is_some());
}

async fn message_ids(n: &TestNode, space: SpaceId, channel: ChannelId) -> Vec<EventId> {
    n.ok::<m::MessageList>(MessageListParams {
        space,
        channel,
        before: None,
        limit: None,
        thread: None,
    })
    .await
    .messages
    .into_iter()
    .map(|v| v.id)
    .collect()
}

/// 미리 올린 첨부를 메시지에 묶지 못하면(묶기 전에 첨부가 사라짐) 보내기가 실패하고, 받을 수 없는 첨부가 실린
/// 메시지는 누구에게도 가지 않으며 보낸 기기에도 남지 않습니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unclaimable_attachment_is_never_sent() {
    let c = crew().await;
    let (a, b) = (c.a(), c.b());
    let ch = volatile_channel(
        &c,
        settings(JoinMode::Always, ChatTtl::None, HistoryVisibility::Private),
    )
    .await;
    converged(&c.all(), c.space, ch).await;
    let bytes = content(40 * 1024, 13, None);
    let src = write_source(&c, "lost.bin", &bytes);
    vt::fail_volatile_claims(&a.node, 1);
    assert_eq!(
        code(send_file(a, c.space, ch, "묶지 못함", &src, None).await),
        ERR_CONFLICT
    );
    assert!(ram_empty(a), "묶지 못한 첨부가 RAM에 남음");
    assert!(
        message_ids(a, c.space, ch).await.is_empty(),
        "거둔 메시지가 남음"
    );

    // 다음 메시지는 나가고, 앞의 것은 아무에게도 가지 않았습니다.
    let id = send_file(a, c.space, ch, "두 번째", &src, None)
        .await
        .expect("보내기");
    wait_downloaded(b, c.space, ch, id).await;
    assert_eq!(message_ids(b, c.space, ch).await, vec![id]);
    assert_eq!(message_ids(a, c.space, ch).await, vec![id]);
    assert_eq!(
        read_all(b, c.space, attachment(b, c.space, ch, id).await.blob, id).await,
        bytes
    );
}

/// 보낸 기기가 중간에 멈춘 것처럼 받다 멈추면, 다시 시도를 기다리는 동안은 가진 만큼만 예산을 잡고, 오래 진전이
/// 없으면 받다 만 조각까지 지워 예산을 모두 돌려줍니다. 직접 받으면 처음부터 다시 받습니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stalled_partial_fetch_frees_budget_and_gives_up() {
    let c = crew().await;
    let (a, b) = (c.a(), c.b());
    let ch = volatile_channel(
        &c,
        settings(JoinMode::Always, ChatTtl::None, HistoryVisibility::Private),
    )
    .await;
    converged(&c.all(), c.space, ch).await;
    vt::tune_volatile_fetch(
        &b.node,
        Some(Duration::from_millis(300)),
        Some(Duration::from_secs(4)),
        Some(2),
    );
    let bytes = content(150 * 1024, 14, None);
    let src = write_source(&c, "stall.bin", &bytes);
    let id = send_file(a, c.space, ch, "", &src, None)
        .await
        .expect("보내기");
    let blob = attachment(a, c.space, ch, id).await.blob;
    let total = vt::volatile_blob_info(&a.node, &c.space, &blob)
        .expect("보낸 쪽 RAM")
        .total;
    assert!(total > 4, "조각이 여럿이어야 함");

    eventually(
        "b가 일부만 받고 쉬는 동안 가진 만큼만 예산을 잡음",
        || async {
            let i = vt::volatile_blob_info(&b.node, &c.space, &blob)?;
            let s = vt::volatile_stats(&b.node);
            (i.have == 2 && i.total == total && s.blob_bytes > 0 && s.blob_reserved == s.blob_bytes)
                .then_some(())
        },
    )
    .await;
    eventually("진전이 없어 받다 만 것을 지움", || async {
        ram_empty(b).then_some(())
    })
    .await;
    let st = status(b, c.space, blob).await.expect("상태");
    assert_eq!((st.have_chunks, st.downloading), (0, false));

    vt::tune_volatile_fetch(&b.node, None, None, None);
    b.ok::<m::BlobFetch>(BlobFetchParams {
        space: c.space,
        blob,
        priority: None,
    })
    .await;
    wait_downloaded(b, c.space, ch, id).await;
    assert_eq!(read_all(b, c.space, blob, id).await, bytes);
}

fn one_download(cfg: &mut NodeConfig) {
    small_chunks(cfg);
    cfg.sharing.max_active_downloads = 1;
}

/// 휘발성 첨부도 일반 첨부와 같은 동시 내려받기 한도(Tor로 돌면 Tor 한도) 안에서 받습니다. 자리가 없으면 기다리고,
/// 기다리는 동안 일반 대기열이 다음 빈자리를 가져가지 않습니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn volatile_fetches_share_download_slots() {
    let c = crew_with([small_chunks, one_download, small_chunks]).await;
    let (a, b) = (c.a(), c.b());
    let ch = volatile_channel(
        &c,
        settings(JoinMode::Always, ChatTtl::None, HistoryVisibility::Private),
    )
    .await;
    converged(&c.all(), c.space, ch).await;
    let normal = b
        .ok::<m::ChannelList>(SpaceParams { space: c.space })
        .await
        .into_iter()
        .find(|v| v.channel != ch)
        .expect("일반 채널")
        .channel;

    // b의 일반 내려받기를 한 조각에서 멈춰 하나뿐인 자리를 붙잡게 합니다.
    testkit::blobs::hold_downloads(&b.node, Some(1));
    let plain = content(120 * 1024, 15, None);
    let src = write_source(&c, "plain.bin", &plain);
    let n_id = send_file(a, c.space, normal, "일반", &src, None)
        .await
        .expect("일반 첨부 보내기");
    let n_blob = attachment(b, c.space, normal, n_id).await.blob;
    b.ok::<m::BlobFetch>(BlobFetchParams {
        space: c.space,
        blob: n_blob,
        priority: None,
    })
    .await;
    eventually("일반 첨부가 자리를 잡음", || async {
        (testkit::blobs::download_active(&b.node, &n_blob)
            && vt::download_slots(&b.node) == (1, 0, false))
            .then_some(())
    })
    .await;

    let bytes = content(60 * 1024, 16, None);
    let src = write_source(&c, "v.bin", &bytes);
    let v_id = send_file(a, c.space, ch, "", &src, None)
        .await
        .expect("휘발성 첨부 보내기");
    let v_blob = attachment(b, c.space, ch, v_id).await.blob;
    eventually("휘발성 첨부가 자리를 기다림", || async {
        (vt::download_slots(&b.node) == (1, 0, true)).then_some(())
    })
    .await;
    assert!(
        vt::volatile_blob_info(&b.node, &c.space, &v_blob).is_none(),
        "자리 없이 받기 시작함"
    );
    assert!(status(b, c.space, v_blob).await.expect("상태").downloading);

    // 일반 내려받기가 끝나면 그 자리를 휘발성 첨부가 받습니다.
    testkit::blobs::hold_downloads(&b.node, None);
    wait_downloaded(b, c.space, ch, v_id).await;
    assert_eq!(read_all(b, c.space, v_blob, v_id).await, bytes);
    wait_downloaded(b, c.space, normal, n_id).await;
    eventually("빌린 자리를 모두 돌려줌", || async {
        (vt::download_slots(&b.node) == (0, 0, false)).then_some(())
    })
    .await;
}

/// Tor로 도는 기기는 큰 휘발성 첨부도 일반 첨부처럼 파일 회로(`filelink`)로 나눠 받습니다. 회로는 메모리에만 있고
/// 조각을 나르기만 하며, 내주는 쪽은 주 연결과 같은 참가자 검사로 RAM에서 답합니다. 루프백에서는 파일 스웜이 Arti
/// 대신 받지 않는 직접 스웜이라 회로 대신 연결만 따로입니다. 조각 답을 늦춰 받는 동안 회로가 이어지게 합니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tor_volatile_attachment_uses_file_circuits() {
    let c = crew().await;
    let (a, b) = (c.a(), c.b());
    for n in [a, b] {
        testkit::sfu::enter_tor_policy(&n.node).await;
    }
    // 선택 기능이라 받는 B와 조각을 가진 A·C가 모두 켭니다.
    for n in [a, b, c.c()] {
        n.ok::<m::NetSetFileCircuits>(FileCircuitsParams {
            enabled: true,
            count: None,
        })
        .await;
    }
    let ch = volatile_channel(
        &c,
        settings(JoinMode::Always, ChatTtl::None, HistoryVisibility::Private),
    )
    .await;
    converged(&c.all(), c.space, ch).await;
    testkit::blobs::delay_serving(&a.node, 100);
    testkit::blobs::delay_serving(&c.c().node, 100);

    let bytes = content(1024 * 1024, 17, None);
    let src = write_source(&c, "big.bin", &bytes);
    let id = send_file(a, c.space, ch, "", &src, None)
        .await
        .expect("휘발성 첨부 보내기");
    let blob = attachment(b, c.space, ch, id).await.blob;
    b.ok::<m::BlobFetch>(BlobFetchParams {
        space: c.space,
        blob,
        priority: None,
    })
    .await;
    wait_downloaded(b, c.space, ch, id).await;
    assert_eq!(read_all(b, c.space, blob, id).await, bytes);

    let (ready, _, swarms) = testkit::file_links(&b.node);
    assert!(
        swarms >= 1 && ready >= 1,
        "B가 파일 회로를 이음: {ready}개, 스웜 {swarms}개"
    );
    // 조각을 가진 참가자는 A와(먼저 받았다면) C입니다.
    let served =
        testkit::file_link_chunks_served(&a.node) + testkit::file_link_chunks_served(&c.c().node);
    assert!(served > 0, "파일 회로로 내준 조각이 없음");
    assert!(
        vt::volatile_blob_info(&b.node, &c.space, &blob).is_some_and(|i| i.have == i.total),
        "B의 RAM에 다 있음"
    );
}
