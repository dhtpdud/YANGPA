//! 크루 영역(B1) 통합 시험: 초대 가입, 3자 대화, 권한·순위(API와 적용 경로), 추방·탈퇴, 비공개 채널, 이모지,
//! 소유권 이전, 1:1→크루 전환, 포럼 게시글 변경 순서, 초대 만료·횟수·폐기, 커밋 확정 노드가 꺼졌을 때, P2P 전용 채널,
//! 신뢰 스위치, 크루 STUN 서버.
//!
//! 노드는 한 프로세스 안에서 127.0.0.1로 직접 연결합니다(`common` 참고). 고정 sleep 없이 조건을 기다립니다.

use crate::common;

use common::{Cluster, TestNode, befriend, connect_both, eventually, next_notification};
use yangpa_core::body::{EventBody, MessageBody};
use yangpa_core::event::{EventKind, StreamId};
use yangpa_core::ids::{BlobId, ChannelId, EventId, RoleId, SpaceId, UserId};
use yangpa_core::links::InviteLink;
use yangpa_core::model::{
    ChannelKind, EmojiLimits, ForumTag, HistoryVisibility, OverwriteTarget, PermissionOverwrite,
    Permissions, SecurityLevel, SpaceKind,
};
use yangpa_core::time::Timestamp;
use yangpa_core::wire::{LamportRange, WireErrorCode, YpRequest, YpResponse};
use yangpa_daemon::CallCtx;
use yangpa_daemon::testkit;
use yangpa_rpc::api::{
    AttachmentInput, ChannelCreateParams, ChannelSetP2pParams, ChannelUpdateParams, ChannelView,
    CrewConvertRequestParams, CrewCreateParams, CrewUpdateParams, EmojiCreateParams,
    EmojiDeleteParams, ForumCreatePostParams, ForumFeaturedParams, ForumListPostsParams,
    ForumPostParams, ForumPostView, ForumSort, ForumUpdatePostParams, InviteCreateParams,
    InviteJoinStatus, InviteRevokeParams, MarkReadParams, MemberKickParams, MemberSetRolesParams,
    MessageEditParams, MessageRefParams, MessageSendParams, RoleCreateParams, RoleUpdateParams,
    SortOrder, SpaceChannelParams, SpaceParams, TagMatch, TransferOwnershipParams, UrlParams, m,
};
use yangpa_rpc::api::{ConnectionMode, TrustSetSpaceParams, TrustSetUserParams, UserParams};
use yangpa_rpc::api::{CrewServersParams, CrewUseServersParams};
use yangpa_rpc::api::{Empty, FileCircuitsParams, MessageListParams};
use yangpa_rpc::api::{Method, Notification, Scope};
use yangpa_rpc::jsonrpc::{
    ERR_CONFIRMATION_REQUIRED, ERR_CONFLICT, ERR_FORBIDDEN, ERR_INVALID_INPUT, ERR_NOT_FOUND,
};
use yangpa_rpc::{AuthInfo, RpcError};

// ───────────── 도우미 ─────────────

fn code(r: Result<impl std::fmt::Debug, RpcError>) -> i64 {
    match r {
        Ok(v) => panic!("실패해야 하는데 성공함: {v:?}"),
        Err(e) => e.code,
    }
}

/// AI용(자동화) API 토큰으로 부릅니다.
async fn automation_call<M: Method>(n: &TestNode, p: M::Params) -> Result<M::Result, RpcError> {
    let ctx = CallCtx::from_auth(AuthInfo {
        token_id: "ai".into(),
        token_name: "시험 AI".into(),
        scopes: vec![Scope::Owner],
        spaces: Vec::new(),
        automation: true,
    });
    let v = serde_json::to_value(&p).unwrap();
    let out = n.node.call(&ctx, M::NAME, v).await?;
    Ok(serde_json::from_value(out).unwrap())
}

async fn create_crew(a: &TestNode, name: &str) -> SpaceId {
    a.ok::<m::CrewCreate>(CrewCreateParams {
        name: name.into(),
        description: None,
    })
    .await
    .space
}

async fn invite(a: &TestNode, space: SpaceId, max_uses: Option<u32>, secs: Option<u64>) -> String {
    a.ok::<m::InviteCreate>(InviteCreateParams {
        space,
        expires_in_secs: secs,
        max_uses,
        roles: Vec::new(),
    })
    .await
    .url
}

/// 멤버이고 크루 키 그룹에 들어갈 때까지.
async fn wait_joined(n: &TestNode, space: SpaceId) {
    let me = n.user();
    eventually(&format!("{} 가입 완료", n.name), || async {
        let st = testkit::space_state(&n.node, &space)?;
        (st.is_member(&me) && testkit::crew_key_seq(&n.node, &space).is_some()).then_some(())
    })
    .await;
}

/// 모두 같은 크루 키 순번에 이를 때까지(키 갱신을 다 받음).
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

/// `joiner`가 `gateway`의 링크로 들어옵니다(커밋 확정 노드가 받아 주면 바로 가입).
async fn join(gateway: &TestNode, joiner: &TestNode, space: SpaceId) -> InviteJoinStatus {
    connect_both(gateway, joiner).await;
    let url = invite(gateway, space, None, None).await;
    let r = joiner.ok::<m::InviteJoin>(UrlParams { url }).await;
    assert_eq!(r.space, space);
    r.status
}

/// 서로 멤버로 본 뒤 직접 인증합니다. 시험에는 기기 위치 찾기(DHT·`locator`)가 없어서, 가입 전에 맺은 연결은 그대로
/// 두면 인증되지 않은 채 남고 두 사람 사이의 직접 전파가 닿지 않습니다.
async fn meet(x: &TestNode, y: &TestNode, space: SpaceId) {
    wait_member(x, space, y.user(), true).await;
    wait_member(y, space, x.user(), true).await;
    testkit::authenticate(&x.node, y.peer())
        .await
        .unwrap_or_else(|e| panic!("{} ↔ {} 인증 실패: {e}", x.name, y.name));
}

/// A가 만든 크루에 B·C가 들어온 상태(서로 모두 연결·인증).
async fn three_member_crew(cl: &Cluster) -> (TestNode, TestNode, TestNode, SpaceId, ChannelId) {
    three_member_crew_with(cl, |_| {}).await
}

async fn three_member_crew_with(
    cl: &Cluster,
    tweak: fn(&mut yangpa_daemon::NodeConfig),
) -> (TestNode, TestNode, TestNode, SpaceId, ChannelId) {
    let a = cl.node_with("a", tweak).await;
    let b = cl.node_with("b", tweak).await;
    let c = cl.node_with("c", tweak).await;
    let space = create_crew(&a, "양파 크루").await;
    let channel = a.first_channel(space).await;
    connect_both(&b, &c).await;
    assert_eq!(join(&a, &b, space).await, InviteJoinStatus::Joined);
    assert_eq!(join(&a, &c, space).await, InviteJoinStatus::Joined);
    wait_joined(&b, space).await;
    wait_joined(&c, space).await;
    wait_same_crew_key(&[&a, &b, &c], space).await;
    for n in [&a, &b, &c] {
        for u in [a.user(), b.user(), c.user()] {
            wait_member(n, space, u, true).await;
        }
    }
    meet(&b, &c, space).await;
    (a, b, c, space, channel)
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

async fn set_roles(a: &TestNode, space: SpaceId, user: UserId, roles: Vec<RoleId>) {
    a.ok::<m::MemberSetRoles>(MemberSetRolesParams { space, user, roles })
        .await;
}

async fn wait_roles(n: &TestNode, space: SpaceId, user: UserId, roles: &[RoleId]) {
    let mut want = roles.to_vec();
    want.sort();
    eventually(&format!("{}가 역할 변경을 봄", n.name), || async {
        let st = testkit::space_state(&n.node, &space)?;
        let mut have = st.members.get(&user)?.roles.clone();
        have.sort();
        (have == want).then_some(())
    })
    .await;
}

fn channel_params(space: SpaceId, name: &str, kind: ChannelKind) -> ChannelCreateParams {
    ChannelCreateParams {
        space,
        name: name.into(),
        kind,
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
    }
}

async fn wait_channel(n: &TestNode, space: SpaceId, channel: ChannelId) {
    eventually(&format!("{}가 채널을 앎", n.name), || async {
        testkit::space_state(&n.node, &space)?
            .channels
            .contains_key(&channel)
            .then_some(())
    })
    .await;
}

async fn sees_text(n: &TestNode, space: SpaceId, channel: ChannelId, text: &str) {
    eventually(&format!("{}가 '{text}'를 받음", n.name), || async {
        n.messages(space, channel)
            .await
            .iter()
            .any(|v| v.text == text)
            .then_some(())
    })
    .await;
}

fn send_params(
    space: SpaceId,
    channel: ChannelId,
    text: &str,
    thread: Option<EventId>,
) -> MessageSendParams {
    MessageSendParams {
        space,
        channel,
        text: text.into(),
        attachments: Vec::new(),
        reply_to: None,
        thread,
    }
}

// ───────────── 1. 초대 → 두 사람 가입 → 3자 대화 ─────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crew_invite_two_joiners_and_three_way_messages() {
    let cl = Cluster::new();
    let (a, b, c, space, channel) = three_member_crew(&cl).await;

    let sa = a.send(space, channel, "A 안녕").await;
    let sb = b.send(space, channel, "B 안녕").await;
    let sc = c.send(space, channel, "C 안녕").await;
    for n in [&a, &b, &c] {
        for t in ["A 안녕", "B 안녕", "C 안녕"] {
            sees_text(n, space, channel, t).await;
        }
    }
    assert!(sa != sb && sb != sc);

    for n in [&a, &b, &c] {
        let members = n.ok::<m::MemberList>(SpaceParams { space }).await;
        assert_eq!(members.len(), 3, "{}의 멤버 목록", n.name);
        let list = n.ok::<m::SpaceList>(yangpa_rpc::api::Empty).await;
        let s = list
            .iter()
            .find(|s| s.space == space)
            .expect("크루가 목록에 있음");
        assert_eq!(s.kind, SpaceKind::Crew);
        assert_eq!(s.member_count, 3);
        assert_eq!(s.name, "양파 크루");
    }
    // 이미 멤버면 같은 링크를 다시 써도 그대로 가입 상태를 돌려줍니다.
    let url = invite(&a, space, None, None).await;
    let again = b.ok::<m::InviteJoin>(UrlParams { url }).await;
    assert_eq!(again.status, InviteJoinStatus::Joined);
}

// ───────────── 2. 권한·순위: API와 적용 경로 ─────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn permissions_and_ranks_are_enforced_by_api_and_ingest() {
    let cl = Cluster::new();
    let (a, b, c, space, _) = three_member_crew(&cl).await;

    // 역할 없는 B.
    let r = b
        .call::<m::ChannelCreate>(channel_params(space, "몰래", ChannelKind::Text))
        .await;
    assert_eq!(code(r), ERR_FORBIDDEN);
    let r = b
        .call::<m::MemberKick>(MemberKickParams {
            space,
            user: c.user(),
            reason: None,
        })
        .await;
    assert_eq!(code(r), ERR_FORBIDDEN);
    let r = b
        .call::<m::InviteCreate>(InviteCreateParams {
            space,
            expires_in_secs: None,
            max_uses: None,
            roles: Vec::new(),
        })
        .await;
    assert_eq!(code(r), ERR_FORBIDDEN);

    // B: 순위 10(추방·역할 관리), C: 순위 20.
    let mid = create_role(&a, space, "운영", &["kick", "manage_roles"], 10).await;
    let high = create_role(&a, space, "고참", &[], 20).await;
    set_roles(&a, space, b.user(), vec![mid]).await;
    set_roles(&a, space, c.user(), vec![high]).await;
    wait_roles(&b, space, b.user(), &[mid]).await;
    wait_roles(&b, space, c.user(), &[high]).await;

    let r = b
        .call::<m::MemberKick>(MemberKickParams {
            space,
            user: c.user(),
            reason: Some("순위 시험".into()),
        })
        .await;
    assert_eq!(code(r), ERR_FORBIDDEN, "낮은 순위가 높은 순위를 추방");
    let r = b
        .call::<m::MemberSetRoles>(MemberSetRolesParams {
            space,
            user: c.user(),
            roles: Vec::new(),
        })
        .await;
    assert_eq!(code(r), ERR_FORBIDDEN, "높은 순위 멤버의 역할을 뺌");
    let r = b
        .call::<m::RoleCreate>(RoleCreateParams {
            space,
            name: "같은 순위".into(),
            color: None,
            permissions: Vec::new(),
            rank: Some(10),
        })
        .await;
    assert_eq!(code(r), ERR_FORBIDDEN, "자기 순위와 같은 역할");
    let r = b
        .call::<m::RoleCreate>(RoleCreateParams {
            space,
            name: "관리자 흉내".into(),
            color: None,
            permissions: vec!["administrator".into()],
            rank: Some(5),
        })
        .await;
    assert_eq!(code(r), ERR_FORBIDDEN, "없는 권한을 줌");
    let r = b
        .call::<m::RoleCreate>(RoleCreateParams {
            space,
            name: "x".into(),
            color: None,
            permissions: vec!["없는권한".into()],
            rank: Some(5),
        })
        .await;
    assert_eq!(code(r), ERR_INVALID_INPUT);
    let low = create_role(&b, space, "새내기", &["kick"], 5).await;
    wait_role_known(&a, space, low).await;

    // 적용 경로: B가 권한 없이 서명한 상태 이벤트는 A가 받아도 거부합니다.
    let sneaky = ChannelId::random();
    let forged = [
        EventBody::ChannelCreate {
            channel: sneaky,
            settings: yangpa_core::model::ChannelSettings::text("몰래"),
        },
        EventBody::RoleUpdate {
            role: yangpa_core::model::Role {
                id: mid,
                name: "운영".into(),
                color: 0,
                rank: 30,
                permissions: yangpa_core::model::Permissions::ADMINISTRATOR,
                automation: false,
            },
        },
        EventBody::MemberKick {
            user: c.user(),
            reason: String::new(),
        },
        EventBody::OwnershipTransfer {
            new_owner: b.user(),
            cert: testkit::checkpoint::ownership_cert(&b.node, space, b.user()).unwrap(),
        },
    ];
    for body in forged {
        let ev = testkit::author_state_event(&b.node, space, None, &body).unwrap();
        let id = ev.id();
        let rep = testkit::ingest_from(&a.node, space, vec![ev], b.peer())
            .await
            .unwrap();
        assert!(
            rep.is_rejected(&id),
            "위조 상태 이벤트가 받아들여짐: {body:?}"
        );
    }
    let st = testkit::space_state(&a.node, &space).unwrap();
    assert!(!st.channels.contains_key(&sneaky));
    assert!(st.is_member(&c.user()));
    assert_eq!(st.owner, a.user());
    assert_eq!(st.roles.get(&mid).unwrap().rank, 10);
    // 멤버가 아닌 노드가 만든 상태 이벤트도 거부합니다.
    let d = cl.node("d").await;
    let ev = testkit::author_state_event(
        &d.node,
        space,
        None,
        &EventBody::MemberLeave { user: c.user() },
    );
    assert!(ev.is_err(), "멤버가 아니면 상태 키가 없어 만들 수도 없음");
}

/// 새 역할이 이 노드에 퍼질 때까지.
async fn wait_role_known(n: &TestNode, space: SpaceId, role: RoleId) {
    eventually("새 역할이 퍼짐", || async {
        testkit::space_state(&n.node, &space)?
            .roles
            .contains_key(&role)
            .then_some(())
    })
    .await;
}

// ───────────── 3. 추방 ─────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn kicked_member_cannot_read_new_messages_and_is_refused() {
    let cl = Cluster::new();
    let (a, b, c, space, channel) = three_member_crew(&cl).await;
    let c_dev = testkit::device_id(&c.node).unwrap();
    let old = a.send(space, channel, "추방 전").await;
    for n in [&b, &c] {
        sees_text(n, space, channel, "추방 전").await;
    }

    a.ok::<m::MemberKick>(MemberKickParams {
        space,
        user: c.user(),
        reason: Some("시험".into()),
    })
    .await;
    wait_member(&a, space, c.user(), false).await;
    wait_member(&b, space, c.user(), false).await;
    // 쫓겨난 노드에도 알려 그 화면이 멤버가 아님을 보여 줍니다.
    wait_member(&c, space, c.user(), false).await;
    // 확정 기기(A)가 C의 기기를 키 그룹에서 빼고(회전) B도 그 갱신을 받음.
    eventually("C 기기가 공간 그룹에서 빠짐", || async {
        let ma = testkit::key_group_members(&a.node, &space, None)?;
        let mb = testkit::key_group_members(&b.node, &space, None)?;
        (!ma.contains(&c_dev) && ma == mb).then_some(())
    })
    .await;
    wait_same_crew_key(&[&a, &b], space).await;

    let new = a.send(space, channel, "추방 뒤").await;
    sees_text(&b, space, channel, "추방 뒤").await;
    // 억지로 넣어도 C는 새 기록 키가 없어 읽지 못합니다(기존 메시지는 남아 있음).
    let evs = testkit::stream_events(&a.node, &space, StreamId::Channel(channel)).unwrap();
    testkit::ingest_from(&c.node, space, evs, a.peer())
        .await
        .unwrap();
    assert_eq!(
        testkit::message_text(&c.node, &old).as_deref(),
        Some("추방 전")
    );
    assert!(
        testkit::message_text(&c.node, &new).is_none(),
        "추방된 멤버가 새 메시지를 읽음"
    );
    // 새 상태 키로 쓴 상태 이벤트도 열지 못합니다.
    let later = a
        .ok::<m::ChannelCreate>(channel_params(space, "추방 뒤 채널", ChannelKind::Text))
        .await
        .channel;
    wait_channel(&b, space, later).await;
    let evs = testkit::stream_events(&a.node, &space, StreamId::Space).unwrap();
    testkit::ingest_from(&c.node, space, evs, a.peer())
        .await
        .unwrap();
    let st = testkit::space_state(&c.node, &space).unwrap();
    assert!(!st.channels.contains_key(&later), "추방 뒤 상태를 읽음");
    // 시더는 쫓겨난 멤버의 요청을 거절합니다.
    let r = testkit::net_request(
        &c.node,
        a.peer(),
        YpRequest::GetHeads {
            space,
            stream: StreamId::Channel(channel),
        },
    )
    .await
    .unwrap();
    assert!(matches!(r, YpResponse::Error { .. }), "시더가 답함: {r:?}");
    // 쫓겨난 쪽은 API로 쓸 수 없고, 나가기를 부르면 남은 데이터를 지웁니다.
    let r = c
        .call::<m::MessageSend>(send_params(space, channel, "아직 있니", None))
        .await;
    assert_eq!(code(r), ERR_FORBIDDEN);
    c.ok::<m::CrewLeave>(SpaceParams { space }).await;
    assert!(!testkit::known_spaces(&c.node).contains(&space));
}

// ───────────── 4. 비공개 채널 역할 더하기·빼기 ─────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn private_channel_follows_role_add_and_remove() {
    let cl = Cluster::new();
    let (a, b, c, space, _) = three_member_crew(&cl).await;
    let b_dev = testkit::device_id(&b.node).unwrap();
    let role = create_role(&a, space, "비밀", &[], 5).await;
    let mut p = channel_params(space, "비밀방", ChannelKind::Text);
    p.private_roles = Some(vec![role]);
    p.history = Some(HistoryVisibility::Public);
    let secret = a.ok::<m::ChannelCreate>(p).await.channel;
    eventually("A가 비공개 채널 그룹을 만듦", || async {
        testkit::channel_key_seq(&a.node, &space, secret)
    })
    .await;
    let m1 = a.send(space, secret, "역할 전 비밀").await;
    for n in [&b, &c] {
        wait_channel(n, space, secret).await;
        let chans = n.ok::<m::ChannelList>(SpaceParams { space }).await;
        assert!(
            chans.iter().all(|ch| ch.channel != secret),
            "볼 수 없는 채널이 목록에 나옴"
        );
        let r = n
            .call::<m::MessageList>(yangpa_rpc::api::MessageListParams {
                space,
                channel: secret,
                before: None,
                limit: None,
                thread: None,
            })
            .await;
        assert_eq!(code(r), ERR_FORBIDDEN);
    }

    // B에게 역할: 채널 그룹에 들어가고, 과거 기록 공개라 지난 메시지도 읽음.
    set_roles(&a, space, b.user(), vec![role]).await;
    eventually("B가 채널 그룹에 들어감", || async {
        testkit::channel_key_seq(&b.node, &space, secret)
    })
    .await;
    sees_text(&b, space, secret, "역할 전 비밀").await;
    b.send(space, secret, "B의 비밀").await;
    sees_text(&a, space, secret, "B의 비밀").await;
    assert!(
        testkit::message_text(&c.node, &m1).is_none(),
        "역할 없는 C가 읽음"
    );

    // 역할을 빼면 그룹에서 빠지고 그 뒤 메시지는 받아도 못 읽습니다.
    set_roles(&a, space, b.user(), Vec::new()).await;
    eventually("B 기기가 채널 그룹에서 빠짐", || async {
        let ma = testkit::key_group_members(&a.node, &space, Some(secret))?;
        (!ma.contains(&b_dev)).then_some(())
    })
    .await;
    let m3 = a.send(space, secret, "역할 뒤 비밀").await;
    let evs = testkit::stream_events(&a.node, &space, StreamId::Channel(secret)).unwrap();
    testkit::ingest_from(&b.node, space, evs.clone(), a.peer())
        .await
        .unwrap();
    testkit::ingest_from(&c.node, space, evs, a.peer())
        .await
        .unwrap();
    assert!(
        testkit::message_text(&b.node, &m3).is_none(),
        "역할을 잃은 B가 읽음"
    );
    assert!(testkit::message_text(&c.node, &m3).is_none());
    assert!(testkit::message_text(&c.node, &m1).is_none());
    wait_roles(&b, space, b.user(), &[]).await;
    let chans = b.ok::<m::ChannelList>(SpaceParams { space }).await;
    assert!(chans.iter().all(|ch| ch.channel != secret));
}

// ───────────── 4-0. gossip 이웃 ─────────────

/// gossip을 쓰는 크루에서 연결이 끊긴 멤버 기기는 이웃 고르기의 다음 바퀴 대상이 됩니다(최근 연결에 성공한 기록이
/// 있음). 직접 보내는 작은 크루에서는 이웃을 고르지 않습니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gossip_crew_plans_dropped_neighbor() {
    let cl = Cluster::new();
    let (a, b, c, _space, _channel) = three_member_crew_with(&cl, |c| {
        c.push_max_devices = 0;
    })
    .await;
    // 모두 연결돼 있으면 걸 곳이 없습니다.
    assert!(testkit::neighbor_plan(&b.node).is_empty());
    c.shutdown().await;
    // 연결이 닫힌 것과 인증 표에서 빠진 것은 따로 처리되므로(바쁠 때 몇 ms 차이) 계획 자체를 기다립니다.
    for n in [&b, &a] {
        eventually("C가 이웃 계획에 들어감", || async {
            (testkit::neighbor_plan(&n.node) == vec![c.peer()]).then_some(())
        })
        .await;
    }

    let cl2 = Cluster::new();
    let (_a, b, c, _space, _channel) = three_member_crew(&cl2).await;
    c.shutdown().await;
    eventually("B가 C 끊김을 앎", || async {
        (!testkit::is_connected(&b.node, c.peer())).then_some(())
    })
    .await;
    assert!(testkit::neighbor_plan(&b.node).is_empty());
}

