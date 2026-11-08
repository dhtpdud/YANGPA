//! 관리·기기 영역 통합 시험(B4): API 토큰 범위·공간 제한·폐기, 자동화 토큰의 사람 확인(승인·거절·만료, 휘발성
//! 대상은 RAM에만), 감사 로그(등급별 보관), 프로필 전파·크루 별명, 개인 메모의 내 기기 동기화, 복구한 새 기기의
//! 합류(내 기기·친구 수락)와 모르는 노드에 대한 비공개, 기기 목록 상한, 기기 폐기, 친구 삭제.
//!
//! 노드는 한 프로세스 안에서 127.0.0.1로 직접 연결합니다(`common` 참고). 고정 sleep으로 맞추지 않고 조건이 될 때까지
//! 기다립니다.

use crate::common;

use std::time::Duration;

use common::{
    Cluster, TestNode, befriend, connect, connect_both, eventually, next_notification, wait_message,
};
use yangpa_core::event::{EventKind, StreamId};
use yangpa_core::ids::{ChannelId, DeviceId, EventId, SpaceId, UserId};
use yangpa_core::wire::{SocialRequest, WireErrorCode, YpRequest, YpResponse};
use yangpa_daemon::{ApiHandler, CallCtx, testkit};
use yangpa_rpc::api::{
    AuditListParams, DeviceRevokeParams, Empty, IdParams, IdentityCreateParams, MessageListParams,
    MessageRefParams, MessageSendParams, Method, NoteSetParams, Notification, ProfileGetParams,
    ProfileSetCrewParams, ProfileSetParams, Scope, TokenCreateParams, UserParams, m,
};
use yangpa_rpc::jsonrpc::{
    ERR_AUTH_REQUIRED, ERR_CONFIRMATION_REQUIRED, ERR_CONFLICT, ERR_FORBIDDEN, ERR_INVALID_INPUT,
    ERR_NOT_FOUND, ERR_OUT_OF_SCOPE,
};
use yangpa_rpc::{RpcError, RpcHandler};

// ───────────── 도우미 ─────────────

/// 소유자가 토큰을 만들고, 그 토큰 원문으로 인증한 호출 맥락을 돌려줍니다(ID, 원문, 맥락).
async fn token(
    n: &TestNode,
    name: &str,
    scopes: Vec<Scope>,
    spaces: Vec<SpaceId>,
    automation: bool,
) -> (String, String, CallCtx) {
    let r = n
        .ok::<m::TokenCreate>(TokenCreateParams {
            name: name.into(),
            scopes,
            spaces,
            automation: Some(automation),
        })
        .await;
    let secret = r.token.expose().to_owned();
    let auth = ApiHandler::new(n.node.clone())
        .authenticate(&secret)
        .await
        .expect("새 토큰으로 인증");
    (r.id, secret, CallCtx::from_auth(auth))
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

fn code<T: std::fmt::Debug>(r: Result<T, RpcError>) -> i64 {
    r.expect_err("오류여야 합니다").code
}

fn list_params(space: SpaceId, channel: ChannelId) -> MessageListParams {
    MessageListParams {
        space,
        channel,
        before: None,
        limit: Some(200),
        thread: None,
    }
}

fn send_params(space: SpaceId, channel: ChannelId, text: &str) -> MessageSendParams {
    MessageSendParams {
        space,
        channel,
        text: text.into(),
        attachments: Vec::new(),
        reply_to: None,
        thread: None,
    }
}

/// 그 메시지를 이 노드가 그 글로 읽을 수 있는지(목록을 못 읽어도 거짓).
async fn readable(
    n: &TestNode,
    space: SpaceId,
    channel: ChannelId,
    id: EventId,
    text: &str,
) -> bool {
    n.call::<m::MessageList>(list_params(space, channel))
        .await
        .map(|r| r.messages.iter().any(|v| v.id == id && v.text == text))
        .unwrap_or(false)
}

/// `a`가 만든 크루에 `c`가 멤버로 있고, 서로 인증해 크루 키 순번이 같아진 상태.
async fn crew_with_member(cl: &Cluster) -> (TestNode, TestNode, SpaceId, ChannelId) {
    let a = cl.node("a").await;
    let c = cl.node("c").await;
    let space = testkit::seed_crew(&a.node, std::slice::from_ref(&c.node), "시험 크루")
        .await
        .expect("크루 만들기");
    connect_both(&a, &c).await;
    testkit::authenticate(&a.node, c.peer())
        .await
        .expect("크루 멤버끼리 인증");
    eventually("크루 멤버 크루 키 순번", || async {
        let ea = testkit::crew_key_seq(&a.node, &space)?;
        let ec = testkit::crew_key_seq(&c.node, &space)?;
        (ea == ec).then_some(())
    })
    .await;
    let channel = a.first_channel(space).await;
    (a, c, space, channel)
}

/// 친구 B와 1:1 공간이 있는 A에, 같은 신원의 새 기기 A2를 들여 그 공간에 합류시킵니다.
struct TwoDevices {
    a1: TestNode,
    a2: TestNode,
    b: TestNode,
    space: SpaceId,
    channel: ChannelId,
    a2_dev: DeviceId,
}

async fn two_devices(cl: &Cluster) -> TwoDevices {
    let a1 = cl.node("a1").await;
    a1.ok::<m::ProfileSet>(ProfileSetParams {
        bio: Some("예전 소개".into()),
        ..Default::default()
    })
    .await;
    let b = cl.node("b").await;
    let space = befriend(&a1, &b).await;
    let channel = a1.first_channel(space).await;
    let a2 = common::linked(cl, &a1, "a2", |_| {}).await;
    let a2_dev = testkit::device_id(&a2.node).expect("새 기기 ID");
    common::wait_own_device(&a1, a2_dev).await;
    eventually("친구가 새 기기를 앎", || async {
        testkit::device_list(&b.node, &a1.user())?
            .contains_active(&a2_dev)
            .then_some(())
    })
    .await;
    eventually("새 기기가 1:1 공간 키 그룹에 들어감", || async {
        let members = testkit::crew_key_members(&b.node, &space)?;
        let e2 = testkit::crew_key_seq(&a2.node, &space)?;
        let eb = testkit::crew_key_seq(&b.node, &space)?;
        (members.contains(&a2_dev) && e2 == eb).then_some(())
    })
    .await;
    connect_both(&a2, &b).await;
    testkit::authenticate(&a2.node, b.peer())
        .await
        .expect("새 기기와 친구 인증");
    TwoDevices {
        a1,
        a2,
        b,
        space,
        channel,
        a2_dev,
    }
}

// ───────────── 토큰 ─────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn token_scopes_and_space_limits() {
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let s1 = testkit::seed_crew(&a.node, &[], "크루 하나")
        .await
        .expect("크루 하나");
    let s2 = testkit::seed_crew(&a.node, &[], "크루 둘")
        .await
        .expect("크루 둘");
    let c1 = a.first_channel(s1).await;
    let c2 = a.first_channel(s2).await;

    // 읽기 전용, 공간 하나로 제한.
    let (read_id, read_secret, read) =
        token(&a, "읽기 봇", vec![Scope::Read], vec![s1], false).await;
    call_as::<m::MessageList>(&a, &read, list_params(s1, c1))
        .await
        .expect("허용 공간 읽기");
    assert_eq!(
        code(call_as::<m::MessageList>(&a, &read, list_params(s2, c2)).await),
        ERR_OUT_OF_SCOPE
    );
    assert_eq!(
        code(call_as::<m::MessageSend>(&a, &read, send_params(s1, c1, "x")).await),
        ERR_OUT_OF_SCOPE
    );
    // 공간 제한 토큰은 전역 작업을 못 함.
    assert_eq!(
        code(call_as::<m::FriendList>(&a, &read, Empty).await),
        ERR_OUT_OF_SCOPE
    );

    // 읽기·쓰기, 공간 제한 없음.
    let (_, _, write) = token(
        &a,
        "쓰기 봇",
        vec![Scope::Read, Scope::Write],
        Vec::new(),
        false,
    )
    .await;
    call_as::<m::MessageSend>(&a, &write, send_params(s2, c2, "봇이 보낸 비밀 글"))
        .await
        .expect("쓰기 범위로 보내기");
    // 설정·소유자 범위가 없으면 토큰을 다루지 못함.
    assert_eq!(
        code(call_as::<m::TokenList>(&a, &write, Empty).await),
        ERR_OUT_OF_SCOPE
    );
    assert_eq!(
        code(
            call_as::<m::TokenCreate>(
                &a,
                &write,
                TokenCreateParams {
                    name: "몰래".into(),
                    scopes: vec![Scope::Owner],
                    spaces: Vec::new(),
                    automation: None,
                }
            )
            .await
        ),
        ERR_OUT_OF_SCOPE
    );

    // 잘못된 발급 요청.
    let bad = |scopes: Vec<Scope>, spaces: Vec<SpaceId>, automation: bool| TokenCreateParams {
        name: "잘못".into(),
        scopes,
        spaces,
        automation: Some(automation),
    };
    assert_eq!(
        code(
            a.call::<m::TokenCreate>(bad(vec![Scope::Owner], Vec::new(), true))
                .await
        ),
        ERR_INVALID_INPUT
    );
    assert_eq!(
        code(
            a.call::<m::TokenCreate>(bad(Vec::new(), Vec::new(), false))
                .await
        ),
        ERR_INVALID_INPUT
    );
    assert_eq!(
        code(
            a.call::<m::TokenCreate>(bad(vec![Scope::Read], vec![SpaceId::random()], false))
                .await
        ),
        ERR_NOT_FOUND
    );

    // 목록에는 원문이 없음.
    let list = a.ok::<m::TokenList>(Empty).await;
    assert!(list.iter().any(|t| t.id == read_id
        && t.name == "읽기 봇"
        && t.spaces == vec![s1]
        && t.scopes == vec![Scope::Read]
        && !t.automation));
    let json = serde_json::to_string(&list).expect("직렬화");
    assert!(!json.contains(&read_secret), "토큰 목록에 원문이 나옴");

    // 폐기: 새 인증도, 이미 붙은 연결의 다음 호출도 막힘.
    a.ok::<m::TokenRevoke>(IdParams {
        id: read_id.clone(),
    })
    .await;
    assert!(
        ApiHandler::new(a.node.clone())
            .authenticate(&read_secret)
            .await
            .is_none()
    );
    assert_eq!(
        code(call_as::<m::MessageList>(&a, &read, list_params(s1, c1)).await),
        ERR_AUTH_REQUIRED
    );
    assert_eq!(
        code(a.call::<m::TokenRevoke>(IdParams { id: read_id }).await),
        ERR_NOT_FOUND
    );
    assert!(
        !a.ok::<m::TokenList>(Empty)
            .await
            .iter()
            .any(|t| t.name == "읽기 봇")
    );
    assert_eq!(
        code(
            a.call::<m::TokenRevoke>(IdParams { id: "owner".into() })
                .await
        ),
        ERR_INVALID_INPUT
    );

    // 감사 로그: 메서드·토큰 이름·결과만, 최신순. 내용은 없음.
    let audit = a.ok::<m::AuditList>(AuditListParams::default()).await;
    assert!(
        audit
            .iter()
            .any(|e| e.method == "message.send" && e.token_name == "쓰기 봇" && e.ok)
    );
    assert!(audit.iter().any(|e| e.method == "message.list"
        && e.token_name == "읽기 봇"
        && !e.ok
        && e.code == Some(ERR_AUTH_REQUIRED)));
    assert!(audit.windows(2).all(|w| w[0].id > w[1].id), "최신순이 아님");
    let json = serde_json::to_string(&audit).expect("직렬화");
    assert!(
        !json.contains("봇이 보낸 비밀 글"),
        "감사 로그에 내용이 남음"
    );
    // 쪽 나누기.
    let first = audit.first().expect("감사 로그 있음").id;
    let older = a
        .ok::<m::AuditList>(AuditListParams {
            limit: Some(2),
            before: Some(first),
        })
        .await;
    assert_eq!(older.len(), 2);
    assert!(older.iter().all(|e| e.id < first));
}

