//! 모더레이션 영역(W2) 통합 시험: 밴(재가입 차단·위조 가입 거부·해제 뒤 새 초대·최근 메시지 삭제), 차단(보기에서
//! 접기·알림 끄기·친구 요청 조용히 거절·시더 거절, 상대에게 드러나지 않음), 신고(운영진에게만 봉인 전달·처리·
//! 자동화 토큰의 사람 확인).
//!
//! 노드는 한 프로세스 안에서 127.0.0.1로 직접 연결합니다(`common` 참고). 고정 sleep 없이 조건을 기다립니다.

use crate::common;

use common::{Cluster, TestNode, connect_both, eventually, next_notification, wait_message};
use yangpa_core::body::EventBody;
use yangpa_core::event::StreamId;
use yangpa_core::ids::{ChannelId, EventId, RoleId, SpaceId, UserId};
use yangpa_core::model::{ChannelKind, Member};
use yangpa_core::model::{ChannelSettings, ChatTtl, HistoryVisibility, JoinMode, SecurityLevel};
use yangpa_core::wire::{ModerationRequest, WireErrorCode, YpRequest, YpResponse};
use yangpa_daemon::testkit;
use yangpa_daemon::testkit::volatile as vt;
use yangpa_daemon::{ApiHandler, CallCtx};
use yangpa_rpc::api::{
    AllowApiReadParams, ChannelCreateParams, ContactKind, CrewCreateParams, Empty, FriendAddStatus,
    FriendLinkCreateParams, IdParams, InviteCreateParams, InviteJoinStatus, MemberBanParams,
    MemberSetRolesParams, MessageEditParams, MessageListParams, MessageReportParams, Method,
    Notification, ReportAction, ReportListParams, ReportResolveParams, ReportStatus, ReportView,
    RoleCreateParams, Scope, SpaceParams, SpaceUserParams, TokenCreateParams, UrlParams,
    UserParams, m,
};
use yangpa_rpc::jsonrpc::{
    ERR_CONFIRMATION_REQUIRED, ERR_CONFLICT, ERR_FORBIDDEN, ERR_INVALID_INPUT,
};
use yangpa_rpc::{RpcError, RpcHandler};

// ───────────── 도우미 ─────────────

fn code<T: std::fmt::Debug>(r: Result<T, RpcError>) -> i64 {
    r.expect_err("오류여야 합니다").code
}

