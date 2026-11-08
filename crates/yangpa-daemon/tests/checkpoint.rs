//! 상태 체크포인트 시험: 소유자 기기가 낸 체크포인트를 모든 멤버가 같은 상태로 받고, 새 멤버가 체크포인트와 꼬리만
//! 받아 들어오며, 재시작이 스냅숏에서 시작하고, 위조한 기기 키 답을 버립니다. 키 그룹과 함께: 회전·추방·확정 기기
//! 바꾸기를 지난 크루에 체크포인트로 들어온 새 멤버가 지금 메시지·공개 기록을 읽고 비공개 옛 기록은 못 읽으며, 키
//! 머리에서 그룹 사슬을 시작하고, 확정 기기가 되어 기기 키를 물어 갱신을 냅니다.
//!
//! 문턱을 낮춘 설정(`low`)으로 몇 개의 상태 이벤트만으로 체크포인트를 냅니다. 고정 sleep 없이 조건을 기다립니다.

use crate::common;

use std::time::Duration;

use common::{Cluster, TestNode, connect_both, eventually, next_notification};
use yangpa_core::badge::Badge;
use yangpa_core::event::{EventKind, StreamId};
use yangpa_core::ids::{ChannelId, EventId, RoleId, SpaceId, UserId};
use yangpa_core::model::{ChannelKind, HistoryVisibility};
use yangpa_core::wire::{MAX_MEMBER_CARDS_REQUEST, WireErrorCode, YpRequest, YpResponse};
use yangpa_daemon::NodeConfig;
use yangpa_daemon::testkit;
use yangpa_daemon::testkit::checkpoint as ct;
use yangpa_rpc::api::{
    BadgeSetShownParams, ChannelCreateParams, CheckpointAlertKind, CommitterSetParams,
    CrewCreateParams, CrewUpdateParams, Empty, InviteCreateParams, InviteJoinStatus,
    MemberKickParams, MemberSetRolesParams, MessageSendParams, Notification, ProfileGetParams,
    ProfileSetParams, RoleCreateParams, ShownBadgeView, UrlParams, UserParams, m,
};

/// 몇 개의 상태 이벤트만으로 체크포인트를 냅니다.
fn low(c: &mut NodeConfig) {
    c.checkpoint.start_events = 6;
    c.checkpoint.every_events = 6;
}

/// [`low`]에 두 번째 출처를 짧게만 기다리게.
fn low_short(c: &mut NodeConfig) {
    low(c);
    c.checkpoint.second_source_wait = Duration::from_secs(5);
}

async fn create_crew(a: &TestNode) -> SpaceId {
    a.ok::<m::CrewCreate>(CrewCreateParams {
        name: "체크포인트 크루".into(),
        description: None,
    })
    .await
    .space
}

async fn join(gateway: &TestNode, joiner: &TestNode, space: SpaceId) -> InviteJoinStatus {
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
    r.status
}

async fn wait_joined(n: &TestNode, space: SpaceId) {
    let me = n.user();
    eventually(&format!("{} 가입 완료", n.name), || async {
        let st = testkit::space_state(&n.node, &space)?;
        (st.is_member(&me) && testkit::crew_key_seq(&n.node, &space).is_some()).then_some(())
    })
    .await;
}

