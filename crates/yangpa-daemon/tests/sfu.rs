//! SFU 통화(X2) 시험: 데몬들이 같은 SFU 호스트를 고르고(히스테리시스·넘겨주기), str0m 참가자가 앱 대신 데몬 신호
//! 경로와 실제 127.0.0.1 UDP로 호스트에 붙어 미디어를 주고받는다. 시험에서는 참가자 3명부터 SFU, 1명이 되면
//! 메시로 기준을 낮춥니다(`mesh_max = 2`, `mesh_min = 1`). 통화 밖 시더는 그 채널의 `manage_voice`가 있어야
//! 호스트로 나섭니다.

use crate::common;

use std::net::SocketAddr;
use std::time::Instant;

use base64::Engine;
use common::{Cluster, TestNode, connect_both, eventually, next_notification};
use yangpa_core::ids::{CallId, ChannelId, DeviceId, SpaceId};
use yangpa_core::model::Permissions;
use yangpa_core::wire::{
    SfuCaps, SfuLayer, SignalPayload, TrackSource, WireErrorCode, YpRequest, YpResponse,
};
use yangpa_daemon::testkit;
use yangpa_daemon::testkit::net as tnet;
use yangpa_daemon::testkit::sfu::{self as tsfu, MediaClient, TrackSpec};
use yangpa_daemon::{NodeConfig, SfuHostMode};
use yangpa_rpc::api::{
    AuditListParams, Notification, SpaceChannelParams, SpaceParams, StunServersParams, Topology,
    TrustSetUserParams, VoiceJoinResult, VoiceMediaParams, VoiceSignalParams, m,
};

fn small(cfg: &mut NodeConfig) {
    cfg.sfu.mesh_max = 2;
    cfg.sfu.mesh_min = 1;
}

/// 참가하지 않고 SFU만 맡겠다는 노드(헤드리스 시더).
fn seeder(cfg: &mut NodeConfig) {
    small(cfg);
    cfg.sfu.host = SfuHostMode::Prefer;
}

/// 둘부터 SFU, 혼자면 메시.
fn pair(cfg: &mut NodeConfig) {
    cfg.sfu.mesh_max = 1;
    cfg.sfu.mesh_min = 1;
}

fn dev(n: &TestNode) -> DeviceId {
    testkit::device_id(&n.node).expect("기기 ID")
}

fn specs() -> Vec<TrackSpec> {
    vec![
        TrackSpec::audio(),
        TrackSpec::simulcast(TrackSource::Camera, [200, 600, 1500]),
    ]
}

/// 모두 연결·인증하고 크루와 통화 채널을 만들어 전파합니다(첫 노드가 주인).
async fn setup(nodes: &[&TestNode]) -> (SpaceId, ChannelId) {
    for (i, x) in nodes.iter().enumerate() {
        for y in &nodes[i + 1..] {
            connect_both(x, y).await;
        }
    }
    let others: Vec<_> = nodes[1..].iter().map(|n| n.node.clone()).collect();
    let space = tnet::create_crew(&nodes[0].node, &others)
        .await
        .expect("크루 만들기");
    for (i, x) in nodes.iter().enumerate() {
        for y in &nodes[i + 1..] {
            testkit::authenticate(&x.node, y.peer())
                .await
                .expect("멤버끼리 인증");
        }
    }
    let channel = tnet::create_voice_channel(&nodes[0].node, space, "음성")
        .await
        .expect("통화 채널");
    for n in nodes {
        eventually("통화 채널 전파", || async {
            n.ok::<m::ChannelList>(SpaceParams { space })
                .await
                .iter()
                .any(|c| c.channel == channel)
                .then_some(())
        })
        .await;
    }
    (space, channel)
}

/// 통화 채널 하나를 더 만들어 모두에게 전파합니다.
async fn another_channel(nodes: &[&TestNode], space: SpaceId, name: &str) -> ChannelId {
    let channel = tnet::create_voice_channel(&nodes[0].node, space, name)
        .await
        .expect("통화 채널");
    for n in nodes {
        eventually("통화 채널 전파", || async {
            n.ok::<m::ChannelList>(SpaceParams { space })
                .await
                .iter()
                .any(|c| c.channel == channel)
                .then_some(())
        })
        .await;
    }
    channel
}