/// `member.search`: 온라인 → 최근 순, 거르기와 이름 검색, 쪽 나누기.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn member_search_filters_online_recent_and_name() {
    use yangpa_rpc::api::{MemberFilter, MemberSearchParams};

    let cl = Cluster::new();
    let (a, b, c, space, _channel) = three_member_crew(&cl).await;
    let search = |filter, query: Option<&str>, offset, limit| MemberSearchParams {
        space,
        query: query.map(str::to_owned),
        filter,
        offset,
        limit,
    };
    // C의 프로필이 B에 닿은 뒤.
    eventually("B가 이름으로 C를 찾음", || async {
        let page = b
            .ok::<m::MemberSearch>(search(MemberFilter::All, Some("C"), None, None))
            .await;
        page.members
            .iter()
            .any(|m| m.user == c.user())
            .then_some(())
    })
    .await;
    let page = b
        .ok::<m::MemberSearch>(search(MemberFilter::All, Some("c"), None, None))
        .await;
    assert!(page.members.iter().all(
        |m| m.profile.name.contains('c') || m.user.to_string().to_lowercase().starts_with('c')
    ));
    assert_eq!(page.total as usize, page.members.len());

    // C가 B에게 보이는 상태로 이어진 뒤(인증 직후 표시 상태를 받기 전에는 아직 보이지 않음) 끕니다.
    eventually("B가 C를 온라인으로 봄", || async {
        let page = b
            .ok::<m::MemberSearch>(search(MemberFilter::Online, None, None, None))
            .await;
        (page.online == 3).then_some(())
    })
    .await;
    c.shutdown().await;
    // 연결이 닫힌 뒤 온라인 표에서 빠지기까지 바쁠 때 조금 걸려 결과를 기다립니다.
    let online = eventually("B가 C 끊김을 앎", || async {
        let page = b
            .ok::<m::MemberSearch>(search(MemberFilter::Online, None, None, None))
            .await;
        (page.online == 2).then_some(page)
    })
    .await;
    let users: Vec<UserId> = online.members.iter().map(|m| m.user).collect();
    assert_eq!(online.online, 2);
    assert_eq!(online.total, 2);
    assert!(users.contains(&a.user()) && users.contains(&b.user()));
    let recent = b
        .ok::<m::MemberSearch>(search(MemberFilter::Recent, None, None, None))
        .await;
    assert_eq!(recent.total, 3);
    let last = recent.members.last().expect("C");
    assert_eq!(last.user, c.user());
    assert!(!last.online && last.last_seen.is_some());
    // 쪽 나누기: 둘째 쪽은 나머지.
    let second = b
        .ok::<m::MemberSearch>(search(MemberFilter::All, None, Some(2), Some(2)))
        .await;
    assert_eq!((second.total, second.members.len()), (3, 1));
    assert_eq!(second.members[0].user, c.user());
}

// ───────────── 4-0b. 채널 묶음·알림 토픽(선택 기능) ─────────────

/// `@everyone`·`@here`는 받는 쪽이 작성자의 그 채널 `mention_many` 권한을 보고 나를 멘션한 글로 칩니다(안 읽은 멘션
/// 수에도 셈). 권한 없는 멤버(기본 @everyone 역할)가 쓰면 글자로만 남고, 작성자 자신에게는 멘션이 아닙니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn everyone_mentions_need_mention_many() {
    let cl = Cluster::new();
    let (a, b, c, space, channel) = three_member_crew(&cl).await;
    let id = a.send(space, channel, "@everyone 공지").await;
    for n in [&b, &c] {
        common::wait_message(
            n,
            space,
            channel,
            id,
            "소유자의 @everyone은 멘션",
            |v| v.mentions_me,
        )
        .await;
    }
    eventually("B의 안 읽은 멘션", || async {
        b.ok::<m::SpaceList>(Empty)
            .await
            .into_iter()
            .find(|s| s.space == space)
            .filter(|s| s.mention > 0)
    })
    .await;
    let mine = common::wait_message(&a, space, channel, id, "A의 글", |_| true).await;
    assert!(!mine.mentions_me, "내 글은 나를 멘션하지 않음");
    let id = b.send(space, channel, "@here 다들 모여").await;
    let v = common::wait_message(&a, space, channel, id, "A가 B의 글을 받음", |_| true).await;
    assert!(!v.mentions_me, "권한 없는 멤버의 @here는 멘션이 아님");
}

/// 채널 토픽을 켜면 모두 보는 채널 글은 그 채널 묶음 토픽으로 가고(연 채널의 묶음만 구독), 멘션은 알림 토픽으로
/// 따라가지 않는 멤버에게도 닿습니다. 알림 토픽에 멘션 없는 글이나 다른 묶음에 실린 글은 퍼뜨리지 않습니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn channel_topics_route_followed_buckets_and_mentions() {
    use yangpa_core::wire::{GossipTopic, channel_bucket};
    use yangpa_daemon::testkit::net as tnet;
    use yangpa_rpc::api::ChannelTopicsParams;

    let cl = Cluster::new();
    let (a, b, c, space, channel) = three_member_crew_with(&cl, |c| c.push_max_devices = 0).await;
    for n in [&a, &b, &c] {
        n.ok::<m::NetSetChannelTopics>(ChannelTopicsParams { enabled: true })
            .await;
        assert!(n.ok::<m::NetChannelTopics>(Empty).await.enabled);
    }
    let bucket = channel_bucket(&space, &channel);
    b.ok::<m::ChannelMarkRead>(MarkReadParams {
        space,
        channel,
        event: None,
    })
    .await;
    eventually("B가 채널 묶음을 구독", || async {
        (testkit::channel_buckets(&b.node, space) == vec![bucket]).then_some(())
    })
    .await;
    assert!(testkit::channel_buckets(&c.node, space).is_empty());

    // A가 B의 구독을 알 때까지: 묶음 토픽으로 (크루 토픽으로 돌리지 않고) 올라간 글이 B에 닿음.
    let mut warm = None;
    for i in 0..40 {
        let before = testkit::channel_topic_sends(&a.node);
        let id = a.send(space, channel, &format!("데우기 {i}")).await;
        eventually("B가 글을 받음", || async {
            testkit::get_event(&b.node, &id).ok().flatten()
        })
        .await;
        let after = testkit::channel_topic_sends(&a.node);
        if after.0 > before.0 && after.1 == before.1 {
            warm = Some(id);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    let warm = warm.expect("묶음 토픽으로 올라감");

    // 멘션: 따라가지 않는 C에게 알림 토픽으로 닿고 안 읽은 멘션으로 셉니다.
    let mut got = false;
    for i in 0..40 {
        let before = testkit::channel_topic_sends(&a.node).2;
        let id = a
            .send(space, channel, &format!("<@{}> 봐요 {i}", c.user()))
            .await;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while testkit::channel_topic_sends(&a.node).2 == before
            && std::time::Instant::now() < deadline
        {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        if testkit::channel_topic_sends(&a.node).2 > before {
            eventually("C가 멘션을 받음", || async {
                testkit::get_event(&c.node, &id).ok().flatten()
            })
            .await;
            got = true;
            break;
        }
    }
    assert!(got, "알림 토픽으로 올라감");
    eventually("C의 안 읽은 멘션", || async {
        c.ok::<m::SpaceList>(Empty)
            .await
            .into_iter()
            .find(|s| s.space == space)
            .filter(|s| s.mention > 0)
    })
    .await;

    // 멘션 없는 글을 알림 토픽에, 다른 묶음 토픽에 실으면 퍼뜨리지 않습니다.
    let ev = testkit::get_event(&a.node, &warm).unwrap().unwrap();
    let v = tnet::gossip_verdict_on(
        &c.node,
        a.peer(),
        GossipTopic::Notify(space),
        vec![ev.clone()],
    )
    .await;
    assert_eq!(v, yangpa_net::GossipVerdict::Ignore);
    let other = GossipTopic::Bucket(space, (bucket + 1) % yangpa_core::wire::CHANNEL_BUCKETS);
    let v = tnet::gossip_verdict_on(&b.node, a.peer(), other, vec![ev]).await;
    assert_eq!(v, yangpa_net::GossipVerdict::Ignore);

    // 안 읽음 힌트: 묶음으로만 간 새 글은 C에 오지 않고, 멤버에게 끝을 물으면 has_new가 켜지며, 열면 받아 옵니다.
    let mut fresh = None;
    for i in 0..40 {
        let before = testkit::channel_topic_sends(&a.node);
        let id = a.send(space, channel, &format!("새 글 {i}")).await;
        eventually("B가 새 글을 받음", || async {
            testkit::get_event(&b.node, &id).ok().flatten()
        })
        .await;
        let after = testkit::channel_topic_sends(&a.node);
        if after.0 > before.0 && after.1 == before.1 {
            fresh = Some(id);
            break;
        }
    }
    let fresh = fresh.expect("묶음 토픽으로 올라감");
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(testkit::get_event(&c.node, &fresh).unwrap().is_none());
    let flag = || async {
        c.ok::<m::ChannelList>(SpaceParams { space })
            .await
            .into_iter()
            .find(|v| v.channel == channel)
            .map(|v| v.has_new)
            .unwrap_or(false)
    };
    assert!(!flag().await);
    // 보통 노드는 열어 본 채널만 내주므로(시더 정책) A도 채널을 열어 둡니다. 묻는 곳은 무작위라 몇 번 묻습니다.
    a.ok::<m::ChannelMarkRead>(MarkReadParams {
        space,
        channel,
        event: None,
    })
    .await;
    let mut hinted = false;
    for _ in 0..20 {
        testkit::poll_channel_tips(&c.node).await;
        if flag().await {
            hinted = true;
            break;
        }
    }
    assert!(hinted, "끝을 물은 뒤 새 글 힌트");
    c.ok::<m::ChannelMarkRead>(MarkReadParams {
        space,
        channel,
        event: None,
    })
    .await;
    assert!(!flag().await, "열면 힌트를 지움");
    eventually("열면 새 글을 받아 옴", || async {
        testkit::get_event(&c.node, &fresh).ok().flatten()
    })
    .await;
}

/// 채널 토픽을 끈 노드도 gossip 크루에서는 묶음 16개를 모두 구독해, 켠 노드가 묶음에 올린 글을 실시간으로 받습니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn channel_topics_reach_nodes_that_left_them_off() {
    use yangpa_rpc::api::ChannelTopicsParams;

    let cl = Cluster::new();
    let (a, b, _c, space, channel) = three_member_crew_with(&cl, |c| c.push_max_devices = 0).await;
    a.ok::<m::NetSetChannelTopics>(ChannelTopicsParams { enabled: true })
        .await;
    assert!(!b.ok::<m::NetChannelTopics>(Empty).await.enabled);
    let all: Vec<u8> = (0..yangpa_core::wire::CHANNEL_BUCKETS).collect();
    eventually("끈 B가 묶음을 모두 구독", || async {
        (testkit::channel_buckets(&b.node, space) == all).then_some(())
    })
    .await;
    // A는 그 채널을 따라가지 않아도 묶음 토픽에 올리고, 크루 토픽으로 돌리지 않은 글이 B에 닿습니다.
    let mut ok = false;
    for i in 0..40 {
        let before = testkit::channel_topic_sends(&a.node);
        let id = a.send(space, channel, &format!("섞인 크루 {i}")).await;
        eventually("B가 글을 받음", || async {
            testkit::get_event(&b.node, &id).ok().flatten()
        })
        .await;
        let after = testkit::channel_topic_sends(&a.node);
        if after.0 > before.0 && after.1 == before.1 {
            ok = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    assert!(ok, "묶음 토픽으로 올라가 끈 노드에 닿음");
}

// ───────────── 4-1. 비공개 채널의 그룹 토픽 ─────────────

/// 볼 수 있는 기기가 많은 비공개 채널은 그 키 그룹의 gossip 토픽으로 갑니다. 볼 수 있는 멤버만 그 토픽을 구독하고,
/// 그룹 밖 이벤트를 그룹 토픽에 실으면 무시합니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn private_channel_goes_over_its_group_topic() {
    use yangpa_core::wire::GossipTopic;
    use yangpa_daemon::testkit::net as tnet;

    let cl = Cluster::new();
    // 직접 보내기를 끄고(크루 토픽), 볼 수 있는 기기가 1대만 넘어도 그룹 토픽을 씁니다.
    let tweak = |c: &mut yangpa_daemon::NodeConfig| {
        c.push_max_devices = 0;
        c.group_topic_min = 1;
    };
    let (a, b, c, space, open) = three_member_crew_with(&cl, tweak).await;
    let role = create_role(&a, space, "비밀", &[], 5).await;
    let mut p = channel_params(space, "비밀방", ChannelKind::Text);
    p.private_roles = Some(vec![role]);
    let secret = a.ok::<m::ChannelCreate>(p).await.channel;
    set_roles(&a, space, b.user(), vec![role]).await;
    eventually("B가 채널 그룹에 들어감", || async {
        testkit::channel_key_seq(&b.node, &space, secret)
    })
    .await;
    let g = testkit::channel_key_group(&a.node, space, secret).expect("비공개 그룹");
    for n in [&a, &b] {
        eventually("볼 수 있는 멤버가 그룹 토픽을 구독", || async {
            (testkit::group_topics(&n.node, space) == vec![g]).then_some(())
        })
        .await;
    }
    assert!(testkit::group_topics(&c.node, space).is_empty());

    let m1 = a.send(space, secret, "그룹 토픽으로").await;
    sees_text(&b, space, secret, "그룹 토픽으로").await;
    let marker = a.send(space, open, "표시").await;
    eventually("C가 공개 채널 글을 받음", || async {
        testkit::get_event(&c.node, &marker).ok().flatten()
    })
    .await;
    assert!(testkit::get_event(&c.node, &m1).unwrap().is_none());

    // 그룹 밖(모두 보는 채널) 이벤트를 그룹 토픽에 실으면 감점 없이 버립니다.
    let ev = testkit::get_event(&a.node, &marker).unwrap().unwrap();
    let v =
        tnet::gossip_verdict_on(&b.node, a.peer(), GossipTopic::Group(space, g), vec![ev]).await;
    assert_eq!(v, yangpa_net::GossipVerdict::Ignore);
}

fn role_display(
    space: SpaceId,
    role: RoleId,
    visible_to: Option<Option<Vec<RoleId>>>,
    see_admins: Option<bool>,
) -> RoleUpdateParams {
    RoleUpdateParams {
        space,
        role,
        name: None,
        color: None,
        permissions: None,
        rank: None,
        visible_to,
        see_admins,
    }
}

fn overwrite(
    target: OverwriteTarget,
    allow: Permissions,
    deny: Permissions,
) -> PermissionOverwrite {
    PermissionOverwrite {
        target,
        allow,
        deny,
    }
}

/// 역할 표시 설정은 보기에서만 역할·관리자를 거르고, `overwrites`는 채널 덮어쓰기 전체를 바꿉니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn role_display_filters_views_and_overwrites_replace_channel_permissions() {
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let b = cl.node("b").await;
    let space = create_crew(&a, "역할 크루").await;
    let channel = a.first_channel(space).await;
    assert_eq!(join(&a, &b, space).await, InviteJoinStatus::Joined);
    wait_joined(&b, space).await;
    wait_member(&b, space, a.user(), true).await;
    let admins = create_role(&a, space, "관리진", &["administrator"], 30).await;
    let staff = create_role(&a, space, "운영진", &["kick"], 20).await;
    let quiet = create_role(&a, space, "조용", &[], 10).await;
    set_roles(&a, space, a.user(), vec![admins, staff]).await;

    a.ok::<m::RoleUpdate>(role_display(space, staff, Some(Some(vec![quiet])), None))
        .await;
    a.ok::<m::RoleUpdate>(role_display(space, RoleId::EVERYONE, None, Some(false)))
        .await;
    let r = a
        .call::<m::RoleUpdate>(role_display(
            space,
            RoleId::EVERYONE,
            Some(Some(vec![staff])),
            None,
        ))
        .await;
    assert_eq!(code(r), ERR_INVALID_INPUT);
    eventually("B가 표시 설정을 받음", || async {
        let st = testkit::space_state(&b.node, &space)?;
        (st.role_display.len() == 2 && st.members.get(&a.user())?.roles.len() == 2).then_some(())
    })
    .await;

    // B: 운영진(조용만 봄)과 관리진(관리자 못 봄)이 빠지고, 표시 설정 표는 받지 않습니다.
    let view = b.ok::<m::SpaceGet>(SpaceParams { space }).await;
    let ids: Vec<RoleId> = view.roles.iter().map(|r| r.id).collect();
    assert!(ids.contains(&quiet) && !ids.contains(&staff) && !ids.contains(&admins));
    assert!(view.admins_hidden && view.role_display.is_empty());
    let a_roles = |v: &[yangpa_rpc::api::MemberView]| {
        v.iter().find(|m| m.user == a.user()).unwrap().roles.clone()
    };
    assert!(a_roles(&view.members).is_empty());
    let listed = b.ok::<m::MemberList>(SpaceParams { space }).await;
    assert!(a_roles(&listed).is_empty());
    let roles = b.ok::<m::RoleList>(SpaceParams { space }).await;
    assert!(roles.iter().all(|r| r.id != staff && r.id != admins));
    // 확정 기기 후보는 곧 관리자 목록이라 B에게는 비어 있습니다.
    let candidates = b
        .ok::<m::CrewCommitterCandidates>(SpaceParams { space })
        .await;
    assert!(candidates.is_empty());
    // A(소유자)는 모두 봅니다.
    let view = a.ok::<m::SpaceGet>(SpaceParams { space }).await;
    assert_eq!(view.roles.len(), 4);
    assert!(!view.admins_hidden && view.role_display.len() == 2);

    // B에게 '조용'을 주면 운영진이 보이고, '조용'에 관리자 보기를 켜면 관리진도 보입니다.
    set_roles(&a, space, b.user(), vec![quiet]).await;
    wait_roles(&b, space, b.user(), &[quiet]).await;
    let view = b.ok::<m::SpaceGet>(SpaceParams { space }).await;
    assert_eq!(a_roles(&view.members), vec![staff]);
    assert!(view.admins_hidden);
    a.ok::<m::RoleUpdate>(role_display(space, quiet, None, Some(true)))
        .await;
    eventually("B가 관리자를 봄", || async {
        let view = b.ok::<m::SpaceGet>(SpaceParams { space }).await;
        (!view.admins_hidden && view.roles.len() == 4).then_some(())
    })
    .await;
    let mut have = a_roles(&b.ok::<m::MemberList>(SpaceParams { space }).await);
    have.sort();
    let mut want = vec![admins, staff];
    want.sort();
    assert_eq!(have, want);

    // 덮어쓰기: @everyone의 보내기를 막으면 B는 못 보냅니다.
    let everyone = OverwriteTarget::Role(RoleId::EVERYONE);
    let mut p = ChannelUpdateParams::new(space, channel);
    p.overwrites = Some(vec![
        overwrite(everyone, Permissions::NONE, Permissions::SEND_MESSAGES),
        overwrite(
            OverwriteTarget::Role(quiet),
            Permissions::NONE,
            Permissions::NONE,
        ),
    ]);
    a.ok::<m::ChannelUpdate>(p).await;
    let ow = &testkit::space_state(&a.node, &space).unwrap().channels[&channel].overwrites;
    assert_eq!(ow.len(), 1, "빈 덮어쓰기는 빠짐");
    eventually("B가 보내기를 못 함", || async {
        let r = b
            .call::<m::MessageSend>(send_params(space, channel, "막힘", None))
            .await;
        (r.err()?.code == ERR_FORBIDDEN).then_some(())
    })
    .await;

    let mut both = ChannelUpdateParams::new(space, channel);
    both.private_roles = Some(vec![quiet]);
    both.overwrites = Some(Vec::new());
    assert_eq!(
        code(a.call::<m::ChannelUpdate>(both).await),
        ERR_INVALID_INPUT
    );
    let mut overlap = ChannelUpdateParams::new(space, channel);
    overlap.overwrites = Some(vec![overwrite(
        everyone,
        Permissions::SPEAK,
        Permissions::SPEAK,
    )]);
    assert_eq!(
        code(a.call::<m::ChannelUpdate>(overlap).await),
        ERR_INVALID_INPUT
    );

    // 채널 관리만 가진 B는 자기에게 있는 권한만 덮어씁니다.
    let chans = create_role(&a, space, "채널지기", &["manage_channels"], 5).await;
    set_roles(&a, space, b.user(), vec![quiet, chans]).await;
    wait_roles(&b, space, b.user(), &[quiet, chans]).await;
    let mut grant = ChannelUpdateParams::new(space, channel);
    grant.overwrites = Some(vec![overwrite(
        everyone,
        Permissions::KICK_MEMBERS,
        Permissions::NONE,
    )]);
    assert_eq!(code(b.call::<m::ChannelUpdate>(grant).await), ERR_FORBIDDEN);
    let mut open = ChannelUpdateParams::new(space, channel);
    open.overwrites = Some(vec![overwrite(
        everyone,
        Permissions::NONE,
        Permissions::ADD_REACTIONS,
    )]);
    // 보내기 거부를 푸는 것도 B에게 있는 권한(보내기)이라 됩니다.
    b.ok::<m::ChannelUpdate>(open).await;
    eventually("B가 다시 보냄", || async {
        b.call::<m::MessageSend>(send_params(space, channel, "열림", None))
            .await
            .ok()
    })
    .await;
}

// ───────────── 5. 커스텀 이모지 ─────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn custom_emoji_becomes_visible_to_members() {
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let b = cl.node("b").await;
    let space = create_crew(&a, "이모지 크루").await;
    assert_eq!(join(&a, &b, space).await, InviteJoinStatus::Joined);
    let png_path = cl.dir("a").join("onion.png");
    let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
    png.extend_from_slice(&[0u8; 17]);
    std::fs::write(&png_path, &png).unwrap();
    let path = png_path.to_string_lossy().into_owned();

    let r = a
        .call::<m::EmojiCreate>(EmojiCreateParams {
            space,
            name: "x".into(),
            path: path.clone(),
        })
        .await;
    assert_eq!(code(r), ERR_INVALID_INPUT, "짧은 이름");
    let created = a
        .ok::<m::EmojiCreate>(EmojiCreateParams {
            space,
            name: "양파".into(),
            path: path.clone(),
        })
        .await;
    assert!(!created.animated);
    let seen = eventually("B가 이모지를 봄", || async {
        b.ok::<m::EmojiList>(SpaceParams { space })
            .await
            .into_iter()
            .find(|e| e.id == created.id)
    })
    .await;
    assert_eq!(seen.name, "양파");
    assert_eq!(seen.mime, "image/png");
    assert_eq!(seen.created_by, Some(a.user()), "올린 사람");
    // 권한 없는 멤버는 만들 수 없습니다.
    let b_png = cl.dir("b").join("b.png");
    std::fs::write(&b_png, &png).unwrap();
    let r = b
        .call::<m::EmojiCreate>(EmojiCreateParams {
            space,
            name: "몰래".into(),
            path: b_png.to_string_lossy().into_owned(),
        })
        .await;
    assert_eq!(code(r), ERR_FORBIDDEN);

    // 크루 한도: 크루 관리(manage_space)만 정하고, 개수·크기를 넘는 이모지는 올리지 못합니다.
    let set_limits = |max_count, max_bytes| CrewUpdateParams {
        space,
        name: None,
        description: None,
        icon_path: None,
        strict_rotation: None,
        emoji_limits: Some(EmojiLimits {
            max_count,
            max_bytes,
        }),
    };
    let r = b.call::<m::CrewUpdate>(set_limits(Some(1), None)).await;
    assert_eq!(code(r), ERR_FORBIDDEN);
    let r = a.call::<m::CrewUpdate>(set_limits(Some(0), None)).await;
    assert_eq!(code(r), ERR_INVALID_INPUT, "개수 0");
    a.ok::<m::CrewUpdate>(set_limits(Some(5), Some(png.len() as u64 - 1)))
        .await;
    eventually("B가 한도를 봄", || async {
        (b.ok::<m::SpaceGet>(SpaceParams { space })
            .await
            .emoji_limits
            .max_count
            == Some(5))
        .then_some(())
    })
    .await;
    let r = a
        .call::<m::EmojiCreate>(EmojiCreateParams {
            space,
            name: "크다".into(),
            path: path.clone(),
        })
        .await;
    assert_eq!(code(r), ERR_INVALID_INPUT, "크기 한도");
    a.ok::<m::CrewUpdate>(set_limits(Some(1), None)).await;
    let r = a
        .call::<m::EmojiCreate>(EmojiCreateParams {
            space,
            name: "둘째".into(),
            path: path.clone(),
        })
        .await;
    assert_eq!(code(r), ERR_INVALID_INPUT, "개수 한도");

    a.ok::<m::EmojiDelete>(EmojiDeleteParams {
        space,
        id: created.id,
    })
    .await;
    eventually("B에게서 이모지가 사라짐", || async {
        b.ok::<m::EmojiList>(SpaceParams { space })
            .await
            .is_empty()
            .then_some(())
    })
    .await;
}

