//! 프로토콜 견고성 시험(W3): 여러 노드에 공격자를 섞어 기기 목록·기기 연결·커밋 확정·가입 묶기·휘발성 키·메모의 빈칸을
//! 확인합니다.
//!
//! 1. 폐기된 기기가 신원 키로 새 기기를 더한 목록은 어느 노드도 받지 않고(내 살아 있는 기기는 그 기기를 폐기한 판으로
//!    바로잡음), 폐기된 기기는 다음에 연결할 때 폐기를 알고 로컬 데이터를 지움.
//! 2. 시드만 가진 새 기기는 친구·멤버·내 기기 누구도 받지 않고, 연결한 기기를 살아 있는 기기가 폐기하면 친구도 폐기를
//!    받고 그 기기는 지워짐.
//! 3. 내 기기가 들인 새 기기가 1:1 공간에서 읽고 씀.
//! 4. 커밋 확정 기기(소유자의 유일한 기기)를 소유자가 연결한 새 기기로 폐기하면, 새 기기가 확정 기기가 되고 남은 멤버가
//!    키를 넘겨 그룹(공간·비공개 채널)을 다시 세움. 새 멤버가 들어오고 비공개 채널도 계속 씀.
//! 5. 동시에 들어온 가입 여럿은 키 갱신(`KeyUpdate`) 하나(가입 목록 하나)로 들어감.
//! 6. 휘발성 세션 키 갱신을 보내지 못하면 다시 보내 모든 참가자가 같은 세대로 모임.
//! 7. 묶음 상한(4MiB)을 넘는 개인 메모도 잘리지 않고 내 새 기기로 모두 감.
//! 8. 연결한 새 기기는 폐기된 기기가 주는, 이 기기를 폐기한 목록을 받지 않음.
//! 9. 꺼져 있던 동안 더해진 기기가 이 기기를 폐기하면, 다른 사용자가 알려 준 폐기 목록으로 지움.
//! 10. 소유자가 비공개 채널 그룹에 없는 관리자의 기기를 확정 기기로 정해도 그 채널을 계속 씀(`crew.set_committer`).
//! 11. 공동 서명 기기를 더한 사이 판보다 먼저 온 판은 미뤄 두었다가 사이 판을 받은 뒤 받음.
//! 12. 예전 빌드가 남긴 OTP 비밀은 시작할 때 지움.
//! 13. 한 기기에서 생긴 연락처·친구가 내 다른 기기에 같은 종류로 감(켜져 있으면 바로, 꺼져 있었으면 다시 연결될 때).
//! 14. 폐기된 기기가 쓴 글은 그 사람의 살아 있는 기기가 내줄 때 받아, 나중에 연결한 기기에도 빠지지 않음.
//! 15. 친구 추가 요청은 내 기기 하나에만 닿아도 모두에 보이고, 수락·거절도 따라감.
//! 16. 친구를 끊으면 양쪽과 내 다른 기기에서 연락처·대화가 사라지고, 꺼져 있던 기기가 되살리지 못함.
//! 17. 기기 이름 바꾸기·휴대폰 여부가 내 기기에 보이고, 폐기한 기기는 친구에게 보이지 않음.
//!
//! 노드는 한 프로세스 안에서 127.0.0.1로 직접 연결합니다(`common` 참고). 고정 sleep 없이 조건이 될 때까지 기다립니다.

use crate::common;

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use common::{
    Cluster, TestNode, befriend, connect, connect_both, eventually, wait_message, wait_own_device,
};
use yangpa_core::body::EventBody;
use yangpa_core::device::{BoxPublicKey, DeviceEntry};
use yangpa_core::ids::{ChannelId, DeviceId, EventId, SpaceId, UserId};
use yangpa_core::model::{
    ChannelKind, ChannelSettings, HistoryVisibility, JoinMode, Permissions, SecurityLevel,
};
use yangpa_core::time::Timestamp;
use yangpa_core::wire::{SocialRequest, YpRequest, YpResponse};
use yangpa_daemon::testkit::volatile as vt;
use yangpa_daemon::{Node, NodeConfig, testkit};
use yangpa_rpc::api::{
    ChannelCreateParams, CommitterSetParams, ContactKind, CrewCreateParams, DeviceRenameParams,
    DeviceRevokeParams, Empty, FriendLinkCreateParams, InviteCreateParams, InviteJoinStatus,
    MemberSetRolesParams, MessageListParams, MessageSendParams, NoteSetParams, RequestIdParams,
    RoleCreateParams, SpaceChannelParams, SpaceParams, TokenIdParams, UrlParams, UserDeviceParams,
    UserParams, m,
};

// ───────────── 도우미 ─────────────

fn dev(n: &TestNode) -> DeviceId {
    testkit::device_id(&n.node).expect("기기 ID")
}

/// 주기 작업(새 기기의 재인증·따라잡기)을 자주 돌립니다.
fn fast_sync(c: &mut NodeConfig) {
    c.sync_interval = Duration::from_millis(200);
}

/// 이 노드가 아는 그 사용자의 기기 목록에서 (있음, 활성, 폐기).
fn device_seen(n: &TestNode, user: UserId, device: DeviceId) -> (bool, bool, bool) {
    match testkit::device_list(&n.node, &user) {
        Some(l) => (
            l.devices.iter().any(|d| d.device == device),
            l.contains_active(&device),
            l.is_revoked(&device),
        ),
        None => (false, false, false),
    }
}

async fn wait_active(n: &TestNode, user: UserId, device: DeviceId, what: &str) {
    eventually(what, || async {
        device_seen(n, user, device).1.then_some(())
    })
    .await;
}

async fn wait_revoked(n: &TestNode, user: UserId, device: DeviceId, what: &str) {
    eventually(what, || async {
        device_seen(n, user, device).2.then_some(())
    })
    .await;
}

/// 지워질 때까지 기다리며, 아직이면 그 피어에 다시 자신을 알리게 합니다(연결 때 저절로 도는 것과 같은 경로).
async fn wait_wiped(n: &TestNode, peer: yangpa_net::PeerId) {
    eventually(&format!("{}가 로컬 데이터를 지움", n.name), || async {
        if testkit::is_wiped(&n.node) {
            return Some(());
        }
        let _ = testkit::authenticate(&n.node, peer).await;
        None
    })
    .await;
}

async fn create_crew(a: &TestNode, name: &str) -> SpaceId {
    a.ok::<m::CrewCreate>(CrewCreateParams {
        name: name.into(),
        description: None,
    })
    .await
    .space
}

async fn invite_url(gateway: &TestNode, space: SpaceId, uses: u32) -> String {
    gateway
        .ok::<m::InviteCreate>(InviteCreateParams {
            space,
            expires_in_secs: None,
            max_uses: Some(uses),
            roles: Vec::new(),
        })
        .await
        .url
}

/// `joiner`가 `gateway`의 초대 링크로 들어옵니다(둘만 연결).
async fn join_by_invite(gateway: &TestNode, joiner: &TestNode, space: SpaceId) {
    connect_both(gateway, joiner).await;
    let url = invite_url(gateway, space, 1).await;
    let r = joiner.ok::<m::InviteJoin>(UrlParams { url }).await;
    assert_eq!(r.space, space);
    assert_eq!(r.status, InviteJoinStatus::Joined, "{} 가입", joiner.name);
}

async fn wait_member(n: &TestNode, space: SpaceId, user: UserId) {
    eventually(&format!("{}가 멤버를 봄", n.name), || async {
        testkit::space_state(&n.node, &space)?
            .is_member(&user)
            .then_some(())
    })
    .await;
}

/// 모든 노드가 같은 크루 키 순번에 있고, 첫 노드의 그룹에 주어진 기기가 모두 있을 때까지.
async fn wait_space_group(nodes: &[&TestNode], space: SpaceId, devices: &[DeviceId], what: &str) {
    eventually(what, || async {
        let members = testkit::crew_key_members(&nodes[0].node, &space)?;
        let first = testkit::crew_key_seq(&nodes[0].node, &space)?;
        (devices.iter().all(|d| members.contains(d))
            && nodes
                .iter()
                .all(|n| testkit::crew_key_seq(&n.node, &space) == Some(first)))
        .then_some(())
    })
    .await;
}