/// 주인(`nodes[0]`)이 `host`에게 그 채널의 `manage_voice`를 주고(통화 밖 호스트 자격), 모두가 알 때까지 기다립니다.
async fn grant_hosting(nodes: &[&TestNode], host: &TestNode, space: SpaceId, channel: ChannelId) {
    tnet::allow_in_channel(
        &nodes[0].node,
        space,
        channel,
        host.user(),
        Permissions::MANAGE_VOICE,
    )
    .await
    .expect("음성 관리 권한 주기");
    for n in nodes {
        eventually("음성 관리 권한 전파", || async {
            testkit::space_state(&n.node, &space)?
                .channel_permissions(&host.user(), &channel)
                .contains(Permissions::MANAGE_VOICE)
                .then_some(())
        })
        .await;
    }
}

/// SFU 소켓이 모두 닫혀 같은 주소를 다시 열 수 있는지.
fn assert_released(addrs: &[SocketAddr]) {
    assert!(!addrs.is_empty());
    for a in addrs {
        assert!(
            std::net::UdpSocket::bind(a).is_ok(),
            "멈춘 SFU의 소켓이 아직 열려 있음"
        );
    }
}

async fn join(n: &TestNode, space: SpaceId, channel: ChannelId) -> VoiceJoinResult {
    n.ok::<m::VoiceJoin>(SpaceChannelParams { space, channel })
        .await
}

/// 모두가 같은 토폴로지·호스트를 알리고 그것이 `want`를 만족할 때까지 기다려 호스트를 돌려줍니다.
async fn agree(
    nodes: &[&TestNode],
    space: SpaceId,
    channel: ChannelId,
    what: &str,
    want: impl Fn(Topology, Option<DeviceId>) -> bool,
) -> Option<DeviceId> {
    eventually(what, || async {
        let mut seen: Option<Option<DeviceId>> = None;
        for n in nodes {
            let (t, h) = tsfu::topology(&n.node, &space, &channel)?;
            if !want(t, h) {
                return None;
            }
            match seen {
                None => seen = Some(h),
                Some(x) if x == h => {}
                Some(_) => return None,
            }
        }
        seen
    })
    .await
}

/// 참가자마다 다른 참가자의 오디오·카메라 프레임을 `since` 뒤로 받을 때까지.
async fn wait_media(clients: &[&MediaClient], since: Instant, what: &str) {
    eventually(what, || async {
        for x in clients {
            for y in clients {
                if x.device == y.device {
                    continue;
                }
                if x.frames_since(y.device, TrackSource::Audio, since) < 10
                    || x.frames_since(y.device, TrackSource::Camera, since) < 5
                {
                    return None;
                }
            }
        }
        Some(())
    })
    .await;
}

/// 받은 프레임이 모두 보낸 쪽이 보낸 바이트 그대로인지(SFU·데몬이 E2EE 암호문을 풀거나 바꾸지 않음).
fn assert_unchanged(clients: &[&MediaClient]) {
    let mut checked = 0;
    for x in clients {
        let received: Vec<(DeviceId, TrackSource, Vec<u8>)> = x.with(|s| {
            s.peer
                .received
                .iter()
                .map(|r| (r.origin, r.source, r.data.clone()))
                .collect()
        });
        for (origin, source, data) in received {
            let Some(o) = clients.iter().find(|c| c.device == origin) else {
                continue;
            };
            let sent = o.with(|s| {
                s.peer
                    .sent
                    .get(&source)
                    .is_some_and(|set| set.contains(&data))
            });
            assert!(sent, "{source:?} 프레임이 보낸 바이트와 다름");
            checked += 1;
        }
    }
    assert!(checked > 0);
}

fn node_of<'a>(nodes: &[&'a TestNode], d: DeviceId) -> &'a TestNode {
    nodes.iter().find(|n| dev(n) == d).expect("호스트 노드")
}