/// 소유자가 자동화(AI) 토큰을 만들고, 그 토큰으로 인증한 호출 맥락을 돌려줍니다.
async fn automation_token(n: &TestNode, name: &str) -> CallCtx {
    let r = n
        .ok::<m::TokenCreate>(TokenCreateParams {
            name: name.into(),
            scopes: vec![Scope::Read, Scope::Write, Scope::Admin],
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

async fn call_as<M: Method>(
    n: &TestNode,
    ctx: &CallCtx,
    p: M::Params,
) -> Result<M::Result, RpcError> {
    let v = serde_json::to_value(&p).expect("매개변수 직렬화");
    let out = n.node.call(ctx, M::NAME, v).await?;
    Ok(serde_json::from_value(out).expect("결과 형식"))
}

fn confirmation_id(e: &RpcError) -> String {
    assert_eq!(e.code, ERR_CONFIRMATION_REQUIRED, "{e:?}");
    e.data
        .as_ref()
        .and_then(|d| d.get("confirmation_id"))
        .and_then(|v| v.as_str())
        .expect("confirmation_id")
        .to_owned()
}

async fn create_crew(a: &TestNode, name: &str) -> SpaceId {
    a.ok::<m::CrewCreate>(CrewCreateParams {
        name: name.into(),
        description: None,
    })
    .await
    .space
}

async fn invite(a: &TestNode, space: SpaceId) -> String {
    a.ok::<m::InviteCreate>(InviteCreateParams {
        space,
        expires_in_secs: None,
        max_uses: None,
        roles: Vec::new(),
    })
    .await
    .url
}

async fn wait_joined(n: &TestNode, space: SpaceId) {
    let me = n.user();
    eventually(&format!("{} 가입 완료", n.name), || async {
        let st = testkit::space_state(&n.node, &space)?;
        (st.is_member(&me) && testkit::crew_key_seq(&n.node, &space).is_some()).then_some(())
    })
    .await;
}

async fn wait_same_crew_key(nodes: &[&TestNode], space: SpaceId) {
    eventually("같은 크루 키 순번", || async {
        let first = testkit::crew_key_seq(&nodes[0].node, &space)?;
        nodes
            .iter()
            .all(|n| testkit::crew_key_seq(&n.node, &space) == Some(first))
            .then_some(())
    })
    .await;
}

async fn wait_member(n: &TestNode, space: SpaceId, user: UserId, present: bool) {
    eventually(
        &format!("{}가 본 멤버 여부 {present}", n.name),
        || async {
            let st = testkit::space_state(&n.node, &space)?;
            (st.is_member(&user) == present).then_some(())
        },
    )
    .await;
}

async fn wait_banned(n: &TestNode, space: SpaceId, user: UserId, banned: bool) {
    eventually(&format!("{}가 본 밴 여부 {banned}", n.name), || async {
        let st = testkit::space_state(&n.node, &space)?;
        (st.bans.contains_key(&user) == banned).then_some(())
    })
    .await;
}

async fn join(gateway: &TestNode, joiner: &TestNode, space: SpaceId) -> InviteJoinStatus {
    connect_both(gateway, joiner).await;
    let url = invite(gateway, space).await;
    let r = joiner.ok::<m::InviteJoin>(UrlParams { url }).await;
    assert_eq!(r.space, space);
    r.status
}

async fn meet(x: &TestNode, y: &TestNode, space: SpaceId) {
    wait_member(x, space, y.user(), true).await;
    wait_member(y, space, x.user(), true).await;
    testkit::authenticate(&x.node, y.peer())
        .await
        .unwrap_or_else(|e| panic!("{} ↔ {} 인증 실패: {e}", x.name, y.name));
}

/// A가 만든 크루에 나머지가 들어온 상태(서로 모두 연결·인증).
async fn crew_of(cl: &Cluster, names: &[&str]) -> (Vec<TestNode>, SpaceId, ChannelId) {
    let mut nodes = Vec::new();
    for n in names {
        nodes.push(cl.node(n).await);
    }
    let a = &nodes[0];
    let space = create_crew(a, "모더레이션 크루").await;
    let channel = a.first_channel(space).await;
    for x in &nodes[1..] {
        assert_eq!(join(a, x, space).await, InviteJoinStatus::Joined);
        wait_joined(x, space).await;
    }
    let refs: Vec<&TestNode> = nodes.iter().collect();
    wait_same_crew_key(&refs, space).await;
    for x in &nodes {
        for y in &nodes {
            wait_member(x, space, y.user(), true).await;
        }
    }
    for i in 1..nodes.len() {
        for j in (i + 1)..nodes.len() {
            connect_both(&nodes[i], &nodes[j]).await;
            meet(&nodes[i], &nodes[j], space).await;
        }
    }
    (nodes, space, channel)
}

async fn create_role(
    a: &TestNode,
    space: SpaceId,
    name: &str,
    perms: &[&str],
    rank: u32,
) -> RoleId {
    a.ok::<m::RoleCreate>(RoleCreateParams {
        space,
        name: name.into(),
        color: None,
        permissions: perms.iter().map(|s| (*s).to_owned()).collect(),
        rank: Some(rank),
    })
    .await
    .role
}

async fn wait_roles(n: &TestNode, space: SpaceId, user: UserId, roles: &[RoleId]) {
    // 데몬은 역할을 정렬해 저장하므로 순서 없이 비교합니다.
    let mut want = roles.to_vec();
    want.sort_unstable();
    eventually(&format!("{}가 역할 변경을 봄", n.name), || async {
        let st = testkit::space_state(&n.node, &space)?;
        let mut got = st.members.get(&user)?.roles.clone();
        got.sort_unstable();
        (got == want).then_some(())
    })
    .await;
}

async fn sees(n: &TestNode, space: SpaceId, channel: ChannelId, id: EventId) {
    wait_message(
        n,
        space,
        channel,
        id,
        &format!("{}가 메시지를 받음", n.name),
        |_| true,
    )
    .await;
}

async fn reports(n: &TestNode, space: SpaceId) -> Vec<ReportView> {
    n.ok::<m::ReportList>(ReportListParams {
        space,
        status: None,
    })
    .await
}

async fn wait_report(n: &TestNode, space: SpaceId, id: &str, status: ReportStatus) -> ReportView {
    eventually(
        &format!("{}의 신고 {id} 상태 {status:?}", n.name),
        || async {
            reports(n, space)
                .await
                .into_iter()
                .find(|r| r.report_id == id && r.status == status)
        },
    )
    .await
}

fn report_params(
    space: SpaceId,
    channel: ChannelId,
    message: EventId,
    hide: bool,
) -> MessageReportParams {
    MessageReportParams {
        space,
        channel,
        message,
        reason: "욕설과 도배".into(),
        hide,
    }
}

fn resolve(
    space: SpaceId,
    id: &str,
    action: ReportAction,
    note: Option<&str>,
) -> ReportResolveParams {
    ReportResolveParams {
        space,
        report_id: id.to_owned(),
        action,
        note: note.map(str::to_owned),
    }
}

fn not_seeded(r: &YpResponse) -> bool {
    matches!(r, YpResponse::Error { code: WireErrorCode::NotSeeded, message } if message == "이 시더가 제공하지 않는 데이터")
}

// ───────────── 1. 밴: 접근 제거, 옛·새 초대로 재가입 거부, 위조 가입 거부, 해제 뒤 새 초대 ─────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ban_removes_access_and_blocks_rejoin_until_unbanned() {
    let cl = Cluster::new();
    let (nodes, space, channel) = crew_of(&cl, &["a", "b", "c"]).await;
    let (a, b, c) = (&nodes[0], &nodes[1], &nodes[2]);
    let c_dev = testkit::device_id(&c.node).unwrap();
    let before = a.send(space, channel, "밴 전").await;
    sees(c, space, channel, before).await;
    let old_url = invite(a, space).await;

    // 권한 없는 멤버·자동화 토큰.
    let ban_c = MemberBanParams {
        space,
        user: c.user(),
        reason: Some("도배".into()),
        delete_recent: false,
    };
    assert_eq!(
        code(b.call::<m::MemberBan>(ban_c.clone()).await),
        ERR_FORBIDDEN
    );
    let bot = automation_token(a, "정리 봇").await;
    let e = call_as::<m::MemberBan>(a, &bot, ban_c.clone())
        .await
        .expect_err("자동화 토큰은 사람 확인");
    confirmation_id(&e);
    assert_eq!(
        code(
            a.call::<m::MemberBan>(MemberBanParams {
                user: a.user(),
                ..ban_c.clone()
            })
            .await
        ),
        ERR_INVALID_INPUT,
        "자기 자신"
    );

    a.ok::<m::MemberBan>(ban_c.clone()).await;
    for n in [a, b, c] {
        wait_member(n, space, c.user(), false).await;
        wait_banned(n, space, c.user(), true).await;
    }
    assert_eq!(
        code(a.call::<m::MemberBan>(ban_c.clone()).await),
        ERR_CONFLICT
    );
    eventually("C 기기가 공간 그룹에서 빠짐", || async {
        let ma = testkit::key_group_members(&a.node, &space, None)?;
        let mb = testkit::key_group_members(&b.node, &space, None)?;
        (!ma.contains(&c_dev) && ma == mb).then_some(())
    })
    .await;
    let after = a.send(space, channel, "밴 뒤").await;
    sees(b, space, channel, after).await;
    assert!(
        testkit::message_text(&c.node, &after).is_none(),
        "밴된 멤버가 읽음"
    );

    // 밴 목록은 밴 권한이 있는 사람만 봅니다.
    let bans = a.ok::<m::MemberBans>(SpaceParams { space }).await;
    assert_eq!(bans.len(), 1);
    assert_eq!(bans[0].user, c.user());
    assert_eq!(bans[0].reason.as_deref(), Some("도배"));
    assert_eq!(bans[0].banned_by, a.user());
    assert_eq!(bans[0].profile.as_ref().map(|p| p.name.as_str()), Some("c"));
    assert_eq!(
        code(b.call::<m::MemberBans>(SpaceParams { space }).await),
        ERR_FORBIDDEN
    );

    // 옛 초대·새 초대 모두 거부.
    let r = c.call::<m::InviteJoin>(UrlParams { url: old_url }).await;
    assert_eq!(code(r), ERR_FORBIDDEN, "옛 초대로 재가입");
    let new_url = invite(a, space).await;
    let r = c.call::<m::InviteJoin>(UrlParams { url: new_url }).await;
    assert_eq!(code(r), ERR_FORBIDDEN, "새 초대로 재가입");
    assert!(
        !testkit::space_state(&a.node, &space)
            .unwrap()
            .is_member(&c.user())
    );

    // 다른 멤버가 C의 가입 이벤트를 직접 만들어 넣어도 받는 노드마다 거부합니다.
    let forged = EventBody::MemberJoin {
        member: Member {
            user: c.user(),
            roles: Vec::new(),
            nickname: None,
            automation: false,
        },
        devices: testkit::signed_devices(&c.node).unwrap(),
        profile: None,
        invite: None,
    };
    let ev = testkit::author_state_event(&b.node, space, None, &forged).unwrap();
    let id = ev.id();
    let rep = testkit::ingest_from(&a.node, space, vec![ev], b.peer())
        .await
        .unwrap();
    let why = rep
        .rejected
        .iter()
        .find(|(x, _)| *x == id)
        .map(|(_, w)| w.clone())
        .expect("위조 가입이 거부돼야 함");
    assert!(why.contains("밴된"), "{why}");

    // 해제도 자동화 토큰이면 사람 확인. 사람이 풀면 새 초대로 돌아올 수 있습니다.
    let unban = SpaceUserParams {
        space,
        user: c.user(),
    };
    let e = call_as::<m::MemberUnban>(a, &bot, unban.clone())
        .await
        .expect_err("자동화 토큰은 사람 확인");
    confirmation_id(&e);
    assert_eq!(
        code(b.call::<m::MemberUnban>(unban.clone()).await),
        ERR_FORBIDDEN
    );
    a.ok::<m::MemberUnban>(unban.clone()).await;
    for n in [a, b] {
        wait_banned(n, space, c.user(), false).await;
    }
    assert!(
        a.ok::<m::MemberBans>(SpaceParams { space })
            .await
            .is_empty()
    );
    let url = invite(a, space).await;
    let r = c.ok::<m::InviteJoin>(UrlParams { url }).await;
    assert_eq!(r.status, InviteJoinStatus::Joined);
    wait_joined(c, space).await;
    wait_member(b, space, c.user(), true).await;
    let back = c.send(space, channel, "다시 왔어요").await;
    sees(a, space, channel, back).await;
}

// ───────────── 2. 밴과 함께 최근 메시지 삭제 ─────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ban_with_delete_recent_removes_recent_messages_everywhere() {
    let cl = Cluster::new();
    let (mut nodes, space, channel) = crew_of(&cl, &["a", "b", "c"]).await;
    let (a, b, c) = (&nodes[0], &nodes[1], &nodes[2]);
    let c1 = c.send(space, channel, "도배 1").await;
    let c2 = c.send(space, channel, "도배 2").await;
    let keep = b.send(space, channel, "B의 평범한 말").await;
    for n in [a, b] {
        for id in [c1, c2, keep] {
            sees(n, space, channel, id).await;
        }
    }
    let mut rx = b.subscribe();
    a.ok::<m::MemberBan>(MemberBanParams {
        space,
        user: c.user(),
        reason: None,
        delete_recent: true,
    })
    .await;
    for n in [a, b] {
        for id in [c1, c2] {
            let v =
                wait_message(n, space, channel, id, "최근 메시지가 지워짐", |v| v.deleted).await;
            assert!(v.text.is_empty(), "지운 메시지에 본문이 남음");
        }
        let v = wait_message(n, space, channel, keep, "다른 사람 메시지는 남음", |_| true).await;
        assert!(!v.deleted);
        assert_eq!(testkit::message_text(&n.node, &c1), None);
    }
    // 화면이 바로 고칠 수 있게 수정 알림이 갑니다.
    next_notification(&mut rx, "삭제 알림", |n| match n {
        Notification::MessageUpdated { message, .. } if message.id == c1 && message.deleted => {
            Some(())
        }
        _ => None,
    })
    .await;
    // 밴된 쪽 노드도 밴을 받아 자기 사본을 지웁니다.
    eventually("C의 사본도 지워짐", || async {
        testkit::message_text(&c.node, &c2).is_none().then_some(())
    })
    .await;
    let bans = a.ok::<m::MemberBans>(SpaceParams { space }).await;
    assert_eq!(bans[0].reason, None);

    // 밴을 풀고 새 초대로 돌아와 쓴 메시지는 지우지 않습니다. 시작 때 상태를 다시 세우며 밴을 다시 적용해도
    // 마찬가지입니다(지울 구간은 밴 시각까지이고, 끝낸 밴은 되풀이하지 않음).
    a.ok::<m::MemberUnban>(SpaceUserParams {
        space,
        user: c.user(),
    })
    .await;
    for n in [a, b] {
        wait_banned(n, space, c.user(), false).await;
    }
    assert_eq!(join(a, c, space).await, InviteJoinStatus::Joined);
    wait_joined(c, space).await;
    wait_member(b, space, c.user(), true).await;
    let back = c.send(space, channel, "다시 왔어요").await;
    for n in [a, b] {
        let v = wait_message(n, space, channel, back, "돌아와 쓴 메시지", |_| true).await;
        assert!(!v.deleted);
    }
    nodes[0].restart().await;
    let a = &nodes[0];
    let listed = a.messages(space, channel).await;
    let v = listed
        .iter()
        .find(|v| v.id == back)
        .expect("다시 시작 뒤에도 보임");
    assert!(!v.deleted, "다시 시작하며 밴 해제 뒤 메시지를 지움");
    assert_eq!(v.text, "다시 왔어요");
    assert!(listed.iter().any(|v| v.id == c1 && v.deleted));
}

// ───────────── 3. 차단: 접기·알림 끄기·친구 요청 거절·시더 거절, 드러나지 않음 ─────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn block_hides_refuses_and_stops_seeding_without_leaking() {
    let cl = Cluster::new();
    let (nodes, space, channel) = crew_of(&cl, &["a", "b", "c"]).await;
    let (a, b, c) = (&nodes[0], &nodes[1], &nodes[2]);
    // A가 채널을 열어 둠(시더 기본값 '연 채널만').
    a.messages(space, channel).await;

    a.ok::<m::UserBlock>(UserParams { user: b.user() }).await;
    // 같은 사람을 다시 차단해도 그대로, 자기 자신은 안 됨.
    a.ok::<m::UserBlock>(UserParams { user: b.user() }).await;
    assert_eq!(
        code(a.call::<m::UserBlock>(UserParams { user: a.user() }).await),
        ERR_INVALID_INPUT
    );
    let blocked = a.ok::<m::UserBlocked>(Empty).await;
    assert_eq!(blocked.len(), 1);
    assert_eq!(blocked[0].user, b.user());
    assert_eq!(
        blocked[0].profile.as_ref().map(|p| p.name.as_str()),
        Some("b")
    );
    let members = a.ok::<m::MemberList>(SpaceParams { space }).await;
    assert!(members.iter().any(|v| v.user == b.user() && v.blocked));
    assert!(members.iter().any(|v| v.user == c.user() && !v.blocked));

    let mut rx = a.subscribe();
    let from_b = b.send(space, channel, "차단된 사람의 말").await;
    let from_c = c.send(space, channel, "보통 말").await;
    let vb = wait_message(a, space, channel, from_b, "B 메시지 도착", |_| true).await;
    let vc = wait_message(a, space, channel, from_c, "C 메시지 도착", |_| true).await;
    // 내용은 그대로 받아 두고 표시만 합니다.
    assert!(vb.author_blocked);
    assert_eq!(vb.text, "차단된 사람의 말");
    assert!(!vc.author_blocked);
    // 차단한 사람의 새 메시지 알림은 나가지 않습니다. B 메시지를 받아 적용한 뒤에 C가 보낸 표지 메시지의 알림까지
    // 모아 보면, 그 앞에 나갔어야 할 알림은 모두 들어 있습니다(적용은 차례대로).
    let sentinel = c.send(space, channel, "표지").await;
    let mut news = Vec::new();
    next_notification(&mut rx, "표지 메시지 알림", |n| match n {
        Notification::MessageNew { message, .. } => {
            news.push(message.id);
            (message.id == sentinel).then_some(())
        }
        _ => None,
    })
    .await;
    assert!(news.contains(&from_c), "C 메시지 알림이 없음");
    assert!(!news.contains(&from_b), "차단한 사람의 알림이 나감");
    // 안 읽은 수에도 넣지 않습니다.
    let chans = a.ok::<m::ChannelList>(SpaceParams { space }).await;
    let unread = chans.iter().find(|v| v.channel == channel).unwrap().unread;
    assert_eq!(unread, 2, "차단한 사람 메시지가 안 읽은 수에 들어감");

    // 시더: B가 A에게 메시지를 달라고 하면 정책이 빼 둔 데이터와 같은 답. C에게는 줍니다.
    let r = testkit::net_request(
        &b.node,
        a.peer(),
        YpRequest::GetEvents {
            space,
            ids: vec![from_c],
        },
    )
    .await
    .unwrap();
    assert!(not_seeded(&r), "차단한 사람에게 메시지를 줌: {r:?}");
    let r = testkit::net_request(
        &b.node,
        a.peer(),
        YpRequest::GetHeads {
            space,
            stream: StreamId::Channel(channel),
        },
    )
    .await
    .unwrap();
    assert!(not_seeded(&r), "{r:?}");
    // 크루 상태(멤버·역할)는 계속 맞춰야 하므로 줍니다.
    let r = testkit::net_request(
        &b.node,
        a.peer(),
        YpRequest::GetHeads {
            space,
            stream: StreamId::Space,
        },
    )
    .await
    .unwrap();
    assert!(matches!(r, YpResponse::Heads { .. }), "{r:?}");
    let r = testkit::net_request(
        &c.node,
        a.peer(),
        YpRequest::GetEvents {
            space,
            ids: vec![from_b],
        },
    )
    .await
    .unwrap();
    assert!(matches!(&r, YpResponse::Events(e) if e.len() == 1), "{r:?}");

    // 연락처 링크: B에게는 주인이 아직 받지 않은 것처럼 '대기 중'으로 보이지만 A에게는 아무것도 남지 않습니다(임시
    // 연락처도 1:1 공간도 없음). 1회용 링크는 보통처럼 쓰여서, 다른 사람(B의 다른 계정일 수도 있음)이 같은 링크를
    // 쓰면 차단이 없을 때와 같이 거절됩니다.
    let link = a
        .ok::<m::FriendLinkCreate>(FriendLinkCreateParams {
            expires_in_secs: Some(3600),
            single_use: Some(true),
        })
        .await;
    let added = b
        .ok::<m::FriendAdd>(UrlParams {
            url: link.url.clone(),
        })
        .await;
    assert_eq!(added.status, FriendAddStatus::Pending);
    assert!(a.ok::<m::FriendRequests>(Empty).await.is_empty());
    // 같은 사람이 같은 링크로 다시 보내면(응답을 잃은 경우) 보통처럼 다시 '대기 중'.
    let again = b
        .ok::<m::FriendAdd>(UrlParams {
            url: link.url.clone(),
        })
        .await;
    assert_eq!(again.status, FriendAddStatus::Pending);
    assert_eq!(
        code(c.call::<m::FriendAdd>(UrlParams { url: link.url }).await),
        ERR_INVALID_INPUT,
        "차단한 사람이 쓴 1회용 링크가 다른 사람에게 열림"
    );
    let link = a
        .ok::<m::FriendLinkCreate>(FriendLinkCreateParams {
            expires_in_secs: Some(3600),
            single_use: Some(true),
        })
        .await;
    let added = c.ok::<m::FriendAdd>(UrlParams { url: link.url }).await;
    assert_eq!(added.status, FriendAddStatus::Pending);
    let c_user = c.user();
    eventually("차단하지 않은 C는 임시 연락처가 됨", || async {
        a.ok::<m::FriendList>(Empty)
            .await
            .iter()
            .any(|f| f.user == c_user && f.kind == ContactKind::Temporary)
            .then_some(())
    })
    .await;
    let b_user = b.user();
    assert!(
        !a.ok::<m::FriendList>(Empty)
            .await
            .iter()
            .any(|f| f.user == b_user),
        "차단한 사람이 연락처가 됨"
    );
    assert!(a.ok::<m::FriendRequests>(Empty).await.is_empty());

    // B 쪽에는 아무 흔적이 없습니다(차단은 퍼뜨리지 않음). 같은 크루에도 그대로 남습니다.
    assert!(b.ok::<m::UserBlocked>(Empty).await.is_empty());
    let seen_by_b = b.ok::<m::MemberList>(SpaceParams { space }).await;
    assert!(seen_by_b.iter().all(|v| !v.blocked));
    for n in [a, b, c] {
        let st = testkit::space_state(&n.node, &space).unwrap();
        assert!(st.is_member(&a.user()) && st.is_member(&b.user()));
    }
    let later = b.send(space, channel, "차단돼도 전파는 그대로").await;
    sees(a, space, channel, later).await;

    // 풀면 원래대로.
    a.ok::<m::UserUnblock>(UserParams { user: b.user() }).await;
    assert!(a.ok::<m::UserBlocked>(Empty).await.is_empty());
    let v = wait_message(a, space, channel, from_b, "차단 해제", |_| true).await;
    assert!(!v.author_blocked);
    let r = testkit::net_request(
        &b.node,
        a.peer(),
        YpRequest::GetEvents {
            space,
            ids: vec![from_c],
        },
    )
    .await
    .unwrap();
    assert!(matches!(&r, YpResponse::Events(e) if e.len() == 1), "{r:?}");
}

