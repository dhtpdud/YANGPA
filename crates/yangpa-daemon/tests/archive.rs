//! 보관 전용 노드(X3) 통합 시험: 관리자가 지정 → 기록·첨부가 보관 노드로 → 멤버가 모두 꺼져도 돌아온 멤버와
//! 새 멤버(과거 기록 공개 채널)가 보관 노드에서만 따라잡고 풀어 읽음 → 멤버가 아닌 노드·밴된 멤버 거절 → 해제하면
//! 내주기·받기가 멈추고 보관 노드가 지움. 보관 노드의 저장소·파일에는 평문 표식이 끝까지 나오지 않아야 합니다.
//!
//! 노드는 한 프로세스 안에서 127.0.0.1로 직접 연결합니다(`common` 참고). 고정 sleep 없이 조건을 기다립니다.

use crate::common;

use std::path::Path;
use std::time::Duration;

use base64::Engine;
use common::{Cluster, TestNode, connect, connect_both, eventually, wait_message};
use yangpa_core::event::StreamId;
use yangpa_core::ids::{BlobId, ChannelId, EventId, RoleId, SpaceId};
use yangpa_core::model::{ChannelKind, HistoryVisibility, Permissions};
use yangpa_core::wire::{WireErrorCode, YpRequest, YpResponse};
use yangpa_daemon::NodeConfig;
use yangpa_daemon::testkit;
use yangpa_daemon::testkit::archive as at;
use yangpa_rpc::api::{
    ArchiveDesignateParams, ArchiveOfferParams, ArchiveRevokeParams, ArchiveView, AttachmentInput,
    BlobFetchParams, BlobParams, BlobReadParams, ChannelCreateParams, CrewCreateParams,
    InviteCreateParams, InviteJoinStatus, LeakcheckParams, MemberBanParams, MemberSetRolesParams,
    MessageRefParams, MessageSendParams, RoleCreateParams, SeedSetParams, SpaceParams, UrlParams,
    m,
};

// ───────────── 도우미 ─────────────

/// 주기 작업(따라잡기·보관 작업)을 짧게.
fn fast(cfg: &mut NodeConfig) {
    cfg.sync_interval = Duration::from_millis(300);
}

/// [`fast`]에 몇 개의 상태 이벤트만으로 체크포인트를 내게.
fn fast_checkpoints(cfg: &mut NodeConfig) {
    fast(cfg);
    cfg.checkpoint.start_events = 6;
    cfg.checkpoint.every_events = 6;
}

/// `yangpad --archive`로 띄운 노드.
fn archive_node(cfg: &mut NodeConfig) {
    fast(cfg);
    cfg.archive.enabled = true;
}

/// 용량 상한이 작은 보관 노드(상한의 7/8부터 메시지 본문을 빼고 받음).
const SMALL_QUOTA: u64 = 96 * 1024;

fn small_archive_node(cfg: &mut NodeConfig) {
    archive_node(cfg);
    cfg.archive.quota_bytes = SMALL_QUOTA;
}

/// 시험마다 다른 평문 표식(다른 시험·이전 실행과 겹치지 않게).
fn marker(tag: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("YANGPA-ARCHIVE-{tag}-{nanos}")
}

async fn create_crew(a: &TestNode, name: &str) -> SpaceId {
    a.ok::<m::CrewCreate>(CrewCreateParams {
        name: name.into(),
        description: None,
    })
    .await
    .space
}

async fn join(gateway: &TestNode, joiner: &TestNode, space: SpaceId) {
    connect_both(gateway, joiner).await;
    let url = gateway
        .ok::<m::InviteCreate>(InviteCreateParams {
            space,
            expires_in_secs: None,
            max_uses: None,
            roles: Vec::new(),
        })
        .await
        .url;
    let r = joiner.ok::<m::InviteJoin>(UrlParams { url }).await;
    assert_eq!(r.space, space);
    assert_eq!(r.status, InviteJoinStatus::Joined);
    let me = joiner.user();
    eventually(&format!("{} 가입 완료", joiner.name), || async {
        let st = testkit::space_state(&joiner.node, &space)?;
        (st.is_member(&me) && testkit::crew_key_seq(&joiner.node, &space).is_some()).then_some(())
    })
    .await;
    eventually("같은 크루 키 순번", || async {
        let ea = testkit::crew_key_seq(&gateway.node, &space)?;
        (testkit::crew_key_seq(&joiner.node, &space)? == ea).then_some(())
    })
    .await;
}