/// 셋째 참가자가 들어오면 모두 같은 호스트로 SFU에 동의하고, 미디어가 바이트 그대로 오가며, 층을 고를 수 있다.
/// 호스트가 나가면 남은 참가자 중 하나로 넘겨받고, 혼자 남으면 메시, 다시 둘이 되어도 메시(히스테리시스).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn election_forwarding_layers_and_handover() {
    let cl = Cluster::new();
    let a = cl.node_with("a", small).await;
    let b = cl.node_with("b", small).await;
    let c = cl.node_with("c", small).await;
    let all = [&a, &b, &c];
    let (space, channel) = setup(&all).await;

    join(&a, space, channel).await;
    let jb = join(&b, space, channel).await;
    assert_eq!((jb.topology, jb.sfu_host), (Topology::Mesh, None));
    agree(&all[..2], space, channel, "둘은 메시", |t, _| {
        t == Topology::Mesh
    })
    .await;
    // 셋째가 참가를 마칠 때 이미 SFU임을 압니다(관리 기기가 참가 알림에 답하기 전에 결정을 보냄).
    let jc = join(&c, space, channel).await;
    assert_eq!(jc.topology, Topology::Sfu);
    assert!(jc.sfu_host.is_some());
    let host = agree(
        &all,
        space,
        channel,
        "셋이 같은 SFU 호스트",
        |t, h| t == Topology::Sfu && h.is_some(),
    )
    .await
    .expect("호스트");
    let hn = node_of(&all, host);
    eventually("호스트가 통화방을 엶", || async {
        tsfu::hosting(&hn.node, &space, &channel).then_some(())
    })
    .await;
    for n in all.iter().filter(|n| dev(n) != host) {
        assert!(!tsfu::hosting(&n.node, &space, &channel));
    }

    let ca = MediaClient::start(&a.node, space, channel, specs())
        .await
        .expect("a 참가자");
    let cb = MediaClient::start(&b.node, space, channel, specs())
        .await
        .expect("b 참가자");
    let cc = MediaClient::start(&c.node, space, channel, specs())
        .await
        .expect("c 참가자");
    let start = Instant::now();
    wait_media(&[&ca, &cb, &cc], start, "셋이 서로의 미디어를 받음").await;
    assert_unchanged(&[&ca, &cb, &cc]);
    let sessions = tsfu::sessions(&hn.node, space, channel).await;
    assert_eq!(sessions.len(), 3);
    assert!(sessions.iter().all(|s| s.connected && s.receiving == 4));
    // SFU 쪽 연결 확인도 응답을 받음(실행기가 참가자의 STUN 응답을 STUN 서버 응답으로 알고 버리지 않음).
    eventually("SFU 쪽 ICE 확인이 성공", || async {
        let ss = tsfu::sessions(&hn.node, space, channel).await;
        (ss.len() == 3 && ss.iter().all(|s| s.ice_responses > 0)).then_some(())
    })
    .await;

    // a가 b의 카메라를 낮은 층으로, 그다음 높은 층으로 받기.
    let db = dev(&b);
    let last_layers = |n: usize| {
        ca.with(|s| {
            let frames = s.peer.frames_from(db, TrackSource::Camera);
            frames
                .iter()
                .rev()
                .take(n)
                .map(|r| r.layer())
                .collect::<Vec<_>>()
        })
    };
    ca.select(db, TrackSource::Camera, SfuLayer::Low);
    eventually("a가 b의 카메라를 낮은 층으로 받음", || async {
        let l = last_layers(5);
        (l.len() == 5 && l.iter().all(|x| *x == 0)).then_some(())
    })
    .await;
    ca.select(db, TrackSource::Camera, SfuLayer::High);
    eventually("a가 b의 카메라를 높은 층으로 받음", || async {
        let l = last_layers(5);
        (l.len() == 5 && l.iter().all(|x| *x == 2)).then_some(())
    })
    .await;

    // 호스트가 나가면 남은 둘 중 하나가 넘겨받습니다(둘은 mesh_min보다 많아 SFU 유지).
    let mut clients = vec![ca, cb, cc];
    let i = clients
        .iter()
        .position(|c| c.device == host)
        .expect("호스트의 참가자");
    clients.remove(i).stop();
    hn.ok::<m::VoiceLeave>(SpaceChannelParams { space, channel })
        .await;
    let rest: Vec<&TestNode> = all.iter().copied().filter(|n| dev(n) != host).collect();
    let next = agree(
        &rest,
        space,
        channel,
        "남은 둘이 새 호스트에 동의",
        |t, h| t == Topology::Sfu && h.is_some_and(|h| h != host),
    )
    .await
    .expect("새 호스트");
    eventually("나간 호스트가 통화방을 닫음", || async {
        (!tsfu::hosting(&hn.node, &space, &channel)).then_some(())
    })
    .await;
    let since = Instant::now();
    let refs: Vec<&MediaClient> = clients.iter().collect();
    wait_media(&refs, since, "넘겨받은 호스트로 다시 미디어").await;
    assert_unchanged(&refs);
    let nn = node_of(&rest, next);
    assert_eq!(tsfu::sessions(&nn.node, space, channel).await.len(), 2);

    // 혼자 남으면 메시, 다시 둘이 되어도 메시(SFU로 가려면 mesh_max를 넘어야 함).
    let other = *rest.iter().find(|n| dev(n) != next).expect("다른 참가자");
    let j = clients
        .iter()
        .position(|c| c.device == dev(other))
        .expect("참가자");
    clients.remove(j).stop();
    other
        .ok::<m::VoiceLeave>(SpaceChannelParams { space, channel })
        .await;
    agree(&[nn], space, channel, "혼자 남으면 메시", |t, _| {
        t == Topology::Mesh
    })
    .await;
    eventually("메시가 되면 통화방을 닫음", || async {
        (tsfu::hosted_rooms(&nn.node) == 0).then_some(())
    })
    .await;
    let again = join(other, space, channel).await;
    assert_eq!(again.topology, Topology::Mesh);
    agree(
        &[nn, other],
        space,
        channel,
        "다시 둘이어도 메시",
        |t, _| t == Topology::Mesh,
    )
    .await;
}

