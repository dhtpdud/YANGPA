//! 키 그룹(`KeyUpdate`) 통합 시험(키 갱신 3부). 데몬의 실제 경로(확정 기기 바퀴, 멤버 적용, 중계, 보내는 쪽 규칙,
//! 표 넘기기)로 protocol.md "키 그룹"·"기록 키"의 약속을 확인합니다.
//!
//! 1. 새 멤버는 회전 전 공개 기록의 뿌리를 멤버에게 물어(`GetChannelRoots`) 읽음.
//! 2. 추방된 멤버는 추방 뒤의 기록 키를 파생하지도, 멤버에게 받아 내지도 못함.
//! 3. 기록 비공개 채널에 새로 든 사람은 들기 전 기록을 읽지 못함(T는 한 방향 전진, 중계도 거절).
//! 4. 기록 공개를 끄면 그 그룹의 R을 새로 해(`RRefresh`), 그 뒤 들어온 사람이 공개 시절 뿌리를 파생하지 못함.
//! 5. 중계받은 뿌리는 키에서 계산한 ID가 물은 ID일 때만 씀(틀린 키·다른 ID·다른 채널은 버림).
//! 6. 보내는 쪽은 제거를 반영한 회전을 기다리고(`rotation_wait`), 엄격한 키 회전이면 끝없이 기다림.
//! 7. 확정 기기를 넘기면 표가 함께 가고(전체 재키 없음), 옛 확정 기기는 표를 버림.
//! 8. 관리자에서 내려온 옛 확정 기기(표 보유 기기)는 크루 전체 재키로 끊김: 그 기기가 남긴 표의 키는 지금 키가 아님.
//! 9. 기기 256대를 넘는 크루(트리 인코딩): 지어낸 멤버로 크기를 채워 가입·회전·새 실제 멤버를 확인.
//! 10. 가입 묶음보다 오래된 상태 키는 가입자가 멤버에게 물어(`GetStateKeys`) 상태를 끝까지 세움(묶음 크기는 크루 나이와
//!     상관없음). 멤버가 아니면 받지 못함.
//! 11. 보기 규칙이 있는 채널은 처음부터 비공개 그룹. 크루 채널의 기록 공개 끄기와 같은 바퀴의 가입: 가입 전진보다 크루 R
//!     새로 하기가 먼저(`crew_flip_with_a_join_in_the_same_round_refreshes_crew_r_first`).
//! 12. 기록 공개 끄기와 같은 바퀴의 가입: 그룹 R을 먼저 새로 하고 가입은 다음 바퀴.
//! 13. 자격을 잃은 옛 확정 기기의 표는 받지 않음(전체 재키로 이어받음).
//! 14. 다시 띄운 확정 기기는 사슬에 있는 저장 칸을 골라 이어 감(내기 전에 써 둔 칸은 고르지 않음).
//!     표가 사슬 끝보다 앞서면(낸 크루 갱신이 늦음) 사슬 끝 칸에서 다시 냄(전체 재키 없음).
//! 15. 전체 재키 몫은 `GetKeyShard`로 기기 몫만, 멤버에게만, 예산 안에서.
//! 16. 역할을 잃은 멤버는 Rotate 전이라도 그룹의 지금 T·R을 중계받지 못함(`GetGroupKeys`는 지금 자격을 봄).
//!
//! 확정 기기를 잃고 표 없이 전체 재키로 이어받는 경우는 robustness.rs 5번이 봅니다.

use crate::common;

use std::time::{Duration, Instant};

use common::{Cluster, TestNode, connect_both, eventually};
use yangpa_core::body::Secret32;
use yangpa_core::event::StreamId;
use yangpa_core::ids::{ChannelId, DeviceId, EventId, RoleId, SpaceId, UserId};
use yangpa_core::links::InviteLink;
use yangpa_core::model::{ChannelKind, HistoryVisibility};
use yangpa_core::wire::{WireErrorCode, YpRequest, YpResponse};
use yangpa_daemon::testkit;
use yangpa_rpc::api::{
    ChannelCreateParams, ChannelUpdateParams, CommitterSetParams, CrewCreateParams,
    CrewUpdateParams, InviteCreateParams, InviteJoinStatus, MemberKickParams, MemberSetRolesParams,
    MessageSendParams, RoleCreateParams, SpaceParams, UrlParams, m,
};

// ───────────── 도우미 ─────────────

fn dev(n: &TestNode) -> DeviceId {
    testkit::device_id(&n.node).expect("기기 ID")
}

async fn create_crew(a: &TestNode, name: &str) -> SpaceId {
    a.ok::<m::CrewCreate>(CrewCreateParams {
        name: name.into(),
        description: None,
    })
    .await
    .space
}

async fn invite_url(gateway: &TestNode, space: SpaceId, uses: Option<u32>) -> String {
    gateway
        .ok::<m::InviteCreate>(InviteCreateParams {
            space,
            expires_in_secs: None,
            max_uses: uses,
            roles: Vec::new(),
        })
        .await
        .url
}

