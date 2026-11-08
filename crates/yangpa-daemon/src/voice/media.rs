//! Tor 통화(`Topology::Tor`)의 소리 패킷 중계.
//!
//! Tor로 도는 기기의 일반 채널 통화는 WebRTC를 쓰지 않습니다(연결을 만드는 것만으로 IP 후보가 모임). 앱이 통화 미디어
//! 키로 봉인한 패킷을 `voice.media`로 넘기면, 데몬은 내용을 보지 않고 지금 참가자의 노드마다 `/yangpa/media/1`(Tor 연결)로
//! 보냅니다. 받는 데몬은 보낸 노드가 인증된 참가자 기기인지, 둘 다 그 통화에 있는지, 크기·빈도를 보고 `voice.media` 알림으로
//! 앱에 넘깁니다. 앱은 보낸 기기를 프레임 AAD로 검사해 엽니다. 패킷은 메모리에만 지나가고 디스크·로그에 남지 않습니다.
//!
//! 프레임 = 공간 ID(16) || 채널 ID(16) || 봉인한 패킷.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use base64::Engine;
use yangpa_core::ids::{CallId, ChannelId, DeviceId, SpaceId};
use yangpa_core::model::Permissions;
use yangpa_net::PeerId;
use yangpa_rpc::api::Notification;

use crate::error::{Error, Result};
use crate::node::Node;
use crate::peers;

/// 봉인한 패킷 하나의 최대 크기. 소리(Mimi 약 30바이트·Opus 수백 바이트·DAC 약 800바이트)와 비상용 화면공유의 VP8 조각
/// (앱이 12KiB씩 나눔)에 봉인 여유를 둡니다. 프레임(머리 32바이트 포함)이 네트워크 계층 한도(16KiB) 안에 듭니다.
pub(crate) const MAX_PACKET: usize = 15 * 1024;
const HEADER: usize = 16 + 16;
/// 보낸 기기마다 받는 패킷 빈도(초당)와 몰아 오는 여유. 소리 한 줄기는 초당 50개를 넘지 않습니다.
const RATE: f64 = 100.0;
const BURST: f64 = 200.0;

/// 보낸 기기별 토큰 통.
#[derive(Default)]
pub(crate) struct Buckets(HashMap<DeviceId, (Instant, f64)>);

impl Buckets {
    fn take(&mut self, device: DeviceId, now: Instant) -> bool {
        if self.0.len() > 1024 {
            self.0
                .retain(|_, (at, _)| now.duration_since(*at).as_secs() < 10);
        }
        let (at, tokens) = self.0.entry(device).or_insert((now, BURST));
        let refill = now.duration_since(*at).as_secs_f64() * RATE;
        *tokens = (*tokens + refill).min(BURST);
        *at = now;
        if *tokens < 1.0 {
            return false;
        }
        *tokens -= 1.0;
        true
    }
}

fn frame(space: &SpaceId, channel: &ChannelId, packet: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(HEADER + packet.len());
    f.extend_from_slice(space.as_bytes());
    f.extend_from_slice(channel.as_bytes());
    f.extend_from_slice(packet);
    f
}

fn parse(f: &[u8]) -> Option<(SpaceId, ChannelId, &[u8])> {
    let (s, rest) = f.split_first_chunk::<16>()?;
    let (c, packet) = rest.split_first_chunk::<16>()?;
    (!packet.is_empty() && packet.len() <= MAX_PACKET)
        .then(|| (SpaceId::from_bytes(*s), ChannelId::from_bytes(*c), packet))
}

