//! SFU 통화(X2) 시험 도우미.
//!
//! [`MediaClient`]는 앱 대신 str0m 참가자(`yangpa_sfu::testkit::TestPeer`)를 돌립니다. 신호는 이 노드의 로컬 API
//! 경로(`voice.signal`과 같은 `relay_from_app`, `voice.signal` 알림)로, 미디어는 실제 127.0.0.1 UDP로 오갑니다.
//! 앱처럼 참가자 알림의 SFU 호스트가 바뀌면 새 세션으로 다시 붙고, 호스트가 거절하거나 세션을 닫으면 잠시 뒤 다시
//! 제안합니다.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::net::UdpSocket;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use yangpa_core::ids::{CallId, ChannelId, DeviceId, SpaceId, UserId};
use yangpa_core::wire::{SfuCaps, SfuLayer, SignalPayload, TrackSource};
use yangpa_rpc::api::{Notification, Topology};

pub use yangpa_sfu::SessionInfo;
pub use yangpa_sfu::testkit::{Received, TestPeer, TrackSpec};

use crate::error::{Error, Result};
use crate::node::Node;
use crate::{sfu, tor, voice};

/// 거절·닫힘 뒤 다시 제안하기까지.
const RETRY: Duration = Duration::from_millis(200);
/// 미디어 박자.
const MEDIA_TICK: Duration = Duration::from_millis(10);

/// 이 노드가 앱에 알리는 토폴로지와 SFU 호스트.
pub fn topology(
    node: &Node,
    space: &SpaceId,
    channel: &ChannelId,
) -> Option<(Topology, Option<DeviceId>)> {
    voice::inspect::plan(node, space, channel).map(|(t, h, _, _)| (t, h))
}

/// 이 노드가 따르는 결정의 SFU 호스트(앱에 알리는 것과 다를 수 있음: Tor 통화는 늘 `Tor`로 알림).
pub fn plan_host(node: &Node, space: &SpaceId, channel: &ChannelId) -> Option<DeviceId> {
    voice::inspect::plan(node, space, channel).and_then(|(_, _, h, _)| h)
}

/// Tor 통화에서 이 노드가 WebRTC로 바로 잇는다고 앱에 알린 참가자(서로 신뢰).
pub fn direct(node: &Node, space: &SpaceId, channel: &ChannelId) -> Vec<DeviceId> {
    voice::inspect::direct(node, space, channel)
}

/// 이 노드가 그 통화의 SFU를 맡고 있는지.
pub fn hosting(node: &Node, space: &SpaceId, channel: &ChannelId) -> bool {
    sfu::is_hosting(node, &(*space, *channel))
}

/// 이 노드가 맡은 SFU 세션(맡지 않으면 빈 목록).
pub async fn sessions(node: &Node, space: SpaceId, channel: ChannelId) -> Vec<SessionInfo> {
    sfu::inspect::sessions(node, (space, channel)).await
}

/// SFU 미디어 소켓 주소(실행기가 없으면 빈 목록).
pub fn media_addrs(node: &Node) -> Vec<SocketAddr> {
    sfu::inspect::addrs(node)
}

/// 이 노드가 앱에 알리는 SFU 호스트의 사용자.
pub fn host_user(node: &Node, space: &SpaceId, channel: &ChannelId) -> Option<UserId> {
    voice::inspect::host_user(node, space, channel)
}

/// 참가하지 않은 통화에 SFU 호스트 후보로 나섰는지.
pub fn announced(node: &Node, space: &SpaceId, channel: &ChannelId) -> bool {
    voice::inspect::announced(node, space, channel)
}

/// 이 노드가 따르던 토폴로지 결정을 잊게 합니다(결정을 받지 못한 호스트 흉내). 맡던 통화방도 닫습니다.
pub fn forget_plan(node: &Arc<Node>, space: SpaceId, channel: ChannelId) {
    voice::inspect::forget_plan(node, space, channel);
}

/// 키 관리 기기가 20초마다 하는 결정 다시 보내기를 지금 한 번 합니다.
pub fn resend_plans(node: &Arc<Node>) {
    voice::resend_plans(node);
}