/// 먼저 맡겠다는 시더가(`manage_voice`를 받아) 참가하지 않고 호스트가 되며 프레임 키를 받지 않는다. 권한 없이
/// 호스트로 나서거나, 참가하지 않은 멤버나 남을 사칭한 신호는 거절되고, 통화 권한을 잃은 참가자의 세션은 닫힌다.
/// 결정을 놓친 호스트는 관리 기기가 다시 보내는 결정으로 따라잡는다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn seeder_hosts_and_non_participants_are_rejected() {
    let cl = Cluster::new();
    let a = cl.node_with("a", small).await;
    let b = cl.node_with("b", small).await;
    let c = cl.node_with("c", small).await;
    let s = cl.node_with("s", seeder).await;
    let x = cl.node_with("x", small).await;
    let everyone = [&a, &b, &c, &s, &x];
    let (space, channel) = setup(&everyone).await;
    grant_hosting(&everyone, &s, space, channel).await;
    let parts = [&a, &b, &c];
    for n in parts {
        join(n, space, channel).await;
    }
    let ds = dev(&s);
    agree(
        &parts,
        space,
        channel,
        "참가자가 시더를 호스트로",
        |t, h| t == Topology::Sfu && h == Some(ds),
    )
    .await;
    eventually("시더가 통화방을 엶", || async {
        tsfu::hosting(&s.node, &space, &channel).then_some(())
    })
    .await;
    // 앱이 누가 중계하는지 보여 줄 수 있게 호스트의 사용자도 알립니다.
    for n in parts {
        assert_eq!(tsfu::host_user(&n.node, &space, &channel), Some(s.user()));
    }
    let v = tnet::voice_call(&s.node, &space, &channel).expect("시더도 통화를 앎");
    assert!(!v.joined);
    assert_eq!(v.key_fingerprint, None, "시더는 프레임 키를 받지 않음");

    let ca = MediaClient::start(&a.node, space, channel, specs())
        .await
        .expect("a");
    let cb = MediaClient::start(&b.node, space, channel, specs())
        .await
        .expect("b");
    let cc = MediaClient::start(&c.node, space, channel, specs())
        .await
        .expect("c");
    wait_media(&[&ca, &cb, &cc], Instant::now(), "시더를 거쳐 미디어").await;
    assert_unchanged(&[&ca, &cb, &cc]);
    assert_eq!(tsfu::sessions(&s.node, space, channel).await.len(), 3);

    // 참가하지 않은 멤버는 호스트에 세션을 열 수 없습니다(데몬끼리도, 자기 앱에서도).
    let offer = SignalPayload::SfuOffer {
        session: 1,
        sdp: "v=0".into(),
        tracks: Vec::new(),
    };
    let req = |from: DeviceId| YpRequest::Signal {
        space,
        channel,
        call: CallId::random(),
        from,
        payload: offer.clone(),
    };
    let r = testkit::net_request(&x.node, s.peer(), req(dev(&x)))
        .await
        .expect("응답");
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
    // 참가자인 척(보낸 기기가 연결과 다름)도 거절.
    let r = testkit::net_request(&x.node, s.peer(), req(dev(&a)))
        .await
        .expect("응답");
    assert!(matches!(
        r,
        YpResponse::Error {
            code: WireErrorCode::BadRequest,
            ..
        }
    ));
    let call = tnet::voice_call(&a.node, &space, &channel).map(|_| ());
    assert!(call.is_some());
    assert!(
        x.call::<m::VoiceSignal>(VoiceSignalParams {
            space,
            channel,
            call: CallId::random(),
            to: ds,
            payload: offer.clone(),
        })
        .await
        .is_err()
    );
    // 참가자가 호스트가 아닌 기기로 SFU 신호를 보내도 거절.
    let a_call = a
        .ok::<m::VoiceJoin>(SpaceChannelParams { space, channel })
        .await
        .call;
    assert!(
        a.call::<m::VoiceSignal>(VoiceSignalParams {
            space,
            channel,
            call: a_call,
            to: dev(&b),
            payload: offer.clone(),
        })
        .await
        .is_err()
    );
    assert_eq!(tsfu::sessions(&s.node, space, channel).await.len(), 3);

    // `manage_voice`가 없는 멤버는 통화 밖 호스트 후보로 나설 수 없습니다(먼저 맡겠다는 조건이어도).
    let bold = SfuCaps {
        preferred: true,
        public: true,
        relayed: 0,
        rtt_ms: Some(1),
        capacity: 100,
    };
    let r = testkit::net_request(
        &x.node,
        a.peer(),
        YpRequest::Signal {
            space,
            channel,
            call: CallId::random(),
            from: dev(&x),
            payload: SignalPayload::SfuHost { caps: Some(bold) },
        },
    )
    .await
    .expect("응답");
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

    // c가 통화 권한을 잃으면 시더가 c의 세션을 닫습니다.
    tnet::deny_in_channel(
        &a.node,
        space,
        channel,
        c.user(),
        Permissions::CONNECT_VOICE,
    )
    .await
    .expect("권한 빼기");
    let dc = dev(&c);
    eventually("시더가 권한 잃은 c의 세션을 닫음", || async {
        let ss = tsfu::sessions(&s.node, space, channel).await;
        (ss.len() == 2 && ss.iter().all(|x| x.device != dc)).then_some(())
    })
    .await;
    let since = Instant::now();
    wait_media(&[&ca, &cb], since, "남은 둘은 계속 받음").await;
    agree(
        &[&a, &b],
        space,
        channel,
        "권한 없는 후보는 무시",
        |t, h| t == Topology::Sfu && h == Some(ds),
    )
    .await;

    // 결정을 놓친 호스트(통화방을 닫음)는 관리 기기가 다시 보내는 결정(`keepalive`마다)으로 따라잡습니다.
    tsfu::forget_plan(&s.node, space, channel);
    eventually("결정을 잊은 시더가 통화방을 닫음", || async {
        (!tsfu::hosting(&s.node, &space, &channel)).then_some(())
    })
    .await;
    for n in [&a, &b] {
        tsfu::resend_plans(&n.node);
    }
    eventually("다시 받은 결정으로 통화방을 엶", || async {
        tsfu::hosting(&s.node, &space, &channel).then_some(())
    })
    .await;
    let since = Instant::now();
    wait_media(&[&ca, &cb], since, "시더가 다시 중계").await;
}

