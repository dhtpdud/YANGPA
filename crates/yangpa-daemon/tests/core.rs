//! 코어 흐름 통합 시험: 신원, 친구·1:1 공간, 메시지, 오프라인 따라잡기, 재시작, 위조 거부, 시더, 순서 뒤바뀜.
//!
//! 노드는 한 프로세스 안에서 127.0.0.1로 직접 연결합니다(`common` 참고). 고정 sleep으로 맞추지 않고
//! 조건이 될 때까지 기다립니다.

use crate::common;

use std::collections::{BTreeSet, HashSet};

use common::{
    Cluster, TestNode, befriend, connect_both, eventually, next_notification, wait_message,
};
use yangpa_core::event::StreamId;
use yangpa_core::ids::UserId;
use yangpa_core::ids::{ChannelId, SpaceId};
use yangpa_core::model::SpaceKind;
use yangpa_core::wire::{LamportRange, WireErrorCode, YpRequest, YpResponse};
use yangpa_daemon::testkit;
use yangpa_rpc::api::{
    ContactKind, Empty, FriendLinkCreateParams, IdentityCreateParams, MarkReadParams,
    MessageEditParams, MessageListParams, MessageRefParams, Notification, ProfileGetParams,
    ProfileSetParams, ReactParams, ReactionView, Scope, SpaceParams, TokenIdParams, UrlParams,
    UserParams, m,
};
use yangpa_rpc::jsonrpc::{
    ERR_AUTH_REQUIRED, ERR_FORBIDDEN, ERR_INVALID_INPUT, ERR_NO_IDENTITY, ERR_NOT_FOUND,
};
use yangpa_rpc::{Endpoint, RpcClient};

fn texts(n: &[yangpa_rpc::api::MessageView]) -> Vec<String> {
    n.iter().map(|v| v.text.clone()).collect()
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// 두 친구(공간 `space`)와, B와만 친구인 C(공간 `other`). C는 `space`의 멤버가 아닙니다.
struct Triangle {
    a: TestNode,
    b: TestNode,
    c: TestNode,
    space: SpaceId,
    other: SpaceId,
    channel: ChannelId,
}

async fn triangle(cl: &Cluster) -> Triangle {
    let a = cl.node("a").await;
    let b = cl.node("b").await;
    let c = cl.node("c").await;
    let space = befriend(&a, &b).await;
    let other = befriend(&b, &c).await;
    let channel = a.first_channel(space).await;
    Triangle {
        a,
        b,
        c,
        space,
        other,
        channel,
    }
}

/// 인증이 끝날 때까지 같은 요청을 다시 보내 `Unauthenticated`가 아닌 첫 응답을 돌려줍니다.
async fn request_after_auth(from: &TestNode, to: &TestNode, req: YpRequest) -> YpResponse {
    eventually("인증 뒤 응답", || async {
        match testkit::net_request(&from.node, to.peer(), req.clone()).await {
            Ok(YpResponse::Error {
                code: WireErrorCode::Unauthenticated,
                ..
            })
            | Err(_) => None,
            Ok(r) => Some(r),
        }
    })
    .await
}

fn is_error(r: &YpResponse, want: WireErrorCode) -> bool {
    matches!(r, YpResponse::Error { code, .. } if *code == want)
}

// ───────────── 1. 신원 ─────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn identity_round_trip() {
    let cl = Cluster::new();
    let mut a = cl.bare("a").await;
    let e = a.call::<m::IdentityGet>(Empty).await.unwrap_err();
    assert_eq!(e.code, ERR_NO_IDENTITY);

    let created = a
        .ok::<m::IdentityCreate>(IdentityCreateParams {
            name: "앨리스".into(),
        })
        .await;
    let got = a.ok::<m::IdentityGet>(Empty).await;
    assert_eq!(got.user, created.user);
    assert_eq!(got.device, created.device);
    assert_eq!(got.profile.name, "앨리스");
    assert!(got.devices.iter().any(|d| d.device == created.device));

    let p = a
        .ok::<m::ProfileSet>(ProfileSetParams {
            name: Some("앨리스2".into()),
            bio: Some("양파 좋아함".into()),
            ..Default::default()
        })
        .await;
    assert_eq!(p.name, "앨리스2");
    let p = a.ok::<m::ProfileGet>(ProfileGetParams { user: None }).await;
    assert_eq!(
        (p.name.as_str(), p.bio.as_str()),
        ("앨리스2", "양파 좋아함")
    );

    // 재시작해도 같은 신원·기기·프로필.
    a.restart().await;
    let again = a.ok::<m::IdentityGet>(Empty).await;
    assert_eq!(again.user, created.user);
    assert_eq!(again.device, created.device);
    assert_eq!(again.profile.name, "앨리스2");
    let devices = a.ok::<m::DeviceList>(Empty).await;
    assert!(devices.iter().any(|d| d.entry.device == created.device));
}

// ───────────── 2. 친구·1:1 공간 ─────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn friends_share_one_space_and_strangers_cannot_read() {
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let b = cl.node("b").await;
    let stranger = cl.node("stranger").await;
    let mut a_rx = a.subscribe();
    let mut b_rx = b.subscribe();

    let space = befriend(&a, &b).await;
    let b_user = b.user();
    let a_user = a.user();
    // 링크 주인은 수락하지 않으므로 친구 요청 알림 없이 연락처가 바뀌었다는 알림만 받습니다.
    next_notification(&mut a_rx, "A의 friend.updated", |n| {
        matches!(n, Notification::FriendUpdated { user } if *user == b_user).then_some(())
    })
    .await;
    next_notification(&mut b_rx, "B의 friend.accepted", |n| {
        matches!(n, Notification::FriendAccepted { user, space: s } if *user == a_user && *s == space)
            .then_some(())
    })
    .await;

    for (n, peer_name) in [(&a, "b"), (&b, "a")] {
        let list = n.ok::<m::SpaceList>(Empty).await;
        assert_eq!(list.len(), 1, "{}", n.name);
        assert_eq!(list[0].space, space);
        assert_eq!(list[0].kind, SpaceKind::Space);
        assert_eq!(list[0].name, peer_name, "1:1 공간 이름은 상대 이름");
        let view = n.ok::<m::SpaceGet>(SpaceParams { space }).await;
        let members: BTreeSet<_> = view.members.iter().map(|m| m.user).collect();
        assert_eq!(members, BTreeSet::from([a_user, b_user]));
        let friends = n.ok::<m::FriendList>(Empty).await;
        assert_eq!(friends.len(), 1);
        assert_eq!(friends[0].space, space);
        assert_eq!(friends[0].kind, ContactKind::Friend);
        assert!(!friends[0].proposed && friends[0].expires_at.is_none());
    }
    // 둘 다 같은 시작 이벤트·같은 크루 키 순번.
    let a_space: BTreeSet<_> = testkit::stream_events(&a.node, &space, StreamId::Space)
        .unwrap()
        .iter()
        .map(|e| e.id())
        .collect();
    let b_space: BTreeSet<_> = testkit::stream_events(&b.node, &space, StreamId::Space)
        .unwrap()
        .iter()
        .map(|e| e.id())
        .collect();
    assert!(a_space.is_subset(&b_space) || b_space.is_subset(&a_space));
    assert_eq!(
        testkit::crew_key_seq(&a.node, &space),
        testkit::crew_key_seq(&b.node, &space)
    );
    // 안전 번호는 양쪽이 같습니다.
    let sa = a
        .ok::<m::FriendSafetyNumber>(UserParams { user: b_user })
        .await;
    let sb = b
        .ok::<m::FriendSafetyNumber>(UserParams { user: a_user })
        .await;
    assert_eq!(sa.digits, sb.digits);

    let channel = a.first_channel(space).await;
    let id = a.send(space, channel, "비밀 메시지").await;
    wait_message(&b, space, channel, id, "B가 메시지 받음", |_| true).await;

    // 친구가 아닌 노드: 공간을 모르고, 저장된 본문은 암호문이며, 시더도 거절합니다.
    connect_both(&stranger, &a).await;
    assert!(stranger.ok::<m::SpaceList>(Empty).await.is_empty());
    let e = stranger
        .call::<m::MessageList>(MessageListParams {
            space,
            channel,
            before: None,
            limit: None,
            thread: None,
        })
        .await
        .unwrap_err();
    assert_eq!(e.code, ERR_NOT_FOUND);
    let ev = testkit::get_event(&a.node, &id).unwrap().unwrap();
    assert!(!contains(
        ev.body.as_ref().unwrap(),
        "비밀 메시지".as_bytes()
    ));
    assert!(
        testkit::authenticate(&stranger.node, a.peer())
            .await
            .is_err()
    );
    let r = testkit::net_request(
        &stranger.node,
        a.peer(),
        YpRequest::GetEvents {
            space,
            ids: vec![id],
        },
    )
    .await
    .unwrap();
    assert!(is_error(&r, WireErrorCode::Unauthenticated), "{r:?}");
}

