//! 음성·화면공유 신호, 위치 기록(DHT), 들어온 연결의 주소 기록, Tor 설정(B5), 예전 빌드에서 Tor로 옮기기, 노드 카드.
//!
//! 실제 Tor 망이 필요한 시험은 `#[ignore]`이고, 나머지는 Tor 망 없이 설정·저장·직접 연결로 돌아가지 않음을 봅니다.

use crate::common;

use std::collections::BTreeSet;

use common::{
    Cluster, TestNode, befriend, call, connect, connect_both, eventually, listen_addrs,
    make_friends, meet_by_link, next_notification, wait_message,
};
use tokio::sync::broadcast;
use yangpa_core::ids::{ChannelId, DeviceId, SpaceId, UserId};
use yangpa_core::links::DeviceLink;
use yangpa_core::model::Permissions;
use yangpa_core::wire::{
    NodeContact, SignalPayload, SocialReply, SocialRequest, WireErrorCode, YpRequest, YpResponse,
};
use yangpa_daemon::testkit;
use yangpa_daemon::testkit::net as tnet;
use yangpa_daemon::{CallCtx, Node, NodeConfig};
use yangpa_net::{KadMode, Multiaddr, PeerId};
use yangpa_rpc::AuthInfo;
use yangpa_rpc::api::{
    ConnectionMode, ContactKind, CrewCreateParams, DeviceLinkDecideParams, DeviceLinkJoinParams,
    DeviceLinkOffer, DeviceLinkRequestView, DeviceLinkStatus, DeviceRevokeParams, Empty,
    FriendLinkCreateParams, InviteCreateParams, InviteJoinStatus, MemberBanParams, Method, NetMode,
    Notification, Scope, SpaceChannelParams, SpaceParams, StunServersParams, UrlParams, UserParams,
    VoiceSignalParams, VoiceStateParams, VoiceWatchParams, m,
};
use yangpa_rpc::jsonrpc::{
    ERR_CONFLICT, ERR_FORBIDDEN, ERR_INVALID_INPUT, ERR_NETWORK, ERR_NOT_FOUND,
};

fn dev(n: &TestNode) -> DeviceId {
    testkit::device_id(&n.node).expect("기기 ID")
}

fn token_ctx(scopes: Vec<Scope>, automation: bool) -> CallCtx {
    CallCtx::from_auth(AuthInfo {
        token_id: "t".into(),
        token_name: "시험 토큰".into(),
        scopes,
        spaces: Vec::new(),
        automation,
    })
}