// ───────────── 6. 소유권 이전 ─────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ownership_transfer_moves_owner_and_committer() {
    let cl = Cluster::new();
    let (a, b, c, space, channel) = three_member_crew(&cl).await;
    let b_dev = testkit::device_id(&b.node).unwrap();
    let c_dev = testkit::device_id(&c.node).unwrap();
    let r = b
        .call::<m::CrewTransferOwnership>(TransferOwnershipParams {
            space,
            user: c.user(),
        })
        .await;
    assert_eq!(code(r), ERR_FORBIDDEN, "소유자가 아닌 멤버가 넘김");

    a.ok::<m::CrewTransferOwnership>(TransferOwnershipParams {
        space,
        user: b.user(),
    })
    .await;
    for n in [&a, &b, &c] {
        eventually(&format!("{}가 새 소유자를 봄", n.name), || async {
            let st = testkit::space_state(&n.node, &space)?;
            (st.owner == b.user() && st.committer == b_dev).then_some(())
        })
        .await;
    }
    let r = a
        .call::<m::CrewTransferOwnership>(TransferOwnershipParams {
            space,
            user: c.user(),
        })
        .await;
    assert_eq!(code(r), ERR_FORBIDDEN, "옛 소유자가 다시 넘김");
    let r = a
        .call::<m::MemberKick>(MemberKickParams {
            space,
            user: b.user(),
            reason: None,
        })
        .await;
    assert_eq!(code(r), ERR_FORBIDDEN, "새 소유자를 추방");

    // 새 확정 기기(B)가 추방을 키 그룹에 반영합니다(회전).
    b.ok::<m::MemberKick>(MemberKickParams {
        space,
        user: c.user(),
        reason: None,
    })
    .await;
    eventually("B가 C를 그룹에서 뺌", || async {
        let mb = testkit::key_group_members(&b.node, &space, None)?;
        let ma = testkit::key_group_members(&a.node, &space, None)?;
        (!mb.contains(&c_dev) && ma == mb).then_some(())
    })
    .await;
    b.send(space, channel, "새 소유자").await;
    sees_text(&a, space, channel, "새 소유자").await;
}

/// 새 소유자가 볼 수 없던 비공개 채널이 있어도, 넘긴 뒤 새 확정 기기(새 소유자)가 그 그룹을 계속 바꿀 수 있습니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ownership_transfer_hands_private_channel_groups_to_new_owner() {
    let cl = Cluster::new();
    let (a, b, c, space, _) = three_member_crew(&cl).await;
    let a_dev = testkit::device_id(&a.node).unwrap();
    let b_dev = testkit::device_id(&b.node).unwrap();
    let c_dev = testkit::device_id(&c.node).unwrap();
    let role = create_role(&a, space, "비밀", &[], 5).await;
    set_roles(&a, space, c.user(), vec![role]).await;
    let mut p = channel_params(space, "비밀방", ChannelKind::Text);
    p.private_roles = Some(vec![role]);
    p.history = Some(HistoryVisibility::Public);
    let secret = a.ok::<m::ChannelCreate>(p).await.channel;
    eventually("C가 비공개 채널 그룹에 들어감", || async {
        testkit::channel_key_seq(&c.node, &space, secret)
    })
    .await;
    // 지난 메시지는 C가 씁니다(받는 쪽은 받을 때의 상태로 작성자 권한을 보므로, 이전 뒤 채널을 못 보는 A의 글은
    // 새로 받는 노드가 거절함).
    c.send(space, secret, "넘기기 전 비밀").await;
    sees_text(&a, space, secret, "넘기기 전 비밀").await;
    assert!(
        testkit::key_group_members(&b.node, &space, Some(secret)).is_none(),
        "볼 수 없는 B가 그룹에 있음(시험 전제)"
    );

    a.ok::<m::CrewTransferOwnership>(TransferOwnershipParams {
        space,
        user: b.user(),
    })
    .await;
    eventually(
        "B가 새 소유자·확정 기기이고 비밀방 그룹에 있음",
        || async {
            let st = testkit::space_state(&b.node, &space)?;
            (st.owner == b.user()
                && st.committer == b_dev
                && testkit::channel_key_seq(&b.node, &space, secret).is_some())
            .then_some(())
        },
    )
    .await;
    // 과거 기록 공개 채널이라 넘기는 쪽이 봉인해 준 기록 키로 지난 메시지도 읽습니다.
    sees_text(&b, space, secret, "넘기기 전 비밀").await;

    // 새 확정 기기가 그룹을 바꿉니다: 볼 수 없게 된 옛 소유자와 역할을 잃은 C가 빠짐.
    set_roles(&b, space, c.user(), Vec::new()).await;
    eventually("B가 비밀방 그룹을 자기만 남게 맞춤", || async {
        let mb = testkit::key_group_members(&b.node, &space, Some(secret))?;
        (mb == vec![b_dev]).then_some(())
    })
    .await;
    let m = b.send(space, secret, "넘긴 뒤 비밀").await;
    let evs = testkit::stream_events(&b.node, &space, StreamId::Channel(secret)).unwrap();
    for (n, dev) in [(&a, a_dev), (&c, c_dev)] {
        testkit::ingest_from(&n.node, space, evs.clone(), b.peer())
            .await
            .unwrap();
        assert!(
            testkit::message_text(&n.node, &m).is_none(),
            "그룹에서 빠진 {}({})가 읽음",
            n.name,
            dev.short()
        );
    }
}

// ───────────── 7. 탈퇴 ─────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn leave_removes_member_everywhere_and_purges_local_data() {
    let cl = Cluster::new();
    let (a, b, c, space, channel) = three_member_crew(&cl).await;
    let c_dev = testkit::device_id(&c.node).unwrap();
    let r = a.call::<m::CrewLeave>(SpaceParams { space }).await;
    assert_eq!(code(r), ERR_CONFLICT, "소유자는 먼저 넘겨야 함");
    // 탈퇴는 내 데이터를 지우는 파괴적 작업이라 자동화 토큰은 사람 확인 없이 할 수 없습니다(확인 대기열로 감).
    let r = automation_call::<m::CrewLeave>(&c, SpaceParams { space }).await;
    assert_eq!(code(r), ERR_CONFIRMATION_REQUIRED, "자동화 토큰의 탈퇴");
    assert!(testkit::known_spaces(&c.node).contains(&space));

    c.ok::<m::CrewLeave>(SpaceParams { space }).await;
    // 나간 기기에는 공간 데이터가 남지 않습니다.
    assert!(!testkit::known_spaces(&c.node).contains(&space));
    let list = c.ok::<m::SpaceList>(yangpa_rpc::api::Empty).await;
    assert!(list.iter().all(|s| s.space != space));
    wait_member(&a, space, c.user(), false).await;
    wait_member(&b, space, c.user(), false).await;
    eventually("C 기기가 그룹에서 빠짐", || async {
        let ma = testkit::key_group_members(&a.node, &space, None)?;
        (!ma.contains(&c_dev)).then_some(())
    })
    .await;
    wait_same_crew_key(&[&a, &b], space).await;
    a.send(space, channel, "C 없이").await;
    sees_text(&b, space, channel, "C 없이").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn disband_ends_crew_for_all_and_leave_purges() {
    let cl = Cluster::new();
    let (a, b, mut c, space, channel) = three_member_crew(&cl).await;
    let disband = || SpaceParams { space };
    let r = b.call::<m::CrewDisband>(disband()).await;
    assert_eq!(code(r), ERR_FORBIDDEN, "소유자가 아닌 멤버가 해산");
    let r = automation_call::<m::CrewDisband>(&a, disband()).await;
    assert_eq!(code(r), ERR_CONFIRMATION_REQUIRED, "자동화 토큰의 해산");

    // C는 꺼진 사이 해산됩니다.
    c.shutdown().await;
    a.ok::<m::CrewDisband>(disband()).await;
    // 해산한 기기는 데이터를 지우고, 멤버 기기는 해산한 크루로 남겨 둡니다(늦게 온 멤버에게 전함).
    assert!(!testkit::known_spaces(&a.node).contains(&space));
    // 다시 켜진 C는 남은 멤버 B와의 평소 동기화로 해산을 받습니다(A는 이미 지움).
    c.start_again().await;
    connect_both(&b, &c).await;
    for n in [&b, &c] {
        eventually(&format!("{}가 해산을 봄", n.name), || async {
            testkit::space_state(&n.node, &space)?
                .disbanded
                .then_some(())
        })
        .await;
    }
    // 해산 표시는 상태 행에 저장하지 않으므로, 다시 켜면 이벤트를 다시 해소해 되찾습니다.
    c.restart().await;
    assert!(c.ok::<m::SpaceGet>(SpaceParams { space }).await.disbanded);
    assert!(b.ok::<m::SpaceGet>(SpaceParams { space }).await.disbanded);
    let r = b
        .call::<m::MessageSend>(send_params(space, channel, "아직 있니", None))
        .await;
    assert_eq!(code(r), ERR_FORBIDDEN);
    // 소유자가 아닌 멤버도 나가기로 지웁니다(해산한 크루는 탈퇴 이벤트를 내지 않음).
    b.ok::<m::CrewLeave>(SpaceParams { space }).await;
    assert!(!testkit::known_spaces(&b.node).contains(&space));
}

/// 같은 멤버가 있는 크루 둘 가운데 하나를 이 기기에서 지워도, 남은 크루가 그 멤버 기기에 봉인할 키 행은 남습니다
/// (키 행은 (키 약속, 공간)마다라 먼저 본 크루를 지워도 다른 크루의 행은 그대로).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn purging_one_crew_keeps_device_keys_used_by_another() {
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let b = cl.node("b").await;
    let x = testkit::seed_crew(&a.node, std::slice::from_ref(&b.node), "X")
        .await
        .expect("X");
    let y = testkit::seed_crew(&a.node, std::slice::from_ref(&b.node), "Y")
        .await
        .expect("Y");
    let a_dev = testkit::device_id(&a.node).unwrap();
    let a_box = testkit::device_list(&a.node, &a.user())
        .expect("A 목록")
        .devices
        .into_iter()
        .find(|d| d.device == a_dev)
        .expect("A 기기")
        .box_key;
    let commit = testkit::state_key_commit(&b.node, y, a.user(), a_dev).expect("Y의 A 기록");
    assert_eq!(
        testkit::state_key_commit(&b.node, x, a.user(), a_dev),
        Some(commit)
    );

    testkit::purge_space(&b.node, x).await.expect("X 지우기");
    assert!(!testkit::known_spaces(&b.node).contains(&x));
    testkit::clear_device_key_cache(&b.node);
    assert_eq!(
        testkit::stored_box_key(&b.node, commit),
        Some(a_box.clone()),
        "Y가 쓰는 키 행이 남음"
    );
    assert_eq!(
        testkit::sealing_box_key(&b.node, y, a.user(), a_dev),
        Some(a_box)
    );
}

/// 상태가 바뀌면 마지막으로 쓴 판에 비춰 바뀐 행만 씁니다: 크루 이름을 바꾸면 통째로 다시 쓰지 않고 머리 한 행만 쓰고,
/// 다시 시작해 저장된 이벤트로 같은 상태를 다시 세우면 상태 행을 하나도 쓰지 않습니다(읽어 둔 행이 기준).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn state_changes_write_only_changed_rows() {
    let cl = Cluster::new();
    let mut a = cl.node("a").await;
    let space = create_crew(&a, "X").await;
    let before = testkit::state_writes(&a.node, space).expect("런타임");
    assert_eq!(before.full, 1, "새 공간은 처음 한 번만 통째로");
    a.ok::<m::CrewUpdate>(CrewUpdateParams {
        space,
        name: Some("Y".into()),
        description: None,
        icon_path: None,
        strict_rotation: None,
        emoji_limits: None,
    })
    .await;
    let after = testkit::state_writes(&a.node, space).expect("런타임");
    assert_eq!(after.full, 1);
    assert_eq!(after.deltas, before.deltas + 1);
    assert_eq!(after.rows, before.rows + 1, "머리 한 행");

    a.restart().await;
    assert_eq!(
        testkit::space_state(&a.node, &space).map(|s| s.settings.name.clone()),
        Some("Y".into())
    );
    assert_eq!(
        testkit::state_writes(&a.node, space),
        Some(testkit::StateWrites::default()),
        "다시 세운 상태가 저장된 행과 같아 쓰지 않음"
    );
}

/// 공간 하나를 만들거나 지우는 일은 그 공간 잠금만 쥡니다: 다른 크루의 긴 적용(잠금을 쥔 채)이 끝나기를 기다리지
/// 않습니다(모든 공간 잠금이면 그 뒤에 줄을 서고, 그동안 다른 공간도 모두 막힘). 같은 공간은 여전히 한 줄로 서고,
/// 쥐거나 기다리는 쪽이 없는 잠금은 표에 남지 않습니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn single_space_work_does_not_wait_for_another_space_lock() {
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let x = create_crew(&a, "X").await;
    let y = create_crew(&a, "Y").await;
    let busy = testkit::hold_space_lock(&a.node, y).await;
    let short = std::time::Duration::from_millis(300);
    let within = std::time::Duration::from_secs(10);
    assert!(
        tokio::time::timeout(short, testkit::hold_space_lock(&a.node, y))
            .await
            .is_err(),
        "같은 공간은 기다림"
    );
    let z = tokio::time::timeout(within, create_crew(&a, "Z"))
        .await
        .expect("다른 크루를 쥔 동안에도 만듦");
    tokio::time::timeout(within, testkit::purge_space(&a.node, x))
        .await
        .expect("다른 크루를 쥔 동안에도 지움")
        .expect("X 지우기");
    drop(busy);
    let known = testkit::known_spaces(&a.node);
    assert!(known.contains(&z) && known.contains(&y) && !known.contains(&x));
    let left = testkit::space_lock_table_after(&a.node, 200).await;
    assert!(left < 16, "쥐는 쪽이 없는 공간 잠금이 남음: {left}");
}

/// 따라잡기가 부분 전달 스트림의 "가진 것"을 묻거나 한 바퀴 끝을 적는 일은 같은 공간의 긴 적용(`SpaceCore`를 적용
/// 내내 쥠)을 기다리지 않습니다. 기다리면 그동안 비동기 작업 스레드가 묶여 다른 공간의 적용·RPC·네트워크 작업까지
/// 멈춥니다(작업 스레드 4개에 따라잡기 셋이면 데몬 전체).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn catch_up_bookkeeping_does_not_wait_for_a_long_apply() {
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let x = create_crew(&a, "X").await;
    let busy = testkit::hold_space_core(&a.node, x);
    let node = a.node.clone();
    let probe = tokio::task::spawn_blocking(move || {
        let known = testkit::scoped_known_ids(&node, x, &[EventId::hash_of(b"unknown")]);
        testkit::note_caught_up(&node, x);
        known
    });
    let got = tokio::time::timeout(std::time::Duration::from_secs(5), probe).await;
    drop(busy);
    assert_eq!(
        got.expect("긴 적용을 기다리지 않음").expect("작업"),
        vec![false]
    );
}

/// `space.get`(키 회전 대기 여부)은 긴 적용을 기다리지 않습니다: 키 그룹 규칙은 적용마다 새로 쓰는 요약만 읽습니다.
/// 예전에는 `SpaceCore`를 잠가, 앱이 묻는 동안 작업 스레드가 묶여 다른 공간의 `space.get`까지 멈췄습니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn space_get_does_not_wait_for_a_long_apply() {
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let x = create_crew(&a, "X").await;
    let y = create_crew(&a, "Y").await;
    let busy = testkit::hold_space_core(&a.node, x);
    let within = std::time::Duration::from_secs(5);
    let got = tokio::time::timeout(within, async {
        tokio::join!(
            a.ok::<m::SpaceGet>(SpaceParams { space: x }),
            a.ok::<m::SpaceGet>(SpaceParams { space: x }),
            a.ok::<m::SpaceGet>(SpaceParams { space: y }),
        )
    })
    .await;
    drop(busy);
    got.expect("X의 space.get이 긴 적용(SpaceCore)을 기다림");
}

// ───────────── 8. 1:1 → 크루 전환 ─────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_to_one_converts_to_crew_only_with_both_consents() {
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let b = cl.node("b").await;
    let c = cl.node("c").await;
    let space = befriend(&a, &b).await;
    let r = a
        .call::<m::InviteCreate>(InviteCreateParams {
            space,
            expires_in_secs: None,
            max_uses: None,
            roles: Vec::new(),
        })
        .await;
    assert_eq!(code(r), ERR_INVALID_INPUT, "전환 전에는 초대할 수 없음");

    a.ok::<m::CrewConvertRequest>(CrewConvertRequestParams {
        space,
        name: "우리 크루".into(),
    })
    .await;
    let r = a.call::<m::CrewConvertAccept>(SpaceParams { space }).await;
    assert_eq!(code(r), ERR_CONFLICT, "요청한 사람이 스스로 수락");
    eventually("B가 전환 요청을 봄", || async {
        let v = b.ok::<m::SpaceGet>(SpaceParams { space }).await;
        (v.pending_crew_conversion == Some(a.user())).then_some(())
    })
    .await;
    assert_eq!(
        testkit::space_state(&a.node, &space).unwrap().kind,
        SpaceKind::Space
    );
    let r = automation_call::<m::CrewConvertAccept>(&b, SpaceParams { space }).await;
    assert_eq!(
        code(r),
        ERR_CONFIRMATION_REQUIRED,
        "자동화 토큰의 전환 수락"
    );
    let old_channel = a.first_channel(space).await;
    let before = a.send(space, old_channel, "전환 전 둘만의 이야기").await;
    sees_text(&b, space, old_channel, "전환 전 둘만의 이야기").await;
    b.ok::<m::CrewConvertAccept>(SpaceParams { space }).await;
    for n in [&a, &b] {
        eventually(&format!("{}가 크루로 봄", n.name), || async {
            let st = testkit::space_state(&n.node, &space)?;
            (st.kind == SpaceKind::Crew && st.settings.name == "우리 크루" && st.owner == a.user())
                .then_some(())
        })
        .await;
    }
    let v = b.ok::<m::SpaceGet>(SpaceParams { space }).await;
    assert!(
        v.my_permissions.iter().any(|p| p == "administrator"),
        "수락한 쪽은 관리자"
    );

    // 전환 전 채널의 과거 기록은 한 사람(소유자라도)만으로는 새 멤버에게 열 수 없습니다.
    let history = |n: &TestNode| {
        testkit::space_state(&n.node, &space).unwrap().channels[&old_channel].history
    };
    assert_eq!(history(&a), HistoryVisibility::Private);
    a.ok::<m::ChannelUpdate>(ChannelUpdateParams {
        history: Some(HistoryVisibility::Public),
        ..ChannelUpdateParams::new(space, old_channel)
    })
    .await;
    assert_eq!(history(&a), HistoryVisibility::Private, "한 사람만 공개함");

    // 이제 크루라 다른 사람을 초대할 수 있습니다.
    assert_eq!(join(&a, &c, space).await, InviteJoinStatus::Joined);
    let channel = a.first_channel(space).await;
    assert_eq!(channel, old_channel);
    wait_same_crew_key(&[&a, &b, &c], space).await;
    connect_both(&b, &c).await;
    meet(&b, &c, space).await;
    c.send(space, channel, "새로 왔어요").await;
    sees_text(&a, space, channel, "새로 왔어요").await;
    sees_text(&b, space, channel, "새로 왔어요").await;
    a.send(space, channel, "전환 뒤").await;
    sees_text(&c, space, channel, "전환 뒤").await;
    let evs = testkit::stream_events(&a.node, &space, StreamId::Channel(channel)).unwrap();
    testkit::ingest_from(&c.node, space, evs, a.peer())
        .await
        .unwrap();
    assert!(
        testkit::message_text(&c.node, &before).is_none(),
        "새 멤버가 전환 전 1:1 기록을 읽음"
    );

    // 원래 두 사람이 모두 공개하면 바뀝니다.
    b.ok::<m::ChannelUpdate>(ChannelUpdateParams {
        history: Some(HistoryVisibility::Public),
        ..ChannelUpdateParams::new(space, old_channel)
    })
    .await;
    for n in [&a, &b, &c] {
        eventually(&format!("{}가 공개로 봄", n.name), || async {
            (history(n) == HistoryVisibility::Public).then_some(())
        })
        .await;
    }
}

/// 친구가 된 두 사람이 그 1:1 공간을 크루로 바꿉니다.
async fn converted_pair(a: &TestNode, b: &TestNode) -> SpaceId {
    let crew = befriend(a, b).await;
    a.ok::<m::CrewConvertRequest>(CrewConvertRequestParams {
        space: crew,
        name: "우리 크루".into(),
    })
    .await;
    eventually("B가 전환 요청을 봄", || async {
        let v = b.ok::<m::SpaceGet>(SpaceParams { space: crew }).await;
        (v.pending_crew_conversion == Some(a.user())).then_some(())
    })
    .await;
    b.ok::<m::CrewConvertAccept>(SpaceParams { space: crew })
        .await;
    for n in [a, b] {
        eventually(&format!("{}가 크루로 봄", n.name), || async {
            (testkit::space_state(&n.node, &crew)?.kind == SpaceKind::Crew).then_some(())
        })
        .await;
    }
    crew
}