/// 보관 노드가 제안 링크를 만들고 관리자가 지정합니다. 보관 노드가 공간을 받아 둘 때까지 기다립니다.
async fn designate(
    a: &TestNode,
    r: &TestNode,
    space: SpaceId,
    private_channels: Vec<ChannelId>,
) -> ArchiveView {
    connect(a, r).await;
    let url = r
        .ok::<m::ArchiveOffer>(ArchiveOfferParams {
            expires_in_secs: Some(600),
            name: Some("동아리 보관함".into()),
        })
        .await
        .url;
    let v = a
        .ok::<m::ArchiveDesignate>(ArchiveDesignateParams {
            space,
            url: url.clone(),
            private_channels,
            name: None,
        })
        .await;
    assert_eq!(Some(v.device), testkit::device_id(&r.node));
    assert_eq!(v.name, "동아리 보관함");
    eventually("보관 노드가 공간을 받아 둠", || async {
        at::is_hosted(&r.node, &space).then_some(())
    })
    .await;
    // 제안 링크는 한 번만 씁니다.
    let again = a
        .call::<m::ArchiveDesignate>(ArchiveDesignateParams {
            space,
            url,
            private_channels: Vec::new(),
            name: None,
        })
        .await;
    assert!(again.is_err(), "쓴 제안 링크로 다시 지정됨");
    v
}

/// 보관 노드가 이 이벤트를 본문까지 받아 둘 때까지.
async fn wait_archived(r: &TestNode, id: EventId, what: &str) {
    eventually(what, || async {
        (at::stored(&r.node, &id) == Some(true)).then_some(())
    })
    .await;
}

fn write_file(n: &TestNode, name: &str, bytes: &[u8]) -> String {
    let dir = n.dir.join("files");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, bytes).unwrap();
    assert!(Path::new(&path).is_absolute());
    path.to_string_lossy().into_owned()
}