/// 데몬은 받은 경로의 아무 파일이나 읽고 써서, 소유자가 아닌 토큰이 `owner.token`을 첨부로 올려 읽으면 소유자가
/// 됩니다. 그래서 경로 인자는 소유자 토큰만 넘기고, 이미지를 지우는 빈 경로는 막지 않습니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn file_paths_need_the_owner_token() {
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let space = testkit::seed_crew(&a.node, &[], "크루")
        .await
        .expect("크루");
    let channel = a.first_channel(space).await;
    let dir = cl.dir("files");
    std::fs::create_dir_all(&dir).expect("시험 경로");
    let file = dir.join("메모.txt");
    std::fs::write(&file, "비밀").expect("시험 파일");
    let path = file.to_string_lossy().into_owned();
    let send = serde_json::json!({
        "space": space,
        "channel": channel,
        "text": "",
        "attachments": [{ "path": path }],
    });
    let (_, _, write) = token(
        &a,
        "쓰기 봇",
        vec![Scope::Read, Scope::Write],
        Vec::new(),
        false,
    )
    .await;
    let (_, _, bot) = token(&a, "AI", vec![Scope::Read, Scope::Write], Vec::new(), true).await;
    for ctx in [&write, &bot] {
        for (method, p) in [
            ("message.send", send.clone()),
            ("profile.set", serde_json::json!({ "avatar_path": path })),
            (
                "blob.save",
                serde_json::json!({ "path": dir.join("새 파일").to_string_lossy() }),
            ),
        ] {
            let e = a.node.call(ctx, method, p).await.expect_err(method);
            assert_eq!(e.code, ERR_OUT_OF_SCOPE, "{method}: {e:?}");
        }
    }
    a.node
        .call(
            &write,
            "profile.set",
            serde_json::json!({ "avatar_path": "" }),
        )
        .await
        .expect("빈 경로는 이미지 지우기");
    a.node
        .call(&CallCtx::owner(), "message.send", send)
        .await
        .expect("소유자는 첨부 경로를 넘김");
}

