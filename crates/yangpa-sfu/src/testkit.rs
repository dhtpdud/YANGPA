//! 시험용 SFU 참가자(str0m). 소켓·시계가 없어서 메모리 안 가상 네트워크(이 크레이트의 시험)로도, 실제 UDP와
//! 데몬 신호 경로(데몬 시험)로도 돌릴 수 있습니다. 앱(`session.ts`)과 같은 규칙을 따릅니다: 세션마다 한 번 제안하고
//! 올릴 트랙을 미리 다 담으며, 그 뒤로는 SFU의 제안에 답만 합니다.
//!
//! 보내는 프레임은 E2EE 암호문처럼 무작위 바이트이고, 맨 앞 몇 바이트만 정해 둡니다:
//! `[키프레임이면 짝수, 층, 출처 표지, 일련번호 4바이트, …]`(VP8 키프레임 표시는 첫 바이트 최하위 비트).

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::time::Instant;

use str0m::change::{SdpAnswer, SdpOffer, SdpPendingOffer};
use str0m::format::Codec;
use str0m::media::{
    Direction, Frequency, MediaData, MediaKind, MediaTime, Mid, Rid, Simulcast, SimulcastLayer,
};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, Input, Output, Rtc};
use yangpa_core::DeviceId;
use yangpa_core::wire::{SfuLayer, SfuTrack, SignalPayload, TrackSource};

/// 올릴 트랙 하나.
#[derive(Clone, Debug)]
pub struct TrackSpec {
    pub source: TrackSource,
    /// simulcast 층 rid(낮은 층 → 높은 층). 비면 층 하나.
    pub rids: Vec<&'static str>,
    /// 층마다 프레임 크기(바이트).
    pub frame_bytes: Vec<usize>,
}

impl TrackSpec {
    pub fn audio() -> Self {
        Self {
            source: TrackSource::Audio,
            rids: Vec::new(),
            frame_bytes: vec![80],
        }
    }

    pub fn camera() -> Self {
        Self {
            source: TrackSource::Camera,
            rids: Vec::new(),
            frame_bytes: vec![1_000],
        }
    }

    /// 세 층 simulcast. 30fps에서 대략 `sizes × 240` bps.
    pub fn simulcast(source: TrackSource, sizes: [usize; 3]) -> Self {
        Self {
            source,
            rids: vec!["q", "h", "f"],
            frame_bytes: sizes.to_vec(),
        }
    }
}

/// 받은 프레임.
#[derive(Clone, Debug)]
pub struct Received {
    pub origin: DeviceId,
    pub source: TrackSource,
    pub data: Vec<u8>,
    pub keyframe: bool,
    pub time: MediaTime,
    pub at: Instant,
}

impl Received {
    /// 보낸 쪽이 적은 층 번호.
    pub fn layer(&self) -> u8 {
        self.data.get(1).copied().unwrap_or(0)
    }
}

struct Write {
    mid: Mid,
    rid: Option<Rid>,
    codec: Codec,
    time: MediaTime,
    data: Vec<u8>,
    source: TrackSource,
}

struct SendTrack {
    spec: TrackSpec,
    mid: Mid,
    want_key: HashSet<Option<Rid>>,
}

/// 시험용 참가자 하나.
pub struct TestPeer {
    pub device: DeviceId,
    pub addr: SocketAddr,
    pub session: u32,
    pub rtc: Rtc,
    specs: Vec<TrackSpec>,
    tracks: Vec<SendTrack>,
    pending: Option<SdpPendingOffer>,
    /// SFU가 알려 준 받는 트랙 표(mid → 출처).
    pub recv_tracks: HashMap<Mid, (DeviceId, TrackSource)>,
    pub received: Vec<Received>,
    /// 보낸 프레임(출처별). 받은 프레임이 바이트 그대로인지 맞춰 봅니다.
    pub sent: HashMap<TrackSource, HashSet<Vec<u8>>>,
    pub connected: bool,
    /// SFU가 세션을 닫음(`SfuClose`).
    pub closed: bool,
    /// SFU에게서 받은 재협상 제안 수.
    pub offers: usize,
    /// 받은 키프레임 요청 수.
    pub keyframe_requests: usize,
    tag: u8,
    frames: u64,
    rng: u64,
    next_timeout: Instant,
    signals: VecDeque<SignalPayload>,
    transmits: VecDeque<(SocketAddr, SocketAddr, Vec<u8>)>,
}

impl TestPeer {
    /// 참가자를 만들고 첫 제안(`SfuOffer`)을 신호 큐에 넣습니다.
    pub fn new(
        now: Instant,
        device: DeviceId,
        addr: SocketAddr,
        session: u32,
        specs: Vec<TrackSpec>,
    ) -> Self {
        let tag = device.0[0];
        let mut peer = Self {
            device,
            addr,
            session,
            rtc: build_rtc(now),
            specs,
            tracks: Vec::new(),
            pending: None,
            recv_tracks: HashMap::new(),
            received: Vec::new(),
            sent: HashMap::new(),
            connected: false,
            closed: false,
            offers: 0,
            keyframe_requests: 0,
            tag,
            frames: 0,
            rng: u64::from_le_bytes(device.0[..8].try_into().unwrap_or([7; 8])) | 1,
            next_timeout: now,
            signals: VecDeque::new(),
            transmits: VecDeque::new(),
        };
        peer.offer(now);
        peer
    }

