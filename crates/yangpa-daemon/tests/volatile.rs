//! 휘발성 채널(B2) 통합 시험: 입장·퇴장과 세션 키 갱신, 수명 만료, 퇴장 시 삭제, 재시작, 늦은 입장자 스냅숏,
//! 자동화 토큰 읽기 거부, 누출 검사·진단 지표, 참가자가 아닌 기기의 이벤트·키 묶음 거르기, 입장 알림 유실,
//! 스냅숏 논리 시계 상한.
//!
//! 세 노드를 한 프로세스에 띄워 크루 하나를 함께 쓰고(`testkit::volatile::create_group_space`), 고정 sleep 없이
//! 조건이 될 때까지 기다립니다.

use crate::common;

use std::collections::BTreeSet;
use std::sync::Arc;

use common::{Cluster, TestNode, connect_both, eventually, next_notification, wait_message};
use serde_json::json;
use yangpa_core::body::{Secret32, SharedKey};
use yangpa_core::event::KeyRef;
use yangpa_core::ids::{ChannelId, EventId, SpaceId, UserId};
use yangpa_core::model::{ChannelSettings, ChatTtl, HistoryVisibility, JoinMode, SecurityLevel};
use yangpa_core::wire::{WireErrorCode, YpRequest, YpResponse};
use yangpa_daemon::testkit;
use yangpa_daemon::testkit::ShareVia;
use yangpa_daemon::testkit::volatile as vt;
use yangpa_daemon::{ApiHandler, CallCtx, Node};
use yangpa_rpc::api::{
    AllowApiReadParams, ChannelUpdateParams, ChannelView, Empty, LeakcheckParams, MarkReadParams,
    MessageEditParams, MessageListResult, MessageRefParams, MessageSendParams, Notification,
    ReactParams, Scope, SpaceChannelParams, SpaceParams, m,
};
use yangpa_rpc::jsonrpc::{ERR_CONFLICT, ERR_FORBIDDEN, ERR_VOLATILE_READ_DENIED};
use yangpa_rpc::{AuthInfo, RpcHandler};

/// 세 사람 크루. 노드를 먼저 두어 시험 경로(`Cluster`)보다 먼저 정리되게 합니다.
struct Crew {
    nodes: Vec<TestNode>,
    space: SpaceId,
    _cluster: Cluster,
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
}

/// 서로 연결·인증한 세 노드와 `a`가 만든 크루.
async fn crew() -> Crew {
    let cluster = Cluster::new();
    let mut nodes = Vec::new();
    for name in ["a", "b", "c"] {
        nodes.push(cluster.node(name).await);
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
        _cluster: cluster,
    }
}

fn settings(
    name: &str,
    mode: JoinMode,
    ttl: ChatTtl,
    history: HistoryVisibility,
) -> ChannelSettings {
    let mut s = ChannelSettings::text(name);
    s.security = SecurityLevel::Volatile;
    s.join_mode = mode;
    s.ttl = ttl;
    s.history = history;
    s.link_previews = false;
    s
}

/// 휘발성 채널을 만들고 세 노드 모두에 보일 때까지 기다립니다.
async fn volatile_channel(c: &Crew, s: ChannelSettings) -> ChannelId {
    let ch = vt::create_channel_with(&c.a().node, c.space, s)
        .await
        .expect("채널 만들기");
    for n in c.all() {
        channel_view_eventually(n, c.space, ch).await;
    }
    ch
}

async fn channel_view(n: &TestNode, space: SpaceId, ch: ChannelId) -> Option<ChannelView> {
    n.ok::<m::ChannelList>(SpaceParams { space })
        .await
        .into_iter()
        .find(|v| v.channel == ch)
}

async fn channel_view_eventually(n: &TestNode, space: SpaceId, ch: ChannelId) -> ChannelView {
    eventually("채널이 보임", || channel_view(n, space, ch)).await
}

async fn join(n: &TestNode, space: SpaceId, channel: ChannelId) {
    n.ok::<m::ChannelJoin>(SpaceChannelParams { space, channel })
        .await;
}

