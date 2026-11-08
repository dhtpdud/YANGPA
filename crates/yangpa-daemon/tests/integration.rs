//! 영역을 넘나드는 통합 시험: 여러 작업 영역(크루 B1·휘발성 B2·블롭 B3·관리 B4·망 B5)이 함께 맞물려야만 통과하는
//! 다중 노드 시나리오.
//!
//! 1. 초대로 들어온 멤버끼리 위치 기록(DHT)으로 서로 찾아 직접 전하고, 비공개 채널의 첨부는 역할 있는 멤버만 받음.
//! 2. 휘발성 채널·음성 통화 중인 크루에서 자동화 토큰의 추방은 사람 확인 뒤에만 실행되고, 추방된 기기는 통화·휘발성
//!    세션 키·새 기록에서 빠지며, 휘발성 내용은 어느 노드의 디스크·로그에도 남지 않음.
//! 3. 복구 문구로 들인 새 기기가 내 다른 기기를 통해 크루(비공개 채널 포함)에 다시 들어가 읽고 씀.
//! 4. 크루 아이콘·커스텀 이모지가 블롭으로 다른 멤버에게 가고 그 멤버가 풀어 읽음(키는 로컬 API에 나오지 않음).
//! 5. 망 설정(Tor, B5)이 블롭·정책 영역(B3)에도 그대로 적용됨: Tor면 링크 미리보기를 직접 조회하지 않고 Tor 기본
//!    대역폭을 씀.
//! 6. 다른 기기에서 폐기한 내 기기(B4)는 통화(B5)·휘발성 세션(B2)·키 그룹에서 빠지고 새 기록·첨부(B3)를 받지 못함.
//! 7. 휘발성 첨부(X1)는 RAM에만 있어 저장 공간 정리(X4)의 한도·디스크 여유·비우기·고정·집계와 무관하고, 정리가 돌아도
//!    휘발성 내용이 디스크에 닿지 않음.
//! 8. 보관 노드(X3)가 지정된 크루에서 휘발성 채널에 글·첨부(X1)를 주고받아도 보관 노드의 명단·저장소·파일 어디에도
//!    휘발성 채널·첨부·내용이 없고, 보관 노드가 직접 달라고 해도 멤버가 주지 않음.
//! 9. 보관 노드(X3)가 받아 둔 첨부는 보관 노드의 저장 공간 정리(X4)로 비워지지 않고, 멤버가 정리로 비운 첨부는 올린
//!    사람이 꺼져 있어도 보관 노드에서 다시 받아 풂.
//!
//! 노드는 한 프로세스 안에서 127.0.0.1로 직접 연결합니다(`common` 참고). 고정 sleep 없이 조건을 기다립니다.