/// `joiner`가 `gateway`의 초대 링크로 들어와 크루 키를 얻을 때까지(둘만 연결).
async fn join(gateway: &TestNode, joiner: &TestNode, space: SpaceId) {
    connect_both(gateway, joiner).await;
    let url = invite_url(gateway, space, Some(1)).await;
    let r = joiner.ok::<m::InviteJoin>(UrlParams { url }).await;
    assert_eq!(r.space, space);
    assert_eq!(r.status, InviteJoinStatus::Joined, "{} 가입", joiner.name);
    let me = joiner.user();
    eventually(&format!("{} 가입 완료", joiner.name), || async {
        let st = testkit::space_state(&joiner.node, &space)?;
        (st.is_member(&me) && testkit::crew_key_seq(&joiner.node, &space).is_some()).then_some(())
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

/// 모두 같은 크루 키 순번에 이를 때까지.
async fn wait_same_crew_key(nodes: &[&TestNode], space: SpaceId) -> u64 {
    eventually("같은 크루 키 순번", || async {
        let first = testkit::crew_key_seq(&nodes[0].node, &space)?;
        nodes
            .iter()
            .all(|n| testkit::crew_key_seq(&n.node, &space) == Some(first))
            .then_some(first)
    })
    .await
}

/// 크루 그룹에서 이 기기가 빠지고 남은 노드가 같은 순번에 이를 때까지.
async fn wait_removed_from_crew(nodes: &[&TestNode], space: SpaceId, gone: DeviceId) {
    eventually("크루 그룹에서 빠짐", || async {
        let members = testkit::key_group_members(&nodes[0].node, &space, None)?;
        let first = testkit::crew_key_seq(&nodes[0].node, &space)?;
        (!members.contains(&gone)
            && nodes
                .iter()
                .all(|n| testkit::crew_key_seq(&n.node, &space) == Some(first)))
        .then_some(())
    })
    .await;
}

/// 서로 멤버로 본 뒤 직접 인증합니다(시험에는 위치 찾기가 없어 가입 전 연결은 인증되지 않은 채 남음).
async fn meet(x: &TestNode, y: &TestNode, space: SpaceId) {
    connect_both(x, y).await;
    wait_member(x, space, y.user(), true).await;
    wait_member(y, space, x.user(), true).await;
    eventually(&format!("{} ↔ {} 인증", x.name, y.name), || async {
        testkit::authenticate(&x.node, y.peer()).await.ok()
    })
    .await;
}

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

/// 기록 공개 채널(전원이 봄: 크루 키를 씀).
async fn public_channel(a: &TestNode, space: SpaceId, name: &str) -> ChannelId {
    let mut p = channel_params(space, name);
    p.history = Some(HistoryVisibility::Public);
    a.ok::<m::ChannelCreate>(p).await.channel
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

async fn kick(a: &TestNode, space: SpaceId, user: UserId) {
    a.ok::<m::MemberKick>(MemberKickParams {
        space,
        user,
        reason: None,
    })
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

/// 보낼 수 있을 때까지(그룹에 막 들어와 그룹 키를 기다리는 중이면 다시) 보냅니다.
async fn send_when_ready(n: &TestNode, space: SpaceId, channel: ChannelId, text: &str) -> EventId {
    eventually(&format!("{}가 '{text}'를 보냄", n.name), || async {
        n.call::<m::MessageSend>(send_params(space, channel, text))
            .await
            .ok()
            .map(|r| r.id)
    })
    .await
}

fn header(n: &TestNode, id: &EventId) -> yangpa_core::event::EventHeader {
    testkit::get_event(&n.node, id)
        .expect("저장소")
        .expect("이벤트")
        .header
}

/// 메시지 머리의 채널 뿌리 ID.
fn root_id(n: &TestNode, id: &EventId) -> yangpa_core::ids::ChannelRootId {
    match header(n, id).key {
        yangpa_core::event::KeyRef::Record { root } => root,
        k => panic!("기록 키가 아님: {k:?}"),
    }
}

/// 채널 스트림 전부를 `to`에 억지로 넣습니다(보낸 노드가 거절해도 데이터는 손에 넣었다고 침).
async fn force_channel(from: &TestNode, to: &TestNode, space: SpaceId, channel: ChannelId) {
    let evs = testkit::stream_events(&from.node, &space, StreamId::Channel(channel)).unwrap();
    testkit::ingest_from(&to.node, space, evs, from.peer())
        .await
        .unwrap();
}

fn last_crew_kind(n: &TestNode, space: SpaceId) -> Option<&'static str> {
    testkit::key_update_kinds(&n.node, &space, None)
        .last()
        .map(|(_, k)| *k)
}

/// 크루 사슬에 이 종류의 갱신이 `after` 순번보다 뒤에 받아들여질 때까지.
async fn wait_crew_kind(n: &TestNode, space: SpaceId, after: u64, kind: &str) -> u64 {
    eventually(
        &format!("{}가 크루 {kind} 갱신을 받아들임", n.name),
        || async {
            testkit::key_update_kinds(&n.node, &space, None)
                .into_iter()
                .find(|(s, k)| *s > after && *k == kind)
                .map(|(s, _)| s)
        },
    )
    .await
}

async fn rotation_pending(n: &TestNode, space: SpaceId) -> bool {
    n.ok::<m::SpaceGet>(SpaceParams { space })
        .await
        .key_rotation_pending
}

// ───────────── 1. 공개 기록은 중계로 ─────────────

/// 회전(추방) 전에 쓴 기록 공개 채널의 메시지는 새 R에서 파생되지 않습니다. 회전 뒤 들어온 D는 그 뿌리를 멤버(A)에게
/// `GetChannelRoots`로 물어 받고(답하는 쪽이 기록 공개·열람 권한 확인) 읽습니다. 회전 뒤 메시지는 받은 R로 바로 읽습니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn newcomer_reads_public_history_through_channel_roots() {
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let c = cl.node("c").await;
    let d = cl.node("d").await;
    let space = create_crew(&a, "기록 크루").await;
    let open = public_channel(&a, space, "공개 기록").await;
    join(&a, &c, space).await;
    let old = send_when_ready(&a, space, open, "회전 전 공개 기록").await;
    sees_text(&c, space, open, "회전 전 공개 기록").await;

    let before = wait_same_crew_key(&[&a, &c], space).await;
    kick(&a, space, c.user()).await;
    wait_removed_from_crew(&[&a], space, dev(&c)).await;
    wait_crew_kind(&a, space, before, "rotate").await;
    let new = send_when_ready(&a, space, open, "회전 뒤 공개 기록").await;

    join(&a, &d, space).await;
    sees_text(&d, space, open, "회전 뒤 공개 기록").await;
    sees_text(&d, space, open, "회전 전 공개 기록").await;
    // 회전 전 뿌리는 D가 가진 그룹 상태로는 파생되지 않아(새 R) 중계로 받은 것입니다.
    assert!(!testkit::can_derive_root(&d.node, &header(&a, &old)));
    assert!(testkit::has_root(&d.node, &root_id(&a, &old)));
    assert!(testkit::can_derive_root(&d.node, &header(&a, &new)));
}

// ───────────── 2. 추방 뒤 기록 키 ─────────────

/// 추방된 C는 추방 뒤의 메시지·상태를 받아도 기록 키를 파생하지 못하고(그룹 키가 추방 전 순번에 머묾), 남은 멤버에게
/// 뿌리·그룹 키를 물어도 받지 못합니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn kicked_member_cannot_derive_post_kick_record_keys() {
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let b = cl.node("b").await;
    let c = cl.node("c").await;
    let space = create_crew(&a, "추방 크루").await;
    let open = public_channel(&a, space, "공개").await;
    join(&a, &b, space).await;
    join(&a, &c, space).await;
    meet(&b, &c, space).await;
    wait_same_crew_key(&[&a, &b, &c], space).await;
    send_when_ready(&a, space, open, "추방 전").await;
    sees_text(&c, space, open, "추방 전").await;
    let c_before = testkit::member_key_digest(&c.node, &space, None).expect("C의 크루 키");

    kick(&a, space, c.user()).await;
    wait_member(&b, space, c.user(), false).await;
    wait_removed_from_crew(&[&a, &b], space, dev(&c)).await;
    let new = send_when_ready(&a, space, open, "추방 뒤").await;
    sees_text(&b, space, open, "추방 뒤").await;

    // 데이터를 손에 넣어도(시더가 실수로 줌) 파생할 키가 없습니다.
    force_channel(&a, &c, space, open).await;
    let evs = testkit::stream_events(&a.node, &space, StreamId::Space).unwrap();
    testkit::ingest_from(&c.node, space, evs, a.peer())
        .await
        .unwrap();
    let h = header(&a, &new);
    assert!(
        !testkit::can_derive_root(&c.node, &h),
        "추방된 C가 새 뿌리를 파생함"
    );
    assert!(testkit::record_root_of(&c.node, &h).is_none());
    assert!(testkit::message_text(&c.node, &new).is_none());
    // C는 새 사슬 끝의 상태를 얻지 못하고(해소기의 끝이 C가 모르는 약속을 가리킴), 가진 상태는 추방 전 순번에 머물며,
    // 남은 멤버의 지금 키와 다릅니다.
    assert!(
        testkit::member_key_digest(&c.node, &space, None).is_none(),
        "추방된 C가 새 크루 키를 얻음"
    );
    let held = testkit::held_key_digests(&c.node, &space, None);
    let c_max = held.iter().map(|(s, _)| *s).max().expect("C의 옛 크루 키");
    assert_eq!(c_max, c_before.1, "추방된 C의 크루 키가 나아감: {held:?}");
    let b_now = testkit::member_key_digest(&b.node, &space, None).expect("B의 크루 키");
    assert!(b_now.1 > c_max);
    assert!(
        held.iter().all(|(_, d)| *d != b_now.2),
        "C가 남은 멤버의 지금 키를 가짐"
    );

    // 남은 멤버에게 물어도 받지 못합니다(멤버가 아님·인증 전 어느 쪽이든 키 없음).
    let b_state_key = testkit::current_state_key_id(&b.node, &space).expect("B의 상태 키");
    for req in [
        YpRequest::GetChannelRoots {
            space,
            channel: open,
            ids: vec![root_id(&a, &new)],
        },
        YpRequest::GetGroupKeys {
            space,
            group: yangpa_core::ids::KeyGroupId::crew(&space),
        },
        YpRequest::GetStateKeys {
            space,
            ids: vec![b_state_key],
        },
    ] {
        let got = testkit::net_request(&c.node, b.peer(), req).await;
        assert!(
            !matches!(got, Ok(YpResponse::SealedKeys(_))),
            "추방된 C에게 키를 중계함"
        );
    }
    assert!(!testkit::has_root(&c.node, &root_id(&a, &new)));
}

// ───────────── 3. 기록 비공개 채널의 새 멤버 ─────────────

/// 기록 비공개 채널의 그룹에 새로 든 C는 T가 한 방향으로 전진한 상태만 받아 들기 전 메시지의 뿌리를 파생하지 못하고,
/// 멤버에게 물어도 `HistoryPrivate`로 거절당합니다. 든 뒤의 메시지는 읽습니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn private_channel_joiner_cannot_read_pre_join_history() {
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let b = cl.node("b").await;
    let c = cl.node("c").await;
    let space = create_crew(&a, "비밀 크루").await;
    join(&a, &b, space).await;
    join(&a, &c, space).await;
    let role = create_role(&a, space, "비밀", &[], 5).await;
    set_roles(&a, space, b.user(), vec![role]).await;
    let mut p = channel_params(space, "비밀방");
    p.private_roles = Some(vec![role]);
    p.history = Some(HistoryVisibility::Private);
    let secret = a.ok::<m::ChannelCreate>(p).await.channel;
    eventually("B가 비밀방 그룹에 들어감", || async {
        testkit::channel_key_seq(&b.node, &space, secret)
    })
    .await;
    let before = send_when_ready(&a, space, secret, "들기 전 비밀").await;
    sees_text(&b, space, secret, "들기 전 비밀").await;

    let c_dev = dev(&c);
    set_roles(&a, space, c.user(), vec![role]).await;
    eventually("C가 비밀방 그룹에 들어감", || async {
        let m = testkit::key_group_members(&a.node, &space, Some(secret))?;
        (m.contains(&c_dev) && testkit::channel_key_seq(&c.node, &space, secret).is_some())
            .then_some(())
    })
    .await;
    // 가입만이라 그룹 사슬의 마지막 갱신은 전진(`advance`)입니다.
    let kinds = testkit::key_update_kinds(&a.node, &space, Some(secret));
    assert_eq!(kinds.last().map(|(_, k)| *k), Some("advance"), "{kinds:?}");
    let after = send_when_ready(&a, space, secret, "든 뒤 비밀").await;
    sees_text(&c, space, secret, "든 뒤 비밀").await;

    force_channel(&a, &c, space, secret).await;
    assert!(!testkit::can_derive_root(&c.node, &header(&a, &before)));
    assert!(
        testkit::message_text(&c.node, &before).is_none(),
        "들기 전 기록을 읽음"
    );
    assert!(testkit::message_text(&c.node, &after).is_some());
    meet(&a, &c, space).await;
    let r = testkit::net_request(
        &c.node,
        a.peer(),
        YpRequest::GetChannelRoots {
            space,
            channel: secret,
            ids: vec![root_id(&a, &before)],
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
}

// ───────────── 4. 기록 공개 → 비공개 ─────────────

/// 크루 키를 쓰는 기록 공개 채널의 기록 공개를 끄면 확정 기기가 크루 그룹의 R만 새로 합니다(`r_refresh`, T는 그대로).
/// 그 뒤 들어온 D는 새 R만 받아 공개 시절 뿌리를 파생하지 못하고, 중계도 `HistoryPrivate`로 거절됩니다. 이 회전이 없으면
/// 가입은 R을 그대로 두므로 D가 끈 뒤에도 지난 기록을 읽게 됩니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_to_private_flip_refreshes_r() {
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let b = cl.node("b").await;
    let d = cl.node("d").await;
    let space = create_crew(&a, "뒤집는 크루").await;
    let ch = public_channel(&a, space, "한때 공개").await;
    join(&a, &b, space).await;
    let old = send_when_ready(&a, space, ch, "공개 시절").await;
    sees_text(&b, space, ch, "공개 시절").await;
    let seq0 = wait_same_crew_key(&[&a, &b], space).await;
    let (_, _, before) = testkit::member_key_digest(&a.node, &space, None).expect("크루 키");

    let mut up = ChannelUpdateParams::new(space, ch);
    up.history = Some(HistoryVisibility::Private);
    a.ok::<m::ChannelUpdate>(up).await;
    let seq1 = wait_crew_kind(&a, space, seq0, "r_refresh").await;
    assert_eq!(seq1, seq0 + 1);
    let (_, _, after) = testkit::member_key_digest(&a.node, &space, None).expect("크루 키");
    assert_ne!(before, after);
    wait_same_crew_key(&[&a, &b], space).await;
    // 이미 읽던 B는 그대로 읽습니다.
    assert!(testkit::message_text(&b.node, &old).is_some());

    join(&a, &d, space).await;
    let late = send_when_ready(&a, space, ch, "비공개 뒤").await;
    sees_text(&d, space, ch, "비공개 뒤").await;
    force_channel(&a, &d, space, ch).await;
    assert!(
        !testkit::can_derive_root(&d.node, &header(&a, &old)),
        "끈 뒤 들어온 D가 공개 시절 뿌리를 파생함"
    );
    assert!(testkit::message_text(&d.node, &old).is_none());
    assert!(testkit::message_text(&d.node, &late).is_some());
    let r = testkit::net_request(
        &d.node,
        a.peer(),
        YpRequest::GetChannelRoots {
            space,
            channel: ch,
            ids: vec![root_id(&a, &old)],
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
}

// ───────────── 5. 중계받은 뿌리는 ID로 확인 ─────────────

/// 멤버가 중계 답에 틀린 키, 다른 메시지의 키, 다른 채널의 뿌리를 넣으면 받는 쪽은 키에서 계산한 ID가 물은 ID와 달라
/// 버립니다(그 ID의 메시지를 엉뚱한 키로 열지 않음). 맞는 키는 받아 그 메시지를 엽니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn relayed_root_with_wrong_key_is_rejected_by_id() {
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let c = cl.node("c").await;
    let d = cl.node("d").await;
    let space = create_crew(&a, "중계 크루").await;
    let ch = public_channel(&a, space, "공개").await;
    let other = public_channel(&a, space, "다른 공개").await;
    join(&a, &c, space).await;
    let m1 = send_when_ready(&a, space, ch, "회전 전 하나").await;
    let m2 = send_when_ready(&a, space, other, "회전 전 다른 채널").await;
    let before = wait_same_crew_key(&[&a, &c], space).await;
    kick(&a, space, c.user()).await;
    wait_crew_kind(&a, space, before, "rotate").await;
    let m3 = send_when_ready(&a, space, ch, "회전 뒤").await;

    // D는 중계를 묻지 않게 붙잡아 두고 들어옵니다(답을 시험이 직접 넣음).
    testkit::hold_root_relay(&d.node, true);
    join(&a, &d, space).await;
    sees_text(&d, space, ch, "회전 뒤").await;
    force_channel(&a, &d, space, ch).await;
    let id1 = root_id(&a, &m1);
    assert!(!testkit::has_root(&d.node, &id1));
    assert!(testkit::message_text(&d.node, &m1).is_none());
    let d_user = d.user();
    let d_dev = dev(&d);
    let seal = |roots: Vec<(ChannelId, Secret32)>| {
        testkit::seal_relayed_roots(&a.node, &space, &d_user, &d_dev, roots).expect("봉인")
    };

    // 틀린 키, 다른 메시지(회전 뒤)의 맞는 키, 다른 채널 뿌리.
    let (_, k3) = testkit::record_root_of(&a.node, &header(&a, &m3)).expect("A의 뿌리");
    let (_, k2) = testkit::record_root_of(&a.node, &header(&a, &m2)).expect("A의 뿌리");
    for (what, roots) in [
        ("틀린 키", vec![(ch, Secret32::random())]),
        ("다른 메시지의 키", vec![(ch, k3)]),
        ("다른 채널 뿌리", vec![(other, k2.clone())]),
        ("다른 채널이라 적은 같은 키", vec![(ch, k2)]),
    ] {
        let n = testkit::accept_relayed_roots(&d.node, space, ch, vec![id1], &seal(roots)).await;
        assert_eq!(n, 0, "{what}을 받아들임");
        assert!(!testkit::has_root(&d.node, &id1), "{what}");
    }
    assert!(testkit::message_text(&d.node, &m1).is_none());

    let (_, k1) = testkit::record_root_of(&a.node, &header(&a, &m1)).expect("A의 뿌리");
    let n =
        testkit::accept_relayed_roots(&d.node, space, ch, vec![id1], &seal(vec![(ch, k1)])).await;
    assert_eq!(n, 1);
    sees_text(&d, space, ch, "회전 전 하나").await;
}

// ───────────── 6. 보내는 쪽 규칙 ─────────────

/// 확정 기기(A)가 회전을 미루는 동안 C를 추방하면, B는 보내기 전에 `rotation_wait`(시험 2초)만큼 회전을 기다렸다가 옛
/// 키로 보내고 `key_rotation_pending`을 켭니다(그 메시지는 나간 C도 읽을 수 있음: 문서의 남는 위험). 엄격한 키 회전을
/// 켜면 회전이 올 때까지 보내지 않고, 회전 뒤 새 키로 보내 C가 읽지 못합니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sender_waits_for_pending_rotation_and_strict_mode_blocks() {
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let b = cl.node("b").await;
    let c = cl.node("c").await;
    let space = create_crew(&a, "기다리는 크루").await;
    let ch = public_channel(&a, space, "공개").await;
    join(&a, &b, space).await;
    join(&a, &c, space).await;
    meet(&b, &c, space).await;
    wait_same_crew_key(&[&a, &b, &c], space).await;
    send_when_ready(&b, space, ch, "준비").await;
    assert!(!rotation_pending(&b, space).await);

    testkit::hold_commits(&a.node, true);
    kick(&a, space, c.user()).await;
    wait_member(&b, space, c.user(), false).await;

    let t = Instant::now();
    let weak = b
        .ok::<m::MessageSend>(send_params(space, ch, "기다리다 옛 키로"))
        .await
        .id;
    assert!(
        t.elapsed() >= Duration::from_millis(1500),
        "회전을 기다리지 않음: {:?}",
        t.elapsed()
    );
    assert!(
        rotation_pending(&b, space).await,
        "옛 키로 보냈는데 표시가 없음"
    );
    // 남는 위험: 회전 전에 옛 키로 보낸 글은 나간 C도 엽니다.
    force_channel(&b, &c, space, ch).await;
    assert_eq!(
        testkit::message_text(&c.node, &weak).as_deref(),
        Some("기다리다 옛 키로")
    );

    // 엄격한 키 회전: 회전이 올 때까지 보내지 않습니다.
    a.ok::<m::CrewUpdate>(CrewUpdateParams {
        space,
        name: None,
        description: None,
        icon_path: None,
        strict_rotation: Some(true),
        emoji_limits: None,
    })
    .await;
    eventually("B가 엄격한 키 회전을 봄", || async {
        testkit::space_state(&b.node, &space)?
            .settings
            .strict_rotation
            .then_some(())
    })
    .await;
    let node = b.node.clone();
    let strict = tokio::spawn(async move {
        common::call::<m::MessageSend>(&node, send_params(space, ch, "엄격하게 새 키로")).await
    });
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert!(!strict.is_finished(), "엄격한 키 회전인데 회전 전에 보냄");
    testkit::hold_commits(&a.node, false);
    let sent = strict.await.expect("작업").expect("보내기").id;
    wait_removed_from_crew(&[&a, &b], space, dev(&c)).await;
    sees_text(&a, space, ch, "엄격하게 새 키로").await;
    force_channel(&b, &c, space, ch).await;
    assert!(
        testkit::message_text(&c.node, &sent).is_none(),
        "나간 C가 엄격한 글을 읽음"
    );
    eventually("회전 뒤 표시가 꺼짐", || async {
        (!rotation_pending(&b, space).await).then_some(())
    })
    .await;
}

// ───────────── 7. 확정 기기 넘기기 ─────────────

/// 소유자가 관리자 B의 기기를 확정 기기로 정하면 A가 표를 B에게 넘기고(전체 재키 없이 이어 감) 표를 버립니다. B는
/// 넘겨받은 표로 새 멤버 C를 넣고(가입 전진), 모두 계속 주고받습니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn committer_handover_keeps_the_table_without_full_rekey() {
    common::init_tracing();
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let b = cl.node("b").await;
    let c = cl.node("c").await;
    let space = create_crew(&a, "넘기는 크루").await;
    let ch = public_channel(&a, space, "공개").await;
    join(&a, &b, space).await;
    let admin = create_role(&a, space, "관리", &["administrator"], 10).await;
    set_roles(&a, space, b.user(), vec![admin]).await;
    let seq0 = wait_same_crew_key(&[&a, &b], space).await;
    assert!(testkit::has_key_table(&a.node, &space));
    assert!(!testkit::has_key_table(&b.node, &space));
    let rekeys_before = testkit::key_update_kinds(&a.node, &space, None)
        .iter()
        .filter(|(_, k)| *k == "full_rekey")
        .count();

    let db = dev(&b);
    a.ok::<m::CrewSetCommitter>(CommitterSetParams { space, device: db })
        .await;
    eventually("B가 표를 넘겨받음", || async {
        (testkit::committer_of(&b.node, &space) == Some(db)
            && testkit::has_key_table(&b.node, &space)
            && !testkit::has_key_table(&a.node, &space))
        .then_some(())
    })
    .await;

    // 새 확정 기기 B가 C를 넣습니다(넘겨받은 표로 이어서: 전체 재키 없음).
    join(&b, &c, space).await;
    meet(&a, &c, space).await;
    wait_same_crew_key(&[&a, &b, &c], space).await;
    let kinds = testkit::key_update_kinds(&b.node, &space, None);
    let after: Vec<&str> = kinds
        .iter()
        .filter(|(s, _)| *s > seq0)
        .map(|(_, k)| *k)
        .collect();
    assert!(after.contains(&"advance"), "{kinds:?}");
    assert_eq!(
        kinds.iter().filter(|(_, k)| *k == "full_rekey").count(),
        rekeys_before,
        "표를 넘겼는데 전체 재키함: {kinds:?}"
    );
    let tip = testkit::crew_tip(&b.node, &space).expect("크루 사슬");
    assert!(tip.holders.contains(&dev(&a)), "넘긴 A는 보유 기기로 남음");
    send_when_ready(&c, space, ch, "새 확정 기기 아래").await;
    sees_text(&a, space, ch, "새 확정 기기 아래").await;
    sees_text(&b, space, ch, "새 확정 기기 아래").await;
}

// ───────────── 8. 내려온 옛 확정 기기 ─────────────

/// 관리자 B가 확정 기기였다가 소유자에게 확정 기기를 돌려준 뒤 관리자에서 내려오면, B는 표 보유 기기 자격을 잃어
/// 다음 크루 갱신이 전체 재키여야 합니다(`rekey_required`). 재키 뒤 보유 기기에서 B가 빠지고, B가 몰래 남겨 둔 표의 크루·
/// 비공개 그룹 키는 지금 키가 아니며, B(멤버로는 남음)는 볼 수 없게 된 비공개 채널의 새 메시지를 읽지 못합니다. D는
/// 비밀방을 못 보는 멤버라 비밀방이 크루와 다른 그룹을 씁니다(모두가 보면 크루 키를 씀).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn demoted_ex_committer_is_cut_off_by_full_rekey() {
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let b = cl.node("b").await;
    let c = cl.node("c").await;
    let d = cl.node("d").await;
    let space = create_crew(&a, "강등 크루").await;
    let open = public_channel(&a, space, "공개").await;
    join(&a, &b, space).await;
    join(&a, &c, space).await;
    join(&a, &d, space).await;
    meet(&b, &c, space).await;
    let (da, db) = (dev(&a), dev(&b));
    let admin = create_role(&a, space, "관리", &["administrator"], 10).await;
    let secret_role = create_role(&a, space, "비밀", &[], 5).await;
    set_roles(&a, space, b.user(), vec![admin]).await;
    set_roles(&a, space, c.user(), vec![secret_role]).await;
    let mut p = channel_params(space, "비밀방");
    p.private_roles = Some(vec![secret_role]);
    let secret = a.ok::<m::ChannelCreate>(p).await.channel;
    eventually("비밀방 그룹(A·B·C)", || async {
        let m = testkit::key_group_members(&a.node, &space, Some(secret))?;
        (m.contains(&db) && m.contains(&dev(&c)) && !m.contains(&dev(&d))).then_some(())
    })
    .await;

    // B가 확정 기기가 되어 표를 받고, 다시 A에게 돌려줍니다.
    a.ok::<m::CrewSetCommitter>(CommitterSetParams { space, device: db })
        .await;
    // 넘기기는 크루 하나 + 그룹마다 하나라, 크루가 먼저 닿은 뒤 비밀방 그룹이 따로 옵니다.
    let crew_g = testkit::member_key_digest(&a.node, &space, None)
        .expect("크루")
        .0;
    let secret_g = testkit::member_key_digest(&a.node, &space, Some(secret))
        .expect("비밀방")
        .0;
    let kept = eventually("B가 표(크루·비밀방)를 받음", || async {
        let snap = testkit::key_table_snapshot(&b.node, &space)?;
        let d = testkit::key_table_digests(&b.node, &space, &snap)?;
        (d.contains_key(&crew_g) && d.contains_key(&secret_g)).then_some(d)
    })
    .await;
    a.ok::<m::CrewSetCommitter>(CommitterSetParams { space, device: da })
        .await;
    eventually("A가 표를 돌려받음", || async {
        (testkit::committer_of(&a.node, &space) == Some(da)
            && testkit::has_key_table(&a.node, &space)
            && !testkit::has_key_table(&b.node, &space))
        .then_some(())
    })
    .await;
    let tip = testkit::crew_tip(&a.node, &space).expect("크루 사슬");
    assert!(tip.holders.contains(&db), "넘겨받았던 B는 보유 기기");
    let seq0 = tip.seq;

    // B를 관리자에서 내립니다: 보유 기기 자격을 잃어 크루 전체 재키가 요구됩니다.
    set_roles(&a, space, b.user(), Vec::new()).await;
    wait_crew_kind(&a, space, seq0, "full_rekey").await;
    eventually("재키 뒤 보유 기기에서 B가 빠짐", || async {
        let t = testkit::crew_tip(&a.node, &space)?;
        (!t.holders.contains(&db) && !t.rekey_required).then_some(())
    })
    .await;
    // 비밀방 그룹에서도 B가 빠지고(볼 수 없음) 새 T를 냅니다.
    eventually("비밀방 그룹에서 B가 빠짐", || async {
        let m = testkit::key_group_members(&a.node, &space, Some(secret))?;
        (!m.contains(&db)).then_some(())
    })
    .await;
    wait_same_crew_key(&[&a, &b, &c], space).await;

    // B가 남긴 표의 키는 어느 그룹에서도 지금 키가 아닙니다.
    let (crew_g, _, crew_now) = testkit::member_key_digest(&a.node, &space, None).expect("크루");
    let (secret_g, _, secret_now) =
        testkit::member_key_digest(&a.node, &space, Some(secret)).expect("비밀방");
    assert!(
        kept.contains_key(&crew_g) && kept.contains_key(&secret_g),
        "{kept:?}"
    );
    assert_ne!(kept[&crew_g].1, crew_now, "남긴 표에 지금 크루 키가 있음");
    assert_ne!(
        kept[&secret_g].1, secret_now,
        "남긴 표에 지금 비밀방 키가 있음"
    );

    // B는 멤버로 남아 공개 채널을 계속 읽고, 비밀방의 새 글은 받아도 읽지 못합니다.
    send_when_ready(&a, space, open, "재키 뒤 공개").await;
    sees_text(&b, space, open, "재키 뒤 공개").await;
    let hidden = send_when_ready(&a, space, secret, "재키 뒤 비밀").await;
    sees_text(&c, space, secret, "재키 뒤 비밀").await;
    force_channel(&a, &b, space, secret).await;
    assert!(
        testkit::message_text(&b.node, &hidden).is_none(),
        "내려온 B가 비밀방을 읽음"
    );
}