/// 주어진 토큰 권한으로 API를 부릅니다.
async fn call_as<M: Method>(n: &TestNode, ctx: &CallCtx, p: M::Params) -> M::Result {
    let v = serde_json::to_value(&p).expect("매개변수");
    let out = n
        .node
        .call(ctx, M::NAME, v)
        .await
        .unwrap_or_else(|e| panic!("{} 실패: {e:?}", M::NAME));
    serde_json::from_value(out).expect("결과 형식")
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

async fn wait_channel(n: &TestNode, space: SpaceId, channel: ChannelId) {
    eventually("통화 채널 전파", || async {
        n.ok::<m::ChannelList>(SpaceParams { space })
            .await
            .iter()
            .any(|c| c.channel == channel)
            .then_some(())
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

fn signal(
    space: SpaceId,
    channel: ChannelId,
    call: yangpa_core::ids::CallId,
    to: DeviceId,
    payload: SignalPayload,
) -> VoiceSignalParams {
    VoiceSignalParams {
        space,
        channel,
        call,
        to,
        payload,
    }
}

/// 세 기기가 통화 채널에서 데몬을 거쳐 offer·answer·ICE를 주고받고, 상태·시청·퇴장·끊김이 참가자 목록과
/// 프레임 키 세대에 반영된다. 멤버가 아닌 친구의 신호는 거절된다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn voice_signaling_roster_and_media_keys() {
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let b = cl.node("b").await;
    let c = cl.node("c").await;
    connect_both(&a, &b).await;
    connect_both(&a, &c).await;
    connect_both(&b, &c).await;
    let space = tnet::create_crew(&a.node, &[b.node.clone(), c.node.clone()])
        .await
        .expect("크루 만들기");
    for (x, y) in [(&a, &b), (&a, &c), (&b, &c)] {
        testkit::authenticate(&x.node, y.peer())
            .await
            .expect("멤버끼리 인증");
    }
    let channel = tnet::create_voice_channel(&a.node, space, "음성")
        .await
        .expect("통화 채널");
    wait_channel(&b, space, channel).await;
    wait_channel(&c, space, channel).await;
    let (da, db, dc) = (dev(&a), dev(&b), dev(&c));

    let ja = a
        .ok::<m::VoiceJoin>(SpaceChannelParams { space, channel })
        .await;
    assert!(ja.generation >= 1, "혼자여도 키가 있어야 함");
    assert_eq!(
        ja.participants.iter().map(|p| p.device).collect::<Vec<_>>(),
        vec![da]
    );
    // 시험 노드는 STUN 서버를 내주지 않습니다.
    assert!(ja.ice_servers.is_empty());
    // 사람의 토큰은 프레임 키를 받고, 자동화 토큰에는 프레임 키를 주지 않습니다.
    let writer = token_ctx(vec![Scope::Read, Scope::Write], false);
    let jw = call_as::<m::VoiceJoin>(&a, &writer, SpaceChannelParams { space, channel }).await;
    assert!(jw.ice_servers.is_empty());
    assert_eq!(
        (jw.generation, jw.media_key.clone()),
        (ja.generation, ja.media_key.clone())
    );
    let ai = token_ctx(vec![Scope::Read, Scope::Write, Scope::Settings], true);
    let jai = call_as::<m::VoiceJoin>(&a, &ai, SpaceChannelParams { space, channel }).await;
    assert_eq!(jai.generation, 0);
    assert!(jai.media_key.expose().is_empty());
    let a_user = a.user();
    eventually("b의 채널 목록에 참가자가 보임", || async {
        let v = b
            .ok::<m::ChannelList>(SpaceParams { space })
            .await
            .into_iter()
            .find(|v| v.channel == channel)?;
        (v.participants == Some(vec![a_user]) && v.joined == Some(false)).then_some(())
    })
    .await;

    let jb = b
        .ok::<m::VoiceJoin>(SpaceChannelParams { space, channel })
        .await;
    assert!(jb.ice_servers.is_empty());
    // 참가가 끝났을 때 이미 있던 참가자를 압니다(참가 답장).
    let set = |v: &[yangpa_rpc::api::VoiceParticipant]| {
        v.iter().map(|p| p.device).collect::<BTreeSet<_>>()
    };
    assert_eq!(set(&jb.participants), BTreeSet::from([da, db]));
    let jc = c
        .ok::<m::VoiceJoin>(SpaceChannelParams { space, channel })
        .await;
    assert_eq!(set(&jc.participants), BTreeSet::from([da, db, dc]));
    let (g1, fp1) = wait_call_agrees(
        &[&a, &b, &c],
        space,
        channel,
        &[da, db, dc],
        0,
        "세 기기가 같은 프레임 키",
    )
    .await;

    // offer → answer → ICE.
    let mut rb = b.subscribe();
    let mut ra = a.subscribe();
    a.ok::<m::VoiceSignal>(signal(
        space,
        channel,
        ja.call,
        db,
        SignalPayload::Offer {
            sdp: "v=0 a→b".into(),
        },
    ))
    .await;
    let call_at_b = next_notification(&mut rb, "b가 offer를 받음", |n| match n {
        Notification::VoiceSignal {
            space: s,
            channel: ch,
            call,
            from,
            payload: SignalPayload::Offer { sdp },
        } if *s == space && *ch == channel && *from == da => {
            assert_eq!(sdp, "v=0 a→b");
            Some(*call)
        }
        _ => None,
    })
    .await;
    assert_eq!(call_at_b, jb.call, "앱에는 자기 통화 ID로 보임");
    b.ok::<m::VoiceSignal>(signal(
        space,
        channel,
        jb.call,
        da,
        SignalPayload::Answer {
            sdp: "v=0 b→a".into(),
        },
    ))
    .await;
    next_notification(&mut ra, "a가 answer를 받음", |n| {
        matches!(n, Notification::VoiceSignal { from, payload: SignalPayload::Answer { sdp }, .. }
            if *from == db && sdp == "v=0 b→a")
        .then_some(())
    })
    .await;
    c.ok::<m::VoiceSignal>(signal(
        space,
        channel,
        jc.call,
        da,
        SignalPayload::Ice {
            candidate: "candidate:1 1 udp 2122260223 192.0.2.7 50000 typ host".into(),
            sdp_mid: Some("0".into()),
            sdp_mline_index: Some(0),
        },
    ))
    .await;
    next_notification(&mut ra, "a가 c의 ICE 후보를 받음", |n| {
        matches!(n, Notification::VoiceSignal { from, payload: SignalPayload::Ice { sdp_mline_index: Some(0), .. }, .. }
            if *from == dc)
        .then_some(())
    })
    .await;
    // 앱은 제어 신호를 직접 보낼 수 없고, 통화 ID·받는 기기가 맞아야 합니다.
    assert!(
        a.call::<m::VoiceSignal>(signal(space, channel, ja.call, db, SignalPayload::Leave))
            .await
            .is_err()
    );
    let wrong_call = yangpa_core::ids::CallId::random();
    assert!(
        a.call::<m::VoiceSignal>(signal(
            space,
            channel,
            wrong_call,
            db,
            SignalPayload::Offer { sdp: "v=0".into() }
        ))
        .await
        .is_err()
    );
    assert!(
        a.call::<m::VoiceSignal>(signal(
            space,
            channel,
            ja.call,
            DeviceId([9; 32]),
            SignalPayload::Offer { sdp: "v=0".into() }
        ))
        .await
        .is_err()
    );

    // 화면공유 상태와 시청.
    let mut rb = b.subscribe();
    c.ok::<m::VoiceState>(VoiceStateParams {
        space,
        channel,
        muted: true,
        deafened: false,
        screen: true,
    })
    .await;
    next_notification(&mut rb, "b가 c의 화면공유를 봄", |n| match n {
        Notification::VoiceParticipants { participants, .. } => participants
            .iter()
            .any(|p| p.device == dc && p.screen && p.muted)
            .then_some(()),
        _ => None,
    })
    .await;
    assert!(
        b.call::<m::VoiceWatch>(VoiceWatchParams {
            space,
            channel,
            device: da,
            watching: true,
        })
        .await
        .is_err(),
        "화면공유 중이 아닌 기기는 볼 수 없음"
    );
    let mut rc = c.subscribe();
    a.ok::<m::VoiceWatch>(VoiceWatchParams {
        space,
        channel,
        device: dc,
        watching: true,
    })
    .await;
    next_notification(&mut rc, "c의 참가자 목록에 시청자 a", |n| match n {
        Notification::VoiceParticipants { participants, .. } => participants
            .iter()
            .any(|p| p.device == dc && p.viewers == vec![da])
            .then_some(()),
        _ => None,
    })
    .await;
    next_notification(&mut rc, "c가 시청 신호를 받음", |n| {
        matches!(n, Notification::VoiceSignal { from, payload: SignalPayload::WatchScreen { watching: true }, .. }
            if *from == da)
        .then_some(())
    })
    .await;
    // 뱃지 시간: 셋 다 다른 사람과 통화 중이고, 화면을 공유하는 것은 c만.
    assert_eq!(tnet::voice_talking(&c.node), (true, true));
    assert_eq!(tnet::voice_talking(&a.node), (true, false));

    // 멤버가 아닌 친구 d의 신호는 거절됩니다.
    let d = cl.node("d").await;
    befriend(&a, &d).await;
    testkit::authenticate(&d.node, a.peer())
        .await
        .expect("친구끼리 인증");
    let dd = dev(&d);
    let join = SignalPayload::Join {
        muted: false,
        deafened: false,
        screen: false,
        generation: 99,
        reply: false,
        sfu: None,
        session: 0,
    };
    for payload in [join, SignalPayload::Offer { sdp: "v=0".into() }] {
        let r = testkit::net_request(
            &d.node,
            a.peer(),
            YpRequest::Signal {
                space,
                channel,
                call: ja.call,
                from: dd,
                payload,
            },
        )
        .await
        .expect("요청 전달");
        assert!(
            matches!(
                r,
                YpResponse::Error {
                    code: WireErrorCode::NotMember,
                    ..
                }
            ),
            "{r:?}"
        );
    }
    // 다른 기기인 척 보낸 퇴장도 거절.
    let r = testkit::net_request(
        &d.node,
        a.peer(),
        YpRequest::Signal {
            space,
            channel,
            call: ja.call,
            from: db,
            payload: SignalPayload::Leave,
        },
    )
    .await
    .expect("요청 전달");
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
    assert!(
        d.call::<m::VoiceJoin>(SpaceChannelParams { space, channel })
            .await
            .is_err()
    );
    let va = tnet::voice_call(&a.node, &space, &channel).expect("a의 통화");
    assert_eq!(
        va.devices.iter().copied().collect::<BTreeSet<_>>(),
        BTreeSet::from([da, db, dc])
    );
    assert_eq!((va.generation, va.key_fingerprint), (g1, Some(fp1)));

    // b가 나가면 남은 참가자가 새 세대 키로 바꿉니다.
    let mut ra = a.subscribe();
    b.ok::<m::VoiceLeave>(SpaceChannelParams { space, channel })
        .await;
    next_notification(&mut ra, "a가 b의 퇴장을 받음", |n| {
        matches!(n, Notification::VoiceSignal { from, payload: SignalPayload::Leave, .. } if *from == db)
            .then_some(())
    })
    .await;
    let (g2, fp2) = wait_call_agrees(
        &[&a, &c],
        space,
        channel,
        &[da, dc],
        g1,
        "b가 나간 뒤 새 키",
    )
    .await;
    assert_ne!(fp2, fp1);
    let vb = tnet::voice_call(&b.node, &space, &channel).expect("b도 통화를 앎");
    assert!(!vb.joined);
    assert_eq!(vb.key_fingerprint, None, "나간 기기는 키를 지움");

    // c의 노드가 끊기면 c가 빠지고 다시 새 키.
    c.shutdown().await;
    let (_, fp3) = wait_call_agrees(&[&a], space, channel, &[da], g2, "c가 끊긴 뒤 새 키").await;
    assert_ne!(fp3, fp2);
    eventually("b의 목록에서도 c가 빠짐", || async {
        let v = tnet::voice_call(&b.node, &space, &channel)?;
        (v.devices == vec![da]).then_some(())
    })
    .await;
}

/// STUN 서버 목록(P2P 주소 알아내기용): 저장한 적 없으면 기본 목록, 저장한 목록은 `host:port`로 맞춰 남고 다시
/// 띄워도 그대로이며, 비운 목록은 STUN을 쓰지 않는다는 선택으로 남고, `null`이면 기본 목록으로 돌아갑니다. TURN·
/// 비밀번호가 든 주소와 자동화 토큰의 변경은 거절합니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stun_servers_default_save_and_reset() {
    let cl = Cluster::new();
    let mut a = cl
        .node_with("a", |c| {
            c.default_stun_servers = yangpa_daemon::config::DEFAULT_STUN_SERVERS
                .map(String::from)
                .to_vec();
        })
        .await;
    let v = a.ok::<m::NetStunServers>(Empty).await;
    assert!(v.is_default);
    assert_eq!(
        v.servers,
        [
            "stun.l.google.com:19302",
            "stun1.l.google.com:19302",
            "stun.cloudflare.com:3478"
        ]
    );
    let set = |servers: Option<Vec<&str>>| StunServersParams {
        servers: servers.map(|l| l.into_iter().map(String::from).collect()),
    };

    a.ok::<m::NetSetStunServers>(set(Some(vec![
        "stun:Stun.Example.org",
        "198.51.100.7:3479",
        "stun.example.org:3478",
    ])))
    .await;
    let saved = vec!["stun.example.org:3478", "198.51.100.7:3479"];
    let v = a.ok::<m::NetStunServers>(Empty).await;
    assert!(!v.is_default);
    assert_eq!(v.servers, saved);
    for bad in [
        vec!["turn:turn.example.org:3478"],
        vec!["user:pw@stun.example.org"],
        vec!["stun.example.org:99999"],
        (0..9).map(|_| "stun.example.org").collect(),
    ] {
        assert!(
            a.call::<m::NetSetStunServers>(set(Some(bad)))
                .await
                .is_err()
        );
    }
    // 자동화 토큰은 설정 범위가 있어도 바꾸지 못합니다(고른 서버가 이 기기의 공인 IP를 보게 되므로).
    let ai = token_ctx(vec![Scope::Read, Scope::Settings], true);
    let v = serde_json::to_value(set(Some(vec!["evil.example.org"]))).unwrap();
    assert!(
        a.node
            .call(&ai, m::NetSetStunServers::NAME, v)
            .await
            .is_err()
    );
    assert_eq!(a.ok::<m::NetStunServers>(Empty).await.servers, saved);

    a.ok::<m::NetSetStunServers>(set(Some(Vec::new()))).await;
    a.restart().await;
    let v = a.ok::<m::NetStunServers>(Empty).await;
    assert!(
        v.servers.is_empty() && !v.is_default,
        "비운 선택이 다시 띄운 뒤에도 남아야 함"
    );
    a.ok::<m::NetSetStunServers>(set(None)).await;
    let v = a.ok::<m::NetStunServers>(Empty).await;
    assert!(v.is_default);
    assert_eq!(v.servers.len(), 3);
}

/// Tor 브리지 설정: 기본 꺼짐, 직접 넣은 줄은 다듬어 저장하고 다시 띄워도 남으며, 깨진 줄·빈 직접 목록·lyrebird
/// 없이 켜기는 거절합니다. 브리지 줄은 사람만 읽고 바꿉니다(자동화 토큰은 읽기도 못 함).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bridges_setting_validates_and_persists() {
    use yangpa_rpc::api::{BridgeKind, BridgesParams};
    let cl = Cluster::new();
    let mut a = cl.node("a").await;
    let v = a.ok::<m::NetBridges>(Empty).await;
    assert!(!v.enabled && !v.auto);
    assert_eq!(v.kind, BridgeKind::Snowflake);

    let webtunnel = "webtunnel [2001:db8::1]:443 0123456789ABCDEF0123456789ABCDEF01234567 \
                     url=https://example.com/abc ver=0.0.1";
    let set = |enabled, kind, custom: Vec<&str>| BridgesParams {
        enabled,
        kind,
        custom: custom.into_iter().map(String::from).collect(),
    };
    a.ok::<m::NetSetBridges>(set(
        false,
        BridgeKind::Custom,
        vec!["", &format!("  {webtunnel} ")],
    ))
    .await;
    for bad in [
        set(false, BridgeKind::Custom, vec!["이건 브리지가 아님"]),
        set(
            false,
            BridgeKind::Custom,
            vec!["conjure 192.0.2.1:80 0123456789ABCDEF0123456789ABCDEF01234567"],
        ),
        set(true, BridgeKind::Custom, vec![]),
        set(
            false,
            BridgeKind::Custom,
            (0..33).map(|_| webtunnel).collect(),
        ),
    ] {
        assert!(a.call::<m::NetSetBridges>(bad).await.is_err());
    }
    // 시험 바이너리 옆·PATH에 lyrebird가 없으면 켤 수 없음.
    let v = a.ok::<m::NetBridges>(Empty).await;
    if !v.available {
        assert!(
            a.call::<m::NetSetBridges>(set(true, BridgeKind::Snowflake, vec![]))
                .await
                .is_err()
        );
    }
    let ai = token_ctx(vec![Scope::Read, Scope::Settings], true);
    assert!(
        a.node
            .call(&ai, m::NetBridges::NAME, serde_json::json!({}))
            .await
            .is_err()
    );

    a.restart().await;
    let v = a.ok::<m::NetBridges>(Empty).await;
    assert!(!v.enabled);
    assert_eq!(v.kind, BridgeKind::Custom);
    assert_eq!(v.custom, [webtunnel]);
}

/// 프레임 키는 자기 목록의 관리 기기가 보낸 것만 쓰고(다른 참가자의 키·범위 밖 세대는 쓰지 않음), 통화 권한을
/// 잃은 멤버는 모든 기기의 참가자 목록에서 빠져 새 키를 받지 못한다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn voice_keys_only_from_manager_and_revoked_members_dropped() {
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let b = cl.node("b").await;
    let c = cl.node("c").await;
    connect_both(&a, &b).await;
    connect_both(&a, &c).await;
    connect_both(&b, &c).await;
    let space = tnet::create_crew(&a.node, &[b.node.clone(), c.node.clone()])
        .await
        .expect("크루 만들기");
    for (p, q) in [(&a, &b), (&a, &c), (&b, &c)] {
        testkit::authenticate(&p.node, q.peer())
            .await
            .expect("멤버끼리 인증");
    }
    let channel = tnet::create_voice_channel(&a.node, space, "음성")
        .await
        .expect("통화 채널");
    wait_channel(&b, space, channel).await;
    wait_channel(&c, space, channel).await;
    let (da, db, dc) = (dev(&a), dev(&b), dev(&c));
    let all = [da, db, dc];
    for n in [&a, &b, &c] {
        n.ok::<m::VoiceJoin>(SpaceChannelParams { space, channel })
            .await;
    }
    let (g1, fp1) = wait_call_agrees(
        &[&a, &b, &c],
        space,
        channel,
        &all,
        0,
        "세 기기가 같은 프레임 키",
    )
    .await;

    // 키 관리 기기는 기기 ID가 가장 작은 기기입니다. 나머지 둘 중 x가 y에게 자기가 만든 키를 심으려 합니다.
    let mut ranked = [(&a, da), (&b, db), (&c, dc)];
    ranked.sort_by_key(|(_, d)| *d);
    let [(mgr, dm), (x, dx), (y, dy)] = ranked;
    let (req, forged) =
        tnet::forged_media_key(&x.node, space, channel, dy, g1 + 5).expect("키 만들기");
    let r = testkit::net_request(&x.node, y.peer(), req)
        .await
        .expect("요청 전달");
    assert!(matches!(r, YpResponse::Ok), "{r:?}");
    let vy = tnet::voice_call(&y.node, &space, &channel).expect("y의 통화");
    assert_eq!(
        (vy.generation, vy.key_fingerprint),
        (g1, Some(fp1)),
        "관리 기기가 아닌 참가자의 키를 씀"
    );
    // 범위 밖 세대는 거절합니다.
    for generation in [0, u64::MAX] {
        let (req, _) =
            tnet::forged_media_key(&x.node, space, channel, dy, generation).expect("키 만들기");
        let r = testkit::net_request(&x.node, y.peer(), req)
            .await
            .expect("요청 전달");
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
    }
    // 참가 알림에 아주 큰 세대를 실어도 관리 기기는 그 세대를 이어 받지 않습니다(넘침·키 교체 멈춤 없음).
    let r = testkit::net_request(
        &x.node,
        mgr.peer(),
        YpRequest::Signal {
            space,
            channel,
            call: yangpa_core::ids::CallId::random(),
            from: dx,
            payload: SignalPayload::Join {
                muted: false,
                deafened: false,
                screen: false,
                generation: u64::MAX,
                reply: true,
                sfu: None,
                session: 0,
            },
        },
    )
    .await
    .expect("요청 전달");
    assert!(matches!(r, YpResponse::Ok), "{r:?}");

    // x가 나가면 관리 기기가 새 키를 만들고, y는 x가 심으려던 키가 아니라 그 키를 씁니다.
    x.ok::<m::VoiceLeave>(SpaceChannelParams { space, channel })
        .await;
    let (g2, fp2) = wait_call_agrees(
        &[mgr, y],
        space,
        channel,
        &[dm, dy],
        g1,
        "x가 나간 뒤 관리 기기의 새 키",
    )
    .await;
    assert!(g2 < 1 << 39, "알려진 큰 세대를 이어 받음: {g2}");
    assert_ne!(fp2, fp1);
    assert_ne!(fp2, forged);
    x.ok::<m::VoiceJoin>(SpaceChannelParams { space, channel })
        .await;
    let (g3, _) = wait_call_agrees(&[&a, &b, &c], space, channel, &all, g2, "x가 다시 참가").await;

    // 소유자 a가 b의 음성 접속을 막으면 모든 기기에서 b가 빠지고, 남은 둘만 새 키를 받습니다.
    tnet::deny_in_channel(
        &a.node,
        space,
        channel,
        b.user(),
        Permissions::CONNECT_VOICE,
    )
    .await
    .expect("권한 막기");
    wait_call_agrees(&[&a, &c], space, channel, &[da, dc], g3, "b를 뺀 새 키").await;
    eventually("b가 통화에서 나와 키를 지움", || async {
        tnet::voice_call(&b.node, &space, &channel)
            .is_none_or(|v| !v.joined && v.key_fingerprint.is_none())
            .then_some(())
    })
    .await;
    assert!(
        b.call::<m::VoiceJoin>(SpaceChannelParams { space, channel })
            .await
            .is_err()
    );
}