// ───────────── 3. 메시지 ─────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn messages_both_ways_with_edit_delete_react() {
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let b = cl.node("b").await;
    let space = befriend(&a, &b).await;
    let ch = a.first_channel(space).await;
    let mut a_rx = a.subscribe();
    let mut b_rx = b.subscribe();

    let m1 = a.send(space, ch, "안녕 B").await;
    let (msg, notify) = next_notification(&mut b_rx, "B의 message.new", |n| match n {
        Notification::MessageNew {
            space: s,
            channel: c,
            message,
            notify,
        } if *s == space && *c == ch && message.id == m1 => Some((message.clone(), notify.clone())),
        _ => None,
    })
    .await;
    assert_eq!(msg.text, "안녕 B");
    assert_eq!(msg.author, a.user());
    assert_eq!(notify.title, "a");
    assert_eq!(notify.body.as_deref(), Some("안녕 B"));

    let m2 = b.send(space, ch, "안녕 A").await;
    next_notification(&mut a_rx, "A의 message.new", |n| {
        matches!(n, Notification::MessageNew { message, .. } if message.id == m2).then_some(())
    })
    .await;
    let ids: Vec<_> = a.messages(space, ch).await.iter().map(|v| v.id).collect();
    assert_eq!(ids, vec![m1, m2]);

    // 읽지 않음: B에게 A의 메시지 하나.
    let unread = b.ok::<m::SpaceList>(Empty).await[0].unread;
    assert_eq!(unread, 1);
    b.ok::<m::ChannelMarkRead>(MarkReadParams {
        space,
        channel: ch,
        event: Some(m2),
    })
    .await;
    assert_eq!(b.ok::<m::SpaceList>(Empty).await[0].unread, 0);

    // 수정: 작성자만.
    a.ok::<m::MessageEdit>(MessageEditParams {
        space,
        channel: ch,
        id: m1,
        text: "안녕 B (수정)".into(),
    })
    .await;
    let edited = next_notification(&mut b_rx, "B의 message.updated(수정)", |n| match n {
        Notification::MessageUpdated { message, .. }
            if message.id == m1 && message.text == "안녕 B (수정)" =>
        {
            Some(message.clone())
        }
        _ => None,
    })
    .await;
    assert!(edited.edited_at.is_some());
    let e = b
        .call::<m::MessageEdit>(MessageEditParams {
            space,
            channel: ch,
            id: m1,
            text: "남의 글".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(e.code, ERR_FORBIDDEN);

    // 반응 → 취소.
    let react = ReactParams {
        space,
        channel: ch,
        id: m1,
        emoji: "👍".into(),
    };
    b.ok::<m::MessageReact>(react.clone()).await;
    next_notification(&mut a_rx, "A의 message.updated(반응)", |n| match n {
        Notification::MessageUpdated { message, .. }
            if message.id == m1
                && message.reactions
                    == vec![ReactionView {
                        emoji: "👍".into(),
                        count: 1,
                        me: false,
                    }] =>
        {
            Some(())
        }
        _ => None,
    })
    .await;
    wait_message(&b, space, ch, m1, "B에게 내 반응 표시", |v| {
        v.reactions.len() == 1 && v.reactions[0].me
    })
    .await;
    b.ok::<m::MessageUnreact>(react).await;
    wait_message(&a, space, ch, m1, "A에게 반응 취소", |v| {
        v.reactions.iter().all(|r| r.count == 0)
    })
    .await;

    // 삭제.
    a.ok::<m::MessageDelete>(MessageRefParams {
        space,
        channel: ch,
        id: m1,
    })
    .await;
    let gone = wait_message(&b, space, ch, m1, "B에게 삭제", |v| v.deleted).await;
    assert!(
        gone.text.is_empty(),
        "삭제된 메시지 본문은 보이지 않아야 함"
    );
    // 1:1 공간에서는 남의 메시지를 지울 수 없습니다.
    let e = a
        .call::<m::MessageDelete>(MessageRefParams {
            space,
            channel: ch,
            id: m2,
        })
        .await
        .unwrap_err();
    assert_eq!(e.code, ERR_FORBIDDEN);
}

// ───────────── 4. 오프라인 따라잡기 ─────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn offline_catch_up_of_50_messages_after_restart() {
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let mut b = cl.node("b").await;
    let space = befriend(&a, &b).await;
    let ch = a.first_channel(space).await;

    b.shutdown().await;
    let mut sent = Vec::new();
    for i in 0..50 {
        sent.push(a.send(space, ch, &format!("오프라인 {i}")).await);
    }
    // B는 같은 데이터 경로로 다시 뜨고, 저장된 노드 기록으로 스스로 A에 다시 연결합니다.
    b.start_again().await;
    // 앱 시작 화면이 기다리는 시작 연결: 끝났다고 알릴 때는 친구가 이미 인증(온라인)돼 있어야 합니다.
    eventually("시작 연결이 끝남", || async {
        let s = b.ok::<m::NetStatusGet>(Empty).await;
        s.connecting.is_empty().then_some(())
    })
    .await;
    assert!(testkit::is_authed(&b.node, a.peer()));
    eventually("B가 50개를 따라잡음", || async {
        let have: HashSet<_> = b.messages(space, ch).await.iter().map(|v| v.id).collect();
        sent.iter().all(|id| have.contains(id)).then_some(())
    })
    .await;
    let list = b.messages(space, ch).await;
    let want: Vec<String> = (0..50).map(|i| format!("오프라인 {i}")).collect();
    assert_eq!(texts(&list), want);
}

// ───────────── 5. 재시작 뒤 키·키 그룹 상태 ─────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn keys_and_group_state_survive_restart() {
    let cl = Cluster::new();
    let mut a = cl.node("a").await;
    let mut b = cl.node("b").await;
    let space = befriend(&a, &b).await;
    let ch = a.first_channel(space).await;
    let m1 = a.send(space, ch, "재시작 전").await;
    wait_message(&b, space, ch, m1, "B가 m1 받음", |_| true).await;
    let seq = testkit::crew_key_seq(&a.node, &space).expect("크루 키 순번");

    a.restart().await;
    assert_eq!(testkit::crew_key_seq(&a.node, &space), Some(seq));
    assert_eq!(texts(&a.messages(space, ch).await), vec!["재시작 전"]);
    let view = a.ok::<m::SpaceGet>(SpaceParams { space }).await;
    assert_eq!(view.members.len(), 2);

    let m2 = a.send(space, ch, "재시작 후 A").await;
    wait_message(&b, space, ch, m2, "B가 재시작한 A의 메시지 받음", |_| true).await;
    let m3 = b.send(space, ch, "재시작한 A에게").await;
    wait_message(
        &a,
        space,
        ch,
        m3,
        "재시작한 A가 B 메시지를 엶",
        |v| v.text == "재시작한 A에게",
    )
    .await;

    b.restart().await;
    assert_eq!(testkit::crew_key_seq(&b.node, &space), Some(seq));
    let m4 = a.send(space, ch, "B도 재시작").await;
    wait_message(&b, space, ch, m4, "재시작한 B가 받음", |v| {
        v.text == "B도 재시작"
    })
    .await;
    assert_eq!(
        texts(&b.messages(space, ch).await),
        vec!["재시작 전", "재시작 후 A", "재시작한 A에게", "B도 재시작"]
    );
}

// ───────────── 6. 위조·변조 거부 ─────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn forged_and_tampered_events_are_rejected() {
    let cl = Cluster::new();
    let t = triangle(&cl).await;
    let (a, b, c) = (&t.a, &t.b, &t.c);
    let (space, ch) = (t.space, t.channel);
    let m1 = a.send(space, ch, "진짜").await;
    wait_message(b, space, ch, m1, "B가 m1 받음", |_| true).await;
    let ev = testkit::get_event(&a.node, &m1).unwrap().unwrap();
    let from = a.peer();

    // 본문 변조(본문 해시 불일치).
    let r = testkit::ingest_from(&b.node, space, vec![testkit::tamper_body(&ev)], from)
        .await
        .unwrap();
    assert!(r.is_rejected(&m1), "{r:?}");
    // 헤더 변조(서명 불일치): 새 ID로도 저장되지 않아야 함.
    let forged_header = testkit::tamper_header(&ev);
    let r = testkit::ingest_from(&b.node, space, vec![forged_header.clone()], from)
        .await
        .unwrap();
    assert!(r.is_rejected(&forged_header.id()), "{r:?}");
    assert!(
        testkit::get_event(&b.node, &forged_header.id())
            .unwrap()
            .is_none()
    );
    // 다른 공간으로 넣기.
    let r = testkit::ingest_from(&b.node, t.other, vec![ev.clone()], from)
        .await
        .unwrap();
    assert!(r.is_rejected(&m1), "{r:?}");
    // 멤버가 아닌 C가 서명한 메시지: 가입이 늦게 왔을 수 있으므로 저장하지 않고 RAM에만 보류합니다.
    let lamport = ev.header.lamport + 1;
    let forged =
        testkit::forge_message(&c.node, space, ch, vec![m1], lamport, "가짜", None).unwrap();
    let r = testkit::ingest_from(&b.node, space, vec![forged.clone()], from)
        .await
        .unwrap();
    assert_eq!(r.held, 1, "{r:?}");
    assert!(r.accepted.is_empty(), "{r:?}");
    assert!(testkit::get_event(&b.node, &forged.id()).unwrap().is_none());
    // C가 A인 척(헤더 사용자 = A, 서명 기기 = C).
    let fake_a = testkit::forge_message(
        &c.node,
        space,
        ch,
        vec![m1],
        lamport,
        "A인 척",
        Some(a.user()),
    )
    .unwrap();
    let r = testkit::ingest_from(&b.node, space, vec![fake_a.clone()], from)
        .await
        .unwrap();
    assert_eq!(r.held, 1, "{r:?}");
    assert!(r.accepted.is_empty(), "{r:?}");
    assert!(testkit::get_event(&b.node, &fake_a.id()).unwrap().is_none());
    // C가 A 기기인 척 짧은 서명(채널 이벤트는 공개키를 싣지 않음): B가 상태 기록의 A 키로 확인해 거절합니다.
    let slim_a = testkit::forge_slim_as(
        &c.node,
        &forged,
        (testkit::device_id(&a.node).unwrap(), a.user()),
    )
    .unwrap();
    let r = testkit::ingest_from(&b.node, space, vec![slim_a.clone()], from)
        .await
        .unwrap();
    assert!(r.is_rejected(&slim_a.id()), "{r:?}");
    assert!(testkit::get_event(&b.node, &slim_a.id()).unwrap().is_none());
    // 네트워크로 밀어 넣기: C는 B와 인증된 친구지만 이 공간 멤버가 아니므로 거절.
    let r = request_after_auth(
        c,
        b,
        YpRequest::PushEvents {
            space,
            events: vec![forged.clone()],
        },
    )
    .await;
    assert!(is_error(&r, WireErrorCode::NotMember), "{r:?}");

    assert_eq!(texts(&b.messages(space, ch).await), vec!["진짜"]);
}

