//! SFU를 메모리 안에서 str0m 참가자들과 돌려 전달·층 선택·드나듦·거절을 확인합니다.

mod common;

use std::time::Duration;

use common::{Net, device};
use yangpa_core::wire::{SfuLayer, SfuTrack, SignalPayload, TrackSource};
use yangpa_sfu::testkit::TrackSpec;
use yangpa_sfu::{SfuConfig, SfuError, SfuEvent};

fn av() -> Vec<TrackSpec> {
    vec![TrackSpec::audio(), TrackSpec::camera()]
}

/// 받은 프레임이 모두 그 출처가 보낸 바이트 그대로인지.
fn assert_unchanged(net: &Net, receiver: u8, origin: u8, source: TrackSource) -> usize {
    let r = net.peer_ref(receiver);
    let o = net.peer_ref(origin);
    let frames = r.frames_from(o.device, source);
    let sent = o.sent.get(&source).expect("보낸 프레임");
    for f in &frames {
        assert!(
            sent.contains(&f.data),
            "{receiver}가 {origin}의 {source:?}에서 바뀐 프레임을 받음"
        );
    }
    frames.len()
}

#[test]
fn three_clients_forward_audio_and_video_unchanged() {
    let mut net = Net::new(SfuConfig::default());
    net.allow(&[device(1), device(2), device(3)]);
    for n in 1..=3 {
        net.join(n, av());
    }
    net.run_until(Duration::from_secs(10), "모두 연결", |n| {
        n.all_connected() && (1..=3).all(|i| n.sfu_receiving(i) == 4)
    });
    net.set_media(true);
    net.run_for(Duration::from_secs(3));

    for r in 1..=3u8 {
        for o in 1..=3u8 {
            if r == o {
                assert!(
                    net.peer_ref(r)
                        .received
                        .iter()
                        .all(|f| f.origin != device(r)),
                    "자기 트랙은 돌려받지 않음"
                );
                continue;
            }
            let audio = assert_unchanged(&net, r, o, TrackSource::Audio);
            let video = assert_unchanged(&net, r, o, TrackSource::Camera);
            // 3초 동안 오디오 50fps, 영상 33fps 중 대부분이 와야 함.
            assert!(audio > 100, "{r} ← {o} 오디오 {audio}");
            assert!(video > 60, "{r} ← {o} 영상 {video}");
        }
    }
    // 영상은 키프레임부터.
    for r in 1..=3u8 {
        for o in (1..=3u8).filter(|o| *o != r) {
            let frames = net.peer_ref(r).frames_from(device(o), TrackSource::Camera);
            assert!(frames[0].keyframe, "{r} ← {o} 첫 영상이 키프레임이 아님");
        }
    }
    assert!(net.rejected.is_empty(), "{:?}", net.rejected);
    assert!(net.stray_signals.is_empty());
    // SFU 쪽 ICE 연결 확인에도 응답이 옴(엔진이 바인딩 응답을 받음).
    net.run_until(Duration::from_secs(5), "SFU 연결 확인 응답", |n| {
        n.sfu.sessions(&n.room).iter().all(|s| s.ice_responses > 0)
    });
}