/// 글과 파일 하나를 보냅니다. (메시지, 블롭).
async fn send_file(
    n: &TestNode,
    space: SpaceId,
    channel: ChannelId,
    text: &str,
    path: &str,
) -> (EventId, BlobId) {
    let id = n
        .ok::<m::MessageSend>(MessageSendParams {
            space,
            channel,
            text: text.into(),
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
    (id, view.attachments[0].blob)
}

/// 표식이 든 파일 내용.
fn file_with(marker: &str, len: usize) -> Vec<u8> {
    let mut v = marker.as_bytes().to_vec();
    while v.len() < len {
        v.extend_from_slice(marker.as_bytes());
        v.push(b'\n');
    }
    v.truncate(len);
    v
}

async fn fetch_and_read(n: &TestNode, space: SpaceId, blob: BlobId, key_from: EventId) -> Vec<u8> {
    n.ok::<m::BlobFetch>(BlobFetchParams {
        space,
        blob,
        priority: Some(200),
    })
    .await;
    eventually(&format!("{}가 첨부를 다 받음", n.name), || async {
        let s = n.ok::<m::BlobStatus>(BlobParams { space, blob }).await;
        (s.total_chunks > 0 && s.have_chunks == s.total_chunks).then_some(())
    })
    .await;
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

/// 보관 노드의 저장소·데이터 경로 어디에도 평문 표식이 없어야 합니다.
async fn assert_no_plaintext(r: &TestNode, marker: &str) {
    let leak = r
        .ok::<m::DiagLeakcheck>(LeakcheckParams {
            marker: marker.into(),
        })
        .await;
    assert!(leak.clean, "보관 노드에 평문이 있음: {:?}", leak.findings);
}

async fn request(from: &TestNode, to: &TestNode, req: YpRequest) -> YpResponse {
    testkit::net_request(&from.node, to.peer(), req)
        .await
        .expect("요청 전송")
}

fn heads(space: SpaceId, stream: StreamId) -> YpRequest {
    YpRequest::GetHeads { space, stream }
}

// ───────────── 1. 돌아온 멤버가 보관 노드에서만 따라잡음 ─────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn returning_member_catches_up_from_archive_alone() {
    let cl = Cluster::new();
    let a = cl.node_with("a", fast).await;
    let mut b = cl.node_with("b", fast).await;
    let r = cl.node_with("r", archive_node).await;
    let space = create_crew(&a, "보관 크루").await;
    let channel = a.first_channel(space).await;
    join(&a, &b, space).await;
    let early = a.send(space, channel, "지정 전 메시지").await;
    wait_message(
        &b,
        space,
        channel,
        early,
        "B가 지정 전 메시지를 받음",
        |_| true,
    )
    .await;

    designate(&a, &r, space, Vec::new()).await;
    let r_dev = testkit::device_id(&r.node).unwrap();
    eventually("B가 지정을 봄", || async {
        testkit::space_state(&b.node, &space)?
            .archives
            .contains_key(&r_dev)
            .then_some(())
    })
    .await;
    // 지정 전 기록은 보관 노드가 당겨 옵니다. 보관 노드는 멤버가 아니라 상태를 풀지 못합니다.
    wait_archived(&r, early, "보관 노드가 지정 전 메시지를 당겨 옴").await;
    assert!(testkit::space_state(&r.node, &space).is_none());
    assert!(!testkit::known_spaces(&r.node).contains(&space));
    // 시더 범위는 보관 노드를 따로 셉니다.
    eventually("보관 노드가 시더 범위에 잡힘", || async {
        a.ok::<m::SeedCoverage>(SpaceParams { space })
            .await
            .into_iter()
            .find(|c| c.channel == channel && c.online_archives == 1 && c.online_seeders >= 1)
    })
    .await;
    let list = a.ok::<m::ArchiveList>(SpaceParams { space }).await;
    assert_eq!(list.len(), 1);
    assert!(list[0].online);
    let hosted = r.ok::<m::ArchiveHosted>(yangpa_rpc::api::Empty).await;
    assert_eq!(hosted.len(), 1);
    assert_eq!(hosted[0].space, space);
    assert_eq!(hosted[0].members, 2);

    // B가 꺼진 사이 A가 표식 메시지와 첨부를 보냄.
    b.shutdown().await;
    let mark = marker("returning");
    let file = file_with(&mark, 70_000);
    let path = write_file(&a, "기록.bin", &file);
    let (msg, blob) = send_file(&a, space, channel, &mark, &path).await;
    wait_archived(&r, msg, "보관 노드가 표식 메시지를 받음").await;
    eventually("보관 노드가 첨부를 다 받음", || async {
        at::blob_complete(&r.node, &blob).then_some(())
    })
    .await;
    // 이제 A도 꺼짐: 기록은 보관 노드에만 있습니다.
    a.shutdown().await;

    b.start_again().await;
    connect(&b, &r).await;
    wait_message(
        &b,
        space,
        channel,
        msg,
        "B가 보관 노드에서 표식 메시지를 받아 풂",
        |v| v.text == mark,
    )
    .await;
    assert_eq!(fetch_and_read(&b, space, blob, msg).await, file);

    // 보관 노드는 내용을 읽을 수 없습니다. 같은 검사가 멤버 저장소에서는 표식을 찾습니다(검사가 동작한다는 대조).
    assert_no_plaintext(&r, &mark).await;
    let found = b
        .ok::<m::DiagLeakcheck>(LeakcheckParams {
            marker: mark.clone(),
        })
        .await;
    assert!(!found.clean, "대조: 멤버 저장소에서는 표식을 찾아야 함");
}

// ───────────── 2. 새 멤버가 과거 기록 공개 채널을 보관 노드에서 받음 ─────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn new_member_reads_public_history_from_archive() {
    let cl = Cluster::new();
    let a = cl.node_with("a", fast).await;
    let r = cl.node_with("r", archive_node).await;
    let c = cl.node_with("c", fast).await;
    let space = create_crew(&a, "기록 크루").await;
    // 채널 목록 순서는 위치가 같으면 ID 순이라, 새 채널을 만들기 전에 기본 채널을 잡아 둡니다.
    let general = a.first_channel(space).await;
    let history = a
        .ok::<m::ChannelCreate>(ChannelCreateParams {
            space,
            name: "기록".into(),
            kind: ChannelKind::Text,
            parent: None,
            topic: None,
            security: None,
            join_mode: None,
            ttl_secs: None,
            ttl_on_leave: None,
            history: Some(HistoryVisibility::Public),
            link_previews: None,
            private_roles: None,
            position: None,
            forum_tags: None,
            p2p: None,
            overwrites: None,
            anonymous: None,
            attach_limit: None,
            no_edit: None,
            no_delete: None,
        })
        .await
        .channel;
    let mark = marker("history");
    let before = a.send(space, history, &format!("{mark} 지정 전")).await;
    // 과거 기록 비공개(기본) 채널의 입장 전 메시지.
    let closed = a.send(space, general, &format!("{mark} 비공개 기록")).await;

    designate(&a, &r, space, Vec::new()).await;
    // 고르지 않은 비공개 채널은 보관하지 않습니다.
    let staff = a
        .ok::<m::RoleCreate>(RoleCreateParams {
            space,
            name: "운영진".into(),
            color: None,
            permissions: Vec::new(),
            rank: Some(5),
        })
        .await
        .role;
    let secret = a
        .ok::<m::ChannelCreate>(ChannelCreateParams {
            space,
            name: "운영".into(),
            kind: ChannelKind::Text,
            parent: None,
            topic: None,
            security: None,
            join_mode: None,
            ttl_secs: None,
            ttl_on_leave: None,
            history: None,
            link_previews: None,
            private_roles: Some(vec![staff]),
            position: None,
            forum_tags: None,
            p2p: None,
            overwrites: None,
            anonymous: None,
            attach_limit: None,
            no_edit: None,
            no_delete: None,
        })
        .await
        .channel;
    eventually("A가 비공개 채널 그룹을 만듦", || async {
        testkit::channel_key_seq(&a.node, &space, secret)
    })
    .await;
    let secret_msg = a.send(space, secret, &format!("{mark} 운영진만")).await;
    // 지정 뒤 메시지·첨부는 A가 바로 보냅니다.
    let file = file_with(&mark, 40_000);
    let path = write_file(&a, "자료.bin", &file);
    let (after, blob) = send_file(&a, space, history, &format!("{mark} 지정 뒤"), &path).await;
    wait_archived(&r, before, "보관 노드가 지정 전 기록을 받음").await;
    wait_archived(&r, closed, "보관 노드가 기본 채널 기록을 받음").await;
    wait_archived(&r, after, "보관 노드가 지정 뒤 기록을 받음").await;
    eventually("보관 노드가 첨부를 다 받음", || async {
        at::blob_complete(&r.node, &blob).then_some(())
    })
    .await;
    assert_eq!(
        at::stored(&r.node, &secret_msg),
        None,
        "고르지 않은 비공개 채널이 보관됨"
    );

    // A는 이 채널을 더 제공하지 않습니다: 새 멤버는 이 채널 기록을 보관 노드에서만 받을 수 있습니다.
    a.ok::<m::SeedSet>(SeedSetParams {
        space: Some(space),
        exclude_channels: Some(vec![history]),
        ..Default::default()
    })
    .await;
    join(&a, &c, space).await;
    let refused = request(&c, &a, heads(space, StreamId::Channel(history))).await;
    assert!(
        matches!(refused, YpResponse::Error { .. }),
        "A가 뺀 채널을 줌: {refused:?}"
    );
    connect(&c, &r).await;
    eventually("명단에 새 멤버가 들어감", || async {
        at::roster_members(&r.node, &space)?
            .contains(&c.user())
            .then_some(())
    })
    .await;
    // 가입자 받은편지함의 키 묶음은 명단이 가입자를 싣기 전에 와도 보관합니다(확정 기기가 한 번만 밀어 줌).
    let inbox = testkit::stream_events(&a.node, &space, StreamId::Inbox(c.user())).unwrap();
    assert!(
        !inbox.is_empty(),
        "확정 기기가 가입자 받은편지함에 키를 봉인함"
    );
    for e in &inbox {
        wait_archived(&r, e.id(), "보관 노드가 가입자 받은편지함을 받음").await;
    }
    for (id, what) in [(before, "지정 전"), (after, "지정 뒤")] {
        let want = format!("{mark} {what}");
        wait_message(
            &c,
            space,
            history,
            id,
            &format!("C가 보관 노드에서 {what} 기록을 받아 풂"),
            |v| v.text == want,
        )
        .await;
    }
    assert_eq!(fetch_and_read(&c, space, blob, after).await, file);
    // 과거 기록 비공개 채널의 입장 전 메시지는 보관 노드도 새 멤버에게 주지 않습니다.
    let since = at::roster_since(&r.node, &space, &c.user());
    assert!(
        matches!(since, Some(Some(_))),
        "명단에 새 멤버의 입장 시점이 없음: {since:?}"
    );
    let resp = request(
        &c,
        &r,
        YpRequest::GetEvents {
            space,
            ids: vec![closed],
        },
    )
    .await;
    assert!(
        matches!(
            resp,
            YpResponse::Error {
                code: WireErrorCode::HistoryPrivate,
                ..
            }
        ),
        "since={since:?} {resp:?}"
    );
    assert_no_plaintext(&r, &mark).await;
}

// ───────────── 3. 거절·삭제 반영·해제 ─────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn archive_refuses_outsiders_and_stops_after_revoke() {
    let cl = Cluster::new();
    let a = cl.node_with("a", fast).await;
    let b = cl.node_with("b", fast).await;
    let x = cl.node_with("x", fast).await;
    let s = cl.node_with("s", fast).await;
    let r = cl.node_with("r", archive_node).await;
    let space = create_crew(&a, "거절 크루").await;
    let channel = a.first_channel(space).await;
    join(&a, &b, space).await;
    join(&a, &x, space).await;
    let mark = marker("refuse");
    designate(&a, &r, space, Vec::new()).await;
    let m1 = a.send(space, channel, &format!("{mark} 첫 메시지")).await;
    let m2 = a.send(space, channel, &format!("{mark} 지울 메시지")).await;
    wait_archived(&r, m1, "보관 노드가 첫 메시지를 받음").await;
    wait_archived(&r, m2, "보관 노드가 지울 메시지를 받음").await;

    // 멤버가 아닌 노드: 인증도, 요청도 거절.
    connect(&s, &r).await;
    assert!(
        testkit::authenticate(&s.node, r.peer()).await.is_err(),
        "멤버가 아닌 노드를 인증함"
    );
    let resp = request(&s, &r, heads(space, StreamId::Space)).await;
    assert!(matches!(resp, YpResponse::Error { .. }), "{resp:?}");

    // 멤버 X는 받다가, 밴되면 거절됩니다.
    connect(&x, &r).await;
    testkit::authenticate(&x.node, r.peer())
        .await
        .expect("명단의 멤버 인증");
    let resp = request(&x, &r, heads(space, StreamId::Channel(channel))).await;
    assert!(matches!(resp, YpResponse::Heads { .. }), "{resp:?}");
    a.ok::<m::MemberBan>(MemberBanParams {
        space,
        user: x.user(),
        reason: None,
        delete_recent: false,
    })
    .await;
    eventually("명단에서 밴된 멤버가 빠짐", || async {
        (!at::roster_members(&r.node, &space)?.contains(&x.user())).then_some(())
    })
    .await;
    eventually("밴된 멤버의 요청 거절", || async {
        let resp = request(&x, &r, heads(space, StreamId::Channel(channel))).await;
        matches!(resp, YpResponse::Error { .. }).then_some(())
    })
    .await;
    assert!(
        testkit::reauth(&x.node, r.peer()).await.is_err(),
        "밴된 멤버를 다시 인증함"
    );

    // 지운 메시지는 보관 노드에서도 본문이 지워집니다.
    a.ok::<m::MessageDelete>(MessageRefParams {
        space,
        channel,
        id: m2,
    })
    .await;
    eventually("보관 노드가 지운 메시지 본문을 지움", || async {
        (at::stored(&r.node, &m2) == Some(false)).then_some(())
    })
    .await;
    eventually("삭제 반영을 다 보냄", || async {
        (at::pending_counts(&a.node, &space).0 == 0).then_some(())
    })
    .await;

    // 해제: 보관 노드가 받아 둔 것을 지우고, 멤버는 더 쓰지 않습니다.
    connect(&b, &r).await;
    eventually("B가 보관 노드와 인증됨", || async {
        (at::online_archives(&b.node, &space) == 1).then_some(())
    })
    .await;
    let r_dev = testkit::device_id(&r.node).unwrap();
    a.ok::<m::ArchiveRevoke>(ArchiveRevokeParams {
        space,
        device: r_dev,
    })
    .await;
    eventually("보관 노드가 해제를 받아 지움", || async {
        (!at::is_hosted(&r.node, &space) && at::event_count(&r.node, &space) == 0).then_some(())
    })
    .await;
    eventually("해제 알림을 다 보냄", || async {
        (at::pending_counts(&a.node, &space).1 == 0).then_some(())
    })
    .await;
    eventually("멤버가 보관 노드를 더 쓰지 않음", || async {
        (at::online_archives(&b.node, &space) == 0 && at::online_archives(&a.node, &space) == 0)
            .then_some(())
    })
    .await;
    assert!(
        a.ok::<m::ArchiveList>(SpaceParams { space })
            .await
            .is_empty()
    );
    // 해제 뒤 메시지는 보관 노드로 가지 않고, 보관 노드의 요청도 거절됩니다.
    let m3 = a.send(space, channel, &format!("{mark} 해제 뒤")).await;
    wait_message(
        &b,
        space,
        channel,
        m3,
        "B가 해제 뒤 메시지를 받음",
        |_| true,
    )
    .await;
    assert_eq!(at::event_count(&r.node, &space), 0);
    assert_eq!(at::stored(&r.node, &m3), None);
    let resp = request(&r, &a, heads(space, StreamId::Space)).await;
    assert!(matches!(resp, YpResponse::Error { .. }), "{resp:?}");
    assert!(
        r.ok::<m::ArchiveHosted>(yangpa_rpc::api::Empty)
            .await
            .is_empty()
    );
    assert_no_plaintext(&r, &mark).await;
}