/// 아무도 모르는 상태 키로 봉인한 상태 이벤트는 풀지 못한 채 남아 체크포인트를 막습니다. 멤버가 아닌 사람이 쓴 것은
/// 저장하지 않고 RAM에만 보류하고, 멤버가 쓴 것만 저장해 키를 기다립니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unknown_key_state_events_from_strangers_are_not_stored() {
    let cl = Cluster::new();
    let t = triangle(&cl).await;
    let (a, b, c) = (&t.a, &t.b, &t.c);
    let (space, ch) = (t.space, t.channel);
    let m1 = a.send(space, ch, "진짜").await;
    wait_message(b, space, ch, m1, "B가 m1 받음", |_| true).await;
    let lamport = testkit::get_event(&a.node, &m1)
        .unwrap()
        .unwrap()
        .header
        .lamport
        + 1;
    let from = a.peer();

    let stranger = testkit::forge_unknown_key_state(&c.node, space, lamport).unwrap();
    let r = testkit::ingest_from(&b.node, space, vec![stranger.clone()], from)
        .await
        .unwrap();
    assert_eq!(r.held, 1, "{r:?}");
    assert!(
        testkit::get_event(&b.node, &stranger.id())
            .unwrap()
            .is_none()
    );

    let member = testkit::forge_unknown_key_state(&a.node, space, lamport).unwrap();
    let r = testkit::ingest_from(&b.node, space, vec![member.clone()], from)
        .await
        .unwrap();
    assert_eq!(r.held, 0, "{r:?}");
    assert!(testkit::get_event(&b.node, &member.id()).unwrap().is_some());
}

// ───────────── 7. 시더가 낯선 노드를 거절 ─────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn seeder_refuses_strangers_and_non_members() {
    let cl = Cluster::new();
    let t = triangle(&cl).await;
    let (a, b, c) = (&t.a, &t.b, &t.c);
    let (space, ch) = (t.space, t.channel);
    let m1 = a.send(space, ch, "멤버끼리만").await;
    wait_message(b, space, ch, m1, "B가 m1 받음", |_| true).await;

    // 아무 관계 없는 노드: 인증도 거절되고, 동기화 요청은 인증 전이라 거절.
    let d = cl.node("d").await;
    connect_both(&d, b).await;
    assert!(testkit::authenticate(&d.node, b.peer()).await.is_err());
    for req in [
        YpRequest::GetHeads {
            space,
            stream: StreamId::Space,
        },
        YpRequest::GetEvents {
            space,
            ids: vec![m1],
        },
    ] {
        let r = testkit::net_request(&d.node, b.peer(), req).await.unwrap();
        assert!(is_error(&r, WireErrorCode::Unauthenticated), "{r:?}");
    }
    // 압축되어 오는 요청은 인증 전이면 풀지 않고 거절합니다.
    let r = testkit::net_request(
        &d.node,
        b.peer(),
        YpRequest::GetEvents {
            space,
            ids: vec![m1; 200],
        },
    )
    .await
    .unwrap();
    assert!(
        matches!(&r, YpResponse::Error { code: WireErrorCode::Unauthenticated, message }
            if message == "인증 전에는 압축한 요청을 풀지 않음"),
        "{r:?}"
    );

    // B의 친구 C(인증됨)라도 이 공간 멤버가 아니면 시더 정책이 거절합니다.
    let range = LamportRange {
        start: 0,
        end: u64::MAX,
    };
    for req in [
        YpRequest::GetHeads {
            space,
            stream: StreamId::Space,
        },
        YpRequest::GetHeads {
            space,
            stream: StreamId::Channel(ch),
        },
        YpRequest::GetFingerprints {
            space,
            stream: StreamId::Channel(ch),
            ranges: vec![range],
        },
        YpRequest::GetIds {
            space,
            stream: StreamId::Channel(ch),
            range,
            limit: 100,
        },
        YpRequest::GetEvents {
            space,
            ids: vec![m1],
        },
    ] {
        let r = request_after_auth(c, b, req).await;
        assert!(is_error(&r, WireErrorCode::NotMember), "{r:?}");
    }
    // 대조: C는 자기 공간은 받을 수 있습니다.
    let r = request_after_auth(
        c,
        b,
        YpRequest::GetHeads {
            space: t.other,
            stream: StreamId::Space,
        },
    )
    .await;
    assert!(
        matches!(r, YpResponse::Heads { ref heads, .. } if !heads.is_empty()),
        "{r:?}"
    );
    // C가 자기 공간(`other`) 이름으로 A–B 공간의 이벤트를 달라고 해도 주지 않습니다(이벤트의 공간을 봄).
    let foreign: Vec<_> = testkit::stream_events(&b.node, &space, StreamId::Space)
        .unwrap()
        .iter()
        .map(|e| e.id())
        .chain([m1])
        .collect();
    let r = request_after_auth(
        c,
        b,
        YpRequest::GetEvents {
            space: t.other,
            ids: foreign,
        },
    )
    .await;
    match r {
        YpResponse::Events(evs) => {
            assert!(evs.is_empty(), "다른 공간 이벤트가 나감: {}", evs.len())
        }
        YpResponse::Error { .. } => {}
        other => panic!("예상하지 못한 응답: {other:?}"),
    }
}

// ───────────── 8. 순서가 뒤바뀐 도착 ─────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn out_of_order_events_wait_for_keys_and_parents() {
    use yangpa_core::body::{Secret32, SharedKey};

    let cl = Cluster::new();
    let a = cl.node("a").await;
    let mut b = cl.node("b").await;
    let space = befriend(&a, &b).await;
    let ch = a.first_channel(space).await;

    // B가 꺼진 동안 A가 만든 이벤트를 전파 없이 들고 있다가, A도 끈 뒤 B에게 거꾸로 넣습니다
    // (그 사이 따라잡기가 끼어들지 않게).
    b.shutdown().await;
    let first = testkit::send_unpropagated(&a.node, space, ch, "첫째")
        .await
        .unwrap();
    // 채널 뿌리는 크루 키에서 파생하므로 첫 메시지도 키 묶음 없이 메시지 하나입니다.
    assert_eq!(first.len(), 1, "키 묶음 없이 메시지만");
    // 둘째는 B가 파생할 수 없는 뿌리로 씁니다. 그 뿌리를 크루 상태 키로 채널 스트림에 봉인한 키 묶음(기록 공개를 켤 때
    // 지난 뿌리를 다시 봉인하는 길)이 와야 풀립니다.
    let root = Secret32::random();
    let root_id = yangpa_crypto::keysched::channel_root_id(&space, &ch, &root);
    let m2 = testkit::send_unpropagated_with_root(&a.node, space, ch, "둘째", root.clone())
        .await
        .unwrap();
    let key_share = testkit::author_key_share(
        &a.node,
        space,
        ch,
        vec![SharedKey::ChannelRoot {
            channel: ch,
            id: root_id,
            key: root,
        }],
        testkit::ShareVia::GroupState,
    )
    .unwrap();
    let m1 = first[0].clone();
    let from = a.peer();
    a.shutdown().await;
    b.start_again().await;

    // 1) 둘째: 뿌리도 부모(첫째)도 없음 → 저장하고 기다림.
    let r = testkit::ingest_from(&b.node, space, vec![m2.clone()], from)
        .await
        .unwrap();
    assert!(r.is_accepted(&m2.id()), "{r:?}");
    assert_eq!(r.waiting, 1);
    assert!(r.missing_parents);
    let list = b
        .ok::<m::MessageList>(MessageListParams {
            space,
            channel: ch,
            before: None,
            limit: None,
            thread: None,
        })
        .await;
    assert!(list.messages.is_empty());
    assert!(
        list.gaps.iter().any(|g| g.before == Some(m2.id())),
        "{:?}",
        list.gaps
    );

    // 2) 첫째: 크루 키에서 파생한 뿌리라 바로 풀리고, 둘째는 여전히 뿌리를 기다림.
    let r = testkit::ingest_from(&b.node, space, vec![m1.clone()], from)
        .await
        .unwrap();
    assert!(r.is_accepted(&m1.id()));
    assert_eq!(testkit::waiting_count(&b.node, &space), 1);
    assert_eq!(texts(&b.messages(space, ch).await), vec!["첫째"]);

    // 3) 뿌리 키 묶음(크루 상태 키로 암호화): 기다리던 둘째가 풀림.
    let r = testkit::ingest_from(&b.node, space, vec![key_share.clone()], from)
        .await
        .unwrap();
    assert!(r.is_accepted(&key_share.id()), "{r:?}");
    assert_eq!(testkit::waiting_count(&b.node, &space), 0);
    let list = b
        .ok::<m::MessageList>(MessageListParams {
            space,
            channel: ch,
            before: None,
            limit: None,
            thread: None,
        })
        .await;
    assert_eq!(texts(&list.messages), vec!["첫째", "둘째"]);
    assert!(list.gaps.is_empty(), "{:?}", list.gaps);

    // 다시 넣으면 중복으로 무시.
    let r = testkit::ingest_from(&b.node, space, vec![m1, m2], from)
        .await
        .unwrap();
    assert_eq!(r.duplicate, 2);

    // 재시작 뒤에도 그대로(대기 목록이 비어 있고 보기 행이 남음).
    b.restart().await;
    assert_eq!(texts(&b.messages(space, ch).await), vec!["첫째", "둘째"]);
    assert_eq!(testkit::waiting_count(&b.node, &space), 0);
}