/// 채널 목록에 보이는 통화 채널 참가자.
async fn voice_participants(
    n: &TestNode,
    space: SpaceId,
    channel: ChannelId,
) -> Option<Vec<yangpa_core::ids::UserId>> {
    n.ok::<m::ChannelList>(SpaceParams { space })
        .await
        .into_iter()
        .find(|v| v.channel == channel)?
        .participants
}

/// 참가하지 않고 보기만 하던 기기가 다시 뜨면(참가자 목록은 RAM에만 있음), 다시 연결·인증될 때 참가 중인
/// 기기에게서 목록을 다시 받는다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn voice_roster_restored_after_viewer_restarts() {
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let mut b = cl.node("b").await;
    connect_both(&a, &b).await;
    let space = tnet::create_crew(&a.node, &[b.node.clone()])
        .await
        .expect("크루 만들기");
    testkit::authenticate(&a.node, b.peer())
        .await
        .expect("멤버끼리 인증");
    let channel = tnet::create_voice_channel(&a.node, space, "음성")
        .await
        .expect("통화 채널");
    wait_channel(&b, space, channel).await;
    a.ok::<m::VoiceJoin>(SpaceChannelParams { space, channel })
        .await;
    let want = Some(vec![a.user()]);
    eventually("b가 a의 참가를 봄", || async {
        (voice_participants(&b, space, channel).await == want).then_some(())
    })
    .await;

    // 다른 네트워크 키로 다시 뜹니다: a는 b의 새 PeerId를 몰라 b가 건 인증에 답하기만 하고(이때 a가 보낸 통화
    // 알림은 아직 a를 인증하지 않은 b가 거절할 수 있음), 스스로 b를 인증하지 않습니다.
    tnet::rotate_net_key(&b.node).expect("네트워크 키 바꿈");
    b.restart().await;
    eventually("다시 뜬 b가 a의 참가를 다시 봄", || async {
        (voice_participants(&b, space, channel).await == want).then_some(())
    })
    .await;
}

/// 위치 기록: 다른 포트로 다시 뜬 친구 노드를 DHT로 찾아 메시지를 전하고, 기록을 가진 노드도 기기 ID 없이는
/// 열 수 없다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn locator_finds_moved_device_and_hides_it_from_strangers() {
    let cl = Cluster::new();
    let dht = cl.bare("dht").await;
    let mut a = cl.node("a").await;
    let mut b = cl.node("b").await;
    let boot = format!("{}/p2p/{}", listen_addrs(&dht.node).await[0], dht.peer());
    for n in [&a, &b] {
        n.add_bootstrap(&boot);
    }
    let space = befriend(&a, &b).await;
    let channel = a.first_channel(space).await;
    let (da, db) = (dev(&a), dev(&b));
    eventually("두 기기가 위치 기록을 올림", || async {
        (tnet::locator_published(&a.node).is_some() && tnet::locator_published(&b.node).is_some())
            .then_some(())
    })
    .await;

    // 기록을 가진 DHT 노드(낯선 노드)는 값을 봐도 누구인지·어디인지 알 수 없습니다. 모드마다 키가 달라 모드를
    // 바꿔 올린 기록을 같은 키로 이을 수도 없습니다.
    let key = tnet::locator_key(&da, yangpa_net::NetMode::Normal);
    assert_ne!(key, tnet::locator_key(&db, yangpa_net::NetMode::Normal));
    assert_ne!(key, tnet::locator_key(&da, yangpa_net::NetMode::Tor));
    let values = eventually("DHT에 a의 기록", || async {
        let v = tnet::dht_get(&dht.node, key.clone()).await;
        (!v.is_empty()).then_some(v)
    })
    .await;
    let a_peer = a.peer().to_bytes();
    let a_user = a.user();
    for v in &values {
        assert!(!contains(v, &a_peer), "PeerId가 평문으로 보임");
        assert!(!contains(v, da.as_bytes()));
        assert!(!contains(v, a_user.as_bytes()));
        assert!(!contains(v, b"127.0.0.1"));
        assert!(tnet::locator_open(&db, v).is_none(), "다른 기기 ID로 열림");
        assert!(tnet::locator_open(&DeviceId([7; 32]), v).is_none());
        let (peer, addrs) = tnet::locator_open(&da, v).expect("기기 ID를 알면 열림");
        assert_eq!(peer, a.peer());
        // 기록에는 자기 IP를 넣지 않습니다(릴레이·onion 주소만).
        assert!(
            addrs.iter().all(|x| {
                let s = x.to_string();
                s.contains("/p2p-circuit") || yangpa_net::addr::is_onion(x)
            }),
            "{addrs:?}"
        );
    }

    // 둘 다 다른 네트워크 키(업그레이드로 PeerId가 바뀐 것과 같음)와 다른 포트로 다시 뜹니다. 서로 기록해 둔
    // PeerId·주소가 모두 낡아, 상대의 새 PeerId는 위치 기록에만 있습니다(서로 새 PeerId를 모르므로 DHT 연결이 생겨도
    // 인증하지 않음).
    let old_a_peer = a.peer();
    for n in [&a, &b] {
        tnet::rotate_net_key(&n.node).expect("네트워크 키 바꿈");
    }
    a.shutdown().await;
    b.shutdown().await;
    a.start_on_new_port().await;
    b.start_on_new_port().await;
    let new_a_peer = a.peer();
    assert_ne!(new_a_peer, old_a_peer);
    let new_a = listen_addrs(&a.node).await;
    eventually("a가 새 PeerId로 위치 기록을 올림", || async {
        tnet::locator_published(&a.node)
            .filter(|(_, p)| *p == new_a_peer)
            .map(|_| ())
    })
    .await;
    eventually("DHT가 a의 새 주소를 앎", || async {
        let found = tnet::dht_find_peer(&dht.node, new_a_peer).await;
        found.iter().any(|x| new_a.contains(x)).then_some(())
    })
    .await;
    eventually("b가 다시 위치 기록을 올림(DHT에 붙음)", || async {
        tnet::locator_published(&b.node).map(|_| ())
    })
    .await;
    assert!(!tnet::is_authed(&b.node, &new_a_peer));
    assert_eq!(tnet::push_reroutes(&b.node), 0);

    // b가 보낸 메시지: 기록된 PeerId로 보내기가 실패하면 위치 기록에서 새 PeerId를 찾아 보냅니다.
    let id = b.send(space, channel, "새 PeerId로 찾아감").await;
    wait_message(
        &a,
        space,
        channel,
        id,
        "PeerId가 바뀐 a에게 메시지 도착",
        |_| true,
    )
    .await;
    eventually("위치 기록으로 찾아 보냄", || async {
        (tnet::push_reroutes(&b.node) > 0).then_some(())
    })
    .await;
    let new_a_s: Vec<String> = new_a.iter().map(ToString::to_string).collect();
    eventually("b가 a의 새 주소를 기록", || async {
        let cached = tnet::cached_addrs(&b.node, &new_a_peer);
        new_a_s.iter().any(|x| cached.contains(x)).then_some(())
    })
    .await;
    let (found, _) = tnet::locator_lookup(&b.node, &a_user, &da)
        .await
        .expect("위치 기록 조회");
    assert_eq!(found, new_a_peer);
}

/// `a`가 건 연결 하나만으로 `b`의 연락처 링크를 써 친구가 됩니다(b는 a에게 건 적이 없음).
async fn befriend_inbound(a: &TestNode, b: &TestNode) -> SpaceId {
    let link = b
        .ok::<m::FriendLinkCreate>(FriendLinkCreateParams {
            expires_in_secs: Some(3600),
            single_use: Some(true),
        })
        .await;
    let added = a.ok::<m::FriendAdd>(UrlParams { url: link.url }).await;
    assert_eq!(added.user, b.user());
    let (a_user, b_user) = (a.user(), b.user());
    let space = eventually("링크 주인이 임시 연락처로 기록", || async {
        b.ok::<m::FriendList>(Empty)
            .await
            .into_iter()
            .find(|f| f.user == a_user)
            .map(|f| f.space)
    })
    .await;
    eventually("링크를 쓴 쪽이 1:1 공간을 받음", || async {
        a.ok::<m::FriendList>(Empty)
            .await
            .into_iter()
            .any(|f| f.user == b_user && f.space == space)
            .then_some(())
    })
    .await;
    eventually("크루 키 순번", || async {
        let ea = testkit::crew_key_seq(&a.node, &space)?;
        let eb = testkit::crew_key_seq(&b.node, &space)?;
        (ea == eb).then_some(())
    })
    .await;
    make_friends(a, b).await;
    space
}