/// 비공개 채널 그룹에서 같은 일.
async fn wait_channel_group(
    nodes: &[&TestNode],
    space: SpaceId,
    channel: ChannelId,
    devices: &[DeviceId],
    what: &str,
) {
    eventually(what, || async {
        let members = testkit::key_group_members(&nodes[0].node, &space, Some(channel))?;
        let first = testkit::channel_key_seq(&nodes[0].node, &space, channel)?;
        (devices.iter().all(|d| members.contains(d))
            && nodes
                .iter()
                .all(|n| testkit::channel_key_seq(&n.node, &space, channel) == Some(first)))
        .then_some(())
    })
    .await;
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

async fn sees_text(n: &TestNode, space: SpaceId, channel: ChannelId, text: &str) -> EventId {
    eventually(&format!("{}가 '{text}'를 받음", n.name), || async {
        n.call::<m::MessageList>(list_params(space, channel))
            .await
            .ok()?
            .messages
            .into_iter()
            .find(|v| v.text == text)
            .map(|v| v.id)
    })
    .await
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

// ───────────── 1. 폐기된 기기의 위조 목록 ─────────────

/// 폐기된 기기는 신원 키를 갖고 있어도 새 기기를 더하지 못합니다. 그 기기의 공동 서명으로 새 기기를 더한 판은 친구가
/// 받지 않고, 친구가 1:1 공간에 대신 전해도 상태가 받지 않으며, 내 살아 있는 기기는 그 판이 더한 기기를 폐기한 판을
/// 내어 친구까지 바로잡습니다. 같은 기기 ID에 다른 상자 키를 실은 판은 기기 키 표에 들어가도 봉인 키를 바꾸지
/// 못합니다. 폐기된 기기는 다시 연결하면 폐기를 확인하고 로컬 데이터를 모두 지웁니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn revoked_device_cannot_add_devices_and_wipes_itself() {
    common::init_tracing();
    let cl = Cluster::new();
    let a1 = cl.node("a1").await;
    let b = cl.node("b").await;
    let space = befriend(&a1, &b).await;
    let a_user = a1.user();
    let d1 = dev(&a1);
    let mut a2 = common::linked(&cl, &a1, "a2", |_| {}).await;
    let d2 = dev(&a2);
    wait_own_device(&a1, d2).await;
    wait_active(&b, a_user, d2, "친구가 A2를 앎").await;

    // 폐기를 알면 곧 지우므로 그 전에 A2의 키를 붙잡아 둡니다(훔친 기기·복사한 디스크를 흉내).
    let stolen = testkit::capture_keys(&a2.node).expect("키");
    assert_eq!(stolen.device_id(), d2);
    a1.ok::<m::DeviceRevoke>(DeviceRevokeParams { device: d2 })
        .await;
    wait_revoked(&b, a_user, d2, "친구가 폐기를 앎").await;

    // 폐기를 아는 최신 목록을 바탕으로 폐기된 기기가 공동 서명해 새 기기를 더합니다.
    let base = testkit::device_list(&b.node, &a_user).expect("목록");
    let (forged, extra) = testkit::forge_device_list_with(&stolen, &base).expect("위조");
    assert!(!testkit::accept_device_list(&b.node, &forged).expect("서명은 맞음"));
    assert_eq!(device_seen(&b, a_user, extra), (false, false, false));
    // 아는 판보다 높지 않은 판은 서명을 확인하기 전에 거릅니다(시작 때 다시 세우며 지난 판마다 확인하지 않게): 판 번호를
    // 내려 서명이 깨진 판도 오류 없이 받지 않음.
    let mut stale = forged.clone();
    stale.list.version = base.version;
    assert!(matches!(
        testkit::accept_device_list(&b.node, &stale),
        Ok(false)
    ));
    let relayed = testkit::publish_state_unpropagated(
        &b.node,
        space,
        None,
        EventBody::DeviceListUpdate {
            devices: forged.clone(),
        },
    )
    .await;
    assert!(relayed.is_err(), "상태가 위조 목록을 받음: {relayed:?}");

    // 내 살아 있는 기기는 받지 않고, 위조 판을 받았을지 모를 피어를 위해 그 기기를 폐기한 판을 냅니다.
    assert!(testkit::accept_device_list(&a1.node, &forged).expect("검사"));
    let (known, active, revoked) = device_seen(&a1, a_user, extra);
    assert!(known && !active && revoked);
    assert!(device_seen(&a1, a_user, d1).1, "A1은 그대로 활성");
    wait_revoked(&b, a_user, extra, "친구도 바로잡은 판을 받음").await;
    assert!(!device_seen(&b, a_user, extra).1);

    // 폐기된 기기가 내 살아 있는 기기까지 폐기하고 그 상자 키를 바꾸며 기기를 더한 판(판 번호도 크게): 바로잡는 판은 내
    // 목록 그대로에 더한 기기만 폐기로 더하고, 위조 판의 폐기·상자 키는 가져오지 않습니다.
    let base = testkit::device_list(&b.node, &a_user).expect("목록");
    let a1_box = base
        .devices
        .iter()
        .find(|d| d.device == d1)
        .expect("A1 항목")
        .box_key
        .clone();
    let mut hostile = base.clone();
    for d in hostile.devices.iter_mut().filter(|d| d.device == d1) {
        d.revoked_at = Some(Timestamp::now());
        d.box_key = BoxPublicKey::filled(0xEE);
    }
    hostile.version += 1_000;
    let (hostile, extra2) = testkit::forge_device_list_with(&stolen, &hostile).expect("위조");
    assert!(!testkit::accept_device_list(&b.node, &hostile).expect("서명은 맞음"));
    // 같은 기기 ID에 다른 상자 키를 실은 판을 기기 키 표가 배워도(서명은 맞음) 키 약속이 달라 다른 행에 들어가고,
    // 봉인은 상태 기록이 가리키는 진짜 키로 갑니다.
    testkit::learn_device_list(&b.node, space, &hostile);
    let forged_a1 = hostile
        .list
        .devices
        .iter()
        .find(|d| d.device == d1)
        .expect("위조 판의 A1 항목");
    assert_eq!(
        testkit::stored_box_key(&b.node, forged_a1.key_commit()),
        Some(BoxPublicKey::filled(0xEE)),
        "위조 키는 자기 약속의 행에"
    );
    assert_eq!(
        testkit::sealing_box_key(&b.node, space, a_user, d1),
        Some(a1_box.clone()),
        "봉인은 진짜 상자 키로"
    );
    // 진짜 행도 그대로(기기 ID로 찾는 표였다면 위조 목록이 덮었을 것). 봉인은 저장된 목록으로 돌아가는 길이 있으므로
    // 표 자체를 따로 봅니다.
    let genuine = testkit::state_key_commit(&b.node, space, a_user, d1).expect("상태 기록");
    assert_ne!(genuine, forged_a1.key_commit());
    assert_eq!(
        testkit::stored_box_key(&b.node, genuine),
        Some(a1_box.clone()),
        "진짜 키 행이 남음"
    );
    assert!(testkit::accept_device_list(&a1.node, &hostile).expect("검사"));
    let healed = testkit::device_list(&a1.node, &a_user).expect("목록");
    assert!(healed.version > hostile.list.version, "위조 판보다 높은 판");
    assert!(healed.contains_active(&d1), "A1은 그대로 활성");
    assert_eq!(
        healed
            .devices
            .iter()
            .find(|d| d.device == d1)
            .map(|d| d.box_key.clone()),
        Some(a1_box),
        "상자 키는 내 것 그대로"
    );
    assert!(healed.is_revoked(&extra2) && healed.is_revoked(&d2));
    eventually("친구도 두 번째 바로잡은 판을 받음", || async {
        (testkit::device_list(&b.node, &a_user).as_ref() == Some(&healed)).then_some(())
    })
    .await;
    assert!(!testkit::is_wiped(&a1.node));

    // 폐기된 기기는 다시 연결하면 폐기를 알고 지웁니다(이미 지웠으면 그대로).
    a2.restart().await;
    connect(&a2, &b).await;
    wait_wiped(&a2, b.peer()).await;
    assert!(testkit::known_spaces(&a2.node).is_empty());
}

// ───────────── 2. 시드만으로는 들어오지 못함 ─────────────

/// 신원 시드만 가진 사람(복사한 디스크, 예전 복구 문구)이 자기를 더해 서명한 기기는 친구·크루 멤버·살아 있는 내 기기가 모두
/// 받지 않습니다. 살아 있는 기기의 공동 서명이 없기 때문입니다. 인증도, 목록을 직접 건네는 것도, 들어가지 않습니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn seed_alone_is_refused_by_friends_members_and_own_devices() {
    common::init_tracing();
    let cl = Cluster::new();
    let a1 = cl.node("a1").await;
    let b = cl.node("b").await;
    let c = cl.node("c").await;
    befriend(&a1, &b).await;
    testkit::seed_crew(&a1.node, std::slice::from_ref(&c.node), "크루")
        .await
        .expect("크루");
    connect_both(&a1, &c).await;
    testkit::authenticate(&a1.node, c.peer())
        .await
        .expect("크루 멤버끼리 인증");
    let a_user = a1.user();

    let thief = cl.bare_with("thief", fast_sync).await;
    let dt = testkit::install_stolen_seed(&a1.node, &thief.node).expect("훔친 시드");
    let forged = testkit::signed_device_list(&thief.node, &a_user).expect("지어낸 목록");
    assert!(forged.list.contains_active(&dt));
    for n in [&b, &c, &a1] {
        connect(&thief, n).await;
        assert!(
            testkit::authenticate(&thief.node, n.peer()).await.is_err(),
            "{}가 시드만 가진 기기를 믿음",
            n.name
        );
        assert!(
            !testkit::accept_device_list(&n.node, &forged).expect("서명은 맞음"),
            "{}가 지어낸 목록을 받음",
            n.name
        );
        assert_eq!(device_seen(n, a_user, dt), (false, false, false));
    }
}