// ───────────── 연락처 링크 폐기·1회용·만료 ─────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn contact_link_revoke_single_use_expiry_and_reunion() {
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let b = cl.node("b").await;
    let c = cl.node("c").await;
    connect_both(&a, &b).await;
    connect_both(&a, &c).await;

    // 폐기한 링크는 거절됩니다.
    let revoked = a
        .ok::<m::FriendLinkCreate>(FriendLinkCreateParams {
            expires_in_secs: None,
            single_use: Some(false),
        })
        .await;
    assert_eq!(a.ok::<m::FriendLinkList>(Empty).await.len(), 1);
    a.ok::<m::FriendLinkRevoke>(TokenIdParams {
        token_id: revoked.token_id.clone(),
    })
    .await;
    assert!(a.ok::<m::FriendLinkList>(Empty).await.is_empty());
    let e = b
        .call::<m::FriendAdd>(UrlParams {
            url: revoked.url.clone(),
        })
        .await
        .unwrap_err();
    assert_eq!(e.code, ERR_INVALID_INPUT, "{e:?}");

    // 1회용 링크: B가 쓰면 링크 주인의 수락 없이 둘이 임시 연락처가 되고, C는 못 씁니다.
    let once = a
        .ok::<m::FriendLinkCreate>(FriendLinkCreateParams {
            expires_in_secs: Some(600),
            single_use: Some(true),
        })
        .await;
    b.ok::<m::FriendAdd>(UrlParams {
        url: once.url.clone(),
    })
    .await;
    let e = c
        .call::<m::FriendAdd>(UrlParams { url: once.url })
        .await
        .unwrap_err();
    assert_eq!(e.code, ERR_INVALID_INPUT, "{e:?}");
    let (a_user, b_user, c_user) = (a.user(), b.user(), c.user());
    let contact = |n: &TestNode, user| {
        let n = n.node.clone();
        async move {
            common::call::<m::FriendList>(&n, Empty)
                .await
                .unwrap()
                .into_iter()
                .find(|f| f.user == user)
        }
    };
    let ab = eventually("B가 A의 임시 연락처", || async {
        contact(&b, a_user)
            .await
            .filter(|f| f.kind == ContactKind::Temporary)
    })
    .await;
    assert!(ab.expires_at.is_some(), "링크의 만료 시각을 따름");
    let ba = eventually("A가 B를 임시 연락처로", || async {
        contact(&a, b_user)
            .await
            .filter(|f| f.kind == ContactKind::Temporary)
    })
    .await;
    assert_eq!(ba.space, ab.space);
    assert!(
        a.ok::<m::FriendRequests>(Empty).await.is_empty(),
        "수락할 요청 없음"
    );

    // 만료되면 둘 다 "만료된 연락처": 지난 대화는 읽고, 보내기는 거절됩니다.
    let s_ab = ab.space;
    let ch = a.first_channel(s_ab).await;
    let hello = b.send(s_ab, ch, "만료 전").await;
    wait_message(&a, s_ab, ch, hello, "A가 받음", |_| true).await;
    for (n, other) in [(&a, b_user), (&b, a_user)] {
        testkit::expire_contact_now(&n.node, &other);
        assert_eq!(
            contact(n, other).await.map(|f| f.kind),
            Some(ContactKind::Expired)
        );
    }
    let e = b
        .call::<m::MessageSend>(yangpa_rpc::api::MessageSendParams {
            space: s_ab,
            channel: ch,
            text: "만료 뒤".into(),
            attachments: Vec::new(),
            reply_to: None,
            thread: None,
        })
        .await
        .unwrap_err();
    assert!(e.message.contains("만료"), "{e:?}");
    assert_eq!(texts(&a.messages(s_ab, ch).await), vec!["만료 전"]);
    let e = b
        .call::<m::FriendPropose>(UserParams { user: a_user })
        .await
        .unwrap_err();
    assert_eq!(e.code, ERR_INVALID_INPUT, "{e:?}");

    // 같은 사람이 새 링크를 쓰면 같은 대화로 다시 임시 연락처가 됩니다.
    let again = a
        .ok::<m::FriendLinkCreate>(FriendLinkCreateParams {
            expires_in_secs: None,
            single_use: Some(true),
        })
        .await;
    b.ok::<m::FriendAdd>(UrlParams { url: again.url }).await;
    for (n, other) in [(&a, b_user), (&b, a_user)] {
        let f = eventually("다시 임시 연락처", || async {
            contact(n, other)
                .await
                .filter(|f| f.kind == ContactKind::Temporary)
        })
        .await;
        assert_eq!(f.space, s_ab, "같은 대화");
        assert!(f.expires_at.is_none(), "만료 없는 링크");
    }
    let back = eventually("다시 보낼 수 있음", || async {
        b.call::<m::MessageSend>(yangpa_rpc::api::MessageSendParams {
            space: s_ab,
            channel: ch,
            text: "다시 만남".into(),
            attachments: Vec::new(),
            reply_to: None,
            thread: None,
        })
        .await
        .ok()
        .map(|r| r.id)
    })
    .await;
    wait_message(&a, s_ab, ch, back, "A가 다시 받음", |_| true).await;

    // 링크를 폐기하면 그 링크로 생긴 임시 연락처도 바로 만료되고, 상대도 다음 인증 때 압니다.
    let multi = a
        .ok::<m::FriendLinkCreate>(FriendLinkCreateParams {
            expires_in_secs: Some(600),
            single_use: Some(false),
        })
        .await;
    c.ok::<m::FriendAdd>(UrlParams { url: multi.url }).await;
    eventually("C가 임시 연락처", || async {
        contact(&c, a_user)
            .await
            .filter(|f| f.kind == ContactKind::Temporary)
    })
    .await;
    eventually("A가 C를 임시 연락처로", || async {
        contact(&a, c_user)
            .await
            .filter(|f| f.kind == ContactKind::Temporary)
    })
    .await;
    a.ok::<m::FriendLinkRevoke>(TokenIdParams {
        token_id: multi.token_id,
    })
    .await;
    assert_eq!(
        contact(&a, c_user).await.map(|f| f.kind),
        Some(ContactKind::Expired)
    );
    assert_eq!(
        contact(&a, b_user).await.map(|f| f.kind),
        Some(ContactKind::Temporary),
        "다른 링크로 만난 연락처는 그대로"
    );
    eventually("C도 만료를 앎", || async {
        contact(&c, a_user)
            .await
            .filter(|f| f.kind == ContactKind::Expired)
    })
    .await;

    // 만료된 연락처를 지우면 그 대화도 이 기기에서 지웁니다.
    let s_ac = contact(&c, a_user).await.unwrap().space;
    c.ok::<m::FriendRemove>(UserParams { user: a_user }).await;
    assert!(contact(&c, a_user).await.is_none());
    assert!(!testkit::known_spaces(&c.node).contains(&s_ac));
}

// ───────────── 로컬 API 서버와 소유자 토큰 ─────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_api_server_with_owner_token() {
    let cl = Cluster::new();
    let a = cl.bare("a").await;
    let token_path = a
        .node
        .data_dir()
        .join(yangpa_rpc::endpoint::OWNER_TOKEN_FILE);
    let token = yangpa_rpc::endpoint::read_token_file(&token_path).expect("소유자 토큰 파일");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&token_path).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "소유자 토큰 파일은 본인만 읽어야 함");
    }
    let endpoint = Endpoint::in_dir(&a.dir);
    let server = yangpa_rpc::serve(
        endpoint.clone(),
        std::sync::Arc::new(yangpa_daemon::ApiHandler::new(a.node.clone())),
        a.node.event_hub(),
    )
    .await
    .expect("로컬 API 열기");

    // 인증 전에는 공개 메서드만.
    let anon = RpcClient::connect(&endpoint).await.unwrap();
    anon.request::<m::SystemVersion>(Empty).await.unwrap();
    let e = anon.request::<m::IdentityGet>(Empty).await.unwrap_err();
    assert_eq!(e.code(), Some(ERR_AUTH_REQUIRED), "{e:?}");
    assert!(anon.hello("틀린 토큰").await.is_err());

    let (client, hello) = RpcClient::connect_with_token(&endpoint, token.expose())
        .await
        .expect("소유자 토큰으로 인증");
    assert!(hello.scopes.contains(&Scope::Owner));
    assert!(!hello.automation);
    let created = client
        .request::<m::IdentityCreate>(IdentityCreateParams {
            name: "앨리스".into(),
        })
        .await
        .unwrap();
    let got = client.request::<m::IdentityGet>(Empty).await.unwrap();
    assert_eq!(got.user, created.user);
    assert!(
        client
            .request::<m::SpaceList>(Empty)
            .await
            .unwrap()
            .is_empty()
    );

    drop(client);
    drop(anon);
    server.shutdown();
    server.wait().await;
}