// ───────────── 도우미(역할) ─────────────

/// 권한을 가진 역할을 만들어 멤버에게 주고, 그 멤버가 권한을 볼 때까지 기다립니다.
async fn grant_role(
    a: &TestNode,
    member: &TestNode,
    space: SpaceId,
    perms: &[&str],
    need: Permissions,
) {
    let role: RoleId = a
        .ok::<m::RoleCreate>(RoleCreateParams {
            space,
            name: format!("역할-{}", perms.join("-")),
            color: None,
            permissions: perms.iter().map(|p| (*p).to_owned()).collect(),
            rank: Some(5),
        })
        .await
        .role;
    a.ok::<m::MemberSetRoles>(MemberSetRolesParams {
        space,
        user: member.user(),
        roles: vec![role],
    })
    .await;
    let me = member.user();
    eventually(&format!("{}가 역할을 봄", member.name), || async {
        testkit::space_state(&member.node, &space)?
            .base_permissions(&me)
            .contains(need)
            .then_some(())
    })
    .await;
}

async fn create_text_channel(n: &TestNode, space: SpaceId, name: &str) -> ChannelId {
    n.ok::<m::ChannelCreate>(ChannelCreateParams {
        space,
        name: name.into(),
        kind: ChannelKind::Text,
        parent: None,
        topic: None,
        security: None,
        join_mode: None,
        ttl_secs: None,
        ttl_on_leave: None,
        history: None,
        link_previews: None,
        private_roles: None,
        position: None,
        forum_tags: None,
        p2p: None,
        overwrites: None,
        anonymous: None,
        attach_limit: None,
        no_edit: None,
        no_delete: None,
    })
    .await
    .channel
}