// ───────────── 9. 256대를 넘는 크루(트리) ─────────────

/// 지어낸 사용자 5명(사용자마다 기기 52대)을 넣어 크루가 직접 인코딩 상한(256대)을 넘게 하면 확정 기기가 트리로
/// 다시 세우고(기기 몫은 페이로드 밖: 받은편지함·조각 블롭), 실제 멤버 B가 트리에서 자기 몫을 받아 계속 주고받습니다.
/// 지어낸 사용자 하나를 추방하면 트리 회전, 실제 새 멤버 C는 트리 가입으로 들어옵니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crew_above_direct_limit_uses_tree_encoding() {
    const USERS: usize = 5;
    const DEVICES_EACH: usize = 52;
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let b = cl.node("b").await;
    let c = cl.node("c").await;
    let space = create_crew(&a, "큰 크루").await;
    let ch = public_channel(&a, space, "공개").await;
    join(&a, &b, space).await;
    let seq0 = wait_same_crew_key(&[&a, &b], space).await;
    assert_eq!(testkit::crew_tip(&a.node, &space).expect("사슬").depth, 0);

    let url = invite_url(&a, space, None).await;
    let token = InviteLink::parse(&url).expect("초대 링크").token;
    let joins = testkit::synthetic_members(&a.node, space, &token, USERS, DEVICES_EACH)
        .await
        .expect("지어낸 가입");
    testkit::ingest_from(&b.node, space, joins.clone(), a.peer())
        .await
        .expect("B에 넣기");
    let tip = eventually("크루가 트리로 바뀜", || async {
        let t = testkit::crew_tip(&a.node, &space)?;
        (t.depth > 0 && t.members.len() == 2 + USERS * DEVICES_EACH).then_some(t)
    })
    .await;
    assert!(tip.seq > seq0);
    let tree_seq = wait_same_crew_key(&[&a, &b], space).await;
    assert!(tree_seq >= tip.seq);
    send_when_ready(&a, space, ch, "트리 크루에서 A").await;
    sees_text(&b, space, ch, "트리 크루에서 A").await;
    send_when_ready(&b, space, ch, "트리 크루에서 B").await;
    sees_text(&a, space, ch, "트리 크루에서 B").await;

    // 지어낸 사용자 하나를 추방: 트리 회전(제거 경로만 새로).
    let gone = testkit::space_state(&a.node, &space)
        .expect("상태")
        .members
        .keys()
        .copied()
        .find(|u| *u != a.user() && *u != b.user())
        .expect("지어낸 사용자");
    let before = testkit::crew_tip(&a.node, &space).expect("사슬").seq;
    kick(&a, space, gone).await;
    let rot = wait_crew_kind(&a, space, before, "rotate").await;
    let t = testkit::crew_tip(&a.node, &space).expect("사슬");
    assert!(t.depth > 0, "회전 뒤에도 트리");
    assert_eq!(t.members.len(), 2 + (USERS - 1) * DEVICES_EACH);
    eventually("B가 회전을 따라감", || async {
        (testkit::crew_key_seq(&b.node, &space)? >= rot).then_some(())
    })
    .await;
    send_when_ready(&a, space, ch, "회전 뒤 트리").await;
    sees_text(&b, space, ch, "회전 뒤 트리").await;

    // 실제 새 멤버 C: 트리 가입(잎·경로를 받은편지함 봉인 묶음으로).
    join(&a, &c, space).await;
    meet(&b, &c, space).await;
    wait_same_crew_key(&[&a, &b, &c], space).await;
    assert!(testkit::crew_tip(&c.node, &space).expect("사슬").depth > 0);
    send_when_ready(&c, space, ch, "트리에 든 C").await;
    sees_text(&a, space, ch, "트리에 든 C").await;
    sees_text(&b, space, ch, "트리에 든 C").await;
    assert_eq!(last_crew_kind(&a, space), Some("advance"));
}