/// 상태 이벤트 몇 개(설명 바꾸기·채널 만들기).
async fn churn(a: &TestNode, space: SpaceId, n: usize, tag: &str) -> Vec<ChannelId> {
    let mut out = Vec::new();
    for i in 0..n {
        a.ok::<m::CrewUpdate>(CrewUpdateParams {
            space,
            name: None,
            description: Some(format!("{tag} {i}")),
            icon_path: None,
            strict_rotation: None,
            emoji_limits: None,
        })
        .await;
        if i % 3 == 0 {
            let v = a
                .ok::<m::ChannelCreate>(ChannelCreateParams {
                    space,
                    name: format!("{tag}-{i}"),
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
                .await;
            out.push(v.channel);
        }
    }
    out
}

/// 모두가 같은 바탕 체크포인트(순번 `at_least` 이상)와 같은 상태에 이를 때까지. 그 순번을 돌려줍니다.
async fn wait_same_base(nodes: &[&TestNode], space: SpaceId, at_least: u64) -> u64 {
    eventually("모두 같은 체크포인트와 상태", || async {
        let first = ct::base(&nodes[0].node, &space)?;
        if first.0 < at_least {
            return None;
        }
        let st = testkit::space_state(&nodes[0].node, &space)?;
        for n in nodes {
            if ct::base(&n.node, &space)? != first {
                return None;
            }
            if *testkit::space_state(&n.node, &space)? != *st {
                return None;
            }
            // 저장한 바탕 기록과 시드할 스냅숏 블롭.
            let (seq, snap, kept) = ct::saved(&n.node, &space)?;
            if seq != first.0 || !kept.contains(&snap) {
                return None;
            }
        }
        Some(first.0)
    })
    .await
}

fn space_events(n: &TestNode, space: SpaceId) -> usize {
    testkit::stream_events(&n.node, &space, StreamId::Space)
        .unwrap()
        .len()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn members_agree_and_newcomer_joins_from_checkpoint() {
    let cl = Cluster::new();
    let a = cl.node_with("a", low).await;
    let b = cl.node_with("b", low).await;
    let c = cl.node_with("c", low).await;
    for n in [&a, &b, &c] {
        ct::hold(&n.node, true);
    }
    let space = create_crew(&a).await;
    assert_eq!(join(&a, &b, space).await, InviteJoinStatus::Joined);
    assert_eq!(join(&a, &c, space).await, InviteJoinStatus::Joined);
    wait_joined(&b, space).await;
    wait_joined(&c, space).await;
    // 경계(가장 큰 논리 시계 - `FRONTIER_DEPTH`)가 0보다 크게 넉넉히.
    churn(&a, space, 40, "앞").await;
    assert!(
        ct::base(&a.node, &space).is_none(),
        "멈춰 둔 동안 내지 않음"
    );

    // 1. 소유자 기기가 체크포인트를 내고 모든 멤버가 같은 상태·같은 스냅숏 블롭으로 받음.
    for n in [&a, &b, &c] {
        ct::hold(&n.node, false);
    }
    let seq = wait_same_base(&[&a, &b, &c], space, 1).await;
    assert!(ct::floor(&b.node, &space) > 0);

    // 2. 그 뒤 상태가 더 바뀌어도 다음 체크포인트가 이어짐(순번이 오름).
    churn(&a, space, 8, "뒤").await;
    let seq2 = wait_same_base(&[&a, &b, &c], space, seq + 1).await;
    assert!(seq2 > seq);
    assert!(
        ct::since_base(&a.node, &space).unwrap() < 20,
        "바탕 뒤 기록만 둠"
    );

    // 3. 새 멤버: 링크의 고정에서 사슬을 확인하고 스냅숏과 경계 위 꼬리만 받아 들어옴.
    let d = cl.node_with("d", low).await;
    ct::hold(&a.node, true);
    join(&a, &d, space).await;
    wait_joined(&d, space).await;
    let d_base = ct::base(&d.node, &space).expect("체크포인트로 상태를 세움");
    assert!(d_base.0 >= seq2);
    assert!(!ct::stashed(&d.node, &space));
    assert!(
        space_events(&d, space) < space_events(&a, space),
        "경계 아래 기록을 받지 않음: {} < {}",
        space_events(&d, space),
        space_events(&a, space)
    );
    eventually("새 멤버 상태가 같음", || async {
        let sa = testkit::space_state(&a.node, &space)?;
        let sd = testkit::space_state(&d.node, &space)?;
        (*sa == *sd).then_some(())
    })
    .await;
    // 들어온 뒤 다른 멤버와 머리를 견줌.
    connect_both(&d, &b).await;
    eventually("다른 멤버와 체크포인트 머리 확인", || async {
        (!ct::unconfirmed(&d.node, &space)).then_some(())
    })
    .await;

    // 4. 새 멤버도 상태를 바꿀 수 있고, 다음 체크포인트도 같이 받음.
    ct::hold(&a.node, false);
    let channel = a.first_channel(space).await;
    let sent = d.send(space, channel, "체크포인트로 들어옴").await;
    eventually("A가 새 멤버 글을 받음", || async {
        a.messages(space, channel)
            .await
            .iter()
            .any(|m| m.id == sent)
            .then_some(())
    })
    .await;
    // 체크포인트로 들어온 D는 C와 만난 적이 없고 C의 가입 이벤트(기기 키 목록)도 받지 않았습니다. C의 채널 글은 짧은
    // 서명이라 D는 글을 보류하고 멤버 피어에게 C의 키를 물어 받은 뒤 확인합니다.
    let from_c = c.send(space, channel, "키를 물어 확인").await;
    let ev = eventually("A가 C의 글을 받음", || async {
        testkit::get_event(&a.node, &from_c).ok().flatten()
    })
    .await;
    // 따라잡기·전달로 D에 먼저 닿았을 수 있습니다(그때는 이미 보류해 키를 물었고, 여기서는 중복).
    let r = testkit::ingest_from(&d.node, space, vec![ev], a.peer())
        .await
        .unwrap();
    assert!(r.held == 1 || r.duplicate == 1, "키를 모르면 보류: {r:?}");
    eventually("D가 C의 키를 물어 글을 받음", || async {
        d.messages(space, channel)
            .await
            .iter()
            .any(|m| m.id == from_c)
            .then_some(())
    })
    .await;
    // D가 확정 기기로 봉인할 상자 키를 먼저 물었다면(keygroup) C의 서명 키도 이미 알아 묻지 않고 받습니다.
    let asked = testkit::sign_keys_asked(&d.node);
    if r.held == 1 {
        assert_eq!(asked, 1, "보류했으면 키를 물음");
    }
    assert!(asked <= 1, "키는 한 번만 물음: {asked}");
    churn(&a, space, 8, "더").await;
    wait_same_base(&[&a, &b, &c, &d], space, seq2 + 1).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restart_starts_from_snapshot_and_forged_device_keys_are_dropped() {
    let cl = Cluster::new();
    let a = cl.node_with("a", low).await;
    let mut b = cl.node_with("b", low).await;
    ct::hold(&a.node, true);
    let space = create_crew(&a).await;
    assert_eq!(join(&a, &b, space).await, InviteJoinStatus::Joined);
    wait_joined(&b, space).await;
    churn(&a, space, 9, "앞").await;
    ct::hold(&a.node, false);
    let seq = wait_same_base(&[&a, &b], space, 1).await;
    let before = testkit::space_state(&b.node, &space).unwrap();

    // 재시작: 저장한 스냅숏으로 해소기를 세우고 경계 위만 다시 넣음(네트워크 없이 같은 상태).
    b.restart_with(low).await;
    let after = ct::base(&b.node, &space).expect("재시작 뒤 바탕");
    assert_eq!(after.0, seq);
    assert_eq!(*testkit::space_state(&b.node, &space).unwrap(), *before);

    // GetDeviceKeys: 진짜 답은 키 약속·기기 트리와 맞아 받고, 상자 키를 바꾼 답은 버림.
    connect_both(&a, &b).await;
    testkit::authenticate(&b.node, a.peer()).await.unwrap();
    let a_dev = testkit::device_id(&a.node).unwrap();
    // 재시작 전 연결이 닫힌 것을 A가 늦게 처리하면 B의 새 인증을 지워 '인증 전'으로 답할 수 있습니다. B는 A를 이미
    // 알아 `authenticate`로는 다시 보내지 않으므로 인증을 다시 보내고 묻습니다.
    let items = eventually("A가 기기 키로 답함", || async {
        testkit::reauth(&b.node, a.peer()).await.ok()?;
        let req = YpRequest::GetDeviceKeys {
            space,
            devices: vec![a_dev],
        };
        match testkit::net_request(&b.node, a.peer(), req).await.ok()? {
            YpResponse::DeviceKeys(items) => Some(items),
            _ => None,
        }
    })
    .await;
    assert_eq!(items.len(), 1);
    assert!(
        items[0].proof.is_some(),
        "체크포인트가 있으면 기기 트리 증명을 붙임"
    );
    let good = ct::accept_device_keys(&b.node, &space, &[a_dev], items.clone());
    assert_eq!(good.len(), 1);
    let mut forged = items.clone();
    forged[0].keys.box_key = yangpa_core::device::BoxPublicKey::filled(7);
    assert!(ct::accept_device_keys(&b.node, &space, &[a_dev], forged).is_empty());
    // 묻지 않은 기기의 답도 버림.
    assert!(ct::accept_device_keys(&b.node, &space, &[], items).is_empty());
}

/// 받아 준 노드 말고는 체크포인트 머리를 확인해 줄 출처가 없는 새 멤버: 확인하는 동안 쓰지 않고, 기다림이 지나면
/// 체크포인트 상태를 버리고 처음부터 받아 같은 상태·같은 체크포인트에 이릅니다(받아 준 노드 하나만 믿지 않음).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn newcomer_without_second_source_falls_back_to_full_history() {
    let cl = Cluster::new();
    let a = cl.node_with("a", low).await;
    ct::hold(&a.node, true);
    let space = create_crew(&a).await;
    churn(&a, space, 40, "앞").await;
    ct::hold(&a.node, false);
    wait_same_base(&[&a], space, 1).await;
    ct::hold(&a.node, true);

    let d = cl.node_with("d", low_short).await;
    let mut rx = d.subscribe();
    join(&a, &d, space).await;
    let channel = a.first_channel(space).await;
    // 확인하는 동안에는 쓰지 않습니다(앞뒤 모두 확인 중일 때만 봄: 기다림이 짧아 그새 끝날 수 있음).
    if ct::unconfirmed(&d.node, &space) {
        let r = d
            .call::<m::MessageSend>(MessageSendParams {
                space,
                channel,
                text: "확인 전".into(),
                attachments: Vec::new(),
                reply_to: None,
                thread: None,
            })
            .await;
        if ct::unconfirmed(&d.node, &space) {
            let e = r.expect_err("확인 전에는 쓰지 않음");
            assert!(e.message.contains("확인하는 중"), "{e}");
        }
    }
    let kind = next_notification(&mut rx, "체크포인트 경보", |n| match n {
        Notification::CheckpointAlert { space: s, kind } if *s == space => Some(*kind),
        _ => None,
    })
    .await;
    assert_eq!(kind, CheckpointAlertKind::Fallback);
    eventually("처음부터 받아 같은 상태·같은 바탕", || async {
        let sa = testkit::space_state(&a.node, &space)?;
        let sd = testkit::space_state(&d.node, &space)?;
        let same = *sa == *sd
            && ct::base(&d.node, &space)? == ct::base(&a.node, &space)?
            && space_events(&d, space) >= space_events(&a, space)
            && !ct::unconfirmed(&d.node, &space);
        same.then_some(())
    })
    .await;
    let sent = d.send(space, channel, "처음부터 받은 뒤").await;
    eventually("A가 받음", || async {
        a.messages(space, channel)
            .await
            .iter()
            .any(|m| m.id == sent)
            .then_some(())
    })
    .await;
}

/// 늦은 이벤트를 다시 서명해 내는 것은 이벤트마다 한 번입니다(재시작한 뒤에도).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resigned_events_are_remembered_across_restarts() {
    let cl = Cluster::new();
    let mut a = cl.node_with("a", low).await;
    ct::hold(&a.node, true);
    let space = create_crew(&a).await;
    churn(&a, space, 2, "앞").await;
    let mine: Vec<_> = testkit::stream_events(&a.node, &space, StreamId::Space)
        .unwrap()
        .into_iter()
        .filter(|e| e.header.kind == EventKind::SpaceUpdate)
        .map(|e| e.id())
        .collect();
    assert!(mine.len() >= 2, "{}", mine.len());
    let before = space_events(&a, space);
    ct::resign(&a.node, space, vec![mine[0]]);
    eventually("다시 냄", || async {
        (space_events(&a, space) == before + 1).then_some(())
    })
    .await;
    a.restart_with(low).await;
    ct::hold(&a.node, true);
    // 앞 것은 건너뛰고 뒤 것만 냅니다(같은 작업이 차례로 보므로 뒤 것이 나오면 앞 것은 이미 건너뜀).
    ct::resign(&a.node, space, vec![mine[0], mine[1]]);
    eventually("뒤 것만 다시 냄", || async {
        (space_events(&a, space) >= before + 2).then_some(())
    })
    .await;
    assert_eq!(space_events(&a, space), before + 2);
}

// ───────────── 키 그룹과 함께 ─────────────

fn channel_params(space: SpaceId, name: &str) -> ChannelCreateParams {
    ChannelCreateParams {
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
    }
}

async fn create_role(a: &TestNode, space: SpaceId, name: &str, perms: &[&str]) -> RoleId {
    a.ok::<m::RoleCreate>(RoleCreateParams {
        space,
        name: name.into(),
        color: None,
        permissions: perms.iter().map(|s| (*s).to_owned()).collect(),
        rank: Some(10),
    })
    .await
    .role
}

async fn set_roles(
    a: &TestNode,
    space: SpaceId,
    user: yangpa_core::ids::UserId,
    roles: Vec<RoleId>,
) {
    a.ok::<m::MemberSetRoles>(MemberSetRolesParams { space, user, roles })
        .await;
}

async fn kick(a: &TestNode, space: SpaceId, user: yangpa_core::ids::UserId) {
    a.ok::<m::MemberKick>(MemberKickParams {
        space,
        user,
        reason: None,
    })
    .await;
}

/// 보낼 수 있을 때까지(그룹에 막 들어와 그룹 키를 기다리는 중이면 다시) 보냅니다.
async fn send_when_ready(n: &TestNode, space: SpaceId, channel: ChannelId, text: &str) -> EventId {
    eventually(&format!("{}가 '{text}'를 보냄", n.name), || async {
        n.call::<m::MessageSend>(MessageSendParams {
            space,
            channel,
            text: text.into(),
            attachments: Vec::new(),
            reply_to: None,
            thread: None,
        })
        .await
        .ok()
        .map(|r| r.id)
    })
    .await
}

async fn sees_text(n: &TestNode, space: SpaceId, channel: ChannelId, text: &str) {
    eventually(&format!("{}가 '{text}'를 읽음", n.name), || async {
        n.messages(space, channel)
            .await
            .iter()
            .any(|v| v.text == text)
            .then_some(())
    })
    .await;
}

fn header(n: &TestNode, id: &EventId) -> yangpa_core::event::EventHeader {
    testkit::get_event(&n.node, id)
        .expect("저장소")
        .expect("이벤트")
        .header
}

fn crew_members(n: &TestNode, space: SpaceId) -> Vec<yangpa_core::ids::DeviceId> {
    testkit::key_group_members(&n.node, &space, None).unwrap_or_default()
}

/// 회전(추방)·확정 기기 바꾸기를 지난 크루에 체크포인트로 들어온 새 멤버 D:
/// - 체크포인트 키 머리가 크루와 비공개 그룹을 덮고, D는 경계 아래 기록·그룹 갱신 없이 들어옴.
/// - 지금 메시지는 바로, 회전 전 공개 기록은 `GetChannelRoots`로 읽고, 비공개 채널의 들기 전 기록은 못 읽음.
/// - 체크포인트 뒤 그룹 가입(`KeyUpdate`)은 키 머리에서 시작한 사슬에 붙고, D가 쓴 메시지는 지금 키로 모두가 읽음.
/// - D가 확정 기기가 되어(표를 넘겨받음) 추방 회전을 내면, 체크포인트 앞 멤버의 상자 키는 `GetDeviceKeys`로 받아
///   봉인합니다(전체 재키 없음).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn newcomer_joins_from_checkpoint_after_rotations_and_committer_change() {
    common::init_tracing();
    let cl = Cluster::new();
    let a = cl.node_with("a", low).await;
    let b = cl.node_with("b", low).await;
    let c = cl.node_with("c", low).await;
    let e = cl.node_with("e", low).await;
    for n in [&a, &b, &c, &e] {
        ct::hold(&n.node, true);
    }
    let space = create_crew(&a).await;
    let mut p = channel_params(space, "공개 기록");
    p.history = Some(HistoryVisibility::Public);
    let open = a.ok::<m::ChannelCreate>(p).await.channel;
    for n in [&b, &c, &e] {
        assert_eq!(join(&a, n, space).await, InviteJoinStatus::Joined);
        wait_joined(n, space).await;
    }
    // 비공개 채널(역할 있는 사람만, 기록 비공개): 크루와 다른 키 그룹.
    let secret_role = create_role(&a, space, "비밀", &[]).await;
    set_roles(&a, space, b.user(), vec![secret_role]).await;
    let mut p = channel_params(space, "비밀방");
    p.private_roles = Some(vec![secret_role]);
    p.history = Some(HistoryVisibility::Private);
    let secret = a.ok::<m::ChannelCreate>(p).await.channel;
    eventually("B가 비밀방 그룹에 들어감", || async {
        testkit::channel_key_seq(&b.node, &space, secret)
    })
    .await;
    let old_open = send_when_ready(&a, space, open, "회전 전 공개 기록").await;
    let old_secret = send_when_ready(&a, space, secret, "들기 전 비밀").await;
    sees_text(&b, space, secret, "들기 전 비밀").await;

    // 체크포인트 앞: 추방(회전)과 확정 기기 바꾸기(관리자 B가 표를 넘겨받음).
    let c_dev = testkit::device_id(&c.node).unwrap();
    kick(&a, space, c.user()).await;
    eventually("C가 크루 그룹에서 빠짐", || async {
        (!crew_members(&a, space).contains(&c_dev)).then_some(())
    })
    .await;
    let admin = create_role(&a, space, "관리", &["administrator"]).await;
    set_roles(&a, space, b.user(), vec![secret_role, admin]).await;
    let db = testkit::device_id(&b.node).unwrap();
    a.ok::<m::CrewSetCommitter>(CommitterSetParams { space, device: db })
        .await;
    eventually("B가 표를 넘겨받음", || async {
        (testkit::committer_of(&b.node, &space) == Some(db)
            && testkit::has_key_table(&b.node, &space)
            && !testkit::has_key_table(&a.node, &space))
        .then_some(())
    })
    .await;
    churn(&a, space, 30, "앞").await;
    for n in [&a, &b, &e] {
        ct::hold(&n.node, false);
    }
    let seq = wait_same_base(&[&a, &b, &e], space, 1).await;
    for n in [&a, &b, &e] {
        ct::hold(&n.node, true);
    }
    let crew_seq_at = testkit::crew_key_seq(&a.node, &space).unwrap();
    let (crew_head, secret_head) = ct::key_head_seqs(&a.node, &space, secret).expect("키 머리");
    assert_eq!(crew_head, Some(crew_seq_at), "크루 머리는 해소기 것");
    let secret_head = secret_head.expect("비공개 그룹 머리");
    assert!(secret_head >= 1);

    // 새 멤버 D: 체크포인트와 경계 위 꼬리만 받아 들어옴(그 뒤 크루 가입 갱신은 확정 기기 B가 냄).
    let d = cl.node_with("d", low).await;
    connect_both(&b, &d).await;
    assert_eq!(join(&a, &d, space).await, InviteJoinStatus::Joined);
    wait_joined(&d, space).await;
    let d_base = ct::base(&d.node, &space).expect("체크포인트로 상태를 세움");
    assert!(d_base.0 >= seq);
    eventually("D 상태가 같음", || async {
        let sa = testkit::space_state(&a.node, &space)?;
        let sd = testkit::space_state(&d.node, &space)?;
        (*sa == *sd).then_some(())
    })
    .await;
    eventually("가입 뒤 크루 갱신", || async {
        (testkit::crew_key_seq(&d.node, &space)? > crew_seq_at).then_some(())
    })
    .await;
    connect_both(&d, &e).await;
    eventually("다른 멤버와 체크포인트 머리 확인", || async {
        (!ct::unconfirmed(&d.node, &space)).then_some(())
    })
    .await;

    // 지금 메시지와 회전 전 공개 기록(중계로 받은 뿌리).
    let now_open = send_when_ready(&a, space, open, "지금 공개").await;
    sees_text(&d, space, open, "지금 공개").await;
    assert!(testkit::can_derive_root(&d.node, &header(&a, &now_open)));
    sees_text(&d, space, open, "회전 전 공개 기록").await;
    assert!(!testkit::can_derive_root(&d.node, &header(&a, &old_open)));

    // 비밀방에 들기: 체크포인트 뒤 그룹 가입 갱신이 키 머리에서 시작한 D의 사슬에 붙음.
    let dd = testkit::device_id(&d.node).unwrap();
    set_roles(&a, space, d.user(), vec![secret_role]).await;
    eventually("D가 비밀방 그룹에 들어감", || async {
        let m = testkit::key_group_members(&b.node, &space, Some(secret))?;
        (m.contains(&dd) && testkit::channel_key_seq(&d.node, &space, secret).is_some())
            .then_some(())
    })
    .await;
    let (base_seq, _) = ct::group_chain_base(&d.node, &space, secret).expect("D의 그룹 사슬");
    assert!(
        base_seq >= secret_head,
        "키 머리에서 시작: {base_seq} < {secret_head}"
    );
    let kinds = testkit::key_update_kinds(&d.node, &space, Some(secret));
    assert!(
        kinds.iter().all(|(s, _)| *s > secret_head),
        "경계 아래 그룹 갱신을 받지 않음: {kinds:?}"
    );
    let after = send_when_ready(&a, space, secret, "든 뒤 비밀").await;
    sees_text(&d, space, secret, "든 뒤 비밀").await;
    let evs = testkit::stream_events(&a.node, &space, StreamId::Channel(secret)).unwrap();
    testkit::ingest_from(&d.node, space, evs, a.peer())
        .await
        .unwrap();
    assert!(!testkit::can_derive_root(&d.node, &header(&a, &old_secret)));
    assert!(
        testkit::message_text(&d.node, &old_secret).is_none(),
        "들기 전 비밀 기록을 읽음"
    );
    assert!(testkit::message_text(&d.node, &after).is_some());
    let old_root = match header(&a, &old_secret).key {
        yangpa_core::event::KeyRef::Record { root } => root,
        k => panic!("기록 키가 아님: {k:?}"),
    };
    connect_both(&a, &d).await;
    eventually("D ↔ A 인증", || async {
        testkit::authenticate(&d.node, a.peer()).await.ok()
    })
    .await;
    let r = testkit::net_request(
        &d.node,
        a.peer(),
        YpRequest::GetChannelRoots {
            space,
            channel: secret,
            ids: vec![old_root],
        },
    )
    .await
    .expect("요청");
    assert!(
        matches!(
            r,
            YpResponse::Error {
                code: WireErrorCode::HistoryPrivate,
                ..
            }
        ),
        "{r:?}"
    );

    // D가 쓴 메시지는 지금 키(크루·그룹 사슬 끝)로 모두가 읽음.
    let mine = send_when_ready(&d, space, open, "D의 공개").await;
    assert!(testkit::uses_crew_root(&a.node, &header(&d, &mine)));
    send_when_ready(&d, space, secret, "D의 비밀").await;
    for n in [&a, &b] {
        sees_text(n, space, open, "D의 공개").await;
        sees_text(n, space, secret, "D의 비밀").await;
    }
    assert_eq!(
        testkit::channel_key_seq(&d.node, &space, secret),
        testkit::channel_key_seq(&b.node, &space, secret)
    );

    // D가 확정 기기가 됨(체크포인트로 시작한 기기가 표를 넘겨받음). E를 추방하면 D가 회전을 내고, 남은 A·B의 상자 키는
    // 체크포인트 앞 가입이라 D에게 없어 `GetDeviceKeys`로 받아 봉인합니다.
    set_roles(&a, space, d.user(), vec![secret_role, admin]).await;
    a.ok::<m::CrewSetCommitter>(CommitterSetParams { space, device: dd })
        .await;
    // 바뀔 때마다 남겨 시간 초과 때 어느 조건이 남았는지 보입니다(출력은 실패한 시험만 보임).
    let mut seen = None;
    eventually("D가 표를 넘겨받음", || {
        let now = (
            testkit::committer_of(&d.node, &space) == Some(dd),
            testkit::has_key_table(&d.node, &space),
            testkit::has_key_table(&b.node, &space),
        );
        if seen.replace(now) != Some(now) {
            eprintln!("D 확정 기기·D 표·B 표: {now:?}");
        }
        async move { (now.0 && now.1 && !now.2).then_some(()) }
    })
    .await;
    let before = testkit::key_update_kinds(&d.node, &space, None);
    let e_dev = testkit::device_id(&e.node).unwrap();
    kick(&a, space, e.user()).await;
    eventually("D가 E를 뺀 회전을 냄", || async {
        (!crew_members(&a, space).contains(&e_dev)
            && testkit::crew_key_seq(&b.node, &space) == testkit::crew_key_seq(&d.node, &space))
        .then_some(())
    })
    .await;
    let kinds = testkit::key_update_kinds(&d.node, &space, None);
    let new: Vec<&str> = kinds
        .iter()
        .filter(|k| !before.contains(k))
        .map(|(_, k)| *k)
        .collect();
    assert!(
        !new.contains(&"full_rekey"),
        "넘겨받은 표로 이어야 함: {kinds:?}"
    );
    let last = send_when_ready(&d, space, open, "추방 뒤").await;
    for n in [&a, &b] {
        sees_text(n, space, open, "추방 뒤").await;
    }
    assert!(testkit::uses_crew_root(&b.node, &header(&d, &last)));
}

// ───────────── 경계 아래에 묻힌 프로필·뱃지·후원 표시 ─────────────

async fn name_of(n: &TestNode, user: UserId) -> Option<String> {
    common::call::<m::ProfileGet>(&n.node, ProfileGetParams { user: Some(user) })
        .await
        .ok()
        .map(|p| p.name)
}

async fn halo_of(n: &TestNode, user: UserId) -> bool {
    n.ok::<m::FlairList>(Empty)
        .await
        .iter()
        .any(|f| f.user == user && f.halo_allowed)
}

async fn sees_card(n: &TestNode, who: UserId, shown: ShownBadgeView) -> Option<()> {
    let badges = n.ok::<m::BadgeGet>(UserParams { user: who }).await;
    (name_of(n, who).await.as_deref() == Some("묻힌 이름") && badges.shown == vec![shown])
        .then_some(())
}

/// 주인과 직접 이어져 프로필을 주고받으면 받아오기를 시험하지 못하므로, 멤버에게 미리 걸지 않습니다.
fn low_no_redial(c: &mut NodeConfig) {
    low(c);
    c.redial_interval = Duration::from_secs(3600);
}

/// 체크포인트를 멈춰 둔 A·B·C 크루.
async fn three_members(cl: &Cluster) -> (TestNode, TestNode, TestNode, SpaceId) {
    let a = cl.node_with("a", low_no_redial).await;
    let b = cl.node_with("b", low_no_redial).await;
    let c = cl.node_with("c", low_no_redial).await;
    for n in [&a, &b, &c] {
        ct::hold(&n.node, true);
    }
    let space = create_crew(&a).await;
    assert_eq!(join(&a, &b, space).await, InviteJoinStatus::Joined);
    assert_eq!(join(&a, &c, space).await, InviteJoinStatus::Joined);
    wait_joined(&b, space).await;
    wait_joined(&c, space).await;
    (a, b, c, space)
}

/// 상태를 더 바꿔 체크포인트를 내고, C가 낸 `kinds` 이벤트가 모두 경계 아래로 묻혔는지 봅니다.
async fn bury(nodes: [&TestNode; 3], space: SpaceId, kinds: &[EventKind]) {
    let [a, b, c] = nodes;
    churn(a, space, 40, "앞").await;
    for n in nodes {
        ct::hold(&n.node, false);
    }
    wait_same_base(&[a, b, c], space, 1).await;
    ct::hold(&a.node, true);
    let floor = ct::floor(&a.node, &space);
    let who = c.user();
    let events = testkit::stream_events(&a.node, &space, StreamId::Space).expect("이벤트");
    let carried: Vec<_> = events
        .iter()
        .filter(|e| e.header.user == who && kinds.contains(&e.header.kind))
        .collect();
    assert!(!carried.is_empty(), "C의 이벤트가 크루에 실림");
    assert!(
        carried.iter().all(|e| e.header.lamport < floor),
        "C의 이벤트가 모두 경계 아래"
    );
}

/// A로 들어와 B와 머리를 견주는 새 멤버. 체크포인트로 들어와 경계 아래 기록이 없습니다.
async fn newcomer(cl: &Cluster, a: &TestNode, b: &TestNode, space: SpaceId) -> TestNode {
    let d = cl.node_with("d", low_no_redial).await;
    assert_eq!(join(a, &d, space).await, InviteJoinStatus::Joined);
    wait_joined(&d, space).await;
    assert!(ct::base(&d.node, &space).is_some(), "체크포인트로 들어옴");
    assert!(
        space_events(&d, space) < space_events(a, space),
        "경계 아래 기록을 받지 않음"
    );
    connect_both(&d, b).await;
    d
}

/// 체크포인트로 들어온 멤버는 경계 아래 이벤트를 받지 않지만, 묻힌 프로필·뱃지를 주인과 이어지지 않아도 멤버에게 물어 봅니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn newcomer_from_checkpoint_asks_members_for_buried_profiles_and_badges() {
    let cl = Cluster::new();
    let (a, b, c, space) = three_members(&cl).await;
    let who = c.user();
    let shown = ShownBadgeView {
        badge: Badge::Support3,
        after: false,
    };
    testkit::see_support(&c.node, 5000);
    c.ok::<m::ProfileSet>(ProfileSetParams {
        name: Some("묻힌 이름".into()),
        ..Default::default()
    })
    .await;
    c.ok::<m::BadgeSetShown>(BadgeSetShownParams { shown: vec![shown] })
        .await;
    eventually("A가 C의 프로필·뱃지를 받음", || {
        sees_card(&a, who, shown)
    })
    .await;
    bury(
        [&a, &b, &c],
        space,
        &[EventKind::MemberProfile, EventKind::MemberBadges],
    )
    .await;

    let d = newcomer(&cl, &a, &b, space).await;
    eventually("새 멤버가 묻힌 프로필·뱃지를 봄", || {
        sees_card(&d, who, shown)
    })
    .await;
    assert!(
        !testkit::is_authed(&d.node, c.peer()),
        "C와 직접 이어지지 않고 멤버에게 물어 받음"
    );
}