// ───────────── 4. 재시작한 소유자의 오래된 명단이 채널을 지우지 못함 ─────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restarted_owner_does_not_erase_newer_channel() {
    let cl = Cluster::new();
    let mut a = cl.node_with("a", fast).await;
    let b = cl.node_with("b", fast).await;
    let r = cl.node_with("r", archive_node).await;
    let space = create_crew(&a, "재시작 크루").await;
    join(&a, &b, space).await;
    grant_role(
        &a,
        &b,
        space,
        &["manage_space", "manage_channels"],
        Permissions::MANAGE_SPACE,
    )
    .await;
    designate(&a, &r, space, Vec::new()).await;
    connect(&b, &r).await;
    eventually("B가 보관 노드와 인증됨", || async {
        (at::online_archives(&b.node, &space) == 1).then_some(())
    })
    .await;

    // 소유자가 꺼진 사이 관리자 B가 공개 채널을 만들고 글을 씀: 보관 노드는 B의 명단으로 그 채널을 받아 둠.
    a.shutdown().await;
    let mark = marker("restart");
    let newer = create_text_channel(&b, space, "새 채널").await;
    let msg = b.send(space, newer, &format!("{mark} 새 채널 글")).await;
    eventually("보관 노드의 명단에 새 채널이 들어감", || async {
        at::roster_channels(&r.node, &space)?
            .contains(&newer)
            .then_some(())
    })
    .await;
    wait_archived(&r, msg, "보관 노드가 새 채널 글을 받음").await;
    // B가 꺼지기 전에 B가 가진 공간·새 채널 스트림(키 묶음 포함)을 보관 노드가 모두 받아야 A가 보관 노드만으로 읽습니다.
    for stream in [StreamId::Space, StreamId::Channel(newer)] {
        eventually("보관 노드가 B의 스트림을 다 받음", || async {
            let have: std::collections::HashSet<EventId> = at::stream(&r.node, &space, stream)
                .iter()
                .map(|e| e.id())
                .collect();
            at::stream(&b.node, &space, stream)
                .iter()
                .all(|e| have.contains(&e.id()))
                .then_some(())
        })
        .await;
    }
    b.shutdown().await;
    let before = r.ok::<m::ArchiveHosted>(yangpa_rpc::api::Empty).await[0].roster_issued_at;

    // 소유자가 다시 켜짐: 따라잡기 전 상태에는 새 채널이 없지만, 명단을 보내기 전에 보관 노드와 공간 스트림을 맞춥니다.
    a.start_again().await;
    connect(&a, &r).await;
    eventually("A가 보관 노드에서 새 채널을 받음", || async {
        testkit::space_state(&a.node, &space)?
            .channels
            .contains_key(&newer)
            .then_some(())
    })
    .await;
    wait_message(
        &a,
        space,
        newer,
        msg,
        "A가 보관 노드에서 새 채널 글을 받음",
        |v| v.text.starts_with(&mark),
    )
    .await;
    // 소유자의 명단이 새 채널을 넣어 받아질 때까지(따라잡는 사이 빠진 명단이 먼저 와도 채널은 지우지 않고 기다림).
    eventually("소유자의 새 명단을 받음", || async {
        let h = r.ok::<m::ArchiveHosted>(yangpa_rpc::api::Empty).await;
        (h[0].roster_issued_at > before && at::roster_channels(&r.node, &space)?.contains(&newer))
            .then_some(())
    })
    .await;
    assert_eq!(
        at::stored(&r.node, &msg),
        Some(true),
        "새 채널 본문이 지워짐"
    );
    assert_no_plaintext(&r, &mark).await;
}