/// 20초마다 하는 `keepalive` 다시 보기(참가 자격, SFU 호스트 조건 다시 알리기, 결정 다시 보내기)를 지금 한 번
/// 합니다.
pub async fn recheck(node: &Arc<Node>) {
    voice::recheck(node).await;
}

/// 이 노드가 맡은 통화방 수.
pub fn hosted_rooms(node: &Node) -> usize {
    sfu::inspect::hosted_rooms(node).len()
}

/// 이 기기가 통화에 알린 SFU 호스트 조건(맡을 수 없으면 `None`).
pub fn my_caps(node: &Node, space: &SpaceId, channel: &ChannelId) -> Option<SfuCaps> {
    voice::inspect::my_caps(node, space, channel)
}

/// 지금 SFU 호스트를 맡을 수 있는지.
pub fn can_host(node: &Node) -> bool {
    sfu::can_host(node)
}

/// 직접 연결 스웜을 Tor처럼 보고 통화 정책을 적용합니다(네트워크는 그대로, Tor 망 없이 시험): P2P 전용 채널이
/// 아닌 통화를 끝내고, SFU 호스트를 그만둡니다.
pub async fn enter_tor_policy(node: &Arc<Node>) {
    tor::simulate_tor(node, true);
    voice::enforce_policy(node).await;
}

/// 참가자 하나의 지금 상태.
pub struct ClientState {
    pub peer: TestPeer,
    /// 따르는 SFU 호스트(넘겨받는 중이면 `None`).
    pub host: Option<DeviceId>,
    /// 데몬이 거절한 신호의 오류.
    pub errors: Vec<String>,
    /// 연 세션 수.
    pub sessions: u32,
    retry_at: Option<Instant>,
}

impl ClientState {
    fn restart(&mut self, now: Instant) {
        // 지난 세션에 보내려던 신호는 버립니다.
        while self.peer.poll_signal().is_some() {}
        self.sessions += 1;
        let n = self.sessions;
        self.peer.restart(now, n);
        self.retry_at = None;
    }
}

/// 앱 대신 SFU에 붙는 참가자. 놓으면 멈춥니다.
pub struct MediaClient {
    pub device: DeviceId,
    state: Arc<Mutex<ClientState>>,
    task: JoinHandle<()>,
}

impl MediaClient {
    /// 참가 중인 통화의 SFU 호스트에 붙습니다(호스트가 아직 없으면 알림을 기다림).
    pub async fn start(
        node: &Arc<Node>,
        space: SpaceId,
        channel: ChannelId,
        specs: Vec<TrackSpec>,
    ) -> Result<MediaClient> {
        let device = node.me()?.device_id();
        let call = voice::inspect::call_id(node, &space, &channel)
            .ok_or_else(|| Error::invalid("통화에 참가 중이 아님"))?;
        let notes = node.subscribe();
        let host = topology(node, &space, &channel).and_then(|(_, h)| h);
        let sock = UdpSocket::bind(("127.0.0.1", 0))
            .await
            .map_err(|e| Error::internal(format!("UDP 소켓: {e}")))?;
        let local = sock
            .local_addr()
            .map_err(|e| Error::internal(format!("UDP 소켓: {e}")))?;
        let state = Arc::new(Mutex::new(ClientState {
            peer: TestPeer::new(Instant::now(), device, local, 1, specs),
            host,
            errors: Vec::new(),
            sessions: 1,
            retry_at: None,
        }));
        let task = tokio::spawn(drive(
            node.clone(),
            space,
            channel,
            call,
            sock,
            local,
            state.clone(),
            notes,
        ));
        Ok(MediaClient {
            device,
            state,
            task,
        })
    }

    /// 지금 상태를 봅니다.
    pub fn with<T>(&self, f: impl FnOnce(&ClientState) -> T) -> T {
        f(&self.state.lock())
    }

    /// `origin`의 `source` 트랙에서 받은 프레임 수.
    pub fn frames_from(&self, origin: DeviceId, source: TrackSource) -> usize {
        self.with(|s| s.peer.frames_from(origin, source).len())
    }