use crate::common;

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use base64::Engine;
use common::{
    Cluster, TestNode, connect, connect_both, eventually, listen_addrs, next_notification,
    wait_message, wait_own_device,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use yangpa_core::event::StreamId;
use yangpa_core::ids::{BlobId, ChannelId, DeviceId, EventId, RoleId, SpaceId, UserId};
use yangpa_core::model::{ChannelKind, HistoryVisibility, JoinMode, SecurityLevel};
use yangpa_core::wire::{YpRequest, YpResponse};
use yangpa_daemon::testkit;
use yangpa_daemon::testkit::archive as at;
use yangpa_daemon::testkit::net as tnet;
use yangpa_daemon::testkit::volatile as vt;
use yangpa_daemon::{ApiHandler, CallCtx, NodeConfig};
use yangpa_rpc::api::{
    ArchiveDesignateParams, ArchiveOfferParams, AttachmentInput, AuditListParams,
    BlobCacheLimitParams, BlobFetchParams, BlobParams, BlobReadParams, ChannelCreateParams,
    CrewCreateParams, CrewUpdateParams, DeviceRevokeParams, EmojiCreateParams, Empty, IdParams,
    IdentityCreateParams, InviteCreateParams, InviteJoinStatus, LeakcheckParams, MemberKickParams,
    MemberSetRolesParams, MessageListParams, MessageSendParams, Method, Notification,
    OptSpaceParams, ProfileGetParams, RoleCreateParams, Scope, SpaceChannelParams, SpaceParams,
    TokenCreateParams, UrlParams, m,
};
use yangpa_rpc::jsonrpc::{ERR_CONFIRMATION_REQUIRED, ERR_FORBIDDEN, ERR_NOT_FOUND};
use yangpa_rpc::{RpcError, RpcHandler};

// ───────────── 도우미 ─────────────

fn dev(n: &TestNode) -> DeviceId {
    testkit::device_id(&n.node).expect("기기 ID")
}

fn code<T: std::fmt::Debug>(r: Result<T, RpcError>) -> i64 {
    r.expect_err("오류여야 합니다").code
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

async fn create_crew(a: &TestNode, name: &str) -> SpaceId {
    a.ok::<m::CrewCreate>(CrewCreateParams {
        name: name.into(),
        description: None,
    })
    .await
    .space
}

/// `joiner`가 `gateway`의 초대 링크로 들어옵니다(둘만 연결).
async fn join_by_invite(gateway: &TestNode, joiner: &TestNode, space: SpaceId) {
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
    assert_eq!(r.space, space);
    assert_eq!(r.status, InviteJoinStatus::Joined, "{} 가입", joiner.name);
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

async fn create_role(a: &TestNode, space: SpaceId, name: &str) -> RoleId {
    a.ok::<m::RoleCreate>(RoleCreateParams {
        space,
        name: name.into(),
        color: None,
        permissions: Vec::new(),
        rank: Some(5),
    })
    .await
    .role
}

async fn wait_channel_known(n: &TestNode, space: SpaceId, channel: ChannelId) {
    eventually(&format!("{}가 채널을 앎", n.name), || async {
        testkit::space_state(&n.node, &space)?
            .channels
            .contains_key(&channel)
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

/// 진짜로 풀리는 작은 PNG.
fn png(width: u32, height: u32, seed: u8) -> Vec<u8> {
    let img = image::RgbImage::from_fn(width, height, |x, y| {
        image::Rgb([x as u8 ^ seed, y as u8, seed.wrapping_mul(7)])
    });
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(img)
        .write_to(&mut out, image::ImageFormat::Png)
        .unwrap();
    out.into_inner()
}

fn write_file(n: &TestNode, name: &str, bytes: &[u8]) -> String {
    let dir = n.dir.join("files");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, bytes).unwrap();
    path.to_string_lossy().into_owned()
}

async fn read_blob(n: &TestNode, space: SpaceId, blob: BlobId, key_from: EventId) -> Vec<u8> {
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

async fn wait_blob_complete(n: &TestNode, space: SpaceId, blob: BlobId, what: &str) {
    eventually(what, || async {
        let s = n
            .call::<m::BlobStatus>(BlobParams { space, blob })
            .await
            .ok()?;
        (s.total_chunks > 0 && s.have_chunks == s.total_chunks).then_some(())
    })
    .await;
}

/// 참가자 모두가 같은 기기 목록과 `above`보다 큰 같은 세대·같은 키를 볼 때까지 기다립니다.
async fn wait_call_agrees(
    nodes: &[&TestNode],
    space: SpaceId,
    channel: ChannelId,
    devices: &[DeviceId],
    above: u64,
    what: &str,
) -> (u64, [u8; 8]) {
    let want: BTreeSet<DeviceId> = devices.iter().copied().collect();
    eventually(what, || async {
        let mut seen: Option<(u64, [u8; 8])> = None;
        for n in nodes {
            let v = tnet::voice_call(&n.node, &space, &channel)?;
            let fp = v.key_fingerprint?;
            let set: BTreeSet<DeviceId> = v.devices.iter().copied().collect();
            if !v.joined || set != want || v.generation <= above {
                return None;
            }
            match seen {
                None => seen = Some((v.generation, fp)),
                Some(s) if s == (v.generation, fp) => {}
                Some(_) => return None,
            }
        }
        seen
    })
    .await
}

/// 주어진 노드들이 같은 휘발성 세션 키 세대로 보내게 될 때까지 기다립니다.
async fn volatile_converged(
    nodes: &[&TestNode],
    space: SpaceId,
    ch: ChannelId,
    not: &[u64],
) -> u64 {
    eventually("휘발성 세션 키 세대가 맞춰짐", || async {
        let gens: Vec<Option<u64>> = nodes
            .iter()
            .map(|n| vt::volatile_current_generation(&n.node, &space, &ch))
            .collect();
        let first = gens[0]?;
        (gens.iter().all(|g| *g == Some(first)) && !not.contains(&first)).then_some(first)
    })
    .await
}

// ───────────── 1. 위치 기록으로 서로 찾기 + 비공개 채널 첨부 ─────────────

/// A가 만든 크루에 B·C가 A의 초대로 들어옵니다. B와 C는 서로 연결한 적이 없어도 B가 보낸 메시지는 위치 기록(DHT)으로
/// C의 노드를 찾아 직접 전해지고 둘은 서로 인증합니다. 비공개 채널에 B가 올린 첨부는 역할이 있는 A만 받아 풀고,
/// 역할 없는 C는 B에게 직접 요청해도 받지 못합니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn invited_members_find_each_other_and_private_attachments_stay_private() {
    let cl = Cluster::new();
    let dht = cl.bare("dht").await;
    // 보낼 때 찾는 길을 보려고, 작은 크루 멤버에게 미리 거는 다시 걸기(`peers::spawn_redial`)는 시험 밖으로 미룹니다.
    let late_redial = |c: &mut NodeConfig| c.redial_interval = std::time::Duration::from_secs(3600);
    let a = cl.node_with("a", late_redial).await;
    let b = cl.node_with("b", late_redial).await;
    let c = cl.node_with("c", late_redial).await;
    let boot = format!("{}/p2p/{}", listen_addrs(&dht.node).await[0], dht.peer());
    for n in [&a, &b, &c] {
        n.add_bootstrap(&boot);
    }
    eventually("세 기기가 위치 기록을 올림", || async {
        [&a, &b, &c]
            .iter()
            .all(|n| tnet::locator_published(&n.node).is_some())
            .then_some(())
    })
    .await;

    let space = create_crew(&a, "찾기 크루").await;
    let general = a.first_channel(space).await;
    join_by_invite(&a, &b, space).await;
    join_by_invite(&a, &c, space).await;
    for n in [&a, &b, &c] {
        for u in [a.user(), b.user(), c.user()] {
            wait_member(n, space, u, true).await;
        }
    }
    wait_same_crew_key(&[&a, &b, &c], space).await;
    assert!(
        !tnet::is_authed(&b.node, &c.peer()),
        "B와 C는 아직 모르는 사이"
    );

    // B가 보낸 메시지는 C의 기기를 위치 기록으로 찾아 직접 전해집니다(A를 거친 따라잡기가 아니라).
    b.send(space, general, "B가 모두에게").await;
    sees_text(&c, space, general, "B가 모두에게").await;
    eventually("B와 C가 서로 인증", || async {
        (tnet::is_authed(&b.node, &c.peer()) && tnet::is_authed(&c.node, &b.peer())).then_some(())
    })
    .await;
    c.send(space, general, "C가 답함").await;
    sees_text(&b, space, general, "C가 답함").await;

    // 비공개 채널: 역할 '비밀'이 있는 B(와 소유자 A)만 봄.
    let role = create_role(&a, space, "비밀").await;
    a.ok::<m::MemberSetRoles>(MemberSetRolesParams {
        space,
        user: b.user(),
        roles: vec![role],
    })
    .await;
    let mut p = channel_params(space, "비밀방", ChannelKind::Text);
    p.private_roles = Some(vec![role]);
    let secret = a.ok::<m::ChannelCreate>(p).await.channel;
    eventually("B가 비공개 채널 그룹에 들어감", || async {
        let ea = testkit::channel_key_seq(&a.node, &space, secret)?;
        let eb = testkit::channel_key_seq(&b.node, &space, secret)?;
        (ea == eb).then_some(())
    })
    .await;
    wait_channel_known(&c, space, secret).await;
    assert!(
        testkit::channel_key_seq(&c.node, &space, secret).is_none(),
        "역할 없는 C가 채널 그룹에 있음"
    );
    // A는 채널을 열어 둡니다(열지 않은 크루 채널의 첨부는 자동으로 받지 않음).
    a.ok::<m::MessageList>(list_params(space, secret)).await;

    let file: Vec<u8> = common::noise(150_000, 3);
    let path = write_file(&b, "비밀 자료.bin", &file);
    let sent = b
        .ok::<m::MessageSend>(MessageSendParams {
            space,
            channel: secret,
            text: "비밀 첨부".into(),
            attachments: vec![AttachmentInput {
                path: path.clone(),
                ..Default::default()
            }],
            reply_to: None,
            thread: None,
        })
        .await
        .id;
    let blob = eventually("A가 첨부를 자동으로 받음", || async {
        let v = a
            .ok::<m::MessageList>(list_params(space, secret))
            .await
            .messages
            .into_iter()
            .find(|v| v.id == sent)?;
        let at = v.attachments.first()?;
        at.downloaded.then_some(at.blob)
    })
    .await;
    assert_eq!(read_blob(&a, space, blob, sent).await, file);

    // C: 채널 목록·메시지에 없고, 블롭도 로컬 API·직접 요청 모두 막힘.
    assert_eq!(
        code(c.call::<m::MessageList>(list_params(space, secret)).await),
        ERR_FORBIDDEN
    );
    assert!(testkit::message_text(&c.node, &sent).is_none());
    assert_eq!(
        code(c.call::<m::BlobStatus>(BlobParams { space, blob }).await),
        ERR_NOT_FOUND
    );
    for req in [
        YpRequest::BlobManifest { space, blob },
        YpRequest::BlobChunk {
            space,
            blob,
            index: 0,
        },
    ] {
        let r = testkit::net_request(&c.node, b.peer(), req)
            .await
            .expect("요청 전송");
        assert!(
            matches!(r, YpResponse::Error { .. }),
            "C가 비공개 첨부를 받음: {r:?}"
        );
    }
}

// ───────────── 2. 자동화 토큰의 추방: 사람 확인 → 통화·휘발성·기록에서 빠짐 ─────────────

/// 세 사람이 휘발성 채널과 음성 통화에 함께 있는 크루. 자동화 토큰의 `member.kick`은 확인 대기열로만 가고, 사람이
/// 승인하면 그때 C가 추방됩니다. 남은 두 사람은 새 통화 프레임 키·새 휘발성 세션 키로 넘어가고, C는 그 뒤 메시지를
/// 어느 쪽으로도 읽지 못합니다. 끝으로 모든 노드의 디스크·로그에 휘발성 평문이 없는지 누출 검사를 합니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn automation_kick_needs_human_then_member_leaves_call_and_volatile() {
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let b = cl.node("b").await;
    let c = cl.node("c").await;
    let space = create_crew(&a, "통화 크루").await;
    let general = a.first_channel(space).await;
    join_by_invite(&a, &b, space).await;
    join_by_invite(&a, &c, space).await;
    for n in [&a, &b, &c] {
        for u in [a.user(), b.user(), c.user()] {
            wait_member(n, space, u, true).await;
        }
    }
    wait_same_crew_key(&[&a, &b, &c], space).await;
    connect_both(&b, &c).await;
    testkit::authenticate(&b.node, c.peer())
        .await
        .expect("B·C 인증");
    let (da, db, dc) = (dev(&a), dev(&b), dev(&c));

    // 휘발성 채널(입장·퇴장 방식, 과거 기록 비공개).
    let mut p = channel_params(space, "비밀 수다", ChannelKind::Text);
    p.security = Some(SecurityLevel::Volatile);
    p.join_mode = Some(JoinMode::JoinLeave);
    p.history = Some(HistoryVisibility::Private);
    let vch = a.ok::<m::ChannelCreate>(p).await.channel;
    for n in [&b, &c] {
        wait_channel_known(n, space, vch).await;
    }
    for n in [&a, &b, &c] {
        n.ok::<m::ChannelJoin>(SpaceChannelParams {
            space,
            channel: vch,
        })
        .await;
    }
    let g1 = volatile_converged(&[&a, &b, &c], space, vch, &[]).await;
    let vmark = marker("휘발성비밀");
    a.send(space, vch, &vmark).await;
    for n in [&b, &c] {
        sees_text(n, space, vch, &vmark).await;
    }

    // 통화 채널, 세 사람이 같은 프레임 키.
    let voice = a
        .ok::<m::ChannelCreate>(channel_params(space, "음성", ChannelKind::Voice))
        .await
        .channel;
    for n in [&b, &c] {
        wait_channel_known(n, space, voice).await;
    }
    for n in [&a, &b, &c] {
        n.ok::<m::VoiceJoin>(SpaceChannelParams {
            space,
            channel: voice,
        })
        .await;
    }
    let (vg1, vfp1) = wait_call_agrees(
        &[&a, &b, &c],
        space,
        voice,
        &[da, db, dc],
        0,
        "세 기기가 같은 프레임 키",
    )
    .await;

    // 자동화 토큰(관리 범위)의 추방은 실행되지 않고 확인을 기다립니다.
    let r = a
        .ok::<m::TokenCreate>(TokenCreateParams {
            name: "정리 봇".into(),
            scopes: vec![Scope::Read, Scope::Write, Scope::Admin],
            spaces: vec![space],
            automation: Some(true),
        })
        .await;
    let bot = CallCtx::from_auth(
        ApiHandler::new(a.node.clone())
            .authenticate(r.token.expose())
            .await
            .expect("봇 토큰 인증"),
    );
    let mut rx = a.subscribe();
    let err = call_as::<m::MemberKick>(
        &a,
        &bot,
        MemberKickParams {
            space,
            user: c.user(),
            reason: Some("시험".into()),
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
    assert_eq!(pending.space, Some(space));
    // 아직 실행되지 않음.
    assert!(
        testkit::space_state(&a.node, &space)
            .unwrap()
            .is_member(&c.user())
    );
    // 봇은 스스로 승인하지 못함.
    assert!(
        call_as::<m::ConfirmApprove>(&a, &bot, IdParams { id: cid.clone() })
            .await
            .is_err()
    );
    a.ok::<m::ConfirmApprove>(IdParams { id: cid }).await;
    for n in [&a, &b] {
        wait_member(n, space, c.user(), false).await;
    }
    wait_same_crew_key(&[&a, &b], space).await;

    // 통화: C는 빠지고 남은 둘은 새 세대의 다른 키.
    let (vg2, vfp2) = wait_call_agrees(
        &[&a, &b],
        space,
        voice,
        &[da, db],
        vg1,
        "추방 뒤 두 기기의 새 프레임 키",
    )
    .await;
    assert!(vg2 > vg1);
    assert_ne!(vfp2, vfp1);
    eventually("C가 통화에서 빠짐", || async {
        let v = tnet::voice_call(&c.node, &space, &voice);
        v.is_none_or(|v| !v.joined || v.key_fingerprint != Some(vfp2))
            .then_some(())
    })
    .await;
    // C가 추방을 받아 적용할 때까지(부하가 크면 늦음) 기다린 뒤 다시 들어와 봅니다.
    eventually("C가 추방을 앎", || async {
        testkit::space_state(&c.node, &space)
            .is_none_or(|st| !st.is_member(&c.user()))
            .then_some(())
    })
    .await;
    assert!(
        c.call::<m::VoiceJoin>(SpaceChannelParams {
            space,
            channel: voice
        })
        .await
        .is_err(),
        "추방된 기기가 다시 통화에 들어옴"
    );

    // 휘발성: 남은 둘은 C가 모르는 새 세대로 넘어감.
    let g2 = volatile_converged(&[&a, &b], space, vch, &[g1]).await;
    assert!(
        !vt::volatile_generations(&c.node, &space, &vch).contains(&g2),
        "추방된 C가 새 세션 키를 가짐"
    );
    let after = marker("추방뒤휘발성");
    a.send(space, vch, &after).await;
    sees_text(&b, space, vch, &after).await;
    let c_sees = c
        .call::<m::MessageList>(list_params(space, vch))
        .await
        .map(|r| r.messages.iter().any(|v| v.text == after))
        .unwrap_or(false);
    assert!(!c_sees, "추방된 C가 새 휘발성 메시지를 읽음");

    // 일반 채널: 추방 뒤 메시지는 받아도 못 엶.
    let later = a.send(space, general, "추방 뒤 공지").await;
    let evs = testkit::stream_events(&a.node, &space, StreamId::Channel(general)).unwrap();
    let _ = testkit::ingest_from(&c.node, space, evs, a.peer()).await;
    assert!(testkit::message_text(&c.node, &later).is_none());

    // 감사 로그: 봇의 확인 대기와 승인으로 실행된 추방. 내용 없음.
    let audit = a.ok::<m::AuditList>(AuditListParams::default()).await;
    assert!(audit.iter().any(|e| e.method == "member.kick"
        && e.token_name == "정리 봇"
        && e.code == Some(ERR_CONFIRMATION_REQUIRED)));
    assert!(
        audit
            .iter()
            .any(|e| e.method == "member.kick" && e.token_name == "정리 봇" && e.ok)
    );

    // 누출 검사: 휘발성 내용은 어느 노드에도 없고, 일반 채널 표식은 DB에서 찾힘(검사가 실제로 봄).
    let normal = marker("일반표식");
    a.send(space, general, &normal).await;
    sees_text(&b, space, general, &normal).await;
    for n in [&a, &b, &c] {
        for mk in [&vmark, &after] {
            let r = leakcheck(n, mk).await;
            assert!(r.clean, "{}에 휘발성 내용이 남음: {:?}", n.name, r.findings);
        }
    }
    let r = leakcheck(&b, &normal).await;
    assert!(!r.clean, "일반 채널 표식을 찾지 못함");
    assert!(r.findings.iter().all(|f| f.starts_with("db:")), "{r:?}");
}

// ───────────── 3. 연결한 새 기기가 크루에 들어옴 ─────────────

/// A1이 승인해 들인 A2는 A1을 통해 크루를 받아들이고, 커밋 확정 기기가 A2를 공간 그룹과 비공개 채널
/// 그룹에 넣어 A2가 B의 새 비공개 메시지를 읽고 답할 수 있습니다. 예전 프로필은 그대로입니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn linked_device_joins_crew_with_private_channel() {
    let cl = Cluster::new();
    let a1 = cl.bare("a1").await;
    a1.ok::<m::IdentityCreate>(IdentityCreateParams {
        name: "양파".into(),
    })
    .await;
    let b = cl.node("b").await;
    let space = create_crew(&a1, "복구 크루").await;
    let general = a1.first_channel(space).await;
    join_by_invite(&a1, &b, space).await;
    wait_member(&a1, space, b.user(), true).await;
    wait_same_crew_key(&[&a1, &b], space).await;
    let role = create_role(&a1, space, "비밀").await;
    a1.ok::<m::MemberSetRoles>(MemberSetRolesParams {
        space,
        user: b.user(),
        roles: vec![role],
    })
    .await;
    let mut p = channel_params(space, "비밀방", ChannelKind::Text);
    p.private_roles = Some(vec![role]);
    let secret = a1.ok::<m::ChannelCreate>(p).await.channel;
    eventually("B가 비공개 채널 그룹에 들어감", || async {
        testkit::channel_key_seq(&b.node, &space, secret)
    })
    .await;
    b.send(space, secret, "복구 전 비밀").await;
    sees_text(&a1, space, secret, "복구 전 비밀").await;

    let a2 = common::linked(&cl, &a1, "a2", |_| {}).await;
    assert_eq!(a2.user(), a1.user());
    let a2_dev = dev(&a2);
    wait_own_device(&a1, a2_dev).await;
    eventually("B가 A의 새 기기를 앎", || async {
        testkit::device_list(&b.node, &a1.user())?
            .contains_active(&a2_dev)
            .then_some(())
    })
    .await;
    eventually("A2가 공간 그룹에 들어감", || async {
        let members = testkit::crew_key_members(&a1.node, &space)?;
        let e2 = testkit::crew_key_seq(&a2.node, &space)?;
        let e1 = testkit::crew_key_seq(&a1.node, &space)?;
        (members.contains(&a2_dev) && e1 == e2).then_some(())
    })
    .await;
    eventually("A2가 비공개 채널 그룹에 들어감", || async {
        let members = testkit::key_group_members(&a1.node, &space, Some(secret))?;
        let e2 = testkit::channel_key_seq(&a2.node, &space, secret)?;
        let e1 = testkit::channel_key_seq(&a1.node, &space, secret)?;
        (members.contains(&a2_dev) && e1 == e2).then_some(())
    })
    .await;
    eventually("A2의 프로필이 내 프로필", || async {
        let p = a2
            .call::<m::ProfileGet>(ProfileGetParams { user: None })
            .await
            .ok()?;
        (p.name == "양파").then_some(())
    })
    .await;

    // A2가 크루 키 묶음을 B의 가입보다 먼저 받았을 수 있어, A2의 상태가 B를 멤버로 알 때까지 기다린 뒤 인증합니다.
    wait_member(&a2, space, b.user(), true).await;
    connect_both(&a2, &b).await;
    eventually("A2·B 인증", || async {
        testkit::authenticate(&a2.node, b.peer()).await.ok()
    })
    .await;
    b.send(space, secret, "복구 뒤 비밀").await;
    sees_text(&a2, space, secret, "복구 뒤 비밀").await;
    b.send(space, general, "복구 뒤 공지").await;
    sees_text(&a2, space, general, "복구 뒤 공지").await;
    a2.send(space, secret, "새 기기에서 답함").await;
    sees_text(&b, space, secret, "새 기기에서 답함").await;
    sees_text(&a1, space, secret, "새 기기에서 답함").await;
}

// ───────────── 4. 크루 아이콘·이모지 블롭 ─────────────

/// 크루 아이콘(`crew.update`)과 커스텀 이모지(`emoji.create`)는 크루 공간에 등록된 블롭이고 키는 상태 이벤트에만
/// 있습니다. 다른 멤버는 블롭을 받아 같은 바이트로 풀고, 로컬 API JSON에는 키가 나오지 않습니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crew_icon_and_emoji_reach_members_as_blobs() {
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let b = cl.node("b").await;
    let space = create_crew(&a, "그림 크루").await;
    join_by_invite(&a, &b, space).await;
    wait_member(&a, space, b.user(), true).await;
    wait_same_crew_key(&[&a, &b], space).await;

    let icon_bytes = png(40, 40, 3);
    let icon_path = write_file(&a, "icon.png", &icon_bytes);
    a.ok::<m::CrewUpdate>(CrewUpdateParams {
        space,
        name: None,
        description: None,
        icon_path: Some(icon_path),
        strict_rotation: None,
        emoji_limits: None,
    })
    .await;
    let icon = eventually("B가 크루 아이콘을 봄", || async {
        b.ok::<m::SpaceGet>(SpaceParams { space })
            .await
            .settings
            .icon
    })
    .await;
    let json = serde_json::to_value(b.ok::<m::SpaceGet>(SpaceParams { space }).await).unwrap();
    assert!(
        json["settings"]["icon_key"].is_null(),
        "로컬 API에 아이콘 키가 보임: {json}"
    );
    b.ok::<m::BlobFetch>(BlobFetchParams {
        space,
        blob: icon,
        priority: None,
    })
    .await;
    wait_blob_complete(&b, space, icon, "B가 아이콘을 다 받음").await;
    // 아이콘·이모지는 `key_from`과 무관하게 상태에서 키를 찾습니다.
    let any = EventId([0; 32]);
    assert_eq!(read_blob(&b, space, icon, any).await, icon_bytes);

    let emoji_bytes = png(24, 24, 9);
    let emoji_path = write_file(&a, "emoji.png", &emoji_bytes);
    let created = a
        .ok::<m::EmojiCreate>(EmojiCreateParams {
            space,
            name: "양파".into(),
            path: emoji_path,
        })
        .await;
    let emoji_blob = eventually("B가 이모지를 봄", || async {
        b.ok::<m::EmojiList>(SpaceParams { space })
            .await
            .into_iter()
            .find(|e| e.id == created.id)
            .map(|e| e.blob)
    })
    .await;
    b.ok::<m::BlobFetch>(BlobFetchParams {
        space,
        blob: emoji_blob,
        priority: None,
    })
    .await;
    wait_blob_complete(&b, space, emoji_blob, "B가 이모지를 다 받음").await;
    assert_eq!(read_blob(&b, space, emoji_blob, any).await, emoji_bytes);

    // 크루 밖 노드는 같은 블롭을 요청해도 받지 못합니다.
    let x = cl.node("x").await;
    connect_both(&x, &a).await;
    let r = testkit::net_request(
        &x.node,
        a.peer(),
        YpRequest::BlobManifest { space, blob: icon },
    )
    .await
    .expect("요청 전송");
    assert!(matches!(r, YpResponse::Error { .. }), "{r:?}");
}

// ───────────── 5. 실행 중 망 설정 → 블롭·정책 ─────────────

fn preview_anywhere(cfg: &mut NodeConfig) {
    cfg.sharing.link_preview.allow_private_addrs = true;
    cfg.sharing.link_preview.allow_any_port = true;
}

/// [`preview_anywhere`]에 더해 Tor로 띄웁니다.
fn preview_anywhere_tor(cfg: &mut NodeConfig) {
    preview_anywhere(cfg);
    cfg.net.direct = false;
}

/// 127.0.0.1의 작은 HTTP 서버. 들어온 요청 수를 세고 늘 같은 페이지를 줍니다.
async fn counting_page_server(hits: Arc<AtomicUsize>) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = listener.accept().await {
            let hits = hits.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 1024];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    match s.read(&mut tmp).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buf.extend_from_slice(&tmp[..n]),
                    }
                }
                hits.fetch_add(1, Ordering::SeqCst);
                let body = "<!doctype html><html><head><title>양파 링크</title></head><body></body></html>";
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = s.write_all(head.as_bytes()).await;
                let _ = s.write_all(body.as_bytes()).await;
                let _ = s.shutdown().await;
            });
        }
    });
    port
}

