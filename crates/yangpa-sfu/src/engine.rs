//! SFU 엔진: 참가자마다 PeerConnection 하나(str0m `Rtc`)를 두고, 들어온 프레임을 풀지 않고 같은 통화의 다른
//! 참가자에게 그대로 보냅니다. 소켓·시계·신호 경로는 부르는 쪽(데몬) 몫입니다(sans-IO).
//!
//! 협상 규칙(글레어 없음): 참가자가 세션마다 한 번 제안(올릴 트랙을 미리 다 담음)하고, 그 뒤 재협상은 SFU만
//! 제안합니다. 같은 기기가 더 큰 세션 번호로 다시 제안하면 지난 세션을 갈아 끼웁니다.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use str0m::bwe::{Bitrate, BweKind};
use str0m::change::{SdpAnswer, SdpOffer, SdpPendingOffer};
use str0m::media::{
    Direction, KeyframeRequest, KeyframeRequestKind, MediaData, MediaKind, MediaTime, Mid, Rid,
    Rids,
};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, IceConnectionState, Input, Output as RtcOutput, Rtc};
use tracing::debug;
use yangpa_core::wire::{SfuLayer, SfuTrack, SignalPayload, TrackSource};
use yangpa_core::{ChannelId, DeviceId, SpaceId};

use crate::layer::{LayerInfo, RateMeter, choose};

/// 통화방 = (공간, 통화 채널).
pub type RoomId = (SpaceId, ChannelId);

/// 세션 정리·층 재평가 주기.
const TICK: Duration = Duration::from_millis(250);
/// 같은 층에 키프레임을 다시 청하기까지의 간격(여럿이 한꺼번에 들어올 때 보내는 쪽을 보호).
const KEYFRAME_THROTTLE: Duration = Duration::from_millis(300);
/// 오디오 트랙 하나에 잡아 두는 대역폭(Opus 음성 + 헤더).
const AUDIO_BPS: u64 = 48_000;
/// 한 세션이 올릴 수 있는 트랙 수(오디오·카메라·화면 + 여유).
const MAX_TRACKS_PER_SESSION: usize = 4;
/// simulcast 층 수 상한.
const MAX_LAYERS: usize = 3;
/// 신호 크기 상한(데몬의 앱 신호 한도와 같음). 큰 SDP를 되풀이해 보내 실행기를 붙잡지 못하게 엔진에서도 막습니다.
const MAX_SDP: usize = 64 * 1024;
const MAX_CANDIDATE: usize = 4 * 1024;
/// ICE 쌍 통계(응답 수)를 모으는 간격.
const STATS_INTERVAL: Duration = Duration::from_secs(1);

/// SFU 설정.
#[derive(Clone, Debug)]
pub struct SfuConfig {
    /// 동시에 받는 세션(참가자 PeerConnection) 수 상한.
    pub capacity: usize,
    /// 제안을 받은 뒤 이 시간 안에 연결되지 않으면 세션을 버림.
    pub connect_timeout: Duration,
    /// ICE가 끊긴 채 이 시간이 지나면 세션을 버림.
    pub disconnect_grace: Duration,
    /// SFU가 보낸 재협상 제안에 이 시간 안에 응답이 없으면 세션을 닫음(참가자가 새 세션으로 다시 붙음).
    /// 응답이 사라지면 그 뒤 재협상이 영영 막히므로 기다리지 않습니다.
    pub answer_timeout: Duration,
    /// 받는 쪽마다 대역폭 추정 시작값(bps).
    pub initial_bitrate: u64,
}

impl Default for SfuConfig {
    fn default() -> Self {
        Self {
            capacity: 50,
            connect_timeout: Duration::from_secs(30),
            disconnect_grace: Duration::from_secs(15),
            answer_timeout: Duration::from_secs(10),
            initial_bitrate: 1_000_000,
        }
    }
}

/// 엔진이 내보내는 일.
#[derive(Debug)]
pub enum Output {
    /// UDP 데이터그램을 `source` 소켓에서 `destination`으로 보냄.
    Transmit {
        source: SocketAddr,
        destination: SocketAddr,
        contents: Vec<u8>,
    },
    /// 참가자에게 보낼 신호(`SfuOffer`/`SfuAnswer`/`SfuClose`). 같은 기기에게 가는 신호는 순서대로 전해야 합니다.
    Signal {
        room: RoomId,
        to: DeviceId,
        payload: SignalPayload,
    },
    Event(SfuEvent),
}

/// 세션 상태 변화(데몬의 기록·점검용).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SfuEvent {
    Connected { room: RoomId, device: DeviceId },
    Left { room: RoomId, device: DeviceId },
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SfuError {
    #[error("SFU가 맡지 않은 통화입니다")]
    NoRoom,
    #[error("이 통화의 참가자가 아닙니다")]
    NotAllowed,
    #[error("SFU가 받을 수 있는 인원을 넘었습니다")]
    Full,
    #[error("지난 세션입니다")]
    StaleSession,
    #[error("SDP를 읽을 수 없습니다")]
    BadSdp,
    #[error("트랙 표가 잘못되었습니다")]
    BadTracks,
    #[error("신호가 너무 큽니다")]
    TooLarge,
    #[error("WebRTC 협상 실패: {0}")]
    Rtc(String),
    #[error("SFU가 다루지 않는 신호입니다")]
    Unsupported,
}

