//! 메모리 안 가상 네트워크: SFU 하나와 str0m 참가자 여럿을 가상 시계로 돌립니다(잠자기 없음).

#![allow(dead_code)]

use std::collections::{HashSet, VecDeque};
use std::net::{Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};

use yangpa_core::wire::SignalPayload;
use yangpa_core::{ChannelId, DeviceId, SpaceId};
use yangpa_sfu::testkit::{TestPeer, TrackSpec};
use yangpa_sfu::{Candidate, Output, RoomId, Sfu, SfuConfig, SfuError, SfuEvent};

const LATENCY: Duration = Duration::from_millis(5);
const MEDIA_TICK: Duration = Duration::from_millis(10);

struct Packet {
    at: Instant,
    source: SocketAddr,
    destination: SocketAddr,
    data: Vec<u8>,
}

pub struct Net {
    pub now: Instant,
    pub start: Instant,
    pub sfu: Sfu,
    pub sfu_addr: SocketAddr,
    pub room: RoomId,
    pub peers: Vec<TestPeer>,
    pub events: Vec<SfuEvent>,
    /// 참가자 신호를 SFU가 거절한 기록(기기, 오류).
    pub rejected: Vec<(DeviceId, SfuError)>,
    /// 참가자가 아닌 기기에게 간 신호(있으면 안 됨).
    pub stray_signals: Vec<DeviceId>,
    /// SFU의 재협상 제안을 이 기기들에게는 전하지 않음(신호 경로에서 사라진 응답 흉내).
    pub drop_offers_to: HashSet<DeviceId>,
    pub dropped_offers: usize,
    packets: VecDeque<Packet>,
    media: bool,
    media_tick: u64,
    next_media: Instant,
}

pub fn device(n: u8) -> DeviceId {
    let mut b = [0u8; 32];
    b[0] = n;
    b[31] = 0xA5;
    DeviceId::from_bytes(b)
}

pub fn room() -> RoomId {
    (SpaceId::from_bytes([1; 16]), ChannelId::from_bytes([2; 16]))
}

impl Net {
    pub fn new(config: SfuConfig) -> Self {
        let now = Instant::now();
        let sfu_addr: SocketAddr = (Ipv4Addr::LOCALHOST, 5000).into();
        let mut sfu = Sfu::new(config);
        sfu.add_local_candidate(Candidate::host(sfu_addr, "udp").unwrap());
        Self {
            now,
            start: now,
            sfu,
            sfu_addr,
            room: room(),
            peers: Vec::new(),
            events: Vec::new(),
            rejected: Vec::new(),
            stray_signals: Vec::new(),
            drop_offers_to: HashSet::new(),
            dropped_offers: 0,
            packets: VecDeque::new(),
            media: false,
            media_tick: 0,
            next_media: now,
        }
    }

    pub fn allow(&mut self, devices: &[DeviceId]) {
        self.sfu
            .set_allowed(self.now, self.room, devices.iter().copied());
        self.pump();
    }

    /// 참가자를 만들고 첫 제안을 보냅니다. 돌려준 번호로 `peers`에서 찾습니다.
    pub fn join(&mut self, n: u8, specs: Vec<TrackSpec>) -> usize {
        let addr: SocketAddr = (Ipv4Addr::LOCALHOST, 6000 + u16::from(n)).into();
        let peer = TestPeer::new(self.now, device(n), addr, 1, specs);
        self.peers.push(peer);
        let idx = self.peers.len() - 1;
        self.pump();
        idx
    }

    pub fn peer(&mut self, n: u8) -> &mut TestPeer {
        let d = device(n);
        self.peers
            .iter_mut()
            .find(|p| p.device == d)
            .expect("참가자")
    }

    pub fn peer_ref(&self, n: u8) -> &TestPeer {
        let d = device(n);
        self.peers.iter().find(|p| p.device == d).expect("참가자")
    }

    pub fn set_media(&mut self, on: bool) {
        self.media = on;
        self.next_media = self.now;
    }