/// Tor로 도는 노드는 호스트를 맡지 않는다(시더도 물러나 참가자에게 넘겨줌). 그만두는 호출이 돌아올 때는 미디어
/// 소켓이 이미 닫혀 있다(STUN 서버 목록의 응답을 기다리는 중이어도).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tor_mode_refuses_hosting() {
    let cl = Cluster::new();
    let a = cl.node_with("a", small).await;
    let b = cl.node_with("b", small).await;
    let c = cl.node_with("c", small).await;
    let s = cl.node_with("s", seeder).await;
    let everyone = [&a, &b, &c, &s];
    let (space, channel) = setup(&everyone).await;
    grant_hosting(&everyone, &s, space, channel).await;
    // 답하지 않는 STUN 서버: 시더의 실행기가 반사 주소를 기다리는 동안 멈추게 합니다.
    let stun = tokio::net::UdpSocket::bind(("127.0.0.1", 0))
        .await
        .expect("STUN 소켓");
    let stun_port = stun.local_addr().expect("STUN 주소").port();
    s.ok::<m::NetSetStunServers>(StunServersParams {
        servers: Some(vec![format!("127.0.0.1:{stun_port}")]),
    })
    .await;
    let parts = [&a, &b, &c];
    for n in parts {
        join(n, space, channel).await;
    }
    let ds = dev(&s);
    agree(
        &parts,
        space,
        channel,
        "처음에는 시더가 호스트",
        |t, h| t == Topology::Sfu && h == Some(ds),
    )
    .await;
    eventually("시더가 STUN 서버에 물음", || async {
        let mut buf = [0u8; 512];
        stun.try_recv_from(&mut buf).ok().map(|_| ())
    })
    .await;
    let addrs = tsfu::media_addrs(&s.node);

    // 시더를 Tor처럼 보게 하면(`enter_tor_policy`) SFU를 그만두고 참가자 중 하나가 넘겨받습니다.
    assert!(tsfu::can_host(&s.node));
    tsfu::enter_tor_policy(&s.node).await;
    assert!(!tsfu::can_host(&s.node));
    assert!(tsfu::media_addrs(&s.node).is_empty());
    assert_released(&addrs);
    assert_eq!(tsfu::hosted_rooms(&s.node), 0);
    let host = agree(
        &parts,
        space,
        channel,
        "Tor 모드 시더가 물러남",
        |t, h| t == Topology::Sfu && h.is_some_and(|h| h != ds),
    )
    .await
    .expect("새 호스트");
    assert!(!tsfu::announced(&s.node, &space, &channel));

    let hn = node_of(&parts, host);
    eventually("새 호스트가 통화방을 엶", || async {
        tsfu::hosting(&hn.node, &space, &channel).then_some(())
    })
    .await;

    // Tor로 보는 시더는 새 참가 알림에도, `keepalive` 다시 보기에도 나서지 않습니다.
    let other = *parts
        .iter()
        .find(|n| dev(n) != host)
        .expect("호스트가 아닌 참가자");
    let d_other = dev(other);
    other
        .ok::<m::VoiceLeave>(SpaceChannelParams { space, channel })
        .await;
    eventually("시더가 나간 참가자를 뺌", || async {
        tnet::voice_call(&s.node, &space, &channel)
            .is_some_and(|v| !v.devices.contains(&d_other))
            .then_some(())
    })
    .await;
    join(other, space, channel).await;
    eventually("시더가 다시 들어온 참가자를 봄", || async {
        tnet::voice_call(&s.node, &space, &channel)
            .is_some_and(|v| v.devices.contains(&d_other))
            .then_some(())
    })
    .await;
    tsfu::recheck(&s.node).await;
    assert!(!tsfu::announced(&s.node, &space, &channel));
    assert_eq!(tsfu::hosted_rooms(&s.node), 0);
    assert!(tsfu::media_addrs(&s.node).is_empty());
    agree(
        &parts,
        space,
        channel,
        "시더 없이 SFU 유지",
        |t, h| t == Topology::Sfu && h.is_some_and(|h| h != ds),
    )
    .await;
}