// ───────────── 4. 신고: 운영진에게만, 처리, 자동화 토큰 사람 확인 ─────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reports_reach_only_moderators_and_resolve() {
    let cl = Cluster::new();
    let (nodes, space, channel) = crew_of(&cl, &["a", "b", "c", "d"]).await;
    let (a, b, c, d) = (&nodes[0], &nodes[1], &nodes[2], &nodes[3]);
    // B는 운영진(추방 권한), C는 신고하는 사람, D는 신고당하는 사람.
    let mods = create_role(a, space, "운영진", &["kick"], 10).await;
    a.ok::<m::MemberSetRoles>(MemberSetRolesParams {
        space,
        user: b.user(),
        roles: vec![mods],
    })
    .await;
    for n in [b, c, d] {
        wait_roles(n, space, b.user(), &[mods]).await;
    }
    // 받은 뒤에 쓰게 해서 (논리 시계, ID) 순서가 보낸 순서와 같게 합니다(아래 페이지 검사).
    let early = a.send(space, channel, "처음 공지").await;
    sees(d, space, channel, early).await;
    let m1 = d.send(space, channel, "나쁜 말 1").await;
    let m2 = d.send(space, channel, "나쁜 말 2").await;
    let m3 = d.send(space, channel, "나쁜 말 3").await;
    sees(a, space, channel, m3).await;
    let notice = a.send(space, channel, "공지").await;
    for n in [a, b, c] {
        for id in [early, m1, m2, m3, notice] {
            sees(n, space, channel, id).await;
        }
    }
    let mut rx_a = a.subscribe();
    let mut rx_b = b.subscribe();

    // C가 m1을 신고하며 숨김.
    let r1 = c
        .ok::<m::MessageReport>(report_params(space, channel, m1, true))
        .await
        .report_id;
    for rx in [&mut rx_a, &mut rx_b] {
        let got = next_notification(rx, "신고 알림", |n| match n {
            Notification::ReportNew {
                space: s,
                report_id,
            } if *s == space => Some(report_id.clone()),
            _ => None,
        })
        .await;
        assert_eq!(got, r1);
    }
    for n in [a, b] {
        let v = wait_report(n, space, &r1, ReportStatus::Open).await;
        assert_eq!(v.reporter, c.user());
        assert_eq!(v.message, m1);
        assert_eq!(v.channel, channel);
        assert_eq!(v.reason, "욕설과 도배");
        assert_eq!(v.resolution, None);
    }
    // 운영진이 아닌 사람(신고한 사람·신고당한 사람)은 보지도, 갖고 있지도 않습니다.
    for n in [c, d] {
        assert_eq!(
            code(
                n.call::<m::ReportList>(ReportListParams {
                    space,
                    status: None
                })
                .await
            ),
            ERR_FORBIDDEN
        );
        assert_eq!(testkit::report_count(&n.node, &space), 0);
    }
    // 운영진이 아닌 노드는 신고 전달 자체를 받지 않습니다.
    let r = testkit::net_request(
        &c.node,
        d.peer(),
        YpRequest::Moderation(ModerationRequest::Report {
            space,
            sealed: vec![1, 2, 3],
        }),
    )
    .await
    .unwrap();
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
    // 같은 메시지를 다시 신고하면 같은 신고. 자기 메시지는 신고할 수 없음.
    let again = c
        .ok::<m::MessageReport>(report_params(space, channel, m1, false))
        .await;
    assert_eq!(again.report_id, r1);
    assert_eq!(
        code(
            d.call::<m::MessageReport>(report_params(space, channel, m2, false))
                .await
        ),
        ERR_INVALID_INPUT
    );
    // 신고하며 숨긴 메시지는 C의 목록에서 빠지고, C는 그 메시지를 시더로 내주지 않습니다.
    let listed = c.messages(space, channel).await;
    assert!(listed.iter().all(|v| v.id != m1), "숨긴 메시지가 보임");
    assert!(listed.iter().any(|v| v.id == m2));
    // 숨긴 자리는 더 오래된 메시지로 채웁니다(짧은 페이지는 기록의 시작으로 읽히므로).
    let page = |before: Option<EventId>, limit: u32| {
        c.ok::<m::MessageList>(MessageListParams {
            space,
            channel,
            before,
            limit: Some(limit),
            thread: None,
        })
    };
    let ids =
        |v: Vec<yangpa_rpc::api::MessageView>| v.into_iter().map(|m| m.id).collect::<Vec<_>>();
    assert_eq!(
        ids(page(None, 4).await.messages),
        vec![early, m2, m3, notice],
        "early {early} m1 {m1} m2 {m2} m3 {m3} notice {notice}"
    );
    assert_eq!(
        ids(page(Some(m2), 1).await.messages),
        vec![early],
        "early {early} m1 {m1} m2 {m2}"
    );
    assert!(page(Some(early), 1).await.messages.is_empty());
    let r = testkit::net_request(
        &b.node,
        c.peer(),
        YpRequest::GetEvents {
            space,
            ids: vec![m1],
        },
    )
    .await
    .unwrap();
    assert!(not_seeded(&r), "숨긴 메시지를 내줌: {r:?}");
    let r = testkit::net_request(
        &b.node,
        c.peer(),
        YpRequest::GetEvents {
            space,
            ids: vec![m2],
        },
    )
    .await
    .unwrap();
    assert!(matches!(&r, YpResponse::Events(e) if e.len() == 1), "{r:?}");

    let r2 = c
        .ok::<m::MessageReport>(report_params(space, channel, m2, false))
        .await
        .report_id;
    let r3 = c
        .ok::<m::MessageReport>(report_params(space, channel, m3, false))
        .await
        .report_id;
    for n in [a, b] {
        for id in [&r2, &r3] {
            wait_report(n, space, id, ReportStatus::Open).await;
        }
    }

    // 자동화 토큰: 메시지 삭제 처리는 사람 확인을 거칩니다. 승인하면 실행.
    let bot = automation_token(a, "신고 봇").await;
    let e = call_as::<m::ReportResolve>(
        a,
        &bot,
        resolve(space, &r1, ReportAction::DeleteMessage, None),
    )
    .await
    .expect_err("사람 확인");
    let cid = confirmation_id(&e);
    assert_eq!(
        reports(a, space)
            .await
            .iter()
            .find(|r| r.report_id == r1)
            .unwrap()
            .status,
        ReportStatus::Open
    );
    a.ok::<m::ConfirmApprove>(IdParams { id: cid }).await;
    for n in [a, b, d] {
        wait_message(n, space, channel, m1, "신고된 메시지 삭제", |v| {
            v.deleted
        })
        .await;
    }
    let v = wait_report(a, space, &r1, ReportStatus::Resolved).await;
    assert_eq!(v.resolution.as_deref(), Some("delete_message"));
    // 다른 운영진에게도 처리 결과가 갑니다.
    wait_report(b, space, &r1, ReportStatus::Resolved).await;
    assert_eq!(
        code(
            a.call::<m::ReportResolve>(resolve(space, &r1, ReportAction::Dismiss, None))
                .await
        ),
        ERR_CONFLICT,
        "이미 처리한 신고"
    );

    // 사람(B)이 추방으로 처리.
    b.ok::<m::ReportResolve>(resolve(space, &r2, ReportAction::Kick, Some("경고 무시")))
        .await;
    for n in [a, b, c] {
        wait_member(n, space, d.user(), false).await;
    }
    let v = wait_report(a, space, &r2, ReportStatus::Resolved).await;
    assert_eq!(v.resolution.as_deref(), Some("kick: 경고 무시"));

    // 소유자(A)가 밴으로 처리(이미 나간 사람도 밴 목록에 올라 다시 못 들어옴).
    a.ok::<m::ReportResolve>(resolve(space, &r3, ReportAction::Ban, Some("운영진 메모")))
        .await;
    for n in [a, b, c] {
        wait_banned(n, space, d.user(), true).await;
    }
    let v = wait_report(b, space, &r3, ReportStatus::Resolved).await;
    assert_eq!(v.resolution.as_deref(), Some("ban: 운영진 메모"));
    // 처리 메모는 운영진끼리만: 멤버 모두가 읽는 밴 사유에는 실리지 않습니다.
    let st = testkit::space_state(&c.node, &space).unwrap();
    assert!(st.bans[&d.user()].reason.is_empty(), "처리 메모가 샘");

    // 신고당한 사람이 운영진이면 그 사람에게는 가지 않습니다(A의 공지를 신고 → B만 받음).
    let r4 = c
        .ok::<m::MessageReport>(report_params(space, channel, notice, false))
        .await
        .report_id;
    wait_report(b, space, &r4, ReportStatus::Open).await;
    assert!(reports(a, space).await.iter().all(|r| r.report_id != r4));
    // 무시(dismiss)는 자동화 토큰도 바로 할 수 있습니다.
    let bot_b = automation_token(b, "B의 봇").await;
    call_as::<m::ReportResolve>(b, &bot_b, resolve(space, &r4, ReportAction::Dismiss, None))
        .await
        .expect("무시는 확인 없이");
    let v = wait_report(b, space, &r4, ReportStatus::Resolved).await;
    assert_eq!(v.resolution.as_deref(), Some("dismiss"));
    let open = b
        .ok::<m::ReportList>(ReportListParams {
            space,
            status: Some(ReportStatus::Open),
        })
        .await;
    assert!(open.is_empty(), "{open:?}");
    // 끝까지 신고한 사람 노드에는 신고 기록이 없고, 보낼 것도 남지 않았습니다.
    assert_eq!(testkit::report_count(&c.node, &space), 0);
    eventually("신고 대기열 비움", || async {
        (testkit::moderation_outbox_len(&c.node) == 0).then_some(())
    })
    .await;

    // 크루를 떠나면 받은 신고·보낸 기록·숨김을 모두 지웁니다.
    assert!(testkit::moderation_stored_for_space(&b.node, &space) > 0);
    assert!(testkit::moderation_stored_for_space(&c.node, &space) > 0);
    for n in [b, c] {
        n.ok::<m::CrewLeave>(SpaceParams { space }).await;
        assert_eq!(testkit::report_count(&n.node, &space), 0);
        assert_eq!(testkit::moderation_stored_for_space(&n.node, &space), 0);
        assert_eq!(testkit::moderation_outbox_len(&n.node), 0);
    }
}