/// 친구 기록이 가리키는 공간.
async fn dm_of(n: &TestNode, user: UserId) -> Option<SpaceId> {
    n.ok::<m::FriendList>(Empty)
        .await
        .into_iter()
        .find(|f| f.user == user)
        .map(|f| f.space)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn converted_pair_opens_new_one_to_one() {
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let b = cl.node("b").await;
    let crew = converted_pair(&a, &b).await;
    assert_eq!(dm_of(&a, b.user()).await, Some(crew), "열기 전에는 크루");

    let dm = b
        .ok::<m::FriendOpen>(UserParams { user: a.user() })
        .await
        .space;
    assert_ne!(dm, crew, "크루가 아닌 새 1:1 공간");
    assert_eq!(
        testkit::space_state(&b.node, &dm).unwrap().kind,
        SpaceKind::Space
    );
    let again = b.ok::<m::FriendOpen>(UserParams { user: a.user() }).await;
    assert_eq!(again.space, dm, "다시 열면 같은 공간");
    eventually("A의 친구 기록도 새 1:1 공간", || async {
        (dm_of(&a, b.user()).await == Some(dm)).then_some(())
    })
    .await;
    let from_a = a.ok::<m::FriendOpen>(UserParams { user: b.user() }).await;
    assert_eq!(from_a.space, dm);

    let ch = b.first_channel(dm).await;
    b.send(dm, ch, "다시 둘이서").await;
    sees_text(&a, dm, ch, "다시 둘이서").await;
    a.send(dm, ch, "좋아요").await;
    sees_text(&b, dm, ch, "좋아요").await;
    let crew_ch = a.first_channel(crew).await;
    a.send(crew, crew_ch, "크루는 그대로").await;
    sees_text(&b, crew, crew_ch, "크루는 그대로").await;

    // 친구 삭제는 새 1:1 공간만 지웁니다.
    a.ok::<m::FriendRemove>(UserParams { user: b.user() }).await;
    let st = testkit::space_state(&a.node, &crew).unwrap();
    assert!(st.kind == SpaceKind::Crew && st.is_member(&a.user()) && st.is_member(&b.user()));
    assert!(!testkit::known_spaces(&a.node).contains(&dm));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn leaving_converted_crew_still_opens_one_to_one() {
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let b = cl.node("b").await;
    let crew = converted_pair(&a, &b).await;
    b.ok::<m::CrewLeave>(SpaceParams { space: crew }).await;
    assert!(testkit::space_state(&b.node, &crew).is_none());
    // 나가면서 둘만의 1:1 공간을 새로 만들어, 친구 목록에서 그 사람이 사라지지 않습니다.
    let dm = dm_of(&b, a.user()).await.expect("나간 뒤에도 친구");
    assert_ne!(dm, crew);
    let again = b.ok::<m::FriendOpen>(UserParams { user: a.user() }).await;
    assert_eq!(again.space, dm);
    eventually("A도 새 1:1 공간", || async {
        (dm_of(&a, b.user()).await == Some(dm)).then_some(())
    })
    .await;
    let ch = a.first_channel(dm).await;
    a.send(dm, ch, "나가도 1대1").await;
    sees_text(&b, dm, ch, "나가도 1대1").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn both_opening_apart_after_conversion_settle_on_one_space() {
    let cl = Cluster::new();
    let mut a = cl.node("a").await;
    let mut b = cl.node("b").await;
    let (a_user, b_user) = (a.user(), b.user());
    let crew = converted_pair(&a, &b).await;
    // 서로 닿지 않을 때 둘 다 대화를 열어 각자 새 1:1 공간을 만듭니다.
    b.shutdown().await;
    let sa = a
        .ok::<m::FriendOpen>(UserParams { user: b_user })
        .await
        .space;
    a.shutdown().await;
    b.start_again().await;
    let sb = b
        .ok::<m::FriendOpen>(UserParams { user: a_user })
        .await
        .space;
    assert!(sa != sb && sa != crew && sb != crew);
    a.start_again().await;
    connect_both(&a, &b).await;
    let dm = eventually("둘이 같은 1:1 공간", || async {
        let (x, y) = (dm_of(&a, b_user).await?, dm_of(&b, a_user).await?);
        (x == y).then_some(x)
    })
    .await;
    assert_eq!(dm, sa.min(sb), "양쪽 모두 ID가 작은 쪽");
    let (loser, owner) = if dm == sa { (sb, &b) } else { (sa, &a) };
    eventually("쓰지 않게 된 빈 공간을 지움", || async {
        testkit::space_state(&owner.node, &loser)
            .is_none()
            .then_some(())
    })
    .await;
    let ch = a.first_channel(dm).await;
    a.send(dm, ch, "하나로 모임").await;
    sees_text(&b, dm, ch, "하나로 모임").await;
}

// ───────────── 9. 포럼 ─────────────

async fn posts(n: &TestNode, space: SpaceId, channel: ChannelId) -> Vec<ForumPostView> {
    n.ok::<m::ForumListPosts>(ForumListPostsParams {
        space,
        channel,
        sort: ForumSort::Activity,
        tag: None,
        order: Default::default(),
        tags: Vec::new(),
        tag_match: Default::default(),
    })
    .await
}

async fn wait_post(
    n: &TestNode,
    space: SpaceId,
    channel: ChannelId,
    id: EventId,
    what: &str,
    pred: impl Fn(&ForumPostView) -> bool,
) -> ForumPostView {
    eventually(what, || async {
        posts(n, space, channel)
            .await
            .into_iter()
            .find(|p| p.id == id && pred(p))
    })
    .await
}

fn update(post: EventId, title: Option<&str>, pinned: Option<bool>) -> EventBody {
    EventBody::ForumPostUpdate {
        post,
        title: title.map(str::to_owned),
        tags: None,
        pinned,
        locked: None,
    }
}

fn list_params(
    space: SpaceId,
    channel: ChannelId,
    sort: ForumSort,
    order: SortOrder,
    tags: Vec<u32>,
    tag_match: TagMatch,
) -> ForumListPostsParams {
    ForumListPostsParams {
        space,
        channel,
        sort,
        order,
        tag: None,
        tags,
        tag_match,
    }
}

async fn featured_of(n: &TestNode, space: SpaceId, channel: ChannelId) -> Option<EventId> {
    n.ok::<m::ChannelList>(SpaceParams { space })
        .await
        .into_iter()
        .find(|c| c.channel == channel)
        .and_then(|c| c.featured_post)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn forum_featured_post_attachments_and_list_options() {
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let b = cl.node("b").await;
    let space = create_crew(&a, "대문 크루").await;
    assert_eq!(join(&a, &b, space).await, InviteJoinStatus::Joined);
    let mut p = channel_params(space, "자랑방", ChannelKind::Forum);
    p.forum_tags = Some(vec![
        ForumTag {
            id: 1,
            name: "사진".into(),
            emoji: None,
        },
        ForumTag {
            id: 2,
            name: "여행".into(),
            emoji: None,
        },
    ]);
    let forum = a.ok::<m::ChannelCreate>(p).await.channel;
    wait_channel(&b, space, forum).await;
    let create =
        |title: &str, text: &str, tags: Vec<u32>, attachments, featured| ForumCreatePostParams {
            space,
            channel: forum,
            title: title.into(),
            text: text.into(),
            tags,
            attachments,
            featured,
        };

    let first = a
        .ok::<m::ForumCreatePost>(create("첫 글", "**굵게** 본문", vec![1], Vec::new(), false))
        .await
        .id;
    let png_path = cl.dir("a").join("trip.png");
    let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
    png.extend_from_slice(&[0u8; 17]);
    std::fs::write(&png_path, &png).unwrap();
    let photo = vec![AttachmentInput {
        path: png_path.to_string_lossy().into_owned(),
        ..Default::default()
    }];
    // 본문 없이 첨부만 있어도 되고, 올리면서 대문으로 정합니다.
    let second = a
        .ok::<m::ForumCreatePost>(create("사진 글", "", vec![1, 2], photo, true))
        .await
        .id;
    let r = a
        .call::<m::ForumCreatePost>(create("빈 글", " ", Vec::new(), Vec::new(), false))
        .await;
    assert_eq!(code(r), ERR_INVALID_INPUT, "본문도 첨부도 없음");

    let v = wait_post(
        &b,
        space,
        forum,
        second,
        "B가 사진 글을 대문으로 봄",
        |p| p.featured,
    )
    .await;
    assert!(v.featured);
    assert_eq!(v.attachment_count, 1);
    let cover = v.cover.expect("표지");
    assert!(cover.mime.starts_with("image/"), "{}", cover.mime);
    let v = wait_post(&b, space, forum, first, "B가 첫 글을 봄", |_| true).await;
    assert!(!v.featured && v.cover.is_none());
    assert_eq!(v.preview, "**굵게** 본문");
    eventually("B 채널 보기에 대문", || async {
        (featured_of(&b, space, forum).await == Some(second)).then_some(())
    })
    .await;

    // 정렬 방향과 여러 태그.
    let ids = |v: Vec<ForumPostView>| v.into_iter().map(|p| p.id).collect::<Vec<_>>();
    let list = |order, tags, tag_match| {
        b.ok::<m::ForumListPosts>(list_params(
            space,
            forum,
            ForumSort::Created,
            order,
            tags,
            tag_match,
        ))
    };
    assert_eq!(
        ids(list(SortOrder::Desc, Vec::new(), TagMatch::Any).await),
        vec![second, first]
    );
    assert_eq!(
        ids(list(SortOrder::Asc, Vec::new(), TagMatch::Any).await),
        vec![first, second]
    );
    assert_eq!(
        ids(list(SortOrder::Asc, vec![1, 2], TagMatch::Any).await),
        vec![first, second]
    );
    assert_eq!(
        ids(list(SortOrder::Asc, vec![1, 2], TagMatch::All).await),
        vec![second]
    );
    // 모두 일치는 글의 태그가 고른 태그와 꼭 같은 글만(태그 하나만 골라도 일부 일치와 다름).
    assert_eq!(
        ids(list(SortOrder::Asc, vec![1], TagMatch::Any).await),
        vec![first, second]
    );
    assert_eq!(
        ids(list(SortOrder::Asc, vec![1], TagMatch::All).await),
        vec![first]
    );
    assert!(ids(list(SortOrder::Asc, vec![2], TagMatch::All).await).is_empty());

    // 채널 관리 권한이 없으면 대문을 못 바꾸고, 대문으로 올리려던 글은 올라가지 않습니다.
    let r = b
        .call::<m::ForumSetFeatured>(ForumFeaturedParams {
            space,
            channel: forum,
            post: Some(first),
        })
        .await;
    assert_eq!(code(r), ERR_FORBIDDEN);
    let r = b
        .call::<m::ForumCreatePost>(create("내가 대문", "본문", Vec::new(), Vec::new(), true))
        .await;
    assert_eq!(code(r), ERR_FORBIDDEN);
    assert_eq!(posts(&b, space, forum).await.len(), 2);
    let r = a
        .call::<m::ForumSetFeatured>(ForumFeaturedParams {
            space,
            channel: forum,
            post: Some(EventId([9; 32])),
        })
        .await;
    assert_eq!(code(r), ERR_NOT_FOUND, "없는 글");

    // 다른 글로 바꾸고, 빼고, 대문 글을 지우면 보기에서 빠집니다.
    a.ok::<m::ForumSetFeatured>(ForumFeaturedParams {
        space,
        channel: forum,
        post: Some(first),
    })
    .await;
    eventually("B가 바뀐 대문을 봄", || async {
        (featured_of(&b, space, forum).await == Some(first)).then_some(())
    })
    .await;
    let got = b
        .ok::<m::ForumGetPost>(ForumPostParams {
            space,
            channel: forum,
            post: first,
        })
        .await;
    assert!(got.featured && got.title == "첫 글");
    a.ok::<m::ForumSetFeatured>(ForumFeaturedParams {
        space,
        channel: forum,
        post: None,
    })
    .await;
    eventually("B가 대문이 빠진 것을 봄", || async {
        featured_of(&b, space, forum).await.is_none().then_some(())
    })
    .await;
    a.ok::<m::ForumSetFeatured>(ForumFeaturedParams {
        space,
        channel: forum,
        post: Some(second),
    })
    .await;
    a.ok::<m::MessageDelete>(MessageRefParams {
        space,
        channel: forum,
        id: second,
    })
    .await;
    assert_eq!(featured_of(&a, space, forum).await, None, "지운 대문 글");
    let r = a
        .call::<m::ForumGetPost>(ForumPostParams {
            space,
            channel: forum,
            post: second,
        })
        .await;
    assert_eq!(code(r), ERR_NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn forum_posts_and_updates_apply_in_order_even_before_the_post() {
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let b = cl.node("b").await;
    let space = create_crew(&a, "포럼 크루").await;
    assert_eq!(join(&a, &b, space).await, InviteJoinStatus::Joined);
    let mut p = channel_params(space, "질문방", ChannelKind::Forum);
    p.forum_tags = Some(vec![
        ForumTag {
            id: 1,
            name: "질문".into(),
            emoji: None,
        },
        ForumTag {
            id: 2,
            name: "공지".into(),
            emoji: None,
        },
    ]);
    let forum = a.ok::<m::ChannelCreate>(p).await.channel;
    wait_channel(&b, space, forum).await;
    let mut v = channel_params(space, "휘발 포럼", ChannelKind::Forum);
    v.security = Some(SecurityLevel::Volatile);
    let r = a.call::<m::ChannelCreate>(v).await;
    assert_eq!(
        code(r),
        ERR_INVALID_INPUT,
        "게시글 변경이 디스크에 남는 휘발성 포럼"
    );

    // API 경로: 게시글·댓글·권한.
    let r = a
        .call::<m::MessageSend>(send_params(space, forum, "게시글 밖", None))
        .await;
    assert_eq!(code(r), ERR_INVALID_INPUT);
    let r = a
        .call::<m::ForumCreatePost>(ForumCreatePostParams {
            space,
            channel: forum,
            title: "태그 틀림".into(),
            text: "본문".into(),
            tags: vec![9],
            attachments: Vec::new(),
            featured: false,
        })
        .await;
    assert_eq!(code(r), ERR_INVALID_INPUT);
    let post = a
        .ok::<m::ForumCreatePost>(ForumCreatePostParams {
            space,
            channel: forum,
            title: "첫 글".into(),
            text: "첫 본문".into(),
            tags: vec![1],
            attachments: Vec::new(),
            featured: false,
        })
        .await
        .id;
    let v = wait_post(&b, space, forum, post, "B가 게시글을 봄", |_| true).await;
    assert_eq!(v.title, "첫 글");
    assert_eq!(v.tags, vec![1]);
    let tagged = b
        .ok::<m::ForumListPosts>(ForumListPostsParams {
            space,
            channel: forum,
            sort: ForumSort::Created,
            tag: Some(2),
            order: Default::default(),
            tags: Vec::new(),
            tag_match: Default::default(),
        })
        .await;
    assert!(tagged.is_empty(), "태그 거르기");
    let r = b
        .call::<m::ForumUpdatePost>(ForumUpdatePostParams {
            space,
            channel: forum,
            post,
            title: Some("남의 글".into()),
            tags: None,
            pinned: None,
            locked: None,
        })
        .await;
    assert_eq!(code(r), ERR_FORBIDDEN, "남의 게시글 제목");
    let r = b
        .call::<m::ForumUpdatePost>(ForumUpdatePostParams {
            space,
            channel: forum,
            post,
            title: None,
            tags: None,
            pinned: Some(true),
            locked: None,
        })
        .await;
    assert_eq!(code(r), ERR_FORBIDDEN, "권한 없이 고정");
    b.ok::<m::MessageSend>(send_params(space, forum, "첫 댓글", Some(post)))
        .await;
    // 포럼은 글·댓글이 여러 갈래라, 메시지를 고르지 않고 읽음 처리하면 채널 전체를 읽은 것으로 봅니다.
    wait_post(&a, space, forum, post, "A가 댓글 수를 봄", |p| {
        p.reply_count == 1
    })
    .await;
    // 글타래 목록은 게시글 자체(본문)부터 보여 줍니다.
    let thread = eventually("A가 댓글을 받음", || async {
        let r = a
            .ok::<m::MessageList>(MessageListParams {
                space,
                channel: forum,
                before: None,
                limit: Some(50),
                thread: Some(post),
            })
            .await;
        (r.messages.len() == 2).then_some(r.messages)
    })
    .await;
    assert_eq!(thread[0].id, post);
    assert_eq!(thread[0].text, "첫 본문");
    assert_eq!(thread[1].text, "첫 댓글");
    // 더 오래된 쪽을 물으면 아무것도 없습니다.
    let older = a
        .ok::<m::MessageList>(MessageListParams {
            space,
            channel: forum,
            before: Some(post),
            limit: Some(50),
            thread: Some(post),
        })
        .await;
    assert!(older.messages.is_empty());
    let forum_unread = |chans: Vec<ChannelView>| {
        chans
            .into_iter()
            .find(|c| c.channel == forum)
            .map(|c| c.unread)
    };
    assert_eq!(
        forum_unread(a.ok::<m::ChannelList>(SpaceParams { space }).await),
        Some(1)
    );
    a.ok::<m::ChannelMarkRead>(MarkReadParams {
        space,
        channel: forum,
        event: None,
    })
    .await;
    assert_eq!(
        forum_unread(a.ok::<m::ChannelList>(SpaceParams { space }).await),
        Some(0)
    );
    a.ok::<m::ForumUpdatePost>(ForumUpdatePostParams {
        space,
        channel: forum,
        post,
        title: Some("고친 제목".into()),
        tags: Some(vec![1, 2]),
        pinned: None,
        locked: Some(true),
    })
    .await;
    let v = wait_post(&b, space, forum, post, "B가 잠금을 봄", |p| p.locked).await;
    assert_eq!(v.title, "고친 제목");
    assert_eq!(v.tags, vec![1, 2]);
    let r = b
        .call::<m::MessageSend>(send_params(space, forum, "잠긴 뒤 댓글", Some(post)))
        .await;
    assert_eq!(code(r), ERR_FORBIDDEN, "잠긴 게시글에 댓글");

    // 순서: 게시글보다 변경이 먼저 오고, 변경끼리도 거꾸로 와도 논리 시계가 큰 변경이 이깁니다.
    let body = EventBody::ForumPost {
        title: "순서".into(),
        tags: Vec::new(),
        first: MessageBody {
            text: "순서 본문".into(),
            ..Default::default()
        },
    };
    let post_evs = testkit::publish_message_unpropagated(&a.node, space, forum, body)
        .await
        .unwrap();
    let late = post_evs.last().unwrap().id();
    let u1 = testkit::publish_state_unpropagated(
        &a.node,
        space,
        Some(forum),
        update(late, Some("둘"), Some(true)),
    )
    .await
    .unwrap();
    let u2 = testkit::publish_state_unpropagated(
        &a.node,
        space,
        Some(forum),
        update(late, Some("셋"), None),
    )
    .await
    .unwrap();
    assert_eq!(u2.header.kind, EventKind::ForumPostUpdate);
    for ev in [u2.clone(), u1.clone()] {
        testkit::ingest_from(&b.node, space, vec![ev], a.peer())
            .await
            .unwrap();
    }
    assert!(
        posts(&b, space, forum).await.iter().all(|p| p.id != late),
        "게시글이 먼저 와 버림(시험 전제)"
    );
    testkit::ingest_from(&b.node, space, post_evs, a.peer())
        .await
        .unwrap();
    let v = wait_post(&b, space, forum, late, "기다리던 변경 반영", |p| {
        p.title == "셋"
    })
    .await;
    assert!(v.pinned, "앞선 변경의 고정이 남음");
    // 고정된 글이 먼저 옵니다.
    assert_eq!(
        posts(&b, space, forum).await.first().map(|p| p.id),
        Some(late)
    );

    // 게시글을 아는 상태에서 거꾸로 온 변경: 항목마다 더 나중 변경이 이기므로, 늦게 도착한 앞선 변경의 고정 해제는
    // 남고 제목은 덮이지 않습니다(받은 순서와 상관없이 A와 같은 결과).
    let u3 = testkit::publish_state_unpropagated(
        &a.node,
        space,
        Some(forum),
        update(late, Some("넷"), Some(false)),
    )
    .await
    .unwrap();
    let u4 = testkit::publish_state_unpropagated(
        &a.node,
        space,
        Some(forum),
        update(late, Some("다섯"), None),
    )
    .await
    .unwrap();
    for ev in [u4, u3] {
        testkit::ingest_from(&b.node, space, vec![ev], a.peer())
            .await
            .unwrap();
    }
    let v = wait_post(
        &b,
        space,
        forum,
        late,
        "항목별 마지막 쓰기 우선",
        |p| !p.pinned,
    )
    .await;
    assert_eq!(v.title, "다섯");
    let a_view = posts(&a, space, forum).await;
    let av = a_view.iter().find(|p| p.id == late).unwrap();
    assert_eq!((av.title.as_str(), av.pinned), ("다섯", false));
}

/// 권한을 주는 상태 이벤트보다 먼저 와서 거부된 게시글 변경도, 그 이벤트가 와서 다시 해소하며 적용되면 게시글에
/// 반영됩니다(거부될 때 본문을 버리면 잃음).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn forum_update_rejected_first_applies_when_the_missing_grant_arrives() {
    let cl = Cluster::new();
    let (a, b, c, space, _) = three_member_crew(&cl).await;
    let forum = a
        .ok::<m::ChannelCreate>(channel_params(space, "게시판", ChannelKind::Forum))
        .await
        .channel;
    let mods = create_role(&a, space, "글 관리", &["manage_messages"], 5).await;
    for n in [&b, &c] {
        wait_role_known(n, space, mods).await;
        wait_channel(n, space, forum).await;
    }
    let post = a
        .ok::<m::ForumCreatePost>(ForumCreatePostParams {
            space,
            channel: forum,
            title: "글".into(),
            text: "본문".into(),
            tags: Vec::new(),
            attachments: Vec::new(),
            featured: false,
        })
        .await
        .id;
    for n in [&b, &c] {
        wait_post(n, space, forum, post, "게시글을 봄", |_| true).await;
    }
    // A가 B에게 역할을 주고(C는 아직 모름), B가 그 권한으로 고정합니다.
    let grant = testkit::publish_state_unpropagated(
        &a.node,
        space,
        None,
        EventBody::MemberUpdate {
            user: b.user(),
            roles: vec![mods],
            nickname: None,
        },
    )
    .await
    .unwrap();
    testkit::ingest_from(&b.node, space, vec![grant.clone()], a.peer())
        .await
        .unwrap();
    let pin = testkit::publish_state_unpropagated(
        &b.node,
        space,
        Some(forum),
        update(post, None, Some(true)),
    )
    .await
    .unwrap();
    // C: 고정이 먼저 오면 B에게 권한이 없어 거부 → 역할 이벤트가 빈 부모를 채우면 다시 해소하며 적용.
    let r = testkit::ingest_from(&c.node, space, vec![pin.clone()], b.peer())
        .await
        .unwrap();
    assert!(
        r.rejected.iter().any(|(id, _)| *id == pin.id()),
        "권한 없는 고정이 적용됨(시험 전제)"
    );
    testkit::ingest_from(&c.node, space, vec![grant], a.peer())
        .await
        .unwrap();
    wait_post(
        &c,
        space,
        forum,
        post,
        "다시 해소된 고정이 반영됨",
        |p| p.pinned,
    )
    .await;
}

/// 먼저 적용된 고정이 동시에 일어난 권한 회수에 밀려 다시 해소하며 빠지면, 게시글도 고정 전으로 돌아갑니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn forum_pin_is_undone_when_a_concurrent_role_removal_wins() {
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let b = cl.node("b").await;
    let space = create_crew(&a, "포럼 크루").await;
    assert_eq!(join(&a, &b, space).await, InviteJoinStatus::Joined);
    let mods = create_role(&a, space, "글 관리", &["manage_messages"], 5).await;
    set_roles(&a, space, b.user(), vec![mods]).await;
    wait_roles(&b, space, b.user(), &[mods]).await;
    let forum = a
        .ok::<m::ChannelCreate>(channel_params(space, "게시판", ChannelKind::Forum))
        .await
        .channel;
    wait_channel(&b, space, forum).await;
    let post = a
        .ok::<m::ForumCreatePost>(ForumCreatePostParams {
            space,
            channel: forum,
            title: "글".into(),
            text: "본문".into(),
            tags: Vec::new(),
            attachments: Vec::new(),
            featured: false,
        })
        .await
        .id;
    wait_post(&b, space, forum, post, "B가 게시글을 봄", |_| true).await;

    // 서로 모르는 채로: B는 고정하고, A(소유자)는 B의 역할을 뺍니다.
    let pin = testkit::publish_state_unpropagated(
        &b.node,
        space,
        Some(forum),
        update(post, None, Some(true)),
    )
    .await
    .unwrap();
    let demote = testkit::publish_state_unpropagated(
        &a.node,
        space,
        None,
        EventBody::MemberUpdate {
            user: b.user(),
            roles: Vec::new(),
            nickname: None,
        },
    )
    .await
    .unwrap();
    wait_post(&b, space, forum, post, "B에게는 먼저 고정됨", |p| {
        p.pinned
    })
    .await;
    // 순위가 높은 소유자의 변경이 먼저 놓이므로 B의 고정은 권한 없이 한 것이 됩니다.
    testkit::ingest_from(&b.node, space, vec![demote], a.peer())
        .await
        .unwrap();
    testkit::ingest_from(&a.node, space, vec![pin], b.peer())
        .await
        .unwrap();
    wait_post(&b, space, forum, post, "B에게서 고정이 풀림", |p| {
        !p.pinned
    })
    .await;
    let av = posts(&a, space, forum).await;
    assert!(!av.iter().find(|p| p.id == post).unwrap().pinned);
}

// ───────────── 10. 초대 만료·횟수·폐기 ─────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn invite_expiry_max_uses_and_revoke_are_enforced() {
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let b = cl.node("b").await;
    let c = cl.node("c").await;
    let space = create_crew(&a, "초대 크루").await;
    connect_both(&a, &b).await;
    connect_both(&a, &c).await;

    // 한 번만 쓸 수 있는 링크.
    let once = invite(&a, space, Some(1), None).await;
    let r = b.ok::<m::InviteJoin>(UrlParams { url: once.clone() }).await;
    assert_eq!(r.status, InviteJoinStatus::Joined);
    let r = c.call::<m::InviteJoin>(UrlParams { url: once }).await;
    assert_eq!(code(r), ERR_INVALID_INPUT, "다 쓴 링크");
    let views = a.ok::<m::InviteList>(SpaceParams { space }).await;
    assert!(views.iter().any(|v| v.max_uses == Some(1) && v.uses == 1));

    // 폐기한 링크.
    let revoked = invite(&a, space, None, None).await;
    let hash = yangpa_core::links::token_hash(&InviteLink::parse(&revoked).unwrap().token);
    a.ok::<m::InviteRevoke>(InviteRevokeParams {
        space,
        token_hash: yangpa_rpc::api::serde_util::TokenHash(hash),
    })
    .await;
    let r = c.call::<m::InviteJoin>(UrlParams { url: revoked }).await;
    assert_eq!(code(r), ERR_INVALID_INPUT, "폐기한 링크");

    // 만료: 링크의 만료 시각을 지운 사본으로도 받아 주는 노드가 거절합니다.
    let short = invite(&a, space, None, Some(1)).await;
    let mut link = InviteLink::parse(&short).unwrap();
    let expires = link.expires_at.unwrap();
    link.expires_at = None;
    let forged = link.to_url().unwrap();
    eventually("초대 만료", || async {
        (Timestamp::now().0 > expires.0).then_some(())
    })
    .await;
    let r = c.call::<m::InviteJoin>(UrlParams { url: short }).await;
    assert_eq!(code(r), ERR_INVALID_INPUT, "만료된 링크");
    let r = c.call::<m::InviteJoin>(UrlParams { url: forged }).await;
    assert_eq!(code(r), ERR_INVALID_INPUT, "만료 시각을 지운 링크");
    assert!(!testkit::is_pending_join(&c.node, &space));
    assert!(testkit::space_state(&c.node, &space).is_none());

    // 유효한 링크로는 들어옵니다.
    let ok = invite(&a, space, None, Some(3600)).await;
    let r = c.ok::<m::InviteJoin>(UrlParams { url: ok }).await;
    assert_eq!(r.status, InviteJoinStatus::Joined);
    wait_same_crew_key(&[&a, &b, &c], space).await;
}

// ───────────── 11. 커밋 확정 노드가 꺼졌을 때 ─────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn committer_offline_leaves_pending_commit_then_resumes() {
    let cl = Cluster::new();
    let mut a = cl.node("a").await;
    let b = cl.node("b").await;
    let c = cl.node("c").await;
    let space = create_crew(&a, "느긋한 크루").await;
    let channel = a.first_channel(space).await;
    assert_eq!(join(&a, &b, space).await, InviteJoinStatus::Joined);
    let inviter = create_role(&a, space, "초대 담당", &["create_invite"], 1).await;
    set_roles(&a, space, b.user(), vec![inviter]).await;
    wait_roles(&b, space, b.user(), &[inviter]).await;
    wait_same_crew_key(&[&a, &b], space).await;

    a.shutdown().await;
    // B가 받아 주지만 확정 기기(A)가 없어 C는 가입 키 묶음을 기다립니다.
    let status = join(&b, &c, space).await;
    assert_eq!(status, InviteJoinStatus::Pending);
    assert!(testkit::is_pending_join(&c.node, &space));
    eventually("B가 커밋 대기를 봄", || async {
        let v = b.ok::<m::SpaceGet>(SpaceParams { space }).await;
        v.pending_commit.then_some(())
    })
    .await;
    let r = c
        .call::<m::MessageSend>(send_params(space, channel, "아직", None))
        .await;
    assert!(r.is_err(), "가입 전 메시지가 보내짐");

    a.start_again().await;
    connect_both(&a, &b).await;
    connect_both(&a, &c).await;
    wait_joined(&c, space).await;
    wait_same_crew_key(&[&a, &b, &c], space).await;
    eventually("커밋 대기 해소", || async {
        let v = b.ok::<m::SpaceGet>(SpaceParams { space }).await;
        (!v.pending_commit).then_some(())
    })
    .await;
    assert!(!testkit::is_pending_join(&c.node, &space));
    meet(&a, &c, space).await;
    c.send(space, channel, "드디어").await;
    sees_text(&a, space, channel, "드디어").await;
    sees_text(&b, space, channel, "드디어").await;
}

/// 커밋 대기가 풀리면 확정 기기(내 커밋)와 다른 멤버(받은 커밋) 모두 space.updated를 내서 앱이 다시 읽게 합니다.
/// 키 갱신은 해소된 공간 상태를 바꾸지 않아(크루 키 사슬만), 알리지 않으면 앱의 "변경 사항을 확정하는 중…"이 다시
/// 열 때까지 남습니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn space_updated_when_pending_commit_clears() {
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let b = cl.node("b").await;
    let c = cl.node("c").await;
    let space = create_crew(&a, "알리는 크루").await;
    assert_eq!(join(&a, &b, space).await, InviteJoinStatus::Joined);
    meet(&a, &b, space).await;
    wait_same_crew_key(&[&a, &b], space).await;

    testkit::hold_commits(&a.node, true);
    connect_both(&a, &c).await;
    let url = invite(&a, space, None, None).await;
    let node = c.node.clone();
    let joining =
        tokio::spawn(async move { common::call::<m::InviteJoin>(&node, UrlParams { url }).await });
    wait_member(&a, space, c.user(), true).await;
    wait_member(&b, space, c.user(), true).await;
    for n in [&a, &b] {
        eventually(&format!("{}가 커밋 대기를 봄", n.name), || async {
            let v = n.ok::<m::SpaceGet>(SpaceParams { space }).await;
            v.pending_commit.then_some(())
        })
        .await;
    }

    let mut rx_a = a.subscribe();
    let mut rx_b = b.subscribe();
    testkit::hold_commits(&a.node, false);
    for (n, rx) in [(&a, &mut rx_a), (&b, &mut rx_b)] {
        loop {
            next_notification(rx, &format!("{}의 space.updated", n.name), |x| {
                matches!(x, Notification::SpaceUpdated { space: s } if *s == space).then_some(())
            })
            .await;
            let v = n.ok::<m::SpaceGet>(SpaceParams { space }).await;
            if !v.pending_commit {
                break;
            }
        }
    }
    let r = joining.await.expect("작업").expect("가입");
    assert_eq!(r.space, space);
}

// ───────────── P2P 전용 채널 ─────────────

/// P2P 전용 채널의 글은 참가한 기기끼리 P2P 스웜으로만 오가고, 참가하지 않은 멤버는 Tor로도 받지 못합니다. Tor로
/// 되돌리면 그동안의 기록이 Tor로 모두에게 갑니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn p2p_channel_content_goes_only_between_joined_devices() {
    let cl = Cluster::new();
    let (a, b, c, space, _) = three_member_crew(&cl).await;
    let mut params = channel_params(space, "직접", ChannelKind::Text);
    params.p2p = Some(true);
    let ch = a.ok::<m::ChannelCreate>(params).await.channel;
    for n in [&b, &c] {
        eventually(&format!("{}가 P2P 전용 채널을 앎", n.name), || async {
            testkit::space_state(&n.node, &space)?
                .is_p2p(&ch)
                .then_some(())
        })
        .await;
    }
    // 참가하기 전에는 보낼 수 없습니다.
    let before = a
        .call::<m::MessageSend>(send_params(space, ch, "참가 전", None))
        .await;
    assert_eq!(code(before), ERR_FORBIDDEN);
    for n in [&a, &b] {
        n.ok::<m::ChannelP2pJoin>(SpaceChannelParams { space, channel: ch })
            .await;
    }
    eventually("A와 B가 P2P 스웜에서 서로 인증", || async {
        (testkit::p2p_peers(&a.node).len() == 1 && testkit::p2p_peers(&b.node).len() == 1)
            .then_some(())
    })
    .await;
    assert!(
        !testkit::p2p_running(&c.node),
        "참가하지 않은 기기는 P2P 스웜을 띄우지 않음"
    );
    a.send(space, ch, "A의 P2P 글").await;
    b.send(space, ch, "B의 P2P 글").await;
    sees_text(&b, space, ch, "A의 P2P 글").await;
    sees_text(&a, space, ch, "B의 P2P 글").await;

    // 시더 범위: P2P 전용 채널은 직접 연결로 셉니다(A 자신 + B). 참가하지 않은 C는 셀 수 없어 결과에서 뺍니다.
    eventually("A가 B를 P2P 전용 채널의 시더로 셈", || async {
        a.ok::<m::SeedCoverage>(SpaceParams { space })
            .await
            .into_iter()
            .find(|x| x.channel == ch && x.online_seeders == 2)
    })
    .await;
    let cov = c.ok::<m::SeedCoverage>(SpaceParams { space }).await;
    assert!(cov.iter().all(|x| x.channel != ch), "{cov:?}");
    // 아무도 열지 않은 채널은 0으로 보고 경고는 한 번만 합니다. 기록이 없는 통화 채널은 세지 않습니다.
    let empty = a
        .ok::<m::ChannelCreate>(channel_params(space, "빈", ChannelKind::Text))
        .await
        .channel;
    let voice = a
        .ok::<m::ChannelCreate>(channel_params(space, "통화", ChannelKind::Voice))
        .await
        .channel;
    let mut rx = a.subscribe();
    for _ in 0..2 {
        let cov = a.ok::<m::SeedCoverage>(SpaceParams { space }).await;
        assert!(cov.iter().all(|x| x.channel != voice), "{cov:?}");
        assert!(
            cov.iter()
                .any(|x| x.channel == empty && x.online_seeders == 0),
            "{cov:?}"
        );
    }
    let warned = std::iter::from_fn(|| rx.try_recv().ok())
        .filter(
            |n| matches!(n, Notification::SeedCoverageWarning { channel, .. } if *channel == empty),
        )
        .count();
    assert_eq!(warned, 1, "0인 채널 경고를 거듭 보내지 않음");

    // 참가하지 않은 C에게는 오지 않고, Tor로 달라고 해도 주지 않으며, C는 쓸 수도 없습니다.
    let stream = StreamId::Channel(ch);
    assert!(
        testkit::stream_events(&c.node, &space, stream)
            .expect("저장소")
            .is_empty()
    );
    let r = testkit::net_request(&c.node, a.peer(), YpRequest::GetHeads { space, stream })
        .await
        .expect("응답");
    assert!(
        matches!(
            r,
            YpResponse::Error {
                code: WireErrorCode::NotFound,
                ..
            }
        ),
        "{r:?}"
    );
    let refused = c
        .call::<m::MessageSend>(send_params(space, ch, "C 글", None))
        .await;
    assert_eq!(code(refused), ERR_FORBIDDEN);

    // Tor로 되돌리면 P2P로만 오가던 기록이 C에게도 Tor로 가고, P2P 스웜은 멈춥니다.
    a.ok::<m::ChannelSetP2p>(ChannelSetP2pParams {
        space,
        channel: ch,
        enabled: false,
    })
    .await;
    sees_text(&c, space, ch, "A의 P2P 글").await;
    sees_text(&c, space, ch, "B의 P2P 글").await;
    c.send(space, ch, "Tor로 돌아옴").await;
    sees_text(&a, space, ch, "Tor로 돌아옴").await;
    sees_text(&b, space, ch, "Tor로 돌아옴").await;
    eventually("P2P 스웜이 멈춤", || async {
        (!testkit::p2p_running(&a.node) && !testkit::p2p_running(&b.node)).then_some(())
    })
    .await;
}