/// 앱이 봉인한 패킷을 지금 참가자 모두에게 보냅니다(기다리지 않음, 못 보낸 것은 버림).
pub(crate) fn send(
    node: &Arc<Node>,
    space: SpaceId,
    channel: ChannelId,
    call: CallId,
    data: &str,
    to: Option<&[DeviceId]>,
) -> Result<()> {
    let packet = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|_| Error::invalid("data는 base64여야 합니다"))?;
    if packet.is_empty() || packet.len() > MAX_PACKET {
        return Err(Error::invalid(format!(
            "미디어 패킷은 1~{MAX_PACKET}바이트여야 합니다"
        )));
    }
    let me = node.me()?.device_id();
    let state = node
        .space_state(&space)
        .ok_or_else(|| Error::not_found("모르는 공간"))?;
    if !super::tor_call(node, &state, &channel) {
        return Err(Error::conflict(
            "Tor 통화가 아닙니다(이 통화의 소리는 WebRTC로 갑니다)",
        ));
    }
    if !super::has(&state, &node.me()?.user(), &channel, Permissions::SPEAK) {
        return Err(Error::forbidden("이 채널에서 말할 권한이 없습니다"));
    }
    let targets: Vec<(DeviceId, Option<PeerId>)> = {
        let calls = node.voice.calls.lock();
        let c = calls
            .get(&(space, channel))
            .filter(|c| c.me_in)
            .ok_or_else(|| Error::invalid("이 통화에 참가 중이 아님"))?;
        if c.id != call {
            return Err(Error::invalid("통화 ID가 다름"));
        }
        c.parts
            .iter()
            .filter(|(d, _)| **d != me && to.is_none_or(|to| to.contains(d)))
            .map(|(d, p)| (*d, p.peer))
            .collect()
    };
    let f = frame(&space, &channel, &packet);
    for (device, hint) in targets {
        let peer = hint
            .filter(|p| node.peers.identity_of(p).is_some_and(|(_, d)| d == device))
            .or_else(|| peers::peer_for_device(node, &device));
        if let Some(peer) = peer {
            node.net.send_media(peer, f.clone());
        }
    }
    Ok(())
}

/// 다른 노드가 보낸 프레임 묶음. 조건이 맞지 않는 프레임은 조용히 버립니다(답이 없는 한 방향 전송).
pub(crate) fn on_frames(node: &Arc<Node>, peer: PeerId, frames: Vec<Vec<u8>>) {
    let Some((_, device)) = node.peers.identity_of(&peer) else {
        return;
    };
    let Ok(me) = node.me() else { return };
    let my_device = me.device_id();
    let now = Instant::now();
    for f in frames {
        let Some((space, channel, packet)) = parse(&f) else {
            continue;
        };
        let call = {
            let calls = node.voice.calls.lock();
            calls
                .get(&(space, channel))
                .filter(|c| c.me_in && device != my_device)
                .and_then(|c| c.parts.get(&device).map(|p| (c.id, p.user)))
        };
        let Some((call, user)) = call else { continue };
        // 말할 권한이 없는 참가자의 소리는 넘기지 않습니다(앱은 그 참가자를 음소거로 보임).
        if !node.space_state(&space).is_some_and(|st| {
            super::tor_call(node, &st, &channel)
                && super::has(&st, &user, &channel, Permissions::SPEAK)
        }) {
            continue;
        }
        if !node.voice.media_rate.lock().take(device, now) {
            continue;
        }
        node.notify.publish(Notification::VoiceMedia {
            space,
            channel,
            call,
            from: device,
            data: base64::engine::general_purpose::STANDARD.encode(packet),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_round_trip_and_limits() {
        let (s, c) = (SpaceId::random(), ChannelId::random());
        let f = frame(&s, &c, b"abc");
        assert_eq!(parse(&f), Some((s, c, &b"abc"[..])));
        assert_eq!(parse(&f[..HEADER]), None, "빈 패킷");
        assert_eq!(parse(&f[..10]), None);
        assert_eq!(parse(&frame(&s, &c, &vec![0; MAX_PACKET + 1])), None);
    }

    #[test]
    fn rate_limit_allows_bursts_then_refills() {
        let mut b = Buckets::default();
        let d = DeviceId::random();
        let t = Instant::now();
        let n = (0..1000).filter(|_| b.take(d, t)).count();
        assert_eq!(n, BURST as usize);
        assert!(!b.take(d, t));
        assert!(b.take(d, t + std::time::Duration::from_millis(20)));
        // 다른 기기는 따로 셈
        assert!(b.take(DeviceId::random(), t));
    }
}