// ───────────── 사람 확인 ─────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn automation_destructive_call_runs_once_after_human_approval() {
    let cl = Cluster::new();
    let (a, c, space, channel) = crew_with_member(&cl).await;
    let target = c.send(space, channel, "지울 메시지 본문").await;
    wait_message(
        &a,
        space,
        channel,
        target,
        "크루장이 멤버 메시지를 받음",
        |_| true,
    )
    .await;

    let (_, _, bot) = token(
        &a,
        "정리 봇",
        vec![Scope::Read, Scope::Write],
        vec![space],
        true,
    )
    .await;
    let mut rx = a.subscribe();
    let err = call_as::<m::MessageDelete>(
        &a,
        &bot,
        MessageRefParams {
            space,
            channel,
            id: target,
        },
    )
    .await
    .expect_err("사람 확인이 필요해야 함");
    assert_eq!(err.code, ERR_CONFIRMATION_REQUIRED);
    let cid = err
        .data
        .as_ref()
        .and_then(|d| d.get("confirmation_id"))
        .and_then(|v| v.as_str())
        .expect("confirmation_id")
        .to_owned();
    let pending = next_notification(&mut rx, "확인 요청 알림", |n| match n {
        Notification::ConfirmPending(p) => Some(p.clone()),
        _ => None,
    })
    .await;
    assert_eq!(pending.id, cid);
    assert_eq!(pending.token_name, "정리 봇");
    assert_eq!(pending.space, Some(space));
    assert!(
        pending.summary.contains("남의 메시지 삭제"),
        "{}",
        pending.summary
    );
    assert!(!pending.summary.contains("지울 메시지 본문"));
    assert!(pending.expires_at.is_some());
    // 아직 실행되지 않음.
    assert!(readable(&a, space, channel, target, "지울 메시지 본문").await);

    // 자동화 토큰은 확인 요청을 보지도, 승인·거절하지도 못함.
    assert_eq!(
        code(call_as::<m::ConfirmList>(&a, &bot, Empty).await),
        ERR_OUT_OF_SCOPE
    );
    assert_eq!(
        code(call_as::<m::ConfirmApprove>(&a, &bot, IdParams { id: cid.clone() }).await),
        ERR_OUT_OF_SCOPE
    );
    assert_eq!(
        code(call_as::<m::ConfirmDeny>(&a, &bot, IdParams { id: cid.clone() }).await),
        ERR_OUT_OF_SCOPE
    );

    // 사람(소유자)이 보고 승인. 두 번 동시에 눌러도 한 번만 실행됩니다.
    let list = a.ok::<m::ConfirmList>(Empty).await;
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].id, cid);
    let (r1, r2) = tokio::join!(
        a.call::<m::ConfirmApprove>(IdParams { id: cid.clone() }),
        a.call::<m::ConfirmApprove>(IdParams { id: cid.clone() })
    );
    let codes: Vec<Option<i64>> = [&r1, &r2]
        .iter()
        .map(|r| r.as_ref().err().map(|e| e.code))
        .collect();
    assert_eq!(
        codes.iter().filter(|c| c.is_none()).count(),
        1,
        "승인 결과: {codes:?}"
    );
    assert!(codes.contains(&Some(ERR_NOT_FOUND)), "승인 결과: {codes:?}");
    wait_message(
        &c,
        space,
        channel,
        target,
        "멤버에게도 삭제가 전달",
        |v| v.deleted,
    )
    .await;
    let deletes = testkit::stream_events(&a.node, &space, StreamId::Channel(channel))
        .expect("채널 이벤트")
        .iter()
        .filter(|e| e.header.kind == EventKind::Delete)
        .count();
    assert_eq!(deletes, 1, "삭제가 한 번만 실행되어야 함");
    assert!(a.ok::<m::ConfirmList>(Empty).await.is_empty());

    // 감사 로그: 확인 대기(1006), 승인으로 실행된 원래 호출, 승인 호출. 내용은 없음.
    let audit = a.ok::<m::AuditList>(AuditListParams::default()).await;
    assert!(audit.iter().any(|e| e.method == "message.delete"
        && e.token_name == "정리 봇"
        && e.automation
        && e.code == Some(ERR_CONFIRMATION_REQUIRED)));
    assert!(
        audit
            .iter()
            .any(|e| e.method == "message.delete" && e.token_name == "정리 봇" && e.ok)
    );
    assert!(
        audit
            .iter()
            .any(|e| e.method == "confirm.approve" && e.token_name == "owner" && e.ok)
    );
    let json = serde_json::to_string(&audit).expect("직렬화");
    assert!(
        !json.contains("지울 메시지 본문"),
        "감사 로그에 내용이 남음"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn confirmation_deny_expiry_and_revoked_requester() {
    let cl = Cluster::new();
    let (a, c, space, channel) = crew_with_member(&cl).await;
    let m1 = c.send(space, channel, "첫째").await;
    let m2 = c.send(space, channel, "둘째").await;
    wait_message(&a, space, channel, m2, "멤버 메시지 받음", |_| true).await;
    let (bot_id, _, bot) = token(&a, "봇", vec![Scope::Read, Scope::Write], Vec::new(), true).await;
    let delete = |id: EventId| MessageRefParams { space, channel, id };
    let confirmation_id = |e: RpcError| -> String {
        assert_eq!(e.code, ERR_CONFIRMATION_REQUIRED);
        e.data
            .and_then(|d| d.get("confirmation_id").cloned())
            .and_then(|v| v.as_str().map(str::to_owned))
            .expect("confirmation_id")
    };

    // 거절: 실행되지 않고 대기열에서 사라짐.
    let id = confirmation_id(
        call_as::<m::MessageDelete>(&a, &bot, delete(m1))
            .await
            .expect_err("확인 필요"),
    );
    a.ok::<m::ConfirmDeny>(IdParams { id: id.clone() }).await;
    assert!(a.ok::<m::ConfirmList>(Empty).await.is_empty());
    assert_eq!(
        code(
            a.call::<m::ConfirmApprove>(IdParams { id: id.clone() })
                .await
        ),
        ERR_NOT_FOUND
    );
    assert_eq!(
        code(a.call::<m::ConfirmDeny>(IdParams { id }).await),
        ERR_NOT_FOUND
    );
    assert!(readable(&a, space, channel, m1, "첫째").await);

    // 만료: 유효 시간이 지난 요청은 승인할 수 없음.
    testkit::set_confirm_ttl(&a.node, Duration::ZERO);
    let id = confirmation_id(
        call_as::<m::MessageDelete>(&a, &bot, delete(m2))
            .await
            .expect_err("확인 필요"),
    );
    assert_eq!(
        code(a.call::<m::ConfirmApprove>(IdParams { id }).await),
        ERR_NOT_FOUND
    );
    assert!(a.ok::<m::ConfirmList>(Empty).await.is_empty());
    assert!(readable(&a, space, channel, m2, "둘째").await);

    // 요청한 토큰을 폐기하면 그 요청도 사라짐.
    testkit::set_confirm_ttl(&a.node, Duration::from_secs(3600));
    let id = confirmation_id(
        call_as::<m::MessageDelete>(&a, &bot, delete(m2))
            .await
            .expect_err("확인 필요"),
    );
    assert_eq!(a.ok::<m::ConfirmList>(Empty).await.len(), 1);
    a.ok::<m::TokenRevoke>(IdParams { id: bot_id }).await;
    assert!(a.ok::<m::ConfirmList>(Empty).await.is_empty());
    assert_eq!(
        code(a.call::<m::ConfirmApprove>(IdParams { id }).await),
        ERR_NOT_FOUND
    );
    assert!(readable(&a, space, channel, m2, "둘째").await);

    // 사람 토큰의 남의 메시지 삭제는 확인 없이 바로 실행됩니다.
    let (_, _, human) = token(
        &a,
        "사람",
        vec![Scope::Read, Scope::Write],
        vec![space],
        false,
    )
    .await;
    call_as::<m::MessageDelete>(&a, &human, delete(m2))
        .await
        .expect("사람 토큰은 바로 실행");
    wait_message(&c, space, channel, m2, "삭제 전달", |v| v.deleted).await;
}

// ───────────── 프로필 ─────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn profile_change_reaches_friend_and_crew_member() {
    let cl = Cluster::new();
    let (a, c, crew, _) = crew_with_member(&cl).await;
    let b = cl.node("b").await;
    let friend_space = befriend(&a, &b).await;
    let a_user = a.user();
    let name_on = |n: &TestNode| {
        let node = n.node.clone();
        async move {
            common::call::<m::ProfileGet>(&node, ProfileGetParams { user: Some(a_user) })
                .await
                .ok()
                .map(|p| p.name)
        }
    };

    a.ok::<m::ProfileSet>(ProfileSetParams {
        name: Some("새 이름".into()),
        ..Default::default()
    })
    .await;
    eventually("친구가 바뀐 프로필을 받음", || async {
        (name_on(&b).await.as_deref() == Some("새 이름")).then_some(())
    })
    .await;
    eventually("크루 멤버가 바뀐 프로필을 받음", || async {
        (name_on(&c).await.as_deref() == Some("새 이름")).then_some(())
    })
    .await;

    // 크루별 별명은 그 크루 상태에만 있습니다.
    a.ok::<m::ProfileSetCrew>(ProfileSetCrewParams {
        space: crew,
        nickname: Some("크루장".into()),
    })
    .await;
    eventually("크루 멤버가 별명을 봄", || async {
        let st = testkit::space_state(&c.node, &crew)?;
        (st.members.get(&a_user)?.nickname.as_deref() == Some("크루장")).then_some(())
    })
    .await;
    assert!(testkit::space_state(&b.node, &crew).is_none());
    assert_eq!(
        code(
            a.call::<m::ProfileSetCrew>(ProfileSetCrewParams {
                space: friend_space,
                nickname: Some("별명".into()),
            })
            .await
        ),
        ERR_INVALID_INPUT
    );
    assert_eq!(
        code(
            a.call::<m::ProfileSetCrew>(ProfileSetCrewParams {
                space: crew,
                nickname: Some("가".repeat(65)),
            })
            .await
        ),
        ERR_INVALID_INPUT
    );
    assert_eq!(
        code(
            a.call::<m::ProfileSetCrew>(ProfileSetCrewParams {
                space: SpaceId::random(),
                nickname: Some("별명".into()),
            })
            .await
        ),
        ERR_NOT_FOUND
    );
    // 별명 지우기.
    a.ok::<m::ProfileSetCrew>(ProfileSetCrewParams {
        space: crew,
        nickname: None,
    })
    .await;
    eventually("별명 지움 전달", || async {
        let st = testkit::space_state(&c.node, &crew)?;
        st.members.get(&a_user)?.nickname.is_none().then_some(())
    })
    .await;
}