// ───────────── 5. 비공개 채널 신고는 그 채널을 볼 수 있는 운영진에게만 ─────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn private_channel_reports_skip_moderators_who_cannot_see_it() {
    let cl = Cluster::new();
    let (nodes, space, _) = crew_of(&cl, &["a", "b", "c", "d"]).await;
    let (a, b, c, d) = (&nodes[0], &nodes[1], &nodes[2], &nodes[3]);
    // B는 추방 권한이 있는 운영진이지만 비밀방을 못 봅니다. C·D만 '비밀' 역할로 봅니다(A는 소유자라 봄).
    let mods = create_role(a, space, "운영진", &["kick"], 10).await;
    let inner = create_role(a, space, "비밀", &[], 5).await;
    for (u, roles) in [
        (b.user(), vec![mods]),
        (c.user(), vec![inner]),
        (d.user(), vec![inner]),
    ] {
        a.ok::<m::MemberSetRoles>(MemberSetRolesParams {
            space,
            user: u,
            roles: roles.clone(),
        })
        .await;
        for n in &nodes {
            wait_roles(n, space, u, &roles).await;
        }
    }
    let secret = a
        .ok::<m::ChannelCreate>(ChannelCreateParams {
            space,
            name: "비밀방".into(),
            kind: ChannelKind::Text,
            parent: None,
            topic: None,
            security: None,
            join_mode: None,
            ttl_secs: None,
            ttl_on_leave: None,
            history: None,
            link_previews: None,
            private_roles: Some(vec![inner]),
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
    for n in [c, d] {
        eventually(
            &format!("{}가 비밀방 그룹에 들어감", n.name),
            || async { testkit::channel_key_seq(&n.node, &space, secret) },
        )
        .await;
    }
    let bad = d.send(space, secret, "비밀방의 나쁜 말").await;
    for n in [a, c] {
        sees(n, space, secret, bad).await;
    }

    let r = c
        .ok::<m::MessageReport>(report_params(space, secret, bad, false))
        .await
        .report_id;
    wait_report(a, space, &r, ReportStatus::Open).await;
    eventually("C의 신고 대기열 비움", || async {
        (testkit::moderation_outbox_len(&c.node) == 0).then_some(())
    })
    .await;
    // 비밀방을 못 보는 운영진 B에게는 신고 이유가 가지 않습니다.
    assert_eq!(
        testkit::report_count(&b.node, &space),
        0,
        "못 보는 채널의 신고"
    );
    assert!(reports(b, space).await.is_empty());

    // 처리 결과도 B에게는 가지 않습니다.
    a.ok::<m::ReportResolve>(resolve(
        space,
        &r,
        ReportAction::Dismiss,
        Some("비밀방 메모"),
    ))
    .await;
    eventually("A의 신고 대기열 비움", || async {
        (testkit::moderation_outbox_len(&a.node) == 0).then_some(())
    })
    .await;
    assert_eq!(testkit::report_count(&b.node, &space), 0);

    // B가 비밀방을 볼 수 있게 되면 그 뒤 신고는 B에게도 갑니다.
    a.ok::<m::MemberSetRoles>(MemberSetRolesParams {
        space,
        user: b.user(),
        roles: vec![mods, inner],
    })
    .await;
    for n in [b, c] {
        wait_roles(n, space, b.user(), &[mods, inner]).await;
    }
    // 비밀방은 지난 기록을 새 구성원에게 열지 않으므로, 확정 기기가 B를 그룹에 넣고(묶음 대기 뒤) D가 그 커밋을
    // 받은 다음에 보내야 B가 읽습니다.
    eventually(
        "B가 비밀방 그룹에 들어가고 D와 같은 세대",
        || async {
            let eb = testkit::channel_key_seq(&b.node, &space, secret)?;
            let ed = testkit::channel_key_seq(&d.node, &space, secret)?;
            (eb == ed).then_some(())
        },
    )
    .await;
    let other = d.send(space, secret, "또 나쁜 말").await;
    for n in [b, c] {
        sees(n, space, secret, other).await;
    }
    let r2 = c
        .ok::<m::MessageReport>(report_params(space, secret, other, false))
        .await
        .report_id;
    // 볼 수 있게 된 B도 받습니다.
    wait_report(b, space, &r2, ReportStatus::Open).await;
}

// ───────────── 6. 상한에 걸린 신고는 버리지 않고 자리가 나면 다시 보냄 ─────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reports_over_limit_wait_and_arrive_after_resolve() {
    let cl = Cluster::new();
    let (nodes, space, channel) = crew_of(&cl, &["a", "c", "d"]).await;
    let (a, c, d) = (&nodes[0], &nodes[1], &nodes[2]);
    // 운영진 기기는 신고한 사람마다 열린 신고를 50건까지 받습니다.
    let mut ids = Vec::new();
    for i in 0..51 {
        ids.push(d.send(space, channel, &format!("도배 {i}")).await);
    }
    eventually("C가 메시지를 모두 받음", || async {
        let seen = c.messages(space, channel).await;
        ids.iter()
            .all(|id| seen.iter().any(|v| v.id == *id))
            .then_some(())
    })
    .await;
    let mut sent = Vec::new();
    for id in &ids {
        sent.push(
            c.ok::<m::MessageReport>(report_params(space, channel, *id, false))
                .await
                .report_id,
        );
    }
    // 넘친 하나는 거절돼 사라지지 않고 C의 대기열에 남습니다.
    eventually("50건 받고 하나는 대기", || async {
        (testkit::report_count(&a.node, &space) == 50
            && testkit::moderation_outbox_len(&c.node) == 1)
            .then_some(())
    })
    .await;
    // 운영진이 하나를 처리하면 자리가 나서 다음 재전송 때 들어옵니다.
    let first = reports(a, space).await[0].report_id.clone();
    a.ok::<m::ReportResolve>(resolve(space, &first, ReportAction::Dismiss, None))
        .await;
    eventually("남은 신고 도착", || async {
        (testkit::report_count(&a.node, &space) == 51
            && testkit::moderation_outbox_len(&c.node) == 0)
            .then_some(())
    })
    .await;
    let mut got: Vec<String> = reports(a, space)
        .await
        .into_iter()
        .map(|r| r.report_id)
        .collect();
    got.sort();
    sent.sort();
    assert_eq!(got, sent);
}