async fn embeds_of(n: &TestNode, space: SpaceId, channel: ChannelId, id: EventId) -> usize {
    n.ok::<m::MessageList>(list_params(space, channel))
        .await
        .messages
        .into_iter()
        .find(|v| v.id == id)
        .expect("보낸 메시지")
        .embeds
        .len()
}

/// 망 설정이 블롭·정책 영역에도 따라갑니다. 예전에는 시작 옵션만 봐서, 실행 중에 켠 Tor 모드에서 링크 사이트에
/// 직접 접속해 IP가 드러났고 대역폭도 일반 기본값이었습니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn runtime_network_settings_reach_sharing_policy() {
    let cl = Cluster::new();

    // 링크 미리보기: 직접 연결 스웜에서는 (시험용으로 허용한) 로컬 사이트를 조회해 만듭니다.
    let mut t = cl.node_with("t", preview_anywhere).await;
    let hits = Arc::new(AtomicUsize::new(0));
    let port = counting_page_server(hits.clone()).await;
    let link = format!("http://127.0.0.1:{port}/page");
    let crew = create_crew(&t, "혼자 크루").await;
    let general = t.first_channel(crew).await;
    let normal = t.send(crew, general, &format!("일반 {link}")).await;
    assert_eq!(embeds_of(&t, crew, general, normal).await, 1);
    assert!(hits.load(Ordering::SeqCst) >= 1);
    let seed = t.ok::<m::SeedGet>(OptSpaceParams { space: None }).await;
    let peer = t.ok::<m::PeerGet>(OptSpaceParams { space: None }).await;
    assert_eq!(seed.upload_limit_bps, Some(10 * 1024 * 1024));
    assert_eq!(peer.download_limit_bps, Some(10 * 1024 * 1024));

    // Tor로 다시 띄우면 직접 조회하지 않고, 저장된 설정이 없으면 Tor 기본값입니다(대역폭은 같고 동시 전송만 줄임).
    t.restart_with(preview_anywhere_tor).await;
    eventually("onion 주소", || async {
        t.ok::<m::NetStatusGet>(Empty).await.onion_addr.map(|_| ())
    })
    .await;
    let before = hits.load(Ordering::SeqCst);
    let hidden = t.send(crew, general, &format!("Tor {link}")).await;
    assert_eq!(embeds_of(&t, crew, general, hidden).await, 0);
    assert_eq!(
        hits.load(Ordering::SeqCst),
        before,
        "Tor 모드에서 링크 사이트에 직접 접속함"
    );
    let seed = t.ok::<m::SeedGet>(OptSpaceParams { space: None }).await;
    let peer = t.ok::<m::PeerGet>(OptSpaceParams { space: None }).await;
    assert_eq!(seed.upload_limit_bps, Some(10 * 1024 * 1024));
    assert_eq!(seed.max_concurrent, 2);
    assert_eq!(peer.download_limit_bps, Some(10 * 1024 * 1024));
    assert_eq!(peer.auto_download_max_bytes, 2 * 1024 * 1024);
    // 블롭은 하나씩, 조각 요청은 피어당 셋까지 겹쳐 Tor 왕복을 겹칩니다.
    assert_eq!(testkit::tor_download_limits(&t.node), Some((1, 3, 4)));
}