/// 내 기기가 받아 들인 새 기기는 친구 공간을 받아 주고받고, 살아 있는 기기가 폐기하면 친구도 폐기를 받고 그 기기는 다음
/// 연결 때 로컬 데이터를 지웁니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn revoked_linked_device_wipes_itself() {
    common::init_tracing();
    let cl = Cluster::new();
    let a1 = cl.node("a1").await;
    let b = cl.node("b").await;
    befriend(&a1, &b).await;
    let a_user = a1.user();

    let lost = common::linked(&cl, &a1, "lost", fast_sync).await;
    let dt = dev(&lost);
    // 지운 뒤 같은 프로세스에서 새 신원을 만들어도 옛 차단이 이어지지 않아야 합니다(RAM 사본까지 비움).
    lost.ok::<m::UserBlock>(UserParams {
        user: UserId::random(),
    })
    .await;
    assert_eq!(testkit::blocked_in_memory(&lost.node), 1);
    connect(&lost, &b).await;
    wait_active(&b, a_user, dt, "친구가 새 기기를 앎").await;

    a1.ok::<m::DeviceRevoke>(DeviceRevokeParams { device: dt })
        .await;
    wait_revoked(&b, a_user, dt, "친구가 폐기를 받음").await;
    assert!(device_seen(&a1, a_user, dt).2);

    // 폐기된 기기는 다시 알리면 폐기를 듣고 지웁니다.
    wait_wiped(&lost, b.peer()).await;
    assert!(testkit::known_spaces(&lost.node).is_empty());
    assert_eq!(
        testkit::blocked_in_memory(&lost.node),
        0,
        "차단 목록 RAM 사본"
    );
}

// ───────────── 3. 새 기기와 친구 ─────────────

/// 새 기기는 내 다른 기기에게서 1:1 공간을 받으면 친구 요청 없이 친구 기록을 되살리고(`identity::restore_friend`), 친구와
/// 그대로 주고받습니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn own_device_restores_friend_on_new_device() {
    common::init_tracing();
    let cl = Cluster::new();
    let a1 = cl.node("a1").await;
    let b = cl.node("b").await;
    let space = befriend(&a1, &b).await;
    let channel = a1.first_channel(space).await;
    let (a_user, b_user) = (a1.user(), b.user());
    let a2 = common::linked(&cl, &a1, "a2", fast_sync).await;
    let d2 = dev(&a2);
    eventually("내 기기가 알려 준 1:1 공간을 받음", || async {
        testkit::known_spaces(&a2.node)
            .contains(&space)
            .then_some(())
    })
    .await;
    connect(&a2, &b).await;
    wait_active(&b, a_user, d2, "친구가 새 기기를 앎").await;
    eventually("친구 기록을 되살림", || async {
        a2.ok::<m::FriendList>(Empty)
            .await
            .iter()
            .any(|f| {
                f.user == b_user
                    && f.space == space
                    && f.kind == yangpa_rpc::api::ContactKind::Friend
            })
            .then_some(())
    })
    .await;
    assert!(
        a2.ok::<m::FriendRequests>(Empty)
            .await
            .iter()
            .all(|r| r.user != b_user),
        "친구 요청이 남지 않음"
    );

    wait_space_group(
        &[&a1, &a2, &b],
        space,
        &[d2],
        "새 기기가 1:1 공간 그룹에 들어감",
    )
    .await;
    b.send(space, channel, "되살린 뒤 인사").await;
    sees_text(&a2, space, channel, "되살린 뒤 인사").await;
    send_when_ready(&a2, space, channel, "새 기기에서 답함").await;
    sees_text(&b, space, channel, "새 기기에서 답함").await;
}

/// 이 노드의 연락처 목록에 그 사람이 그 종류로 들 때까지 기다립니다.
async fn wait_contact(n: &TestNode, user: UserId, kind: ContactKind, what: &str) {
    eventually(what, || async {
        n.ok::<m::FriendList>(Empty)
            .await
            .iter()
            .any(|f| f.user == user && f.kind == kind)
            .then_some(())
    })
    .await;
}

/// 한 기기에서 생긴 연락처가 내 다른 기기에 같은 종류·조건으로 갑니다. 켜져 있으면 다시 연결하지 않아도 바로 받고, 그
/// 기기가 그 사람과 연결된 적이 없어도 이름이 보이며 대화를 읽습니다. 꺼져 있었으면 다시 연결될 때 받습니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn contacts_reach_my_other_device() {
    common::init_tracing();
    let cl = Cluster::new();
    let a1 = cl.node("a1").await;
    let mut a2 = common::linked(&cl, &a1, "a2", fast_sync).await;
    let c = cl.node("c").await;
    let d = cl.node("d").await;
    let e = cl.node("e").await;
    let f = cl.node("f").await;
    let (c_user, d_user, e_user, f_user) = (c.user(), d.user(), e.user(), f.user());

    // 임시 연락처는 임시 연락처로(만료 시각까지 같게).
    let space = common::meet_by_link(&a1, &c, Some(3600)).await;
    wait_contact(&a2, c_user, ContactKind::Temporary, "임시 연락처가 감").await;
    let mine = a1.ok::<m::FriendList>(Empty).await;
    let other = a2.ok::<m::FriendList>(Empty).await;
    let (mine, other) = (&mine[0], &other[0]);
    assert_eq!(other.space, space, "같은 1:1 공간");
    assert_eq!(other.expires_at, mine.expires_at, "같은 만료 시각");
    assert_eq!(other.profile.name, mine.profile.name, "이름도 함께 감");

    // 친구가 되면 내 다른 기기도 친구로, 대화도 읽음.
    common::make_friends(&a1, &c).await;
    wait_contact(&a2, c_user, ContactKind::Friend, "친구가 된 것이 감").await;
    let channel = a1.first_channel(space).await;
    c.send(space, channel, "내 다른 기기에도").await;
    sees_text(&a2, space, channel, "내 다른 기기에도").await;

    // 내가 만든 크루도 다시 연결하지 않아도 바로 받습니다.
    let crew = create_crew(&a1, "내 크루").await;
    eventually("새 크루가 감", || async {
        testkit::known_spaces(&a2.node)
            .contains(&crew)
            .then_some(())
    })
    .await;

    // 링크를 폐기해 만료되면 내 다른 기기도 만료.
    connect_both(&a1, &d).await;
    let link = a1
        .ok::<m::FriendLinkCreate>(FriendLinkCreateParams {
            expires_in_secs: None,
            single_use: Some(false),
        })
        .await;
    d.ok::<m::FriendAdd>(UrlParams { url: link.url }).await;
    wait_contact(&a2, d_user, ContactKind::Temporary, "두 번째 연락처가 감").await;
    a1.ok::<m::FriendLinkRevoke>(TokenIdParams {
        token_id: link.token_id,
    })
    .await;
    wait_contact(&a2, d_user, ContactKind::Expired, "만료가 감").await;
    // 만료된 연락처 삭제는 내 다른 기기로도 가서, 그 기기가 다시 알려 되살아나지 않습니다(아래 다시 연결로 확인).
    a2.ok::<m::FriendRemove>(UserParams { user: d_user }).await;

    // 꺼져 있던 동안의 일은 다시 연결될 때 받습니다. 새 연락처를 가진 기기가 건 연결로도 받고, 꺼진 기기의 옛 기록이
    // 친구를 임시 연락처로 되돌리지 않습니다.
    common::meet_by_link(&a1, &e, Some(3600)).await;
    wait_contact(&a2, e_user, ContactKind::Temporary, "꺼지기 전 연락처가 감").await;
    a2.shutdown().await;
    common::make_friends(&a1, &e).await;
    common::meet_by_link(&a1, &f, Some(3600)).await;
    a2.start_again().await;
    connect(&a1, &a2).await;
    wait_contact(
        &a2,
        f_user,
        ContactKind::Temporary,
        "꺼져 있던 동안의 연락처가 감",
    )
    .await;
    wait_contact(&a2, e_user, ContactKind::Friend, "꺼져 있던 동안 친구가 됨").await;
    wait_contact(
        &a1,
        e_user,
        ContactKind::Friend,
        "옛 기록에 되돌아가지 않음",
    )
    .await;
    assert!(
        a2.ok::<m::FriendList>(Empty)
            .await
            .iter()
            .all(|x| x.user != d_user),
        "이 기기에서 지운 만료된 연락처는 돌아오지 않음"
    );
}

/// 이 노드의 받은 친구 요청에 그 사람이 들 때까지 기다려 요청 ID를 돌려줍니다.
async fn wait_request(n: &TestNode, user: UserId, what: &str) -> String {
    eventually(what, || async {
        n.ok::<m::FriendRequests>(Empty)
            .await
            .into_iter()
            .find(|r| r.user == user)
            .map(|r| r.request_id)
    })
    .await
}