#[test]
fn simulcast_layer_follows_selection() {
    let mut net = Net::new(SfuConfig::default());
    net.allow(&[device(1), device(2), device(3), device(4)]);
    net.join(
        1,
        vec![
            TrackSpec::audio(),
            TrackSpec::simulcast(TrackSource::Screen, [300, 900, 2_500]),
        ],
    );
    for n in 2..=4 {
        net.join(n, vec![TrackSpec::audio()]);
    }
    let a = device(1);
    net.peer(2).select(a, TrackSource::Screen, SfuLayer::High);
    net.peer(3).select(a, TrackSource::Screen, SfuLayer::Low);
    // 4는 고르지 않음: 화면공유는 '보기'를 눌러야 받음.
    net.pump();
    net.run_until(Duration::from_secs(10), "모두 연결", |n| {
        n.all_connected()
    });
    net.set_media(true);
    net.run_for(Duration::from_secs(2));

    let layers = |net: &Net, r: u8| -> Vec<u8> {
        net.peer_ref(r)
            .frames_from(a, TrackSource::Screen)
            .iter()
            .map(|f| f.layer())
            .collect()
    };
    let high = layers(&net, 2);
    let low = layers(&net, 3);
    // 높은 층이 아직 안 들어왔으면 들어온 층부터 보여 주고, 높은 층이 오면 그 층에 머묾.
    let first_high = high.iter().position(|l| *l == 2).expect("높은 층");
    assert!(
        high.len() - first_high > 30 && high[first_high..].iter().all(|l| *l == 2),
        "{high:?}"
    );
    assert!(low.len() > 30 && low.iter().all(|l| *l == 0), "{low:?}");
    assert!(
        layers(&net, 4).is_empty(),
        "고르지 않은 사람에게는 화면공유를 보내지 않음"
    );
    assert_unchanged(&net, 2, 1, TrackSource::Screen);
    assert_unchanged(&net, 3, 1, TrackSource::Screen);
    assert_eq!(
        net.sfu
            .forwarding_layer(&net.room, device(2), a, TrackSource::Screen),
        Some(2)
    );

    // 2가 낮은 층으로 바꾸면 키프레임에서 갈아탐.
    let before = net.peer_ref(2).received.len();
    net.peer(2).select(a, TrackSource::Screen, SfuLayer::Low);
    net.pump();
    net.run_for(Duration::from_secs(2));
    let after: Vec<_> = net.peer_ref(2).received[before..]
        .iter()
        .filter(|f| f.source == TrackSource::Screen)
        .collect();
    let first_low = after.iter().position(|f| f.layer() == 0).expect("낮은 층");
    assert!(after[first_low].keyframe, "층은 키프레임에서만 바뀜");
    assert!(after[first_low..].iter().all(|f| f.layer() == 0));
    assert!(after.len() - first_low > 30);
    // 받는 쪽 타임스탬프는 층을 바꿔도 뒤로 가지 않음.
    let times: Vec<u64> = net
        .peer_ref(2)
        .frames_from(a, TrackSource::Screen)
        .iter()
        .map(|f| f.time.numer())
        .collect();
    assert!(times.windows(2).all(|w| w[0] <= w[1]), "타임스탬프 역행");

    // 4가 '보기'를 누르면 받기 시작, 다시 끄면 멈춤.
    net.peer(4).select(a, TrackSource::Screen, SfuLayer::Medium);
    net.pump();
    net.run_for(Duration::from_secs(2));
    let mid = layers(&net, 4);
    assert!(mid.len() > 20 && mid.iter().all(|l| *l == 1), "{mid:?}");
    net.peer(4).select(a, TrackSource::Screen, SfuLayer::Off);
    net.pump();
    net.run_for(Duration::from_millis(500));
    let n = layers(&net, 4).len();
    net.run_for(Duration::from_secs(1));
    assert_eq!(layers(&net, 4).len(), n, "끈 뒤에도 받음");
}

#[test]
fn auto_layer_follows_bandwidth_estimate() {
    let mut net = Net::new(SfuConfig::default());
    net.allow(&[device(1), device(2)]);
    // 대략 70 kbps / 240 kbps / 600 kbps.
    net.join(
        1,
        vec![TrackSpec::simulcast(
            TrackSource::Camera,
            [300, 1_000, 2_500],
        )],
    );
    net.join(2, vec![TrackSpec::audio()]);
    net.run_until(Duration::from_secs(10), "연결", |n| n.all_connected());
    net.set_media(true);
    let a = device(1);
    // 추정이 생기기 전에는 낮은 층부터, 재고 나면 추정 안에서 가장 높은 층.
    net.run_until(Duration::from_secs(10), "가장 높은 층", |n| {
        n.sfu
            .forwarding_layer(&n.room, device(2), a, TrackSource::Camera)
            == Some(2)
    });
    let layers: Vec<u8> = net
        .peer_ref(2)
        .frames_from(a, TrackSource::Camera)
        .iter()
        .map(|f| f.layer())
        .collect();
    assert_eq!(layers.first(), Some(&0), "처음엔 가장 낮은 층");
    assert_unchanged(&net, 2, 1, TrackSource::Camera);
}