    /// 같은 기기로 새 세션을 엽니다(새 PeerConnection, 더 큰 세션 번호).
    pub fn restart(&mut self, now: Instant, session: u32) {
        self.session = session;
        self.rtc = build_rtc(now);
        self.tracks.clear();
        self.pending = None;
        self.recv_tracks.clear();
        self.connected = false;
        self.closed = false;
        self.offer(now);
    }

    fn offer(&mut self, now: Instant) {
        if let Ok(c) = Candidate::host(self.addr, "udp") {
            self.rtc.add_local_candidate(c);
        }
        let mut change = self.rtc.sdp_api();
        let mut tracks = Vec::new();
        for spec in &self.specs {
            let kind = match spec.source {
                TrackSource::Audio => MediaKind::Audio,
                _ => MediaKind::Video,
            };
            let simulcast = (!spec.rids.is_empty()).then(|| {
                let mut s = Simulcast::new();
                for rid in &spec.rids {
                    s.add_send_layer(SimulcastLayer::new(rid));
                }
                s
            });
            let mid = change.add_media(
                kind,
                Direction::SendOnly,
                Some(self.device.to_string()),
                Some(format!("{:?}", spec.source).to_lowercase()),
                simulcast,
            );
            tracks.push(SendTrack {
                spec: spec.clone(),
                mid,
                want_key: HashSet::new(),
            });
        }
        let Some((offer, pending)) = change.apply() else {
            return;
        };
        self.pending = Some(pending);
        let table = tracks
            .iter()
            .map(|t| SfuTrack {
                mid: t.mid.to_string(),
                device: None,
                source: t.spec.source,
            })
            .collect();
        self.tracks = tracks;
        self.signals.push_back(SignalPayload::SfuOffer {
            session: self.session,
            sdp: offer.to_sdp_string(),
            tracks: table,
        });
        self.drain(now);
    }

    /// 받는 쪽 층 선택.
    pub fn select(&mut self, device: DeviceId, source: TrackSource, layer: SfuLayer) {
        self.signals.push_back(SignalPayload::SfuSelect {
            device,
            source,
            layer,
        });
    }

    /// 세션을 끝냄.
    pub fn close(&mut self) {
        self.signals.push_back(SignalPayload::SfuClose {
            session: self.session,
        });
        self.rtc.disconnect();
        self.connected = false;
    }

    /// SFU에게서 온 신호.
    pub fn handle_signal(&mut self, now: Instant, payload: SignalPayload) {
        match payload {
            SignalPayload::SfuAnswer { session, sdp, .. } if session == self.session => {
                if let (Some(pending), Ok(answer)) =
                    (self.pending.take(), SdpAnswer::from_sdp_string(&sdp))
                {
                    self.rtc
                        .sdp_api()
                        .accept_answer(pending, answer)
                        .expect("SFU 응답을 받아야 함");
                }
            }
            SignalPayload::SfuOffer {
                session,
                sdp,
                tracks,
            } if session == self.session => {
                self.offers += 1;
                self.recv_tracks = tracks
                    .into_iter()
                    .filter_map(|t| Some((Mid::from(t.mid.as_str()), (t.device?, t.source))))
                    .collect();
                let offer = SdpOffer::from_sdp_string(&sdp).expect("SFU 제안 SDP");
                let answer = self
                    .rtc
                    .sdp_api()
                    .accept_offer(offer)
                    .expect("SFU 제안을 받아야 함");
                self.signals.push_back(SignalPayload::SfuAnswer {
                    session,
                    sdp: answer.to_sdp_string(),
                    tracks: Vec::new(),
                });
            }
            SignalPayload::SfuClose { session } if session == self.session => {
                self.closed = true;
                self.connected = false;
                self.rtc.disconnect();
            }
            _ => {}
        }
        self.drain(now);
    }

    /// 이 참가자 주소로 온 데이터그램.
    pub fn handle_receive(
        &mut self,
        now: Instant,
        source: SocketAddr,
        destination: SocketAddr,
        data: &[u8],
    ) {
        let Ok(contents) = data.try_into() else {
            return;
        };
        let input = Input::Receive(
            now,
            Receive {
                proto: Protocol::Udp,
                source,
                destination,
                contents,
            },
        );
        if self.rtc.is_alive() && self.rtc.accepts(&input) {
            let _ = self.rtc.handle_input(input);
        }
        self.drain(now);
    }

    pub fn handle_timeout(&mut self, now: Instant) {
        if self.rtc.is_alive() {
            let _ = self.rtc.handle_input(Input::Timeout(now));
        }
        self.drain(now);
    }