/// 이 노드의 받은 친구 요청에서 그 사람이 빠질 때까지 기다립니다.
async fn wait_no_request(n: &TestNode, user: UserId, what: &str) {
    eventually(what, || async {
        n.ok::<m::FriendRequests>(Empty)
            .await
            .iter()
            .all(|r| r.user != user)
            .then_some(())
    })
    .await;
}

/// 상대는 친구 추가 요청을 닿는 기기 하나에만 보내도, 내 기기 모두에 보입니다. 그 사람과 연결된 적 없는 기기에서 수락해도
/// 상대까지 친구가 되고, 꺼져 있던 기기는 다시 연결될 때 받고, 한 기기의 거절은 다른 기기가 다시 알려도 되살아나지 않습니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn friend_request_reaches_my_other_device() {
    common::init_tracing();
    let cl = Cluster::new();
    let a1 = cl.node("a1").await;
    let mut a2 = common::linked(&cl, &a1, "a2", fast_sync).await;
    let c = cl.node("c").await;
    let d = cl.node("d").await;
    let e = cl.node("e").await;
    let (a_user, c_user, d_user, e_user) = (a1.user(), c.user(), d.user(), e.user());

    common::meet_by_link(&a1, &c, Some(3600)).await;
    wait_contact(&a2, c_user, ContactKind::Temporary, "연락처가 감").await;
    c.ok::<m::FriendPropose>(UserParams { user: a_user }).await;
    wait_request(&a1, c_user, "닿은 기기에 요청이 옴").await;
    let request_id = wait_request(&a2, c_user, "내 다른 기기에도 요청이 보임").await;
    a2.ok::<m::FriendAccept>(RequestIdParams { request_id })
        .await;
    wait_contact(&a1, c_user, ContactKind::Friend, "수락이 내 기기로 감").await;
    wait_contact(&c, a_user, ContactKind::Friend, "상대도 친구가 됨").await;
    wait_no_request(&a1, c_user, "수락한 요청이 지워짐").await;

    common::meet_by_link(&a1, &d, Some(3600)).await;
    wait_contact(&a2, d_user, ContactKind::Temporary, "두 번째 연락처가 감").await;
    a2.shutdown().await;
    d.ok::<m::FriendPropose>(UserParams { user: a_user }).await;
    let request_id = wait_request(&a1, d_user, "켜진 기기에 요청이 옴").await;
    a2.start_again().await;
    connect(&a1, &a2).await;
    wait_request(&a2, d_user, "꺼져 있던 기기도 요청을 받음").await;

    a1.ok::<m::FriendReject>(RequestIdParams { request_id })
        .await;
    wait_no_request(&a2, d_user, "거절이 내 다른 기기로 감").await;
    // 내 기기끼리 다시 맞춰도, 상대가 다시 보내도 거절한 요청은 돌아오지 않습니다.
    d.ok::<m::FriendPropose>(UserParams { user: a_user }).await;
    common::meet_by_link(&a1, &e, Some(3600)).await;
    wait_contact(&a2, e_user, ContactKind::Temporary, "다시 맞춰짐").await;
    for n in [&a1, &a2] {
        assert!(
            n.ok::<m::FriendRequests>(Empty)
                .await
                .iter()
                .all(|r| r.user != d_user),
            "거절한 요청은 되살아나지 않음"
        );
    }
}

/// 그 연락처와 1:1 공간이 이 기기에서 사라질 때까지 기다립니다.
async fn wait_cut(n: &TestNode, user: UserId, space: SpaceId, what: &str) {
    eventually(what, || async {
        let friend = n
            .ok::<m::FriendList>(Empty)
            .await
            .iter()
            .any(|f| f.user == user);
        (!friend && !testkit::known_spaces(&n.node).contains(&space)).then_some(())
    })
    .await;
}

/// 친구를 끊으면 양쪽의 모든 기기에서 연락처와 1:1 공간이 사라집니다. 꺼져 있던 내 기기도 다시 연결되면 지우고, 그 기기의
/// 옛 기록으로 되살아나지 않습니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn friend_cut_reaches_every_device() {
    common::init_tracing();
    let cl = Cluster::new();
    let a1 = cl.node("a1").await;
    let mut a2 = common::linked(&cl, &a1, "a2", fast_sync).await;
    let b1 = cl.node("b1").await;
    let b2 = common::linked(&cl, &b1, "b2", fast_sync).await;
    let (a_user, b_user) = (a1.user(), b1.user());
    let dm = befriend(&a1, &b1).await;
    wait_contact(&a2, b_user, ContactKind::Friend, "내 다른 기기도 친구").await;
    wait_contact(&b2, a_user, ContactKind::Friend, "상대의 다른 기기도 친구").await;

    a2.shutdown().await;
    a1.ok::<m::FriendRemove>(UserParams { user: b_user }).await;
    wait_cut(&b1, a_user, dm, "상대에게 끊기가 감").await;
    wait_cut(&b2, a_user, dm, "상대의 다른 기기에 끊기가 감").await;

    a2.start_again().await;
    connect(&a1, &a2).await;
    wait_cut(&a2, b_user, dm, "꺼져 있던 내 기기도 지움").await;
    // 내 기기끼리 한 번 더 맞춰도 돌아오지 않습니다.
    let c = cl.node("c").await;
    let c_user = c.user();
    common::meet_by_link(&a1, &c, Some(3600)).await;
    wait_contact(&a2, c_user, ContactKind::Temporary, "다시 맞춰짐").await;
    wait_cut(&a1, b_user, dm, "옛 기록으로 되살아나지 않음").await;
}

/// 끊어도 공간을 남기던 때의 한 사람만 남은 1:1 공간은 시작할 때 지웁니다. 둘이 있는 공간은 그대로입니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lone_one_to_one_spaces_are_swept_at_start() {
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let b = cl.node("b").await;
    let c = cl.node("c").await;
    let gone = befriend(&a, &b).await;
    let kept = befriend(&a, &c).await;
    testkit::leave_only_me(&a.node, &gone);
    testkit::sweep_cut(&a.node);
    wait_cut(&a, b.user(), gone, "한 사람만 남은 공간을 지움").await;
    assert!(testkit::known_spaces(&a.node).contains(&kept));
}

/// 기기 이름 바꾸기는 내 기기와 친구에게 가고, 휴대폰 여부는 내 기기끼리 알며, 폐기한 기기는 이름이 지워지고 친구에게 보이지 않습니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn device_names_and_kinds_reach_my_devices() {
    common::init_tracing();
    let cl = Cluster::new();
    let a1 = cl.node("a1").await;
    let a2 = common::linked(&cl, &a1, "a2", fast_sync).await;
    let b = cl.node("b").await;
    befriend(&a1, &b).await;
    let (a_user, d1, d2) = (a1.user(), dev(&a1), dev(&a2));

    a1.ok::<m::DeviceRename>(DeviceRenameParams {
        device: d2,
        name: "  내 폰  ".into(),
    })
    .await;
    eventually("내 다른 기기가 새 이름을 앎", || async {
        a2.ok::<m::DeviceList>(Empty)
            .await
            .iter()
            .any(|d| d.entry.device == d2 && d.entry.name == "내 폰")
            .then_some(())
    })
    .await;
    eventually("친구도 새 이름을 앎", || async {
        b.ok::<m::UserDevices>(UserParams { user: a_user })
            .await
            .iter()
            .any(|d| d.device == d2 && d.name == "내 폰")
            .then_some(())
    })
    .await;
    assert!(
        a1.call::<m::DeviceRename>(DeviceRenameParams {
            device: d1,
            name: "   ".into(),
        })
        .await
        .is_err()
    );

    let devices = Box::new(testkit::signed_devices(&a2.node).expect("목록"));
    let r = testkit::net_request(
        &a2.node,
        a1.peer(),
        YpRequest::Social(SocialRequest::Devices {
            devices,
            spaces: Vec::new(),
            contacts: Vec::new(),
            cut: Vec::new(),
            mobile: true,
        }),
    )
    .await
    .expect("요청");
    assert!(matches!(r, YpResponse::Social(_)), "{r:?}");
    let list = a1.ok::<m::DeviceList>(Empty).await;
    assert!(list.iter().any(|d| d.entry.device == d2 && d.mobile));
    assert!(list.iter().any(|d| d.entry.device == d1 && !d.mobile));

    a1.ok::<m::DeviceRevoke>(DeviceRevokeParams { device: d2 })
        .await;
    eventually("친구에게 폐기한 기기가 보이지 않음", || async {
        let shown = b.ok::<m::UserDevices>(UserParams { user: a_user }).await;
        (shown.len() == 1 && shown[0].device == d1).then_some(())
    })
    .await;
    assert!(
        a1.ok::<m::DeviceList>(Empty)
            .await
            .iter()
            .any(|d| d.entry.device == d2 && d.entry.name.is_empty())
    );
}