// ───────────── 메모 ─────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn notes_sync_only_between_own_devices() {
    let cl = Cluster::new();
    let a1 = cl.node("a1").await;
    let b = cl.node("b").await;
    befriend(&a1, &b).await;
    let b_user = b.user();
    let note_of = |n: &TestNode| {
        let node = n.node.clone();
        async move {
            common::call::<m::NoteGet>(&node, UserParams { user: b_user })
                .await
                .map(|v| v.text)
                .unwrap_or_default()
        }
    };

    assert_eq!(note_of(&a1).await, "");
    a1.ok::<m::NoteSet>(NoteSetParams {
        user: b_user,
        text: "생일은 3월".into(),
    })
    .await;
    assert_eq!(note_of(&a1).await, "생일은 3월");
    assert_eq!(
        code(
            a1.call::<m::NoteSet>(NoteSetParams {
                user: a1.user(),
                text: "나".into(),
            })
            .await
        ),
        ERR_INVALID_INPUT
    );

    // 친구는 내 메모를 받지 않고, 메모 묶음을 보내도 거절됩니다. 수락 흐름만으로는 A1이 아직 B를 인증하지 않았을 수
    // 있으므로(그러면 `Unauthenticated`) B가 인증을 보내 A1이 기억하게 합니다.
    testkit::reauth(&b.node, a1.peer()).await.expect("인증");
    let r = testkit::net_request(
        &b.node,
        a1.peer(),
        YpRequest::Social(SocialRequest::Notes {
            sealed: vec![0; 16],
        }),
    )
    .await
    .expect("요청");
    assert!(
        matches!(
            r,
            YpResponse::Error {
                code: WireErrorCode::Forbidden,
                ..
            }
        ),
        "{r:?}"
    );
    assert_eq!(
        b.ok::<m::NoteGet>(UserParams { user: a1.user() })
            .await
            .text,
        ""
    );

    // 같은 신원의 새 기기는 연결되면 메모를 받습니다.
    let a2 = common::linked(&cl, &a1, "a2", |_| {}).await;
    common::wait_own_device(&a1, testkit::device_id(&a2.node).expect("새 기기 ID")).await;
    eventually("새 기기가 메모를 받음", || async {
        (note_of(&a2).await == "생일은 3월").then_some(())
    })
    .await;
    // 한쪽에서 고치면 다른 쪽도.
    a1.ok::<m::NoteSet>(NoteSetParams {
        user: b_user,
        text: "생일은 3월 14일".into(),
    })
    .await;
    eventually("고친 메모 전달", || async {
        (note_of(&a2).await == "생일은 3월 14일").then_some(())
    })
    .await;
    // 지움도 전달.
    a2.ok::<m::NoteSet>(NoteSetParams {
        user: b_user,
        text: String::new(),
    })
    .await;
    eventually("지운 메모 전달", || async {
        note_of(&a1).await.is_empty().then_some(())
    })
    .await;
    assert_eq!(
        b.ok::<m::NoteGet>(UserParams { user: a1.user() })
            .await
            .text,
        ""
    );
}