/// 들어온 연결만 있던 상대의 수신 주소(identify)를 기록해 두었다가, 재시작 뒤 그 주소로 다시 겁니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inbound_peer_address_recorded_and_reused_after_restart() {
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let mut b = cl.node("b").await;
    connect(&a, &b).await;
    assert!(
        tnet::cached_addrs(&b.node, &a.peer()).is_empty(),
        "인증 전에는 기록하지 않음"
    );
    let space = befriend_inbound(&a, &b).await;
    testkit::authenticate(&b.node, a.peer())
        .await
        .expect("친구끼리 인증");
    let a_addrs: Vec<String> = listen_addrs(&a.node)
        .await
        .iter()
        .map(ToString::to_string)
        .collect();
    eventually("b가 a의 수신 주소를 기록", || async {
        let cached = tnet::cached_addrs(&b.node, &a.peer());
        a_addrs.iter().any(|x| cached.contains(x)).then_some(())
    })
    .await;

    b.shutdown().await;
    b.start_on_new_port().await;
    eventually(
        "b가 기록한 주소로 a에게 다시 연결·인증",
        || async {
            (tnet::is_authed(&b.node, &a.peer()) && tnet::is_authed(&a.node, &b.peer()))
                .then_some(())
        },
    )
    .await;
    let channel = a.first_channel(space).await;
    let id = a.send(space, channel, "다시 연결됨").await;
    wait_message(&b, space, channel, id, "b가 메시지를 받음", |_| true).await;
}

/// 끊긴 기기 다시 걸기·친구 요청 다시 보내기가 시험 안에 돌게 간격을 줄입니다.
fn fast_redial(c: &mut NodeConfig) {
    c.redial_interval = std::time::Duration::from_millis(300);
}

/// 둘 다 꺼져 있다가 켜져 서로의 다시 걸기가 처음에 실패해도, 끊긴 친구에게 주기적으로 다시 걸어 이어집니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn offline_friend_is_redialed() {
    let cl = Cluster::new();
    let mut a = cl.node_with("a", fast_redial).await;
    let mut b = cl.node_with("b", fast_redial).await;
    let space = befriend(&a, &b).await;
    b.shutdown().await;
    // b가 꺼진 동안 a가 다른 포트로 다시 뜸: a의 재시작 연결은 실패하고, b가 아는 a 주소는 낡음.
    a.shutdown().await;
    a.start_on_new_port().await;
    b.start_again().await;
    eventually("a가 끊긴 b에게 다시 걸어 인증", || async {
        (tnet::is_authed(&a.node, &b.peer()) && tnet::is_authed(&b.node, &a.peer())).then_some(())
    })
    .await;
    let channel = b.first_channel(space).await;
    let id = b.send(space, channel, "다시 연결됨").await;
    wait_message(&a, space, channel, id, "a가 받음", |_| true).await;
}

/// 앱 기기 흉내: Tor 스웜처럼 DHT 서버 역할을 하지 않고(위치 기록을 받아 둘 노드가 없음) 다시 걸기는 짧게.
fn dht_client(c: &mut NodeConfig) {
    c.net.kad_mode = KadMode::Client;
    fast_redial(c);
}

/// 예약을 받는 릴레이(DHT 서버 아님): 직접 연결 시험 노드의 링크용 주소(릴레이 경유)가 Tor 기기의 onion 주소 몫을 합니다.
fn link_relay(c: &mut NodeConfig) {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("빈 포트")
        .local_addr()
        .expect("주소")
        .port();
    let addr: Multiaddr = format!("/ip4/127.0.0.1/tcp/{port}").parse().expect("주소");
    c.net.listen = vec![addr.clone()];
    c.net.external_addrs = vec![addr];
    c.net.relay_server = true;
    c.net.kad_mode = KadMode::Client;
}

async fn sees_online(n: &TestNode, space: SpaceId, user: UserId) {
    eventually(
        &format!("{}가 상대를 온라인으로 봄", n.name),
        || async {
            n.ok::<m::MemberList>(SpaceParams { space })
                .await
                .iter()
                .any(|v| v.user == user && v.online)
                .then_some(())
        },
    )
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

/// 보고된 상황(2026-10-30): 크루장 B의 크루에 A·C가 B의 초대로 들어옴. A↔B는 친구, B↔C는 연락처 링크, A↔C는 아무 관계
/// 없음. 앱 기기는 모두 DHT 클라이언트라 위치 기록으로는 서로를 찾지 못하지만, 둘 다 연결된 멤버 B가 소개해 A와 C가
/// 보낼 것이 없어도 서로 직접 잇고 온라인으로 보며, B가 꺼진 뒤에도 주고받습니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crew_members_meet_through_a_member_without_dht() {
    let cl = Cluster::new();
    let r = cl.bare_with("r", link_relay).await;
    let a = cl.node_with("a", dht_client).await;
    let b = cl.node_with("b", dht_client).await;
    let c = cl.node_with("c", dht_client).await;
    let boot = format!("{}/p2p/{}", listen_addrs(&r.node).await[0], r.peer());
    for n in [&a, &b, &c] {
        tnet::skip_identify_addrs(&n.node);
        n.add_bootstrap(&boot);
    }
    for n in [&a, &b, &c] {
        eventually("릴레이 예약 주소", || async {
            (!tnet::shareable_addrs(&n.node).is_empty()).then_some(())
        })
        .await;
    }
    befriend(&a, &b).await;
    meet_by_link(&b, &c, None).await;

    let space = b
        .ok::<m::CrewCreate>(CrewCreateParams {
            name: "소개 크루".into(),
            description: None,
        })
        .await
        .space;
    for joiner in [&a, &c] {
        let url = b
            .ok::<m::InviteCreate>(InviteCreateParams {
                space,
                expires_in_secs: None,
                max_uses: Some(1),
                roles: Vec::new(),
            })
            .await
            .url;
        let r = joiner.ok::<m::InviteJoin>(UrlParams { url }).await;
        assert_eq!(r.status, InviteJoinStatus::Joined, "{} 가입", joiner.name);
    }
    let (a_user, c_user) = (a.user(), c.user());
    eventually("모두가 셋을 멤버로 봄", || async {
        [&a, &b, &c]
            .iter()
            .all(|n| {
                testkit::space_state(&n.node, &space)
                    .is_some_and(|st| [a_user, b.user(), c_user].iter().all(|u| st.is_member(u)))
            })
            .then_some(())
    })
    .await;

    // 아무것도 보내지 않아도 B의 소개로 서로 찾아 인증하고 온라인으로 봅니다.
    eventually("A와 C가 서로 인증", || async {
        (tnet::is_authed(&a.node, &c.peer()) && tnet::is_authed(&c.node, &a.peer())).then_some(())
    })
    .await;
    sees_online(&a, space, c_user).await;
    sees_online(&c, space, a_user).await;

    b.shutdown().await;
    let general = a.first_channel(space).await;
    a.send(space, general, "B 없이 A가").await;
    sees_text(&c, space, general, "B 없이 A가").await;
    c.send(space, general, "B 없이 C가").await;
    sees_text(&a, space, general, "B 없이 C가").await;
    sees_online(&a, space, c_user).await;
    sees_online(&c, space, a_user).await;
}

/// 링크에 릴레이 주소가 실리도록 예약을 잡습니다(앱에서는 onion 주소의 몫).
async fn reachable_by_link(n: &TestNode, r: &TestNode) {
    tnet::skip_identify_addrs(&n.node);
    n.add_bootstrap(&format!(
        "{}/p2p/{}",
        listen_addrs(&r.node).await[0],
        r.peer()
    ));
    eventually("릴레이 예약 주소", || async {
        (!tnet::shareable_addrs(&n.node).is_empty()).then_some(())
    })
    .await;
}

async fn device_offer(n: &TestNode) -> DeviceLinkOffer {
    n.ok::<m::DeviceLinkCreate>(Empty).await
}

/// 기기 연결 시험의 새 기기. 아무에게도 손으로 잇지 않습니다.
async fn new_phone(cl: &Cluster) -> TestNode {
    let phone = cl.bare_with("phone", dht_client).await;
    tnet::skip_identify_addrs(&phone.node);
    phone
}

/// 아무도 받지 않는 직접 주소.
fn dead_addr() -> serde_bytes::ByteBuf {
    let addr: Multiaddr = "/ip4/127.0.0.1/tcp/1".parse().expect("주소");
    serde_bytes::ByteBuf::from(addr.to_vec())
}

fn join_params(offer: &DeviceLinkOffer) -> DeviceLinkJoinParams {
    DeviceLinkJoinParams {
        url: offer.url.clone(),
    }
}

async fn wait_request(n: &TestNode) -> DeviceLinkRequestView {
    eventually("승인을 기다리는 요청", || async {
        n.ok::<m::DeviceLinkPending>(Empty).await
    })
    .await
}

async fn decide(n: &TestNode, approve: bool) {
    n.ok::<m::DeviceLinkDecide>(DeviceLinkDecideParams { approve })
        .await;
}

async fn link_result(rx: &mut broadcast::Receiver<Notification>) -> DeviceLinkStatus {
    next_notification(rx, "연결 결과", |n| match n {
        Notification::DeviceLinkResult { status } => Some(*status),
        _ => None,
    })
    .await
}

/// 새 기기는 아무에게도 손으로 잇지 않고 링크의 주소만으로 내 기기에 닿습니다. 양쪽 번호가 같고, 승인 전에는 신원이 없으며,
/// 승인하면 친구 요청 없이 크루를 받습니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn new_device_joins_by_the_link_alone() {
    let cl = Cluster::new();
    let r = cl.bare_with("r", link_relay).await;
    let pc = cl.node_with("pc", dht_client).await;
    reachable_by_link(&pc, &r).await;
    let space = pc
        .ok::<m::CrewCreate>(CrewCreateParams {
            name: "내 크루".into(),
            description: None,
        })
        .await
        .space;
    let offer = device_offer(&pc).await;
    assert!(offer.url.expose().starts_with("yangpa://device/"));
    assert_eq!(
        offer.qr.modules.len(),
        (offer.qr.size * offer.qr.size) as usize
    );

    let phone = new_phone(&cl).await;
    let mut asked = pc.subscribe();
    let mut rx = phone.subscribe();
    let code = phone
        .ok::<m::DeviceLinkJoin>(join_params(&offer))
        .await
        .code;
    assert_eq!(code.len(), 6);
    let request = next_notification(&mut asked, "승인 요청 알림", |n| match n {
        Notification::DeviceLinkRequest(v) => Some(v.clone()),
        _ => None,
    })
    .await;
    assert_eq!(request.code, code);
    assert!(!request.contested);
    assert!(
        phone.call::<m::IdentityGet>(Empty).await.is_err(),
        "승인 전에는 신원이 없음"
    );

    decide(&pc, true).await;
    assert_eq!(link_result(&mut rx).await, DeviceLinkStatus::Approved);
    assert_eq!(phone.user(), pc.user());
    let device = dev(&phone);
    assert!(testkit::device_list(&pc.node, &pc.user()).is_some_and(|l| l.contains_active(&device)));
    eventually("새 기기가 크루를 받음", || async {
        testkit::space_state(&phone.node, &space).map(|_| ())
    })
    .await;
    for n in [&pc, &phone] {
        assert!(n.ok::<m::FriendRequests>(Empty).await.is_empty());
        assert!(n.ok::<m::FriendList>(Empty).await.is_empty());
    }
}

/// 링크의 직접 주소로 닿습니다: 릴레이도 Tor도 없이 번호를 맞추고 승인까지 끝납니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn new_device_links_over_the_direct_address_alone() {
    let cl = Cluster::new();
    let pc = cl.node_with("pc", dht_client).await;
    let offer = device_offer(&pc).await;
    let link = DeviceLink::parse(offer.url.expose()).expect("링크");
    assert!(link.contact.relay_addrs.is_empty() && link.contact.onion.is_none());
    assert!(!link.direct.expect("직접 연락처").addrs.is_empty());

    let phone = new_phone(&cl).await;
    let mut rx = phone.subscribe();
    let code = phone
        .ok::<m::DeviceLinkJoin>(join_params(&offer))
        .await
        .code;
    assert_eq!(wait_request(&pc).await.code, code);
    decide(&pc, true).await;
    assert_eq!(link_result(&mut rx).await, DeviceLinkStatus::Approved);
    assert_eq!(phone.user(), pc.user());
}