/// 예전 기본 이름 "이 기기"는 다음 시작 때 기본 이름으로 바뀝니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn old_default_device_name_is_replaced_at_start() {
    let cl = Cluster::new();
    let mut a = cl
        .node_with("a", |c| c.device_name = "이 기기".into())
        .await;
    a.shutdown().await;
    a.start_again().await;
    eventually("기본 이름으로 바뀜", || async {
        a.ok::<m::DeviceList>(Empty)
            .await
            .iter()
            .all(|d| d.entry.name != "이 기기")
            .then_some(())
    })
    .await;
}

/// 내 기기들이 저마다 시작하며 목록을 고쳐도 같은 목록으로 모이고, 그 뒤의 폐기도 서로 받습니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn devices_tidying_at_once_settle_on_one_list() {
    common::init_tracing();
    let cl = Cluster::new();
    let mut a1 = cl
        .node_with("a1", |c| c.device_name = "이 기기".into())
        .await;
    let mut a2 = common::linked(&cl, &a1, "a2", |c| {
        fast_sync(c);
        c.device_name = "이 기기".into();
    })
    .await;
    let a3 = common::linked(&cl, &a1, "a3", fast_sync).await;
    let d3 = dev(&a3);
    wait_own_device(&a2, d3).await;
    a1.shutdown().await;
    a2.shutdown().await;
    a1.start_again().await;
    a2.start_again().await;
    connect(&a1, &a2).await;
    let names = |n: &TestNode| {
        let list = testkit::device_list(&n.node, &n.user()).expect("목록");
        let mut v: Vec<(DeviceId, String)> = list
            .devices
            .into_iter()
            .map(|d| (d.device, d.name))
            .collect();
        v.sort();
        v
    };
    eventually("두 기기의 목록이 같아짐", || async {
        let (x, y) = (names(&a1), names(&a2));
        (x == y && x.iter().all(|(_, n)| n != "이 기기")).then_some(())
    })
    .await;

    a1.ok::<m::DeviceRevoke>(DeviceRevokeParams { device: d3 })
        .await;
    eventually("폐기가 내 다른 기기로 감", || async {
        testkit::device_list(&a2.node, &a2.user())
            .expect("목록")
            .is_revoked(&d3)
            .then_some(())
    })
    .await;
}

/// 친구가 아는 내 기기를 살아 있는 내 기기가 폐기하면 친구와 1:1 공간 상태 모두 그 기기를 폐기로 보고 그 기기는 지웁니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn revoke_reaches_friend_and_shared_space() {
    common::init_tracing();
    let cl = Cluster::new();
    let a1 = cl.node("a1").await;
    let b = cl.node("b").await;
    let space = befriend(&a1, &b).await;
    let a_user = a1.user();
    let thief = common::linked(&cl, &a1, "thief", fast_sync).await;
    let dt = dev(&thief);
    connect(&thief, &b).await;
    wait_active(&b, a_user, dt, "친구가 앎").await;
    eventually("1:1 공간 상태가 그 기기를 담음", || async {
        testkit::space_state(&b.node, &space)?
            .devices
            .get(&a_user)?
            .contains_active(&dt)
            .then_some(())
    })
    .await;

    a1.ok::<m::DeviceRevoke>(DeviceRevokeParams { device: dt })
        .await;
    eventually("친구가 거부를 받음", || async {
        device_seen(&b, a_user, dt).2.then_some(())
    })
    .await;
    eventually("1:1 공간 상태도 폐기로 봄", || async {
        let st = testkit::space_state(&b.node, &space)?;
        st.devices.get(&a_user)?.is_revoked(&dt).then_some(())
    })
    .await;
    wait_wiped(&thief, b.peer()).await;
}

/// 친구가 남의 기기 하나를 뒤늦게 기기 차단합니다: 그 기기의 인증을 거절하고 그 기기가 쓴 글만 접으며(같은
/// 사람의 다른 기기 글은 그대로), 풀면 되돌아갑니다. 내 기기는 차단할 수 없습니다(폐기로).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn friend_blocks_one_device_of_an_account() {
    common::init_tracing();
    let cl = Cluster::new();
    let a1 = cl.node("a1").await;
    let b = cl.node("b").await;
    let space = befriend(&a1, &b).await;
    let channel = a1.first_channel(space).await;
    let a_user = a1.user();
    let thief = common::linked(&cl, &a1, "thief", fast_sync).await;
    let dt = dev(&thief);
    connect(&thief, &b).await;
    wait_active(&b, a_user, dt, "친구가 앎").await;
    let b_user = b.user();
    send_when_ready(&thief, space, channel, "도둑 기기 글").await;
    sees_text(&b, space, channel, "도둑 기기 글").await;
    a1.send(space, channel, "주인 기기 글").await;
    sees_text(&b, space, channel, "주인 기기 글").await;

    b.ok::<m::UserBlockDevice>(UserDeviceParams {
        user: a_user,
        device: dt,
    })
    .await;
    let folded = |text: &'static str| {
        let b = &b;
        async move {
            b.ok::<m::MessageList>(list_params(space, channel))
                .await
                .messages
                .into_iter()
                .find(|v| v.text == text)
                .map(|v| v.author_blocked)
        }
    };
    assert_eq!(folded("도둑 기기 글").await, Some(true));
    assert_eq!(folded("주인 기기 글").await, Some(false));
    let devices = b.ok::<m::UserDevices>(UserParams { user: a_user }).await;
    assert!(devices.iter().any(|d| d.device == dt && d.blocked));
    assert!(devices.iter().any(|d| d.device == dev(&a1) && !d.blocked));
    assert_eq!(b.ok::<m::UserBlockedDevices>(Empty).await.len(), 1);
    // B는 그 기기의 인증을 잊었고 다시 믿지 않습니다(B가 걸어도, 그 기기가 걸어 와도 거절).
    assert!(
        testkit::authenticate(&b.node, thief.peer()).await.is_err(),
        "차단한 기기를 다시 믿지 않음"
    );
    assert!(
        b.call::<m::UserBlockDevice>(UserDeviceParams {
            user: b_user,
            device: dev(&b),
        })
        .await
        .is_err(),
        "내 기기는 차단 대신 폐기"
    );

    b.ok::<m::UserUnblockDevice>(UserDeviceParams {
        user: a_user,
        device: dt,
    })
    .await;
    assert_eq!(folded("도둑 기기 글").await, Some(false));
    assert!(b.ok::<m::UserBlockedDevices>(Empty).await.is_empty());
}

// ───────────── 5. 커밋 확정 기기 넘기기 ─────────────