// ───────────── 신뢰 스위치 ─────────────

async fn mode(n: &TestNode, of: &TestNode) -> ConnectionMode {
    n.ok::<m::TrustGet>(UserParams { user: of.user() })
        .await
        .connection
}

/// 시간이 지나면 마지막으로 본 양쪽 연결 방식을 남깁니다(간헐 실패에서 상대 쪽만 늦게 바뀌었는지 가리게).
async fn wait_mode(n: &TestNode, of: &TestNode, want: ConnectionMode) {
    let deadline = std::time::Instant::now() + common::TIMEOUT;
    loop {
        let here = mode(n, of).await;
        if here == want {
            return;
        }
        if std::time::Instant::now() >= deadline {
            let there = mode(of, n).await;
            panic!(
                "시간 초과: {}가 본 {}와의 연결이 {want:?}(마지막 {here:?}, 반대쪽 {there:?})",
                n.name, of.name
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// 크루 신뢰는 그 크루 멤버에게 닿고, 서로 신뢰하면 P2P, 한쪽만 신뢰하면 신뢰하지 않은 쪽이 숨은 채 겁니다(단방향).
/// 사람 신뢰를 더하면 단방향이 P2P가 되고, 끄면 다시 단방향으로 돌아갑니다. 어느 쪽이든 일반 채널 글은 오갑니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn trust_switch_picks_p2p_or_one_way_per_pair() {
    let cl = Cluster::new();
    let (a, b, c, space, ch) = three_member_crew(&cl).await;
    for n in [&a, &b, &c] {
        for of in [&a, &b, &c] {
            if n.name != of.name {
                assert_eq!(mode(n, of).await, ConnectionMode::Tor);
            }
        }
    }
    assert!(!testkit::p2p_running(&a.node));

    // B와 C가 크루를 신뢰: B·C는 서로 P2P, 신뢰하지 않은 A는 둘에게 숨은 채 겁니다.
    for n in [&b, &c] {
        n.ok::<m::TrustSetSpace>(TrustSetSpaceParams {
            space,
            enabled: true,
        })
        .await;
        assert!(n.ok::<m::SpaceGet>(SpaceParams { space }).await.trusted);
    }
    wait_mode(&b, &c, ConnectionMode::P2p).await;
    wait_mode(&c, &b, ConnectionMode::P2p).await;
    for n in [&b, &c] {
        wait_mode(&a, n, ConnectionMode::OneWayPeerExposed).await;
        wait_mode(n, &a, ConnectionMode::OneWayMeExposed).await;
    }
    // A는 아무도 신뢰하지 않아 받는 직접 스웜을 띄우지 않습니다(A의 주소는 아무에게도 가지 않음).
    assert!(!testkit::p2p_running(&a.node));
    let v = b.ok::<m::TrustGet>(UserParams { user: a.user() }).await;
    assert!(!v.trusted, "사람 스위치는 꺼져 있음");
    assert_eq!(v.via_crews, vec![space]);

    a.send(space, ch, "숨은 A의 글").await;
    b.send(space, ch, "B의 글").await;
    sees_text(&b, space, ch, "숨은 A의 글").await;
    sees_text(&c, space, ch, "숨은 A의 글").await;
    sees_text(&a, space, ch, "B의 글").await;
    sees_text(&c, space, ch, "B의 글").await;

    // A가 B를 사람 단위로 신뢰: 서로 신뢰가 되어 A·B는 P2P, A·C는 그대로 단방향.
    a.ok::<m::TrustSetUser>(TrustSetUserParams {
        user: b.user(),
        enabled: true,
    })
    .await;
    wait_mode(&a, &b, ConnectionMode::P2p).await;
    wait_mode(&b, &a, ConnectionMode::P2p).await;
    assert_eq!(mode(&a, &c).await, ConnectionMode::OneWayPeerExposed);
    c.send(space, ch, "C의 글").await;
    sees_text(&a, space, ch, "C의 글").await;

    // B가 크루 신뢰를 끄면: A만 B를 신뢰하므로 B가 숨은 채 A에게 겁니다. B·C는 C만 신뢰하므로 B가 숨습니다.
    b.ok::<m::TrustSetSpace>(TrustSetSpaceParams {
        space,
        enabled: false,
    })
    .await;
    wait_mode(&b, &a, ConnectionMode::OneWayPeerExposed).await;
    wait_mode(&a, &b, ConnectionMode::OneWayMeExposed).await;
    wait_mode(&b, &c, ConnectionMode::OneWayPeerExposed).await;
    wait_mode(&c, &b, ConnectionMode::OneWayMeExposed).await;
    assert!(!testkit::p2p_running(&b.node), "B는 아무도 신뢰하지 않음");
    b.send(space, ch, "다시 숨은 B의 글").await;
    sees_text(&a, space, ch, "다시 숨은 B의 글").await;

    // 나 자신·모르는 사람은 신뢰할 수 없습니다.
    let me = a
        .call::<m::TrustSetUser>(TrustSetUserParams {
            user: a.user(),
            enabled: true,
        })
        .await;
    assert_eq!(code(me), ERR_INVALID_INPUT);
}

/// 직접 연결(서로 신뢰한 사람, P2P 전용 채널)로 받는 첨부는 Tor 한도에 묶이지 않습니다. 한 기기가 겹쳐 보낸 조각
/// 요청끼리 시더에서 서로를 바쁨으로 만들지 않고(예전에는 바쁨 뒤 다음 조회까지 최대 10초씩 멈춤), 조각 요청을 잰
/// 속도에 맞춰 겹칩니다. 여러 파일을 함께 받아도 피어당 조각 상한 안에서 받습니다. 단방향 신뢰는 한쪽이 Tor 출구를
/// 거치므로 Tor 한도 그대로입니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn direct_links_fetch_files_without_tor_throttling() {
    use yangpa_rpc::api::{AttachmentInput, BlobFetchParams, BlobParams};
    let cl = Cluster::new();
    let (a, b, c, space, general) = three_member_crew(&cl).await;
    for n in [&a, &b] {
        testkit::sfu::enter_tor_policy(&n.node).await;
    }
    // C는 받지 않아 A만 시더입니다(C의 Tor 연결이 한도를 정하지 않게).
    testkit::blobs::hold_downloads(&c.node, Some(0));
    assert_eq!(
        testkit::blobs::chunk_window(&b.node, &[c.peer()]),
        Some((3, 4)),
        "Tor 피어에게서는 Tor 한도"
    );

    let size = 4 * 1024 * 1024;
    let fetch = |label: &'static str, channel: ChannelId| {
        let (a, b) = (&a, &b);
        async move {
            let dir = a.dir.join("files");
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join(format!("{label}.bin"));
            let salt = label
                .bytes()
                .fold(0u8, |h, c| h.wrapping_mul(31).wrapping_add(c));
            let data: Vec<u8> = common::noise(size, salt);
            std::fs::write(&path, &data).unwrap();
            let id = a
                .ok::<m::MessageSend>(MessageSendParams {
                    space,
                    channel,
                    text: "파일".into(),
                    attachments: vec![AttachmentInput {
                        path: path.to_string_lossy().into_owned(),
                        ..Default::default()
                    }],
                    reply_to: None,
                    thread: None,
                })
                .await
                .id;
            let blob = a
                .messages(space, channel)
                .await
                .into_iter()
                .find(|v| v.id == id)
                .unwrap()
                .attachments[0]
                .blob;
            eventually("B가 메시지를 받음", || async {
                b.messages(space, channel)
                    .await
                    .into_iter()
                    .any(|v| v.id == id)
                    .then_some(())
            })
            .await;
            // Tor 기본값의 자동 받기 한도(2MiB)보다 커서 직접 받습니다. 메시지 줄이 보인 뒤 첨부 기록이 남기 전이면
            // '쓰이지 않는 블롭'이라 될 때까지 다시 부릅니다.
            eventually("B가 직접 받기를 시작", || async {
                b.call::<m::BlobFetch>(BlobFetchParams {
                    space,
                    blob,
                    priority: None,
                })
                .await
                .ok()
            })
            .await;
            let total = eventually(label, || async {
                let s = b.ok::<m::BlobStatus>(BlobParams { space, blob }).await;
                (s.total_chunks > 0 && s.have_chunks == s.total_chunks).then_some(s.total_chunks)
            })
            .await;
            let st = testkit::blobs::fetch_stats(&b.node, &blob);
            assert_eq!(
                st.busy_replies, 0,
                "{label}: 내 겹친 요청끼리 바쁨이 나면 안 됨: {st:?}"
            );
            assert_eq!(st.chunks_from.values().sum::<u32>(), total, "{st:?}");
        }
    };

    // A만 B를 신뢰(단방향): B는 Tor 출구를 거쳐 걸므로 Tor 한도 그대로입니다.
    a.ok::<m::TrustSetUser>(TrustSetUserParams {
        user: b.user(),
        enabled: true,
    })
    .await;
    wait_mode(&b, &a, ConnectionMode::OneWayPeerExposed).await;
    assert_eq!(
        testkit::blobs::chunk_window(&b.node, &[a.peer()]),
        Some((3, 4))
    );

    // 서로 신뢰: 일반 채널의 첨부도 Tor를 거치지 않는 직접 연결로 받습니다.
    b.ok::<m::TrustSetUser>(TrustSetUserParams {
        user: a.user(),
        enabled: true,
    })
    .await;
    wait_mode(&b, &a, ConnectionMode::P2p).await;
    eventually(
        "A의 Tor 피어로 보낼 조각이 직접 연결로 감",
        || async {
            testkit::blobs::chunk_window(&b.node, &[a.peer()])
                .is_none()
                .then_some(())
        },
    )
    .await;
    fetch("신뢰한 사람의 첨부를 다 받음", general).await;

    // P2P 전용 채널: 함께 참가한 P2P 피어에게서 받습니다.
    let mut params = channel_params(space, "직접", ChannelKind::Text);
    params.p2p = Some(true);
    let p2p_ch = a.ok::<m::ChannelCreate>(params).await.channel;
    eventually("B가 P2P 채널을 앎", || async {
        testkit::space_state(&b.node, &space)?
            .is_p2p(&p2p_ch)
            .then_some(())
    })
    .await;
    for n in [&a, &b] {
        n.ok::<m::ChannelP2pJoin>(SpaceChannelParams {
            space,
            channel: p2p_ch,
        })
        .await;
    }
    let p2p_peer = eventually("B가 A와 P2P로 인증", || async {
        testkit::p2p_peers(&b.node).first().copied()
    })
    .await;
    assert_eq!(testkit::blobs::chunk_window(&b.node, &[p2p_peer]), None);
    fetch("P2P 채널 첨부를 다 받음", p2p_ch).await;

    // 셋을 함께 받기(동시 내려받기 3개): 답을 늦춰 겹치게 하면 조각 요청 합이 피어당 상한을 넘으려 합니다. 넘는 것은
    // 보내지 않고 잠깐 쉬어, 망 계층의 상한에 걸려 그 피어를 실패로 빼는 일이 없어야 합니다.
    testkit::blobs::delay_serving(&a.node, 100);
    tokio::join!(
        fetch("함께 받는 첨부 1", p2p_ch),
        fetch("함께 받는 첨부 2", p2p_ch),
        fetch("함께 받는 첨부 3", p2p_ch),
    );
}

/// `from`이 `mib` MiB 첨부를 보내고 `to`가 받기를 마칠 때까지 걸린 시간(메시지가 닿은 뒤부터).
async fn timed_fetch(
    from: &TestNode,
    to: &TestNode,
    space: SpaceId,
    ch: ChannelId,
    mib: u32,
) -> (BlobId, std::time::Duration) {
    use yangpa_rpc::api::{AttachmentInput, BlobFetchParams, BlobParams};
    let data: Vec<u8> = common::noise((mib * 1024 * 1024) as usize, 13);
    let dir = from.dir.join("files");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{}-{mib}.bin", ch.0[0]));
    std::fs::write(&path, &data).unwrap();
    let id = from
        .ok::<m::MessageSend>(MessageSendParams {
            space,
            channel: ch,
            text: "파일".into(),
            attachments: vec![AttachmentInput {
                path: path.to_string_lossy().into_owned(),
                ..Default::default()
            }],
            reply_to: None,
            thread: None,
        })
        .await
        .id;
    let blob = from
        .messages(space, ch)
        .await
        .into_iter()
        .find(|v| v.id == id)
        .expect("보낸 메시지")
        .attachments[0]
        .blob;
    eventually(
        &format!("{}가 첨부 메시지를 받음", to.name),
        || async {
            to.messages(space, ch)
                .await
                .into_iter()
                .any(|v| v.id == id)
                .then_some(())
        },
    )
    .await;
    // 메시지 행이 먼저 보이고 첨부 기록은 그 뒤에 남으므로, 받을 블롭으로 알려질 때까지 기다립니다.
    eventually(&format!("{}가 첨부를 앎", to.name), || async {
        to.call::<m::BlobStatus>(BlobParams { space, blob })
            .await
            .ok()
            .map(|_| ())
    })
    .await;
    let t0 = std::time::Instant::now();
    to.ok::<m::BlobFetch>(BlobFetchParams {
        space,
        blob,
        priority: None,
    })
    .await;
    eventually(&format!("{}가 첨부를 다 받음", to.name), || async {
        let s = to.ok::<m::BlobStatus>(BlobParams { space, blob }).await;
        (s.total_chunks > 0 && s.have_chunks == s.total_chunks).then_some(())
    })
    .await;
    (blob, t0.elapsed())
}

/// `from`이 늦춰 답한 조각 요청 가운데 뒤의 절반이 왔을 때 함께 늦춰지던 수의 최댓값. 받는 쪽 겹침 한도가 `n`이면 `n`을
/// 넘지 못하고, 뒤의 절반만 보므로 처음에만 겹쳤다가 줄어도 잡힙니다. 걸린 시간은 PC가 바쁘면(CI 러너는 BelowNormal)
/// 몇 배~수십 배 늘지만 이 값은 거의 그대로라, 회선을 채우는지는 이것으로 봅니다.
fn late_overlap(from: &TestNode) -> u32 {
    let v = testkit::blobs::serving_overlaps(&from.node);
    v[v.len() / 2..].iter().copied().max().unwrap_or(0)
}

/// 직접 연결은 왕복이 길어도 회선을 채웁니다: 받는 쪽이 잰 속도에 맞춰 조각 요청을 겹쳐 보냅니다. 조각 답을 300ms 늦춰
/// 왕복을 흉내 내면 Tor 한도(전체 4개)로는 16MiB에 왕복 16번 넘게, 직접 연결은 겹침이 24까지 늘어 왕복 네 번 남짓
/// 걸립니다. 속도 제한은 비웠습니다(무제한).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn direct_link_fills_a_long_round_trip() {
    use yangpa_rpc::api::{PeerSetParams, SeedSetParams};
    let cl = Cluster::new();
    let (a, b, _c, space, _) = three_member_crew(&cl).await;
    for n in [&a, &b] {
        testkit::sfu::enter_tor_policy(&n.node).await;
        n.ok::<m::SeedSet>(SeedSetParams {
            upload_limit_bps: Some(None),
            ..Default::default()
        })
        .await;
        n.ok::<m::PeerSet>(PeerSetParams {
            download_limit_bps: Some(None),
            ..Default::default()
        })
        .await;
    }
    let mut params = channel_params(space, "직접", ChannelKind::Text);
    params.p2p = Some(true);
    let ch = a.ok::<m::ChannelCreate>(params).await.channel;
    eventually("B가 P2P 채널을 앎", || async {
        testkit::space_state(&b.node, &space)?
            .is_p2p(&ch)
            .then_some(())
    })
    .await;
    for n in [&a, &b] {
        n.ok::<m::ChannelP2pJoin>(SpaceChannelParams { space, channel: ch })
            .await;
    }
    let p2p_peer = eventually("B가 A와 P2P로 인증", || async {
        testkit::p2p_peers(&b.node).first().copied()
    })
    .await;
    assert_eq!(testkit::blobs::chunk_window(&b.node, &[p2p_peer]), None);

    testkit::blobs::delay_serving(&a.node, 300);
    let (blob, took) = timed_fetch(&a, &b, space, ch, 16).await;
    // 받기 후반에도 Tor 한도(4)의 두 배 넘게 겹쳤습니다(PC를 꽉 채운 부하에서도 16 이상, Tor로는 3).
    let overlap = late_overlap(&a);
    assert!(overlap > 8, "왕복 300ms에서 후반 겹침 {overlap}, {took:?}");
    let st = testkit::blobs::fetch_stats(&b.node, &blob);
    assert_eq!(st.busy_replies, 0, "{st:?}");
}