/// 직접 주소로 닿지 않으면 다른 길(Tor 기기의 onion 몫을 하는 릴레이 주소)로 이어 가 끝까지 연결합니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn new_device_falls_back_when_the_direct_address_fails() {
    let cl = Cluster::new();
    let r = cl.bare_with("r", link_relay).await;
    let pc = cl.node_with("pc", dht_client).await;
    reachable_by_link(&pc, &r).await;
    let offer = device_offer(&pc).await;
    let mut link = DeviceLink::parse(offer.url.expose()).expect("링크");
    let direct = link.direct.as_mut().expect("직접 연락처");
    direct.peer_id = PeerId::random().to_bytes();
    direct.addrs = vec![dead_addr()];
    let url = link.to_url().expect("링크");

    let phone = new_phone(&cl).await;
    let mut rx = phone.subscribe();
    phone
        .ok::<m::DeviceLinkJoin>(DeviceLinkJoinParams { url: url.into() })
        .await;
    wait_request(&pc).await;
    decide(&pc, true).await;
    assert_eq!(link_result(&mut rx).await, DeviceLinkStatus::Approved);
}

/// 기기가 하나면 직접 스웜은 링크를 띄운 동안만 돕니다(IP를 쓰는 수신 포트를 열어 두지 않게).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn direct_swarm_runs_only_while_a_link_is_up() {
    let cl = Cluster::new();
    let pc = cl.node("pc").await;
    assert!(!testkit::p2p_running(&pc.node));
    device_offer(&pc).await;
    assert!(testkit::p2p_running(&pc.node));
    pc.ok::<m::DeviceLinkCancel>(Empty).await;
    eventually("직접 스웜을 내림", || async {
        (!testkit::p2p_running(&pc.node)).then_some(())
    })
    .await;
}

/// 거절하면 새 기기는 신원을 받지 못하고 내 목록도 그대로이며, 그 링크는 끝납니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn refused_device_gets_no_identity() {
    let cl = Cluster::new();
    let pc = cl.node("pc").await;
    let phone = cl.bare("phone").await;
    connect(&phone, &pc).await;
    let offer = device_offer(&pc).await;
    let mut rx = phone.subscribe();
    phone.ok::<m::DeviceLinkJoin>(join_params(&offer)).await;
    wait_request(&pc).await;
    decide(&pc, false).await;

    assert_eq!(link_result(&mut rx).await, DeviceLinkStatus::Rejected);
    assert!(phone.call::<m::IdentityGet>(Empty).await.is_err());
    assert_eq!(pc.ok::<m::DeviceList>(Empty).await.len(), 1);
    let e = phone
        .call::<m::DeviceLinkJoin>(join_params(&offer))
        .await
        .expect_err("끝난 링크");
    assert_eq!(e.code, ERR_FORBIDDEN);
}

/// 링크가 새어 다른 기기가 또 쓰면 그 기기는 거절되고 내 기기 화면에 경고가 뜹니다. 토큰을 알아도 요청한 노드가 아니면
/// 승인 답을 받지 못합니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn second_device_on_one_link_is_flagged_and_gets_nothing() {
    let cl = Cluster::new();
    let pc = cl.node("pc").await;
    let phone = cl.bare("phone").await;
    let other = cl.bare("other").await;
    connect(&phone, &pc).await;
    connect(&other, &pc).await;
    let offer = device_offer(&pc).await;
    let code = phone
        .ok::<m::DeviceLinkJoin>(join_params(&offer))
        .await
        .code;
    wait_request(&pc).await;

    let mut asked = pc.subscribe();
    let e = other
        .call::<m::DeviceLinkJoin>(join_params(&offer))
        .await
        .expect_err("먼저 쓴 기기가 있음");
    assert_eq!(e.code, ERR_FORBIDDEN);
    let flagged = next_notification(&mut asked, "경고", |n| match n {
        Notification::DeviceLinkRequest(v) => Some(v.clone()),
        _ => None,
    })
    .await;
    assert!(flagged.contested);
    assert_eq!(flagged.code, code, "처음 요청의 번호는 그대로");

    decide(&pc, true).await;
    let token = DeviceLink::parse(offer.url.expose()).expect("링크").token;
    let r = testkit::net_request(&other.node, pc.peer(), YpRequest::DeviceLinkPoll { token })
        .await
        .expect("답");
    assert!(
        matches!(
            r,
            YpResponse::Error {
                code: WireErrorCode::Expired,
                ..
            }
        ),
        "{r:?}"
    );
    eventually("요청한 기기만 신원을 받음", || async {
        phone.call::<m::IdentityGet>(Empty).await.ok()
    })
    .await;
    assert!(other.call::<m::IdentityGet>(Empty).await.is_err());
}

/// 새 링크를 띄우거나, 취소하거나, 기한이 지나면 앞 링크는 끝납니다. 기다리는 요청이 없으면 승인할 것도 없고, 신원이 있는
/// 기기는 연결을 청하지 못합니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ended_link_is_refused() {
    let cl = Cluster::new();
    let pc = cl.node("pc").await;
    let phone = cl.bare("phone").await;
    connect(&phone, &pc).await;
    async fn refused(phone: &TestNode, offer: &DeviceLinkOffer) {
        let e = phone
            .call::<m::DeviceLinkJoin>(join_params(offer))
            .await
            .expect_err("끝난 링크");
        assert_eq!(e.code, ERR_FORBIDDEN);
    }

    let replaced = device_offer(&pc).await;
    let canceled = device_offer(&pc).await;
    refused(&phone, &replaced).await;
    pc.ok::<m::DeviceLinkCancel>(Empty).await;
    refused(&phone, &canceled).await;
    let expired = device_offer(&pc).await;
    testkit::expire_device_link(&pc.node);
    refused(&phone, &expired).await;

    let e = pc
        .call::<m::DeviceLinkDecide>(DeviceLinkDecideParams { approve: true })
        .await
        .expect_err("기다리는 요청 없음");
    assert_eq!(e.code, ERR_NOT_FOUND);
    let offer = device_offer(&pc).await;
    let e = pc
        .call::<m::DeviceLinkJoin>(join_params(&offer))
        .await
        .expect_err("이미 신원이 있음");
    assert_eq!(e.code, ERR_CONFLICT);
}

/// 켜져 있지 않은 내 기기는 닿았다고 하지 않습니다. 연락처 링크는 내 기기를 잇는 길이 아니어서 내 링크면 거절합니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn join_reports_an_unreachable_device() {
    let cl = Cluster::new();
    let pc = cl.node_with("pc", dht_client).await;
    // 릴레이가 없어 링크에 주소가 없고, 직접 주소는 아무도 받지 않는 곳입니다.
    let offer = device_offer(&pc).await;
    let mut link = DeviceLink::parse(offer.url.expose()).expect("링크");
    link.direct.as_mut().expect("직접 연락처").addrs = vec![dead_addr()];
    let url = link.to_url().expect("링크").into();
    let phone = cl.bare_with("phone", dht_client).await;
    let e = phone
        .call::<m::DeviceLinkJoin>(DeviceLinkJoinParams { url })
        .await
        .expect_err("닿지 않음");
    assert_eq!(e.code, ERR_NETWORK);
    eventually("그만둔 뒤 직접 스웜을 내림", || async {
        (!testkit::p2p_running(&phone.node)).then_some(())
    })
    .await;

    let url = pc
        .ok::<m::FriendLinkCreate>(FriendLinkCreateParams {
            expires_in_secs: None,
            single_use: None,
        })
        .await
        .url;
    let e = pc
        .call::<m::FriendAdd>(UrlParams { url })
        .await
        .expect_err("내 링크");
    assert_eq!(e.code, ERR_INVALID_INPUT);
}

/// 승인되면 첫 동기화가 끝날 때까지 `first_syncing`이 켜져 있고, 끝났다고 알릴 때는 프로필·친구·크루가 이미 와 있습니다.
/// 친구는 친구 요청 없이 이어집니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn first_sync_ends_with_profile_friends_and_crews_in_place() {
    let cl = Cluster::new();
    let r = cl.bare_with("r", link_relay).await;
    let a = cl.node_with("a", dht_client).await;
    let b = cl.node_with("b", dht_client).await;
    befriend(&b, &a).await;
    let crew = a
        .ok::<m::CrewCreate>(CrewCreateParams {
            name: "내 크루".into(),
            description: None,
        })
        .await;
    reachable_by_link(&a, &r).await;
    let offer = device_offer(&a).await;

    let phone = new_phone(&cl).await;
    let mut rx = phone.subscribe();
    phone.ok::<m::DeviceLinkJoin>(join_params(&offer)).await;
    wait_request(&a).await;
    decide(&a, true).await;
    assert_eq!(link_result(&mut rx).await, DeviceLinkStatus::Approved);
    assert!(phone.ok::<m::IdentityGet>(Empty).await.first_syncing);
    next_notification(&mut rx, "첫 동기화 끝", |n| {
        matches!(n, Notification::DeviceSynced).then_some(())
    })
    .await;

    let id = phone.ok::<m::IdentityGet>(Empty).await;
    assert!(!id.first_syncing);
    assert_eq!(
        id.profile.name,
        a.ok::<m::IdentityGet>(Empty).await.profile.name,
        "내 프로필이 와 있음"
    );
    assert!(
        phone
            .ok::<m::FriendList>(Empty)
            .await
            .iter()
            .any(|f| { f.user == b.user() && f.kind == ContactKind::Friend }),
        "친구가 와 있음"
    );
    assert!(phone.ok::<m::FriendRequests>(Empty).await.is_empty());
    assert!(
        phone
            .ok::<m::SpaceList>(Empty)
            .await
            .iter()
            .any(|s| s.space == crew.space),
        "크루가 와 있음"
    );
}

async fn connection_to(n: &TestNode, user: UserId) -> ConnectionMode {
    n.ok::<m::TrustGet>(UserParams { user }).await.connection
}

/// 내 기기끼리는 신뢰 스위치 없이 직접 잇고, 다른 기기를 폐기하면 끊고 스웜도 내립니다. 기기가 하나면 띄우지 않습니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn own_devices_link_directly_until_revoked() {
    let cl = Cluster::new();
    let r = cl.bare_with("r", link_relay).await;
    let a = cl.node_with("a", dht_client).await;
    reachable_by_link(&a, &r).await;
    assert!(!testkit::p2p_running(&a.node), "기기가 하나면 띄우지 않음");
    let offer = device_offer(&a).await;

    let phone = new_phone(&cl).await;
    assert!(
        !testkit::p2p_running(&phone.node),
        "승인 전에는 띄우지 않음"
    );
    let mut rx = phone.subscribe();
    phone.ok::<m::DeviceLinkJoin>(join_params(&offer)).await;
    wait_request(&a).await;
    decide(&a, true).await;
    assert_eq!(link_result(&mut rx).await, DeviceLinkStatus::Approved);

    let user = a.user();
    for (n, what) in [(&a, "원래 기기"), (&phone, "새 기기")] {
        eventually(&format!("{what}가 직접 연결을 봄"), || async {
            (connection_to(n, user).await == ConnectionMode::P2p).then_some(())
        })
        .await;
    }

    a.ok::<m::DeviceRevoke>(DeviceRevokeParams {
        device: dev(&phone),
    })
    .await;
    eventually("폐기하면 직접 스웜을 내림", || async {
        (!testkit::p2p_running(&a.node) && connection_to(&a, user).await != ConnectionMode::P2p)
            .then_some(())
    })
    .await;
}