// ───────────── 10. 옛 상태 키는 물어서 ─────────────

/// 가입 묶음에 지금 상태 키 하나만 싣게 해 두고(시험용, 기본은 최근 64개) 크루 상태 키를 세 번 회전하며 그사이 채널을
/// 만들면, 새 멤버 C는 옛 키로 쓴 상태 이벤트를 열지 못한 채 받았다가 멤버에게 물어(`GetStateKeys`) 모든 채널이 든 상태를
/// 세웁니다. 받은 키는 ID가 키에서 나온 것만 저장해, 지금 키는 여전히 사슬 끝의 키입니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn joiner_fetches_older_state_keys_from_members() {
    common::init_tracing();
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let b = cl.node("b").await;
    let c = cl.node("c").await;
    let space = create_crew(&a, "오래된 크루").await;
    join(&a, &b, space).await;
    let mut names = Vec::new();
    for i in 0..3 {
        let before = testkit::crew_key_seq(&a.node, &space).expect("크루 순번");
        let before_key = testkit::current_state_key_id(&a.node, &space);
        let name = format!("회전{i}");
        public_channel(&a, space, &name).await;
        names.push(name);
        testkit::force_full_rekey(&a.node, space);
        wait_crew_kind(&a, space, before, "full_rekey").await;
        eventually("새 상태 키", || async {
            (testkit::current_state_key_id(&a.node, &space) != before_key).then_some(())
        })
        .await;
    }
    assert!(testkit::state_key_count(&a.node, &space) >= 4);
    testkit::set_joiner_state_keys(&a.node, 1);
    testkit::set_joiner_state_keys(&b.node, 1);

    join(&a, &c, space).await;
    eventually("C가 옛 채널까지 봄", || async {
        let st = testkit::space_state(&c.node, &space)?;
        names
            .iter()
            .all(|n| st.channels.values().any(|ch| ch.name == *n))
            .then_some(())
    })
    .await;
    eventually("C가 기다리는 이벤트 없음", || async {
        (testkit::waiting_count(&c.node, &space) == 0).then_some(())
    })
    .await;
    assert!(
        testkit::state_key_count(&c.node, &space) >= 4,
        "C가 옛 상태 키를 받지 못함"
    );
    assert_eq!(
        testkit::current_state_key_id(&c.node, &space),
        testkit::current_state_key_id(&a.node, &space),
        "물어 받은 옛 키가 지금 키를 바꿈"
    );
}