/// 점검용 세션 요약.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionInfo {
    pub device: DeviceId,
    pub session: u32,
    pub connected: bool,
    /// 올리는 트랙.
    pub sending: Vec<TrackSource>,
    /// 받는(협상이 끝난) 트랙 수.
    pub receiving: usize,
    /// 고른 ICE 쌍에서 SFU가 보낸 연결 확인에 받은 응답 수(실행기가 응답을 엔진에 넘기는지 점검).
    pub ice_responses: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OutState {
    ToOpen,
    Negotiating(Mid),
    Open(Mid),
    ToStop(Mid),
    NegotiatingStop(Mid),
}

/// 받는 쪽 타임스탬프를 층·세션이 바뀌어도 이어지게 하는 대응표.
#[derive(Debug)]
struct Clock {
    /// 지금 시간축의 출처 층(`None`이면 다음 프레임에서 다시 이어 붙임).
    layer: Option<usize>,
    offset: i64,
    last_out: u64,
    last_wall: Instant,
}

#[derive(Debug)]
struct TrackOut {
    origin: DeviceId,
    source: TrackSource,
    kind: MediaKind,
    state: OutState,
    /// 협상 중에 출처가 떠남(응답을 받으면 바로 멈춤).
    gone: bool,
    /// 지금 보내는 층.
    current: Option<usize>,
    /// 보내려는 층(키프레임이 오면 바꿈).
    target: Option<usize>,
    clock: Option<Clock>,
}

impl TrackOut {
    fn new(origin: DeviceId, source: TrackSource, kind: MediaKind) -> Self {
        Self {
            origin,
            source,
            kind,
            state: OutState::ToOpen,
            gone: false,
            current: None,
            target: None,
            clock: None,
        }
    }

    fn mid(&self) -> Option<Mid> {
        match self.state {
            OutState::ToOpen => None,
            OutState::Negotiating(m)
            | OutState::Open(m)
            | OutState::ToStop(m)
            | OutState::NegotiatingStop(m) => Some(m),
        }
    }

    /// 출처 세션이 바뀜: 새 키프레임부터, 시간축은 이어 붙임.
    fn restart(&mut self) {
        self.current = None;
        self.target = None;
        if let Some(c) = &mut self.clock {
            c.layer = None;
        }
    }

    fn map_time(&mut self, layer: usize, data: &MediaData) -> MediaTime {
        let freq = data.time.frequency();
        let input = data.time.numer() as i64;
        let wall = data.network_time;
        let clock = self.clock.get_or_insert(Clock {
            layer: Some(layer),
            offset: 0,
            last_out: data.time.numer(),
            last_wall: wall,
        });
        if clock.layer != Some(layer) {
            // 층마다(또는 새 세션마다) RTP 시간 기준이 다를 수 있어, 지난 출력 시각에 실제 흐른 시간만큼 더해 잇습니다.
            let elapsed = wall.saturating_duration_since(clock.last_wall);
            let ticks =
                (elapsed.as_micros() as u64).saturating_mul(u64::from(freq.get())) / 1_000_000;
            let want = clock.last_out.saturating_add(ticks.max(1));
            clock.offset = want as i64 - input;
            clock.layer = Some(layer);
        }
        let out = (input + clock.offset).max(0) as u64;
        if out >= clock.last_out {
            clock.last_out = out;
            clock.last_wall = wall;
        }
        MediaTime::new(out, freq)
    }
}

#[derive(Debug)]
struct TrackIn {
    mid: Mid,
    source: TrackSource,
    kind: MediaKind,
    /// 낮은 층 → 높은 층. 비면 simulcast가 아닌 트랙(층 하나).
    rids: Vec<Rid>,
    meters: Vec<RateMeter>,
    last_keyframe_request: Vec<Option<Instant>>,
}

impl TrackIn {
    fn layer_count(&self) -> usize {
        self.rids.len().max(1)
    }

    fn infos(&self, now: Instant) -> Vec<LayerInfo> {
        self.meters.iter().map(|m| m.info(now)).collect()
    }

    fn throttle(&mut self, layer: usize, now: Instant) -> bool {
        let Some(slot) = self.last_keyframe_request.get_mut(layer) else {
            return false;
        };
        if slot.is_some_and(|t| now.saturating_duration_since(t) < KEYFRAME_THROTTLE) {
            return false;
        }
        *slot = Some(now);
        true
    }
}

/// 한 세션 안에서 생겨 다른 세션으로 퍼지는 일. 프레임마다 생기므로 상자에 넣지 않습니다.
#[allow(clippy::large_enum_variant)]
enum Propagated {
    TrackOpen {
        room: RoomId,
        origin: DeviceId,
        source: TrackSource,
        kind: MediaKind,
    },
    Media {
        room: RoomId,
        origin: DeviceId,
        source: TrackSource,
        layer: usize,
        layers: Vec<LayerInfo>,
        data: MediaData,
    },
    Keyframe {
        room: RoomId,
        origin: DeviceId,
        source: TrackSource,
        layer: Option<usize>,
        kind: KeyframeRequestKind,
    },
}

enum Forwarded {
    No,
    Wrote,
    NeedKeyframe(usize),
}

struct Client {
    room: RoomId,
    device: DeviceId,
    session: u32,
    rtc: Rtc,
    created: Instant,
    connected: bool,
    lost_at: Option<Instant>,
    /// 제안의 트랙 표(mid → 출처).
    declared: Vec<(Mid, TrackSource)>,
    ins: Vec<TrackIn>,
    outs: Vec<TrackOut>,
    pending: Option<SdpPendingOffer>,
    /// `pending` 제안을 보낸 시각(응답 기한).
    pending_since: Option<Instant>,
    ice_responses: u64,
    estimate: Option<u64>,
    desired: u64,
    needs_reselect: bool,
    next_timeout: Instant,
}