// ───────────── 11. 수정·삭제·반응 대상 검사 ─────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn edit_delete_targets_must_be_messages_of_the_same_channel() {
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let mut b = cl.node("b").await;
    let space = befriend(&a, &b).await;
    let ch = a.first_channel(space).await;
    let ch2 = testkit::create_channel(&a.node, space, "둘째", Vec::new())
        .await
        .unwrap();
    eventually("B가 둘째 채널을 앎", || async {
        testkit::space_state(&b.node, &space)?
            .channels
            .contains_key(&ch2)
            .then_some(())
    })
    .await;
    let from = a.peer();
    let genesis = testkit::stream_events(&b.node, &space, StreamId::Space)
        .unwrap()
        .into_iter()
        .find(|e| e.header.kind == yangpa_core::event::EventKind::SpaceCreate)
        .expect("공간 생성 이벤트")
        .id();
    let other_ch_msg = a.send(space, ch2, "둘째 채널").await;
    wait_message(
        &b,
        space,
        ch2,
        other_ch_msg,
        "B가 둘째 채널 메시지 받음",
        |_| true,
    )
    .await;

    // 공간 생성 이벤트를 지우려는 삭제와 다른 채널 메시지를 지우려는 삭제는 거부.
    for target in [genesis, other_ch_msg] {
        let evs = testkit::author_message_body(
            &a.node,
            space,
            ch,
            yangpa_core::body::EventBody::Delete { target },
        )
        .await
        .unwrap();
        let del = evs.last().unwrap().id();
        let r = testkit::ingest_from(&b.node, space, evs, from)
            .await
            .unwrap();
        assert!(r.is_rejected(&del), "{r:?}");
        assert!(testkit::has_body(&b.node, &target), "대상 본문이 지워짐");
    }
    // 반응도 같은 채널의 메시지에만.
    let evs = testkit::author_message_body(
        &a.node,
        space,
        ch,
        yangpa_core::body::EventBody::Reaction {
            target: genesis,
            emoji: yangpa_core::body::ReactionEmoji::Unicode {
                value: "👍".into()
            },
        },
    )
    .await
    .unwrap();
    let react = evs.last().unwrap().id();
    let r = testkit::ingest_from(&b.node, space, evs, from)
        .await
        .unwrap();
    assert!(r.is_rejected(&react), "{r:?}");

    // 대상보다 먼저 온 삭제·반응은 대상이 올 때까지 기다렸다가 적용됩니다.
    let target_evs = testkit::send_unpropagated(&a.node, space, ch, "늦게 오는 대상")
        .await
        .unwrap();
    let late = target_evs.last().unwrap().id();
    let mut early = testkit::author_message_body(
        &a.node,
        space,
        ch,
        yangpa_core::body::EventBody::Reaction {
            target: late,
            emoji: yangpa_core::body::ReactionEmoji::Unicode {
                value: "🧅".into()
            },
        },
    )
    .await
    .unwrap();
    early.extend(
        testkit::author_message_body(
            &a.node,
            space,
            ch,
            yangpa_core::body::EventBody::Delete { target: late },
        )
        .await
        .unwrap(),
    );
    // 기록 키 묶음은 먼저 주고 대상만 늦게.
    let (keys_first, rest): (Vec<_>, Vec<_>) = target_evs
        .into_iter()
        .partition(|e| e.header.kind == yangpa_core::event::EventKind::KeyShare);
    testkit::ingest_from(&b.node, space, keys_first, from)
        .await
        .unwrap();
    let r = testkit::ingest_from(&b.node, space, early, from)
        .await
        .unwrap();
    assert!(r.rejected.is_empty(), "{r:?}");
    assert!(r.waiting >= 2, "{r:?}");
    testkit::ingest_from(&b.node, space, rest, from)
        .await
        .unwrap();
    eventually("늦게 온 대상이 지워짐", || async {
        (testkit::get_event(&b.node, &late).unwrap().is_some()
            && !testkit::has_body(&b.node, &late))
        .then_some(())
    })
    .await;

    // 다시 시작해도 공간이 그대로 세워집니다.
    b.restart().await;
    let st = testkit::space_state(&b.node, &space).expect("재시작 뒤 공간 상태");
    assert!(st.channels.contains_key(&ch2));
}

// ───────────── 12. 기록 키 심기 ─────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn planted_history_keys_are_never_used_for_sending() {
    use yangpa_core::body::{Secret32, SharedKey};
    use yangpa_core::event::KeyRef;
    use yangpa_core::ids::ChannelRootId;

    let cl = Cluster::new();
    let a = cl.node("a").await;
    let b = cl.node("b").await;
    let space = befriend(&a, &b).await;
    let ch = a.first_channel(space).await;
    let from = a.peer();
    // 뿌리 ID는 뿌리에서 나옵니다. 남은 공격은 ID 끼워 넣기와, 받은(읽기 전용) 뿌리를 보내는 데 쓰게 하기.
    let planted = |key: Secret32| SharedKey::ChannelRoot {
        channel: ch,
        id: yangpa_crypto::keysched::channel_root_id(&space, &ch, &key),
        key,
    };

    // ID가 뿌리에서 나오지 않은 뿌리: 거부.
    let forged = testkit::author_key_share(
        &a.node,
        space,
        ch,
        vec![SharedKey::ChannelRoot {
            channel: ch,
            id: ChannelRootId::random(),
            key: Secret32::random(),
        }],
        testkit::ShareVia::GroupState,
    )
    .unwrap();
    let r = testkit::ingest_from(&b.node, space, vec![forged.clone()], from)
        .await
        .unwrap();
    assert!(r.is_rejected(&forged.id()), "{r:?}");
    assert!(testkit::get_event(&b.node, &forged.id()).unwrap().is_none());

    // 봉인 상자로 받은 뿌리는 읽기 전용: B가 보낼 때 쓰지 않고 새 뿌리를 만듭니다.
    let planted_key = Secret32::random();
    let planted_id = yangpa_crypto::keysched::channel_root_id(&space, &ch, &planted_key);
    let sealed = testkit::author_key_share(
        &a.node,
        space,
        ch,
        vec![planted(planted_key)],
        testkit::ShareVia::SealedToMembers,
    )
    .unwrap();
    let r = testkit::ingest_from(&b.node, space, vec![sealed.clone()], from)
        .await
        .unwrap();
    assert!(r.rejected.is_empty(), "{r:?}");
    let sent = b.send(space, ch, "새 키로").await;
    let ev = testkit::get_event(&b.node, &sent).unwrap().unwrap();
    assert!(
        matches!(ev.header.key, KeyRef::Record { root } if root != planted_id),
        "심은 키로 보냄: {:?}",
        ev.header.key
    );
    wait_message(
        &a,
        space,
        ch,
        sent,
        "A가 B의 새 키 메시지를 엶",
        |v| v.text == "새 키로",
    )
    .await;
}

// ───────────── 13. 볼 수 없는 채널 ─────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restricted_channels_are_not_pushed_or_notified_to_non_viewers() {
    use yangpa_core::ids::RoleId;
    use yangpa_core::model::{OverwriteTarget, PermissionOverwrite, Permissions};

    let cl = Cluster::new();
    let a = cl.node("a").await;
    let b = cl.node("b").await;
    let space = befriend(&a, &b).await;
    let open = a.first_channel(space).await;
    // 비공개 채널(@everyone 보기 거부): 채널 그룹이 없으면 공간 그룹 키로 내려가지 않고 보내기를 거절.
    let private = testkit::create_channel(
        &a.node,
        space,
        "비공개",
        vec![PermissionOverwrite {
            target: OverwriteTarget::Role(RoleId::EVERYONE),
            allow: Permissions::NONE,
            deny: Permissions::VIEW_CHANNEL,
        }],
    )
    .await
    .unwrap();
    let r = a
        .call::<m::MessageSend>(yangpa_rpc::api::MessageSendParams {
            space,
            channel: private,
            text: "비밀".into(),
            attachments: Vec::new(),
            reply_to: None,
            thread: None,
        })
        .await;
    // 커밋 확정 기기가 채널 그룹을 이미 만들었으면 보내지지만, 그때도 공간 그룹 키는 쓰지 않습니다.
    if r.is_ok() {
        let evs = testkit::stream_events(&a.node, &space, StreamId::Channel(private)).unwrap();
        assert!(
            evs.iter()
                .all(|e| !testkit::uses_crew_root(&a.node, &e.header)),
            "비공개 채널이 크루 그룹 키로 암호화됨"
        );
    }

    // B만 못 보는 채널: A의 메시지가 B에게 직접 가지 않고, 억지로 넣어도 알림이 나가지 않습니다.
    let hidden = testkit::create_channel(
        &a.node,
        space,
        "숨김",
        vec![PermissionOverwrite {
            target: OverwriteTarget::Member(b.user()),
            allow: Permissions::NONE,
            deny: Permissions::VIEW_CHANNEL,
        }],
    )
    .await
    .unwrap();
    eventually("B가 숨김 채널 설정을 앎", || async {
        testkit::space_state(&b.node, &space)?
            .channels
            .contains_key(&hidden)
            .then_some(())
    })
    .await;
    // 채널 그룹은 확정 기기가 묶음 대기(`commit_batch_window`) 뒤에 만들므로, 그 전에 보내면 `1010`입니다.
    eventually("A가 숨김 채널 그룹을 만듦", || async {
        testkit::channel_key_seq(&a.node, &space, hidden)
    })
    .await;
    let mut rx = b.subscribe();
    let secret = a.send(space, hidden, "B는 못 봄").await;
    let marker = a.send(space, open, "표시").await;
    wait_message(&b, space, open, marker, "B가 표시 메시지 받음", |_| true).await;
    assert!(
        testkit::get_event(&b.node, &secret).unwrap().is_none(),
        "숨김 채널 이벤트가 B에게 감"
    );

    let evs = testkit::stream_events(&a.node, &space, StreamId::Channel(hidden)).unwrap();
    testkit::ingest_from(&b.node, space, evs, a.peer())
        .await
        .unwrap();
    let marker2 = a.send(space, open, "표시2").await;
    next_notification(&mut rx, "표시2 알림", |n| match n {
        Notification::MessageNew { message, .. } if message.id == secret => {
            panic!("볼 수 없는 채널 메시지 알림")
        }
        Notification::MessageNew { message, .. } if message.id == marker2 => Some(()),
        _ => None,
    })
    .await;

    // 뿌리를 모르는 메시지도 볼 수 없는 채널에 쓴 것은 저장하지 않습니다(볼 수 있는 채널이면 저장해 뿌리를 기다림).
    let lamport = testkit::get_event(&a.node, &marker2)
        .unwrap()
        .unwrap()
        .header
        .lamport
        + 1;
    let blind =
        testkit::forge_message(&b.node, space, hidden, Vec::new(), lamport, "몰래", None).unwrap();
    let r = testkit::ingest_from(&a.node, space, vec![blind.clone()], b.peer())
        .await
        .unwrap();
    assert!(r.is_rejected(&blind.id()), "{r:?}");
    assert!(testkit::get_event(&a.node, &blind.id()).unwrap().is_none());
    let seen = testkit::forge_message(&b.node, space, open, Vec::new(), lamport, "뿌리 대기", None)
        .unwrap();
    let r = testkit::ingest_from(&a.node, space, vec![seen.clone()], b.peer())
        .await
        .unwrap();
    assert_eq!(r.waiting, 1, "{r:?}");
    assert!(testkit::get_event(&a.node, &seen.id()).unwrap().is_some());
}