/// 크루 소유자가 새 기기를 들인 뒤 옛 기기(커밋 확정 기기)를 잃었습니다. 새 기기가 옛 기기를 폐기하면, 상태가
/// 새 기기를 확정 기기로 정하고 남은 멤버가 키를 넘깁니다. 새 확정 기기는 공간 그룹과 비공개 채널 그룹을 다시 세워
/// 멤버를 넣고, 새 멤버를 초대로 받으며, 비공개 채널에서 계속 주고받습니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn committer_handover_reestablishes_groups() {
    common::init_tracing();
    let cl = Cluster::new();
    let o1 = cl.node("o1").await;
    let b = cl.node("b").await;
    let c = cl.node("c").await;
    let o_user = o1.user();
    let (do1, db) = (dev(&o1), dev(&b));
    let space = create_crew(&o1, "넘기기 크루").await;
    let general = o1.first_channel(space).await;
    join_by_invite(&o1, &b, space).await;
    wait_member(&o1, space, b.user()).await;
    wait_space_group(&[&o1, &b], space, &[db], "B가 그룹에 들어감").await;
    let role = o1
        .ok::<m::RoleCreate>(RoleCreateParams {
            space,
            name: "비밀".into(),
            color: None,
            permissions: Vec::new(),
            rank: Some(5),
        })
        .await
        .role;
    o1.ok::<m::MemberSetRoles>(MemberSetRolesParams {
        space,
        user: b.user(),
        roles: vec![role],
    })
    .await;
    let secret = o1
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
            private_roles: Some(vec![role]),
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
    wait_channel_group(
        &[&o1, &b],
        space,
        secret,
        &[db],
        "B가 비공개 채널 그룹에 들어감",
    )
    .await;
    b.send(space, secret, "잃기 전 비밀").await;
    sees_text(&o1, space, secret, "잃기 전 비밀").await;
    assert_eq!(testkit::committer_of(&b.node, &space), Some(do1));

    // 소유자가 새 기기를 들이고, 옛 기기가 멤버의 상태에 알린 뒤 사라집니다.
    let o2 = common::linked(&cl, &o1, "o2", fast_sync).await;
    let do2 = dev(&o2);
    connect(&o2, &b).await;
    eventually("크루 상태가 새 기기를 앎", || async {
        testkit::space_state(&b.node, &space)?
            .devices
            .get(&o_user)?
            .contains_active(&do2)
            .then_some(())
    })
    .await;
    // 옛 확정 기기는 아직 활성이라 넘기기 전입니다.
    assert_eq!(testkit::committer_of(&b.node, &space), Some(do1));
    // 인증이 먼저 끝나고 크루는 그 뒤에 알려 올 수 있으므로 크루를 받아들일 때까지 기다립니다.
    eventually("O2가 B를 믿음(크루를 알려 받음)", || async {
        testkit::authenticate(&o2.node, b.peer()).await.ok()?;
        testkit::genesis_of(&o2.node, &space).map(|_| ())
    })
    .await;
    o1.shutdown().await;
    // 멤버가 해소기가 받지 않을 키 갱신(순번이 터무니없이 큼)을 넣어 둡니다. 새 확정 기기는 받아들인 사슬만 보고
    // 그룹을 다시 세웁니다(이것을 믿으면 자기 갱신을 끝없이 돌며 적용 잠금을 쥠).
    let junk = vec![
        testkit::plant_key_update(&b.node, space, None, 1 << 40)
            .await
            .expect("공간 그룹 커밋"),
        testkit::plant_key_update(&b.node, space, Some(secret), 1 << 40)
            .await
            .expect("채널 그룹 커밋"),
    ];
    testkit::ingest_from(&o2.node, space, junk, b.peer())
        .await
        .expect("넣기");

    // 새 기기가 잃어버린 옛 기기를 폐기합니다.
    o2.ok::<m::DeviceRevoke>(DeviceRevokeParams { device: do1 })
        .await;
    eventually("B가 본 확정 기기가 O2로 바뀜", || async {
        (testkit::committer_of(&b.node, &space) == Some(do2)).then_some(())
    })
    .await;
    eventually(
        "O2가 크루 상태를 세움(넘겨받은 상태 키)",
        || async {
            testkit::space_state(&o2.node, &space)?
                .is_member(&o_user)
                .then_some(())
        },
    )
    .await;
    wait_space_group(&[&o2, &b], space, &[do2, db], "O2가 공간 그룹을 다시 세움").await;
    wait_channel_group(
        &[&o2, &b],
        space,
        secret,
        &[do2, db],
        "O2가 비공개 채널 그룹을 다시 세움",
    )
    .await;
    let members = testkit::crew_key_members(&b.node, &space).expect("그룹");
    assert!(!members.contains(&do1), "폐기한 옛 기기는 새 그룹에 없음");
    assert!(
        testkit::crew_key_seq(&o2.node, &space).is_some_and(|e| e < 1 << 20)
            && testkit::channel_key_seq(&o2.node, &space, secret).is_some_and(|e| e < 1 << 20),
        "받지 않은 갱신의 순번을 따르지 않음"
    );

    // 비공개 채널을 계속 씁니다.
    send_when_ready(&b, space, secret, "넘긴 뒤 비밀").await;
    sees_text(&o2, space, secret, "넘긴 뒤 비밀").await;
    send_when_ready(&o2, space, secret, "새 확정 기기의 답").await;
    sees_text(&b, space, secret, "새 확정 기기의 답").await;

    // 새 멤버가 새 확정 기기의 초대로 들어옵니다.
    join_by_invite(&o2, &c, space).await;
    wait_space_group(&[&o2, &b, &c], space, &[dev(&c)], "C가 그룹에 들어감").await;
    send_when_ready(&o2, space, general, "새 확정 기기가 맞음").await;
    sees_text(&c, space, general, "새 확정 기기가 맞음").await;
    // 멤버끼리 직접 연결·인증되면(실제로는 위치 기록으로 찾음) B와 C도 주고받습니다.
    connect_both(&b, &c).await;
    eventually("B·C 인증", || async {
        testkit::authenticate(&b.node, c.peer()).await.ok()
    })
    .await;
    send_when_ready(&b, space, general, "C 어서 와").await;
    sees_text(&c, space, general, "C 어서 와").await;
    send_when_ready(&c, space, general, "반가워").await;
    sees_text(&o2, space, general, "반가워").await;
}

// ───────────── 6. 동시 가입을 커밋 하나로 ─────────────

/// 같은 초대 링크로 여러 명이 동시에 들어오면 확정 기기가 가입을 모아 키 갱신 하나(가입 목록 하나)로 넣습니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn simultaneous_joins_share_one_commit() {
    common::init_tracing();
    const N: usize = 4;
    let cl = Cluster::new();
    let o = cl.node("o").await;
    let space = create_crew(&o, "붐비는 크루").await;
    let general = o.first_channel(space).await;
    let mut joiners = Vec::new();
    for i in 0..N {
        let j = cl.node(&format!("j{i}")).await;
        connect_both(&o, &j).await;
        joiners.push(j);
    }
    let url = invite_url(&o, space, N as u32).await;
    let (commits_before, _) = testkit::crew_key_updates(&o.node, &space);

    // 맞추기를 잠시 붙잡아 가입이 모두 상태에 들어오게 합니다(묶음 창 안에 함께 온 것과 같음).
    testkit::hold_commits(&o.node, true);
    let handles: Vec<_> = joiners
        .iter()
        .map(|j| {
            let node: Arc<Node> = j.node.clone();
            let url = url.clone();
            tokio::spawn(
                async move { common::call::<m::InviteJoin>(&node, UrlParams { url }).await },
            )
        })
        .collect();
    for j in &joiners {
        wait_member(&o, space, j.user()).await;
    }
    assert_eq!(
        testkit::crew_key_updates(&o.node, &space).0,
        commits_before,
        "붙잡은 동안에는 커밋이 없음"
    );
    testkit::hold_commits(&o.node, false);
    for h in handles {
        let r = h.await.expect("작업").expect("가입");
        assert_eq!(r.space, space);
    }

    let devices: Vec<DeviceId> = joiners.iter().map(dev).collect();
    let mut all: Vec<&TestNode> = vec![&o];
    all.extend(joiners.iter());
    wait_space_group(&all, space, &devices, "모두 같은 순번의 그룹에 들어감").await;
    let (commits, join_lists) = testkit::crew_key_updates(&o.node, &space);
    assert_eq!(commits, commits_before + 1, "가입 {N}개가 커밋 하나로");
    let want: BTreeSet<DeviceId> = devices.iter().copied().collect();
    let joined: Vec<&Vec<DeviceId>> = join_lists
        .iter()
        .filter(|w| w.iter().any(|d| want.contains(d)))
        .collect();
    assert_eq!(joined.len(), 1, "가입 목록 하나: {joined:?}");
    assert_eq!(joined[0].iter().copied().collect::<BTreeSet<_>>(), want);

    o.send(space, general, "모두 환영").await;
    for j in &joiners {
        sees_text(j, space, general, "모두 환영").await;
    }
}

// ───────────── 7. 휘발성 세션 키 갱신 재전송 ─────────────

/// 입장·퇴장 방식 휘발성 채널에서 갱신 담당 기기가 새 세션 키를 한 참가자에게 보내지 못해도(두 번 유실) 다시 보내
/// 모든 참가자가 같은 새 세대로 모이고 서로의 메시지를 읽습니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn volatile_key_update_is_retried_after_dropped_send() {
    common::init_tracing();
    let cl = Cluster::new();
    let mut nodes = Vec::new();
    for name in ["a", "b", "c"] {
        nodes.push(cl.node(name).await);
    }
    for i in 0..nodes.len() {
        for j in i + 1..nodes.len() {
            connect_both(&nodes[i], &nodes[j]).await;
        }
    }
    let others: Vec<Arc<Node>> = nodes[1..].iter().map(|n| n.node.clone()).collect();
    let space = vt::create_group_space(&nodes[0].node, &others)
        .await
        .expect("크루");
    for i in 0..nodes.len() {
        for j in i + 1..nodes.len() {
            testkit::authenticate(&nodes[i].node, nodes[j].peer())
                .await
                .expect("멤버끼리 인증");
        }
    }
    let mut settings = ChannelSettings::text("휘발");
    settings.security = SecurityLevel::Volatile;
    settings.join_mode = JoinMode::JoinLeave;
    settings.link_previews = false;
    let ch = vt::create_channel_with(&nodes[0].node, space, settings)
        .await
        .expect("채널");
    for n in &nodes {
        eventually("채널이 보임", || async {
            n.ok::<m::ChannelList>(SpaceParams { space })
                .await
                .iter()
                .any(|v| v.channel == ch)
                .then_some(())
        })
        .await;
    }
    let join = |n: &TestNode| {
        let node = n.node.clone();
        async move {
            common::call::<m::ChannelJoin>(&node, SpaceChannelParams { space, channel: ch })
                .await
                .expect("입장");
        }
    };
    let gen_of = |n: &TestNode| vt::volatile_current_generation(&n.node, &space, &ch);
    let (a, b, c) = (&nodes[0], &nodes[1], &nodes[2]);
    join(a).await;
    join(b).await;
    let g1 = eventually("A·B가 같은 세대", || async {
        let g = gen_of(a)?;
        (gen_of(b) == Some(g)).then_some(g)
    })
    .await;

    // C가 들어오면 참가자 중 기기 ID가 가장 작은 기기가 새 키를 돌립니다. 그 기기가 다른 기존 참가자에게 보내는 갱신을
    // 두 번 잃게 합니다.
    let rotator = nodes.iter().min_by_key(|n| dev(n)).expect("노드");
    let target = [a, b]
        .into_iter()
        .find(|n| n.name != rotator.name)
        .expect("대상");
    vt::fail_key_to(&rotator.node, target.peer(), 2);
    join(c).await;
    let g2 = eventually("세 참가자가 같은 새 세대", || async {
        let g = gen_of(a)?;
        (g != g1 && gen_of(b) == Some(g) && gen_of(c) == Some(g)).then_some(g)
    })
    .await;
    assert_ne!(g1, g2);
    assert_eq!(
        vt::key_faults_left(&rotator.node, target.peer()),
        0,
        "걸어 둔 유실을 모두 겪고 다시 보냄"
    );

    let text = format!("갱신 뒤 {}", target.name);
    let id = send_when_ready(target, space, ch, &text).await;
    for n in &nodes {
        wait_message(
            n,
            space,
            ch,
            id,
            "새 세대로 보낸 메시지를 읽음",
            |v| v.text == text,
        )
        .await;
    }
    let id = send_when_ready(c, space, ch, "C의 인사").await;
    wait_message(
        target,
        space,
        ch,
        id,
        "대상이 C의 메시지를 읽음",
        |v| v.text == "C의 인사",
    )
    .await;
}