// ───────────── 6. 폐기한 기기가 통화·휘발성·그룹에서 빠짐 ─────────────

/// A1·A2(같은 사람의 두 기기)와 B가 휘발성 채널과 통화에 함께 있다가 A1이 A2를 폐기합니다. B는 A의 새 기기 목록을
/// 받아 A2를 더 믿지 않고, 통화는 A1·B만의 새 프레임 키로, 휘발성 채널은 A2가 모르는 새 세대로 넘어가며, A2는 공간
/// 그룹에서 빠져 그 뒤 메시지를 열지 못하고 첨부도 받지 못합니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn revoked_device_leaves_call_volatile_and_group() {
    let cl = Cluster::new();
    let a1 = cl.bare("a1").await;
    a1.ok::<m::IdentityCreate>(IdentityCreateParams {
        name: "양파".into(),
    })
    .await;
    let b = cl.node("b").await;
    let space = create_crew(&a1, "폐기 크루").await;
    let general = a1.first_channel(space).await;
    join_by_invite(&a1, &b, space).await;
    wait_member(&a1, space, b.user(), true).await;
    wait_same_crew_key(&[&a1, &b], space).await;

    let a2 = common::linked(&cl, &a1, "a2", |_| {}).await;
    let (d1, d2, db) = (dev(&a1), dev(&a2), dev(&b));
    wait_own_device(&a1, d2).await;
    eventually("A2가 공간 그룹에 들어감", || async {
        let members = testkit::crew_key_members(&a1.node, &space)?;
        (members.contains(&d2)
            && testkit::crew_key_seq(&a2.node, &space)? == testkit::crew_key_seq(&a1.node, &space)?)
        .then_some(())
    })
    .await;
    eventually("B가 A2를 앎", || async {
        testkit::device_list(&b.node, &a1.user())?
            .contains_active(&d2)
            .then_some(())
    })
    .await;
    // A2는 가입 키 묶음을 공간 이벤트 동기화보다 먼저 받을 수 있습니다. B의 가입을 보기 전에는 B를 믿지 않습니다.
    wait_member(&a2, space, b.user(), true).await;
    connect_both(&a2, &b).await;
    // B의 크루 상태가 A2를 담기 전이면 B가 아직 받지 않으므로 될 때까지.
    eventually("A2·B 인증", || async {
        testkit::authenticate(&a2.node, b.peer()).await.ok()
    })
    .await;
    wait_same_crew_key(&[&a1, &a2, &b], space).await;

    // 휘발성 채널과 통화에 세 기기가 함께 있음.
    let mut p = channel_params(space, "휘발 수다", ChannelKind::Text);
    p.security = Some(SecurityLevel::Volatile);
    p.join_mode = Some(JoinMode::JoinLeave);
    let vch = a1.ok::<m::ChannelCreate>(p).await.channel;
    let voice = a1
        .ok::<m::ChannelCreate>(channel_params(space, "음성", ChannelKind::Voice))
        .await
        .channel;
    for n in [&a2, &b] {
        wait_channel_known(n, space, vch).await;
        wait_channel_known(n, space, voice).await;
    }
    for n in [&a1, &a2, &b] {
        n.ok::<m::ChannelJoin>(SpaceChannelParams {
            space,
            channel: vch,
        })
        .await;
        n.ok::<m::VoiceJoin>(SpaceChannelParams {
            space,
            channel: voice,
        })
        .await;
    }
    let g1 = volatile_converged(&[&a1, &a2, &b], space, vch, &[]).await;
    let (vg1, vfp1) = wait_call_agrees(
        &[&a1, &a2, &b],
        space,
        voice,
        &[d1, d2, db],
        0,
        "세 기기가 같은 프레임 키",
    )
    .await;
    let before = marker("폐기전휘발성");
    b.send(space, vch, &before).await;
    sees_text(&a2, space, vch, &before).await;

    // A1이 A2를 폐기. 사람(소유자 토큰)이 부르면 바로 실행됩니다.
    a1.ok::<m::DeviceRevoke>(DeviceRevokeParams { device: d2 })
        .await;
    eventually("B가 A2의 폐기를 앎", || async {
        let l = testkit::device_list(&b.node, &a1.user())?;
        (!l.contains_active(&d2)).then_some(())
    })
    .await;

    let (vg2, vfp2) = wait_call_agrees(
        &[&a1, &b],
        space,
        voice,
        &[d1, db],
        vg1,
        "폐기 뒤 두 기기의 새 프레임 키",
    )
    .await;
    assert!(vg2 > vg1);
    assert_ne!(vfp2, vfp1);
    let g2 = volatile_converged(&[&a1, &b], space, vch, &[g1]).await;
    assert!(
        !vt::volatile_generations(&a2.node, &space, &vch).contains(&g2),
        "폐기된 A2가 새 세션 키를 가짐"
    );
    eventually("A2가 공간 그룹에서 빠짐", || async {
        let members = testkit::crew_key_members(&a1.node, &space)?;
        (!members.contains(&d2)).then_some(())
    })
    .await;
    wait_same_crew_key(&[&a1, &b], space).await;

    // 폐기 뒤 메시지·첨부: A2에게 그대로 넘겨도 열지 못하고, B에게 직접 블롭을 요청해도 받지 못함.
    let after = marker("폐기뒤휘발성");
    b.send(space, vch, &after).await;
    sees_text(&a1, space, vch, &after).await;
    let a2_sees = a2
        .call::<m::MessageList>(list_params(space, vch))
        .await
        .map(|r| r.messages.iter().any(|v| v.text == after))
        .unwrap_or(false);
    assert!(!a2_sees, "폐기된 A2가 새 휘발성 메시지를 읽음");

    let file: Vec<u8> = common::noise(40_000, 13);
    let path = write_file(&b, "폐기 뒤.bin", &file);
    let later = b
        .ok::<m::MessageSend>(MessageSendParams {
            space,
            channel: general,
            text: "폐기 뒤 공지".into(),
            attachments: vec![AttachmentInput {
                path,
                ..Default::default()
            }],
            reply_to: None,
            thread: None,
        })
        .await
        .id;
    sees_text(&a1, space, general, "폐기 뒤 공지").await;
    let blob = b
        .ok::<m::MessageList>(list_params(space, general))
        .await
        .messages
        .into_iter()
        .find(|v| v.id == later)
        .and_then(|v| v.attachments.first().map(|at| at.blob))
        .expect("첨부");
    let evs = testkit::stream_events(&b.node, &space, StreamId::Channel(general)).unwrap();
    let _ = testkit::ingest_from(&a2.node, space, evs, b.peer()).await;
    assert!(
        testkit::message_text(&a2.node, &later).is_none(),
        "폐기된 A2가 새 메시지를 엶"
    );
    let r = testkit::net_request(&a2.node, b.peer(), YpRequest::BlobManifest { space, blob }).await;
    assert!(
        !matches!(r, Ok(YpResponse::Manifest(_))),
        "폐기된 A2가 첨부 매니페스트를 받음: {r:?}"
    );
    for n in [&a1, &a2, &b] {
        for mk in [&before, &after] {
            let r = leakcheck(n, mk).await;
            assert!(r.clean, "{}에 휘발성 내용이 남음: {:?}", n.name, r.findings);
        }
    }
}