/// 큰 크루 흉내: 활성 기기 2대를 넘으면 gossip과 이웃 고르기를 쓰는 크기(작은 크루처럼 멤버 모두에게 다시 걸지 않음).
fn big_crew_client(c: &mut NodeConfig) {
    dht_client(c);
    c.push_max_devices = 2;
}

async fn all_members(nodes: &[&TestNode], space: SpaceId, users: &[UserId]) {
    eventually("모두가 모두를 멤버로 봄", || async {
        nodes
            .iter()
            .all(|n| {
                testkit::space_state(&n.node, &space)
                    .is_some_and(|st| users.iter().all(|u| st.is_member(u)))
            })
            .then_some(())
    })
    .await;
}

async fn join_by_invite(owner: &TestNode, joiner: &TestNode, space: SpaceId) {
    let url = owner
        .ok::<m::InviteCreate>(InviteCreateParams {
            space,
            expires_in_secs: None,
            max_uses: Some(1),
            roles: Vec::new(),
        })
        .await
        .url;
    let r = joiner.ok::<m::InviteJoin>(UrlParams { url }).await;
    assert_eq!(r.status, InviteJoinStatus::Joined, "{} 가입", joiner.name);
}

/// 큰 크루에서 B의 크루에 A·C가 B의 초대로만 들어오고, C가 꺼진 동안 D도 B의 초대로 들어와 B에게서 A·C의 노드를 받은
/// 상태. A·C는 서로 만난 적이 없어도 B에게 받은 멤버 노드 목록으로 이웃이 됩니다.
struct LateJoiner {
    _r: TestNode,
    a: TestNode,
    b: TestNode,
    c: TestNode,
    d: TestNode,
    space: SpaceId,
}

async fn big_crew_with_late_joiner(cl: &Cluster) -> LateJoiner {
    let r = cl.bare_with("r", link_relay).await;
    let a = cl.node_with("a", big_crew_client).await;
    let b = cl.node_with("b", big_crew_client).await;
    let c = cl.node_with("c", big_crew_client).await;
    let d = cl.node_with("d", big_crew_client).await;
    let boot = format!("{}/p2p/{}", listen_addrs(&r.node).await[0], r.peer());
    for n in [&a, &b, &c, &d] {
        tnet::skip_identify_addrs(&n.node);
        n.add_bootstrap(&boot);
    }
    for n in [&a, &b, &c, &d] {
        eventually("릴레이 예약 주소", || async {
            (!tnet::shareable_addrs(&n.node).is_empty()).then_some(())
        })
        .await;
    }

    let space = b
        .ok::<m::CrewCreate>(CrewCreateParams {
            name: "큰 크루".into(),
            description: None,
        })
        .await
        .space;
    join_by_invite(&b, &a, space).await;
    join_by_invite(&b, &c, space).await;
    let (a_user, b_user, c_user, d_user) = (a.user(), b.user(), c.user(), d.user());
    all_members(&[&a, &b, &c], space, &[a_user, b_user, c_user]).await;

    eventually("A와 C가 서로 인증", || async {
        (tnet::is_authed(&a.node, &c.peer()) && tnet::is_authed(&c.node, &a.peer())).then_some(())
    })
    .await;

    c.shutdown().await;
    join_by_invite(&b, &d, space).await;
    all_members(&[&a, &b, &d], space, &[a_user, b_user, c_user, d_user]).await;
    eventually("D가 A와 이어짐", || async {
        tnet::is_authed(&d.node, &a.peer()).then_some(())
    })
    .await;
    LateJoiner {
        _r: r,
        a,
        b,
        c,
        d,
        space,
    }
}

/// C와 D가 서로 인증하고 B·A 없이 주고받습니다.
async fn late_pair_talks(c: &TestNode, d: &TestNode, space: SpaceId) {
    eventually("C와 D가 서로 인증", || async {
        (tnet::is_authed(&c.node, &d.peer()) && tnet::is_authed(&d.node, &c.peer())).then_some(())
    })
    .await;
    let general = d.first_channel(space).await;
    d.send(space, general, "B·A 없이 D가").await;
    sees_text(c, space, general, "B·A 없이 D가").await;
    c.send(space, general, "B·A 없이 C가").await;
    sees_text(d, space, general, "B·A 없이 C가").await;
}

/// D는 C가 꺼져 있을 때 들어와 C의 노드를 소개만 받아 두고, C와 한 번도 함께 켜진 적 없이 B·A가 모두 꺼진 뒤 다시
/// 켜져도 저장해 둔 소개로 C와 이어져 주고받습니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn big_crew_members_meet_through_member_node_lists() {
    let cl = Cluster::new();
    let LateJoiner {
        _r,
        a,
        b,
        mut c,
        mut d,
        space,
    } = big_crew_with_late_joiner(&cl).await;
    let d_user = d.user();
    // D가 꺼진 뒤 C가 켜져 B·A에게서 D의 가입을 받습니다. 그다음 B·A가 꺼지고 D가 켜지면, C와 D는 그 전에 함께 켜진 적이
    // 없으므로 소개해 둔 노드(D는 C의 것, C는 B·A에게 받은 D의 것)로만 이어집니다.
    // 시험의 릴레이 경유 주소가 Tor 기기의 onion 주소 몫이라 다시 뜬 노드도 같은 릴레이에 다시 예약해야 같은 주소로
    // 닿습니다(같은 포트로 다시 띄우면 릴레이 예약이 돌아오지 않아 새 포트로 띄움, 릴레이 경유 주소는 포트와 상관없음).
    d.shutdown().await;
    c.start_on_new_port().await;
    tnet::skip_identify_addrs(&c.node);
    eventually("C가 D의 가입을 받음", || async {
        testkit::space_state(&c.node, &space)
            .is_some_and(|st| st.is_member(&d_user))
            .then_some(())
    })
    .await;
    b.shutdown().await;
    a.shutdown().await;
    d.start_on_new_port().await;
    tnet::skip_identify_addrs(&d.node);
    late_pair_talks(&c, &d, space).await;
}

/// C가 꺼진 동안 들어온 D의 가입을 아는 B·A가 모두 꺼진 채 C·D만 켜지면, C는 처음에 D를 멤버로 모르지만 D에게서 공간
/// 상태를 받아 D의 가입을 확인하고 이어집니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn missed_join_is_learned_from_the_newcomer() {
    let cl = Cluster::new();
    let LateJoiner {
        _r,
        a,
        b,
        mut c,
        mut d,
        space,
    } = big_crew_with_late_joiner(&cl).await;
    let d_user = d.user();
    b.shutdown().await;
    a.shutdown().await;
    // 새 포트로 띄우는 까닭은 위 시험과 같음.
    d.shutdown().await;
    d.start_on_new_port().await;
    tnet::skip_identify_addrs(&d.node);
    c.start_on_new_port().await;
    tnet::skip_identify_addrs(&c.node);
    assert!(
        testkit::space_state(&c.node, &space).is_some_and(|st| !st.is_member(&d_user)),
        "C가 D의 가입을 이미 앎"
    );
    late_pair_talks(&c, &d, space).await;
}

/// 놓친 가입 확인은 지목한 크루 중 받는 쪽이 멤버인 것만 받습니다: 그런 크루가 없으면 `NotMember`로 끝나 인증을
/// 보내지 않습니다. 그 크루를 지목하면 신원만 맞는 남도 받습니다(크루 ID를 아는 누구나 인증을 받는 남는 노출).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn check_join_takes_only_named_crews_of_mine() {
    let cl = Cluster::new();
    let c = cl.node("c").await;
    let e = cl.node("e").await;
    let f = cl.node("f").await;
    let space = c
        .ok::<m::CrewCreate>(CrewCreateParams {
            name: "크루".into(),
            description: None,
        })
        .await
        .space;
    connect(&e, &c).await;
    connect(&f, &c).await;
    let refused = |r: &YpResponse, want: WireErrorCode| matches!(r, YpResponse::Error { code, .. } if *code == want);
    let too_many: Vec<SpaceId> = (0..=yangpa_core::wire::MAX_CHECK_JOIN_SPACES)
        .map(|_| SpaceId::random())
        .collect();
    for spaces in [Vec::new(), too_many] {
        let r = testkit::check_join(&e.node, c.peer(), spaces)
            .await
            .expect("요청 전달");
        assert!(refused(&r, WireErrorCode::BadRequest), "{r:?}");
    }
    let r = testkit::check_join(&e.node, c.peer(), vec![SpaceId::random()])
        .await
        .expect("요청 전달");
    assert!(refused(&r, WireErrorCode::NotMember), "{r:?}");
    // 차례를 쓴 뒤에는 멤버 여부와 무관하게 같은 답이라 크루마다 물어보는 데 쓰지 못합니다.
    let r = testkit::check_join(&e.node, c.peer(), vec![SpaceId::random()])
        .await
        .expect("요청 전달");
    assert!(matches!(r, YpResponse::Ok), "{r:?}");
    // 그래서 진짜 크루는 다른 기기로 확인합니다.
    let r = testkit::check_join(&f.node, c.peer(), vec![space])
        .await
        .expect("요청 전달");
    assert!(matches!(r, YpResponse::Ok), "{r:?}");
    // 밴된 사람은 놓친 가입을 핑계로 내가 자기에게서 공간 기록을 받아 가게 하지 못합니다.
    let g = cl.node("g").await;
    connect(&g, &c).await;
    c.ok::<m::MemberBan>(MemberBanParams {
        space,
        user: g.user(),
        reason: None,
        delete_recent: false,
    })
    .await;
    let r = testkit::check_join(&g.node, c.peer(), vec![space])
        .await
        .expect("요청 전달");
    assert!(refused(&r, WireErrorCode::NotMember), "{r:?}");
}

/// 링크 주인이 꺼져 있으면 `friend.add`는 곧 `pending`으로 끝나고, 주인이 켜지면 요청이 한 번만 닿아 둘이 임시
/// 연락처가 됩니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn contact_link_delivered_when_owner_comes_online() {
    let cl = Cluster::new();
    let a = cl.node_with("a", fast_redial).await;
    let mut b = cl.node_with("b", fast_redial).await;
    connect(&a, &b).await;
    let link = b
        .ok::<m::FriendLinkCreate>(FriendLinkCreateParams {
            expires_in_secs: Some(3600),
            single_use: Some(true),
        })
        .await;
    b.shutdown().await;
    let started = std::time::Instant::now();
    let added = a.ok::<m::FriendAdd>(UrlParams { url: link.url }).await;
    assert_eq!(added.user, b.user());
    assert!(started.elapsed() < std::time::Duration::from_secs(40));
    assert_eq!(testkit::pending_redeems(&a.node), 1);

    b.start_again().await;
    let (a_user, b_user) = (a.user(), b.user());
    let space = eventually("켜진 b가 a를 임시 연락처로", || async {
        b.ok::<m::FriendList>(Empty)
            .await
            .into_iter()
            .find(|f| f.user == a_user && f.kind == ContactKind::Temporary)
            .map(|f| f.space)
    })
    .await;
    eventually("a의 대기열이 빔", || async {
        (testkit::pending_redeems(&a.node) == 0).then_some(())
    })
    .await;
    eventually("a도 1:1 공간을 받음", || async {
        a.ok::<m::FriendList>(Empty)
            .await
            .into_iter()
            .any(|f| f.user == b_user && f.space == space && f.kind == ContactKind::Temporary)
            .then_some(())
    })
    .await;
    assert_eq!(
        b.ok::<m::SpaceList>(Empty).await.len(),
        1,
        "요청이 한 번만 닿음"
    );
    assert!(b.ok::<m::FriendRequests>(Empty).await.is_empty());
}