impl Client {
    fn drain(&mut self, now: Instant, queue: &mut VecDeque<Output>, props: &mut Vec<Propagated>) {
        loop {
            if !self.rtc.is_alive() {
                return;
            }
            if self.negotiate(now, queue) {
                continue;
            }
            match self.rtc.poll_output() {
                Ok(RtcOutput::Timeout(t)) => {
                    self.next_timeout = t;
                    return;
                }
                Ok(RtcOutput::Transmit(t)) => queue.push_back(Output::Transmit {
                    source: t.source,
                    destination: t.destination,
                    contents: t.contents.into(),
                }),
                Ok(RtcOutput::Event(e)) => self.on_event(e, now, queue, props),
                Err(e) => {
                    debug!(device = %self.device.short(), error = %e, "SFU 세션 오류");
                    self.rtc.disconnect();
                    return;
                }
            }
        }
    }

    /// 받을 트랙이 바뀌었으면 SFU 쪽에서 제안합니다(응답을 기다리는 제안이 있으면 그 뒤로 미룸).
    fn negotiate(&mut self, now: Instant, queue: &mut VecDeque<Output>) -> bool {
        if self.pending.is_some() {
            return false;
        }
        let mut change = self.rtc.sdp_api();
        for out in &mut self.outs {
            match out.state {
                OutState::ToOpen => {
                    let mid = change.add_media(
                        out.kind,
                        Direction::SendOnly,
                        Some(out.origin.to_string()),
                        Some(source_name(out.source).to_owned()),
                        None,
                    );
                    out.state = OutState::Negotiating(mid);
                }
                OutState::ToStop(mid) => {
                    change.stop_media(mid);
                    out.state = OutState::NegotiatingStop(mid);
                }
                _ => {}
            }
        }
        if !change.has_changes() {
            return false;
        }
        let Some((offer, pending)) = change.apply() else {
            return false;
        };
        self.pending = Some(pending);
        self.pending_since = Some(now);
        queue.push_back(Output::Signal {
            room: self.room,
            to: self.device,
            payload: SignalPayload::SfuOffer {
                session: self.session,
                sdp: offer.to_sdp_string(),
                tracks: self.track_table(),
            },
        });
        true
    }

    fn track_table(&self) -> Vec<SfuTrack> {
        self.outs
            .iter()
            .filter_map(|o| match o.state {
                OutState::Negotiating(m) | OutState::Open(m) | OutState::ToStop(m) => {
                    Some(SfuTrack {
                        mid: m.to_string(),
                        device: Some(o.origin),
                        source: o.source,
                    })
                }
                _ => None,
            })
            .collect()
    }

    fn answered(&mut self) {
        for o in &mut self.outs {
            if let OutState::Negotiating(m) = o.state {
                o.state = if o.gone {
                    OutState::ToStop(m)
                } else {
                    OutState::Open(m)
                };
            }
        }
        self.outs
            .retain(|o| !matches!(o.state, OutState::NegotiatingStop(_)));
        self.needs_reselect = true;
    }

    fn on_event(
        &mut self,
        e: Event,
        now: Instant,
        queue: &mut VecDeque<Output>,
        props: &mut Vec<Propagated>,
    ) {
        match e {
            Event::Connected => {
                self.connected = true;
                self.lost_at = None;
                self.needs_reselect = true;
                queue.push_back(Output::Event(SfuEvent::Connected {
                    room: self.room,
                    device: self.device,
                }));
            }
            Event::IceConnectionStateChange(IceConnectionState::Disconnected) => {
                self.lost_at.get_or_insert(now);
            }
            Event::IceConnectionStateChange(
                IceConnectionState::Connected | IceConnectionState::Completed,
            ) => self.lost_at = None,
            Event::PeerStats(s) => {
                if let Some(p) = s.selected_candidate_pair {
                    self.ice_responses = p.responses_received;
                }
            }
            Event::MediaData(d) => self.on_media_data(d, now, props),
            Event::KeyframeRequest(r) => self.on_keyframe_request(r, props),
            Event::EgressBitrateEstimate(BweKind::Twcc { estimate, .. })
            | Event::EgressBitrateEstimate(BweKind::Remb { estimate, .. }) => {
                let v = estimate.as_u64();
                // 10% 넘게 바뀔 때만 층을 다시 고름.
                if self.estimate.is_none_or(|old| v.abs_diff(old) * 10 > old) {
                    self.needs_reselect = true;
                }
                self.estimate = Some(v);
            }
            _ => {}
        }
    }

    /// 제안을 받아들인 직후 트랙 표대로 받을 트랙을 등록합니다. str0m의 `MediaAdded`는 SRTP가 준비된 뒤에야 오므로
    /// 그걸 기다리면 같은 기기가 새 세션으로 갈아탈 때 다른 참가자의 m-line을 멈췄다 다시 여는 재협상이 생깁니다.
    fn register_tracks(&mut self, props: &mut Vec<Propagated>) {
        for &(mid, source) in &self.declared {
            let Some(media) = self.rtc.media(mid) else {
                debug!(device = %self.device.short(), "트랙 표의 mid가 제안에 없음");
                continue;
            };
            let expected = match source {
                TrackSource::Audio => MediaKind::Audio,
                TrackSource::Camera | TrackSource::Screen => MediaKind::Video,
            };
            if !media.direction().is_receiving() || media.kind() != expected {
                continue;
            }
            let mut rids: Vec<Rid> = match media.rids_rx() {
                Rids::Specific(v) => v.clone(),
                _ => Vec::new(),
            };
            rids.truncate(MAX_LAYERS);
            let n = rids.len().max(1);
            self.ins.push(TrackIn {
                mid,
                source,
                kind: expected,
                rids,
                meters: vec![RateMeter::default(); n],
                last_keyframe_request: vec![None; n],
            });
            props.push(Propagated::TrackOpen {
                room: self.room,
                origin: self.device,
                source,
                kind: expected,
            });
        }
    }