    /// 지금 세션에서 `origin`의 `source`를 받았는지(세션이 바뀐 뒤 다시 받는지 볼 때).
    pub fn frames_since(&self, origin: DeviceId, source: TrackSource, since: Instant) -> usize {
        self.with(|s| {
            s.peer
                .frames_from(origin, source)
                .iter()
                .filter(|r| r.at >= since)
                .count()
        })
    }

    /// 받는 쪽 층 선택(`SfuSelect`).
    pub fn select(&self, device: DeviceId, source: TrackSource, layer: SfuLayer) {
        self.state.lock().peer.select(device, source, layer);
    }

    /// 세션을 닫고 멈춥니다.
    pub fn stop(self) {
        self.state.lock().peer.close();
        self.task.abort();
    }
}

impl Drop for MediaClient {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[allow(clippy::too_many_arguments)]
async fn drive(
    node: Arc<Node>,
    space: SpaceId,
    channel: ChannelId,
    call: CallId,
    sock: UdpSocket,
    local: SocketAddr,
    st: Arc<Mutex<ClientState>>,
    mut notes: broadcast::Receiver<Notification>,
) {
    let mut buf = vec![0u8; 2048];
    let mut tick = 0u64;
    let mut next_media = Instant::now();
    loop {
        // 신호 보내기(앱처럼 하나씩 차례로).
        let host = st.lock().host;
        if let Some(h) = host {
            loop {
                let Some(p) = st.lock().peer.poll_signal() else {
                    break;
                };
                let offer = matches!(p, SignalPayload::SfuOffer { .. });
                if let Err(e) = voice::relay_from_app(&node, space, channel, call, h, p).await {
                    let mut s = st.lock();
                    s.errors.push(e.to_string());
                    if offer {
                        s.retry_at = Some(Instant::now() + RETRY);
                    }
                }
            }
        }
        loop {
            let Some((_, dst, data)) = st.lock().peer.poll_transmit() else {
                break;
            };
            let _ = sock.send_to(&data, dst).await;
        }
        let now = Instant::now();
        let wake = {
            let s = st.lock();
            let mut w = next_media;
            if let Some(t) = s.peer.poll_timeout() {
                w = w.min(t);
            }
            if let Some(t) = s.retry_at {
                w = w.min(t);
            }
            w.max(now + Duration::from_millis(1))
        };
        tokio::select! {
            r = sock.recv_from(&mut buf) => {
                if let Ok((n, from)) = r {
                    st.lock().peer.handle_receive(Instant::now(), from, local, &buf[..n]);
                }
            }
            n = notes.recv() => match n {
                Ok(Notification::VoiceSignal { space: s, channel: c, from, payload, .. })
                    if s == space && c == channel =>
                {
                    let mut g = st.lock();
                    if g.host == Some(from) {
                        let now = Instant::now();
                        g.peer.handle_signal(now, payload);
                        if g.peer.closed && g.retry_at.is_none() {
                            g.retry_at = Some(now + RETRY);
                        }
                    }
                }
                Ok(Notification::VoiceParticipants { space: s, channel: c, topology, sfu_host, .. })
                    if s == space && c == channel =>
                {
                    let host = sfu_host.filter(|_| topology == Topology::Sfu);
                    let mut g = st.lock();
                    if host != g.host {
                        g.host = host;
                        if host.is_some() {
                            g.restart(Instant::now());
                        }
                    }
                }
                Err(broadcast::error::RecvError::Closed) => return,
                _ => {}
            },
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(wake)) => {}
        }
        let now = Instant::now();
        let mut s = st.lock();
        if s.peer.poll_timeout().is_some_and(|t| t <= now) {
            s.peer.handle_timeout(now);
        }
        if now >= next_media {
            s.peer.send_media(now, tick);
            tick += 1;
            next_media = (next_media + MEDIA_TICK).max(now);
        }
        if s.retry_at.is_some_and(|t| t <= now) && s.host.is_some() {
            s.restart(now);
        }
    }
}