/// Tor 정책으로 돌리고 속도 제한을 풉니다.
async fn tor_unlimited(n: &TestNode) {
    use yangpa_rpc::api::{PeerSetParams, SeedSetParams};
    testkit::sfu::enter_tor_policy(&n.node).await;
    n.ok::<m::SeedSet>(SeedSetParams {
        upload_limit_bps: Some(None),
        ..Default::default()
    })
    .await;
    n.ok::<m::PeerSet>(PeerSetParams {
        download_limit_bps: Some(None),
        ..Default::default()
    })
    .await;
}

async fn set_file_circuits(n: &TestNode, enabled: bool) {
    n.ok::<m::NetSetFileCircuits>(FileCircuitsParams {
        enabled,
        count: None,
    })
    .await;
}

/// Tor로 도는 기기는 큰 첨부를 글과 다른 파일 회로 여럿(회로마다 파일 스웜 하나)으로 받습니다: 회로마다 조각을
/// 겹쳐 보내 왕복이 길어도 빨리 받고, 그동안 글은 주 연결로 바로 옵니다. 루프백에서는 파일 스웜이 Arti 대신 받지 않는
/// 직접 스웜이라 회로 대신 연결만 따로입니다. 조각 답을 300ms 늦추면 주 연결의 Tor 한도(피어당 3)로는 16MiB에 왕복
/// 22번, 파일 회로 넷(회로마다 3)으로는 왕복 여섯 번 걸립니다. 파일 회로는 선택 기능이라 두 기기 모두 켭니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tor_downloads_use_a_separate_file_circuit() {
    let cl = Cluster::new();
    let (a, b, c, space, ch) = three_member_crew(&cl).await;
    for n in [&a, &b] {
        tor_unlimited(n).await;
        set_file_circuits(n, true).await;
    }
    testkit::blobs::delay_serving(&a.node, 300);
    let (_, took) = timed_fetch(&a, &b, space, ch, 16).await;
    // 조각을 가진 A에게만 잇습니다(C는 멤버지만 조각이 없음). 회로마다 스웜 하나.
    let (ready, _, swarms) = testkit::file_links(&b.node);
    assert!(
        swarms >= 1 && (1..=4).contains(&ready),
        "B가 A와 파일 회로를 이음: {ready}개, 스웜 {swarms}개"
    );
    let (_, inbound, _) = testkit::file_links(&a.node);
    assert!(inbound >= 1, "A가 B의 파일 링크를 받아 둠");
    let (_, inbound_c, _) = testkit::file_links(&c.node);
    assert_eq!(inbound_c, 0, "조각이 없는 C에게는 파일 회로를 열지 않음");
    // 64조각 거의 모두 파일 회로로 오고(회로가 이어지기 전 몇 개만 주 연결), 받기 후반에도 주 연결 한도(피어당 3)보다
    // 많이 겹쳤습니다: 회로 여럿에 함께 실림(PC를 꽉 채운 부하에서도 9 이상, 주 연결로는 3).
    let served = testkit::file_link_chunks_served(&a.node);
    assert!(served > 32, "파일 회로로 내준 조각 {served}개");
    let overlap = late_overlap(&a);
    assert!(overlap > 3, "파일 회로로 후반 겹침 {overlap}, {took:?}");
    // 받는 동안에도 글은 주 연결로 곧 옵니다.
    a.send(space, ch, "파일 받은 뒤 글").await;
    sees_text(&b, space, ch, "파일 받은 뒤 글").await;
}

/// 같은 첨부를 직접 연결 상대(서로 신뢰한 A)와 Tor 상대(C)에게서 함께 받으면, A의 조각은 직접 연결로만 받고 A에게는
/// 파일 회로를 열지 않습니다. Tor 상대에게만 파일 회로를 엽니다(셋 다 파일 회로를 켬).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mixed_fetch_keeps_trusted_peer_on_the_direct_link() {
    use yangpa_rpc::api::{BlobFetchParams, BlobParams};
    let cl = Cluster::new();
    let (a, b, c, space, ch) = three_member_crew(&cl).await;
    for n in [&a, &b, &c] {
        tor_unlimited(n).await;
        set_file_circuits(n, true).await;
    }
    for (n, of) in [(&a, &b), (&b, &a)] {
        n.ok::<m::TrustSetUser>(TrustSetUserParams {
            user: of.user(),
            enabled: true,
        })
        .await;
    }
    wait_mode(&b, &a, ConnectionMode::P2p).await;
    eventually("B가 A에게 보낼 조각이 직접 연결로 감", || async {
        testkit::blobs::chunk_window(&b.node, &[a.peer()])
            .is_none()
            .then_some(())
    })
    .await;
    // C가 먼저 받아 시더가 됩니다.
    let (blob, _) = timed_fetch(&a, &c, space, ch, 4).await;
    let served_before = testkit::file_link_chunks_served(&a.node);

    b.ok::<m::BlobFetch>(BlobFetchParams {
        space,
        blob,
        priority: None,
    })
    .await;
    eventually("B가 첨부를 다 받음", || async {
        let s = b.ok::<m::BlobStatus>(BlobParams { space, blob }).await;
        (s.total_chunks > 0 && s.have_chunks == s.total_chunks).then_some(())
    })
    .await;
    let st = testkit::blobs::fetch_stats(&b.node, &blob);
    assert!(
        st.chunks_from.get(&a.peer()).copied().unwrap_or(0) > 0,
        "A에게서도 받음: {st:?}"
    );
    assert_eq!(
        testkit::file_circuits_to(&b.node, a.peer()),
        0,
        "직접 연결 상대 A에게는 파일 회로를 열지 않음"
    );
    assert_eq!(
        testkit::file_link_chunks_served(&a.node),
        served_before,
        "A는 B에게 파일 회로로 내주지 않음"
    );
}

/// 서로 신뢰해 이어진 직접 연결이 끊기면(VPN을 켬, 망을 옮김) 다시 거는 간격(5분)을 기다리지 않고 곧 다시 잇습니다.
/// 끊긴 동안에도 글은 Tor로 오갑니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn trusted_direct_link_reconnects_soon_after_a_drop() {
    let cl = Cluster::new();
    let (a, b, _c, space, ch) = three_member_crew(&cl).await;
    for n in [&a, &b] {
        testkit::sfu::enter_tor_policy(&n.node).await;
    }
    for (n, of) in [(&a, &b), (&b, &a)] {
        n.ok::<m::TrustSetUser>(TrustSetUserParams {
            user: of.user(),
            enabled: true,
        })
        .await;
    }
    wait_mode(&b, &a, ConnectionMode::P2p).await;
    let peer = eventually("B가 A와 직접 연결로 인증", || async {
        testkit::p2p_peers(&b.node).first().copied()
    })
    .await;

    // 끊긴 직후 요청이 몇 ms 안에 다시 걸 수 있어 빈 표 대신 끊긴 수를 봅니다.
    let drops = testkit::p2p_drops(&b.node);
    testkit::cut_p2p(&b.node, peer);
    eventually("직접 연결이 끊김", || async {
        (testkit::p2p_drops(&b.node) > drops).then_some(())
    })
    .await;
    a.send(space, ch, "끊긴 동안 글").await;
    sees_text(&b, space, ch, "끊긴 동안 글").await;
    let t0 = std::time::Instant::now();
    eventually("직접 연결을 다시 이음", || async {
        (!testkit::p2p_peers(&b.node).is_empty() && !testkit::p2p_peers(&a.node).is_empty())
            .then_some(())
    })
    .await;
    assert!(
        t0.elapsed() < std::time::Duration::from_secs(30),
        "다시 잇는 데 {:?}",
        t0.elapsed()
    );
    eventually(
        "B에게 보낼 조각이 다시 직접 연결로 감",
        || async {
            testkit::blobs::chunk_window(&b.node, &[a.peer()])
                .is_none()
                .then_some(())
        },
    )
    .await;
}

/// 파일 회로는 기본으로 꺼져 있고, 받는 쪽과 내주는 쪽이 모두 켜야 쓰입니다: 받는 쪽만 켜면 내주는 쪽이 파일 링크를
/// 거절해 주 연결로 받습니다. 자동화 토큰은 이 설정을 바꾸지 못합니다(이 기기의 onion 서비스가 회로를 더 만들게 되므로).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn file_circuits_need_both_sides_opted_in() {
    let cl = Cluster::new();
    let (a, b, _c, space, ch) = three_member_crew(&cl).await;
    for n in [&a, &b] {
        tor_unlimited(n).await;
        assert!(
            !n.ok::<m::NetFileCircuits>(Empty).await.enabled,
            "기본 꺼짐"
        );
    }
    let r = automation_call::<m::NetSetFileCircuits>(
        &b,
        FileCircuitsParams {
            enabled: true,
            count: None,
        },
    )
    .await;
    assert!(r.is_err(), "자동화 토큰은 못 바꿈");
    assert!(!b.ok::<m::NetFileCircuits>(Empty).await.enabled);

    // 둘 다 꺼짐: 파일 스웜도 띄우지 않습니다.
    timed_fetch(&a, &b, space, ch, 2).await;
    assert_eq!(testkit::file_links(&b.node), (0, 0, 0));
    assert_eq!(testkit::file_links(&a.node).1, 0);

    // 받는 쪽만 켬: 회로를 걸어 보지만 A가 거절합니다.
    set_file_circuits(&b, true).await;
    assert!(b.ok::<m::NetFileCircuits>(Empty).await.enabled);
    timed_fetch(&a, &b, space, ch, 3).await;
    let (ready, _, swarms) = testkit::file_links(&b.node);
    assert!(swarms >= 1, "B가 파일 회로를 걸어 봄");
    assert_eq!(ready, 0, "A가 끈 동안 이어진 파일 회로가 없어야 함");
    assert_eq!(
        testkit::file_links(&a.node).1,
        0,
        "끈 A는 파일 링크를 받지 않음"
    );
    assert_eq!(testkit::file_link_chunks_served(&a.node), 0);

    // 끄면 띄워 둔 파일 스웜도 멈춥니다.
    set_file_circuits(&b, false).await;
    assert_eq!(testkit::file_links(&b.node), (0, 0, 0));
}

/// 파일 회로 설정은 상대 하나에 더 여는 회로 수(0~4)입니다. 넘는 값은 거절하고, 예전 켜기·끄기 설정은 4개·0개로 읽으며,
/// 예전 호출(`enabled`만)도 그대로 4개·0개로 받습니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn file_circuit_count_is_bounded_and_reads_legacy_switch() {
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let get = || async { a.ok::<m::NetFileCircuits>(Empty).await };
    assert_eq!(
        (get().await.enabled, get().await.count),
        (false, 0),
        "기본 0"
    );
    a.ok::<m::NetSetFileCircuits>(FileCircuitsParams {
        enabled: false,
        count: Some(2),
    })
    .await;
    assert_eq!((get().await.enabled, get().await.count), (true, 2));
    let e = a
        .call::<m::NetSetFileCircuits>(FileCircuitsParams {
            enabled: false,
            count: Some(5),
        })
        .await
        .unwrap_err();
    assert_eq!(e.code, yangpa_rpc::jsonrpc::ERR_INVALID_INPUT);
    assert_eq!(get().await.count, 2, "거절한 값은 저장하지 않음");
    set_file_circuits(&a, true).await;
    assert_eq!(get().await.count, 4, "예전 호출로 켜면 4개");
    set_file_circuits(&a, false).await;
    assert_eq!((get().await.enabled, get().await.count), (false, 0));
    testkit::put_legacy_file_circuits(&a.node, true);
    assert_eq!(
        (get().await.enabled, get().await.count),
        (true, 4),
        "예전 켬 = 4개"
    );
    testkit::put_legacy_file_circuits(&a.node, false);
    assert_eq!(get().await.count, 0);
}

// ───────────── 익명 채널 ─────────────

async fn set_anonymous(n: &TestNode, space: SpaceId, channel: ChannelId, on: bool) {
    let mut p = ChannelUpdateParams::new(space, channel);
    p.anonymous = Some(on);
    n.ok::<m::ChannelUpdate>(p).await;
}

async fn wait_anonymous(n: &TestNode, space: SpaceId, channel: ChannelId, on: bool) {
    eventually("익명 설정이 퍼짐", || async {
        let st = testkit::space_state(&n.node, &space)?;
        (st.is_anonymous(&channel) == on).then_some(())
    })
    .await;
}