/// 통화 밖 시더는 `manage_voice`를 받은 채널에서만 나서고(이미 있던 통화에도 다시 보기로 나섬), 맡은 통화방의
/// 참가자 수만큼 다른 통화에 알리는 수용 인원을 줄인다. 앞 통화방이 닫히면 남은 수용 인원으로 다시 나서 넘겨받는다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unjoined_host_needs_permission_and_capacity_is_shared() {
    let cl = Cluster::new();
    let a = cl.node_with("a", pair).await;
    let b = cl.node_with("b", pair).await;
    let c = cl.node_with("c", pair).await;
    let d = cl.node_with("d", pair).await;
    let s = cl
        .node_with("s", |cfg| {
            pair(cfg);
            cfg.sfu.host = SfuHostMode::Prefer;
            cfg.sfu.capacity = 3;
        })
        .await;
    let everyone = [&a, &b, &c, &d, &s];
    let (space, ch1) = setup(&everyone).await;
    let ch2 = another_channel(&everyone, space, "음성 2").await;
    let ds = dev(&s);

    // 권한이 없으면 시더는 나서지 않고 참가자 중 하나가 맡습니다.
    let first = [&a, &b];
    for n in first {
        join(n, space, ch1).await;
    }
    let h1 = agree(
        &first,
        space,
        ch1,
        "권한 없는 시더 없이 SFU",
        |t, h| t == Topology::Sfu && h.is_some(),
    )
    .await
    .expect("호스트");
    assert!([dev(&a), dev(&b)].contains(&h1));
    eventually("시더가 통화를 앎", || async {
        tnet::voice_call(&s.node, &space, &ch1)
            .is_some_and(|v| v.devices.len() == 2)
            .then_some(())
    })
    .await;
    tsfu::recheck(&s.node).await;
    assert!(!tsfu::announced(&s.node, &space, &ch1));
    assert_eq!(tsfu::hosted_rooms(&s.node), 0);

    // 권한을 받으면 다음 다시 보기에서 이미 있던 통화에도 나서고, 먼저 맡겠다는 조건이라 넘겨받습니다.
    grant_hosting(&everyone, &s, space, ch1).await;
    grant_hosting(&everyone, &s, space, ch2).await;
    tsfu::recheck(&s.node).await;
    assert!(tsfu::announced(&s.node, &space, &ch1));
    agree(&first, space, ch1, "시더가 넘겨받음", |t, h| {
        t == Topology::Sfu && h == Some(ds)
    })
    .await;
    eventually("시더가 첫 통화방을 엶", || async {
        tsfu::hosting(&s.node, &space, &ch1).then_some(())
    })
    .await;

    // 수용 인원 3 중 둘을 첫 통화방이 쓰므로 둘째 통화에는 1로 알리고, 둘을 받을 수 없어 참가자가 맡습니다.
    let second = [&c, &d];
    for n in second {
        join(n, space, ch2).await;
    }
    eventually("시더가 둘째 통화에 나섬", || async {
        tsfu::announced(&s.node, &space, &ch2).then_some(())
    })
    .await;
    let h2 = agree(
        &second,
        space,
        ch2,
        "남은 수용 인원이 모자란 시더는 빠짐",
        |t, h| t == Topology::Sfu && h.is_some(),
    )
    .await
    .expect("호스트");
    assert!([dev(&c), dev(&d)].contains(&h2));
    assert!(!tsfu::hosting(&s.node, &space, &ch2));
    assert_eq!(tsfu::hosted_rooms(&s.node), 1);

    // 첫 통화가 끝나 통화방이 닫히면 시더가 늘어난 수용 인원을 알리고 둘째 통화를 넘겨받습니다.
    for n in first {
        n.ok::<m::VoiceLeave>(SpaceChannelParams {
            space,
            channel: ch1,
        })
        .await;
    }
    eventually("시더가 첫 통화방을 닫음", || async {
        (!tsfu::hosting(&s.node, &space, &ch1)).then_some(())
    })
    .await;
    agree(
        &second,
        space,
        ch2,
        "늘어난 수용 인원으로 시더가 넘겨받음",
        |t, h| t == Topology::Sfu && h == Some(ds),
    )
    .await;
    eventually("시더가 둘째 통화방을 엶", || async {
        (tsfu::hosting(&s.node, &space, &ch2) && tsfu::hosted_rooms(&s.node) == 1).then_some(())
    })
    .await;
}