// ───────────── 11. 부모 상한을 넘는 동시 가입 ─────────────

/// 서로 모르는 가입 40개(공간 스트림 머리 40개, 부모 상한 16개)가 한꺼번에 오면 확정 기기는 갱신 머리에서 닿는 가입만
/// 넣고 나머지를 다음 바퀴에 넣어, 모두 크루 키 그룹에 듭니다(예전에는 갱신마다 "가입 기기를 넣은 이벤트가 갱신의
/// 조상이 아님"으로 거부되어 크루 키가 멈췄음). 실제 멤버 B는 같은 크루 키를 얻습니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_joins_beyond_parent_limit_all_get_the_crew_key() {
    const USERS: usize = 40;
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let b = cl.node("b").await;
    let space = create_crew(&a, "동시 가입").await;
    join(&a, &b, space).await;
    let seq0 = wait_same_crew_key(&[&a, &b], space).await;

    let url = invite_url(&a, space, None).await;
    let token = InviteLink::parse(&url).expect("초대 링크").token;
    // 하나씩 적용하지 않고 지어 한 번에 넣으므로 모두 같은 부모를 둔 형제 이벤트입니다.
    let joins = testkit::synthetic_members(&a.node, space, &token, USERS, 1)
        .await
        .expect("지어낸 가입");
    testkit::ingest_from(&b.node, space, joins, a.peer())
        .await
        .expect("B에 넣기");
    let tip = eventually("동시 가입이 모두 크루 그룹에", || async {
        let t = testkit::crew_tip(&a.node, &space)?;
        (t.members.len() == 2 + USERS && t.debt == 0).then_some(t)
    })
    .await;
    assert!(tip.seq > seq0 + 1, "갱신이 여러 바퀴로 나뉘어야 함");
    let seq = wait_same_crew_key(&[&a, &b], space).await;
    assert!(seq >= tip.seq);
}