// ───────────── 기기 ─────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restored_device_joins_existing_friend_space() {
    let cl = Cluster::new();
    let t = two_devices(&cl).await;
    let (a1, a2, b) = (&t.a1, &t.a2, &t.b);
    let a_user = a1.user();

    // 두 기기의 목록이 같은 판으로 모이고, 두 기기가 모두 활성.
    eventually("기기 목록 수렴", || async {
        let l1 = testkit::device_list(&a1.node, &a_user)?;
        let l2 = testkit::device_list(&a2.node, &a_user)?;
        (l1 == l2 && l1.active().count() == 2).then_some(())
    })
    .await;
    assert_eq!(a2.ok::<m::DeviceList>(Empty).await.len(), 2);
    // 복구가 프로필을 덮어쓰지 않습니다: 새 기기는 예전 프로필(이름·소개)을 받아 쓰고, 예전 기기와 친구는 그대로.
    let profile_on = |n: &TestNode| {
        let node = n.node.clone();
        async move {
            common::call::<m::ProfileGet>(&node, ProfileGetParams { user: Some(a_user) })
                .await
                .ok()
                .map(|p| (p.name, p.bio))
        }
    };
    let old = Some(("a1".to_owned(), "예전 소개".to_owned()));
    eventually("새 기기가 예전 프로필을 받음", || async {
        (profile_on(a2).await == old).then_some(())
    })
    .await;
    assert_eq!(profile_on(a1).await, old);
    assert_eq!(profile_on(b).await, old);
    // 새 기기의 친구 목록이 되살아남.
    eventually("새 기기의 친구 목록", || async {
        a2.ok::<m::FriendList>(Empty)
            .await
            .into_iter()
            .any(|f| f.user == b.user() && f.space == t.space)
            .then_some(())
    })
    .await;

    // 합류 뒤의 새 메시지를 양쪽에서 읽고 씀.
    let from_b = b.send(t.space, t.channel, "복구 뒤 메시지").await;
    wait_message(
        a2,
        t.space,
        t.channel,
        from_b,
        "새 기기가 새 메시지를 읽음",
        |v| v.text == "복구 뒤 메시지",
    )
    .await;
    let from_a2 = a2.send(t.space, t.channel, "새 기기에서").await;
    wait_message(
        b,
        t.space,
        t.channel,
        from_a2,
        "친구가 새 기기의 메시지를 읽음",
        |v| v.text == "새 기기에서" && v.author == a_user,
    )
    .await;
    wait_message(
        a1,
        t.space,
        t.channel,
        from_a2,
        "내 다른 기기도 읽음",
        |v| v.text == "새 기기에서",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn revoked_device_cannot_decrypt_or_authenticate() {
    let cl = Cluster::new();
    let t = two_devices(&cl).await;
    let (a1, a2, b) = (&t.a1, &t.a2, &t.b);
    let a1_dev = testkit::device_id(&a1.node).expect("기기 ID");

    // 지금 기기·모르는 기기는 폐기할 수 없음.
    assert_eq!(
        code(
            a1.call::<m::DeviceRevoke>(DeviceRevokeParams { device: a1_dev })
                .await
        ),
        ERR_INVALID_INPUT
    );
    assert_eq!(
        code(
            a1.call::<m::DeviceRevoke>(DeviceRevokeParams {
                device: DeviceId::random()
            })
            .await
        ),
        ERR_NOT_FOUND
    );

    a1.ok::<m::DeviceRevoke>(DeviceRevokeParams { device: t.a2_dev })
        .await;
    assert!(
        a1.ok::<m::DeviceList>(Empty)
            .await
            .iter()
            .any(|d| d.entry.device == t.a2_dev && d.entry.revoked_at.is_some())
    );
    assert_eq!(
        code(
            a1.call::<m::DeviceRevoke>(DeviceRevokeParams { device: t.a2_dev })
                .await
        ),
        ERR_CONFLICT
    );
    eventually("폐기한 기기가 키 그룹에서 빠짐", || async {
        let m1 = testkit::crew_key_members(&a1.node, &t.space)?;
        let mb = testkit::crew_key_members(&b.node, &t.space)?;
        let e1 = testkit::crew_key_seq(&a1.node, &t.space)?;
        let eb = testkit::crew_key_seq(&b.node, &t.space)?;
        (!m1.contains(&t.a2_dev) && !mb.contains(&t.a2_dev) && e1 == eb).then_some(())
    })
    .await;

    let after = b.send(t.space, t.channel, "폐기 뒤 비밀").await;
    wait_message(
        a1,
        t.space,
        t.channel,
        after,
        "남은 기기는 읽음",
        |v| v.text == "폐기 뒤 비밀",
    )
    .await;
    // 폐기된 기기에 이벤트가 그대로 닿아도 열 수 없습니다(회전 뒤 키가 없음).
    for stream in [StreamId::Space, StreamId::Channel(t.channel)] {
        let events = testkit::stream_events(&b.node, &t.space, stream).expect("이벤트");
        let _ = testkit::ingest_from(&a2.node, t.space, events, b.peer()).await;
    }
    assert!(!readable(a2, t.space, t.channel, after, "폐기 뒤 비밀").await);

    // 친구도 내 다른 기기도 폐기된 기기의 인증을 받지 않습니다.
    assert!(testkit::reauth(&a2.node, b.peer()).await.is_err());
    assert!(testkit::reauth(&a2.node, a1.peer()).await.is_err());
    let r = testkit::net_request(
        &a2.node,
        b.peer(),
        YpRequest::GetHeads {
            space: t.space,
            stream: StreamId::Space,
        },
    )
    .await
    .expect("요청");
    assert!(
        matches!(
            r,
            YpResponse::Error {
                code: WireErrorCode::Unauthenticated,
                ..
            }
        ),
        "{r:?}"
    );
}

// ───────────── 친구 삭제 ─────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn friend_remove_clears_both_sides() {
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let b = cl.node("b").await;
    let mut c = cl.node("c").await;
    let ab = befriend(&a, &b).await;
    let ac = befriend(&a, &c).await;
    let ch = a.first_channel(ab).await;
    let before = a.send(ab, ch, "삭제 전 대화").await;
    wait_message(&b, ab, ch, before, "삭제 전 대화 전달", |_| true).await;
    let (a_user, b_user, c_user) = (a.user(), b.user(), c.user());
    let has_friend = |n: &TestNode, who: UserId| {
        let node = n.node.clone();
        async move {
            common::call::<m::FriendList>(&node, Empty)
                .await
                .map(|v| v.iter().any(|f| f.user == who))
                .unwrap_or(false)
        }
    };

    assert_eq!(
        code(
            a.call::<m::FriendRemove>(UserParams {
                user: UserId::random()
            })
            .await
        ),
        ERR_NOT_FOUND
    );

    // 상대가 연결돼 있으면 바로 전해지고, 양쪽 모두 연락처와 대화를 지웁니다.
    let mut b_rx = b.subscribe();
    a.ok::<m::NoteSet>(NoteSetParams {
        user: b_user,
        text: "메모".into(),
    })
    .await;
    a.ok::<m::FriendRemove>(UserParams { user: b_user }).await;
    assert!(!has_friend(&a, b_user).await);
    assert!(!testkit::known_spaces(&a.node).contains(&ab));
    eventually("상대도 친구 목록에서 지움", || async {
        (!has_friend(&b, a_user).await).then_some(())
    })
    .await;
    // 앱은 이 알림으로 친구 목록을 다시 읽습니다.
    next_notification(&mut b_rx, "상대의 friend.updated", |n| {
        matches!(n, Notification::FriendUpdated { user } if *user == a_user).then_some(())
    })
    .await;
    assert!(!testkit::known_spaces(&b.node).contains(&ab));
    assert_eq!(
        code(b.call::<m::MessageList>(list_params(ab, ch)).await),
        ERR_NOT_FOUND
    );
    assert_eq!(
        code(a.call::<m::MessageSend>(send_params(ab, ch, "더는")).await),
        ERR_NOT_FOUND
    );
    // 더 만나지 않는 사람의 남은 정보도 지웁니다.
    assert!(testkit::device_list(&a.node, &b_user).is_none());
    assert!(testkit::device_list(&b.node, &a_user).is_none());
    assert!(
        a.ok::<m::NoteGet>(UserParams { user: b_user })
            .await
            .text
            .is_empty()
    );
    assert!(
        a.ok::<m::SpaceList>(Empty)
            .await
            .iter()
            .all(|s| s.space != ab)
    );
    // 다른 친구는 그대로.
    assert!(has_friend(&a, c_user).await);

    // 상대가 꺼져 있으면 다음에 연결될 때 전해집니다. 상대가 못 받은 이벤트가 있어도(내가 공간을 지워 내줄 수 없음) 받습니다.
    c.shutdown().await;
    a.send(ac, a.first_channel(ac).await, "못 받을 글").await;
    a.ok::<m::FriendRemove>(UserParams { user: c_user }).await;
    assert!(!has_friend(&a, c_user).await);
    assert!(!testkit::known_spaces(&a.node).contains(&ac));
    c.start_again().await;
    connect_both(&a, &c).await;
    eventually(
        "다시 연결된 상대도 친구 목록에서 지움",
        || async { (!has_friend(&c, a_user).await).then_some(()) },
    )
    .await;
    eventually("다시 연결된 상대도 대화를 지움", || async {
        (!testkit::known_spaces(&c.node).contains(&ac)).then_some(())
    })
    .await;
    eventually("전할 끊기가 지워짐", || async {
        (!testkit::has_pending_leave(&a.node, &c_user)).then_some(())
    })
    .await;

    // 다시 만나면 새 공간에서 시작합니다(묘비는 끊은 공간에만).
    let again = befriend(&a, &b).await;
    assert_ne!(again, ab);
    let ch = a.first_channel(again).await;
    let id = a.send(again, ch, "다시 만남").await;
    wait_message(&b, again, ch, id, "다시 만난 대화 전달", |_| true).await;
}

// ───────────── 검토 반영 회귀 시험 ─────────────