/// Tor로 도는 기기의 일반 채널 통화(`Topology::Tor`): SFU를 고르지 않고(호스트를 맡을 수 있는 시더가 있어도), ICE
/// 서버를 내주지 않고, WebRTC 신호는 거절하며, 앱이 봉인한 소리 패킷만 노드 연결로 참가자에게 갑니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tor_calls_carry_sealed_packets_without_webrtc() {
    let cl = Cluster::new();
    let a = cl.node_with("a", pair).await;
    let b = cl.node_with("b", pair).await;
    let s = cl.node_with("s", seeder).await;
    let nodes = [&a, &b, &s];
    let (space, channel) = setup(&nodes).await;
    grant_hosting(&nodes, &s, space, channel).await;
    for n in [&a, &b] {
        tsfu::enter_tor_policy(&n.node).await;
    }
    let ja = join(&a, space, channel).await;
    let jb = join(&b, space, channel).await;
    for j in [&ja, &jb] {
        assert_eq!(j.topology, Topology::Tor);
        assert!(j.sfu_host.is_none());
        assert!(j.ice_servers.is_empty(), "Tor 통화에 ICE 서버를 줌");
    }
    eventually("둘이 서로를 참가자로 앎", || async {
        let both = [&a, &b].iter().all(|n| {
            tnet::voice_call(&n.node, &space, &channel)
                .is_some_and(|c| c.joined && c.devices.len() == 2)
        });
        both.then_some(())
    })
    .await;

    // 소리 패킷: A → B(보낸 기기가 함께 옴). 내용은 데몬이 보지 않고 그대로 넘깁니다.
    let mut rx = b.subscribe();
    let data = base64::engine::general_purpose::STANDARD.encode(b"sealed audio packet");
    a.ok::<m::VoiceMedia>(VoiceMediaParams {
        space,
        channel,
        call: ja.call,
        data: data.clone(),
        to: None,
    })
    .await;
    let got = next_notification(&mut rx, "B가 소리 패킷을 받음", |n| match n {
        Notification::VoiceMedia {
            from,
            data: d,
            call,
            ..
        } if *from == dev(&a) => Some((d.clone(), *call)),
        _ => None,
    })
    .await;
    assert_eq!(got, (data, jb.call));
    // 성공한 소리 패킷은 감사 로그(디스크)에 남기지 않습니다(말한 때가 남지 않게).
    let audit = a.ok::<m::AuditList>(AuditListParams::default()).await;
    assert!(
        !audit.is_empty() && audit.iter().all(|e| e.method != "voice.media"),
        "감사 로그에 소리 패킷이 남음"
    );

    // 너무 큰 패킷·다른 통화 ID·WebRTC 신호는 거절.
    let big = base64::engine::general_purpose::STANDARD.encode(vec![0u8; 16 * 1024]);
    let e = a
        .call::<m::VoiceMedia>(VoiceMediaParams {
            space,
            channel,
            call: ja.call,
            data: big,
            to: None,
        })
        .await
        .expect_err("큰 패킷");
    assert_eq!(e.code, yangpa_rpc::jsonrpc::ERR_INVALID_INPUT);
    let e = a
        .call::<m::VoiceSignal>(VoiceSignalParams {
            space,
            channel,
            call: ja.call,
            to: dev(&b),
            payload: SignalPayload::Offer { sdp: "v=0".into() },
        })
        .await
        .expect_err("Tor 통화의 WebRTC 신호");
    assert_eq!(e.code, yangpa_rpc::jsonrpc::ERR_CONFLICT);
    // 다른 노드가 직접 보낸 WebRTC 신호도 앱에 넘기지 않습니다.
    let r = testkit::net_request(
        &a.node,
        b.peer(),
        YpRequest::Signal {
            space,
            channel,
            call: ja.call,
            from: dev(&a),
            payload: SignalPayload::Offer { sdp: "v=0".into() },
        },
    )
    .await
    .expect("요청");
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

    // 둘뿐이라 기준(1명 넘으면 SFU)을 넘어도, 맡겠다는 시더가 있어도 SFU로 가지 않습니다.
    assert!(!tsfu::hosting(&s.node, &space, &channel));
    for n in [&a, &b] {
        assert_eq!(
            tsfu::topology(&n.node, &space, &channel),
            Some((Topology::Tor, None))
        );
        assert!(
            tsfu::plan_host(&n.node, &space, &channel).is_none(),
            "Tor 통화에 SFU 결정"
        );
    }
}