// ───────────── 14. gossip 경로 ─────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gossip_delivery_and_allow_list_refresh_on_auth() {
    let cl = Cluster::new();
    let gossip = |c: &mut yangpa_daemon::NodeConfig| c.push_max_devices = 0;
    let a = cl.node_with("a", gossip).await;
    let b = cl.node_with("b", gossip).await;
    let space = befriend(&a, &b).await;
    let ch = a.first_channel(space).await;
    let m1 = a.send(space, ch, "gossip 1").await;
    wait_message(&b, space, ch, m1, "B가 gossip 1 받음", |_| true).await;
    let m2 = b.send(space, ch, "gossip 2").await;
    wait_message(&a, space, ch, m2, "A가 gossip 2 받음", |_| true).await;

    // B의 허용 목록이 A를 모를 때 세워졌다고 치고, A가 다시 인증하면 목록이 새로 채워져야 합니다.
    testkit::clear_gossip_peers(&b.node, space);
    testkit::reauth(&a.node, b.peer()).await.unwrap();
    // 인증 뒤 따라잡기가 끝나야 다음 메시지가 gossip으로만 옵니다.
    eventually("따라잡기 끝", || async {
        (testkit::catch_up_idle(&b.node) && testkit::catch_up_idle(&a.node)).then_some(())
    })
    .await;
    let m3 = a.send(space, ch, "gossip 3").await;
    wait_message(&b, space, ch, m3, "B가 gossip 3 받음", |_| true).await;
}

/// gossip 허용 목록(멤버 노드): 연결된 멤버 기기의 노드, 끊겼으면 그 기기가 묶인 노드 기록. 다른 공간에만 있는 친구의
/// 노드와 내 노드는 넣지 않습니다(기기마다 찾지 않고 아는 노드를 한 번 훑어 고름).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn member_peers_keep_offline_members_and_skip_strangers() {
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let b = cl.node("b").await;
    let c = cl.node("c").await;
    let ab = befriend(&a, &b).await;
    let ac = befriend(&a, &c).await;
    let peers = testkit::space_member_peers(&a.node, ab);
    assert_eq!(peers, vec![b.peer()]);
    assert_eq!(testkit::space_member_peers(&a.node, ac), vec![c.peer()]);
    b.shutdown().await;
    eventually("A가 B 연결 끊김을 앎", || async {
        (!testkit::is_authed(&a.node, b.peer())).then_some(())
    })
    .await;
    assert_eq!(
        testkit::space_member_peers(&a.node, ab),
        vec![b.peer()],
        "끊긴 멤버도 노드 기록으로 남음"
    );
}

/// gossip 판정: 지금 상태로 갈린 거부는 전달한 멤버를 감점하지 않습니다. 권한이 회수되기 직전에 쓴 메시지를
/// 회수를 아직 모르는 멤버가 전달하면 무시(`Ignore`)만 하고, 서명·본문이 틀린 것처럼 어느 상태에서도 틀린 것만
/// 거부(`Reject`, 전달한 피어 감점)합니다. (모든 거부를 `Reject`로 알리면 운영 조치 직후 정직한 중계 노드가
/// 회색 목록에 올라 큰 크루의 전파가 끊깁니다.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gossip_verdict_spares_relays_lagging_behind_state() {
    use yangpa_core::event::EventKind;
    use yangpa_core::model::Permissions;
    use yangpa_daemon::testkit::net as tnet;
    use yangpa_net::GossipVerdict;

    let cl = Cluster::new();
    let a = cl.node("a").await;
    let b = cl.node("b").await;
    let c = cl.node("c").await;
    // 연결 없이 각자 적용합니다: B는 권한 회수를 끝까지 받지 않습니다.
    let space = tnet::create_crew(&a.node, &[b.node.clone(), c.node.clone()])
        .await
        .expect("크루");
    let ch = c.first_channel(space).await;

    // B가 아직 보낼 수 있을 때 쓴 메시지(전파 중).
    let in_flight = testkit::send_unpropagated(&b.node, space, ch, "회수 직전")
        .await
        .expect("B의 메시지");
    let msg = in_flight
        .iter()
        .find(|e| e.header.kind == EventKind::Message)
        .expect("메시지 이벤트")
        .clone();

    // 소유자 A가 B의 SEND_MESSAGES를 막고, C는 그 상태 이벤트를 받습니다.
    tnet::deny_in_channel(&a.node, space, ch, b.user(), Permissions::SEND_MESSAGES)
        .await
        .expect("권한 회수");
    let state_events = testkit::stream_events(&a.node, &space, StreamId::Space).unwrap();
    testkit::ingest_from(&c.node, space, state_events, a.peer())
        .await
        .unwrap();
    let st = testkit::space_state(&c.node, &space).expect("C의 상태");
    assert!(
        !st.channel_permissions(&b.user(), &ch)
            .contains(Permissions::SEND_MESSAGES),
        "C가 권한 회수를 모름"
    );

    // 회수를 모르는 멤버(A의 노드라고 칩니다)가 B의 메시지를 전달: C는 받지 않되 전달한 멤버를 감점하지 않습니다.
    let v = tnet::gossip_verdict(&c.node, a.peer(), space, in_flight.clone()).await;
    assert_eq!(v, GossipVerdict::Ignore);
    assert!(!testkit::has_body(&c.node, &msg.id()));

    // 본문을 바꾼 이벤트는 어느 상태에서도 틀리므로 전달한 피어를 감점합니다.
    let tampered = testkit::tamper_body(&msg);
    let v = tnet::gossip_verdict(&c.node, a.peer(), space, vec![tampered]).await;
    assert_eq!(v, GossipVerdict::Reject);

    // 권한이 있는 멤버의 메시지는 받아 더 전달합니다.
    let ok = testkit::send_unpropagated(&a.node, space, ch, "정상")
        .await
        .expect("A의 메시지");
    let ok_id = ok
        .iter()
        .find(|e| e.header.kind == EventKind::Message)
        .expect("메시지 이벤트")
        .id();
    let v = tnet::gossip_verdict(&c.node, a.peer(), space, ok).await;
    assert_eq!(v, GossipVerdict::Accept);
    assert!(testkit::has_body(&c.node, &ok_id));
}