/// 폐기한 토큰의 열린 연결에는 알림을 더 보내지 않습니다.
/// 계정 삭제는 이 기기의 신원을 지우고 앱이 처음 화면으로 돌아가도록 `device.wiped`를 알립니다.
/// 데몬은 다시 떠 새 노드 키로 시작합니다(같은 프로세스에서 새 계정을 만들면 키를 잃은 onion 서비스가 남아 처음 오는
/// 노드가 닿지 못함).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn identity_delete_wipes_this_device() {
    let cl = Cluster::new();
    let mut a = cl.node("a").await;
    let old_peer = a.peer();
    let mut events = a.subscribe();
    a.ok::<m::IdentityDelete>(Empty).await;
    next_notification(&mut events, "계정 삭제 알림", |n| {
        matches!(n, Notification::DeviceWiped).then_some(())
    })
    .await;
    a.call::<m::IdentityGet>(Empty {})
        .await
        .expect_err("신원이 지워짐");
    eventually("지운 뒤 데몬이 멈춤", || async {
        a.node.is_shutting_down().then_some(())
    })
    .await;
    assert!(a.node.restart_requested());
    a.shutdown().await;
    a.start_again().await;
    assert_ne!(a.peer(), old_peer, "지운 신원과 이어지지 않는 새 노드 키");
    a.ok::<m::IdentityCreate>(IdentityCreateParams {
        name: "새 계정".into(),
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn revoked_token_gets_no_more_notifications() {
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let (id, secret, _) = token(&a, "구독 봇", vec![Scope::Read], Vec::new(), true).await;
    let handler = ApiHandler::new(a.node.clone());
    let auth = handler.authenticate(&secret).await.expect("인증");
    let n = Notification::Presence {
        user: UserId::random(),
        online: true,
        away: false,
        last_seen: None,
    };
    assert!(handler.filter_notification(&auth, &n).is_some());
    a.ok::<m::TokenRevoke>(IdParams { id }).await;
    assert!(handler.filter_notification(&auth, &n).is_none());
    // 소유자 연결은 그대로 받습니다.
    let owner_secret = std::fs::read_to_string(a.dir.join(yangpa_rpc::endpoint::OWNER_TOKEN_FILE))
        .expect("소유자 토큰 파일");
    let owner = handler
        .authenticate(owner_secret.trim())
        .await
        .expect("소유자 인증");
    assert!(handler.filter_notification(&owner, &n).is_some());
}

/// `friend.remove`는 파괴적이라 자동화 토큰은 사람 확인을 거칩니다. 감사 로그는 인증하지 않은 호출을 남기지 않고,
/// 평범한 호출이 넘쳐도 중요한 줄(확인 대기·승인 실행)은 밀려나지 않습니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn automation_friend_remove_needs_human_and_audit_keeps_important_rows() {
    let cl = Cluster::new();
    let a = cl.node_with("a", |c| c.audit_caps = (5, 1000)).await;
    let b = cl.node("b").await;
    befriend(&a, &b).await;
    let (a_user, b_user) = (a.user(), b.user());
    let (_, _, bot) = token(
        &a,
        "친구 정리 봇",
        vec![Scope::Read, Scope::Write],
        Vec::new(),
        true,
    )
    .await;

    let err = call_as::<m::FriendRemove>(&a, &bot, UserParams { user: b_user })
        .await
        .expect_err("사람 확인이 필요해야 함");
    assert_eq!(err.code, ERR_CONFIRMATION_REQUIRED);
    let cid = err
        .data
        .as_ref()
        .and_then(|d| d.get("confirmation_id"))
        .and_then(|v| v.as_str())
        .expect("confirmation_id")
        .to_owned();
    assert!(
        a.ok::<m::FriendList>(Empty)
            .await
            .iter()
            .any(|f| f.user == b_user)
    );

    // 인증하지 않은 공개 호출은 남기지 않고, 평범한 호출을 잔뜩 해도 중요한 줄은 남습니다.
    a.node
        .call(
            &CallCtx::anonymous(),
            "system.version",
            serde_json::json!({}),
        )
        .await
        .expect("공개 메서드");
    for _ in 0..20 {
        call_as::<m::FriendList>(&a, &bot, Empty)
            .await
            .expect("읽기");
    }
    let audit = a.ok::<m::AuditList>(AuditListParams::default()).await;
    assert!(!audit.iter().any(|e| e.method == "system.version"));
    assert!(
        audit.iter().filter(|e| e.method == "friend.list").count() <= 5,
        "평범한 줄은 상한까지만"
    );
    assert!(audit.iter().any(|e| e.method == "friend.remove"
        && e.token_name == "친구 정리 봇"
        && e.code == Some(ERR_CONFIRMATION_REQUIRED)));

    // 사람이 승인하면 실행됩니다.
    a.ok::<m::ConfirmApprove>(IdParams { id: cid }).await;
    assert!(
        !a.ok::<m::FriendList>(Empty)
            .await
            .iter()
            .any(|f| f.user == b_user)
    );
    eventually("상대도 친구 목록에서 지움", || async {
        (!b.ok::<m::FriendList>(Empty)
            .await
            .iter()
            .any(|f| f.user == a_user))
        .then_some(())
    })
    .await;
    for _ in 0..20 {
        call_as::<m::FriendList>(&a, &bot, Empty)
            .await
            .expect("읽기");
    }
    let audit = a.ok::<m::AuditList>(AuditListParams::default()).await;
    assert!(
        audit
            .iter()
            .any(|e| e.method == "friend.remove" && e.token_name == "친구 정리 봇" && e.ok)
    );
    assert!(
        audit
            .iter()
            .any(|e| e.method == "friend.remove" && e.code == Some(ERR_CONFIRMATION_REQUIRED))
    );
}

/// 휘발성 채널을 대상으로 한 확인 요청은 RAM에만 두고(재시작하면 사라짐), 보통 채널 대상은 저장합니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn volatile_confirmation_stays_in_memory() {
    let cl = Cluster::new();
    let (mut a, _c, space, text) = crew_with_member(&cl).await;
    let volatile = testkit::create_volatile_channel(&a.node, space, "휘발")
        .await
        .expect("휘발성 채널");
    let (_, _, bot) = token(&a, "봇", vec![Scope::Read, Scope::Write], Vec::new(), true).await;
    let params = |channel: ChannelId| {
        serde_json::to_value(MessageRefParams {
            space,
            channel,
            id: EventId::random(),
        })
        .expect("직렬화")
    };

    let e = testkit::confirm_when_others(&a.node, &bot, "message.delete", &params(volatile))
        .await
        .expect_err("확인 필요");
    assert_eq!(e.code, ERR_CONFIRMATION_REQUIRED);
    assert_eq!(testkit::confirmation_counts(&a.node), (1, 0));
    let e = testkit::confirm_when_others(&a.node, &bot, "message.delete", &params(text))
        .await
        .expect_err("확인 필요");
    assert_eq!(e.code, ERR_CONFIRMATION_REQUIRED);
    assert_eq!(testkit::confirmation_counts(&a.node), (1, 1));
    assert_eq!(a.ok::<m::ConfirmList>(Empty).await.len(), 2);

    a.restart().await;
    assert_eq!(testkit::confirmation_counts(&a.node), (0, 1));
    assert_eq!(a.ok::<m::ConfirmList>(Empty).await.len(), 1);
}

/// 기기 목록 상한을 넘는 목록은 서명이 맞아도 받지 않습니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn oversized_device_list_is_rejected() {
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let b = cl.node("b").await;
    befriend(&a, &b).await;
    // 수락 흐름만으로는 a가 아직 b를 인증하지 않았을 수 있으므로 b가 인증을 보내 a가 기억하게 합니다.
    testkit::reauth(&b.node, a.peer()).await.expect("인증");
    let b_user = b.user();
    let big = testkit::padded_device_list(&b.node, 64).expect("목록");
    let r = testkit::net_request(
        &b.node,
        a.peer(),
        YpRequest::Social(SocialRequest::Devices {
            devices: Box::new(big),
            spaces: Vec::new(),
            contacts: Vec::new(),
            cut: Vec::new(),
            mobile: false,
        }),
    )
    .await
    .expect("요청");
    assert!(
        matches!(
            r,
            YpResponse::Error {
                code: WireErrorCode::BadRequest,
                ..
            }
        ),
        "{r:?}"
    );
    assert_eq!(
        testkit::device_list(&a.node, &b_user)
            .expect("목록")
            .devices
            .len(),
        1
    );
    // 상한 안이면 받습니다.
    let ok = testkit::padded_device_list(&b.node, 3).expect("목록");
    let r = testkit::net_request(
        &b.node,
        a.peer(),
        YpRequest::Social(SocialRequest::Devices {
            devices: Box::new(ok),
            spaces: Vec::new(),
            contacts: Vec::new(),
            cut: Vec::new(),
            mobile: false,
        }),
    )
    .await
    .expect("요청");
    assert!(matches!(r, YpResponse::Social(_)), "{r:?}");
    assert_eq!(
        testkit::device_list(&a.node, &b_user)
            .expect("목록")
            .devices
            .len(),
        4
    );
}