    fn on_media_data(&mut self, d: MediaData, now: Instant, props: &mut Vec<Propagated>) {
        let Some(t) = self.ins.iter_mut().find(|t| t.mid == d.mid) else {
            return;
        };
        let layer = if t.rids.is_empty() {
            0
        } else {
            match d.rid.and_then(|rid| t.rids.iter().position(|r| *r == rid)) {
                Some(i) => i,
                None => return,
            }
        };
        t.meters[layer].record(now, d.data.len());
        if !d.contiguous && t.kind == MediaKind::Video && t.throttle(layer, now) {
            // 빠진 프레임이 있으면 받는 쪽 화면이 깨진 채 머물지 않게 보내는 쪽에 키프레임을 청함.
            if let Some(mut w) = self.rtc.writer(t.mid) {
                let _ = w.request_keyframe(d.rid, KeyframeRequestKind::Pli);
            }
        }
        props.push(Propagated::Media {
            room: self.room,
            origin: self.device,
            source: t.source,
            layer,
            layers: t.infos(now),
            data: d,
        });
    }

    fn on_keyframe_request(&mut self, r: KeyframeRequest, props: &mut Vec<Propagated>) {
        let Some(o) = self.outs.iter().find(|o| o.mid() == Some(r.mid)) else {
            return;
        };
        props.push(Propagated::Keyframe {
            room: self.room,
            origin: o.origin,
            source: o.source,
            layer: o.current.or(o.target),
            kind: r.kind,
        });
    }

    fn open_out(&mut self, origin: DeviceId, source: TrackSource, kind: MediaKind) {
        if let Some(o) = self.outs.iter_mut().find(|o| {
            o.origin == origin
                && o.source == source
                && !matches!(o.state, OutState::NegotiatingStop(_))
        }) {
            // 같은 기기가 새 세션으로 돌아옴: 멈추려던 m-line을 그대로 다시 씀(재협상 없음).
            if let OutState::ToStop(m) = o.state {
                o.state = OutState::Open(m);
            }
            o.gone = false;
            o.restart();
            return;
        }
        self.outs.push(TrackOut::new(origin, source, kind));
    }

    /// 출처 기기가 떠남. 바뀐 게 있으면 `true`.
    fn origin_gone(&mut self, origin: DeviceId) -> bool {
        let before = self.outs.len();
        self.outs
            .retain(|o| !(o.origin == origin && o.state == OutState::ToOpen));
        let mut changed = self.outs.len() != before;
        for o in self.outs.iter_mut().filter(|o| o.origin == origin) {
            match o.state {
                OutState::Open(m) => {
                    o.state = OutState::ToStop(m);
                    changed = true;
                }
                OutState::Negotiating(_) => {
                    o.gone = true;
                    changed = true;
                }
                _ => {}
            }
        }
        changed
    }

    fn forward(
        &mut self,
        origin: DeviceId,
        source: TrackSource,
        layer: usize,
        layers: &[LayerInfo],
        pref: SfuLayer,
        data: &MediaData,
    ) -> Forwarded {
        if !self.connected || pref == SfuLayer::Off {
            return Forwarded::No;
        }
        let Some(out) = self
            .outs
            .iter_mut()
            .find(|o| o.origin == origin && o.source == source)
        else {
            return Forwarded::No;
        };
        let OutState::Open(mid) = out.state else {
            return Forwarded::No;
        };
        if out.kind == MediaKind::Video {
            if out.target.is_none() {
                out.target = choose(layers, pref, None, out.current);
            }
            let Some(target) = out.target else {
                return Forwarded::No;
            };
            if out.current != Some(layer) {
                if layer != target {
                    return Forwarded::No;
                }
                if !data.is_keyframe() {
                    return Forwarded::NeedKeyframe(target);
                }
                // 고른 층의 키프레임에서만 바꿔야 받는 쪽 디코더가 끊기지 않음.
                out.current = Some(layer);
            }
        }
        let time = out.map_time(layer, data);
        let Some(mut writer) = self.rtc.writer(mid) else {
            return Forwarded::No;
        };
        let Some(pt) = writer.match_params(data.params) else {
            return Forwarded::No;
        };
        if let Some(level) = data.ext_vals.audio_level {
            writer = writer.audio_level(level, data.ext_vals.voice_activity.unwrap_or(false));
        }
        match writer.write(pt, data.network_time, time, data.data.clone()) {
            Ok(()) => Forwarded::Wrote,
            Err(e) => {
                debug!(device = %self.device.short(), error = %e, "SFU 전달 실패");
                Forwarded::No
            }
        }
    }

    fn request_keyframe(
        &mut self,
        source: TrackSource,
        layer: Option<usize>,
        kind: KeyframeRequestKind,
        now: Instant,
    ) -> bool {
        let Some(t) = self.ins.iter_mut().find(|t| t.source == source) else {
            return false;
        };
        if t.kind != MediaKind::Video {
            return false;
        }
        let idx = layer.unwrap_or(0).min(t.layer_count() - 1);
        if !t.throttle(idx, now) {
            return false;
        }
        let rid = t.rids.get(idx).copied();
        let Some(mut w) = self.rtc.writer(t.mid) else {
            return false;
        };
        w.request_keyframe(rid, kind).is_ok()
    }
}

fn source_name(s: TrackSource) -> &'static str {
    match s {
        TrackSource::Audio => "audio",
        TrackSource::Camera => "camera",
        TrackSource::Screen => "screen",
    }
}