// ───────────── 7. 휘발성 채널 신고: 오래된 메시지, RAM에만 두는 확인, 차단 표시 ─────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn volatile_reports_reach_old_messages_and_stay_in_ram() {
    let cl = Cluster::new();
    let (nodes, space, _) = crew_of(&cl, &["a", "b", "c"]).await;
    let (a, b, c) = (&nodes[0], &nodes[1], &nodes[2]);
    let mut s = ChannelSettings::text("휘발");
    s.security = SecurityLevel::Volatile;
    s.join_mode = JoinMode::Always;
    s.ttl = ChatTtl::None;
    s.history = HistoryVisibility::Private;
    s.link_previews = false;
    let ch = vt::create_channel_with(&a.node, space, s)
        .await
        .expect("휘발성 채널");
    for n in [a, b, c] {
        eventually(&format!("{}가 휘발성 채널을 봄", n.name), || async {
            n.ok::<m::ChannelList>(SpaceParams { space })
                .await
                .into_iter()
                .any(|v| v.channel == ch)
                .then_some(())
        })
        .await;
        // 늘 참가 방식이라 읽기만 해도 참가합니다.
        n.messages(space, ch).await;
    }
    eventually("세션 키 세대가 맞춰짐", || async {
        let gens: Vec<Option<u64>> = [a, b, c]
            .iter()
            .map(|n| vt::volatile_current_generation(&n.node, &space, &ch))
            .collect();
        let first = gens[0]?;
        gens.iter().all(|g| *g == Some(first)).then_some(())
    })
    .await;

    // 목록 한 페이지(200개)보다 오래된 메시지도 신고할 수 있습니다. 휘발성 메시지는 다시 보내 주지 않으므로 하나씩
    // 받은 것을 확인하며 보냅니다(한꺼번에 몰아 보내다 잃지 않게).
    let mut rx_a = a.subscribe();
    let mut rx_c = c.subscribe();
    let mut sent = Vec::new();
    for i in 0..=200 {
        let text = if i == 0 {
            "오래된 나쁜 말".to_owned()
        } else {
            format!("채움 {i}")
        };
        let id = b.send(space, ch, &text).await;
        for rx in [&mut rx_a, &mut rx_c] {
            next_notification(rx, "휘발성 메시지 도착", |n| match n {
                Notification::MessageNew { message, .. } if message.id == id => Some(()),
                _ => None,
            })
            .await;
        }
        sent.push(id);
    }
    let (old, last) = (sent[0], sent[200]);
    let r = c
        .ok::<m::MessageReport>(report_params(space, ch, old, false))
        .await
        .report_id;
    wait_report(a, space, &r, ReportStatus::Open).await;
    // 휘발성 채널 신고는 양쪽 모두 디스크에 남지 않습니다.
    assert_eq!(testkit::moderation_stored_for_space(&a.node, &space), 0);
    assert_eq!(testkit::moderation_stored_for_space(&c.node, &space), 0);

    // 자동화 토큰의 처리 요청은 사람 확인을 기다리는 동안에도 RAM에만 있습니다(신고 ID·메모가 디스크로 가지 않게).
    a.ok::<m::ChannelAllowApiRead>(AllowApiReadParams {
        space,
        channel: ch,
        allowed: true,
    })
    .await;
    let bot = automation_token(a, "휘발 봇").await;
    let e = call_as::<m::ReportResolve>(
        a,
        &bot,
        resolve(space, &r, ReportAction::DeleteMessage, Some("휘발 메모")),
    )
    .await
    .expect_err("사람 확인");
    let cid = confirmation_id(&e);
    assert_eq!(testkit::confirmation_counts(&a.node), (1, 0));
    a.ok::<m::ConfirmApprove>(IdParams { id: cid }).await;
    let v = wait_report(a, space, &r, ReportStatus::Resolved).await;
    assert_eq!(v.resolution.as_deref(), Some("delete_message: 휘발 메모"));
    assert_eq!(testkit::moderation_stored_for_space(&a.node, &space), 0);

    // 차단한 사람의 휘발성 메시지가 고쳐져도 알림의 보기에 차단 표시가 붙습니다.
    c.ok::<m::UserBlock>(UserParams { user: b.user() }).await;
    let mut rx = c.subscribe();
    b.ok::<m::MessageEdit>(MessageEditParams {
        space,
        channel: ch,
        id: last,
        text: "고친 말".into(),
    })
    .await;
    let blocked = next_notification(&mut rx, "휘발성 수정 알림", |n| match n {
        Notification::MessageUpdated { message, .. } if message.id == last => {
            Some(message.author_blocked)
        }
        _ => None,
    })
    .await;
    assert!(
        blocked,
        "차단한 사람의 휘발성 메시지 알림에 차단 표시가 없음"
    );
}