// ───────────── 11. 볼 수 없는 가입자와 제한 채널 ─────────────

/// 보기 규칙이 있는 채널은 지금 모든 멤버가 보더라도 처음부터 자기 비공개 키 그룹을 씁니다(상태만 보는 구조 규칙,
/// `SpaceState::channel_key_group`). 그래서 그 채널을 볼 수 없는 D가 들어와도 채널이 크루를 떠나는 일이 없고, D가 받는
/// 크루 R로는 가입 전 모드방 뿌리를 파생할 수 없습니다. 예전(모두가 보는 동안 크루 키) 규칙에서는 확정 기기가 D의 가입
/// 전진보다 크루 R을 먼저 새로 해야 했습니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restricted_channel_viewed_by_all_keeps_its_private_group() {
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let d = cl.node("d").await;
    let space = create_crew(&a, "모드 크루").await;
    let role = create_role(&a, space, "모드", &[], 5).await;
    let mut p = channel_params(space, "모드방");
    p.private_roles = Some(vec![role]);
    p.history = Some(HistoryVisibility::Public);
    let modch = a.ok::<m::ChannelCreate>(p).await.channel;
    assert!(
        testkit::channel_key_group(&a.node, space, modch).is_some(),
        "보기 규칙이 있는 채널은 모두가 봐도 비공개 그룹"
    );
    let old = send_when_ready(&a, space, modch, "가입 전 모드방").await;
    let h = header(&a, &old);
    assert!(
        !testkit::uses_crew_root(&a.node, &h),
        "보기 규칙이 있는 채널은 크루 키를 쓰지 않음"
    );
    join(&a, &d, space).await;
    wait_same_crew_key(&[&a, &d], space).await;
    assert!(
        !testkit::can_derive_root(&d.node, &h),
        "볼 수 없는 가입자 D가 가입 전 모드방 뿌리를 파생함"
    );
    force_channel(&a, &d, space, modch).await;
    assert!(
        testkit::message_text(&d.node, &old).is_none(),
        "D가 가입 전 모드방을 읽음"
    );
}

/// 크루 채널의 기록 공개를 끄는 것과 가입이 한 바퀴에 오면, 확정 기기는 가입 전진보다 크루 R을 먼저 새로 합니다
/// (`pending_refresh`). 순서가 뒤바뀌면 같은 바퀴에 든 D가 공개 시절 크루 뿌리를 파생합니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crew_flip_with_a_join_in_the_same_round_refreshes_crew_r_first() {
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let d = cl.node("d").await;
    let space = create_crew(&a, "크루 뒤집기").await;
    let ch = public_channel(&a, space, "한때 공개").await;
    let old = send_when_ready(&a, space, ch, "공개 시절 크루").await;
    let h = header(&a, &old);
    let seq0 = wait_same_crew_key(&[&a], space).await;

    // 바퀴를 멈춘 채 기록 공개를 끄고 D를 들여, 둘이 한 바퀴에 오게 합니다.
    connect_both(&a, &d).await;
    let url = invite_url(&a, space, Some(1)).await;
    testkit::hold_commits(&a.node, true);
    let mut up = ChannelUpdateParams::new(space, ch);
    up.history = Some(HistoryVisibility::Private);
    a.ok::<m::ChannelUpdate>(up).await;
    // 바퀴를 멈춰 크루 키가 아직 없으니 가입은 기다리는 중일 수 있습니다.
    d.ok::<m::InviteJoin>(UrlParams { url }).await;
    wait_member(&a, space, d.user(), true).await;
    testkit::hold_commits(&a.node, false);

    eventually("D가 크루 키를 받음", || async {
        testkit::crew_key_seq(&d.node, &space)
    })
    .await;
    wait_same_crew_key(&[&a, &d], space).await;
    let kinds = testkit::key_update_kinds(&a.node, &space, None);
    let first = kinds.iter().find(|(s, _)| *s > seq0).map(|(_, k)| *k);
    assert_eq!(first, Some("r_refresh"), "{kinds:?}");
    assert!(
        !testkit::can_derive_root(&d.node, &h),
        "같은 바퀴에 든 D가 공개 시절 크루 뿌리를 파생함"
    );
}

// ───────────── 12. 기록 공개 끄기와 같은 바퀴의 가입 ─────────────

/// 기록 공개를 끄는 것과 그 그룹의 가입이 한 바퀴에 오면, 확정 기기는 먼저 그룹 R을 새로 하고 가입은 다음 바퀴에
/// 넣습니다. 가입 전진이 R 새로 하기를 삼키면(예전) C와 그 뒤 들어온 D가 공개 시절 뿌리를 파생했습니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn private_flip_with_a_join_in_the_same_round_still_refreshes_r() {
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let b = cl.node("b").await;
    let c = cl.node("c").await;
    let e = cl.node("e").await;
    let space = create_crew(&a, "뒤집는 크루").await;
    join(&a, &b, space).await;
    join(&a, &c, space).await;
    join(&a, &e, space).await;
    meet(&b, &c, space).await;
    let role = create_role(&a, space, "엑스", &[], 5).await;
    set_roles(&a, space, b.user(), vec![role]).await;
    let mut p = channel_params(space, "엑스방");
    p.private_roles = Some(vec![role]);
    p.history = Some(HistoryVisibility::Public);
    let ch = a.ok::<m::ChannelCreate>(p).await.channel;
    let c_dev = dev(&c);
    eventually("엑스방 그룹(A·B)", || async {
        let m = testkit::key_group_members(&a.node, &space, Some(ch))?;
        (m.contains(&dev(&b)) && !m.contains(&c_dev)).then_some(())
    })
    .await;
    let old = send_when_ready(&a, space, ch, "공개 시절 엑스").await;
    sees_text(&b, space, ch, "공개 시절 엑스").await;
    let h = header(&a, &old);

    // 바퀴를 멈춘 채 기록 공개를 끄고 C를 넣어, 둘이 한 바퀴에 오게 합니다.
    testkit::hold_commits(&a.node, true);
    let mut up = ChannelUpdateParams::new(space, ch);
    up.history = Some(HistoryVisibility::Private);
    a.ok::<m::ChannelUpdate>(up).await;
    set_roles(&a, space, c.user(), vec![role]).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    testkit::hold_commits(&a.node, false);

    eventually("C가 엑스방 그룹에 들어감", || async {
        let m = testkit::key_group_members(&a.node, &space, Some(ch))?;
        (m.contains(&c_dev) && testkit::channel_key_seq(&c.node, &space, ch).is_some())
            .then_some(())
    })
    .await;
    let kinds = testkit::key_update_kinds(&a.node, &space, Some(ch));
    assert!(kinds.iter().any(|(_, k)| *k == "r_refresh"), "{kinds:?}");
    let leak_c = testkit::can_derive_root(&c.node, &h);

    // 뒤에 든 D도 공개 시절 R을 받지 못합니다(그룹 R 새로 하기가 남아 있지 않음).
    let d = cl.node("d").await;
    join(&a, &d, space).await;
    set_roles(&a, space, d.user(), vec![role]).await;
    let d_dev = dev(&d);
    eventually("D가 엑스방 그룹에 들어감", || async {
        let m = testkit::key_group_members(&a.node, &space, Some(ch))?;
        (m.contains(&d_dev) && testkit::channel_key_seq(&d.node, &space, ch).is_some())
            .then_some(())
    })
    .await;
    let leak_d = testkit::can_derive_root(&d.node, &h);
    assert!(
        !leak_c && !leak_d,
        "기록 공개를 끈 뒤 든 사람이 공개 시절 뿌리를 파생함(C {leak_c}, D {leak_d})"
    );
}

// ───────────── 13. 자격을 잃은 기기의 표 넘기기 ─────────────