/// 받는 쪽이 아직 고르지 않았을 때: 화면공유는 '보기'를 눌러야(`SfuSelect`) 받음, 나머지는 자동.
fn default_pref(source: TrackSource) -> SfuLayer {
    match source {
        TrackSource::Screen => SfuLayer::Off,
        TrackSource::Audio | TrackSource::Camera => SfuLayer::Auto,
    }
}

type Prefs = HashMap<(RoomId, DeviceId), HashMap<(DeviceId, TrackSource), SfuLayer>>;

fn pref_of(
    prefs: &Prefs,
    room: RoomId,
    receiver: DeviceId,
    origin: DeviceId,
    source: TrackSource,
) -> SfuLayer {
    prefs
        .get(&(room, receiver))
        .and_then(|m| m.get(&(origin, source)))
        .copied()
        .unwrap_or_else(|| default_pref(source))
}

fn validate_tracks(tracks: Vec<SfuTrack>) -> Result<Vec<(Mid, TrackSource)>, SfuError> {
    if tracks.len() > MAX_TRACKS_PER_SESSION {
        return Err(SfuError::BadTracks);
    }
    let mut out: Vec<(Mid, TrackSource)> = Vec::with_capacity(tracks.len());
    for t in tracks {
        let ok_mid = !t.mid.is_empty()
            && t.mid.len() <= 16
            && t.mid.chars().all(|c| c.is_ascii_alphanumeric());
        if !ok_mid || t.device.is_some() {
            return Err(SfuError::BadTracks);
        }
        let mid = Mid::from(t.mid.as_str());
        if out.iter().any(|(m, s)| *m == mid || *s == t.source) {
            return Err(SfuError::BadTracks);
        }
        out.push((mid, t.source));
    }
    Ok(out)
}

/// 통화 SFU. 여러 통화방을 한꺼번에 맡을 수 있습니다.
pub struct Sfu {
    config: SfuConfig,
    candidates: Vec<Candidate>,
    rooms: HashMap<RoomId, BTreeSet<DeviceId>>,
    clients: BTreeMap<u64, Client>,
    prefs: Prefs,
    next_id: u64,
    queue: VecDeque<Output>,
    dirty: BTreeSet<u64>,
    next_tick: Option<Instant>,
}

impl Sfu {
    pub fn new(config: SfuConfig) -> Self {
        Self {
            config,
            candidates: Vec::new(),
            rooms: HashMap::new(),
            clients: BTreeMap::new(),
            prefs: HashMap::new(),
            next_id: 1,
            queue: VecDeque::new(),
            dirty: BTreeSet::new(),
            next_tick: None,
        }
    }

    pub fn config(&self) -> &SfuConfig {
        &self.config
    }

    /// 새 세션의 응답 SDP에 넣을 로컬 후보(호스트·서버 반사). 이미 열린 세션에는 영향이 없습니다.
    pub fn add_local_candidate(&mut self, c: Candidate) {
        if !self.candidates.iter().any(|x| x.addr() == c.addr()) {
            self.candidates.push(c);
        }
    }

    pub fn local_candidates(&self) -> &[Candidate] {
        &self.candidates
    }

    /// 통화방을 열거나 들어올 수 있는 기기를 바꿉니다. 빠진 기기의 세션은 닫습니다.
    pub fn set_allowed(
        &mut self,
        now: Instant,
        room: RoomId,
        allowed: impl IntoIterator<Item = DeviceId>,
    ) {
        let set: BTreeSet<DeviceId> = allowed.into_iter().collect();
        let kicked: Vec<u64> = self
            .clients
            .iter()
            .filter(|(_, c)| c.room == room && !set.contains(&c.device))
            .map(|(id, _)| *id)
            .collect();
        for id in kicked {
            self.remove(id, true);
        }
        // 떠난 받는 쪽의 선택과 떠난 출처에 대한 선택을 지웁니다(선택 표는 들어올 수 있는 기기 수로 묶임).
        self.prefs.retain(|(r, d), _| *r != room || set.contains(d));
        for ((r, _), m) in self.prefs.iter_mut() {
            if *r == room {
                m.retain(|(origin, _), _| set.contains(origin));
            }
        }
        self.rooms.insert(room, set);
        self.run(now);
    }

    pub fn has_room(&self, room: &RoomId) -> bool {
        self.rooms.contains_key(room)
    }

    pub fn rooms(&self) -> impl Iterator<Item = &RoomId> {
        self.rooms.keys()
    }