// ───────────── 8. 큰 개인 메모 ─────────────

/// 묶음 상한(4MiB)을 넘는 개인 메모도 여러 묶음으로 나눠 모두 내 새 기기로 갑니다(잘리지 않음).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn notes_over_four_mib_reach_new_device() {
    common::init_tracing();
    const COUNT: usize = 80;
    const LEN: usize = 60_000;
    let cl = Cluster::new();
    let a1 = cl.node("a1").await;
    let mut notes = Vec::new();
    for i in 0..COUNT {
        let user = UserId::random();
        let fill = char::from(b'a' + (i % 26) as u8);
        let text = format!("{i:04}:{}", fill.to_string().repeat(LEN));
        a1.ok::<m::NoteSet>(NoteSetParams {
            user,
            text: text.clone(),
        })
        .await;
        notes.push((user, text));
    }
    let total: usize = notes.iter().map(|(_, t)| t.len()).sum();
    assert!(total > 4 * 1024 * 1024, "{total}바이트");

    let a2 = common::linked(&cl, &a1, "a2", |_| {}).await;
    wait_own_device(&a1, dev(&a2)).await;
    for (user, text) in &notes {
        eventually("새 기기가 메모를 받음", || async {
            let got = a2.ok::<m::NoteGet>(UserParams { user: *user }).await.text;
            (got == *text).then_some(())
        })
        .await;
    }
}

// ───────────── 9. 새 기기와 폐기된 기기 ─────────────

/// 훔친 기기 A2를 폐기한 뒤 A3를 들입니다. A2의 키를 가진 쪽은 신원 키로 무엇이든 서명할 수 있지만, 그 기기가 공동 서명한
/// 이 기기의 폐기 목록은 친구가 전해도 따르지 않습니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn linked_device_ignores_revocation_from_revoked_device() {
    common::init_tracing();
    let cl = Cluster::new();
    let a1 = cl.node("a1").await;
    let b = cl.node("b").await;
    befriend(&a1, &b).await;
    let a_user = a1.user();
    let a2 = common::linked(&cl, &a1, "a2", |_| {}).await;
    let d2 = dev(&a2);
    wait_active(&b, a_user, d2, "친구가 A2를 앎").await;
    let stolen = testkit::capture_keys(&a2.node).expect("키");
    a1.ok::<m::DeviceRevoke>(DeviceRevokeParams { device: d2 })
        .await;
    wait_revoked(&b, a_user, d2, "친구가 폐기를 앎").await;
    a2.shutdown().await;

    let a3 = common::linked(&cl, &a1, "a3", fast_sync).await;
    let d3 = dev(&a3);
    connect(&a3, &b).await;
    eventually("A3가 B를 인증", || async {
        testkit::authenticate(&a3.node, b.peer()).await.ok()
    })
    .await;
    let mine = testkit::device_list(&a3.node, &a_user).expect("내 목록");
    assert!(mine.is_revoked(&d2), "연결할 때 받은 목록에 폐기가 있음");
    let entry3 = mine
        .devices
        .iter()
        .find(|d| d.device == d3)
        .expect("내 항목")
        .clone();
    let kill = stolen
        .sign(mine.with_entry(DeviceEntry {
            revoked_at: Some(Timestamp::now()),
            ..entry3
        }))
        .expect("서명");
    testkit::on_revoked(&a3.node, b.peer(), &kill);
    assert!(device_seen(&a3, a_user, d3).1);
    assert!(!testkit::is_wiped(&a3.node));
}

// ───────────── 10. 모르는 기기가 낸 폐기 ─────────────

/// 노트북 A2가 꺼져 있는 동안 새 기기 A3가 들어오고, A3가 A2를 폐기합니다. 다시 켠 A2는 A3를 모르지만, 친구가 알려 준
/// 폐기 목록이 A2의 목록을 그대로 이어받으므로 따르고 로컬 데이터를 지웁니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn revoked_device_wipes_when_revoker_joined_while_offline() {
    common::init_tracing();
    let cl = Cluster::new();
    let a1 = cl.node("a1").await;
    let b = cl.node("b").await;
    befriend(&a1, &b).await;
    let a_user = a1.user();
    let mut a2 = common::linked(&cl, &a1, "a2", |_| {}).await;
    let d2 = dev(&a2);
    wait_own_device(&a1, d2).await;
    wait_active(&b, a_user, d2, "친구가 A2를 앎").await;
    // A2가 친구를 인증해 둡니다(친구의 노드를 친구의 것으로 기억).
    connect_both(&a2, &b).await;
    eventually("A2·B 인증", || async {
        testkit::authenticate(&a2.node, b.peer()).await.ok()
    })
    .await;
    a2.shutdown().await;

    let a3 = common::linked(&cl, &a1, "a3", |_| {}).await;
    let d3 = dev(&a3);
    wait_own_device(&a1, d3).await;
    wait_active(&a3, a_user, d2, "A3가 A2를 앎").await;
    a3.ok::<m::DeviceRevoke>(DeviceRevokeParams { device: d2 })
        .await;
    wait_revoked(&b, a_user, d2, "친구가 폐기를 앎").await;

    a2.start_again().await;
    connect(&a2, &b).await;
    wait_wiped(&a2, b.peer()).await;
    assert!(testkit::known_spaces(&a2.node).is_empty());
    assert!(!testkit::is_wiped(&a1.node) && !testkit::is_wiped(&a3.node));
}

// ───────────── 11. 소유자가 확정 기기를 지정 ─────────────

/// 소유자가 비공개 채널 그룹에 들지 않은 관리자의 기기를 확정 기기로 정합니다(`crew.set_committer`). 옛 확정 기기가 키
/// 그룹 표를 넘기고, 새 확정 기기가 그룹을 다시 맞춰 자신(관리자라 볼 수 있음)을 넣어 비공개 채널을 계속 씁니다. 그 채널을
/// 볼 수 없는 멤버 D를 두어, 관리자가 생겨도 채널이 크루 키로 옮겨 가지 않고 비공개 그룹에 남게 합니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn owner_designates_admin_committer_outside_private_group() {
    common::init_tracing();
    let cl = Cluster::new();
    let o = cl.node("o").await;
    let b = cl.node("b").await;
    let c = cl.node("c").await;
    let d = cl.node("d").await;
    let (d_o, db, dc, dd) = (dev(&o), dev(&b), dev(&c), dev(&d));
    let space = create_crew(&o, "지정 크루").await;
    join_by_invite(&o, &b, space).await;
    join_by_invite(&o, &c, space).await;
    join_by_invite(&o, &d, space).await;
    wait_member(&o, space, b.user()).await;
    wait_member(&o, space, c.user()).await;
    wait_member(&o, space, d.user()).await;
    wait_space_group(&[&o, &b, &c], space, &[db, dc, dd], "B·C·D가 그룹에 들어감").await;
    connect_both(&b, &c).await;
    eventually("B·C 인증", || async {
        testkit::authenticate(&b.node, c.peer()).await.ok()
    })
    .await;
    let secret_role = o
        .ok::<m::RoleCreate>(RoleCreateParams {
            space,
            name: "비밀".into(),
            color: None,
            permissions: Vec::new(),
            rank: Some(5),
        })
        .await
        .role;
    o.ok::<m::MemberSetRoles>(MemberSetRolesParams {
        space,
        user: c.user(),
        roles: vec![secret_role],
    })
    .await;
    let secret = o
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
            private_roles: Some(vec![secret_role]),
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
    wait_channel_group(
        &[&o, &c],
        space,
        secret,
        &[d_o, dc],
        "C가 비공개 채널 그룹에 들어감",
    )
    .await;
    send_when_ready(&c, space, secret, "지정 전 비밀").await;
    sees_text(&o, space, secret, "지정 전 비밀").await;

    // 맞추기를 멈춘 채 B를 관리자로 올리고 확정 기기로 정합니다(B는 비공개 채널 그룹에 들지 않은 채).
    testkit::hold_commits(&o.node, true);
    let admin = o
        .ok::<m::RoleCreate>(RoleCreateParams {
            space,
            name: "관리".into(),
            color: None,
            permissions: vec!["administrator".into()],
            rank: Some(10),
        })
        .await
        .role;
    o.ok::<m::MemberSetRoles>(MemberSetRolesParams {
        space,
        user: b.user(),
        roles: vec![admin],
    })
    .await;
    o.ok::<m::CrewSetCommitter>(CommitterSetParams { space, device: db })
        .await;
    assert!(
        !testkit::key_group_members(&o.node, &space, Some(secret))
            .unwrap_or_default()
            .contains(&db),
        "B는 아직 비공개 채널 그룹에 없음"
    );
    testkit::hold_commits(&o.node, false);
    eventually("B가 본 확정 기기가 B", || async {
        (testkit::committer_of(&b.node, &space) == Some(db)).then_some(())
    })
    .await;

    wait_channel_group(
        &[&b, &o, &c],
        space,
        secret,
        &[db, d_o, dc],
        "B가 비공개 채널 그룹을 다시 세움",
    )
    .await;
    assert!(
        !testkit::key_group_members(&b.node, &space, Some(secret))
            .unwrap_or_default()
            .contains(&dd),
        "볼 수 없는 D는 비공개 채널 그룹에 없음"
    );
    send_when_ready(&c, space, secret, "지정 뒤 비밀").await;
    sees_text(&b, space, secret, "지정 뒤 비밀").await;
    sees_text(&o, space, secret, "지정 뒤 비밀").await;
    send_when_ready(&b, space, secret, "새 확정 기기의 답").await;
    sees_text(&c, space, secret, "새 확정 기기의 답").await;
    sees_text(&o, space, secret, "새 확정 기기의 답").await;
}