/// 끊은 1:1 공간에는 상대가 더 밀어 넣지 못하고, 양쪽이 서로 끊었으면 전할 끊기를 지워 그 사람의 인증을 더 받아 주지
/// 않습니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cut_space_rejects_push_and_stale_pending_leave_is_cleared() {
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let b = cl.node("b").await;
    let mut c = cl.node("c").await;
    let ab = befriend(&a, &b).await;
    befriend(&a, &c).await;
    let ch = a.first_channel(ab).await;
    let (a_user, c_user) = (a.user(), c.user());
    // a가 b를 인증해 둔 채로 시작합니다(삭제 뒤에는 새로 인증하지 못함).
    testkit::reauth(&b.node, a.peer()).await.expect("인증");
    let id = b.send(ab, ch, "끊기 전 메시지").await;
    let ev = testkit::get_event(&b.node, &id)
        .expect("저장소")
        .expect("이벤트");
    wait_message(&a, ab, ch, id, "끊기 전 메시지 전달", |_| true).await;

    a.ok::<m::FriendRemove>(UserParams { user: b.user() }).await;
    let r = testkit::net_request(
        &b.node,
        a.peer(),
        YpRequest::PushEvents {
            space: ab,
            events: vec![ev],
        },
    )
    .await
    .expect("요청");
    assert!(matches!(r, YpResponse::Error { .. }), "{r:?}");
    assert!(testkit::get_event(&a.node, &id).expect("저장소").is_none());

    // c가 꺼진 사이 양쪽이 서로 끊습니다.
    c.shutdown().await;
    a.ok::<m::FriendRemove>(UserParams { user: c_user }).await;
    assert!(testkit::has_pending_leave(&a.node, &c_user));
    c.start_again().await;
    c.ok::<m::FriendRemove>(UserParams { user: a_user }).await;
    connect(&a, &c).await;
    eventually("전할 친구 삭제가 지워짐", || async {
        (!testkit::has_pending_leave(&a.node, &c_user)).then_some(())
    })
    .await;
}

/// 새 기기가 상태 이벤트를 먼저 받고 자기 앞으로 봉인한 상태 키 묶음을 나중에 받아도 상태를 세웁니다(합류 중
/// 따라잡기가 키 갱신·키 묶음보다 먼저 도는 경우).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn state_resolves_when_sealed_state_keys_arrive_last() {
    let cl = Cluster::new();
    let a1 = cl.node("a1").await;
    let b = cl.node("b").await;
    let space = befriend(&a1, &b).await;
    let a2 = cl.bare("a2").await;
    testkit::install_stolen_seed(&a1.node, &a2.node).expect("같은 신원의 기기");
    let genesis = testkit::genesis_of(&a1.node, &space).expect("시작 이벤트");
    testkit::pin_space(&a2.node, space, genesis).expect("런타임");
    let events = testkit::stream_events(&a1.node, &space, StreamId::Space).expect("이벤트");
    testkit::ingest_from(&a2.node, space, events, a1.peer())
        .await
        .expect("적용");
    assert!(testkit::space_state(&a2.node, &space).is_none());

    let to = vec![(
        testkit::device_id(&a2.node).expect("기기"),
        testkit::box_public(&a2.node).expect("상자 키"),
    )];
    let share = testkit::author_sealed_state_keys(&a1.node, space, to).expect("키 묶음");
    testkit::ingest_from(&a2.node, space, vec![share], a1.peer())
        .await
        .expect("적용");
    let st = testkit::space_state(&a2.node, &space).expect("상태가 세워져야 함");
    assert!(st.is_member(&a1.user()) && st.is_member(&b.user()));
}

/// 계정을 연결한 내 기기가 확인한 후원 결과를, 나중에 들어온 내 다른 기기가 인증된 연결로 받아 씁니다(로그인 없이).
/// 연결을 해제하면 끝났다는 결과가 가서 함께 잠기고, 남의 기기에는 보내지 않습니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn supporter_status_is_shared_with_own_devices() {
    use yangpa_core::time::Timestamp;
    use yangpa_rpc::api::{SupporterProvider, SupporterUnlinkParams};

    let cl = Cluster::new();
    let a1 = cl.node("a1").await;
    let a1_dev = testkit::device_id(&a1.node).expect("기기 ID");
    let until = Timestamp::now().saturating_add(Duration::from_secs(3600));
    testkit::set_supporter_link(&a1.node, "octocat", until);
    assert!(a1.ok::<m::SupporterStatusGet>(Empty).await.active);

    let b = cl.node("b").await;
    befriend(&a1, &b).await;
    let a2 = common::linked(&cl, &a1, "a2", |_| {}).await;
    let a2_dev = testkit::device_id(&a2.node).expect("새 기기 ID");
    common::wait_own_device(&a1, a2_dev).await;
    eventually("내 다른 기기가 후원 결과를 받음", || async {
        let s = a2.ok::<m::SupporterStatusGet>(Empty).await;
        (s.active
            && s.links.is_empty()
            && s.shared
                .iter()
                .any(|x| x.device == a1_dev && x.active && x.login == "octocat"))
        .then_some(())
    })
    .await;
    // 친구 기기는 받지 않습니다.
    assert!(b.ok::<m::SupporterStatusGet>(Empty).await.shared.is_empty());

    a1.ok::<m::SupporterUnlink>(SupporterUnlinkParams {
        provider: SupporterProvider::Github,
    })
    .await;
    eventually("연결을 해제하면 내 다른 기기도 잠김", || async {
        let s = a2.ok::<m::SupporterStatusGet>(Empty).await;
        (!s.active && s.shared.iter().all(|x| !x.active)).then_some(())
    })
    .await;
}

async fn flair_of(n: &TestNode, user: UserId) -> Option<yangpa_rpc::api::FlairView> {
    n.ok::<m::FlairList>(Empty)
        .await
        .into_iter()
        .find(|f| f.user == user)
}

/// 단계는 바뀌지 않았어도, 표시를 낸 뒤에 생긴 친구 공간·크루의 상대가 후광을 봅니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn supporter_flair_reaches_spaces_made_after_it_was_published() {
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let user = testkit::user_id(&a.node).expect("사용자");
    testkit::set_supporter_tier(&a.node, 5000);
    eventually("내 화면에는 후광이 보임", || async {
        flair_of(&a, user)
            .await
            .filter(|f| f.halo_allowed)
            .map(|_| ())
    })
    .await;

    let friend = cl.node("friend").await;
    befriend(&a, &friend).await;
    eventually("나중에 사귄 친구가 후광 표시를 받음", || async {
        flair_of(&friend, user)
            .await
            .filter(|f| f.halo_allowed)
            .map(|_| ())
    })
    .await;

    let c = cl.node("c").await;
    testkit::seed_crew(&a.node, std::slice::from_ref(&c.node), "시험 크루")
        .await
        .expect("크루 만들기");
    connect_both(&a, &c).await;
    testkit::authenticate(&a.node, c.peer())
        .await
        .expect("크루 멤버끼리 인증");
    eventually(
        "나중에 만든 크루의 멤버가 후광 표시를 받음",
        || async {
            flair_of(&c, user)
                .await
                .filter(|f| f.halo_allowed)
                .map(|_| ())
        },
    )
    .await;
}