#[test]
fn leave_and_rejoin_renegotiates() {
    let mut net = Net::new(SfuConfig::default());
    net.allow(&[device(1), device(2), device(3)]);
    for n in 1..=3 {
        net.join(n, av());
    }
    net.run_until(Duration::from_secs(10), "연결", |n| {
        n.all_connected() && (1..=3).all(|i| n.sfu_receiving(i) == 4)
    });
    net.set_media(true);
    net.run_for(Duration::from_secs(1));

    // 3이 나감: 1·2는 3의 트랙을 멈추는 재협상을 받음.
    let c = device(3);
    net.peer(3).close();
    net.pump();
    assert!(net.events.contains(&SfuEvent::Left {
        room: net.room,
        device: c
    }));
    net.run_until(Duration::from_secs(5), "3의 트랙 정리", |n| {
        (1..=2).all(|i| n.sfu_receiving(i) == 2)
            && (1..=2).all(|i| n.peer_ref(i).recv_tracks.values().all(|(d, _)| *d != c))
    });
    let got = net.peer_ref(1).frames_from(c, TrackSource::Audio).len();
    net.run_for(Duration::from_secs(1));
    assert_eq!(
        net.peer_ref(1).frames_from(c, TrackSource::Audio).len(),
        got
    );

    // 다시 들어옴(새 세션).
    let now = net.now;
    net.peer(3).restart(now, 2);
    net.pump();
    net.run_until(Duration::from_secs(10), "다시 연결", |n| {
        n.all_connected() && (1..=3).all(|i| n.sfu_receiving(i) == 4)
    });
    let got = net.peer_ref(1).frames_from(c, TrackSource::Camera).len();
    net.run_for(Duration::from_secs(2));
    assert!(net.peer_ref(1).frames_from(c, TrackSource::Camera).len() > got + 30);
    assert!(
        net.peer_ref(3)
            .frames_from(device(1), TrackSource::Audio)
            .len()
            > 50
    );
    assert_unchanged(&net, 1, 3, TrackSource::Camera);
    assert_unchanged(&net, 3, 2, TrackSource::Camera);
}

#[test]
fn new_session_reuses_receiver_mlines() {
    let mut net = Net::new(SfuConfig::default());
    net.allow(&[device(1), device(2)]);
    net.join(1, av());
    net.join(2, av());
    net.run_until(Duration::from_secs(10), "연결", |n| {
        n.all_connected() && (1..=2).all(|i| n.sfu_receiving(i) == 2)
    });
    net.set_media(true);
    net.run_for(Duration::from_secs(1));
    let offers = net.peer_ref(1).offers;
    let got = net
        .peer_ref(1)
        .frames_from(device(2), TrackSource::Camera)
        .len();

    // 2가 닫지 않고 새 세션으로 갈아탐(ICE 재시작 등): 1은 재협상 없이 계속 받음.
    let now = net.now;
    net.peer(2).restart(now, 7);
    net.pump();
    net.run_until(Duration::from_secs(10), "다시 연결", |n| {
        n.all_connected()
    });
    net.run_for(Duration::from_secs(2));
    assert_eq!(net.peer_ref(1).offers, offers, "받는 쪽 m-line을 다시 씀");
    let frames = net.peer_ref(1).frames_from(device(2), TrackSource::Camera);
    assert!(frames.len() > got + 30);
    let times: Vec<u64> = frames.iter().map(|f| f.time.numer()).collect();
    assert!(
        times.windows(2).all(|w| w[0] <= w[1]),
        "세션이 바뀌어도 타임스탬프가 이어짐"
    );
    assert_unchanged(&net, 1, 2, TrackSource::Camera);
    // 지난 세션 번호로 오는 신호는 거절.
    let r = net.deliver(
        2,
        SignalPayload::SfuIce {
            session: 1,
            candidate: "candidate:1 1 udp 2130706431 127.0.0.1 9 typ host".into(),
            sdp_mid: None,
            sdp_mline_index: None,
        },
    );
    assert_eq!(r, Err(SfuError::StaleSession));
}

#[test]
fn non_participants_are_rejected_and_removed_members_kicked() {
    let mut net = Net::new(SfuConfig::default());
    net.allow(&[device(1), device(2)]);
    net.join(1, av());
    net.join(2, av());
    // 참가자가 아닌 기기 9.
    net.join(9, av());
    assert!(
        net.rejected
            .iter()
            .any(|(d, e)| *d == device(9) && *e == SfuError::NotAllowed)
    );
    net.run_until(Duration::from_secs(10), "1·2 연결", |n| {
        n.peer_ref(1).connected && n.peer_ref(2).connected
    });
    net.run_for(Duration::from_secs(1));
    assert!(!net.peer_ref(9).connected);
    assert!(
        net.stray_signals.is_empty(),
        "참가자 아닌 기기에게 신호를 보냄"
    );
    assert_eq!(net.sfu.session_count(), 2);
    assert!(
        net.sfu
            .sessions(&net.room)
            .iter()
            .all(|s| s.device != device(9))
    );

    // 열지 않은 방.
    let other = (net.room.0, yangpa_core::ChannelId::from_bytes([9; 16]));
    let r = net.sfu.handle_signal(
        net.now,
        other,
        device(1),
        SignalPayload::SfuClose { session: 1 },
    );
    assert_eq!(r, Err(SfuError::NoRoom));

    // 2가 권한을 잃음: 세션을 닫고 1에게서 2의 트랙을 뺌.
    net.allow(&[device(1)]);
    assert!(net.peer_ref(2).closed);
    net.run_until(Duration::from_secs(5), "2의 트랙 정리", |n| {
        n.sfu_receiving(1) == 0 && n.peer_ref(1).recv_tracks.is_empty()
    });
    let r = net.deliver(
        2,
        SignalPayload::SfuOffer {
            session: 5,
            sdp: String::new(),
            tracks: Vec::new(),
        },
    );
    assert_eq!(r, Err(SfuError::NotAllowed));
}

