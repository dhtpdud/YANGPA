//! 표시 상태 통합 시험: 숨기(동기화는 그대로, 상대 화면엔 오프라인), 자리 비움, 마지막 동기화 시각과 비공개, 기간 끝,
//! 오프라인 모드(망 없이 다시 뜨고, 돌아오면 다시 동기화).

use crate::common;

use common::{Cluster, TestNode, befriend, eventually, next_notification};
use yangpa_core::ids::{EventId, SpaceId, UserId};
use yangpa_rpc::api::{
    Empty, FriendView, Notification, PresenceMode, PresenceSetParams, SpaceParams, m,
};

async fn friend_of(n: &TestNode, user: UserId) -> Option<FriendView> {
    n.ok::<m::FriendList>(Empty)
        .await
        .into_iter()
        .find(|f| f.user == user)
}

async fn set(n: &TestNode, p: PresenceSetParams) {
    n.ok::<m::PresenceSet>(p).await;
}

fn mode(mode: PresenceMode) -> PresenceSetParams {
    PresenceSetParams {
        mode: Some(mode),
        ..Default::default()
    }
}

async fn wait_shown(
    n: &TestNode,
    user: UserId,
    online: bool,
    away: bool,
    what: &str,
) -> FriendView {
    eventually(what, || async {
        friend_of(n, user)
            .await
            .filter(|f| f.online == online && f.away == away)
    })
    .await
}

async fn send(n: &TestNode, space: SpaceId, text: &str) -> EventId {
    let channel = n.first_channel(space).await;
    n.send(space, channel, text).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn invisible_away_and_hidden_last_seen() {
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let b = cl.node("b").await;
    let space = befriend(&a, &b).await;
    wait_shown(&a, b.user(), true, false, "A가 B를 온라인으로 봄").await;

    // 숨기: A 화면에는 오프라인, 그 순간이 마지막 동기화 시각. 대화는 그대로 오갑니다. 시각은 알림에서 받습니다(목록은
    // 시각을 남기기 직전에도 오프라인으로 보일 수 있고, 알림은 남긴 뒤 나감).
    let mut rx = a.subscribe();
    set(&b, mode(PresenceMode::Invisible)).await;
    let seen = next_notification(&mut rx, "숨긴 B가 A에게 오프라인", |n| match n {
        Notification::Presence {
            user,
            online: false,
            last_seen,
            ..
        } if *user == b.user() => *last_seen,
        _ => None,
    })
    .await;
    let hidden = wait_shown(&a, b.user(), false, false, "숨긴 B가 A 목록에서 오프라인").await;
    assert_eq!(hidden.last_seen, Some(seen), "숨기 전까지의 마지막 동기화");
    let id = send(&b, space, "숨긴 채로 보냄").await;
    let channel = a.first_channel(space).await;
    common::wait_message(&a, space, channel, id, "숨긴 B의 글이 A에 옴", |_| true).await;
    let still = friend_of(&a, b.user()).await.expect("B");
    assert!(!still.online);
    assert_eq!(still.last_seen, Some(seen), "숨긴 동안에는 시각이 멈춤");

    // 자리 비움(자동 판단의 입력 없음도 같은 길).
    set(&b, mode(PresenceMode::Online)).await;
    wait_shown(&a, b.user(), true, false, "B가 다시 온라인").await;
    set(
        &b,
        PresenceSetParams {
            idle: Some(true),
            ..Default::default()
        },
    )
    .await;
    wait_shown(&a, b.user(), true, true, "입력 없는 B는 자리 비움").await;

    // 비공개: A 화면에서 시각이 빠집니다.
    set(
        &b,
        PresenceSetParams {
            hide_last_seen: Some(true),
            ..Default::default()
        },
    )
    .await;
    eventually("B 시각이 비공개", || async {
        friend_of(&a, b.user())
            .await
            .filter(|f| f.last_seen.is_none())
    })
    .await;

    // 나는 내 표시 상태로 보입니다.
    let me = b
        .ok::<m::SpaceGet>(SpaceParams { space })
        .await
        .members
        .into_iter()
        .find(|m| m.user == b.user())
        .expect("나");
    assert!(me.online && me.away);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn timed_mode_ends_and_bad_duration_is_refused() {
    let cl = Cluster::new();
    let a = cl.node("a").await;
    set(
        &a,
        PresenceSetParams {
            mode: Some(PresenceMode::Away),
            duration_secs: Some(1),
            ..Default::default()
        },
    )
    .await;
    let v = a.ok::<m::PresenceGet>(Empty).await;
    assert_eq!(v.mode, PresenceMode::Away);
    assert!(v.until.is_some());
    eventually("기간이 끝나 온라인", || async {
        (a.ok::<m::PresenceGet>(Empty).await.mode == PresenceMode::Online).then_some(())
    })
    .await;
    let err = a
        .call::<m::PresenceSet>(PresenceSetParams {
            mode: Some(PresenceMode::Invisible),
            duration_secs: Some(0),
            ..Default::default()
        })
        .await
        .expect_err("0초는 안 됨");
    assert_eq!(
        (err.code, err.message.as_str()),
        (1005, "기간은 1초에서 365일 사이여야 합니다")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn offline_mode_restarts_without_network_and_resyncs() {
    let cl = Cluster::new();
    let mut a = cl.node("a").await;
    let b = cl.node("b").await;
    let space = befriend(&a, &b).await;
    wait_shown(&b, a.user(), true, false, "B가 A를 온라인으로 봄").await;

    // 오프라인으로 바꾸면 데몬이 멈추고(다시 띄울 것을 알림) 망 없이 다시 뜹니다.
    set(&a, mode(PresenceMode::Offline)).await;
    eventually("A가 멈춤", || async {
        a.node.is_shutting_down().then_some(())
    })
    .await;
    assert!(a.node.restart_requested());
    a.shutdown().await;
    a.start_again().await;
    let v = a.ok::<m::PresenceGet>(Empty).await;
    assert!(v.offline && v.mode == PresenceMode::Offline);
    assert!(a.ok::<m::NetStatusGet>(Empty).await.offline);
    assert!(a.node.listen_addrs().await.is_empty());
    wait_shown(&b, a.user(), false, false, "A가 B에게 오프라인").await;

    // 오프라인에서도 쓸 수 있고, 망을 쓰는 호출은 막힙니다.
    let id = send(&a, space, "오프라인에서 씀").await;
    let err = a
        .call::<m::SupporterRefresh>(Empty)
        .await
        .expect_err("오프라인에서는 막힘");
    assert_eq!(
        (err.code, err.message.as_str()),
        (1009, "오프라인 모드에서는 할 수 없습니다")
    );

    // 돌아오면 다시 띄우고, 다시 이어져 그동안 쓴 글이 상대에게 갑니다.
    set(&a, mode(PresenceMode::Online)).await;
    eventually("A가 다시 멈춤", || async {
        a.node.is_shutting_down().then_some(())
    })
    .await;
    assert!(a.node.restart_requested());
    a.shutdown().await;
    a.start_again().await;
    assert!(!a.ok::<m::PresenceGet>(Empty).await.offline);
    let channel = b.first_channel(space).await;
    common::wait_message(
        &b,
        space,
        channel,
        id,
        "오프라인에서 쓴 글이 B에 옴",
        |_| true,
    )
    .await;
    wait_shown(&b, a.user(), true, false, "A가 다시 온라인").await;
}