async fn wait_message(
    n: &TestNode,
    space: SpaceId,
    channel: ChannelId,
    id: EventId,
    pred: impl Fn(&yangpa_rpc::api::MessageView) -> bool,
) -> yangpa_rpc::api::MessageView {
    eventually("메시지가 보임", || async {
        n.messages(space, channel)
            .await
            .into_iter()
            .find(|m| m.id == id && pred(m))
    })
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn anonymous_channel_hides_authors_and_keeps_anonymous_posts() {
    use yangpa_rpc::api::{ANONYMOUS_DEVICE, ANONYMOUS_USER};
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let b = cl.node("b").await;
    let space = create_crew(&a, "익명 크루").await;
    assert_eq!(join(&a, &b, space).await, InviteJoinStatus::Joined);
    wait_joined(&b, space).await;
    wait_member(&b, space, a.user(), true).await;

    // 카테고리는 익명으로 만들 수 없고, 채널 관리 권한이 없으면 바꿀 수 없습니다.
    let mut cat = channel_params(space, "분류", ChannelKind::Category);
    cat.anonymous = Some(true);
    assert_eq!(
        code(a.call::<m::ChannelCreate>(cat).await),
        ERR_INVALID_INPUT
    );
    let mut p = channel_params(space, "고민", ChannelKind::Text);
    p.anonymous = Some(true);
    let channel = a.ok::<m::ChannelCreate>(p).await.channel;
    wait_anonymous(&b, space, channel, true).await;
    let mut up = ChannelUpdateParams::new(space, channel);
    up.anonymous = Some(false);
    assert_eq!(code(b.call::<m::ChannelUpdate>(up).await), ERR_FORBIDDEN);
    let view = b.ok::<m::SpaceGet>(SpaceParams { space }).await;
    assert!(
        view.channels
            .iter()
            .any(|c| c.channel == channel && c.anonymous)
    );

    // B가 쓴 글: A에게는 글쓴이·기기·알림 보낸 사람이 모두 '익명'이고, B에게는 내 글로 보입니다.
    let mut a_rx = a.subscribe();
    let secret = b.send(space, channel, "익명으로 씀").await;
    let (msg, notify) = next_notification(&mut a_rx, "A의 message.new", |n| match n {
        Notification::MessageNew {
            message, notify, ..
        } if message.id == secret => Some((message.clone(), notify.clone())),
        _ => None,
    })
    .await;
    assert!(msg.anonymous && !msg.mine && !msg.author_blocked);
    assert_eq!(
        (msg.author, msg.author_device),
        (ANONYMOUS_USER, ANONYMOUS_DEVICE)
    );
    // 알림 보낸 사람 이름은 그 채널의 가명(형용사 + 동물)으로, 글 보기의 `anon_name`과 같습니다.
    let anon_name = msg.anon_name.clone().expect("가명");
    assert!(!anon_name.is_empty() && anon_name != "익명");
    let sender = notify.sender.expect("보낸 사람");
    assert_eq!(
        (sender.user, sender.name.as_str()),
        (ANONYMOUS_USER, anon_name.as_str())
    );
    assert!(sender.avatar.is_none());
    assert!(notify.title.starts_with(&format!("{anon_name} · ")));
    let own = wait_message(&b, space, channel, secret, |_| true).await;
    assert!(own.anonymous && own.mine && own.author == ANONYMOUS_USER);
    // 서명된 헤더 종류가 익명 표시입니다.
    let ev = testkit::get_event(&a.node, &secret).unwrap().unwrap();
    assert_eq!(ev.header.kind, EventKind::AnonMessage);

    // 익명을 꺼도 그때 쓴 글은 익명으로 남고, 그 뒤 글은 이름이 보입니다.
    set_anonymous(&a, space, channel, false).await;
    wait_anonymous(&b, space, channel, false).await;
    let named = b.send(space, channel, "이름으로 씀").await;
    let shown = wait_message(&a, space, channel, named, |_| true).await;
    assert!(!shown.anonymous && shown.author == b.user());
    let kept = wait_message(&a, space, channel, secret, |_| true).await;
    assert!(kept.anonymous && kept.author == ANONYMOUS_USER);
    assert_eq!(
        testkit::get_event(&a.node, &named)
            .unwrap()
            .unwrap()
            .header
            .kind,
        EventKind::Message
    );
    // 다시 켜면 켜져 있는 동안은 지난 글도 가려집니다.
    set_anonymous(&a, space, channel, true).await;
    wait_message(&a, space, channel, named, |m| m.anonymous).await;
    let view = a.ok::<m::SpaceGet>(SpaceParams { space }).await;
    assert!(
        view.channels
            .iter()
            .any(|c| c.channel == channel && c.anonymous)
    );

    // 익명 포럼: 게시글 목록의 글쓴이도 가려집니다.
    let mut f = channel_params(space, "익명 게시판", ChannelKind::Forum);
    f.anonymous = Some(true);
    let forum = a.ok::<m::ChannelCreate>(f).await.channel;
    wait_anonymous(&b, space, forum, true).await;
    let post = b
        .ok::<m::ForumCreatePost>(ForumCreatePostParams {
            space,
            channel: forum,
            title: "익명 글".into(),
            text: "본문".into(),
            tags: vec![],
            attachments: Vec::new(),
            featured: false,
        })
        .await
        .id;
    let seen = wait_post(&a, space, forum, post, "A가 익명 게시글을 봄", |_| true).await;
    assert!(seen.anonymous && !seen.mine && seen.author == ANONYMOUS_USER);
    let own = wait_post(&b, space, forum, post, "B가 내 게시글을 봄", |_| {
        true
    })
    .await;
    assert!(own.anonymous && own.mine);
    assert_eq!(
        testkit::get_event(&a.node, &post)
            .unwrap()
            .unwrap()
            .header
            .kind,
        EventKind::AnonForumPost
    );
    set_anonymous(&a, space, forum, false).await;
    wait_anonymous(&a, space, forum, false).await;
    let seen = wait_post(&a, space, forum, post, "끈 뒤에도 익명", |_| true).await;
    assert!(seen.anonymous && seen.author == ANONYMOUS_USER);
}

// ───────────── 크루 STUN ─────────────

/// 크루 STUN 서버는 관리자가 정하고, `use_crew_stun` 역할이 있고 스스로 쓰기로 한 멤버만 씁니다. 목록은 관리자에게만
/// 보입니다. TURN 줄은 받지 않습니다(2026-11-07 TURN 삭제).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crew_stun_servers_need_role_and_opt_in() {
    let cl = Cluster::new();
    let (a, b, _c, space, _) = three_member_crew(&cl).await;

    // 관리자만 정하고, 잘못된 줄은 받지 않습니다.
    let denied = b
        .call::<m::CrewSetServers>(CrewServersParams {
            space,
            stun: vec!["stun:127.0.0.1:3478".into()],
        })
        .await;
    assert_eq!(code(denied), ERR_FORBIDDEN);
    let bad = a
        .call::<m::CrewSetServers>(CrewServersParams {
            space,
            stun: vec!["turn:u:p@x.example.org".into()],
        })
        .await;
    assert_eq!(code(bad), ERR_INVALID_INPUT);
    let v = a.ok::<m::SpaceGet>(SpaceParams { space }).await;
    assert_eq!(
        v.crew_servers,
        Some(Default::default()),
        "관리자에게는 빈 목록도 보임"
    );
    a.ok::<m::CrewSetServers>(CrewServersParams {
        space,
        stun: vec!["stun:127.0.0.1:3478".into()],
    })
    .await;
    eventually("B가 크루 서버 목록을 받음", || async {
        let st = testkit::space_state(&b.node, &space)?;
        (st.crew_servers.stun == vec!["127.0.0.1:3478".to_string()]
            && st.crew_servers.turn.is_empty())
        .then_some(())
    })
    .await;
    let v = b.ok::<m::SpaceGet>(SpaceParams { space }).await;
    assert!(v.crew_servers.is_none(), "관리자가 아니면 목록을 주지 않음");
    assert!(!v.crew_stun, "역할 권한이 없음");

    b.ok::<m::CrewUseServers>(CrewUseServersParams {
        space,
        enabled: true,
    })
    .await;
    assert!(
        b.ok::<m::SpaceGet>(SpaceParams { space })
            .await
            .use_crew_servers
    );
    let role = create_role(&a, space, "STUN", &["use_crew_stun"], 1).await;
    set_roles(&a, space, b.user(), vec![role]).await;
    eventually("B가 크루 STUN을 쓸 수 있음", || async {
        b.ok::<m::SpaceGet>(SpaceParams { space })
            .await
            .crew_stun
            .then_some(())
    })
    .await;
    b.ok::<m::CrewUseServers>(CrewUseServersParams {
        space,
        enabled: false,
    })
    .await;
    assert!(
        !b.ok::<m::SpaceGet>(SpaceParams { space })
            .await
            .use_crew_servers
    );
}

/// 채널 첨부 한도: 기본 무제한이고, 크루 관리자가 채널마다 정하며(1MiB 이상), 넘는 첨부는 파일을 읽기 전에 거절합니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn channel_attach_limit_is_per_channel_and_unlimited_by_default() {
    const MIB: u64 = 1024 * 1024;
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let space = create_crew(&a, "한도").await;
    let channel = a.first_channel(space).await;
    let other = a
        .ok::<m::ChannelCreate>(channel_params(space, "자료", ChannelKind::Text))
        .await
        .channel;
    let category = a
        .ok::<m::ChannelCreate>(channel_params(space, "묶음", ChannelKind::Category))
        .await
        .channel;
    let big = cl.dir("a").join("big.bin");
    std::fs::write(&big, vec![7u8; 2 * MIB as usize]).unwrap();
    let send = |ch: ChannelId| MessageSendParams {
        attachments: vec![AttachmentInput {
            path: big.to_string_lossy().into_owned(),
            ..Default::default()
        }],
        ..send_params(space, ch, "첨부", None)
    };
    let limit = |ch: ChannelId, l: Option<u64>| ChannelUpdateParams {
        attach_limit: Some(l),
        ..ChannelUpdateParams::new(space, ch)
    };
    let limit_of = |ch: ChannelId| {
        let a = &a;
        async move {
            a.ok::<m::ChannelList>(SpaceParams { space })
                .await
                .into_iter()
                .find(|v| v.channel == ch)
                .unwrap()
                .attach_limit
        }
    };

    // 기본은 무제한(예전 기본 한도 100MiB와 무관하게 보기에도 한도가 없음).
    assert_eq!(limit_of(channel).await, None);
    a.ok::<m::MessageSend>(send(channel)).await;

    assert_eq!(
        code(
            a.call::<m::ChannelUpdate>(limit(channel, Some(MIB - 1)))
                .await
        ),
        ERR_INVALID_INPUT,
        "1MiB보다 작은 한도"
    );
    assert_eq!(
        code(a.call::<m::ChannelUpdate>(limit(category, Some(MIB))).await),
        ERR_INVALID_INPUT,
        "카테고리"
    );
    a.ok::<m::ChannelUpdate>(limit(channel, Some(MIB))).await;
    assert_eq!(limit_of(channel).await, Some(MIB));
    let e = a.call::<m::MessageSend>(send(channel)).await.unwrap_err();
    assert_eq!(e.code, ERR_INVALID_INPUT);
    assert!(e.message.contains("1MiB"), "{}", e.message);
    // 다른 채널은 그대로 무제한입니다.
    assert_eq!(limit_of(other).await, None);
    a.ok::<m::MessageSend>(send(other)).await;

    // 0은 첨부 금지입니다(크기와 상관없이).
    a.ok::<m::ChannelUpdate>(limit(channel, Some(0))).await;
    assert_eq!(limit_of(channel).await, Some(0));
    let e = a.call::<m::MessageSend>(send(channel)).await.unwrap_err();
    assert_eq!(e.code, ERR_INVALID_INPUT);
    assert!(e.message.contains("첨부를 받지 않습니다"), "{}", e.message);
    a.send(space, channel, "글은 됨").await;

    // 무제한으로 되돌립니다.
    a.ok::<m::ChannelUpdate>(limit(channel, None)).await;
    assert_eq!(limit_of(channel).await, None);
    a.ok::<m::MessageSend>(send(channel)).await;

    // 크루의 휘발성 채널은 따로 정하지 않으면 10MiB, 만들 때 정할 수도 있습니다.
    let mut p = channel_params(space, "잠깐", ChannelKind::Text);
    p.security = Some(SecurityLevel::Volatile);
    let vol = a.ok::<m::ChannelCreate>(p.clone()).await.channel;
    assert_eq!(limit_of(vol).await, Some(10 * MIB));
    p.name = "잠깐2".into();
    p.attach_limit = Some(Some(2 * MIB));
    let vol2 = a.ok::<m::ChannelCreate>(p.clone()).await.channel;
    assert_eq!(limit_of(vol2).await, Some(2 * MIB));
    p.name = "잠깐3".into();
    p.attach_limit = Some(None);
    let vol3 = a.ok::<m::ChannelCreate>(p).await.channel;
    assert_eq!(limit_of(vol3).await, None, "null이면 무제한");
}

/// 채널 수정·삭제 금지: 켜면 작성자는 고치거나 지울 수 없고(보내는 쪽에서 거절), 운영 삭제는 됩니다. 끄면 다시 됩니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn channel_edit_rules_block_edit_and_own_delete() {
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let b = cl.node("b").await;
    let space = create_crew(&a, "규칙").await;
    assert_eq!(join(&a, &b, space).await, InviteJoinStatus::Joined);
    wait_joined(&b, space).await;
    wait_member(&b, space, a.user(), true).await;
    let channel = a.first_channel(space).await;
    let mine = b.send(space, channel, "남길 말").await;
    sees_text(&a, space, channel, "남길 말").await;

    let rules = |no_edit: bool, no_delete: bool| ChannelUpdateParams {
        no_edit: Some(no_edit),
        no_delete: Some(no_delete),
        ..ChannelUpdateParams::new(space, channel)
    };
    let seen_by_b = |no_edit: bool, no_delete: bool| {
        let b = &b;
        async move {
            eventually("B가 바뀐 수정·삭제 금지를 봄", || async {
                let v = b
                    .ok::<m::ChannelList>(SpaceParams { space })
                    .await
                    .into_iter()
                    .find(|v| v.channel == channel)?;
                (v.no_edit == no_edit && v.no_delete == no_delete).then_some(())
            })
            .await
        }
    };
    let edit = MessageEditParams {
        space,
        channel,
        id: mine,
        text: "고친 말".into(),
    };
    let del = MessageRefParams {
        space,
        channel,
        id: mine,
    };
    assert_eq!(
        code(b.call::<m::ChannelUpdate>(rules(true, true)).await),
        ERR_FORBIDDEN,
        "채널 관리 권한 없음"
    );
    a.ok::<m::ChannelUpdate>(rules(true, true)).await;
    seen_by_b(true, true).await;
    assert_eq!(
        code(b.call::<m::MessageEdit>(edit.clone()).await),
        ERR_FORBIDDEN
    );
    assert_eq!(
        code(b.call::<m::MessageDelete>(del.clone()).await),
        ERR_FORBIDDEN
    );

    a.ok::<m::ChannelUpdate>(rules(false, true)).await;
    seen_by_b(false, true).await;
    b.ok::<m::MessageEdit>(edit).await;
    sees_text(&a, space, channel, "고친 말").await;
    // 삭제 금지여도 운영 삭제는 됩니다.
    a.ok::<m::MessageDelete>(del).await;
}

/// 글 쓰는 중 알림은 그 채널의 연결된 멤버에게 가고(익명 채널이면 가명으로), 작성자 검색(`message.search`의 `author`)은
/// 검색어 없이도 그 사람 글만 찾되 익명 글은 빼서 누가 썼는지 드러내지 않습니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn typing_reaches_members_and_author_search_skips_anonymous() {
    use yangpa_rpc::api::{ANONYMOUS_USER, MessageSearchParams};
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let b = cl.node("b").await;
    let space = create_crew(&a, "검색").await;
    assert_eq!(join(&a, &b, space).await, InviteJoinStatus::Joined);
    wait_joined(&b, space).await;
    wait_member(&b, space, a.user(), true).await;
    let channel = a.first_channel(space).await;
    let mut p = channel_params(space, "고민", ChannelKind::Text);
    p.anonymous = Some(true);
    let secret_ch = a.ok::<m::ChannelCreate>(p).await.channel;
    wait_anonymous(&b, space, secret_ch, true).await;
    // 서로 받는 것을 확인해 연결이 이어진 뒤에 알립니다(글 쓰는 중은 저장하지 않는 일회성 알림).
    a.send(space, channel, "회의 8시").await;
    let mine = b.send(space, channel, "회의 9시").await;
    b.send(space, secret_ch, "회의 익명").await;
    sees_text(&b, space, channel, "회의 8시").await;
    sees_text(&a, space, channel, "회의 9시").await;
    sees_text(&a, space, secret_ch, "회의 익명").await;

    let mut rx = a.subscribe();
    b.ok::<m::ChannelTyping>(SpaceChannelParams { space, channel })
        .await;
    let got = next_notification(&mut rx, "A가 B의 입력 중 알림을 받음", |n| match n {
        Notification::Typing {
            space: s,
            channel: c,
            user,
            anon_name,
        } if *s == space && *c == channel => Some((*user, anon_name.clone())),
        _ => None,
    })
    .await;
    assert_eq!(got, (b.user(), None));
    b.ok::<m::ChannelTyping>(SpaceChannelParams {
        space,
        channel: secret_ch,
    })
    .await;
    let (user, anon) = next_notification(&mut rx, "익명 채널의 입력 중 알림", |n| match n
    {
        Notification::Typing {
            channel: c,
            user,
            anon_name,
            ..
        } if *c == secret_ch => Some((*user, anon_name.clone())),
        _ => None,
    })
    .await;
    assert_eq!(user, ANONYMOUS_USER);
    assert!(anon.is_some_and(|n| !n.is_empty() && n != "익명"));

    let search = |author: Option<UserId>, query: &str| MessageSearchParams {
        space: Some(space),
        channel: None,
        author,
        query: query.into(),
        offset: None,
        limit: None,
    };
    let found = a.ok::<m::MessageSearch>(search(Some(b.user()), "")).await;
    assert_eq!(
        found.messages.iter().map(|v| v.id).collect::<Vec<_>>(),
        vec![mine],
        "B의 익명 글은 빠짐"
    );
    assert_eq!(found.total, 1, "전체 수에서도 빠짐");
    let all = a.ok::<m::MessageSearch>(search(None, "회의")).await;
    assert_eq!((all.messages.len(), all.total), (3, 3));
    // 쪽 나누기: 2개씩이면 둘째 쪽은 남은 하나, 끝을 넘으면 마지막 쪽.
    let page2 = a
        .ok::<m::MessageSearch>(MessageSearchParams {
            offset: Some(2),
            limit: Some(2),
            ..search(None, "회의")
        })
        .await;
    assert_eq!((page2.total, page2.offset), (3, 2));
    assert_eq!(
        page2.messages.iter().map(|v| v.id).collect::<Vec<_>>(),
        [all.messages[2].id]
    );
    let beyond = a
        .ok::<m::MessageSearch>(MessageSearchParams {
            offset: Some(99),
            limit: Some(2),
            ..search(None, "회의")
        })
        .await;
    assert_eq!(beyond.offset, 2);
    let empty = a.ok::<m::MessageSearch>(search(None, "")).await;
    assert!(
        empty.messages.is_empty() && empty.total == 0,
        "작성자 없이 빈 검색어는 아무것도 찾지 않음"
    );
}

// ───────────── 받은편지함(가입자 키 묶음) ─────────────

/// 가입자에게 봉인한 키 묶음은 가입자의 받은편지함 스트림에만 들어가, 다른 멤버는 gossip·직접 전송·따라잡기 어느
/// 쪽으로도 한 바이트도 받지 않습니다. 가입자는 확정 기기가 아닌 멤버의 링크로 들어와도(받아 준 기기가 넘겨줌) 그 키로
/// 지난 기록을 읽고, 시더는 남의 받은편지함을 모르는 스트림과 똑같이 답합니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn join_keys_reach_only_the_joiner_inbox() {
    let cl = Cluster::new();
    let (a, b, c, space, _) = three_member_crew(&cl).await;
    let mut p = channel_params(space, "기록방", ChannelKind::Text);
    p.history = Some(HistoryVisibility::Public);
    let hist = a.ok::<m::ChannelCreate>(p).await.channel;
    for n in [&b, &c] {
        wait_channel(n, space, hist).await;
    }
    a.send(space, hist, "가입 전 기록").await;
    // 연 채널만 시드하므로 B·C도 열어 둡니다(가입자는 B에게서 지난 기록을 받음).
    for n in [&b, &c] {
        sees_text(n, space, hist, "가입 전 기록").await;
    }

    // D는 확정 기기(A)가 아닌 B의 링크로 들어옵니다. A와는 연결하지 않아, A가 봉인한 묶음은 B가 받아 넘겨줍니다.
    let inviter = create_role(&a, space, "초대 담당", &["create_invite"], 1).await;
    set_roles(&a, space, b.user(), vec![inviter]).await;
    wait_roles(&b, space, b.user(), &[inviter]).await;
    let d = cl.node("d").await;
    join(&b, &d, space).await;
    wait_joined(&d, space).await;
    sees_text(&d, space, hist, "가입 전 기록").await;
    let inbox = StreamId::Inbox(d.user());
    let ids: Vec<EventId> = testkit::stream_events(&a.node, &space, inbox)
        .unwrap()
        .iter()
        .map(|e| e.id())
        .collect();
    assert!(
        !ids.is_empty(),
        "확정 기기가 가입자 받은편지함에 키를 봉인함"
    );
    for n in [&b, &d] {
        let got = testkit::stream_events(&n.node, &space, inbox).unwrap();
        assert!(!got.is_empty(), "{}에 가입자 받은편지함이 없음", n.name);
    }

    // C가 A·B·D와 직접 연결해 따라잡은 뒤에도 D의 받은편지함은 한 바이트도 받지 않았습니다.
    connect_both(&a, &d).await;
    connect_both(&c, &d).await;
    meet(&a, &d, space).await;
    meet(&c, &d, space).await;
    for peer in [a.peer(), b.peer(), d.peer()] {
        testkit::spawn_catch_up(&c.node, space, peer);
    }
    c.send(space, hist, "C도 여기").await;
    sees_text(&d, space, hist, "C도 여기").await;
    eventually("C의 따라잡기가 끝남", || async {
        testkit::catch_up_idle(&c.node).then_some(())
    })
    .await;
    assert_eq!(testkit::foreign_scoped_bytes(&c.node), 0);
    assert!(
        testkit::stream_events(&c.node, &space, inbox)
            .unwrap()
            .is_empty(),
        "C가 D의 받은편지함을 가짐"
    );
    // 공간 스트림 이벤트는 어느 노드에서도 받은편지함 이벤트를 부모로 두지 않습니다(그러면 순서가 노드마다 갈림).
    for n in [&a, &b, &c, &d] {
        for e in testkit::stream_events(&n.node, &space, StreamId::Space).unwrap() {
            assert!(
                e.header.parents.iter().all(|p| !ids.contains(p)),
                "{}의 공간 스트림 이벤트가 받은편지함 이벤트를 부모로 둠",
                n.name
            );
        }
    }

    // 시더: 자격 없는 C에게는 모르는 사용자의 받은편지함과 똑같은 빈 답, 가입자 D에게는 내용.
    let nobody = StreamId::Inbox(UserId::random());
    let range = LamportRange {
        start: 0,
        end: 1 << 40,
    };
    let probes = |stream: StreamId, ids: Vec<EventId>| {
        vec![
            YpRequest::GetHeads { space, stream },
            YpRequest::GetFingerprints {
                space,
                stream,
                ranges: vec![range],
            },
            YpRequest::GetIds {
                space,
                stream,
                range,
                limit: 512,
            },
            YpRequest::GetEvents { space, ids },
        ]
    };
    let fake: Vec<EventId> = ids.iter().map(|_| EventId::random()).collect();
    for seeder in [&a, &b] {
        for (asked, unknown) in probes(inbox, ids.clone())
            .into_iter()
            .zip(probes(nobody, fake.clone()))
        {
            let got = testkit::net_request(&c.node, seeder.peer(), asked)
                .await
                .unwrap();
            let empty = testkit::net_request(&c.node, seeder.peer(), unknown)
                .await
                .unwrap();
            assert_eq!(
                got, empty,
                "{}가 C에게 D의 받은편지함을 드러냄",
                seeder.name
            );
        }
    }
    assert_eq!(testkit::foreign_scoped_bytes(&c.node), 0);
    let mine = testkit::net_request(&d.node, b.peer(), YpRequest::GetEvents { space, ids })
        .await
        .unwrap();
    match mine {
        YpResponse::Events(evs) => assert!(!evs.is_empty(), "가입자가 자기 받은편지함을 못 받음"),
        other => panic!("가입자 요청이 거절됨: {other:?}"),
    }
}