/// 링크 요청을 보내는 도중 링크 주인과 연결되면, 연결 때의 다시 보내기는 겹쳐 보내기 막기에 걸려 건너뜁니다. 보내던
/// 시도가 끝나면 곧바로 다시 보내 다음 백오프를 기다리지 않습니다(PC CI의 앱 종단 간 시험에서 친구 요청이 2분 안에
/// 닿지 않던 경쟁, 2026-10-21).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn contact_link_resent_when_owner_connects_mid_attempt() {
    let cl = Cluster::new();
    // 기본 다시 걸기 간격(60초)이라 주기 다시 보내기는 시험 안에 돌지 않습니다.
    let a = cl.node("a").await;
    let mut b = cl.node("b").await;
    connect(&a, &b).await;
    let link = b
        .ok::<m::FriendLinkCreate>(FriendLinkCreateParams {
            expires_in_secs: Some(3600),
            single_use: Some(true),
        })
        .await;
    b.shutdown().await;
    a.ok::<m::FriendAdd>(UrlParams { url: link.url }).await;
    assert_eq!(testkit::pending_redeems(&a.node), 1);

    let held = testkit::hold_friend_sends(&a.node);
    b.start_again().await;
    connect(&a, &b).await;
    eventually(
        "연결 때 다시 보내기가 보내는 중이라 건너뜀",
        || async { testkit::friend_resend_wanted(&a.node).then_some(()) },
    )
    .await;
    let released = std::time::Instant::now();
    drop(held);
    let a_user = a.user();
    eventually("b에게 요청이 닿음", || async {
        b.ok::<m::FriendList>(Empty)
            .await
            .iter()
            .any(|f| f.user == a_user)
            .then_some(())
    })
    .await;
    assert!(
        released.elapsed() < std::time::Duration::from_secs(30),
        "다음 백오프까지 기다림: {:?}",
        released.elapsed()
    );
    assert_eq!(testkit::pending_redeems(&a.node), 0);
}

/// 서로 상대의 연락처 링크를 동시에 써도 1:1 공간은 하나로 맞춰지고, 남은 요청·다시 보내기가 없습니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mutual_link_redeems_leave_one_space() {
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let b = cl.node("b").await;
    connect_both(&a, &b).await;
    let params = || FriendLinkCreateParams {
        expires_in_secs: Some(3600),
        single_use: Some(true),
    };
    let la = a.ok::<m::FriendLinkCreate>(params()).await;
    let lb = b.ok::<m::FriendLinkCreate>(params()).await;
    let (ra, rb) = tokio::join!(
        a.ok::<m::FriendAdd>(UrlParams { url: lb.url }),
        b.ok::<m::FriendAdd>(UrlParams { url: la.url })
    );
    assert_eq!((ra.user, rb.user), (b.user(), a.user()));
    let (a_user, b_user) = (a.user(), b.user());
    let settled = |n: &TestNode, other: UserId| {
        let node = n.node.clone();
        async move {
            let f = call::<m::FriendList>(&node, Empty)
                .await
                .ok()?
                .into_iter()
                .find(|f| f.user == other && f.kind == ContactKind::Temporary)?;
            let spaces = call::<m::SpaceList>(&node, Empty).await.ok()?;
            let idle = call::<m::FriendRequests>(&node, Empty)
                .await
                .ok()?
                .is_empty()
                && testkit::pending_redeems(&node) == 0
                && testkit::pending_accepts(&node) == 0
                && testkit::outgoing_requests(&node) == 0;
            (spaces.len() == 1 && idle).then_some(f.space)
        }
    };
    let (sa, sb) = eventually("둘 다 같은 1:1 공간 하나로", || async {
        let sa = settled(&a, b_user).await?;
        let sb = settled(&b, a_user).await?;
        (sa == sb).then_some((sa, sb))
    })
    .await;
    assert_eq!(sa, sb);
    eventually("크루 키 순번", || async {
        let ea = testkit::crew_key_seq(&a.node, &sa)?;
        let eb = testkit::crew_key_seq(&b.node, &sa)?;
        (ea == eb).then_some(())
    })
    .await;
    let channel = a.first_channel(sa).await;
    let id = a.send(sa, channel, "하나로 맞춘 대화").await;
    wait_message(&b, sa, channel, id, "b가 받음", |_| true).await;
}

/// 서로 링크를 쓴 뒤 같은 친구 수락이 겹쳐 오거나, 겹친 1:1 공간이라 이미 지운 공간의 수락이 다시 와도 받는 쪽은
/// 처리한 것으로 답합니다. 거절(모르는 요청·1:1 공간이 아님)로 답하면 보낸 쪽이 수락을 되돌려 친구 기록·공간이
/// 갈립니다. 붙잡았던 수락을 풀면 둘 다 같은 공간 하나로 맞춰집니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repeated_accepts_for_a_discarded_duplicate_are_ok() {
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let b = cl.node("b").await;
    connect_both(&a, &b).await;
    testkit::hold_friend_accepts(&a.node, true);
    testkit::hold_friend_accepts(&b.node, true);
    let params = || FriendLinkCreateParams {
        expires_in_secs: Some(3600),
        single_use: Some(true),
    };
    let la = a.ok::<m::FriendLinkCreate>(params()).await;
    let lb = b.ok::<m::FriendLinkCreate>(params()).await;
    let (ra, rb) = tokio::join!(
        a.ok::<m::FriendAdd>(UrlParams { url: lb.url }),
        b.ok::<m::FriendAdd>(UrlParams { url: la.url })
    );
    assert_eq!((ra.user, rb.user), (b.user(), a.user()));
    let (a_user, b_user) = (a.user(), b.user());
    let own_space = |n: &TestNode, other: UserId| {
        let node = n.node.clone();
        async move {
            if testkit::pending_accepts(&node) != 1 {
                return None;
            }
            call::<m::FriendList>(&node, Empty)
                .await
                .ok()?
                .into_iter()
                .find(|f| f.user == other)
                .map(|f| f.space)
        }
    };
    let (sa, sb) = eventually("둘 다 1:1 공간을 만들고 수락을 남김", || async {
        Some((own_space(&a, b_user).await?, own_space(&b, a_user).await?))
    })
    .await;
    assert_ne!(sa, sb);
    // 큰 ID의 공간을 만든 쪽(x)의 수락은 받는 쪽(y)이 처리하자마자 겹친 공간으로 지웁니다.
    let (x, y, big) = if sa > sb { (&a, &b, sa) } else { (&b, &a, sb) };
    let (peer, req) = testkit::pending_accept_request(&x.node, big).expect("남은 수락");
    assert_eq!(peer, y.peer());
    let (r1, r2) = tokio::join!(
        testkit::net_request(&x.node, peer, req.clone()),
        testkit::net_request(&x.node, peer, req.clone())
    );
    assert!(matches!(r1, Ok(YpResponse::Ok)), "겹친 첫 수락: {r1:?}");
    assert!(matches!(r2, Ok(YpResponse::Ok)), "겹친 둘째 수락: {r2:?}");
    assert!(
        !testkit::known_spaces(&y.node).contains(&big),
        "겹친 공간은 지움"
    );
    let r3 = testkit::net_request(&x.node, peer, req).await;
    assert!(
        matches!(r3, Ok(YpResponse::Ok)),
        "지운 공간의 수락 재전송: {r3:?}"
    );
    assert!(
        !testkit::known_spaces(&y.node).contains(&big),
        "다시 만들지 않음"
    );

    testkit::hold_friend_accepts(&a.node, false);
    testkit::hold_friend_accepts(&b.node, false);
    let settled = |n: &TestNode, other: UserId| {
        let node = n.node.clone();
        async move {
            let f = call::<m::FriendList>(&node, Empty)
                .await
                .ok()?
                .into_iter()
                .find(|f| f.user == other && f.kind == ContactKind::Temporary)?;
            let spaces = call::<m::SpaceList>(&node, Empty).await.ok()?;
            let idle = call::<m::FriendRequests>(&node, Empty)
                .await
                .ok()?
                .is_empty()
                && testkit::pending_redeems(&node) == 0
                && testkit::pending_accepts(&node) == 0
                && testkit::outgoing_requests(&node) == 0;
            (spaces.len() == 1 && idle).then_some(f.space)
        }
    };
    let small = sa.min(sb);
    eventually("둘 다 작은 ID의 1:1 공간 하나로", || async {
        let sa = settled(&a, b_user).await?;
        let sb = settled(&b, a_user).await?;
        (sa == small && sb == small).then_some(())
    })
    .await;
}

/// 임시 연락처 둘이 떨어져 있는 사이 각자 친구 추가를 눌러도, 다시 만나면 둘 다 친구가 되고 남은 요청이 없습니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crossing_proposals_while_apart_make_friends() {
    let cl = Cluster::new();
    let mut a = cl.node_with("a", fast_redial).await;
    let mut b = cl.node_with("b", fast_redial).await;
    meet_by_link(&a, &b, Some(3600)).await;
    let (a_user, b_user) = (a.user(), b.user());

    // b가 꺼진 사이 a가 누르고, a가 꺼진 사이 b가 누릅니다(둘 다 상대 요청을 받기 전).
    b.shutdown().await;
    a.ok::<m::FriendPropose>(UserParams { user: b_user }).await;
    a.shutdown().await;
    b.start_again().await;
    b.ok::<m::FriendPropose>(UserParams { user: a_user }).await;
    a.start_again().await;

    for (n, other) in [(&a, b_user), (&b, a_user)] {
        eventually("다시 만나 둘 다 친구", || async {
            n.ok::<m::FriendList>(Empty)
                .await
                .into_iter()
                .any(|f| f.user == other && f.kind == ContactKind::Friend && !f.proposed)
                .then_some(())
        })
        .await;
    }
    eventually("남은 요청이 없음", || async {
        (a.ok::<m::FriendRequests>(Empty).await.is_empty()
            && b.ok::<m::FriendRequests>(Empty).await.is_empty())
        .then_some(())
    })
    .await;
}

/// Tor로 도는 상태: onion 주소만 듣고, 시스템 상태에도 켜짐으로 나옵니다. 지금 PeerId를 돌려줍니다.
async fn assert_tor(t: &TestNode) -> PeerId {
    let s = eventually("onion 주소", || async {
        let s = t.ok::<m::NetStatusGet>(Empty).await;
        s.onion_addr.is_some().then_some(s)
    })
    .await;
    assert_eq!(s.mode, NetMode::Tor);
    for x in s.listen_addrs.iter().chain(&s.external_addrs) {
        let ma: Multiaddr = x.parse().expect("multiaddr");
        assert!(yangpa_net::addr::is_onion(&ma), "Tor 모드에서 IP 주소: {x}");
    }
    assert_eq!(s.peer_id, t.peer().to_string());
    let st = t.ok::<m::SystemStatus>(Empty).await;
    assert!(st.tor.enabled);
    t.peer()
}

fn tor(c: &mut NodeConfig) {
    c.net.direct = false;
}