// ───────────── 12. 사이 판보다 먼저 온 판 ─────────────

/// 공동 서명 기기를 더한 사이 판보다 먼저 온 판(직접 전송은 순서를 지키지 않음)은 버리지 않고 미뤄 두었다가, 사이 판을
/// 받으면 이어서 받습니다. 아는 목록에서 폐기된 기기가 만든 판은 미루지 않습니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn device_list_arriving_before_its_cosigner_is_deferred() {
    common::init_tracing();
    let cl = Cluster::new();
    let a1 = cl.node("a1").await;
    let b = cl.node("b").await;
    befriend(&a1, &b).await;
    let a_user = a1.user();
    // 아무와도 연결하지 않은 A2: 키와 목록 항목만 씁니다.
    let a2 = cl.bare("a2").await;
    let d2 = testkit::install_stolen_seed(&a1.node, &a2.node).expect("같은 신원의 기기");
    let a2_keys = testkit::capture_keys(&a2.node).expect("키");
    let a2_entry = testkit::device_list(&a2.node, &a_user)
        .and_then(|l| l.devices.into_iter().find(|d| d.device == d2))
        .expect("A2 항목");

    // A1이 A2를 더한 판(사이 판), A2가 그 위에 기기를 하나 더한 판.
    let v1 = testkit::device_list(&b.node, &a_user).expect("목록");
    let mut v2 = v1.clone();
    v2.devices.push(a2_entry);
    v2.version += 1;
    let v2 = testkit::capture_keys(&a1.node)
        .expect("키")
        .sign(v2)
        .expect("서명");
    let (v3, extra) = testkit::forge_device_list_with(&a2_keys, &v2.list).expect("판");

    assert!(!testkit::accept_device_list(&b.node, &v3).expect("검사"));
    assert_eq!(device_seen(&b, a_user, extra), (false, false, false));
    assert!(testkit::accept_device_list(&b.node, &v2).expect("검사"));
    assert_eq!(
        testkit::device_list(&b.node, &a_user).as_ref(),
        Some(&v3.list),
        "사이 판을 받은 뒤 미뤄 둔 판도 받음"
    );
    assert!(device_seen(&b, a_user, extra).1);
}

// ───────────── 13. 없어진 OTP ─────────────

/// 예전 빌드가 남긴 OTP 비밀(내 계정 비밀, 친구가 맡긴 비밀)은 시작할 때 지웁니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn old_otp_secrets_are_wiped_on_start() {
    common::init_tracing();
    let cl = Cluster::new();
    let mut a = cl.node("a").await;
    let user = a.user();
    testkit::plant_old_otp(&a.node, &user);
    assert_eq!(testkit::old_otp(&a.node), 2);
    a.restart().await;
    assert_eq!(testkit::old_otp(&a.node), 0);
}

// ───────────── 14. 폐기된 기기가 쓴 글 ─────────────

/// 폐기된 기기가 쓴 글은 지난 기록이라 나중에 연결한 기기도 받아야 합니다. 남이 이어 쓴 글 아래에 있어도, 아무도 이어
/// 쓰지 않은 끝 글이어도 그 사람의 살아 있는 기기가 내주면 받아 빈 구간이 남지 않습니다. 다른 멤버가 내주는 것은 받지
/// 않습니다(폐기 뒤에 꾸민 글이 끼지 않게): 그 멤버에게만 붙은 새 멤버는 그 사람의 기기를 만날 때까지 빈 구간으로 둡니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn new_device_gets_what_a_revoked_device_wrote() {
    common::init_tracing();
    let cl = Cluster::new();
    let a1 = cl.node_with("a1", fast_sync).await;
    let b = cl.node_with("b", fast_sync).await;
    let space = create_crew(&a1, "기록 크루").await;
    let channel = a1
        .ok::<m::ChannelCreate>(ChannelCreateParams {
            space,
            name: "기록방".into(),
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
            anonymous: Some(true),
            attach_limit: None,
            no_edit: None,
            no_delete: None,
        })
        .await
        .channel;
    join_by_invite(&a1, &b, space).await;
    wait_member(&a1, space, b.user()).await;
    let a_user = a1.user();
    let old = common::linked(&cl, &a1, "old", fast_sync).await;
    let old_dev = dev(&old);
    wait_own_device(&a1, old_dev).await;
    connect(&old, &b).await;
    wait_space_group(
        &[&a1, &b, &old],
        space,
        &[old_dev],
        "옛 기기가 그룹에 들어감",
    )
    .await;

    for text in ["옛 기기 글 1", "옛 기기 글 2"] {
        send_when_ready(&old, space, channel, text).await;
        sees_text(&a1, space, channel, text).await;
        sees_text(&b, space, channel, text).await;
    }
    b.send(space, channel, "이어 쓴 글").await;
    sees_text(&old, space, channel, "이어 쓴 글").await;
    send_when_ready(&old, space, channel, "옛 기기 끝 글").await;
    sees_text(&a1, space, channel, "옛 기기 끝 글").await;

    a1.ok::<m::DeviceRevoke>(DeviceRevokeParams { device: old_dev })
        .await;
    wait_revoked(&b, a_user, old_dev, "친구가 폐기를 앎").await;

    let new = common::linked(&cl, &a1, "new", fast_sync).await;
    connect(&new, &b).await;
    // 살아 있는 기기가 쓴 글이 먼저 옵니다(아무것도 안 온 것과 구별).
    sees_text(&new, space, channel, "이어 쓴 글").await;
    for text in ["옛 기기 글 1", "옛 기기 글 2", "옛 기기 끝 글"] {
        sees_text(&new, space, channel, text).await;
    }
    let gaps = async |n: &TestNode| {
        n.ok::<m::MessageList>(list_params(space, channel))
            .await
            .gaps
            .len()
    };
    assert_eq!(gaps(&new).await, 0, "새 기기에 빈 구간이 남음");

    let role = a1
        .ok::<m::RoleCreate>(RoleCreateParams {
            space,
            name: "안내".into(),
            color: None,
            permissions: vec!["create_invite".into()],
            rank: Some(5),
        })
        .await
        .role;
    a1.ok::<m::MemberSetRoles>(MemberSetRolesParams {
        space,
        user: b.user(),
        roles: vec![role],
    })
    .await;
    let c = cl.node_with("c", fast_sync).await;
    eventually("B가 초대를 만들 수 있음", || async {
        testkit::space_state(&b.node, &space)?
            .base_permissions(&b.user())
            .contains(Permissions::CREATE_INVITE)
            .then_some(())
    })
    .await;
    join_by_invite(&b, &c, space).await;
    sees_text(&c, space, channel, "이어 쓴 글").await;
    let list = c.ok::<m::MessageList>(list_params(space, channel)).await;
    assert_eq!(list.gaps.len(), 1, "B만 내준 글은 받지 않음");
    assert!(list.messages.iter().all(|v| v.text == "이어 쓴 글"));

    connect_both(&c, &a1).await;
    for text in ["옛 기기 글 1", "옛 기기 글 2", "옛 기기 끝 글"] {
        sees_text(&c, space, channel, text).await;
    }
    assert_eq!(gaps(&c).await, 0, "A의 기기가 내주면 빈 구간이 채워짐");
}