// ───────────── 7~9. 휘발성 첨부·저장 공간 정리·보관 노드가 만나는 곳 ─────────────

/// 동기화·보관 주기 작업을 짧게(보관 노드 시나리오).
fn fast(cfg: &mut NodeConfig) {
    cfg.sync_interval = std::time::Duration::from_millis(300);
}

/// `yangpad --archive`로 띄운 노드.
fn archive_host(cfg: &mut NodeConfig) {
    fast(cfg);
    cfg.archive.enabled = true;
}

/// 결정적 바이트에 표식을 처음·가운데·끝에 넣습니다(조각 경계를 넘는 크기).
fn marked_bytes(len: usize, seed: u8, marker: &str) -> Vec<u8> {
    let mut v: Vec<u8> = common::noise(len, seed);
    let mk = marker.as_bytes();
    for at in [0, len / 2, len - mk.len()] {
        v[at..at + mk.len()].copy_from_slice(mk);
    }
    v
}

/// 노드 데이터 경로 밖(시험 묶음의 `src`)에 원본을 씁니다. 원본이 데이터 경로 검사에 걸리지 않게.
fn write_source(cl: &Cluster, name: &str, bytes: &[u8]) -> String {
    let dir = cl.dir("src");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, bytes).unwrap();
    path.to_string_lossy().into_owned()
}