/// gossip으로 받은 상태 이벤트가 빠진 부모 때문에 지금 상태로 거부돼도(가입 기록보다 먼저 온 크루 키 갱신, 만든
/// 기록보다 먼저 온 채널 바꾸기) 전달한 피어에게서 빈 부모를 채워 다시 해소합니다. 판정은 무시(`Ignore`)라 더 퍼뜨리지
/// 않지만, 채우지 않으면 주기 따라잡기(5분)까지 멈춥니다(키 그룹 데몬 재기에서 찾음).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gossip_rejected_for_missing_parent_fills_the_gap() {
    use yangpa_core::body::EventBody;
    use yangpa_core::model::ChannelSettings;
    use yangpa_daemon::testkit::net as tnet;
    use yangpa_net::GossipVerdict;

    let cl = Cluster::new();
    let a = cl.node("a").await;
    let b = cl.node("b").await;
    let space = tnet::create_crew(&a.node, std::slice::from_ref(&b.node))
        .await
        .expect("크루");
    connect_both(&a, &b).await;
    testkit::authenticate(&b.node, a.peer())
        .await
        .expect("B → A 인증");
    testkit::authenticate(&a.node, b.peer())
        .await
        .expect("A → B 인증");
    eventually("연결 직후 따라잡기가 끝남", || async {
        (testkit::catch_up_idle(&a.node) && testkit::catch_up_idle(&b.node)).then_some(())
    })
    .await;

    // A에만 채널을 만들고 바꿉니다(전파하지 않음): 바꾸기의 부모가 만들기.
    let channel = ChannelId::random();
    let created = testkit::publish_state_unpropagated(
        &a.node,
        space,
        None,
        EventBody::ChannelCreate {
            channel,
            settings: ChannelSettings::text("빈 부모"),
        },
    )
    .await
    .expect("만들기");
    let updated = testkit::publish_state_unpropagated(
        &a.node,
        space,
        None,
        EventBody::ChannelUpdate {
            channel,
            settings: ChannelSettings::text("바꾼 이름"),
        },
    )
    .await
    .expect("바꾸기");
    assert!(updated.header.parents.contains(&created.id()));
    assert!(
        testkit::get_event(&b.node, &created.id())
            .unwrap()
            .is_none()
    );

    // 바꾸기만 gossip으로 받음: 모르는 채널이라 지금 상태로 거부(무시)하되, A에게서 만들기를 채워 둘 다 적용합니다.
    let v = tnet::gossip_verdict(&b.node, a.peer(), space, vec![updated]).await;
    assert_eq!(v, GossipVerdict::Ignore);
    eventually("B가 빈 부모를 채워 바꾸기까지 적용", || async {
        let st = testkit::space_state(&b.node, &space)?;
        (st.channels.get(&channel)?.name == "바꾼 이름").then_some(())
    })
    .await;
}

/// 실시간 전파(gossip·`PushEvents`)로 받은 상태 이벤트는 큰 크루에서 작성 기기마다 버스트(30)까지만 받습니다. 넘친
/// 것은 저장하지 않고 gossip이면 무시(`Ignore`, 전달한 피어 감점 없음)하며, 이미 가진 이벤트는 세지 않습니다. 제한
/// 없는 따라잡기로는 받으므로 버린 이벤트도 결국 들어옵니다. 소유자의 기기는 묶지 않고, 작은 크루(기본 256대 이하)는
/// 예전처럼 묶지 않습니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn live_state_events_are_rate_limited_per_author_device() {
    use yangpa_daemon::testkit::net as tnet;
    use yangpa_net::GossipVerdict;

    let cl = Cluster::new();
    let a = cl.node("a").await;
    let b = cl.node("b").await;
    // C는 큰 크루처럼 제한을 겁니다.
    let c = cl.node_with("c", |c| c.event_limit_devices = 0).await;
    let space = tnet::create_crew(&a.node, &[b.node.clone(), c.node.clone()])
        .await
        .expect("크루");
    // 평범한 멤버 B가 쏟아낸 상태 이벤트 40개(본문 없이 머리만이라 해소기는 순서 연결에만 씀).
    let evs = testkit::header_only_state_events(&b.node, space, 40).expect("B의 상태 이벤트");
    let (first, rest) = evs.split_at(30);

    // A의 노드가 전달했다고 칩니다. 버스트 안은 받고, 넘친 묶음은 무시합니다.
    let v = tnet::gossip_verdict(&c.node, a.peer(), space, first.to_vec()).await;
    assert_eq!(v, GossipVerdict::Accept);
    let v = tnet::gossip_verdict(&c.node, a.peer(), space, rest.to_vec()).await;
    assert_eq!(v, GossipVerdict::Ignore);
    assert!(
        rest.iter()
            .all(|e| testkit::get_event(&c.node, &e.id()).unwrap().is_none())
    );

    // 다시 받아도 가진 30개는 세지 않고(중복), 남은 10개는 여전히 버킷이 비어 버립니다.
    let r = testkit::ingest_live_from(&c.node, space, evs.clone(), b.peer())
        .await
        .unwrap();
    assert_eq!(r.duplicate, 30);
    assert_eq!(r.throttled.len(), 10);
    assert!(r.accepted.is_empty());

    // 제한 없는 따라잡기 길은 묶지 않습니다.
    let r = testkit::ingest_from(&c.node, space, rest.to_vec(), b.peer())
        .await
        .unwrap();
    assert_eq!(r.accepted.len(), 10);
    assert!(r.throttled.is_empty());

    // 소유자(A)의 기기는 묶지 않습니다.
    let own = testkit::header_only_state_events(&a.node, space, 40).expect("A의 상태 이벤트");
    let r = testkit::ingest_live_from(&c.node, space, own, a.peer())
        .await
        .unwrap();
    assert_eq!(r.accepted.len(), 40);
    assert!(r.throttled.is_empty());

    // 기본 설정인 A에게 이 크루(3대)는 작은 크루라 B의 40개를 모두 곧바로 받습니다.
    let r = testkit::ingest_live_from(&a.node, space, evs, b.peer())
        .await
        .unwrap();
    assert_eq!(r.accepted.len(), 40);
    assert!(r.throttled.is_empty());
}

/// 제한에 걸린(또는 아예 실시간으로 보내지 않은) 상태 이벤트를 자식 이벤트 하나로 끌어오지 못합니다. 곧바로 받은
/// 이벤트의 빈 부모를 채우는 따라잡기도 같은 작성 기기 버킷을 쓰고, 넘치면 거기서 멈춥니다. 나머지는 제한 없는
/// 따라잡기(연결·인증 직후, 주기)가 가져옵니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gap_catch_up_does_not_bypass_the_author_limit() {
    use yangpa_daemon::testkit::net as tnet;
    use yangpa_net::GossipVerdict;

    let cl = Cluster::new();
    let a = cl.node("a").await;
    let b = cl.node("b").await;
    let c = cl.node_with("c", |c| c.event_limit_devices = 0).await;
    let space = tnet::create_crew(&a.node, &[b.node.clone(), c.node.clone()])
        .await
        .expect("크루");
    connect_both(&b, &c).await;
    testkit::authenticate(&c.node, b.peer())
        .await
        .expect("C↔B 인증");
    // 인증 직후의 (제한 없는) 따라잡기가 끝난 뒤에 시작합니다.
    eventually("인증 뒤 따라잡기 끝", || async {
        (testkit::catch_up_idle(&b.node) && testkit::catch_up_idle(&c.node)).then_some(())
    })
    .await;

    // 평범한 멤버 B가 이어 붙인 상태 이벤트 41개를 B에만 두고, 끝 하나만 gossip으로 C에게 줍니다.
    let chain = testkit::header_only_state_chain(&b.node, space, 41)
        .await
        .expect("B의 사슬");
    let tip = chain.last().expect("끝 이벤트").clone();
    let v = tnet::gossip_verdict(&c.node, b.peer(), space, vec![tip]).await;
    assert_eq!(v, GossipVerdict::Accept);
    let have = |n: &TestNode| {
        chain
            .iter()
            .filter(|e| testkit::get_event(&n.node, &e.id()).unwrap().is_some())
            .count()
    };
    // 빈 부모 채우기는 버킷에 남은 29개까지만 받고 멈춥니다(끝 이벤트가 하나를 씀).
    eventually("빈 부모 채우기 끝", || async {
        testkit::catch_up_idle(&c.node).then_some(())
    })
    .await;
    assert_eq!(have(&c), 30, "빈 부모 채우기가 작성 기기 제한을 넘어 받음");

    // 제한 없는 따라잡기는 나머지를 가져옵니다.
    testkit::spawn_catch_up(&c.node, space, b.peer());
    eventually("나머지 따라잡기", || async {
        (have(&c) == 41).then_some(())
    })
    .await;
}

/// 초대를 받아 주는 멤버는 가입을 한꺼번에 내므로, 초대 증명이 맞는 `MemberJoin`은 보통 몫(30)을 다 쓴 뒤에도 따로
/// 200개까지 곧바로 받습니다(받는 쪽이 본문을 풀어 지금 상태의 초대로 증명을 확인). 증명이 틀린 가입과 가입이 아닌
/// 상태 이벤트는 그대로 묶입니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn invite_joins_from_an_accepting_member_get_a_larger_burst() {
    use yangpa_core::links::InviteLink;
    use yangpa_daemon::testkit::net as tnet;
    use yangpa_rpc::api::InviteCreateParams;

    let cl = Cluster::new();
    let a = cl.node("a").await;
    let b = cl.node("b").await;
    let c = cl.node_with("c", |c| c.event_limit_devices = 0).await;
    let space = tnet::create_crew(&a.node, &[b.node.clone(), c.node.clone()])
        .await
        .expect("크루");
    // 소유자 A가 만든 초대가 C의 상태에도 있습니다(따라잡기 길로 받아 제한과 무관).
    let url = a
        .ok::<m::InviteCreate>(InviteCreateParams {
            space,
            expires_in_secs: None,
            max_uses: None,
            roles: Vec::new(),
        })
        .await
        .url;
    let token = InviteLink::parse(&url).expect("초대 링크").token;
    let state_events = testkit::stream_events(&a.node, &space, StreamId::Space).unwrap();
    testkit::ingest_from(&c.node, space, state_events, a.peer())
        .await
        .unwrap();
    assert_eq!(
        testkit::space_state(&c.node, &space)
            .expect("C의 상태")
            .invites
            .len(),
        1
    );

    // 평범한 멤버 B가 받아 준 가입(증명이 맞음)과, 증명을 다른 키로 지어낸 가입, 가입이 아닌 상태 이벤트.
    let joins = testkit::invite_join_events(&b.node, space, &token, 205, false).expect("가입");
    let forged = testkit::invite_join_events(&b.node, space, &token, 3, true).expect("가짜 가입");
    let plain = testkit::header_only_state_events(&b.node, space, 33).expect("상태 이벤트");
    let (base, plain) = plain.split_at(30);
    // 버킷이 다시 차는 몫(6초에 하나)을 빼고 셉니다(느린 기기에서 시험이 길어져도 맞게).
    let t0 = std::time::Instant::now();
    let refill = || (t0.elapsed().as_secs() / 6) as usize + 1;
    let r = testkit::ingest_live_from(&c.node, space, base.to_vec(), b.peer())
        .await
        .unwrap();
    assert!(r.throttled.is_empty(), "보통 몫 30개");

    // 보통 몫을 다 쓴 뒤: 증명이 틀린 가입과 가입이 아닌 이벤트는 버립니다.
    let mut mixed = forged;
    mixed.extend_from_slice(plain);
    let r = testkit::ingest_live_from(&c.node, space, mixed, b.peer())
        .await
        .unwrap();
    assert!(
        r.throttled.len() + refill() > 6,
        "증명이 틀린 가입이 가입 몫을 씀: {}개만 버림",
        r.throttled.len()
    );

    // 증명이 맞는 가입은 따로 200개까지(넘친 5개는 버림). 가입도 보통 몫부터 쓰므로 두 버킷이 함께 다시 찬 만큼 더
    // 들어옵니다.
    let r = testkit::ingest_live_from(&c.node, space, joins, b.peer())
        .await
        .unwrap();
    assert!(
        r.throttled.len() <= 5 && r.throttled.len() + 2 * refill() > 5,
        "가입 205개 중 {}개를 버림(5개여야 함)",
        r.throttled.len()
    );
}