/// 관리자에서 내려와 표 보유 자격을 잃은 옛 확정 기기 B가 표를 넘겨도 새 확정 기기 A는 받지 않습니다(넘기는 기기가
/// 지금 소유자·관리자의 기기이자 보유 기기여야 하고, 크루 전체 재키가 요구된 동안은 누구의 표도 받지 않음). A는 표
/// 없이 전체 재키로 이어받습니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn handover_from_a_demoted_holder_is_refused() {
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let b = cl.node("b").await;
    let space = create_crew(&a, "강등 넘기기").await;
    join(&a, &b, space).await;
    let (da, db) = (dev(&a), dev(&b));
    let admin = create_role(&a, space, "관리", &["administrator"], 10).await;
    set_roles(&a, space, b.user(), vec![admin]).await;
    a.ok::<m::CrewSetCommitter>(CommitterSetParams { space, device: db })
        .await;
    // 바뀔 때마다 남겨 시간 초과 때 어느 쪽이 남았는지 보입니다(출력은 실패한 시험만 보임).
    let mut seen = None;
    eventually("B가 표를 받음", || {
        let now = (
            testkit::has_key_table(&b.node, &space),
            testkit::has_key_table(&a.node, &space),
        );
        if seen.replace(now) != Some(now) {
            eprintln!("B 표·A 표: {now:?}");
        }
        async move { (now.0 && !now.1).then_some(()) }
    })
    .await;
    let rekeys_before = testkit::key_update_kinds(&a.node, &space, None)
        .iter()
        .filter(|(_, k)| *k == "full_rekey")
        .count();
    let seq0 = testkit::crew_tip(&a.node, &space).expect("크루 사슬").seq;

    // A의 바퀴를 멈춘 채 B를 관리자에서 내립니다(소유자 A가 확정 기기가 됨). B는 아직 표를 가지고 넘기려 합니다.
    testkit::hold_commits(&a.node, true);
    set_roles(&a, space, b.user(), Vec::new()).await;
    eventually("A가 확정 기기", || async {
        (testkit::committer_of(&a.node, &space) == Some(da)
            && testkit::committer_of(&b.node, &space) == Some(da))
        .then_some(())
    })
    .await;
    let got = tokio::time::timeout(
        Duration::from_secs(10),
        eventually("A가 표를 받음", || async {
            testkit::has_key_table(&a.node, &space).then_some(())
        }),
    )
    .await;
    assert!(got.is_err(), "보유 자격을 잃은 B의 표를 받아들임");
    let tip = testkit::crew_tip(&a.node, &space).expect("크루 사슬");
    assert!(tip.rekey_required, "B가 내려와 전체 재키가 요구됨");

    // 바퀴를 다시 돌리면 A는 전체 재키로 이어받고 B는 보유 기기에서 빠집니다.
    testkit::hold_commits(&a.node, false);
    wait_crew_kind(&a, space, seq0, "full_rekey").await;
    eventually("A가 새 표를 가짐", || async {
        let t = testkit::crew_tip(&a.node, &space)?;
        (testkit::has_key_table(&a.node, &space) && !t.holders.contains(&db)).then_some(())
    })
    .await;
    let rekeys = testkit::key_update_kinds(&a.node, &space, None)
        .iter()
        .filter(|(_, k)| *k == "full_rekey")
        .count();
    assert!(rekeys > rekeys_before);
}

// ───────────── 14. 다시 띄운 확정 기기와 저장 칸 ─────────────

/// 표는 그룹마다 저장 칸 두 개에 둡니다(크루 잎은 leaf_master에서 다시 만들어 빼고 저장). 갱신은 내기 전에 다음 상태를
/// 다른 칸에 써 두므로, 내기 직전에 멈춘 확정 기기를 다시 띄우면 사슬에 있는 칸(앞 상태)을 골라 그대로 이어 갑니다(전체
/// 재키 없음). 예전에는 표 하나를 덮어써 사슬과 어긋난 표를 읽고 전체 재키했습니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restarted_committer_resumes_from_the_slot_on_the_chain() {
    let cl = Cluster::new();
    let mut a = cl.node("a").await;
    let b = cl.node("b").await;
    let c = cl.node("c").await;
    let space = create_crew(&a, "다시 띄우는 크루").await;
    join(&a, &b, space).await;
    let role = create_role(&a, space, "비밀", &[], 5).await;
    set_roles(&a, space, b.user(), vec![role]).await;
    let other = cl.node("x").await;
    join(&a, &other, space).await;
    let mut p = channel_params(space, "비밀방");
    p.private_roles = Some(vec![role]);
    let secret = a.ok::<m::ChannelCreate>(p).await.channel;
    let db = dev(&b);
    eventually("비밀방 그룹(A·B)", || async {
        let m = testkit::key_group_members(&a.node, &space, Some(secret))?;
        (m.contains(&db) && testkit::channel_key_seq(&b.node, &space, secret).is_some())
            .then_some(())
    })
    .await;
    wait_same_crew_key(&[&a, &b, &other], space).await;

    // 그룹마다 행이 따로 있습니다(크루와 비밀방 그룹).
    let (crew_g, _, _) = testkit::member_key_digest(&a.node, &space, None).expect("크루");
    let (secret_g, _, _) =
        testkit::member_key_digest(&a.node, &space, Some(secret)).expect("비밀방");
    let rows = testkit::stored_key_rows(&a.node, &space);
    assert!(rows.iter().any(|(g, _, _)| *g == crew_g), "{rows:?}");
    assert!(rows.iter().any(|(g, _, _)| *g == secret_g), "{rows:?}");
    let rekeys_before = testkit::key_update_kinds(&a.node, &space, None)
        .iter()
        .filter(|(_, k)| *k == "full_rekey")
        .count();

    // 바퀴를 멈추고, 크루의 다음 상태를 다른 칸에 써 둔 채(내지 않음) 다시 띄웁니다.
    testkit::hold_commits(&a.node, true);
    let tip = testkit::crew_tip(&a.node, &space).expect("크루 사슬");
    let staged = testkit::stage_unpublished_crew(&a.node, &space).expect("써 둠");
    assert_eq!(staged, tip.seq + 1);
    let crew_rows = testkit::stored_key_rows(&a.node, &space)
        .iter()
        .filter(|(g, _, _)| *g == crew_g)
        .count();
    assert_eq!(crew_rows, 2, "지금 칸과 써 둔 칸");
    a.restart().await;

    assert!(testkit::has_key_table(&a.node, &space), "다시 띄운 뒤 표");
    let snap = testkit::key_table_snapshot(&a.node, &space).expect("표");
    let digests = testkit::key_table_digests(&a.node, &space, &snap).expect("표 지문");
    assert_eq!(digests[&crew_g].0, tip.seq, "사슬에 없는 써 둔 칸을 고름");
    assert!(digests.contains_key(&secret_g), "비밀방 그룹도 이어 감");

    // 다시 띄운 확정 기기가 C를 가입 전진으로 넣고(전체 재키 없음), 비밀방도 그대로 씁니다.
    join(&a, &c, space).await;
    meet(&a, &b, space).await;
    wait_same_crew_key(&[&a, &b, &c], space).await;
    let kinds = testkit::key_update_kinds(&a.node, &space, None);
    assert!(
        kinds.iter().any(|(s, k)| *s > tip.seq && *k == "advance"),
        "{kinds:?}"
    );
    assert_eq!(
        kinds.iter().filter(|(_, k)| *k == "full_rekey").count(),
        rekeys_before,
        "다시 띄운 뒤 전체 재키함: {kinds:?}"
    );
    send_when_ready(&a, space, secret, "다시 띄운 뒤 비밀").await;
    sees_text(&b, space, secret, "다시 띄운 뒤 비밀").await;
}