    pub fn poll_timeout(&self) -> Option<Instant> {
        self.rtc.is_alive().then_some(self.next_timeout)
    }

    pub fn poll_signal(&mut self) -> Option<SignalPayload> {
        self.signals.pop_front()
    }

    /// 보낼 데이터그램 `(보내는 주소, 받는 주소, 내용)`.
    pub fn poll_transmit(&mut self) -> Option<(SocketAddr, SocketAddr, Vec<u8>)> {
        self.transmits.pop_front()
    }

    /// 10ms 박자마다 부릅니다: 오디오는 20ms, 영상은 30ms마다 한 프레임(층마다).
    pub fn send_media(&mut self, now: Instant, tick: u64) {
        if !self.connected {
            return;
        }
        let audio = tick.is_multiple_of(2);
        let video = tick.is_multiple_of(3);
        let mut writes: Vec<Write> = Vec::new();
        for t in &mut self.tracks {
            let is_audio = t.spec.source == TrackSource::Audio;
            if (is_audio && !audio) || (!is_audio && !video) {
                continue;
            }
            let (codec, time) = if is_audio {
                (
                    Codec::Opus,
                    MediaTime::new(tick / 2 * 960, Frequency::FORTY_EIGHT_KHZ),
                )
            } else {
                (
                    Codec::Vp8,
                    MediaTime::new(tick / 3 * 3_000, Frequency::NINETY_KHZ),
                )
            };
            let layers: Vec<Option<Rid>> = if t.spec.rids.is_empty() {
                vec![None]
            } else {
                t.spec.rids.iter().map(|r| Some(Rid::from(*r))).collect()
            };
            for (i, rid) in layers.into_iter().enumerate() {
                let size = t.spec.frame_bytes.get(i).copied().unwrap_or(500).max(8);
                // 2초마다, 또는 요청을 받으면 키프레임.
                let key = !is_audio && (tick.is_multiple_of(200) || t.want_key.remove(&rid));
                let mut data = Vec::with_capacity(size);
                data.push(if is_audio || !key { 0x11 } else { 0x10 });
                data.push(i as u8);
                data.push(self.tag);
                data.extend_from_slice(&(self.frames as u32).to_be_bytes());
                while data.len() < size {
                    self.rng ^= self.rng << 13;
                    self.rng ^= self.rng >> 7;
                    self.rng ^= self.rng << 17;
                    data.push(self.rng as u8);
                }
                self.frames += 1;
                writes.push(Write {
                    mid: t.mid,
                    rid,
                    codec,
                    time,
                    data,
                    source: t.spec.source,
                });
            }
        }
        for Write {
            mid,
            rid,
            codec,
            time,
            data,
            source,
        } in writes
        {
            let Some(writer) = self.rtc.writer(mid) else {
                continue;
            };
            let Some(pt) = writer
                .payload_params()
                .find(|p| p.spec().codec == codec)
                .map(|p| p.pt())
            else {
                continue;
            };
            let writer = match rid {
                Some(r) => writer.rid(r),
                None => writer,
            };
            if writer.write(pt, now, time, data.clone()).is_ok() {
                self.sent.entry(source).or_default().insert(data);
            }
        }
        self.drain(now);
    }

    /// `origin`의 `source`에서 받은 프레임.
    pub fn frames_from(&self, origin: DeviceId, source: TrackSource) -> Vec<&Received> {
        self.received
            .iter()
            .filter(|r| r.origin == origin && r.source == source)
            .collect()
    }

    fn drain(&mut self, now: Instant) {
        loop {
            if !self.rtc.is_alive() {
                return;
            }
            match self.rtc.poll_output() {
                Ok(Output::Timeout(t)) => {
                    self.next_timeout = t;
                    return;
                }
                Ok(Output::Transmit(t)) => {
                    self.transmits
                        .push_back((t.source, t.destination, t.contents.into()));
                }
                Ok(Output::Event(e)) => self.on_event(e, now),
                Err(_) => {
                    self.rtc.disconnect();
                    return;
                }
            }
        }
    }

    fn on_event(&mut self, e: Event, now: Instant) {
        match e {
            Event::Connected => self.connected = true,
            Event::MediaData(d) => self.on_media(d, now),
            Event::KeyframeRequest(r) => {
                self.keyframe_requests += 1;
                if let Some(t) = self.tracks.iter_mut().find(|t| t.mid == r.mid) {
                    t.want_key.insert(r.rid);
                }
            }
            _ => {}
        }
    }

    fn on_media(&mut self, d: MediaData, now: Instant) {
        let Some(&(origin, source)) = self.recv_tracks.get(&d.mid) else {
            return;
        };
        self.received.push(Received {
            origin,
            source,
            keyframe: d.is_keyframe(),
            data: d.data.to_vec(),
            time: d.time,
            at: now,
        });
    }
}

fn build_rtc(now: Instant) -> Rtc {
    Rtc::builder()
        .clear_codecs()
        .enable_opus(true, false)
        .enable_vp8(true)
        .build(now)
}