/// 후원 표시는 대신 전할 수 없어, 경계 아래로 묻힌 표시를 주인의 기기가 새 멤버를 보고 다시 냅니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn supporter_republishes_flair_buried_below_the_floor_for_a_newcomer() {
    let cl = Cluster::new();
    let (a, b, c, space) = three_members(&cl).await;
    let who = c.user();
    testkit::set_supporter_tier(&c.node, 5000);
    eventually("A가 C의 후광을 받음", || async {
        halo_of(&a, who).await.then_some(())
    })
    .await;
    bury([&a, &b, &c], space, &[EventKind::MemberFlair]).await;

    let d = newcomer(&cl, &a, &b, space).await;
    eventually("새 멤버가 묻힌 후광을 봄", || async {
        halo_of(&d, who).await.then_some(())
    })
    .await;
}

/// 프로필의 이미지 키가 공간 밖으로 가지 않게, 이 공간 멤버 기기에게 이 공간 멤버의 것만 답합니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn member_cards_go_only_to_members_and_only_for_members() {
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let b = cl.node("b").await;
    let outsider = cl.node("x").await;
    let space = create_crew(&a).await;
    assert_eq!(join(&a, &b, space).await, InviteJoinStatus::Joined);
    wait_joined(&b, space).await;

    let ask = |users: Vec<UserId>| YpRequest::GetMemberCards { space, users };
    let reply = eventually("A가 멤버에게 답함", || async {
        testkit::reauth(&b.node, a.peer()).await.ok()?;
        let req = ask(vec![a.user(), outsider.user()]);
        match testkit::net_request(&b.node, a.peer(), req).await.ok()? {
            YpResponse::MemberCards(items) => Some(items),
            _ => None,
        }
    })
    .await;
    let users: Vec<_> = reply
        .iter()
        .filter_map(|c| c.profile.as_ref().map(|p| p.user))
        .collect();
    assert_eq!(users, vec![a.user()], "멤버가 아닌 사람의 것은 답하지 않음");

    let r = testkit::net_request(
        &b.node,
        a.peer(),
        ask(vec![a.user(); MAX_MEMBER_CARDS_REQUEST + 1]),
    )
    .await
    .expect("답");
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

    // 인증은 되는 친구지만 이 크루 멤버는 아닙니다.
    common::befriend(&a, &outsider).await;
    let code = eventually("멤버가 아닌 기기에게는 답하지 않음", || async {
        testkit::reauth(&outsider.node, a.peer()).await.ok()?;
        let req = ask(vec![a.user()]);
        match testkit::net_request(&outsider.node, a.peer(), req)
            .await
            .ok()?
        {
            YpResponse::Error { code, .. } if code != WireErrorCode::Unauthenticated => Some(code),
            YpResponse::MemberCards(_) => panic!("멤버가 아닌 기기에게 답함"),
            _ => None,
        }
    })
    .await;
    assert_eq!(code, WireErrorCode::NotMember);
}