    /// 신호와 출력이 잠잠해질 때까지 주고받습니다.
    pub fn pump(&mut self) {
        loop {
            let mut progressed = false;
            for i in 0..self.peers.len() {
                while let Some(sig) = self.peers[i].poll_signal() {
                    progressed = true;
                    let from = self.peers[i].device;
                    if let Err(e) = self.sfu.handle_signal(self.now, self.room, from, sig) {
                        self.rejected.push((from, e));
                    }
                }
                while let Some((source, destination, data)) = self.peers[i].poll_transmit() {
                    progressed = true;
                    self.packets.push_back(Packet {
                        at: self.now + LATENCY,
                        source,
                        destination,
                        data,
                    });
                }
            }
            while let Some(o) = self.sfu.poll_output() {
                progressed = true;
                match o {
                    Output::Transmit {
                        source,
                        destination,
                        contents,
                    } => self.packets.push_back(Packet {
                        at: self.now + LATENCY,
                        source,
                        destination,
                        data: contents,
                    }),
                    Output::Signal { to, payload, .. } => {
                        if self.drop_offers_to.contains(&to)
                            && matches!(payload, SignalPayload::SfuOffer { .. })
                        {
                            self.dropped_offers += 1;
                            continue;
                        }
                        let now = self.now;
                        match self.peers.iter_mut().find(|p| p.device == to) {
                            Some(p) => p.handle_signal(now, payload),
                            None => self.stray_signals.push(to),
                        }
                    }
                    Output::Event(e) => self.events.push(e),
                }
            }
            if !progressed {
                return;
            }
        }
    }

    /// 다음 일(패킷 도착·시간 초과·미디어 박자) 하나까지 시계를 돌립니다.
    pub fn step(&mut self) {
        let mut next = self.packets.front().map(|p| p.at);
        let mut consider = |t: Option<Instant>| {
            if let Some(t) = t {
                next = Some(next.map_or(t, |n| n.min(t)));
            }
        };
        consider(self.sfu.poll_timeout());
        for p in &self.peers {
            consider(p.poll_timeout());
        }
        if self.media {
            consider(Some(self.next_media));
        }
        let mut t = next.unwrap_or(self.now + MEDIA_TICK);
        if t <= self.now {
            // "지금 바로" 다시 부르라는 시간 초과가 이어져도 시계는 앞으로 감.
            t = self.now + Duration::from_millis(1);
        }
        self.now = t;
        let now = self.now;
        while self.packets.front().is_some_and(|p| p.at <= now) {
            let p = self.packets.pop_front().expect("패킷");
            if p.destination == self.sfu_addr {
                self.sfu
                    .handle_receive(now, p.source, p.destination, &p.data);
            } else if let Some(peer) = self.peers.iter_mut().find(|x| x.addr == p.destination) {
                peer.handle_receive(now, p.source, p.destination, &p.data);
            }
        }
        if self.sfu.poll_timeout().is_some_and(|t| t <= now) {
            self.sfu.handle_timeout(now);
        }
        for p in &mut self.peers {
            if p.poll_timeout().is_some_and(|t| t <= now) {
                p.handle_timeout(now);
            }
        }
        if self.media && self.next_media <= now {
            let tick = self.media_tick;
            for p in &mut self.peers {
                p.send_media(now, tick);
            }
            self.media_tick += 1;
            self.next_media += MEDIA_TICK;
        }
        self.pump();
    }

    pub fn run_for(&mut self, d: Duration) {
        let until = self.now + d;
        while self.now < until {
            self.step();
        }
    }

    /// 조건이 맞을 때까지(가상 시간 `limit` 안에) 돌립니다.
    pub fn run_until(&mut self, limit: Duration, what: &str, mut f: impl FnMut(&Net) -> bool) {
        let until = self.now + limit;
        while !f(self) {
            assert!(self.now < until, "가상 시간 {limit:?} 안에 {what} 안 됨");
            self.step();
        }
    }

    pub fn all_connected(&self) -> bool {
        self.peers.iter().all(|p| p.connected)
    }

    pub fn sfu_receiving(&self, n: u8) -> usize {
        self.sfu
            .sessions(&self.room)
            .iter()
            .find(|s| s.device == device(n))
            .map_or(0, |s| s.receiving)
    }

    pub fn deliver(&mut self, from: u8, payload: SignalPayload) -> Result<(), SfuError> {
        let r = self
            .sfu
            .handle_signal(self.now, self.room, device(from), payload);
        self.pump();
        r
    }
}