    /// 통화방을 닫습니다(모든 세션에 `SfuClose`).
    pub fn close_room(&mut self, now: Instant, room: RoomId) {
        let ids: Vec<u64> = self
            .clients
            .iter()
            .filter(|(_, c)| c.room == room)
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            self.remove(id, true);
        }
        self.rooms.remove(&room);
        self.prefs.retain(|(r, _), _| *r != room);
        self.run(now);
    }

    /// 참가자 기기 `from`이 보낸 신호. 부르는 쪽은 `from`이 인증된 통화 참가자인지 먼저 확인합니다.
    pub fn handle_signal(
        &mut self,
        now: Instant,
        room: RoomId,
        from: DeviceId,
        payload: SignalPayload,
    ) -> Result<(), SfuError> {
        let allowed = self.rooms.get(&room).ok_or(SfuError::NoRoom)?;
        if !allowed.contains(&from) {
            return Err(SfuError::NotAllowed);
        }
        // 통화방에 들어올 수 없는 기기(또는 자기 자신)의 선택은 받지 않습니다. 아무 기기 ID나 받으면 선택 표가
        // 끝없이 커집니다.
        let select_ok = match &payload {
            SignalPayload::SfuSelect { device, .. } => *device != from && allowed.contains(device),
            _ => true,
        };
        let res = match payload {
            SignalPayload::SfuOffer {
                session,
                sdp,
                tracks,
            } => self.on_offer(now, room, from, session, &sdp, tracks),
            SignalPayload::SfuAnswer { session, sdp, .. } => {
                self.on_answer(room, from, session, &sdp)
            }
            SignalPayload::SfuIce {
                session, candidate, ..
            } => self.on_ice(room, from, session, &candidate),
            SignalPayload::SfuSelect {
                device,
                source,
                layer,
            } if select_ok => {
                self.prefs
                    .entry((room, from))
                    .or_default()
                    .insert((device, source), layer);
                if let Some(id) = self.find(room, from) {
                    if let Some(c) = self.clients.get_mut(&id) {
                        c.needs_reselect = true;
                    }
                    self.dirty.insert(id);
                }
                Ok(())
            }
            SignalPayload::SfuSelect { .. } => Err(SfuError::NotAllowed),
            SignalPayload::SfuClose { session } => {
                if let Some(id) = self.find_session(room, from, session) {
                    self.remove(id, false);
                }
                Ok(())
            }
            _ => Err(SfuError::Unsupported),
        };
        self.run(now);
        res
    }

    /// 받은 UDP 데이터그램. 어느 세션의 것도 아니면 `false`.
    pub fn handle_receive(
        &mut self,
        now: Instant,
        source: SocketAddr,
        destination: SocketAddr,
        contents: &[u8],
    ) -> bool {
        let Ok(contents) = contents.try_into() else {
            return false;
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
        let Some(id) = self
            .clients
            .iter()
            .find(|(_, c)| c.rtc.is_alive() && c.rtc.accepts(&input))
            .map(|(id, _)| *id)
        else {
            return false;
        };
        if let Some(c) = self.clients.get_mut(&id)
            && let Err(e) = c.rtc.handle_input(input)
        {
            debug!(device = %c.device.short(), error = %e, "SFU 입력 오류");
            c.rtc.disconnect();
        }
        self.dirty.insert(id);
        self.run(now);
        true
    }

    /// `poll_timeout`이 알려 준 시각이 되면 부릅니다(일찍 불러도 괜찮음).
    pub fn handle_timeout(&mut self, now: Instant) {
        let due: Vec<u64> = self
            .clients
            .iter()
            .filter(|(_, c)| c.next_timeout <= now)
            .map(|(id, _)| *id)
            .collect();
        for id in due {
            if let Some(c) = self.clients.get_mut(&id)
                && let Err(e) = c.rtc.handle_input(Input::Timeout(now))
            {
                debug!(device = %c.device.short(), error = %e, "SFU 시간 처리 오류");
                c.rtc.disconnect();
            }
            self.dirty.insert(id);
        }
        if self.next_tick.is_none_or(|t| t <= now) {
            self.next_tick = (!self.clients.is_empty()).then_some(now + TICK);
            self.tick(now);
        }
        self.run(now);
    }

    pub fn poll_output(&mut self) -> Option<Output> {
        self.queue.pop_front()
    }

    /// 다음에 `handle_timeout`을 부를 시각(세션이 없으면 `None`).
    pub fn poll_timeout(&self) -> Option<Instant> {
        let rtc = self.clients.values().map(|c| c.next_timeout).min();
        match (rtc, self.next_tick) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    pub fn session_count(&self) -> usize {
        self.clients.len()
    }

    /// 기억하는 층 선택 수(점검·시험용).
    pub fn selection_count(&self) -> usize {
        self.prefs.values().map(|m| m.len()).sum()
    }

    pub fn sessions(&self, room: &RoomId) -> Vec<SessionInfo> {
        self.clients
            .values()
            .filter(|c| c.room == *room)
            .map(|c| SessionInfo {
                device: c.device,
                session: c.session,
                connected: c.connected,
                sending: c.ins.iter().map(|t| t.source).collect(),
                receiving: c
                    .outs
                    .iter()
                    .filter(|o| matches!(o.state, OutState::Open(_)))
                    .count(),
                ice_responses: c.ice_responses,
            })
            .collect()
    }

    /// `receiver`에게 `origin`의 `source`를 지금 몇 번째 층으로 보내는지(점검·시험용).
    pub fn forwarding_layer(
        &self,
        room: &RoomId,
        receiver: DeviceId,
        origin: DeviceId,
        source: TrackSource,
    ) -> Option<usize> {
        let c = self
            .clients
            .values()
            .find(|c| c.room == *room && c.device == receiver)?;
        c.outs
            .iter()
            .find(|o| o.origin == origin && o.source == source)?
            .current
    }

    fn find(&self, room: RoomId, device: DeviceId) -> Option<u64> {
        self.clients
            .iter()
            .find(|(_, c)| c.room == room && c.device == device)
            .map(|(id, _)| *id)
    }

    fn find_session(&self, room: RoomId, device: DeviceId, session: u32) -> Option<u64> {
        self.find(room, device)
            .filter(|id| self.clients.get(id).is_some_and(|c| c.session == session))
    }

    fn on_offer(
        &mut self,
        now: Instant,
        room: RoomId,
        from: DeviceId,
        session: u32,
        sdp: &str,
        tracks: Vec<SfuTrack>,
    ) -> Result<(), SfuError> {
        if sdp.len() > MAX_SDP {
            return Err(SfuError::TooLarge);
        }
        let declared = validate_tracks(tracks)?;
        let offer = SdpOffer::from_sdp_string(sdp).map_err(|_| SfuError::BadSdp)?;
        if let Some(id) = self.find(room, from) {
            if self.clients.get(&id).is_some_and(|c| session <= c.session) {
                // 같은 세션의 재제안은 받지 않음(재협상은 SFU만 제안).
                return Err(SfuError::StaleSession);
            }
            self.remove(id, false);
        } else if self.clients.len() >= self.config.capacity {
            return Err(SfuError::Full);
        }
        let mut rtc = Rtc::builder()
            .clear_codecs()
            .enable_opus(true, false)
            .enable_vp8(true)
            .enable_bwe(Some(Bitrate::bps(self.config.initial_bitrate)))
            .set_stats_interval(Some(STATS_INTERVAL))
            .build(now);
        for c in &self.candidates {
            rtc.add_local_candidate(c.clone());
        }
        let answer = rtc
            .sdp_api()
            .accept_offer(offer)
            .map_err(|e| SfuError::Rtc(e.to_string()))?;
        let id = self.next_id;
        self.next_id += 1;
        let mut client = Client {
            room,
            device: from,
            session,
            rtc,
            created: now,
            connected: false,
            lost_at: None,
            declared,
            ins: Vec::new(),
            outs: Vec::new(),
            pending: None,
            pending_since: None,
            ice_responses: 0,
            estimate: None,
            desired: 0,
            needs_reselect: false,
            next_timeout: now,
        };
        // 이미 있는 참가자들의 트랙을 받을 m-line(첫 재협상에서 한꺼번에 제안).
        for other in self.clients.values().filter(|c| c.room == room) {
            for t in &other.ins {
                client.open_out(other.device, t.source, t.kind);
            }
        }
        self.clients.insert(id, client);
        self.queue.push_back(Output::Signal {
            room,
            to: from,
            payload: SignalPayload::SfuAnswer {
                session,
                sdp: answer.to_sdp_string(),
                tracks: Vec::new(),
            },
        });
        if self.next_tick.is_none() {
            self.next_tick = Some(now + TICK);
        }
        // 새 세션의 트랙을 다른 참가자에게 먼저 알려야 같은 기기의 지난 세션 m-line을 멈추지 않고 이어 씀.
        let mut props = Vec::new();
        if let Some(c) = self.clients.get_mut(&id) {
            c.register_tracks(&mut props);
        }
        props.extend(self.drain(id, now));
        for p in props {
            self.propagate(id, p, now);
        }
        self.dirty.insert(id);
        debug!(device = %from.short(), session, "SFU 세션 시작");
        Ok(())
    }

    fn on_answer(
        &mut self,
        room: RoomId,
        from: DeviceId,
        session: u32,
        sdp: &str,
    ) -> Result<(), SfuError> {
        if sdp.len() > MAX_SDP {
            return Err(SfuError::TooLarge);
        }
        let id = self
            .find_session(room, from, session)
            .ok_or(SfuError::StaleSession)?;
        let answer = SdpAnswer::from_sdp_string(sdp).map_err(|_| SfuError::BadSdp)?;
        let Some(c) = self.clients.get_mut(&id) else {
            return Err(SfuError::StaleSession);
        };
        let Some(pending) = c.pending.take() else {
            // 이미 처리했거나 버린 제안에 대한 응답.
            return Ok(());
        };
        c.pending_since = None;
        if let Err(e) = c.rtc.sdp_api().accept_answer(pending, answer) {
            self.remove(id, true);
            return Err(SfuError::Rtc(e.to_string()));
        }
        c.answered();
        self.dirty.insert(id);
        Ok(())
    }

    fn on_ice(
        &mut self,
        room: RoomId,
        from: DeviceId,
        session: u32,
        candidate: &str,
    ) -> Result<(), SfuError> {
        if candidate.len() > MAX_CANDIDATE {
            return Err(SfuError::TooLarge);
        }
        let id = self
            .find_session(room, from, session)
            .ok_or(SfuError::StaleSession)?;
        let line = candidate.trim().trim_start_matches("a=");
        if line.is_empty() {
            return Ok(());
        }
        // mDNS(.local) 후보 등 못 읽는 후보는 버림(연결은 상대 반사 후보로도 됨).
        let Ok(c) = Candidate::from_sdp_string(line) else {
            return Ok(());
        };
        if let Some(client) = self.clients.get_mut(&id) {
            client.rtc.add_remote_candidate(c);
            self.dirty.insert(id);
        }
        Ok(())
    }

    fn remove(&mut self, id: u64, notify: bool) {
        let Some(mut c) = self.clients.remove(&id) else {
            return;
        };
        c.rtc.disconnect();
        if notify {
            self.queue.push_back(Output::Signal {
                room: c.room,
                to: c.device,
                payload: SignalPayload::SfuClose { session: c.session },
            });
        }
        self.queue.push_back(Output::Event(SfuEvent::Left {
            room: c.room,
            device: c.device,
        }));
        for (oid, o) in self.clients.iter_mut() {
            if o.room == c.room && o.origin_gone(c.device) {
                self.dirty.insert(*oid);
            }
        }
        if self.clients.is_empty() {
            self.next_tick = None;
        }
        debug!(device = %c.device.short(), "SFU 세션 끝");
    }

    fn drain(&mut self, id: u64, now: Instant) -> Vec<Propagated> {
        let mut props = Vec::new();
        if let Some(c) = self.clients.get_mut(&id) {
            c.drain(now, &mut self.queue, &mut props);
        }
        props
    }

    /// 바뀐 세션이 없어질 때까지 출력을 비우고 세션 사이로 퍼뜨립니다.
    fn run(&mut self, now: Instant) {
        while let Some(id) = self.dirty.pop_first() {
            let mut props = self.drain(id, now);
            if self
                .clients
                .get_mut(&id)
                .is_some_and(|c| std::mem::take(&mut c.needs_reselect))
            {
                props.extend(self.reselect(id, now));
            }
            for p in props {
                self.propagate(id, p, now);
            }
        }
    }

    fn propagate(&mut self, from: u64, p: Propagated, now: Instant) {
        match p {
            Propagated::TrackOpen {
                room,
                origin,
                source,
                kind,
            } => {
                for (id, c) in self.clients.iter_mut() {
                    if *id != from && c.room == room && c.device != origin {
                        c.open_out(origin, source, kind);
                        c.needs_reselect = true;
                        self.dirty.insert(*id);
                    }
                }
            }
            Propagated::Media {
                room,
                origin,
                source,
                layer,
                layers,
                data,
            } => {
                let mut want = None;
                for (id, c) in self.clients.iter_mut() {
                    if *id == from || c.room != room {
                        continue;
                    }
                    let pref = pref_of(&self.prefs, room, c.device, origin, source);
                    match c.forward(origin, source, layer, &layers, pref, &data) {
                        Forwarded::Wrote => {
                            self.dirty.insert(*id);
                        }
                        Forwarded::NeedKeyframe(l) => want = Some(l),
                        Forwarded::No => {}
                    }
                }
                if let Some(l) = want {
                    self.propagate(
                        from,
                        Propagated::Keyframe {
                            room,
                            origin,
                            source,
                            layer: Some(l),
                            kind: KeyframeRequestKind::Pli,
                        },
                        now,
                    );
                }
            }
            Propagated::Keyframe {
                room,
                origin,
                source,
                layer,
                kind,
            } => {
                if let Some((id, c)) = self
                    .clients
                    .iter_mut()
                    .find(|(_, c)| c.room == room && c.device == origin)
                    && c.request_keyframe(source, layer, kind, now)
                {
                    self.dirty.insert(*id);
                }
            }
        }
    }

    /// 받는 쪽 하나의 층을 대역폭 추정과 선택에 맞춰 다시 고릅니다. 바꿀 층의 키프레임 요청을 돌려줌.
    fn reselect(&mut self, id: u64, now: Instant) -> Vec<Propagated> {
        let Some(room) = self.clients.get(&id).map(|c| c.room) else {
            return Vec::new();
        };
        let infos: HashMap<(DeviceId, TrackSource), Vec<LayerInfo>> = self
            .clients
            .values()
            .filter(|c| c.room == room)
            .flat_map(|c| {
                c.ins
                    .iter()
                    .map(move |t| ((c.device, t.source), t.infos(now)))
            })
            .collect();
        let Some(c) = self.clients.get_mut(&id) else {
            return Vec::new();
        };
        let receiver = c.device;
        let pref = |o: &TrackOut| pref_of(&self.prefs, room, receiver, o.origin, o.source);
        let live = |o: &TrackOut| {
            infos
                .get(&(o.origin, o.source))
                .is_some_and(|l| l.iter().any(|i| i.live))
        };
        let audio = c
            .outs
            .iter()
            .filter(|o| o.kind == MediaKind::Audio && pref(o) != SfuLayer::Off && live(o))
            .count() as u64;
        let video = c
            .outs
            .iter()
            .filter(|o| o.kind == MediaKind::Video && pref(o) != SfuLayer::Off && live(o))
            .count() as u64;
        // 추정의 90%에서 오디오 몫을 빼고 영상 트랙끼리 나눔.
        let budget = c
            .estimate
            .map(|e| (e / 10 * 9).saturating_sub(AUDIO_BPS * audio) / video.max(1));
        let mut desired = AUDIO_BPS * audio;
        let mut props = Vec::new();
        for o in c.outs.iter_mut().filter(|o| o.kind == MediaKind::Video) {
            let p = pref_of(&self.prefs, room, receiver, o.origin, o.source);
            let Some(layers) = infos.get(&(o.origin, o.source)) else {
                o.target = None;
                o.current = None;
                continue;
            };
            let target = choose(layers, p, budget, o.current);
            o.target = target;
            match target {
                None => o.current = None,
                Some(t) => {
                    desired += layers
                        .iter()
                        .filter(|i| i.live)
                        .map(|i| i.bps)
                        .max()
                        .unwrap_or(0);
                    if o.current != Some(t) && matches!(o.state, OutState::Open(_)) {
                        props.push(Propagated::Keyframe {
                            room,
                            origin: o.origin,
                            source: o.source,
                            layer: Some(t),
                            kind: KeyframeRequestKind::Pli,
                        });
                    }
                }
            }
        }
        if desired > 0 && desired != c.desired {
            c.desired = desired;
            // 더 높은 층을 보낼 수 있는지 보도록 그만큼 탐색하게 함.
            c.rtc.bwe().set_desired_bitrate(Bitrate::bps(desired));
            self.dirty.insert(id);
        }
        props
    }

    fn tick(&mut self, now: Instant) {
        let cfg = &self.config;
        let dead: Vec<u64> = self
            .clients
            .iter()
            .filter(|(_, c)| {
                !c.rtc.is_alive()
                    || (!c.connected
                        && now.saturating_duration_since(c.created) > cfg.connect_timeout)
                    || c.lost_at
                        .is_some_and(|t| now.saturating_duration_since(t) > cfg.disconnect_grace)
                    || c.pending_since
                        .is_some_and(|t| now.saturating_duration_since(t) > cfg.answer_timeout)
            })
            .map(|(id, _)| *id)
            .collect();
        for id in dead {
            self.remove(id, true);
        }
        let ids: Vec<u64> = self.clients.keys().copied().collect();
        for id in ids {
            let props = self.reselect(id, now);
            for p in props {
                self.propagate(id, p, now);
            }
        }
    }
}