/// 확정 기기가 낸 크루 갱신이 사슬에 들지 못하면(체크포인트 경계 아래로 늦음, 경쟁에서 짐) 메모리 표가 사슬 끝보다
/// 앞섭니다. 다음 바퀴는 저장 칸에서 사슬 끝과 맞는 앞 상태를 다시 읽어 그 위에서 이어 내고(전체 재키 없음), 표의 크루
/// 순번은 사슬 끝과 같아집니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn committer_ahead_of_the_crew_chain_resumes_from_the_stored_slot() {
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let b = cl.node("b").await;
    let c = cl.node("c").await;
    let space = create_crew(&a, "앞선 표").await;
    join(&a, &b, space).await;
    wait_same_crew_key(&[&a, &b], space).await;
    let rekeys_before = testkit::key_update_kinds(&a.node, &space, None)
        .iter()
        .filter(|(_, k)| *k == "full_rekey")
        .count();

    testkit::hold_commits(&a.node, true);
    let tip = testkit::crew_tip(&a.node, &space).expect("크루 사슬");
    let ahead = testkit::stage_crew_ahead(&a.node, &space).expect("앞선 표");
    assert_eq!(ahead, tip.seq + 1);
    let snap = testkit::key_table_snapshot(&a.node, &space).expect("표");
    let (crew_g, _, _) = testkit::member_key_digest(&a.node, &space, None).expect("크루");
    let digests = testkit::key_table_digests(&a.node, &space, &snap).expect("표 지문");
    assert_eq!(digests[&crew_g].0, ahead, "표가 사슬 끝보다 앞섬");
    testkit::hold_commits(&a.node, false);

    // 다음 바퀴(C의 가입)는 사슬 끝의 저장 칸에서 이어 가입 전진으로 냅니다.
    join(&a, &c, space).await;
    let seq = wait_same_crew_key(&[&a, &b, &c], space).await;
    let kinds = testkit::key_update_kinds(&a.node, &space, None);
    assert!(
        kinds.iter().any(|(s, k)| *s > tip.seq && *k == "advance"),
        "{kinds:?}"
    );
    assert_eq!(
        kinds.iter().filter(|(_, k)| *k == "full_rekey").count(),
        rekeys_before,
        "앞선 표를 버리고 전체 재키함: {kinds:?}"
    );
    let snap = testkit::key_table_snapshot(&a.node, &space).expect("표");
    let digests = testkit::key_table_digests(&a.node, &space, &snap).expect("표 지문");
    assert_eq!(digests[&crew_g].0, seq, "표의 크루 순번이 사슬 끝");
}

// ───────────── 15. 전체 재키 몫은 기기마다 묻기 ─────────────

/// 크루 트리 전체 재키의 기기 몫은 사용자별 조각 블롭에 들고, 멤버는 `GetKeyShard`로 자기 기기 몫만 받습니다(64KiB 블롭
/// 조각이 아니라 약 2KB). 크루 밖 기기에게는 내주지 않고, 한 기기가 잇달아 물으면 내주기 예산을 넘어 `Busy`입니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn full_rekey_share_is_fetched_per_device_within_a_budget() {
    // 사용자마다 2대라 한 사용자 몫이 조각 하나에 들고(조각 블롭으로 냄), 모두 282대라 트리입니다.
    const USERS: usize = 140;
    const DEVICES_EACH: usize = 2;
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let b = cl.node("b").await;
    let space = create_crew(&a, "몫 크루").await;
    join(&a, &b, space).await;
    let url = invite_url(&a, space, None).await;
    let token = InviteLink::parse(&url).expect("초대 링크").token;
    let joins = testkit::synthetic_members(&a.node, space, &token, USERS, DEVICES_EACH)
        .await
        .expect("지어낸 가입");
    testkit::ingest_from(&b.node, space, joins, a.peer())
        .await
        .expect("B에 넣기");
    eventually("크루가 트리로 바뀜", || async {
        let t = testkit::crew_tip(&a.node, &space)?;
        (t.depth > 0 && t.members.len() == 2 + USERS * DEVICES_EACH).then_some(())
    })
    .await;
    wait_same_crew_key(&[&a, &b], space).await;
    let before = testkit::crew_tip(&a.node, &space).expect("사슬").seq;
    testkit::force_full_rekey(&a.node, space);
    wait_crew_kind(&a, space, before, "full_rekey").await;
    // B는 자기 몫을 조각 블롭에서 받아 새 크루 키를 얻습니다.
    wait_same_crew_key(&[&a, &b], space).await;
    let blob = testkit::last_shard_blob(&a.node, &space).expect("조각 블롭");
    let ask = || testkit::net_request(&b.node, a.peer(), YpRequest::GetKeyShard { space, blob });
    let data = loop {
        match ask().await.expect("묻기") {
            YpResponse::Chunk { data } => break data,
            YpResponse::Busy { .. } => tokio::time::sleep(Duration::from_secs(1)).await,
            r => panic!("조각 답이 아님: {r:?}"),
        }
    };
    assert!(data.len() < 8 * 1024, "답 {}B", data.len());
    let devs = testkit::shard_answer_devices(&data).expect("묶음");
    assert!(
        !devs.is_empty() && devs.iter().all(|d| *d == dev(&b)),
        "{devs:?}"
    );
    let x = cl.node("x").await;
    connect_both(&a, &x).await;
    let _ = testkit::authenticate(&x.node, a.peer()).await;
    let r = testkit::net_request(&x.node, a.peer(), YpRequest::GetKeyShard { space, blob })
        .await
        .expect("묻기");
    assert!(
        matches!(
            r,
            YpResponse::Error {
                code: WireErrorCode::NotMember | WireErrorCode::Unauthenticated,
                ..
            }
        ),
        "{r:?}"
    );
    // 예산은 한 번에 512KiB, 묻기 하나를 32KiB로 셉니다.
    let mut busy = false;
    for _ in 0..24 {
        if matches!(ask().await.expect("묻기"), YpResponse::Busy { .. }) {
            busy = true;
            break;
        }
    }
    assert!(busy, "조각 내주기 예산이 없음");
}

// ───────────── 16. 역할을 잃은 멤버의 그룹 키 묻기 ─────────────

/// 역할을 잃어 비공개 채널을 볼 수 없게 된 B는, 확정 기기의 Rotate가 아직 오지 않아 그룹 사슬 끝에 남아 있어도 다른
/// 멤버에게 그 그룹의 지금 T·R을 받지 못합니다(`GetGroupKeys`는 지금 볼 자격을 봄).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn group_keys_are_not_relayed_after_losing_the_role() {
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let b = cl.node("b").await;
    let c = cl.node("c").await;
    // 역할 없는 E가 있어야 채널이 크루가 아닌 자기 그룹을 씁니다(모두 보는 채널은 크루 그룹).
    let e = cl.node("e").await;
    let space = create_crew(&a, "역할 잃는 크루").await;
    join(&a, &b, space).await;
    join(&a, &c, space).await;
    join(&a, &e, space).await;
    meet(&b, &c, space).await;
    let role = create_role(&a, space, "와이", &[], 5).await;
    set_roles(&a, space, b.user(), vec![role]).await;
    set_roles(&a, space, c.user(), vec![role]).await;
    let mut p = channel_params(space, "와이방");
    p.private_roles = Some(vec![role]);
    let ch = a.ok::<m::ChannelCreate>(p).await.channel;
    let (b_dev, c_dev) = (dev(&b), dev(&c));
    let e_dev = dev(&e);
    eventually("와이방 그룹(A·B·C)", || async {
        let m = testkit::key_group_members(&c.node, &space, Some(ch))?;
        (m.contains(&b_dev)
            && m.contains(&c_dev)
            && !m.contains(&e_dev)
            && testkit::channel_key_seq(&b.node, &space, ch).is_some())
        .then_some(())
    })
    .await;
    let group = testkit::space_state(&c.node, &space)
        .and_then(|st| st.channel_key_group(&space, st.channels.get(&ch)?))
        .expect("와이방 키 그룹");
    let ask = || testkit::net_request(&b.node, c.peer(), YpRequest::GetGroupKeys { space, group });
    // 대조: 역할이 있는 동안은 받습니다.
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let got = ask().await;
        if matches!(got, Ok(YpResponse::SealedKeys(_))) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "역할이 있는 B가 그룹 키를 받지 못함: {got:?}"
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
    }

    testkit::hold_commits(&a.node, true);
    set_roles(&a, space, b.user(), vec![]).await;
    let b_user = b.user();
    eventually("C가 B의 역할 잃음을 적용", || async {
        let st = testkit::space_state(&c.node, &space)?;
        (!st.can_view_key_group(&space, &group, &b_user)).then_some(())
    })
    .await;
    let tip = testkit::key_group_members(&c.node, &space, Some(ch)).expect("C의 그룹 키");
    assert!(tip.contains(&b_dev), "Rotate 전인데 B가 사슬 끝에서 빠짐");
    let got = ask().await.expect("묻기");
    assert!(
        matches!(
            got,
            YpResponse::Error {
                code: WireErrorCode::Forbidden,
                ..
            }
        ),
        "볼 수 없는 B에게 그룹 키를 중계함(봉인한 답: {})",
        matches!(got, YpResponse::SealedKeys(_))
    );

    testkit::hold_commits(&a.node, false);
    eventually("B가 와이방 그룹에서 빠짐", || async {
        let m = testkit::key_group_members(&a.node, &space, Some(ch))?;
        (!m.contains(&b_dev)).then_some(())
    })
    .await;
}