// ───────────── 5. 권한자가 없을 때 밴 권한자의 밴이 보관 노드에 닿음 ─────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn moderator_ban_reaches_archive_without_authority() {
    let cl = Cluster::new();
    let a = cl.node_with("a", fast).await;
    let b = cl.node_with("b", fast).await;
    let x = cl.node_with("x", fast).await;
    let r = cl.node_with("r", archive_node).await;
    let space = create_crew(&a, "밴 크루").await;
    let channel = a.first_channel(space).await;
    join(&a, &b, space).await;
    join(&a, &x, space).await;
    grant_role(&a, &b, space, &["ban"], Permissions::BAN_MEMBERS).await;
    designate(&a, &r, space, Vec::new()).await;
    for n in [&b, &x] {
        connect(n, &r).await;
        eventually(&format!("{}가 보관 노드와 인증됨", n.name), || async {
            (at::online_archives(&n.node, &space) == 1).then_some(())
        })
        .await;
    }
    let resp = request(&x, &r, heads(space, StreamId::Channel(channel))).await;
    assert!(matches!(resp, YpResponse::Heads { .. }), "{resp:?}");

    // 권한자(소유자)는 꺼져 있고, 밴 권한만 있는 B가 X를 밴함.
    a.shutdown().await;
    b.ok::<m::MemberBan>(MemberBanParams {
        space,
        user: x.user(),
        reason: None,
        delete_recent: false,
    })
    .await;
    eventually("명단에서 밴된 멤버가 빠짐", || async {
        (!at::roster_members(&r.node, &space)?.contains(&x.user())).then_some(())
    })
    .await;
    assert!(
        at::roster_members(&r.node, &space)
            .unwrap()
            .contains(&b.user()),
        "밴한 멤버가 빠짐"
    );
    eventually("밴된 멤버의 요청 거절", || async {
        let resp = request(&x, &r, heads(space, StreamId::Channel(channel))).await;
        matches!(resp, YpResponse::Error { .. }).then_some(())
    })
    .await;
}