#[test]
fn capacity_and_malformed_signals() {
    let mut net = Net::new(SfuConfig {
        capacity: 2,
        ..SfuConfig::default()
    });
    net.allow(&[device(1), device(2), device(3)]);
    net.join(1, av());
    net.join(2, av());
    net.join(3, av());
    assert!(
        net.rejected
            .iter()
            .any(|(d, e)| *d == device(3) && *e == SfuError::Full)
    );
    // 같은 세션 재제안·나쁜 SDP·나쁜 트랙 표.
    let track = |mid: &str, source| SfuTrack {
        mid: mid.into(),
        device: None,
        source,
    };
    let r = net.deliver(
        1,
        SignalPayload::SfuOffer {
            session: 1,
            sdp: "v=0".into(),
            tracks: vec![],
        },
    );
    assert_eq!(r, Err(SfuError::BadSdp));
    // 너무 큰 SDP·후보는 읽지 않음.
    let r = net.deliver(
        1,
        SignalPayload::SfuOffer {
            session: 9,
            sdp: "v".repeat(64 * 1024 + 1),
            tracks: vec![],
        },
    );
    assert_eq!(r, Err(SfuError::TooLarge));
    let r = net.deliver(
        1,
        SignalPayload::SfuAnswer {
            session: 1,
            sdp: "v".repeat(64 * 1024 + 1),
            tracks: vec![],
        },
    );
    assert_eq!(r, Err(SfuError::TooLarge));
    let r = net.deliver(
        1,
        SignalPayload::SfuIce {
            session: 1,
            candidate: "c".repeat(4 * 1024 + 1),
            sdp_mid: None,
            sdp_mline_index: None,
        },
    );
    assert_eq!(r, Err(SfuError::TooLarge));
    let r = net.deliver(
        3,
        SignalPayload::SfuOffer {
            session: 1,
            sdp: "v=0".into(),
            tracks: vec![
                track("0", TrackSource::Audio),
                track("1", TrackSource::Audio),
            ],
        },
    );
    assert_eq!(r, Err(SfuError::BadTracks));
    let r = net.deliver(
        3,
        SignalPayload::SfuOffer {
            session: 1,
            sdp: "v=0".into(),
            tracks: vec![track("a b", TrackSource::Audio)],
        },
    );
    assert_eq!(r, Err(SfuError::BadTracks));
    let r = net.deliver(
        3,
        SignalPayload::SfuAnswer {
            session: 1,
            sdp: "v=0".into(),
            tracks: vec![],
        },
    );
    assert_eq!(r, Err(SfuError::StaleSession));
    let r = net.deliver(3, SignalPayload::Leave);
    assert_eq!(r, Err(SfuError::Unsupported));
    // mDNS 후보는 조용히 버림.
    let r = net.deliver(
        1,
        SignalPayload::SfuIce {
            session: 1,
            candidate: "candidate:1 1 udp 2130706431 abc.local 9 typ host".into(),
            sdp_mid: None,
            sdp_mline_index: None,
        },
    );
    assert_eq!(r, Ok(()));
    net.run_until(Duration::from_secs(10), "1·2 연결", |n| {
        n.peer_ref(1).connected && n.peer_ref(2).connected
    });
}