/// 표시가 빠진 채 이미 있는 크루는 시작할 때 채우고, 실린 공간에는 재시작해도 다시 내지 않습니다(3만 명 크루에도 퍼짐).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn supporter_flair_fills_existing_spaces_on_start_only_once() {
    let cl = Cluster::new();
    let (mut a, c, space, _) = crew_with_member(&cl).await;
    let user = testkit::user_id(&a.node).expect("사용자");
    testkit::hold_flair(&a.node);
    assert!(flair_of(&c, user).await.is_none());

    a.restart().await;
    connect_both(&a, &c).await;
    testkit::authenticate(&a.node, c.peer())
        .await
        .expect("크루 멤버끼리 인증");
    eventually(
        "이미 있던 크루의 멤버가 후광 표시를 받음",
        || async {
            flair_of(&c, user)
                .await
                .filter(|f| f.halo_allowed)
                .map(|_| ())
        },
    )
    .await;

    let events = |n: &TestNode| {
        testkit::stream_events(&n.node, &space, StreamId::Space)
            .expect("이벤트")
            .iter()
            .filter(|e| e.header.kind == EventKind::MemberFlair)
            .count()
    };
    assert_eq!(events(&a), 1);
    a.restart().await;
    testkit::spread_flair(&a.node).await;
    assert_eq!(events(&a), 1, "실린 공간에 다시 내면 안 됨");
}

/// 후원 계정을 연결한 기기가 꺼져 있어도, 표시를 받아 둔 내 다른 기기가 새 크루에 나릅니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn supporter_flair_is_carried_by_own_device_without_the_account() {
    let cl = Cluster::new();
    let a1 = cl.node("a1").await;
    let a = testkit::user_id(&a1.node).expect("사용자");
    let b = cl.node("b").await;
    let space = befriend(&a1, &b).await;
    let a2 = common::linked(&cl, &a1, "a2", |_| {}).await;
    let a2_dev = testkit::device_id(&a2.node).expect("새 기기 ID");
    common::wait_own_device(&a1, a2_dev).await;
    eventually("A2가 친구 공간에 들어감", || async {
        let st = testkit::space_state(&a1.node, &space)?;
        let st2 = testkit::space_state(&a2.node, &space)?;
        (st.device_belongs(&a, &a2_dev) && st2.is_member(&b.user())).then_some(())
    })
    .await;
    testkit::set_supporter_tier(&a1.node, 5000);
    eventually("내 다른 기기가 후광 표시를 받음", || async {
        flair_of(&a2, a)
            .await
            .filter(|f| f.halo_allowed)
            .map(|_| ())
    })
    .await;

    a1.shutdown().await;
    let c = cl.node("c").await;
    testkit::seed_crew(&a2.node, std::slice::from_ref(&c.node), "시험 크루")
        .await
        .expect("크루 만들기");
    connect_both(&a2, &c).await;
    testkit::authenticate(&a2.node, c.peer())
        .await
        .expect("크루 멤버끼리 인증");
    eventually(
        "계정이 없는 기기가 새 크루에 표시를 나름",
        || async { flair_of(&c, a).await.filter(|f| f.halo_allowed).map(|_| ()) },
    )
    .await;
}

/// 혼자인 크루는 표시를 낼 때 건너뛰므로, 그 뒤 들어온 멤버와 내가 들어간 남의 크루도 후광을 봐야 합니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn supporter_flair_reaches_spaces_joined_after_it_was_published() {
    use yangpa_rpc::api::{CrewCreateParams, InviteCreateParams, InviteJoinStatus, UrlParams};

    async fn crew(n: &TestNode) -> SpaceId {
        n.ok::<m::CrewCreate>(CrewCreateParams {
            name: "시험 크루".into(),
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
                max_uses: Some(1),
                roles: Vec::new(),
            })
            .await
            .url;
        let r = joiner.ok::<m::InviteJoin>(UrlParams { url }).await;
        assert_eq!(r.status, InviteJoinStatus::Joined);
    }

    let cl = Cluster::new();
    let a = cl.node("a").await;
    let user = testkit::user_id(&a.node).expect("사용자");
    let mine = crew(&a).await;
    testkit::set_supporter_tier(&a.node, 5000);
    eventually("내 화면에는 후광이 보임", || async {
        flair_of(&a, user)
            .await
            .filter(|f| f.halo_allowed)
            .map(|_| ())
    })
    .await;

    let c = cl.node("c").await;
    join(&a, &c, mine).await;
    eventually(
        "혼자였던 크루에 든 멤버가 후광 표시를 받음",
        || async {
            flair_of(&c, user)
                .await
                .filter(|f| f.halo_allowed)
                .map(|_| ())
        },
    )
    .await;

    let d = cl.node("d").await;
    let theirs = crew(&d).await;
    join(&d, &a, theirs).await;
    eventually(
        "내가 들어간 크루의 주인이 후광 표시를 받음",
        || async {
            flair_of(&d, user)
                .await
                .filter(|f| f.halo_allowed)
                .map(|_| ())
        },
    )
    .await;
}

/// 후원자 표시는 공간 동기화로 친구와 내 다른 기기에 닿고, 계정이 없는 기기가 고른 후광 색이 확인한 단계를 지우지
/// 않으며, 연결을 해제하면 내려갑니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn supporter_flair_reaches_friends_and_own_devices() {
    use yangpa_rpc::api::{FlairSetParams, SupporterProvider, SupporterUnlinkParams};

    let cl = Cluster::new();
    let a1 = cl.node("a1").await;
    let a = testkit::user_id(&a1.node).expect("사용자");
    let b = cl.node("b").await;
    let space = befriend(&a1, &b).await;
    // 후원자가 아니면 내지 않고, 후광을 고를 수도 없습니다.
    assert!(flair_of(&a1, a).await.is_none());
    let e = a1
        .call::<m::FlairSet>(FlairSetParams { halo: Some([1, 2]) })
        .await
        .expect_err("후원자만");
    assert_eq!(e.code, ERR_FORBIDDEN);

    // 월 14달러: 배너만.
    testkit::set_supporter_tier(&a1.node, 1400);
    eventually("친구가 배너 표시를 받음", || async {
        flair_of(&b, a)
            .await
            .filter(|f| f.banner && !f.halo_allowed && f.halo.is_none())
            .map(|_| ())
    })
    .await;

    // 월 50달러: 후광까지. 계정이 없는 내 다른 기기도 동기화로 받고, 그 기기가 고른 색이 친구에게 갑니다.
    let a2 = common::linked(&cl, &a1, "a2", |_| {}).await;
    let a2_dev = testkit::device_id(&a2.node).expect("새 기기 ID");
    common::wait_own_device(&a1, a2_dev).await;
    eventually("A2가 친구 공간에 들어감", || async {
        let st = testkit::space_state(&a1.node, &space)?;
        let st2 = testkit::space_state(&a2.node, &space)?;
        (st.device_belongs(&a, &a2_dev) && st2.is_member(&b.user())).then_some(())
    })
    .await;
    testkit::set_supporter_tier(&a1.node, 5000);
    eventually("내 다른 기기가 후광 표시를 받음", || async {
        flair_of(&a2, a)
            .await
            .filter(|f| f.banner && f.halo_allowed)
            .map(|_| ())
    })
    .await;
    let colors = [0x112233, 0xABCDEF];
    let v = a2
        .ok::<m::FlairSet>(FlairSetParams { halo: Some(colors) })
        .await;
    assert_eq!(v.halo, Some(colors));
    eventually("친구가 고른 색을 받고 단계는 그대로", || async {
        flair_of(&b, a)
            .await
            .filter(|f| f.banner && f.halo == Some(colors))
            .map(|_| ())
    })
    .await;

    a1.ok::<m::SupporterUnlink>(SupporterUnlinkParams {
        provider: SupporterProvider::Github,
    })
    .await;
    eventually(
        "연결을 해제하면 친구 화면에서 내려감",
        || async { flair_of(&b, a).await.is_none().then_some(()) },
    )
    .await;
}