/// 글과 첨부 하나를 보냅니다. (메시지, 블롭).
async fn send_attachment(
    n: &TestNode,
    space: SpaceId,
    channel: ChannelId,
    text: &str,
    path: String,
) -> (EventId, BlobId) {
    let id = n
        .ok::<m::MessageSend>(MessageSendParams {
            space,
            channel,
            text: text.into(),
            attachments: vec![AttachmentInput {
                path,
                ..Default::default()
            }],
            reply_to: None,
            thread: None,
        })
        .await
        .id;
    let view = wait_message(n, space, channel, id, "보낸 첨부 메시지", |v| {
        v.attachments.len() == 1
    })
    .await;
    (id, view.attachments[0].blob)
}

/// `root` 아래 파일 중 `needle`을 담은 것(데이터 경로 전체 검사).
fn files_containing(root: &std::path::Path, needle: &[u8], out: &mut Vec<std::path::PathBuf>) {
    let Ok(rd) = std::fs::read_dir(root) else {
        return;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            files_containing(&path, needle, out);
        } else if ft.is_file()
            && let Ok(data) = std::fs::read(&path)
            && data.windows(needle.len()).any(|w| w == needle)
        {
            out.push(path);
        }
    }
}

/// 표식이 그 노드의 저장소(복호화한 DB·블롭, `diag.leakcheck`)와 데이터 경로 파일 어디에도 없는지.
async fn assert_no_trace(n: &TestNode, mark: &str, when: &str) {
    let r = leakcheck(n, mark).await;
    assert!(
        r.clean && r.findings.is_empty(),
        "{when}: {} 저장소에 표식: {:?}",
        n.name,
        r.findings
    );
    let mut hits = Vec::new();
    files_containing(&n.dir, mark.as_bytes(), &mut hits);
    assert!(hits.is_empty(), "{when}: {} 데이터 경로: {hits:?}", n.name);
}

/// 디스크에 있는 이 블롭의 조각 바이트(기록이 없거나 비웠으면 0).
fn disk_bytes(n: &TestNode, blob: &BlobId) -> u64 {
    testkit::blobs::stored_bytes(&n.node, blob).unwrap_or(0)
}

/// 크루에 휘발성 채널(입장·퇴장 방식)을 만들고 모두 들어가 같은 세션 키 세대가 될 때까지.
async fn volatile_channel_joined(
    owner: &TestNode,
    others: &[&TestNode],
    space: SpaceId,
) -> ChannelId {
    let mut p = channel_params(space, "휘발성 자료", ChannelKind::Text);
    p.security = Some(SecurityLevel::Volatile);
    p.join_mode = Some(JoinMode::JoinLeave);
    let vch = owner.ok::<m::ChannelCreate>(p).await.channel;
    for n in others {
        wait_channel_known(n, space, vch).await;
    }
    let mut all = vec![owner];
    all.extend_from_slice(others);
    for n in &all {
        n.ok::<m::ChannelJoin>(SpaceChannelParams {
            space,
            channel: vch,
        })
        .await;
    }
    volatile_converged(&all, space, vch, &[]).await;
    vch
}