#[test]
fn late_joiner_gets_keyframe_from_sender() {
    let mut net = Net::new(SfuConfig::default());
    net.allow(&[device(1), device(2), device(3)]);
    net.join(1, av());
    net.join(2, av());
    net.run_until(Duration::from_secs(10), "연결", |n| n.all_connected());
    let media_start = net.now;
    net.set_media(true);
    // 키프레임 주기(2초)와 어긋나게 들어옴.
    net.run_for(Duration::from_millis(700));
    let requests = net.peer_ref(1).keyframe_requests;
    net.join(3, av());
    net.run_until(Duration::from_secs(10), "3이 1의 영상을 받음", |n| {
        !n.peer_ref(3)
            .frames_from(device(1), TrackSource::Camera)
            .is_empty()
    });
    assert!(
        net.peer_ref(1).keyframe_requests > requests,
        "SFU가 키프레임을 청함"
    );
    let first = net.peer_ref(3).frames_from(device(1), TrackSource::Camera)[0].clone();
    assert!(first.keyframe);
    // 2초 주기 키프레임보다 먼저 받아야 함(요청이 통함).
    assert!(first.at.duration_since(media_start) < Duration::from_millis(1900));
}

/// 층 선택은 통화방에 들어올 수 있는 다른 기기에 대해서만 받고, 떠난 기기의 선택은 지웁니다(선택 표가 끝없이
/// 커지지 않음).
#[test]
fn selections_are_bounded_to_room_devices() {
    let mut net = Net::new(SfuConfig::default());
    net.allow(&[device(1), device(2), device(3)]);
    net.join(1, av());
    net.join(2, av());
    net.run_until(Duration::from_secs(10), "1·2 연결", |n| {
        n.peer_ref(1).connected && n.peer_ref(2).connected
    });
    let select = |device, source| SignalPayload::SfuSelect {
        device,
        source,
        layer: SfuLayer::Low,
    };
    for n in 10..=250u8 {
        let r = net.deliver(1, select(device(n), TrackSource::Camera));
        assert_eq!(r, Err(SfuError::NotAllowed));
    }
    assert_eq!(
        net.deliver(1, select(device(1), TrackSource::Camera)),
        Err(SfuError::NotAllowed),
        "자기 자신"
    );
    assert_eq!(net.sfu.selection_count(), 0);
    for source in [TrackSource::Audio, TrackSource::Camera, TrackSource::Screen] {
        assert_eq!(net.deliver(1, select(device(2), source)), Ok(()));
        assert_eq!(net.deliver(1, select(device(3), source)), Ok(()));
        // 같은 대상은 덮어씀.
        assert_eq!(net.deliver(1, select(device(2), source)), Ok(()));
    }
    assert_eq!(net.sfu.selection_count(), 6);
    // 3이 통화방에서 빠지면 3에 대한 선택도 지움.
    net.allow(&[device(1), device(2)]);
    assert_eq!(net.sfu.selection_count(), 3);
    net.allow(&[device(2)]);
    assert_eq!(net.sfu.selection_count(), 0);
}

/// SFU의 재협상 제안에 응답이 오지 않으면(신호가 사라짐) 기한 뒤 세션을 닫아 참가자가 새 세션으로 다시 붙게
/// 합니다. 닫지 않으면 그 세션의 재협상이 영영 막혀 뒤에 온 참가자의 미디어를 받지 못합니다.
#[test]
fn unanswered_renegotiation_closes_session() {
    let timeout = Duration::from_secs(3);
    let mut net = Net::new(SfuConfig {
        answer_timeout: timeout,
        ..SfuConfig::default()
    });
    net.allow(&[device(1), device(2), device(3)]);
    net.join(1, av());
    net.run_until(Duration::from_secs(10), "1 연결", |n| {
        n.peer_ref(1).connected
    });
    net.drop_offers_to.insert(device(1));
    net.join(2, av());
    net.run_until(
        Duration::from_secs(5),
        "1에게 갈 제안이 사라짐",
        |n| n.dropped_offers > 0,
    );
    let lost_at = net.now;
    net.run_until(timeout * 3, "응답 없는 세션을 닫음", |n| {
        n.peer_ref(1).closed
    });
    assert!(net.now.duration_since(lost_at) >= timeout);
    assert!(
        net.sfu
            .sessions(&net.room)
            .iter()
            .all(|s| s.device != device(1))
    );
    // 새 세션으로 다시 붙으면 다른 참가자의 트랙을 받음.
    net.drop_offers_to.clear();
    let now = net.now;
    net.peer(1).restart(now, 2);
    net.pump();
    net.join(3, av());
    net.run_until(
        Duration::from_secs(10),
        "1이 2·3의 트랙을 받음",
        |n| n.peer_ref(1).connected && n.sfu_receiving(1) == 4,
    );
}