// ───────────── 6. 본문을 빼고 받은 이벤트는 내주지 않음, 지우면 사용량이 줆 ─────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn archive_withholds_stripped_events_and_recounts_usage() {
    let cl = Cluster::new();
    let a = cl.node_with("a", fast).await;
    let b = cl.node_with("b", fast).await;
    let r = cl.node_with("r", small_archive_node).await;
    let space = create_crew(&a, "용량 크루").await;
    let channel = a.first_channel(space).await;
    join(&a, &b, space).await;
    designate(&a, &r, space, Vec::new()).await;
    connect(&b, &r).await;
    testkit::authenticate(&b.node, r.peer())
        .await
        .expect("명단의 멤버 인증");

    // 상한을 넘을 때까지 보냄: 넘은 메시지는 헤더만 받아 두고 내주지 않습니다.
    let mark = marker("quota");
    // 본문은 압축해 보내므로, 용량을 채우는 군글은 줄지 않는 글자로. 지운 뒤 줄어드는 양(본문)이 삭제 이벤트(헤더·혼합
    // 서명 약 2KiB)보다 넉넉히 크도록 길게.
    let filler: String = common::noise(6000, 5)
        .into_iter()
        .map(|b| char::from(b % 94 + 33))
        .collect();
    let mut kept = Vec::new();
    let mut stripped = None;
    for i in 0..200 {
        let id = a
            .send(space, channel, &format!("{mark} {i} {filler}"))
            .await;
        eventually("보관 노드가 메시지를 받음", || async {
            at::stored(&r.node, &id).map(|_| ())
        })
        .await;
        if at::seed_excluded(&r.node, &id) {
            stripped = Some(id);
            break;
        }
        assert_eq!(at::stored(&r.node, &id), Some(true));
        kept.push(id);
    }
    let stripped = stripped.expect("용량 상한에 닿지 않음");
    assert!(!kept.is_empty());
    assert_eq!(at::stored(&r.node, &stripped), Some(false));
    let used = at::used_bytes(&r.node, &space).unwrap();
    assert!(used <= SMALL_QUOTA, "용량 상한을 넘음: {used}");
    let resp = request(
        &b,
        &r,
        YpRequest::GetEvents {
            space,
            ids: vec![stripped, kept[0]],
        },
    )
    .await;
    let YpResponse::Events(evs) = resp else {
        panic!("이벤트 응답이 아님: {resp:?}");
    };
    assert_eq!(evs.len(), 1, "헤더만 받아 둔 메시지를 내줌");
    assert_eq!(evs[0].id(), kept[0]);
    assert!(evs[0].body.is_some());

    // 지운 메시지의 본문은 사용량에서 빠집니다(저장소에서 다시 셈).
    a.ok::<m::MessageDelete>(MessageRefParams {
        space,
        channel,
        id: kept[0],
    })
    .await;
    eventually("보관 노드가 지운 본문을 지움", || async {
        (at::stored(&r.node, &kept[0]) == Some(false)).then_some(())
    })
    .await;
    eventually("사용량이 줆", || async {
        let now = at::used_bytes(&r.node, &space)?;
        (now + 1500 < used).then_some(())
    })
    .await;
    assert_no_plaintext(&r, &mark).await;
}