/// 보관 노드가 제안 링크를 만들고 관리자가 지정합니다. 보관 노드가 공간을 받아 둘 때까지 기다립니다.
async fn designate_archive(admin: &TestNode, host: &TestNode, space: SpaceId) {
    connect(admin, host).await;
    let url = host
        .ok::<m::ArchiveOffer>(ArchiveOfferParams {
            expires_in_secs: Some(600),
            name: Some("보관함".into()),
        })
        .await
        .url;
    admin
        .ok::<m::ArchiveDesignate>(ArchiveDesignateParams {
            space,
            url,
            private_channels: Vec::new(),
            name: None,
        })
        .await;
    eventually("보관 노드가 공간을 받아 둠", || async {
        at::is_hosted(&host.node, &space).then_some(())
    })
    .await;
}

/// B가 일반 첨부(디스크)와 휘발성 첨부(RAM)를 함께 받은 뒤, 한도를 0으로 줄이고 디스크 여유가 없다고 보고 정리
/// 대상을 모두 비워도 디스크 첨부만 비워집니다. 휘발성 첨부는 RAM에 그대로 남아 바이트 그대로 읽히고, 저장 공간
/// 집계·고정(`blob.pin`은 `1004`)에 잡히지 않으며, 그 사이 어느 노드도 휘발성 내용을 디스크에 쓰지 않습니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn volatile_attachments_stay_out_of_disk_cache() {
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let b = cl.node("b").await;
    let space = create_crew(&a, "정리 크루").await;
    let general = a.first_channel(space).await;
    join_by_invite(&a, &b, space).await;
    for n in [&a, &b] {
        wait_member(n, space, b.user(), true).await;
    }
    wait_same_crew_key(&[&a, &b], space).await;
    let vch = volatile_channel_joined(&a, &[&b], space).await;

    // 일반 첨부: B가 디스크에 다 받음.
    let plain = marked_bytes(70_000, 3, &marker("일반첨부"));
    let (pmsg, pblob) = send_attachment(
        &a,
        space,
        general,
        "일반 첨부",
        write_source(&cl, "일반.bin", &plain),
    )
    .await;
    wait_message(
        &b,
        space,
        general,
        pmsg,
        "B가 일반 첨부 메시지를 받음",
        |_| true,
    )
    .await;
    b.ok::<m::BlobFetch>(BlobFetchParams {
        space,
        blob: pblob,
        priority: None,
    })
    .await;
    wait_blob_complete(&b, space, pblob, "B가 일반 첨부를 다 받음").await;

    // 휘발성 첨부: B가 RAM에 다 받음.
    let vmark = marker("휘발성첨부");
    let secret = marked_bytes(90_000, 7, &vmark);
    let (vmsg, vblob) = send_attachment(
        &a,
        space,
        vch,
        "휘발성 첨부",
        write_source(&cl, "휘발성.bin", &secret),
    )
    .await;
    let seen = wait_message(
        &b,
        space,
        vch,
        vmsg,
        "B가 휘발성 첨부 메시지를 받음",
        |v| v.attachments.len() == 1,
    )
    .await;
    assert!(seen.volatile);
    b.ok::<m::BlobFetch>(BlobFetchParams {
        space,
        blob: vblob,
        priority: None,
    })
    .await;
    wait_blob_complete(&b, space, vblob, "B가 휘발성 첨부를 다 받음").await;
    assert_eq!(read_blob(&b, space, vblob, vmsg).await, secret);

    // 휘발성 첨부는 디스크 저장소에 기록이 없고, 고정·정리 집계에 들지 않습니다.
    assert_eq!(testkit::blobs::stored_bytes(&b.node, &vblob), None);
    let st = b
        .ok::<m::BlobStatus>(BlobParams { space, blob: vblob })
        .await;
    assert!(!st.pinned && !st.kept);
    assert_eq!(
        code(
            b.call::<m::BlobPin>(BlobParams { space, blob: vblob })
                .await
        ),
        ERR_NOT_FOUND
    );
    let v = b.ok::<m::BlobCacheStatus>(Empty).await;
    assert_eq!(v.cache_count, 1, "정리 대상은 일반 첨부 하나뿐: {v:?}");
    assert_eq!(v.cache_bytes, disk_bytes(&b, &pblob));
    assert_eq!(v.pinned_count, 0);
    let ram = vt::volatile_stats(&b.node);
    assert!(ram.blobs >= 1 && ram.blob_bytes > 0);

    // 한도 0 + 디스크 여유 없음 + 비우기. 30초 내줌 보류에 걸리지 않게 내준 기록은 지웁니다.
    testkit::blobs::hold_serving(&b.node, &pblob, false);
    b.ok::<m::BlobCacheSetLimit>(BlobCacheLimitParams {
        limit_bytes: Some(0),
        ..Default::default()
    })
    .await;
    testkit::blobs::set_disk_free(&b.node, Some(0));
    testkit::blobs::enforce_cache_now(&b.node);
    b.ok::<m::BlobCacheClear>(Empty).await;
    testkit::blobs::set_disk_free(&b.node, None);
    assert_eq!(disk_bytes(&b, &pblob), 0, "일반 첨부는 비워져야 함");
    let after = b
        .ok::<m::BlobStatus>(BlobParams { space, blob: pblob })
        .await;
    assert_eq!(after.have_chunks, 0);

    // 휘발성 첨부는 그대로입니다(같은 RAM 사용량, 바이트 그대로 읽힘).
    let ram_after = vt::volatile_stats(&b.node);
    assert_eq!(ram_after.blobs, ram.blobs);
    assert_eq!(ram_after.blob_bytes, ram.blob_bytes);
    let st = b
        .ok::<m::BlobStatus>(BlobParams { space, blob: vblob })
        .await;
    assert!(st.total_chunks > 0 && st.have_chunks == st.total_chunks);
    assert_eq!(read_blob(&b, space, vblob, vmsg).await, secret);
    // A(보낸 쪽)에서 정리를 돌려도 같습니다.
    a.ok::<m::BlobCacheClear>(Empty).await;
    assert_eq!(read_blob(&a, space, vblob, vmsg).await, secret);

    for n in [&a, &b] {
        assert_no_trace(n, &vmark, "정리 뒤").await;
    }
    // 대조: 일반 첨부는 다시 받을 수 있습니다(비우기는 등록·매니페스트를 남김).
    b.ok::<m::BlobCacheSetLimit>(BlobCacheLimitParams {
        reset: true,
        ..Default::default()
    })
    .await;
    b.ok::<m::BlobFetch>(BlobFetchParams {
        space,
        blob: pblob,
        priority: None,
    })
    .await;
    wait_blob_complete(&b, space, pblob, "B가 비운 일반 첨부를 다시 받음").await;
    assert_eq!(read_blob(&b, space, pblob, pmsg).await, plain);
}