/// 예전 빌드의 일반 모드로 쓰던 기기는 Tor로 다시 뜹니다: 예전 모드·토르망 설정, 없어진 망 설정(IP 숨기기·저전력·
/// 상시 노드·공개 노드·통화 STUN·TURN)과 일반 모드 네트워크 키를 지우고 Tor 네트워크 키(다른 PeerId)로 돌며, 다시
/// 띄워도 그대로입니다. Tor에서는 IP 주소로 직접 걸지 않습니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn legacy_normal_mode_device_moves_to_tor() {
    let cl = Cluster::new();
    let n = cl.node("n").await;
    let mut t = cl.node("t").await;
    let clear_peer = t.peer();
    let n_addrs = listen_addrs(&n.node).await;
    const LEGACY: [&str; 8] = [
        "net.tor",
        "net.tor_open",
        "relay.sticky",
        "net.hide_ip",
        "net.low_power",
        "net.bootstrap",
        "net.public_nodes",
        "net.ice_servers",
    ];
    for key in LEGACY {
        tnet::put_legacy_setting(&t.node, key, false).expect("설정 저장");
    }
    assert!(tnet::has_direct_net_key(&t.node));

    t.restart_with(tor).await;
    let tor_peer = assert_tor(&t).await;
    assert_ne!(tor_peer, clear_peer, "Tor는 다른 네트워크 키");
    for key in LEGACY {
        assert!(!tnet::has_setting(&t.node, key), "{key}가 남음");
    }
    // 없어진 설정의 메서드는 더 없습니다.
    for method in [
        "net.set_hide_ip",
        "net.set_low_power",
        "net.add_bootstrap",
        "net.bootstrap_list",
        "net.set_public_nodes",
        "net.ice_servers",
        "net.set_ice_servers",
    ] {
        let e = t
            .node
            .call(&CallCtx::owner(), method, serde_json::json!({}))
            .await
            .unwrap_err();
        assert_eq!(e.code, yangpa_rpc::jsonrpc::METHOD_NOT_FOUND, "{method}");
    }
    assert!(
        !tnet::has_direct_net_key(&t.node),
        "일반 모드 네트워크 키가 남음"
    );
    assert!(
        t.node.connect(n.peer(), n_addrs).await.is_err(),
        "Tor에서 IP 주소로 직접 연결됨"
    );
    let seen = n.ok::<m::NetPeers>(Empty).await;
    assert!(
        !seen
            .iter()
            .any(|p| p.peer_id == tor_peer.to_string() || p.peer_id == clear_peer.to_string()),
        "{seen:?}"
    );
    t.restart().await;
    assert_eq!(assert_tor(&t).await, tor_peer, "다시 띄워도 같은 Tor 노드");
}

/// Tor를 준비할 수 없으면 직접 연결로 조용히 넘어가지 않고 시작을 거절합니다. Tor로 도는 기기의 P2P 전용이 아닌
/// 채널 통화는 WebRTC 없이 Tor 통화로 합니다(ICE 서버를 주지 않음).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tor_unavailable_never_falls_back_to_clearnet() {
    let cl = Cluster::new();
    let mut t = cl.node("t").await;
    t.shutdown().await;
    // Arti 상태 디렉터리 자리에 파일을 두어 Tor를 준비할 수 없게 합니다.
    let arti = t.dir.join("arti");
    std::fs::write(&arti, b"not a directory").expect("파일");
    let mut cfg = NodeConfig::for_test(&t.dir);
    tor(&mut cfg);
    assert!(
        Node::start(cfg).await.is_err(),
        "Tor 없이 직접 연결로 시작함"
    );

    // 고치면 Tor로 뜹니다.
    std::fs::remove_file(&arti).expect("파일 지움");
    t.restart_with(tor).await;
    assert_tor(&t).await;

    let space = tnet::create_crew(&t.node, &[]).await.expect("크루");
    let channel = tnet::create_voice_channel(&t.node, space, "음성")
        .await
        .expect("통화 채널");
    let j = t
        .ok::<m::VoiceJoin>(SpaceChannelParams { space, channel })
        .await;
    assert_eq!(j.topology, yangpa_rpc::api::Topology::Tor);
    assert!(j.ice_servers.is_empty());
    assert!(tnet::voice_call(&t.node, &space, &channel).is_some_and(|v| v.joined));
}

/// 실제 Tor 망에서 두 노드가 onion 주소로 연결해 친구가 되고 메시지를 주고받습니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "실제 Tor 망 접속이 필요하고 onion 서비스 게시에 몇 분 걸릴 수 있음"]
async fn real_tor_friends_over_onion() {
    let cl = Cluster::new();
    let a = cl.node_with("a", tor).await;
    let b = cl.node_with("b", tor).await;
    assert_tor(&a).await;
    assert_tor(&b).await;
    let a_addrs = listen_addrs(&a.node).await;
    let b_addrs = listen_addrs(&b.node).await;
    eventually("a → b onion 연결", || async {
        a.node.connect(b.peer(), b_addrs.clone()).await.ok()
    })
    .await;
    eventually("b → a onion 연결", || async {
        b.node.connect(a.peer(), a_addrs.clone()).await.ok()
    })
    .await;
    let space = befriend(&a, &b).await;
    let channel = a.first_channel(space).await;
    let id = a.send(space, channel, "onion으로").await;
    wait_message(&b, space, channel, id, "Tor로 메시지 도착", |_| true).await;
}

/// 연락처 요청 하나를 보내고 받은 답(노드 카드 포함)을 돌려줍니다.
async fn contact_reply(from: &TestNode, to: &TestNode, nodes: Vec<NodeContact>) -> SocialReply {
    let req = YpRequest::Social(SocialRequest::Contact {
        addrs: Vec::new(),
        nodes,
    });
    match testkit::net_request(&from.node, to.peer(), req)
        .await
        .expect("연락처 요청")
    {
        YpResponse::Social(r) => r,
        r => panic!("연락처 답이 아님: {r:?}"),
    }
}

/// 노드 카드(PeerId와 onion 주소)는 친구·내 기기와만 주고받습니다. 친구가 다른 PeerId(예전 빌드의 다른 모드 노드)를
/// 실어 보내면 그 기기의 예전 노드로 기억해 그 노드로 돌아와도 알아보고, 크루에만 함께 있는 사람은 받지도 넣지도
/// 못합니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn node_contacts_only_between_friends() {
    let cl = Cluster::new();
    let a = cl.node("a").await;
    let b = cl.node("b").await;
    let c = cl.node("c").await;
    let cards = testkit::node_contacts(&a.node).await;
    assert_eq!(cards.len(), 1);
    assert_eq!(PeerId::from_bytes(&cards[0].peer_id).ok(), Some(a.peer()));
    assert_eq!(cards[0].onion, None, "직접 연결 스웜은 onion이 없음");

    befriend(&a, &b).await;
    assert_eq!(contact_reply(&b, &a, Vec::new()).await.nodes.len(), 1);
    // b가 예전 노드(onion 주소와 함께)를 실어 보내면 a는 b 기기의 예전 노드로 기억합니다.
    let b_old = PeerId::random();
    let onion = format!("{}.onion:4001", "b".repeat(56));
    contact_reply(
        &b,
        &a,
        vec![NodeContact {
            peer_id: b_old.to_bytes(),
            onion: Some(onion),
        }],
    )
    .await;
    assert_eq!(testkit::prev_peer_device(&a.node, &b_old), Some(dev(&b)));
    assert!(
        testkit::peer_addrs(&a.node, &b_old)
            .iter()
            .any(|x| x.contains("/onion3/")),
        "예전 노드의 onion 주소도 기억"
    );

    // 크루에만 함께 있는 c: a의 노드를 받지 못하고, c가 보낸 노드도 기록되지 않습니다.
    testkit::seed_crew(&a.node, std::slice::from_ref(&c.node), "크루")
        .await
        .expect("크루 만들기");
    connect_both(&a, &c).await;
    testkit::authenticate(&c.node, a.peer())
        .await
        .expect("크루 멤버끼리 인증");
    let fake = PeerId::random();
    let r = contact_reply(
        &c,
        &a,
        vec![NodeContact {
            peer_id: fake.to_bytes(),
            onion: None,
        }],
    )
    .await;
    assert!(r.nodes.is_empty(), "{:?}", r.nodes);
    assert!(testkit::prev_peer_device(&a.node, &fake).is_none());
}

/// 실제 Tor 망: 함께 쓰는 노드 없는 두 친구가 둘 다 껐다 켜도 서로의 onion으로 다시 만나 대화가 이어집니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "실제 Tor 망 접속이 필요하고 onion 서비스 게시에 몇 분 걸릴 수 있음"]
async fn real_tor_friends_meet_again_after_restart() {
    fn lone(c: &mut NodeConfig) {
        tor(c);
        fast_redial(c);
    }
    let cl = Cluster::new();
    let mut a = cl.node_with("a", lone).await;
    let mut b = cl.node_with("b", lone).await;
    let space = befriend(&a, &b).await;
    for (x, y) in [(&a, &b), (&b, &a)] {
        eventually("상대 onion 주소를 기억", || async {
            testkit::peer_addrs(&x.node, &y.peer())
                .iter()
                .any(|s| s.contains("/onion3/"))
                .then_some(())
        })
        .await;
    }
    a.shutdown().await;
    b.shutdown().await;
    b.start_on_new_port().await;
    a.start_on_new_port().await;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(900);
    while !(tnet::is_authed(&a.node, &b.peer()) && tnet::is_authed(&b.node, &a.peer())) {
        assert!(
            std::time::Instant::now() < deadline,
            "onion으로 다시 만나지 못함"
        );
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    let channel = a.first_channel(space).await;
    let id = a.send(space, channel, "onion으로 다시 만남").await;
    wait_message(&b, space, channel, id, "b가 받음", |_| true).await;
}

/// 실제 Tor 망 측정: 친구 b가 켜져 있을 때 a만 다시 켜서, 단계마다(Tor 준비, 연결, 서로 인증) 걸린 시간을 찍습니다.
/// 같은 프로세스에서 다시 띄우므로 a의 Arti는 상태 파일을 읽기 전용으로 씁니다(회로 시간 학습값을 못 남김).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "실제 Tor 망 측정"]
async fn real_tor_restart_timing() {
    fn late(c: &mut NodeConfig) {
        tor(c);
        c.redial_interval = std::time::Duration::from_secs(3600);
    }
    let cl = Cluster::new();
    let mut a = cl.node_with("a", late).await;
    let b = cl.node_with("b", late).await;
    befriend(&a, &b).await;
    eventually("a가 b onion 주소를 기억", || async {
        testkit::peer_addrs(&a.node, &b.peer())
            .iter()
            .any(|s| s.contains("/onion3/"))
            .then_some(())
    })
    .await;
    for round in 0..3 {
        a.shutdown().await;
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        let t0 = std::time::Instant::now();
        a.start_on_new_port().await;
        let started = t0.elapsed();
        let (mut ready, mut conn, mut a_auth, mut b_auth) = (None, None, None, None);
        while a_auth.is_none() || b_auth.is_none() {
            let t = t0.elapsed();
            assert!(t < std::time::Duration::from_secs(300), "다시 만나지 못함");
            if ready.is_none() && testkit::net_status(&a.node).tor_bootstrap == Some(100) {
                ready = Some(t);
            }
            if conn.is_none() && testkit::is_connected(&a.node, b.peer()) {
                conn = Some(t);
            }
            if a_auth.is_none() && tnet::is_authed(&a.node, &b.peer()) {
                a_auth = Some(t);
            }
            if b_auth.is_none() && tnet::is_authed(&b.node, &a.peer()) {
                b_auth = Some(t);
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        eprintln!(
            "측정 {round}: 시작 {started:?} · Tor 준비 {ready:?} · 연결 {conn:?} · a가 b 인증 {a_auth:?} · b가 a 인증 {b_auth:?}"
        );
    }
}