// ───────────── 체크포인트·키 그룹과 함께 ─────────────

/// 체크포인트가 있는 크루: 권한자는 바탕 명단 위 차이만 보내고, 가입을 받아 준 멤버의 차이로 가입자가 바로 명단에
/// 들며, 스냅숏 알림으로 보관 노드가 스냅숏 블롭을 받아 둡니다. 확정 기기가 가입자 받은편지함에 넣은 키 묶음도 보관
/// 노드가 받아 두고(명단에 든 사용자의 받은편지함), 가입한 뒤 꺼졌던 가입자는 보관 노드에서만 그 묶음과 체크포인트 뒤
/// 기록을 따라잡아 지금 크루 키로 읽습니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn archive_keeps_roster_deltas_snapshots_and_joiner_inbox_with_checkpoints() {
    use yangpa_daemon::testkit::checkpoint as ct;
    let cl = Cluster::new();
    let a = cl.node_with("a", fast_checkpoints).await;
    let r = cl.node_with("r", archive_node).await;
    let mut b = cl.node_with("b", fast_checkpoints).await;
    ct::hold(&a.node, true);
    let space = create_crew(&a, "체크포인트 보관 크루").await;
    let channel = a.first_channel(space).await;
    designate(&a, &r, space, Vec::new()).await;
    for i in 0..30 {
        a.ok::<m::CrewUpdate>(yangpa_rpc::api::CrewUpdateParams {
            space,
            name: None,
            description: Some(format!("설명 {i}")),
            icon_path: None,
            strict_rotation: None,
            emoji_limits: None,
        })
        .await;
    }
    ct::hold(&a.node, false);
    let snap = eventually("체크포인트와 스냅숏 블롭", || async {
        ct::saved(&a.node, &space).map(|(_, snap, _)| snap)
    })
    .await;
    ct::hold(&a.node, true);
    eventually("보관 노드가 스냅숏을 받아 둠", || async {
        (at::snapshots(&r.node, &space).contains(&snap) && at::blob_complete(&r.node, &snap))
            .then_some(())
    })
    .await;

    // 체크포인트로 가입: 받아 준 A가 명단 차이로 B를 넣고, B 받은편지함의 키 묶음이 보관 노드로.
    join(&a, &b, space).await;
    assert!(ct::base(&b.node, &space).is_some(), "체크포인트로 들어옴");
    eventually("보관 노드 명단에 B", || async {
        at::roster_members(&r.node, &space)?
            .contains(&b.user())
            .then_some(())
    })
    .await;
    let inbox = testkit::stream_events(&a.node, &space, StreamId::Inbox(b.user())).unwrap();
    assert!(!inbox.is_empty(), "가입자 받은편지함");
    for ev in &inbox {
        wait_archived(&r, ev.id(), "보관 노드가 가입자 키 묶음을 받음").await;
    }

    // B가 꺼진 사이 A가 쓴 메시지를, A도 꺼진 뒤 보관 노드에서만 받아 지금 크루 키로 읽음.
    b.shutdown().await;
    let mark = marker("checkpoint");
    let msg = a.send(space, channel, &mark).await;
    wait_archived(&r, msg, "보관 노드가 메시지를 받음").await;
    a.shutdown().await;
    b.start_again().await;
    connect(&b, &r).await;
    wait_message(
        &b,
        space,
        channel,
        msg,
        "B가 보관 노드에서 메시지를 받아 풂",
        |v| v.text == mark,
    )
    .await;
    assert_no_plaintext(&r, &mark).await;
}