/// 보관 노드가 지정된 크루에서 휘발성 채널에 글·첨부를 주고받고, 그 뒤 보낸 일반 메시지가 보관 노드에 닿을 때까지
/// 기다려도 보관 노드의 명단·저장소·파일 어디에도 휘발성 채널·첨부·표식이 없습니다. 보관 노드가 휘발성 첨부를 직접
/// 달라고 해도 멤버는 주지 않습니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn archive_never_receives_volatile_data() {
    let cl = Cluster::new();
    let a = cl.node_with("a", fast).await;
    let b = cl.node_with("b", fast).await;
    let r = cl.node_with("r", archive_host).await;
    let space = create_crew(&a, "보관 휘발성 크루").await;
    let general = a.first_channel(space).await;
    join_by_invite(&a, &b, space).await;
    for n in [&a, &b] {
        wait_member(n, space, b.user(), true).await;
    }
    wait_same_crew_key(&[&a, &b], space).await;
    designate_archive(&a, &r, space).await;
    connect_both(&b, &r).await;
    let first = a.send(space, general, "지정 뒤 첫 메시지").await;
    eventually("보관 노드가 첫 메시지를 받아 둠", || async {
        (at::stored(&r.node, &first) == Some(true)).then_some(())
    })
    .await;

    let vch = volatile_channel_joined(&a, &[&b], space).await;
    let vmark = marker("보관휘발성");
    let text = format!("{vmark} 휘발성 글");
    let secret = marked_bytes(60_000, 11, &vmark);
    let (vmsg, vblob) = send_attachment(
        &a,
        space,
        vch,
        &text,
        write_source(&cl, "휘발성.bin", &secret),
    )
    .await;
    sees_text(&b, space, vch, &text).await;
    b.ok::<m::BlobFetch>(BlobFetchParams {
        space,
        blob: vblob,
        priority: None,
    })
    .await;
    wait_blob_complete(&b, space, vblob, "B가 휘발성 첨부를 다 받음").await;
    assert_eq!(read_blob(&b, space, vblob, vmsg).await, secret);

    // 보관 노드가 휘발성 첨부를 직접 달라고 해도 두 멤버 모두 주지 않습니다.
    for holder in [&a, &b] {
        for req in [
            YpRequest::BlobManifest { space, blob: vblob },
            YpRequest::BlobChunk {
                space,
                blob: vblob,
                index: 0,
            },
        ] {
            let resp = testkit::net_request(&r.node, holder.peer(), req)
                .await
                .expect("요청 전송");
            assert!(
                matches!(resp, YpResponse::Error { .. }),
                "{}가 보관 노드에 휘발성 첨부를 줌",
                holder.name
            );
        }
    }

    // 휘발성 뒤에 보낸 일반 메시지와 첨부가 보관 노드에 닿을 때까지(그 사이 보낼 것은 다 보냈음).
    let (after, ablob) = send_attachment(
        &a,
        space,
        general,
        "휘발성 뒤 일반 첨부",
        write_source(&cl, "일반.bin", &marked_bytes(30_000, 5, "일반")),
    )
    .await;
    eventually(
        "보관 노드가 뒤의 일반 메시지를 받아 둠",
        || async { (at::stored(&r.node, &after) == Some(true)).then_some(()) },
    )
    .await;
    eventually(
        "보관 노드가 뒤의 일반 첨부를 받아 둠",
        || async { at::blob_complete(&r.node, &ablob).then_some(()) },
    )
    .await;

    let chans = at::roster_channels(&r.node, &space).expect("보관 명단");
    assert!(!chans.contains(&vch), "휘발성 채널이 보관 명단에 들어감");
    assert_eq!(at::stored(&r.node, &vmsg), None, "휘발성 메시지가 보관됨");
    assert_eq!(testkit::blobs::stored_bytes(&r.node, &vblob), None);
    assert!(!at::blob_complete(&r.node, &vblob));
    assert_no_trace(&r, &vmark, "보관 노드").await;
    // 멤버 쪽 디스크에도 없습니다(보관 노드에 보낼 것을 모으는 경로 포함).
    for n in [&a, &b] {
        assert_no_trace(n, &vmark, "멤버").await;
    }
}

/// 보관 노드가 받아 둔 첨부는 보관 노드의 한도 0·디스크 여유 없음·비우기에도 그대로 남고(늘 남기는 것으로 셈),
/// 멤버 B가 정리로 비운 첨부는 올린 A가 꺼져 있어도 보관 노드에서 다시 받아 바이트 그대로 풉니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn archive_blobs_survive_eviction_and_refill_members() {
    let cl = Cluster::new();
    let a = cl.node_with("a", fast).await;
    let b = cl.node_with("b", fast).await;
    let r = cl.node_with("r", archive_host).await;
    let space = create_crew(&a, "보관 정리 크루").await;
    let general = a.first_channel(space).await;
    join_by_invite(&a, &b, space).await;
    for n in [&a, &b] {
        wait_member(n, space, b.user(), true).await;
    }
    wait_same_crew_key(&[&a, &b], space).await;
    designate_archive(&a, &r, space).await;
    connect_both(&b, &r).await;

    let bytes = marked_bytes(80_000, 9, "보관첨부");
    let (msg, blob) = send_attachment(
        &a,
        space,
        general,
        "보관할 첨부",
        write_source(&cl, "보관.bin", &bytes),
    )
    .await;
    eventually("보관 노드가 첨부를 다 받음", || async {
        at::blob_complete(&r.node, &blob).then_some(())
    })
    .await;
    wait_message(&b, space, general, msg, "B가 첨부 메시지를 받음", |_| true).await;
    b.ok::<m::BlobFetch>(BlobFetchParams {
        space,
        blob,
        priority: None,
    })
    .await;
    wait_blob_complete(&b, space, blob, "B가 첨부를 다 받음").await;

    // 보관 노드: 받아 둔 첨부는 정리 대상이 아닙니다.
    let held = disk_bytes(&r, &blob);
    assert!(held > 0);
    testkit::blobs::hold_serving(&r.node, &blob, false);
    r.ok::<m::BlobCacheSetLimit>(BlobCacheLimitParams {
        limit_bytes: Some(0),
        ..Default::default()
    })
    .await;
    testkit::blobs::set_disk_free(&r.node, Some(0));
    assert_eq!(testkit::blobs::enforce_cache_now(&r.node), (0, 0));
    let cleared = r.ok::<m::BlobCacheClear>(Empty).await;
    testkit::blobs::set_disk_free(&r.node, None);
    assert_eq!(cleared.evicted_count, 0);
    assert!(at::blob_complete(&r.node, &blob));
    assert_eq!(disk_bytes(&r, &blob), held);
    let v = r.ok::<m::BlobCacheStatus>(Empty).await;
    assert_eq!(v.cache_count, 0, "보관한 첨부가 정리 대상으로 잡힘: {v:?}");
    assert!(v.kept_count >= 1 && v.kept_bytes >= held);

    // 멤버 B: 비우기로 원본을 비웁니다(보관 노드에 내준 기록이 있으면 보류되므로 지움).
    testkit::blobs::hold_serving(&b.node, &blob, false);
    let r_b = b.ok::<m::BlobCacheClear>(Empty).await;
    assert!(r_b.evicted_count >= 1);
    assert_eq!(disk_bytes(&b, &blob), 0);

    // 올린 A가 꺼지면 B는 보관 노드에서만 다시 받습니다.
    a.shutdown().await;
    b.ok::<m::BlobFetch>(BlobFetchParams {
        space,
        blob,
        priority: None,
    })
    .await;
    wait_blob_complete(&b, space, blob, "B가 보관 노드에서 첨부를 다시 받음").await;
    assert_eq!(read_blob(&b, space, blob, msg).await, bytes);
}