async fn leave(n: &TestNode, space: SpaceId, channel: ChannelId) {
    n.ok::<m::ChannelLeave>(SpaceChannelParams { space, channel })
        .await;
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

/// 주어진 노드들이 같은 세션 키 세대로 보내게 될 때까지 기다립니다.
async fn converged(nodes: &[&TestNode], space: SpaceId, ch: ChannelId) -> u64 {
    eventually("세션 키 세대가 맞춰짐", || async {
        let gens: Vec<Option<u64>> = nodes
            .iter()
            .map(|n| vt::volatile_current_generation(&n.node, &space, &ch))
            .collect();
        let first = gens[0]?;
        gens.iter().all(|g| *g == Some(first)).then_some(first)
    })
    .await
}

fn users(nodes: &[&TestNode]) -> BTreeSet<UserId> {
    nodes.iter().map(|n| n.user()).collect()
}

fn generation_of(n: &TestNode, space: SpaceId, ch: ChannelId, id: EventId) -> u64 {
    let ev = vt::volatile_raw_events(&n.node, &space, &ch)
        .into_iter()
        .find(|e| e.id() == id)
        .expect("RAM에 이벤트가 있음");
    match ev.header.key {
        KeyRef::Volatile { generation } => generation,
        other => panic!("세션 키가 아님: {other:?}"),
    }
}

fn marker(prefix: &str) -> String {
    format!(
        "{prefix}-{:016x}{:016x}",
        rand::random::<u64>(),
        rand::random::<u64>()
    )
}

async fn leakcheck(n: &TestNode, marker: &str) -> yangpa_rpc::api::LeakcheckResult {
    n.ok::<m::DiagLeakcheck>(LeakcheckParams {
        marker: marker.into(),
    })
    .await
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn join_leave_rotates_session_key_and_leave_wipes() {
    let c = crew().await;
    let (a, b, cc) = (c.a(), c.b(), c.c());
    let ch = volatile_channel(
        &c,
        settings(
            "입퇴장",
            JoinMode::JoinLeave,
            ChatTtl::None,
            HistoryVisibility::Private,
        ),
    )
    .await;
    // 들어가기 전에는 보낼 수 없고, 채널 보기에도 들어가지 않았다고 나옵니다.
    let err = a
        .call::<m::MessageSend>(send_params(c.space, ch, "아직"))
        .await
        .unwrap_err();
    assert_eq!(err.code, ERR_CONFLICT);
    assert_eq!(
        channel_view_eventually(a, c.space, ch).await.joined,
        Some(false)
    );

    for n in [a, b, cc] {
        join(n, c.space, ch).await;
    }
    let g1 = converged(&[a, b, cc], c.space, ch).await;
    let view = channel_view_eventually(a, c.space, ch).await;
    assert_eq!(view.joined, Some(true));
    assert_eq!(
        view.participants
            .unwrap_or_default()
            .into_iter()
            .collect::<BTreeSet<_>>(),
        users(&[a, b, cc])
    );

    let m1 = a.send(c.space, ch, "모두 안녕").await;
    for n in [b, cc] {
        wait_message(n, c.space, ch, m1, "첫 메시지", |v| {
            v.text == "모두 안녕" && v.volatile && v.expires_at.is_none()
        })
        .await;
    }
    let c_gens: BTreeSet<u64> = vt::volatile_generations(&cc.node, &c.space, &ch)
        .into_iter()
        .collect();
    assert!(c_gens.contains(&g1), "c가 지금 세대 키를 가짐");

    // c 퇴장: c 기기의 내용·키가 바로 지워지고 만료 알림이 나갑니다.
    let mut rx_c = cc.subscribe();
    leave(cc, c.space, ch).await;
    assert!(vt::volatile_raw_events(&cc.node, &c.space, &ch).is_empty());
    assert!(!vt::volatile_has_key(&cc.node, &c.space, &ch));
    assert!(vt::volatile_generations(&cc.node, &c.space, &ch).is_empty());
    assert!(cc.messages(c.space, ch).await.is_empty());
    let expired = next_notification(&mut rx_c, "퇴장 때 만료 알림", |n| match n {
        Notification::MessageExpired { channel, ids, .. } if *channel == ch => Some(ids.clone()),
        _ => None,
    })
    .await;
    assert!(expired.contains(&m1));

    // 남은 a·b는 c가 받은 적 없는 새 세대로 맞춥니다.
    let g2 = eventually("c 없는 새 세대", || async {
        let ga = vt::volatile_current_generation(&a.node, &c.space, &ch)?;
        let gb = vt::volatile_current_generation(&b.node, &c.space, &ch)?;
        (ga == gb && !c_gens.contains(&ga)).then_some(ga)
    })
    .await;
    assert_ne!(g1, g2);
    let view = channel_view_eventually(b, c.space, ch).await;
    assert_eq!(
        view.participants
            .unwrap_or_default()
            .into_iter()
            .collect::<BTreeSet<_>>(),
        users(&[a, b])
    );

    let m2 = a.send(c.space, ch, "c 없이").await;
    wait_message(b, c.space, ch, m2, "퇴장 뒤 메시지", |v| {
        v.text == "c 없이"
    })
    .await;
    let used = generation_of(b, c.space, ch, m2);
    assert!(
        !c_gens.contains(&used),
        "퇴장한 기기가 가진 키로 암호화하지 않음"
    );

    // c에게는 가지 않았고, 억지로 넣어도(들어가 있지 않으니) 받지 않습니다.
    assert!(vt::volatile_raw_events(&cc.node, &c.space, &ch).is_empty());
    let ev = vt::volatile_raw_events(&b.node, &c.space, &ch)
        .into_iter()
        .find(|e| e.id() == m2)
        .expect("b의 RAM에 있음");
    testkit::ingest_from(&cc.node, c.space, vec![ev], a.peer())
        .await
        .expect("적용 경로");
    assert!(vt::volatile_raw_events(&cc.node, &c.space, &ch).is_empty());
    assert!(cc.messages(c.space, ch).await.is_empty());
    // 휘발성 이벤트는 디스크 저장소에 들어가지 않습니다.
    assert!(!testkit::has_body(&b.node, &m2));
    assert!(testkit::get_event(&b.node, &m2).expect("조회").is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ttl_expires_on_every_node_with_notification() {
    let c = crew().await;
    let ch = volatile_channel(
        &c,
        settings(
            "수명",
            JoinMode::Always,
            ChatTtl::Seconds(2),
            HistoryVisibility::Private,
        ),
    )
    .await;
    let mut rxs: Vec<_> = c.all().iter().map(|n| n.subscribe()).collect();
    converged(&c.all(), c.space, ch).await;
    let id = c.a().send(c.space, ch, "곧 사라짐").await;
    for rx in rxs.iter_mut() {
        let (message, notify) = next_notification(rx, "새 메시지", |n| match n {
            Notification::MessageNew {
                message, notify, ..
            } if message.id == id => Some((message.clone(), notify.clone())),
            _ => None,
        })
        .await;
        assert!(message.volatile);
        assert!(message.expires_at.is_some(), "수명이 있음");
        // OS 알림에는 내용·작성자가 없습니다.
        assert!(notify.body.is_none());
        assert_eq!(notify.title, "새 메시지");
    }
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
        assert!(n.messages(c.space, ch).await.iter().all(|v| v.id != id));
        assert!(
            vt::volatile_raw_events(&n.node, &c.space, &ch)
                .iter()
                .all(|e| e.id() != id)
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn on_leave_ttl_removes_leavers_messages_elsewhere() {
    let c = crew().await;
    let (a, b, cc) = (c.a(), c.b(), c.c());
    let ch = volatile_channel(
        &c,
        settings(
            "퇴장 시 삭제",
            JoinMode::JoinLeave,
            ChatTtl::OnLeave,
            HistoryVisibility::Private,
        ),
    )
    .await;
    for n in [a, b, cc] {
        join(n, c.space, ch).await;
    }
    converged(&[a, b, cc], c.space, ch).await;
    let from_a = a.send(c.space, ch, "a가 남김").await;
    let from_b = b.send(c.space, ch, "b가 남김").await;
    for n in [a, b, cc] {
        for id in [from_a, from_b] {
            wait_message(n, c.space, ch, id, "두 메시지", |_| true).await;
        }
    }
    let mut rx = cc.subscribe();
    leave(b, c.space, ch).await;
    next_notification(&mut rx, "b 메시지 만료", |n| match n {
        Notification::MessageExpired { ids, .. } if ids.contains(&from_b) => Some(()),
        _ => None,
    })
    .await;
    for n in [a, cc] {
        eventually("b의 메시지만 사라짐", || async {
            let ids: Vec<EventId> = n.messages(c.space, ch).await.iter().map(|v| v.id).collect();
            (ids.contains(&from_a) && !ids.contains(&from_b)).then_some(())
        })
        .await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn edit_react_delete_and_read_marker_stay_in_ram() {
    let c = crew().await;
    let (a, b, cc) = (c.a(), c.b(), c.c());
    let ch = volatile_channel(
        &c,
        settings(
            "고치기",
            JoinMode::Always,
            ChatTtl::None,
            HistoryVisibility::Private,
        ),
    )
    .await;
    converged(&[a, b, cc], c.space, ch).await;
    let id = a.send(c.space, ch, "처음 글").await;
    wait_message(cc, c.space, ch, id, "처음 글", |_| true).await;
    assert_eq!(channel_view_eventually(cc, c.space, ch).await.unread, 1);
    // B도 받은 뒤에 반응합니다(C보다 늦게 닿을 수 있음).
    wait_message(b, c.space, ch, id, "처음 글", |_| true).await;

    b.ok::<m::MessageReact>(ReactParams {
        space: c.space,
        channel: ch,
        id,
        emoji: "👍".into(),
    })
    .await;
    a.ok::<m::MessageEdit>(MessageEditParams {
        space: c.space,
        channel: ch,
        id,
        text: "고친 글".into(),
    })
    .await;
    // 남의 메시지는 고칠 수 없습니다.
    let err = b
        .call::<m::MessageEdit>(MessageEditParams {
            space: c.space,
            channel: ch,
            id,
            text: "남이 고침".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(err.code, ERR_FORBIDDEN);
    wait_message(cc, c.space, ch, id, "고침·반응", |v| {
        v.text == "고친 글"
            && v.edited_at.is_some()
            && v.reactions.iter().any(|r| r.emoji == "👍" && r.count == 1)
    })
    .await;

    // 읽음 위치는 RAM에만 둡니다.
    cc.ok::<m::ChannelMarkRead>(MarkReadParams {
        space: c.space,
        channel: ch,
        event: Some(id),
    })
    .await;
    assert_eq!(channel_view_eventually(cc, c.space, ch).await.unread, 0);

    let mut rx = cc.subscribe();
    a.ok::<m::MessageDelete>(MessageRefParams {
        space: c.space,
        channel: ch,
        id,
    })
    .await;
    next_notification(&mut rx, "삭제 알림", |n| match n {
        Notification::MessageUpdated { message, .. } if message.id == id && message.deleted => {
            Some(())
        }
        _ => None,
    })
    .await;
    for n in [a, b, cc] {
        eventually("삭제가 퍼짐", || async {
            n.messages(c.space, ch)
                .await
                .iter()
                .all(|v| v.id != id)
                .then_some(())
        })
        .await;
        // 메시지와 그에 딸린 수정·반응 암호문도 RAM에서 지워집니다.
        eventually("RAM에서도 지워짐", || async {
            vt::volatile_raw_events(&n.node, &c.space, &ch)
                .is_empty()
                .then_some(())
        })
        .await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restart_leaves_nothing() {
    let mut c = crew().await;
    let space = c.space;
    let ch = volatile_channel(
        &c,
        settings(
            "재시작",
            JoinMode::Always,
            ChatTtl::None,
            HistoryVisibility::Private,
        ),
    )
    .await;
    converged(&c.all(), space, ch).await;
    let text = marker("RESTART");
    let id = c.a().send(space, ch, &text).await;
    for n in [c.b(), c.c()] {
        wait_message(n, space, ch, id, "재시작 전", |v| v.text == text).await;
    }

    let old_gens = vt::volatile_generations(&c.b().node, &space, &ch);
    assert!(!old_gens.is_empty());
    // 끈 노드의 RAM에도 암호문·세션 키가 남지 않습니다.
    let old = c.b().node.clone();
    c.nodes[1].shutdown().await;
    assert!(vt::volatile_raw_events(&old, &space, &ch).is_empty());
    assert!(!vt::volatile_has_key(&old, &space, &ch));
    drop(old);

    c.nodes[1].start_again().await;
    let b = &c.nodes[1];
    assert!(vt::volatile_raw_events(&b.node, &space, &ch).is_empty());
    assert!(b.messages(space, ch).await.is_empty());
    assert!(leakcheck(b, &text).await.clean, "디스크에 남지 않음");

    // 다시 연결돼 참가자로 돌아와도(비공개 기록) 예전 세대 키·예전 메시지는 오지 않습니다.
    converged(&[c.a(), b], space, ch).await;
    let after = c.a().send(space, ch, "재시작 뒤").await;
    wait_message(b, space, ch, after, "재시작 뒤 메시지", |_| true).await;
    assert!(b.messages(space, ch).await.iter().all(|v| v.id != id));
    let now = vt::volatile_generations(&b.node, &space, &ch);
    assert!(
        old_gens.iter().all(|g| !now.contains(g)),
        "{old_gens:?} / {now:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn late_joiner_gets_snapshot_only_when_history_is_public() {
    let c = crew().await;
    let (a, b, cc) = (c.a(), c.b(), c.c());
    let public = volatile_channel(
        &c,
        settings(
            "공개",
            JoinMode::JoinLeave,
            ChatTtl::Seconds(3600),
            HistoryVisibility::Public,
        ),
    )
    .await;
    let private = volatile_channel(
        &c,
        settings(
            "비공개",
            JoinMode::JoinLeave,
            ChatTtl::None,
            HistoryVisibility::Private,
        ),
    )
    .await;
    let mut before = Vec::new();
    for ch in [public, private] {
        join(a, c.space, ch).await;
        join(b, c.space, ch).await;
        converged(&[a, b], c.space, ch).await;
        let id = a.send(c.space, ch, "입장 전 기록").await;
        wait_message(b, c.space, ch, id, "입장 전 기록", |_| true).await;
        before.push(id);
    }

    // 비공개 기록 채널에는 스냅숏을 주지 않습니다.
    join(cc, c.space, private).await;
    let resp = testkit::net_request(
        &cc.node,
        a.peer(),
        YpRequest::VolatileSnapshot {
            space: c.space,
            channel: private,
        },
    )
    .await
    .expect("요청");
    assert!(matches!(
        resp,
        YpResponse::Error {
            code: WireErrorCode::HistoryPrivate,
            ..
        }
    ));

    // 공개 채널: 입장하면서 남은 수명과 함께 받습니다.
    let mut rx = cc.subscribe();
    join(cc, c.space, public).await;
    let got = wait_message(cc, c.space, public, before[0], "스냅숏", |v| {
        v.text == "입장 전 기록"
    })
    .await;
    assert!(got.expires_at.is_some(), "남은 수명을 이어받음");

    // 비공개 채널: 입장 뒤 메시지만. 입장할 때 갱신한 키만 받고 입장 전 세대 키는 받지 않습니다.
    converged(&[a, b, cc], c.space, private).await;
    let after = a.send(c.space, private, "입장 뒤").await;
    wait_message(cc, c.space, private, after, "입장 뒤 메시지", |_| {
        true
    })
    .await;
    let old_gen = generation_of(b, c.space, private, before[1]);
    assert_ne!(old_gen, generation_of(cc, c.space, private, after));
    assert!(
        !vt::volatile_generations(&cc.node, &c.space, &private).contains(&old_gen),
        "입장 전 세대 키가 새 참가자에게 가지 않음"
    );
    assert!(
        vt::volatile_raw_events(&cc.node, &c.space, &private)
            .iter()
            .all(|e| e.id() != before[1])
    );
    // 입장 전 암호문을 손에 넣어도(참가자가 다시 보내도) 풀 수 없어 보이지 않습니다.
    let old = vt::volatile_raw_events(&b.node, &c.space, &private)
        .into_iter()
        .find(|e| e.id() == before[1])
        .expect("b의 RAM에 있음");
    testkit::ingest_from(&cc.node, c.space, vec![old], a.peer())
        .await
        .expect("적용 경로");
    assert!(
        cc.messages(c.space, private)
            .await
            .iter()
            .all(|v| v.id != before[1])
    );
    assert!(!vt::volatile_generations(&cc.node, &c.space, &private).contains(&old_gen));
    // 스냅숏으로 받은 메시지는 새 메시지 알림을 내지 않습니다(입장 뒤 메시지는 알림).
    let first_new = next_notification(&mut rx, "입장 뒤 새 메시지 알림", |n| match n {
        Notification::MessageNew { message, .. } => Some(message.id),
        _ => None,
    })
    .await;
    assert_eq!(first_new, after);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn automation_token_needs_explicit_api_read() {
    let c = crew().await;
    let (a, b, cc) = (c.a(), c.b(), c.c());
    let ch = volatile_channel(
        &c,
        settings(
            "자동화",
            JoinMode::Always,
            ChatTtl::None,
            HistoryVisibility::Private,
        ),
    )
    .await;
    converged(&[a, b, cc], c.space, ch).await;
    let mut rx = a.subscribe();
    let id = b.send(c.space, ch, "봇이 읽으면 안 됨").await;
    let raw = next_notification(&mut rx, "새 메시지", |n| match n {
        Notification::MessageNew { message, .. } if message.id == id => Some(n.clone()),
        _ => None,
    })
    .await;

    let bot = automation();
    let auth = bot.auth().expect("인증 정보").clone();
    let handler = ApiHandler::new(a.node.clone());
    let list = json!({ "space": c.space, "channel": ch });

    // 허용 전: 목록은 1007, 알림은 내용 없이.
    let err = a
        .node
        .call(&bot, "message.list", list.clone())
        .await
        .unwrap_err();
    assert_eq!(err.code, ERR_VOLATILE_READ_DENIED);
    match handler.filter_notification(&auth, &raw) {
        Some(Notification::MessageNew {
            message, notify, ..
        }) => {
            assert!(message.text.is_empty());
            assert!(notify.body.is_none());
        }
        other => panic!("내용 없는 알림이어야 함: {other:?}"),
    }
    // 자동화 토큰은 스스로 허용할 수 없습니다(사람·소유자만).
    let allow = json!({ "space": c.space, "channel": ch, "allowed": true });
    assert!(
        a.node
            .call(&bot, "channel.allow_api_read", allow)
            .await
            .is_err()
    );
    assert!(
        !channel_view_eventually(a, c.space, ch)
            .await
            .api_read_allowed
    );

    // 사람이 허용하면 읽을 수 있습니다.
    let _: Empty = a
        .ok::<m::ChannelAllowApiRead>(AllowApiReadParams {
            space: c.space,
            channel: ch,
            allowed: true,
        })
        .await;
    assert!(
        channel_view_eventually(a, c.space, ch)
            .await
            .api_read_allowed
    );
    let out = a
        .node
        .call(&bot, "message.list", list.clone())
        .await
        .expect("허용 뒤 읽기");
    let res: MessageListResult = serde_json::from_value(out).expect("결과 형식");
    assert!(
        res.messages
            .iter()
            .any(|v| v.id == id && v.text == "봇이 읽으면 안 됨")
    );
    match handler.filter_notification(&auth, &raw) {
        Some(Notification::MessageNew { message, .. }) => {
            assert_eq!(message.text, "봇이 읽으면 안 됨");
        }
        other => panic!("내용 있는 알림이어야 함: {other:?}"),
    }

    // 다시 막으면 다시 1007.
    a.ok::<m::ChannelAllowApiRead>(AllowApiReadParams {
        space: c.space,
        channel: ch,
        allowed: false,
    })
    .await;
    let err = a.node.call(&bot, "message.list", list).await.unwrap_err();
    assert_eq!(err.code, ERR_VOLATILE_READ_DENIED);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn leakcheck_finds_no_volatile_plaintext_on_disk() {
    let c = crew().await;
    let (a, b, cc) = (c.a(), c.b(), c.c());
    let vch = volatile_channel(
        &c,
        settings(
            "누출 검사",
            JoinMode::Always,
            ChatTtl::None,
            HistoryVisibility::Public,
        ),
    )
    .await;
    converged(&[a, b, cc], c.space, vch).await;
    let secret = marker("VOLATILE");
    let id = a.send(c.space, vch, &secret).await;
    for n in [b, cc] {
        wait_message(n, c.space, vch, id, "휘발성 표식", |v| {
            v.text == secret
        })
        .await;
    }
    let edited = marker("VOLATILE-EDIT");
    a.ok::<m::MessageEdit>(MessageEditParams {
        space: c.space,
        channel: vch,
        id,
        text: edited.clone(),
    })
    .await;
    wait_message(b, c.space, vch, id, "고친 표식", |v| v.text == edited).await;
    for n in [a, b, cc] {
        for mk in [&secret, &edited] {
            let r = leakcheck(n, mk).await;
            assert!(
                r.clean,
                "{}: 휘발성 평문이 디스크에 있음: {:?}",
                n.name, r.findings
            );
            assert!(r.findings.is_empty());
        }
    }

    // 같은 표식을 일반 채널에 보내면 찾지만, 암호화된 DB 안에서만(파일 원문에는 없음).
    let normal = c
        .a()
        .ok::<m::ChannelList>(SpaceParams { space: c.space })
        .await
        .into_iter()
        .find(|v| !v.settings.is_volatile())
        .expect("일반 채널")
        .channel;
    let plain = marker("NORMAL");
    let nid = eventually("일반 채널에 보냄", || async {
        a.call::<m::MessageSend>(send_params(c.space, normal, &plain))
            .await
            .ok()
            .map(|r| r.id)
    })
    .await;
    wait_message(b, c.space, normal, nid, "일반 표식", |v| {
        v.text == plain
    })
    .await;
    for n in [a, b] {
        let r = leakcheck(n, &plain).await;
        assert!(!r.clean, "{}: 일반 채널 표식을 찾아야 함", n.name);
        assert!(
            r.findings.iter().all(|f| f.starts_with("db:")),
            "{}: 파일에 평문이 있음: {:?}",
            n.name,
            r.findings
        );
        assert!(r.findings.iter().any(|f| f.contains("(복호화)")));
    }
    // 휘발성 표식은 여전히 없습니다.
    assert!(leakcheck(a, &secret).await.clean);

    // 진단 지표: 받은 메시지의 전달 지연, 메모리, 휘발성 RAM 크기.
    let metrics = b.ok::<m::DiagMetricsGet>(Empty).await;
    assert!(metrics.memory_bytes > 0);
    assert!(metrics.delivery_ms_p50 <= metrics.delivery_ms_p95);
    assert!(metrics.cpu_percent >= 0.0);
    assert!(metrics.events_per_sec >= 0.0);
    assert!(
        metrics
            .queues
            .get("volatile_events")
            .is_some_and(|n| *n >= 1)
    );
    // 표식이 짧으면 잘못된 입력.
    assert!(
        b.call::<m::DiagLeakcheck>(LeakcheckParams {
            marker: "abc".into()
        })
        .await
        .is_err()
    );
}

/// 참가자 목록(사용자)이 `want`가 될 때까지 기다립니다.
async fn participants_eventually(n: &TestNode, space: SpaceId, ch: ChannelId, want: &[&TestNode]) {
    let want = users(want);
    eventually("참가자 목록", || async {
        let v = channel_view(n, space, ch).await?;
        let got: BTreeSet<UserId> = v.participants.unwrap_or_default().into_iter().collect();
        (got == want).then_some(())
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn late_event_from_departed_device_is_dropped() {
    let c = crew().await;
    let (a, b, cc) = (c.a(), c.b(), c.c());
    let ch = volatile_channel(
        &c,
        settings(
            "늦은 작별",
            JoinMode::JoinLeave,
            ChatTtl::OnLeave,
            HistoryVisibility::Private,
        ),
    )
    .await;
    for n in [a, b, cc] {
        join(n, c.space, ch).await;
    }
    converged(&[a, b, cc], c.space, ch).await;
    // c가 마지막으로 보낸 메시지가 c의 퇴장 알림보다 늦게 a에 도착하는 경우(별도 요청이라 순서가 바뀔 수 있음).
    let bye = vt::author_unsent(&cc.node, c.space, ch, "잘 있어", None).expect("서명");
    leave(cc, c.space, ch).await;
    participants_eventually(a, c.space, ch, &[a, b]).await;
    testkit::ingest_from(&a.node, c.space, vec![bye.clone()], cc.peer())
        .await
        .expect("적용 경로");
    // 퇴장 시 삭제 채널에서 떠난 기기의 메시지가 남지 않습니다(참가자가 아니면 받지 않음).
    assert!(
        a.messages(c.space, ch)
            .await
            .iter()
            .all(|v| v.id != bye.id())
    );
    assert!(
        vt::volatile_raw_events(&a.node, &c.space, &ch)
            .iter()
            .all(|e| e.id() != bye.id())
    );
    // 남은 참가자끼리는 그대로 주고받습니다.
    let id = b.send(c.space, ch, "둘이서").await;
    wait_message(a, c.space, ch, id, "남은 참가자 메시지", |_| true).await;
    // c가 다시 들어와도 퇴장 전에 보낸 메시지가 되살아나지 않습니다.
    join(cc, c.space, ch).await;
    converged(&[a, b, cc], c.space, ch).await;
    let back = cc.send(c.space, ch, "다시 왔어").await;
    wait_message(a, c.space, ch, back, "재입장 뒤 메시지", |_| true).await;
    assert!(
        a.messages(c.space, ch)
            .await
            .iter()
            .all(|v| v.id != bye.id())
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn peer_key_bundles_never_plant_keys() {
    let c = crew().await;
    let (a, b, cc) = (c.a(), c.b(), c.c());
    let ch = volatile_channel(
        &c,
        settings(
            "키 묶음",
            JoinMode::JoinLeave,
            ChatTtl::None,
            HistoryVisibility::Private,
        ),
    )
    .await;
    join(a, c.space, ch).await;
    join(b, c.space, ch).await;
    converged(&[a, b], c.space, ch).await;
    let volatile_key = |generation: u64| SharedKey::Volatile {
        channel: ch,
        generation,
        key: Secret32::random(),
    };

    // 세션 키는 회전 공지·감싸기로만 옵니다. 참가자 b라도 키 묶음(여러 개든 하나든)으로는 세대 키를 심지 못합니다.
    let flood: Vec<u64> = (0..256).map(|_| rand::random()).collect();
    let one: u64 = rand::random();
    for (from, keys) in [
        (
            b,
            flood.iter().map(|g| volatile_key(*g)).collect::<Vec<_>>(),
        ),
        (b, vec![volatile_key(one)]),
        (cc, vec![volatile_key(one)]),
    ] {
        let ev =
            testkit::author_key_share(&from.node, c.space, ch, keys, ShareVia::SealedToMembers)
                .expect("서명");
        testkit::ingest_from(&a.node, c.space, vec![ev], from.peer())
            .await
            .expect("적용 경로");
    }
    let have = vt::volatile_generations(&a.node, &c.space, &ch);
    assert!(flood.iter().all(|g| !have.contains(g)), "여러 키 묶음 무시");
    assert!(!have.contains(&one), "키 하나 묶음도 무시");

    // 정상 회전은 그대로 됩니다(c가 들어오면 셋이 같은 세대로 맞춰짐).
    join(cc, c.space, ch).await;
    converged(&[a, b, cc], c.space, ch).await;
}

/// 가입만 있는 회전은 부모 키에서 유도합니다: 기존 참가자는 감싸기를 받지 않고, 가입자는 부모 세대 키를 갖지 못합니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn join_rotation_is_derived_and_joiner_cannot_read_the_parent() {
    let c = crew().await;
    let (a, b, cc) = (c.a(), c.b(), c.c());
    let ch = volatile_channel(
        &c,
        settings(
            "가입 유도",
            JoinMode::JoinLeave,
            ChatTtl::None,
            HistoryVisibility::Private,
        ),
    )
    .await;
    join(a, c.space, ch).await;
    join(b, c.space, ch).await;
    let parent = converged(&[a, b], c.space, ch).await;
    let dev = |n: &TestNode| testkit::device_id(&n.node).expect("신원");
    let before: Vec<u64> = [a, b].iter().map(|n| vt::wraps_received(&n.node)).collect();

    join(cc, c.space, ch).await;
    let g = converged(&[a, b, cc], c.space, ch).await;
    assert_ne!(g, parent);
    let commits: Vec<_> = [a, b, cc]
        .iter()
        .map(|n| vt::key_commit(&n.node, &c.space, &ch, g).expect("새 세대 키"))
        .collect();
    assert!(commits.windows(2).all(|w| w[0] == w[1]), "셋이 같은 키");
    assert!(
        vt::key_commit(&cc.node, &c.space, &ch, parent).is_none(),
        "가입자는 들어오기 전 세대 키를 갖지 않음"
    );
    // 회전을 낸 기기는 가장 작은 기기 ID. 가입자가 아니면 가입 회전이라 남은 기존 참가자는 감싸기를 받지 않습니다.
    let rotator = [a, b, cc].into_iter().min_by_key(|n| dev(n)).expect("셋");
    if dev(rotator) != dev(cc) {
        for (n, was) in [a, b].iter().zip(before) {
            if dev(n) != dev(rotator) {
                assert_eq!(vt::wraps_received(&n.node), was, "기존 참가자는 유도");
            }
        }
    }
    // 새 세대로 셋 다 주고받습니다.
    let text = marker("가입 뒤");
    let id = a
        .ok::<m::MessageSend>(send_params(c.space, ch, &text))
        .await;
    for n in [b, cc] {
        wait_message(n, c.space, ch, id.id, "가입 뒤 메시지", |_| true).await;
    }
}

/// 공지의 약속값과 다른 키를 봉인한 감싸기는 버리고, 공지를 쓰지 않은 기기의 감싸기는 받지 않습니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wrap_that_does_not_match_the_commit_is_rejected() {
    let c = crew().await;
    let (a, b, cc) = (c.a(), c.b(), c.c());
    let ch = volatile_channel(
        &c,
        settings(
            "거짓 감싸기",
            JoinMode::JoinLeave,
            ChatTtl::None,
            HistoryVisibility::Private,
        ),
    )
    .await;
    for n in [a, b, cc] {
        join(n, c.space, ch).await;
    }
    converged(&[a, b, cc], c.space, ch).await;
    let a_dev = testkit::device_id(&a.node).expect("신원");
    let real = Secret32::random();
    let (ev, g, list) = vt::forge_rotation(&b.node, c.space, ch, a_dev, &real).expect("공지");
    let wrap = |from: &TestNode, key: &Secret32| {
        vt::forge_wrap(&from.node, c.space, ch, g, a_dev, &real, key).expect("감싸기")
    };
    let wrong = wrap(b, &Secret32::random());
    assert!(matches!(
        vt::deliver_wrap(&a.node, b.peer(), c.space, ch, g, wrong, Some(list.clone())),
        YpResponse::Ok
    ));
    testkit::ingest_from(&a.node, c.space, vec![ev], b.peer())
        .await
        .expect("적용 경로");
    assert!(
        vt::key_commit(&a.node, &c.space, &ch, g).is_none(),
        "약속과 다른 키는 받지 않음"
    );
    // 공지를 쓰지 않은 c는 맞는 키라도 그 세대 감싸기를 보낼 수 없습니다.
    assert!(matches!(
        vt::deliver_wrap(
            &a.node,
            cc.peer(),
            c.space,
            ch,
            g,
            wrap(cc, &real),
            Some(list.clone())
        ),
        YpResponse::Error { .. }
    ));
    assert!(vt::key_commit(&a.node, &c.space, &ch, g).is_none());
    // 공지를 쓴 b가 맞는 감싸기를 보내면 기다리던 공지가 적용됩니다.
    assert!(matches!(
        vt::deliver_wrap(
            &a.node,
            b.peer(),
            c.space,
            ch,
            g,
            wrap(b, &real),
            Some(list)
        ),
        YpResponse::Ok
    ));
    assert_eq!(
        vt::key_commit(&a.node, &c.space, &ch, g),
        Some(yangpa_crypto::keysched::vol_key_commit(
            &c.space, &ch, g, &real
        ))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lost_presence_announcement_is_retried() {
    let c = crew().await;
    let (a, b, cc) = (c.a(), c.b(), c.c());
    let ch = volatile_channel(
        &c,
        settings(
            "알림 유실",
            JoinMode::JoinLeave,
            ChatTtl::None,
            HistoryVisibility::Private,
        ),
    )
    .await;
    join(a, c.space, ch).await;
    join(b, c.space, ch).await;
    converged(&[a, b], c.space, ch).await;

    // 들어가는 c의 입장 알림이 a에게 닿지 못함(연결은 그대로): 1초 주기 작업이 다시 알립니다.
    vt::fail_presence_to(&cc.node, a.peer(), 1);
    join(cc, c.space, ch).await;
    for n in [a, b, cc] {
        participants_eventually(n, c.space, ch, &[a, b, cc]).await;
    }
    converged(&[a, b, cc], c.space, ch).await;
    let id = a.send(c.space, ch, "다 모임").await;
    wait_message(cc, c.space, ch, id, "a의 메시지", |_| true).await;

    // 이미 들어가 있는 a의 답(자기 입장)이 c에게 닿지 못한 경우도 다시 알립니다.
    leave(cc, c.space, ch).await;
    participants_eventually(a, c.space, ch, &[a, b]).await;
    vt::fail_presence_to(&a.node, cc.peer(), 1);
    join(cc, c.space, ch).await;
    for n in [a, b, cc] {
        participants_eventually(n, c.space, ch, &[a, b, cc]).await;
    }
    converged(&[a, b, cc], c.space, ch).await;
    let id = cc.send(c.space, ch, "다시 모임").await;
    for n in [a, b] {
        wait_message(n, c.space, ch, id, "c의 메시지", |_| true).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn snapshot_asks_every_participant() {
    let c = crew().await;
    let (a, b, cc) = (c.a(), c.b(), c.c());
    let ch = volatile_channel(
        &c,
        settings(
            "모두에게",
            JoinMode::JoinLeave,
            ChatTtl::Seconds(3600),
            HistoryVisibility::Public,
        ),
    )
    .await;
    // c가 먼저 묻게 될(피어 ID가 작은) 쪽이 기록을 하나도 갖지 않게 합니다.
    let (empty, full) = if a.peer() < b.peer() { (a, b) } else { (b, a) };
    join(full, c.space, ch).await;
    let id = full.send(c.space, ch, "먼저 온 사람").await;
    // `empty`는 들어가면서 받아 오기에 실패해 아무것도 없습니다.
    vt::fail_snapshot(&empty.node, 1);
    join(empty, c.space, ch).await;
    converged(&[empty, full], c.space, ch).await;
    assert!(empty.messages(c.space, ch).await.is_empty());

    join(cc, c.space, ch).await;
    let got = wait_message(cc, c.space, ch, id, "다른 참가자의 기록", |v| {
        v.text == "먼저 온 사람"
    })
    .await;
    assert!(got.expires_at.is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn snapshot_items_cannot_inflate_lamport() {
    let c = crew().await;
    let (a, b) = (c.a(), c.b());
    let ch = volatile_channel(
        &c,
        settings(
            "논리 시계",
            JoinMode::JoinLeave,
            ChatTtl::None,
            HistoryVisibility::Public,
        ),
    )
    .await;
    join(a, c.space, ch).await;
    join(b, c.space, ch).await;
    converged(&[a, b], c.space, ch).await;
    let known = vt::max_lamport(&a.node, &c.space).expect("논리 시계");
    // 응답자 b가 논리 시계를 크게 부풀린(풀리는) 메시지를 스냅숏에 넣음.
    let inflated = vt::author_unsent(&b.node, c.space, ch, "부풀림", Some(known + (1u64 << 40)))
        .expect("서명");
    vt::deliver_snapshot(
        &a.node,
        c.space,
        ch,
        b.peer(),
        vec![(inflated.clone(), None)],
    )
    .expect("스냅숏 적용");
    assert!(
        a.messages(c.space, ch)
            .await
            .iter()
            .all(|v| v.id != inflated.id())
    );
    assert!(
        vt::volatile_raw_events(&a.node, &c.space, &ch)
            .iter()
            .all(|e| e.id() != inflated.id())
    );
    // a의 다음 메시지는 저장된 최댓값 가까이의 논리 시계로 나가 b에게 보류 없이 닿습니다.
    let id = a.send(c.space, ch, "다음 메시지").await;
    wait_message(b, c.space, ch, id, "a의 다음 메시지", |_| true).await;
    let lamport = vt::volatile_raw_events(&a.node, &c.space, &ch)
        .into_iter()
        .find(|e| e.id() == id)
        .expect("a의 RAM에 있음")
        .header
        .lamport;
    let now_known = vt::max_lamport(&a.node, &c.space).expect("논리 시계");
    assert!(lamport <= now_known + (1 << 20), "{lamport} / {now_known}");
}

/// 상시 채널 참가 상한: 넘친 기기(기기 ID가 가장 큰 기기)는 보내지 못하고 받지도 않다가, 상한을 늘리면 들어옵니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn always_on_cap_keeps_the_extra_device_out_until_raised() {
    let c = crew().await;
    let ch = volatile_channel(
        &c,
        settings(
            "상한",
            JoinMode::Always,
            ChatTtl::None,
            HistoryVisibility::Private,
        ),
    )
    .await;
    let set_cap = |cap: Option<u32>| {
        let mut p = ChannelUpdateParams::new(c.space, ch);
        p.volatile_cap = Some(cap);
        p
    };
    // 범위 밖은 데몬이 먼저 거절합니다.
    assert!(
        c.a()
            .call::<m::ChannelUpdate>(set_cap(Some(1)))
            .await
            .is_err()
    );
    c.a().ok::<m::ChannelUpdate>(set_cap(Some(2))).await;

    let mut nodes = c.all();
    nodes.sort_by_key(|n| testkit::device_id(&n.node).expect("신원"));
    let (inside, out) = (&nodes[..2], nodes[2]);
    let view = eventually("상한이 차서 밀려남", || async {
        channel_view(out, c.space, ch)
            .await
            .filter(|v| v.volatile_full)
    })
    .await;
    assert_eq!(view.volatile_cap, Some(2));
    let err = out
        .call::<m::MessageSend>(send_params(c.space, ch, "못 보냄"))
        .await
        .unwrap_err();
    assert_eq!(err.code, ERR_CONFLICT);
    assert!(err.message.contains("가득"), "{}", err.message);

    converged(inside, c.space, ch).await;
    let id = inside[0]
        .ok::<m::MessageSend>(send_params(c.space, ch, &marker("둘만")))
        .await;
    wait_message(inside[1], c.space, ch, id.id, "둘만", |_| true).await;
    assert!(vt::volatile_raw_events(&out.node, &c.space, &ch).is_empty());
    assert!(!vt::volatile_has_key(&out.node, &c.space, &ch));

    // 기본값(200)으로 되돌리면 셋 다 참가합니다.
    c.a().ok::<m::ChannelUpdate>(set_cap(None)).await;
    converged(&nodes, c.space, ch).await;
    assert!(
        !channel_view_eventually(out, c.space, ch)
            .await
            .volatile_full
    );
}