/// 키 그룹 스트림: 확정 기기가 비공개 채널의 키 그룹에 낸 묶음은 그 채널을 볼 수 있는 기기만 받고(밀기·따라잡기 모두),
/// 시더는 볼 수 없는 멤버에게 모르는 그룹과 똑같이 답합니다. 역할을 빼면 그 멤버에게도 모르는 그룹과 같은 답이 됩니다.
/// 모든 멤버가 보는 채널은 크루 그룹(공간 스트림)을 써서 비공개 그룹이 없습니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn key_group_stream_reaches_only_viewers() {
    let cl = Cluster::new();
    let (a, b, c, space, general) = three_member_crew(&cl).await;
    assert_eq!(testkit::channel_key_group(&a.node, space, general), None);
    let role = create_role(&a, space, "비밀", &[], 5).await;
    let mut p = channel_params(space, "비밀방", ChannelKind::Text);
    p.private_roles = Some(vec![role]);
    let secret = a.ok::<m::ChannelCreate>(p).await.channel;
    set_roles(&a, space, b.user(), vec![role]).await;
    for n in [&b, &c] {
        wait_channel(n, space, secret).await;
        wait_roles(n, space, b.user(), &[role]).await;
    }
    let group = testkit::channel_key_group(&a.node, space, secret).expect("비공개 그룹");
    assert_eq!(
        testkit::committer_of(&a.node, &space),
        testkit::device_id(&a.node)
    );
    let ev = testkit::publish_key_group_share(&a.node, space, group)
        .await
        .expect("키 그룹 묶음");
    let stream = StreamId::KeyGroup(group);
    let has = |n: &TestNode| {
        testkit::stream_events(&n.node, &space, stream)
            .unwrap()
            .iter()
            .any(|e| e.id() == ev.id())
    };
    eventually("B가 키 그룹 묶음을 받음", || async {
        has(&b).then_some(())
    })
    .await;

    // C는 볼 수 없습니다: A·B와 따라잡아도 그 스트림을 받지 않습니다.
    for peer in [a.peer(), b.peer()] {
        testkit::spawn_catch_up(&c.node, space, peer);
    }
    c.send(space, general, "C의 따라잡기 표시").await;
    eventually("C의 따라잡기가 끝남", || async {
        testkit::catch_up_idle(&c.node).then_some(())
    })
    .await;
    assert!(!has(&c), "볼 수 없는 C가 키 그룹 묶음을 받음");
    assert_eq!(testkit::foreign_scoped_bytes(&c.node), 0);

    // 시더: 자격 없는 쪽에게는 모르는 그룹과 똑같은 답.
    let nobody = StreamId::KeyGroup(yangpa_core::ids::KeyGroupId::random());
    let range = LamportRange {
        start: 0,
        end: 1 << 40,
    };
    let probes = |stream: StreamId, ids: Vec<EventId>| {
        vec![
            YpRequest::GetHeads { space, stream },
            YpRequest::GetFingerprints {
                space,
                stream,
                ranges: vec![range],
            },
            YpRequest::GetIds {
                space,
                stream,
                range,
                limit: 512,
            },
            YpRequest::GetEvents { space, ids },
        ]
    };
    let hidden_from = |asker: &TestNode, seeder: &TestNode| {
        let (asker, seeder) = (asker.node.clone(), seeder.peer());
        let ids = vec![ev.id()];
        async move {
            for (asked, unknown) in probes(stream, ids)
                .into_iter()
                .zip(probes(nobody, vec![EventId::random()]))
            {
                let got = testkit::net_request(&asker, seeder, asked).await.ok()?;
                let empty = testkit::net_request(&asker, seeder, unknown).await.ok()?;
                if got != empty {
                    return None;
                }
            }
            Some(())
        }
    };
    for seeder in [&a, &b] {
        assert!(
            hidden_from(&c, seeder).await.is_some(),
            "{}가 C에게 키 그룹을 드러냄",
            seeder.name
        );
    }
    let mine = testkit::net_request(
        &b.node,
        a.peer(),
        YpRequest::GetEvents {
            space,
            ids: vec![ev.id()],
        },
    )
    .await
    .unwrap();
    match mine {
        YpResponse::Events(evs) => assert_eq!(evs.len(), 1, "볼 수 있는 B가 못 받음"),
        other => panic!("B의 요청이 거절됨: {other:?}"),
    }

    // 역할을 빼면 B에게도 모르는 그룹과 같은 답(머리도 비움).
    set_roles(&a, space, b.user(), Vec::new()).await;
    eventually("A가 역할을 잃은 B에게 키 그룹을 숨김", || async {
        hidden_from(&b, &a).await
    })
    .await;
}

// ───────────── 받은편지함 권한·전달자 ─────────────

fn rcpt_of(
    n: &TestNode,
) -> Vec<(
    yangpa_core::ids::DeviceId,
    yangpa_core::device::BoxPublicKey,
)> {
    vec![(
        testkit::device_id(&n.node).unwrap(),
        testkit::box_public(&n.node).unwrap(),
    )]
}

fn nick(n: &TestNode, space: SpaceId, user: UserId) -> Option<String> {
    testkit::space_state(&n.node, &space)?
        .members
        .get(&user)?
        .nickname
        .clone()
}

/// 키를 나눌 자격이 없는 멤버 C가 B의 받은편지함에 지어낸 상태 키를 보내도 B는 그 키를 열어 쓰지 않습니다. 열어 쓰면 B만
/// 그 키로 쓴 C의 상태 이벤트를 적용해 B의 상태가 다른 멤버와 갈립니다(기준점을 아무도 모르게 적어 확인을 미뤄도).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unauthorized_inbox_share_does_not_split_state() {
    use yangpa_core::body::{Secret32, SharedKey, state_key_id};
    let cl = Cluster::new();
    let (a, b, c, space, _) = three_member_crew(&cl).await;
    let k = Secret32::random();
    let body = EventBody::MemberProfile {
        nickname: Some("갈라짐".into()),
        profile: None,
    };
    let hidden = testkit::author_state_event_with(&c.node, space, &body, k.clone()).unwrap();
    for n in [&a, &b] {
        testkit::ingest_live_from(&n.node, space, vec![hidden.clone()], c.peer())
            .await
            .unwrap();
    }
    let unknown = EventId::random();
    let share = testkit::author_inbox_share(
        &c.node,
        space,
        b.user(),
        rcpt_of(&b),
        vec![SharedKey::State {
            id: state_key_id(&space, &k),
            key: k,
        }],
        &[unknown],
    )
    .unwrap();
    let r = testkit::ingest_live_from(&b.node, space, vec![share.clone()], c.peer())
        .await
        .unwrap();
    assert!(
        r.is_rejected(&share.id()),
        "자격 없는 묶음을 거부해야 함: {r:?}"
    );
    assert_ne!(nick(&b, space, c.user()).as_deref(), Some("갈라짐"));
    assert!(
        testkit::stream_events(&b.node, &space, StreamId::Inbox(b.user()))
            .unwrap()
            .iter()
            .all(|e| e.id() != share.id())
    );
    let sa = testkit::space_state(&a.node, &space).unwrap();
    let sb = testkit::space_state(&b.node, &space).unwrap();
    assert_eq!(*sa, *sb, "A와 B의 상태가 갈림");
}

/// 관리자였다가 강등된 C가 강등 전 기준점을 적어 B의 받은편지함에 상태 키를 보내도 B는 받지 않습니다(기준점 상태만
/// 보면 그때의 권한이 되살아남). 같은 묶음을 다시 받아도 기준점 상태를 다시 세지 않고 거부합니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn demoted_admin_cannot_backdate_an_inbox_anchor() {
    use yangpa_core::body::{Secret32, SharedKey, state_key_id};
    let cl = Cluster::new();
    let (a, b, c, space, _) = three_member_crew(&cl).await;
    let adm = create_role(&a, space, "관리", &["administrator"], 1).await;
    set_roles(&a, space, c.user(), vec![adm]).await;
    for n in [&b, &c] {
        wait_roles(n, space, c.user(), &[adm]).await;
    }
    let promoted = testkit::stream_events(&b.node, &space, StreamId::Space)
        .unwrap()
        .into_iter()
        .max_by_key(|e| e.header.lamport)
        .unwrap()
        .id();
    set_roles(&a, space, c.user(), vec![]).await;
    for n in [&b, &c] {
        wait_roles(n, space, c.user(), &[]).await;
    }
    let k = Secret32::random();
    let back = testkit::author_inbox_share(
        &c.node,
        space,
        b.user(),
        rcpt_of(&b),
        vec![SharedKey::State {
            id: state_key_id(&space, &k),
            key: k,
        }],
        &[promoted; 4],
    )
    .unwrap();
    assert_eq!(back.header.state_anchor[0], promoted);
    for _ in 0..2 {
        let r = testkit::ingest_live_from(&b.node, space, vec![back.clone()], c.peer())
            .await
            .unwrap();
        assert!(
            r.is_rejected(&back.id()) || r.duplicate == 1,
            "강등된 관리자의 소급 묶음: {r:?}"
        );
    }
    assert!(
        testkit::stream_events(&b.node, &space, StreamId::Inbox(b.user()))
            .unwrap()
            .iter()
            .all(|e| e.id() != back.id()),
        "강등된 관리자의 소급 기준점 묶음을 B가 받아들임"
    );
}

/// 권한 없는 멤버 C가 D의 가입을 흉내 낸 `MemberJoin`(해소기가 거부)을 내도 C는 D의 받은편지함 전달자가 되지 않습니다.
/// 전달자는 해소기가 적용한 가입(받아 준 B의 것)에서만 정합니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rejected_member_join_does_not_make_a_relay() {
    use yangpa_core::model::Member;
    let cl = Cluster::new();
    let (a, b, c, space, _) = three_member_crew(&cl).await;
    let inviter = create_role(&a, space, "초대 담당", &["create_invite"], 1).await;
    set_roles(&a, space, b.user(), vec![inviter]).await;
    wait_roles(&b, space, b.user(), &[inviter]).await;
    let d = cl.node("d").await;
    testkit::hold_commits(&a.node, true);
    connect_both(&b, &d).await;
    let url = invite(&b, space, None, None).await;
    let dn = d.node.clone();
    let joining =
        tokio::spawn(async move { common::call::<m::InviteJoin>(&dn, UrlParams { url }).await });
    for n in [&a, &b, &c] {
        wait_member(n, space, d.user(), true).await;
    }
    let devices = testkit::signed_device_list(&a.node, &d.user()).expect("D의 기기 목록");
    let fake = testkit::author_state_event(
        &c.node,
        space,
        None,
        &EventBody::MemberJoin {
            member: Member {
                user: d.user(),
                roles: Vec::new(),
                nickname: None,
                automation: false,
            },
            devices,
            profile: None,
            invite: None,
        },
    )
    .unwrap();
    for n in [&a, &b, &c] {
        testkit::ingest_live_from(&n.node, space, vec![fake.clone()], c.peer())
            .await
            .unwrap();
    }
    testkit::hold_commits(&a.node, false);
    wait_joined(&d, space).await;
    let inbox = StreamId::Inbox(d.user());
    let sent = testkit::stream_events(&a.node, &space, inbox).unwrap();
    assert!(!sent.is_empty(), "A가 D의 받은편지함 묶음을 냄");
    eventually(
        "B가 전달자로서 D의 받은편지함을 가짐",
        || async {
            (testkit::stream_events(&b.node, &space, inbox).ok()?.len() >= sent.len()).then_some(())
        },
    )
    .await;
    assert!(
        testkit::stream_events(&c.node, &space, inbox)
            .unwrap()
            .is_empty(),
        "거부된 가입을 낸 C가 D의 받은편지함을 가짐"
    );
    assert_eq!(testkit::foreign_scoped_bytes(&c.node), 0);
    let _ = joining.await;
}

/// 멤버 하나가 본문 없는 상태 이벤트(헤더만)를 내도 그 뒤 가입자의 키 묶음 확인이 늦어지지 않습니다(그 이벤트는 풀릴
/// 일이 없어 기준점 상태가 기다리지 않음).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn header_only_state_event_does_not_hold_join_keys() {
    let cl = Cluster::new();
    let (a, b, c, space, _) = three_member_crew(&cl).await;
    let mut p = channel_params(space, "기록방", ChannelKind::Text);
    p.history = Some(HistoryVisibility::Public);
    let hist = a.ok::<m::ChannelCreate>(p).await.channel;
    for n in [&b, &c] {
        wait_channel(n, space, hist).await;
    }
    a.send(space, hist, "가입 전 기록").await;
    for n in [&b, &c] {
        sees_text(n, space, hist, "가입 전 기록").await;
    }
    let evs = testkit::header_only_state_events(&c.node, space, 1).unwrap();
    for n in [&a, &b, &c] {
        testkit::ingest_live_from(&n.node, space, evs.clone(), c.peer())
            .await
            .unwrap();
    }
    let d = cl.node("d").await;
    let started = std::time::Instant::now();
    join(&a, &d, space).await;
    wait_joined(&d, space).await;
    sees_text(&d, space, hist, "가입 전 기록").await;
    // 예전에는 빈 구간 유예(120초)까지 기다렸습니다.
    assert!(
        started.elapsed() < std::time::Duration::from_secs(60),
        "가입자가 지난 기록을 읽기까지 {:?}",
        started.elapsed()
    );
}

/// 가입자를 받아 준 B가 확정 기기 C의 키 묶음이 나갈 때 꺼져 있었어도, 다시 켜진 B가 확정 기기와 따라잡으며 전달자로서
/// 가입자의 받은편지함을 당겨 와 넘겨줍니다. 가입자는 상태를 세우기 전에는 B와 공간을 만든 A만 믿는데 A는 그 묶음을
/// 갖지 않으므로, 이것이 없으면 가입이 끝나지 않습니다. B는 다시 띄울 때 적용된 가입 이벤트에서 전달자임을 다시 셉니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn joiner_gets_keys_after_gateway_missed_the_bundle() {
    use yangpa_rpc::api::CommitterSetParams;
    let cl = Cluster::new();
    let (a, mut b, c, space, _) = three_member_crew(&cl).await;
    let adm = create_role(&a, space, "관리", &["administrator"], 1).await;
    let inviter = create_role(&a, space, "초대 담당", &["create_invite"], 2).await;
    set_roles(&a, space, c.user(), vec![adm]).await;
    set_roles(&a, space, b.user(), vec![inviter]).await;
    wait_roles(&c, space, c.user(), &[adm]).await;
    wait_roles(&b, space, b.user(), &[inviter]).await;
    let dc = testkit::device_id(&c.node).unwrap();
    a.ok::<m::CrewSetCommitter>(CommitterSetParams { space, device: dc })
        .await;
    for n in [&a, &b, &c] {
        eventually(&format!("{}가 본 확정 기기가 C", n.name), || async {
            (testkit::committer_of(&n.node, &space) == Some(dc)).then_some(())
        })
        .await;
    }
    wait_same_crew_key(&[&a, &b, &c], space).await;

    testkit::hold_commits(&c.node, true);
    let d = cl.node("d").await;
    connect_both(&b, &d).await;
    let url = invite(&b, space, None, None).await;
    // 가입 요청의 응답을 받은 뒤에 B를 끕니다. 응답 전에 끄면 D는 네트워크 오류로 가입 대기를 지우고
    // (다시 시도하라는 오류) 돌아온 B의 인증도 받지 않으므로, 이 시험이 보려는 경로가 아닙니다.
    let r = d.ok::<m::InviteJoin>(UrlParams { url }).await;
    assert_eq!(
        r.status,
        InviteJoinStatus::Pending,
        "확정 기기가 커밋을 붙잡고 있음"
    );
    for n in [&a, &b, &c] {
        wait_member(n, space, d.user(), true).await;
    }
    // B가 꺼진 채 확정 기기가 묶음을 냅니다(B에게 보내지 못함).
    b.shutdown().await;
    testkit::hold_commits(&c.node, false);
    let inbox = StreamId::Inbox(d.user());
    eventually("C가 D의 받은편지함 묶음을 냄", || async {
        (!testkit::stream_events(&c.node, &space, inbox)
            .ok()?
            .is_empty())
        .then_some(())
    })
    .await;
    assert!(
        testkit::stream_events(&a.node, &space, inbox)
            .unwrap()
            .is_empty(),
        "전달자가 아닌 A는 D의 받은편지함을 갖지 않음"
    );
    b.start_again().await;
    connect_both(&b, &c).await;
    connect_both(&b, &d).await;
    wait_joined(&d, space).await;
}

/// 원년멤버 증서를 서명하는 개발자 계정(이 시험만 정함). `NodeConfig`를 바꾸는 함수는 값을 잡을 수 없어 여기에 둡니다.
static DEVELOPER: std::sync::OnceLock<UserId> = std::sync::OnceLock::new();

fn with_developer(c: &mut yangpa_daemon::NodeConfig) {
    c.developer = DEVELOPER.get().copied();
}

async fn badges_of(n: &TestNode, user: UserId) -> yangpa_rpc::api::BadgeView {
    n.ok::<m::BadgeGet>(UserParams { user }).await
}

/// 둘뿐인 크루에서 바꾼 프로필·뱃지와 크루를 만든 사람의 프로필이, 그 사람과 직접 이어지지 않은 셋째 멤버에게도 닿습니다.
#[tokio::test]
async fn profile_and_badges_set_with_two_members_reach_the_third() {
    use yangpa_core::badge::Badge;
    use yangpa_rpc::api::{
        BadgeSetShownParams, ProfileGetParams, ProfileSetParams, ShownBadgeView,
    };

    let cl = Cluster::new();
    let a = cl.node("a").await;
    let b = cl.node("b").await;
    let c = cl.node("c").await;
    let space = create_crew(&a, "둘 크루").await;
    assert_eq!(join(&a, &b, space).await, InviteJoinStatus::Joined);
    wait_joined(&b, space).await;

    let shown = ShownBadgeView {
        badge: Badge::Support3,
        after: false,
    };
    testkit::see_support(&a.node, 5000);
    a.ok::<m::ProfileSet>(ProfileSetParams {
        name: Some("만든 사람".into()),
        ..Default::default()
    })
    .await;
    a.ok::<m::BadgeSetShown>(BadgeSetShownParams { shown: vec![shown] })
        .await;

    let inviter = create_role(&a, space, "초대 담당", &["create_invite"], 1).await;
    set_roles(&a, space, b.user(), vec![inviter]).await;
    wait_roles(&b, space, b.user(), &[inviter]).await;
    assert_eq!(join(&b, &c, space).await, InviteJoinStatus::Joined);
    wait_joined(&c, space).await;
    let who = a.user();
    // 시험 망에서는 얼마 뒤 A와 C가 직접 이어져 프로필을 주고받으므로, 이벤트로 실렸는지를 봅니다.
    let carried = |kind| {
        testkit::stream_events(&c.node, &space, StreamId::Space)
            .expect("이벤트")
            .iter()
            .any(|e| e.header.kind == kind && e.header.user == who)
    };
    eventually(
        "셋째 멤버가 만든 사람의 프로필·뱃지 이벤트를 받음",
        || async {
            (carried(EventKind::MemberProfile) && carried(EventKind::MemberBadges)).then_some(())
        },
    )
    .await;
    eventually(
        "셋째 멤버가 만든 사람의 프로필·뱃지를 봄",
        || async {
            let name = c
                .call::<m::ProfileGet>(ProfileGetParams { user: Some(who) })
                .await
                .ok()
                .map(|p| p.name);
            let v = badges_of(&c, who).await;
            (name.as_deref() == Some("만든 사람") && v.shown == vec![shown]).then_some(())
        },
    )
    .await;

    // 가입 이벤트에 프로필이 실린 멤버는 따로 내지 않습니다(큰 크루에서 가입마다 이벤트가 늘지 않게).
    testkit::spread_cards(&c.node).await;
    let own = testkit::stream_events(&c.node, &space, StreamId::Space)
        .expect("이벤트")
        .iter()
        .filter(|e| e.header.kind == EventKind::MemberProfile && e.header.user == c.user())
        .count();
    assert_eq!(own, 0, "가입으로 실린 프로필을 다시 내면 안 됨");
}

/// 뱃지: 개발자 계정만 원년멤버를 주고, 받은 사람이 고른 뱃지가 크루 멤버에게 보입니다. 이름·별명에는 이모지를 넣을 수
/// 없습니다.
#[tokio::test]
async fn badges_founder_grant_and_shown_badges_reach_members() {
    use yangpa_core::badge::Badge;
    use yangpa_rpc::api::{
        BadgeSetShownParams, ProfileSetCrewParams, ProfileSetParams, ShownBadgeView,
    };

    let cl = Cluster::new();
    let mut a = cl.node("a").await;
    DEVELOPER.set(a.user()).expect("이 시험만 정함");
    a.restart_with(with_developer).await;
    let b = cl.node_with("b", with_developer).await;
    let c = cl.node_with("c", with_developer).await;
    let space = create_crew(&a, "뱃지 크루").await;
    connect_both(&b, &c).await;
    assert_eq!(join(&a, &b, space).await, InviteJoinStatus::Joined);
    assert_eq!(join(&a, &c, space).await, InviteJoinStatus::Joined);
    wait_joined(&b, space).await;
    wait_joined(&c, space).await;
    for n in [&a, &b, &c] {
        for u in [a.user(), b.user(), c.user()] {
            wait_member(n, space, u, true).await;
        }
    }

    // 개발자가 아니면 줄 수 없습니다.
    let r = b
        .call::<m::BadgeGrantFounder>(UserParams { user: c.user() })
        .await;
    assert_eq!(code(r), ERR_FORBIDDEN);
    assert!(!badges_of(&b, c.user()).await.can_grant_founder);

    // 개발자가 B에게 주면 B의 기기가 증서를 카드에 넣어 퍼뜨립니다.
    assert!(badges_of(&a, b.user()).await.can_grant_founder);
    a.ok::<m::BadgeGrantFounder>(UserParams { user: b.user() })
        .await;
    assert!(!badges_of(&a, b.user()).await.can_grant_founder);
    eventually("B가 원년멤버", || async {
        badges_of(&b, b.user())
            .await
            .earned
            .contains(&Badge::Founder)
            .then_some(())
    })
    .await;

    // 고른 뱃지가 C에게도 보입니다. 모으지 않은 뱃지는 고를 수 없습니다.
    let founder = ShownBadgeView {
        badge: Badge::Founder,
        after: false,
    };
    b.ok::<m::BadgeSetShown>(BadgeSetShownParams {
        shown: vec![founder],
    })
    .await;
    eventually("C가 B의 뱃지를 봄", || async {
        let v = badges_of(&c, b.user()).await;
        (v.earned == vec![Badge::Founder] && v.shown == vec![founder]).then_some(())
    })
    .await;
    let r = c
        .call::<m::BadgeSetShown>(BadgeSetShownParams {
            shown: vec![ShownBadgeView {
                badge: Badge::Support50,
                after: true,
            }],
        })
        .await;
    assert_eq!(code(r), ERR_INVALID_INPUT);

    // 이름·별명의 이모지는 거부하고, 이름을 그대로 둔 저장은 받습니다.
    let r = b
        .call::<m::ProfileSet>(ProfileSetParams {
            name: Some("철수🧅".into()),
            ..Default::default()
        })
        .await;
    assert_eq!(code(r), ERR_INVALID_INPUT);
    let r = b
        .call::<m::ProfileSetCrew>(ProfileSetCrewParams {
            space,
            nickname: Some("💎".into()),
        })
        .await;
    assert_eq!(code(r), ERR_INVALID_INPUT);
    b.ok::<m::ProfileSet>(ProfileSetParams {
        name: Some("b".into()),
        bio: Some("소개".into()),
        ..Default::default()
    })
    .await;
}