/// Tor 통화에서도 서로 신뢰한(신뢰 스위치) 참가자끼리는 WebRTC로 잇습니다(`VoiceParticipant.direct`): 신뢰가 있으면 ICE
/// 서버를 주고, WebRTC 신호는 내가 신뢰한 참가자와만 오갑니다. 한쪽만 신뢰하면 Tor 그대로입니다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn trusted_pairs_use_webrtc_in_tor_calls() {
    let cl = Cluster::new();
    let a = cl.node_with("a", pair).await;
    let b = cl.node_with("b", pair).await;
    let (space, channel) = setup(&[&a, &b]).await;
    for n in [&a, &b] {
        tsfu::enter_tor_policy(&n.node).await;
        n.ok::<m::NetSetStunServers>(StunServersParams {
            servers: Some(vec!["127.0.0.1:3478".into()]),
        })
        .await;
    }
    for (n, of) in [(&a, &b), (&b, &a)] {
        n.ok::<m::TrustSetUser>(TrustSetUserParams {
            user: of.user(),
            enabled: true,
        })
        .await;
    }
    let ja = join(&a, space, channel).await;
    let jb = join(&b, space, channel).await;
    for j in [&ja, &jb] {
        assert_eq!(j.topology, Topology::Tor);
        assert!(!j.ice_servers.is_empty(), "신뢰가 있으면 STUN을 줌");
    }
    for (n, of) in [(&a, &b), (&b, &a)] {
        eventually("서로 신뢰한 상대와 직접 연결", || async {
            (tsfu::direct(&n.node, &space, &channel) == vec![dev(of)]).then_some(())
        })
        .await;
    }
    let offer = SignalPayload::Offer { sdp: "v=0".into() };
    let mut rx = b.subscribe();
    a.ok::<m::VoiceSignal>(VoiceSignalParams {
        space,
        channel,
        call: ja.call,
        to: dev(&b),
        payload: offer.clone(),
    })
    .await;
    next_notification(&mut rx, "B 앱이 제안을 받음", |n| match n {
        Notification::VoiceSignal {
            from,
            payload: SignalPayload::Offer { .. },
            ..
        } if *from == dev(&a) => Some(()),
        _ => None,
    })
    .await;

    // B가 신뢰를 끄면 그 쌍은 Tor: B는 보내지 못하고, A가 보낸 것(A는 아직 B를 신뢰)은 B 데몬이 받지 않습니다.
    b.ok::<m::TrustSetUser>(TrustSetUserParams {
        user: a.user(),
        enabled: false,
    })
    .await;
    for n in [&a, &b] {
        eventually("한쪽만 신뢰하면 Tor", || async {
            tsfu::direct(&n.node, &space, &channel)
                .is_empty()
                .then_some(())
        })
        .await;
    }
    let e = b
        .call::<m::VoiceSignal>(VoiceSignalParams {
            space,
            channel,
            call: jb.call,
            to: dev(&a),
            payload: offer.clone(),
        })
        .await
        .expect_err("신뢰하지 않는 상대에게 WebRTC 신호");
    assert_eq!(e.code, yangpa_rpc::jsonrpc::ERR_CONFLICT);
    let r = testkit::net_request(
        &a.node,
        b.peer(),
        YpRequest::Signal {
            space,
            channel,
            call: ja.call,
            from: dev(&a),
            payload: offer,
        },
    )
    .await
    .expect("요청");
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
}