// ───────────── 15. 친구 흐름의 경쟁·유실 ─────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn friend_flow_survives_lost_responses_duplicates_and_forgeries() {
    use yangpa_core::links::FriendLink;

    let cl = Cluster::new();
    let a = cl.node("a").await;
    let b = cl.node("b").await;
    let c = cl.node("c").await;
    connect_both(&a, &b).await;
    connect_both(&c, &b).await;

    // 1. 응답을 잃고 같은 일회용 링크로 다시 보내도 같은 요청으로 봅니다(링크 사용을 다시 쓰지 않음).
    let link = a
        .ok::<m::FriendLinkCreate>(FriendLinkCreateParams {
            expires_in_secs: Some(3600),
            single_use: Some(true),
        })
        .await;
    b.ok::<m::FriendAdd>(UrlParams {
        url: link.url.clone(),
    })
    .await;
    b.ok::<m::FriendAdd>(UrlParams {
        url: link.url.clone(),
    })
    .await;
    let (a_user, b_user) = (a.user(), b.user());
    let space = eventually("A가 B를 연락처로 한 번만 기록", || async {
        let list = a.ok::<m::FriendList>(Empty).await;
        let mine: Vec<_> = list.iter().filter(|f| f.user == b_user).collect();
        (mine.len() == 1).then(|| mine[0].space)
    })
    .await;

    // 2. 인증 전에 오는 가짜 수락: 링크 주인이 아닌 C가 A인 척 만든 공간은 아무 흔적도 남기지 않습니다.
    let token = FriendLink::parse(&link.url).unwrap().token;
    let fake_space = SpaceId::random();
    let forged = testkit::forge_friend_space(&c.node, &b.node, fake_space, a.user()).unwrap();
    let r = testkit::net_request(
        &c.node,
        b.peer(),
        YpRequest::FriendAccepted {
            token,
            space: fake_space,
            events: forged.clone(),
            profile: None,
            contact: None,
        },
    )
    .await
    .unwrap();
    // 진짜 수락이 먼저 닿았으면 B는 그 요청을 이미 마쳐 모르는 요청으로 답합니다.
    assert!(
        is_error(&r, WireErrorCode::BadRequest) || is_error(&r, WireErrorCode::InvalidToken),
        "{r:?}"
    );
    assert!(!testkit::known_spaces(&b.node).contains(&fake_space));
    assert!(testkit::space_state(&b.node, &fake_space).is_none());
    for ev in &forged {
        assert!(testkit::get_event(&b.node, &ev.id()).unwrap().is_none());
    }

    // 3. 둘이 동시에 친구 추가를 눌러도 둘 다 친구가 되고 공간은 하나.
    eventually("B가 1:1 공간을 받음", || async {
        b.ok::<m::FriendList>(Empty)
            .await
            .into_iter()
            .any(|f| f.user == a_user && f.space == space)
            .then_some(())
    })
    .await;
    let propose = |n: &TestNode, user: UserId| {
        let node = n.node.clone();
        tokio::spawn(
            async move { common::call::<m::FriendPropose>(&node, UserParams { user }).await },
        )
    };
    let (r1, r2) = tokio::join!(propose(&a, b_user), propose(&b, a_user));
    r1.unwrap().unwrap();
    r2.unwrap().unwrap();
    for (n, other) in [(&a, b_user), (&b, a_user)] {
        eventually("둘 다 친구", || async {
            n.ok::<m::FriendList>(Empty)
                .await
                .into_iter()
                .any(|f| f.user == other && f.kind == ContactKind::Friend && !f.proposed)
                .then_some(())
        })
        .await;
    }
    let spaces = a.ok::<m::SpaceList>(Empty).await;
    assert_eq!(spaces.len(), 1, "{spaces:?}");
    assert!(a.ok::<m::FriendRequests>(Empty).await.is_empty());
    assert!(b.ok::<m::FriendRequests>(Empty).await.is_empty());

    // 4. 링크를 쓴 쪽이 요청을 잃었으면(수락을 모르는 요청으로 거절) 링크 주인은 연락처·공간을 되돌립니다.
    let link_c = a
        .ok::<m::FriendLinkCreate>(FriendLinkCreateParams {
            expires_in_secs: Some(3600),
            single_use: Some(true),
        })
        .await;
    connect_both(&a, &c).await;
    let c_user = c.user();
    let r = testkit::redeem_unrecorded(&c.node, &link_c.url)
        .await
        .unwrap();
    assert!(
        matches!(r, YpResponse::FriendPending | YpResponse::Ok),
        "{r:?}"
    );
    eventually("A가 받지 않은 연락처·공간을 되돌림", || async {
        let friends = a.ok::<m::FriendList>(Empty).await;
        (!friends.iter().any(|f| f.user == c_user) && testkit::known_spaces(&a.node) == vec![space])
            .then_some(())
    })
    .await;
}

// ───────────── 16. 신원·종료 경쟁, 읽음 위치 ─────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn identity_races_shutdown_and_foreign_read_markers() {
    let cl = Cluster::new();
    // 동시에 신원을 만들면 하나만 성공하고, 다시 시작해도 그 신원이 그대로 돌아옵니다.
    let mut n = cl.bare("n").await;
    let create = |name: &str| {
        let node = n.node.clone();
        let name = name.to_owned();
        tokio::spawn(async move {
            common::call::<m::IdentityCreate>(&node, IdentityCreateParams { name }).await
        })
    };
    let (r1, r2) = tokio::join!(create("하나"), create("둘"));
    let (r1, r2) = (r1.unwrap(), r2.unwrap());
    assert!(r1.is_ok() != r2.is_ok(), "{r1:?} {r2:?}");
    let created = r1.or(r2).unwrap().user;
    // 종료를 동시에 불러도 모두 끝까지 기다립니다.
    let (n1, n2) = (n.node.clone(), n.node.clone());
    let (x, y) = tokio::join!(
        tokio::spawn(async move { n1.shutdown().await }),
        tokio::spawn(async move { n2.shutdown().await })
    );
    x.unwrap();
    y.unwrap();
    n.start_again().await;
    assert_eq!(n.user(), created);

    // Tor를 준비할 수 없으면(Arti 상태 경로 자리에 파일) 직접 연결로 조용히 뜨지 않고 시작을 멈춥니다.
    let tor_dir = cl.dir("tor");
    std::fs::create_dir_all(&tor_dir).unwrap();
    std::fs::write(tor_dir.join("arti"), b"x").unwrap();
    let mut cfg = yangpa_daemon::NodeConfig::for_test(&tor_dir);
    cfg.net.direct = false;
    assert!(yangpa_daemon::Node::start(cfg).await.is_err());

    // 읽음 위치·페이지 기준은 그 채널의 메시지만.
    let a = cl.node("a").await;
    let b = cl.node("b").await;
    let c = cl.node("c").await;
    let s1 = befriend(&a, &b).await;
    let s2 = befriend(&a, &c).await;
    let ch1 = a.first_channel(s1).await;
    let ch2 = a.first_channel(s2).await;
    let foreign = a.send(s2, ch2, "다른 공간").await;
    let own = a.send(s1, ch1, "이 공간").await;
    let r = a
        .call::<m::ChannelMarkRead>(MarkReadParams {
            space: s1,
            channel: ch1,
            event: Some(foreign),
        })
        .await;
    assert_eq!(r.unwrap_err().code, ERR_NOT_FOUND);
    let r = a
        .call::<m::MessageList>(MessageListParams {
            space: s1,
            channel: ch1,
            before: Some(foreign),
            limit: Some(10),
            thread: None,
        })
        .await;
    assert_eq!(r.unwrap_err().code, ERR_NOT_FOUND);
    a.ok::<m::ChannelMarkRead>(MarkReadParams {
        space: s1,
        channel: ch1,
        event: Some(own),
    })
    .await;
}
