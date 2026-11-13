//! libp2p 동작 묶음. 모드에 따라 일부는 [`Toggle`]로 끕니다.

use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::num::NonZeroUsize;
use std::task::{Context, Poll};
use std::time::Duration;

use either::Either;
use libp2p::core::transport::PortUse;
use libp2p::core::{Endpoint, Multiaddr};
use libp2p::kad::store::{MemoryStore, MemoryStoreConfig};
use libp2p::swarm::behaviour::toggle::Toggle;
use libp2p::swarm::{
    ConnectionDenied, ConnectionId, FromSwarm, NetworkBehaviour, NotifyHandler, PeerAddresses,
    THandler, THandlerInEvent, THandlerOutEvent, ToSwarm, dummy,
};
use libp2p::{
    PeerId, StreamProtocol, allow_block_list, autonat, connection_limits, dcutr, gossipsub,
    identify, kad, mdns, memory_connection_limits, ping, relay, request_response,
};
use yangpa_core::wire::RPC_PROTOCOL;

use crate::NetError;
use crate::codec::YpCodec;
use crate::config::{IDENTIFY_PROTOCOL, KAD_PROTOCOL, MAX_RECORD_VALUE, NetConfig, NetMode};
use crate::gossip;

/// 피어별 주소 캐시. `Swarm::add_peer_address`로 넣은 주소(mDNS, `dial` 인자, identify)를
/// 다이얼할 때 돌려줘서, 요청이 자동 다이얼할 때도 같은 주소를 씁니다.
pub(crate) struct AddrBook {
    inner: PeerAddresses,
}

impl AddrBook {
    fn new() -> Self {
        AddrBook {
            inner: PeerAddresses::new(NonZeroUsize::new(1024).expect("0 아님")),
        }
    }

    pub fn get(&mut self, peer: &PeerId) -> Vec<Multiaddr> {
        self.inner.get(peer).collect()
    }
}

impl NetworkBehaviour for AddrBook {
    type ConnectionHandler = dummy::ConnectionHandler;
    type ToSwarm = Infallible;

    fn handle_established_inbound_connection(
        &mut self,
        _: ConnectionId,
        _: PeerId,
        _: &Multiaddr,
        _: &Multiaddr,
    ) -> Result<THandler<Self>, ConnectionDenied> {
        Ok(dummy::ConnectionHandler)
    }

    fn handle_pending_outbound_connection(
        &mut self,
        _: ConnectionId,
        maybe_peer: Option<PeerId>,
        _: &[Multiaddr],
        _: Endpoint,
    ) -> Result<Vec<Multiaddr>, ConnectionDenied> {
        Ok(maybe_peer.map(|p| self.get(&p)).unwrap_or_default())
    }

    fn handle_established_outbound_connection(
        &mut self,
        _: ConnectionId,
        _: PeerId,
        _: &Multiaddr,
        _: Endpoint,
        _: PortUse,
    ) -> Result<THandler<Self>, ConnectionDenied> {
        Ok(dummy::ConnectionHandler)
    }

    fn on_swarm_event(&mut self, event: FromSwarm) {
        self.inner.on_swarm_event(&event);
    }

    fn on_connection_handler_event(
        &mut self,
        _: PeerId,
        _: ConnectionId,
        ev: THandlerOutEvent<Self>,
    ) {
        match ev {}
    }

    fn poll(&mut self, _: &mut Context<'_>) -> Poll<ToSwarm<Self::ToSwarm, THandlerInEvent<Self>>> {
        Poll::Pending
    }
}

/// 감싼 동작(identify)을 onion 연결에서는 쓰지 않습니다. onion 주소는 연락처 링크에 실려 친구가 아닌 사람도 알 수
/// 있는데, 그 연결로 identify가 돌면 수신 주소(직접 연결 스웜이면 IP·중계 주소)를 알려 줍니다. 주소는 인증한 친구에게만
/// 카드로 줍니다(데몬). 또 감싼 동작이 내는 외부 주소 후보 중 onion·빈 주소를 버립니다: onion으로 걸어 온 상대가 그
/// 주소를 '상대가 본 내 주소'로 알려 오면 AutoNAT이 서버(부트스트랩 노드)에, DCUtR이 홀펀칭 상대에게 보내 새어 나갑니다.
/// 지금은 identify를 켜는 직접 연결 스웜에 Arti가 없어 onion 연결이 생기지 않지만, 둘이 한 스웜에 섞여도 새지 않게
/// 둡니다.
pub(crate) struct OnionQuiet<B> {
    inner: B,
    /// onion 연결(감싼 동작에 알리지 않고 빈 처리기를 둔 연결).
    onion: HashSet<ConnectionId>,
    /// 감싼 동작에 알린 연결. 피어 아무 연결로 보내는 알림(`NotifyHandler::Any`)을 이 중 하나로 보냅니다(onion
    /// 연결의 빈 처리기로 가면 처리기 종류가 맞지 않음).
    others: HashMap<PeerId, Vec<ConnectionId>>,
}

impl<B> OnionQuiet<B> {
    pub fn new(inner: B) -> Self {
        OnionQuiet {
            inner,
            onion: HashSet::new(),
            others: HashMap::new(),
        }
    }
}

impl<B: NetworkBehaviour> NetworkBehaviour for OnionQuiet<B> {
    type ConnectionHandler = Either<B::ConnectionHandler, dummy::ConnectionHandler>;
    type ToSwarm = B::ToSwarm;

    fn handle_pending_inbound_connection(
        &mut self,
        id: ConnectionId,
        local_addr: &Multiaddr,
        remote_addr: &Multiaddr,
    ) -> Result<(), ConnectionDenied> {
        self.inner
            .handle_pending_inbound_connection(id, local_addr, remote_addr)
    }

    fn handle_established_inbound_connection(
        &mut self,
        id: ConnectionId,
        peer: PeerId,
        local_addr: &Multiaddr,
        remote_addr: &Multiaddr,
    ) -> Result<THandler<Self>, ConnectionDenied> {
        if crate::addr::is_onion(local_addr) {
            self.onion.insert(id);
            return Ok(Either::Right(dummy::ConnectionHandler));
        }
        self.inner
            .handle_established_inbound_connection(id, peer, local_addr, remote_addr)
            .map(Either::Left)
    }

    fn handle_pending_outbound_connection(
        &mut self,
        id: ConnectionId,
        maybe_peer: Option<PeerId>,
        addresses: &[Multiaddr],
        role: Endpoint,
    ) -> Result<Vec<Multiaddr>, ConnectionDenied> {
        self.inner
            .handle_pending_outbound_connection(id, maybe_peer, addresses, role)
    }

    fn handle_established_outbound_connection(
        &mut self,
        id: ConnectionId,
        peer: PeerId,
        addr: &Multiaddr,
        role: Endpoint,
        port_use: PortUse,
    ) -> Result<THandler<Self>, ConnectionDenied> {
        if crate::addr::is_onion(addr) {
            self.onion.insert(id);
            return Ok(Either::Right(dummy::ConnectionHandler));
        }
        self.inner
            .handle_established_outbound_connection(id, peer, addr, role, port_use)
            .map(Either::Left)
    }

    fn on_swarm_event(&mut self, event: FromSwarm) {
        match event {
            FromSwarm::ConnectionEstablished(e) => {
                if self.onion.contains(&e.connection_id) {
                    return;
                }
                self.others
                    .entry(e.peer_id)
                    .or_default()
                    .push(e.connection_id);
            }
            FromSwarm::ConnectionClosed(e) => {
                if self.onion.remove(&e.connection_id) {
                    return;
                }
                if let Some(ids) = self.others.get_mut(&e.peer_id) {
                    ids.retain(|id| *id != e.connection_id);
                    if ids.is_empty() {
                        self.others.remove(&e.peer_id);
                    }
                }
            }
            FromSwarm::AddressChange(e) if self.onion.contains(&e.connection_id) => return,
            _ => {}
        }
        self.inner.on_swarm_event(event);
    }

    fn on_connection_handler_event(
        &mut self,
        peer: PeerId,
        id: ConnectionId,
        ev: THandlerOutEvent<Self>,
    ) {
        match ev {
            Either::Left(ev) => self.inner.on_connection_handler_event(peer, id, ev),
            Either::Right(never) => match never {},
        }
    }

    fn poll(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<ToSwarm<Self::ToSwarm, THandlerInEvent<Self>>> {
        loop {
            let ev = match self.inner.poll(cx) {
                Poll::Ready(ev) => ev,
                Poll::Pending => return Poll::Pending,
            };
            match ev {
                ToSwarm::NewExternalAddrCandidate(a)
                    if a.is_empty() || crate::addr::is_onion(&a) => {}
                ToSwarm::NotifyHandler {
                    peer_id,
                    handler,
                    event,
                } => {
                    let target = match handler {
                        NotifyHandler::One(id) if self.onion.contains(&id) => None,
                        NotifyHandler::One(id) => Some(id),
                        NotifyHandler::Any => self
                            .others
                            .get(&peer_id)
                            .and_then(|ids| ids.first())
                            .copied(),
                    };
                    if let Some(id) = target {
                        return Poll::Ready(ToSwarm::NotifyHandler {
                            peer_id,
                            handler: NotifyHandler::One(id),
                            event: Either::Left(event),
                        });
                    }
                }
                other => return Poll::Ready(other.map_in(Either::Left)),
            }
        }
    }
}

// rustc 1.100부터 libp2p 파생 코드(derive가 만든 impl)의 빈(Infallible) 이벤트 갈래를 도달 불가로 봅니다.
// 파생 impl에는 구조체의 lint 속성이 닿지 않아 모듈로 감쌉니다.
#[allow(unreachable_code)]
mod derived {
    use super::*;

    /// 노드의 전체 동작. 연결 제한·차단 목록을 앞에 둬서 다른 동작보다 먼저 거절합니다.
    #[derive(NetworkBehaviour)]
    pub(crate) struct YpBehaviour {
        pub limits: connection_limits::Behaviour,
        pub memory: memory_connection_limits::Behaviour,
        pub blocked: allow_block_list::Behaviour<allow_block_list::BlockedPeers>,
        pub addrbook: AddrBook,
        pub rpc: request_response::Behaviour<YpCodec>,
        /// 통화 미디어 프레임(`media.rs`). RPC와 따로라 서로의 동시 요청 수를 쓰지 않습니다.
        pub media: request_response::Behaviour<crate::media::MediaCodec>,
        pub gossip: gossipsub::Behaviour<gossipsub::IdentityTransform, gossip::SubscriptionFilter>,
        pub kad: kad::Behaviour<MemoryStore>,
        pub ping: ping::Behaviour,
        pub identify: Toggle<OnionQuiet<identify::Behaviour>>,
        pub relay_client: Toggle<relay::client::Behaviour>,
        pub relay_server: Toggle<relay::Behaviour>,
        pub dcutr: Toggle<dcutr::Behaviour>,
        pub autonat: Toggle<autonat::Behaviour>,
        pub mdns: Toggle<mdns::tokio::Behaviour>,
        pub upnp: Toggle<libp2p::upnp::tokio::Behaviour>,
    }
}
pub(crate) use derived::{YpBehaviour, YpBehaviourEvent};

/// 피어당 최대 연결 수(직접 + 릴레이 경유 + 홀펀칭 중 여유). P2P 스웜에서 릴레이 경유로 이어진 상대와 홀펀칭하는
/// 동안 둘이 함께 있습니다(onion 연결은 Tor 스웜에만 있어 이것들과 섞이지 않음).
const MAX_CONNECTIONS_PER_PEER: u32 = 4;
/// 연결 하나에서 동시에 열 수 있는 요청 스트림 수(보내는 것과 받는 것 합계). 넘치면 libp2p가 스트림을 그냥
/// 버려 입출력 오류가 나므로, 이 노드가 보내는 요청은 피어당 [`crate::task::MAX_OUTBOUND_PER_PEER`]개로
/// 따로 막습니다. 큰 요청의 메모리는 코덱의 읽기 예산과 스웜 작업의 대기 바이트 상한이 막습니다.
const MAX_CONCURRENT_RPC_STREAMS: usize = 2 * crate::task::MAX_OUTBOUND_PER_PEER;

impl YpBehaviour {
    pub fn new(
        cfg: &NetConfig,
        relay_client: Option<relay::client::Behaviour>,
    ) -> Result<Self, NetError> {
        let plan = cfg.plan();
        let keypair = &cfg.keypair;
        let peer_id = keypair.public().to_peer_id();
        let tor = cfg.mode == NetMode::Tor;

        let max = cfg.max_connections.max(1);
        let limits = connection_limits::Behaviour::new(
            connection_limits::ConnectionLimits::default()
                .with_max_established(Some(max))
                .with_max_pending_incoming(Some(max.div_ceil(2).max(8)))
                .with_max_pending_outgoing(Some(max.div_ceil(2).max(8)))
                .with_max_established_per_peer(Some(MAX_CONNECTIONS_PER_PEER)),
        );

        let rpc = request_response::Behaviour::with_codec(
            YpCodec::new(cfg.traffic.clone()),
            [(
                StreamProtocol::new(RPC_PROTOCOL),
                request_response::ProtocolSupport::Full,
            )],
            request_response::Config::default()
                .with_request_timeout(cfg.request_timeout)
                .with_max_concurrent_streams(MAX_CONCURRENT_RPC_STREAMS),
        );

        let media = request_response::Behaviour::with_codec(
            crate::media::MediaCodec::new(crate::codec::ReadBudget::new(
                crate::media::MAX_INFLIGHT_MEDIA,
            )),
            [(
                StreamProtocol::new(crate::media::MEDIA_PROTOCOL),
                request_response::ProtocolSupport::Full,
            )],
            // 늦은 소리는 쓸모가 없어 짧게 기다립니다. 받는 쪽은 바로 답합니다.
            request_response::Config::default()
                .with_request_timeout(Duration::from_secs(10))
                .with_max_concurrent_streams(4 * crate::media::MAX_IN_FLIGHT),
        );

        let gossip_cfg = gossip::build_config().map_err(NetError::InvalidInput)?;
        let mut gossip = gossipsub::Behaviour::new_with_subscription_filter(
            // 출처를 싣지 않습니다(gossip 모듈 머리말).
            gossipsub::MessageAuthenticity::Anonymous,
            gossip_cfg,
            gossip::subscription_filter(),
        )
        .map_err(|e| NetError::InvalidInput(e.to_string()))?;
        let (score, thresholds) = gossip::score_params();
        gossip
            .with_peer_score(score, thresholds)
            .map_err(NetError::InvalidInput)?;

        let mut kad_cfg = kad::Config::new(StreamProtocol::new(KAD_PROTOCOL));
        kad_cfg
            .set_record_filtering(kad::StoreInserts::FilterBoth)
            // 위치 기록 키는 하루마다 바뀌므로 길게 둘 필요가 없습니다.
            .set_record_ttl(Some(Duration::from_secs(36 * 60 * 60)))
            .set_publication_interval(Some(Duration::from_secs(12 * 60 * 60)))
            .set_provider_publication_interval(None)
            .set_query_timeout(if tor {
                Duration::from_secs(120)
            } else {
                Duration::from_secs(45)
            })
            .set_periodic_bootstrap_interval(Some(if tor {
                Duration::from_secs(30 * 60)
            } else {
                Duration::from_secs(5 * 60)
            }));
        let store = MemoryStore::with_config(
            peer_id,
            MemoryStoreConfig {
                max_records: 4096,
                max_value_bytes: MAX_RECORD_VALUE,
                max_providers_per_key: 0,
                max_provided_keys: 0,
            },
        );
        let mut kad = kad::Behaviour::with_config(peer_id, store, kad_cfg);
        // Tor 스웜·클라이언트 지정(P2P 스웜 포함): DHT에 자기 주소를 올리지 않도록 서버 역할을 하지 않습니다.
        if plan.kad_client_only {
            kad.set_mode(Some(kad::Mode::Client));
        } else if cfg.kad_mode == crate::config::KadMode::Server {
            kad.set_mode(Some(kad::Mode::Server));
        }

        let ping_interval = if tor {
            Duration::from_secs(60)
        } else {
            Duration::from_secs(15)
        };
        let ping = ping::Behaviour::new(
            ping::Config::new()
                .with_interval(ping_interval)
                .with_timeout(Duration::from_secs(if tor { 60 } else { 20 })),
        );

        let identify = plan.identify.then(|| {
            OnionQuiet::new(identify::Behaviour::new(
                identify::Config::new(IDENTIFY_PROTOCOL.to_string(), keypair.public())
                    .with_agent_version("yangpa".to_string())
                    .with_push_listen_addr_updates(true)
                    .with_cache_size(256),
            ))
        });

        let relay_server = plan.relay_server.then(|| {
            let d = relay::Config::default();
            relay::Behaviour::new(
                peer_id,
                relay::Config {
                    max_reservations: 256,
                    max_circuits: 64,
                    // 홀펀칭이 실패해도 텍스트 대화는 릴레이로 이어지도록 기본값(2분·128KiB)보다 넉넉히.
                    max_circuit_duration: Duration::from_secs(10 * 60),
                    max_circuit_bytes: 4 * 1024 * 1024,
                    reservation_rate_limiters: skip_loopback(d.reservation_rate_limiters),
                    circuit_src_rate_limiters: skip_loopback(d.circuit_src_rate_limiters),
                    ..d
                },
            )
        });

        let dcutr = plan.dcutr.then(|| dcutr::Behaviour::new(peer_id));
        let autonat = plan.autonat.then(|| {
            autonat::Behaviour::new(
                peer_id,
                autonat::Config {
                    boot_delay: Duration::from_secs(5),
                    retry_interval: Duration::from_secs(60),
                    refresh_interval: Duration::from_secs(15 * 60),
                    ..Default::default()
                },
            )
        });
        let mdns = if plan.mdns {
            match mdns::tokio::Behaviour::new(mdns::Config::default(), peer_id) {
                Ok(m) => Some(m),
                Err(e) => {
                    // 네트워크 인터페이스 감시를 못 해도 나머지는 동작해야 합니다.
                    tracing::warn!(error = %e, "mDNS를 켜지 못함");
                    None
                }
            }
        } else {
            None
        };

        Ok(YpBehaviour {
            limits,
            memory: memory_connection_limits::Behaviour::with_max_percentage(0.9),
            blocked: allow_block_list::Behaviour::default(),
            addrbook: AddrBook::new(),
            rpc,
            media,
            gossip,
            kad,
            ping,
            identify: identify.into(),
            relay_client: relay_client.filter(|_| plan.relay_client).into(),
            relay_server: relay_server.into(),
            dcutr: dcutr.into(),
            autonat: autonat.into(),
            mdns: mdns.into(),
            upnp: plan
                .upnp
                .then(libp2p::upnp::tokio::Behaviour::default)
                .into(),
        })
    }
}

/// 릴레이 서버의 요청 제한(기본: 피어마다, IP마다)에서 같은 기계(루프백)의 연결은 뺍니다. 한 기계에 노드 여럿을 띄우면
/// 모두 127.0.0.1 하나의 몫(한 번에 60, 그 뒤 1분에 하나)을 나눠 써서, 꺼진 노드에 거는 재시도가 다른 노드의 회로까지
/// 몇 분씩 막았습니다(바쁜 Windows CI의 시험 시간 초과).
fn skip_loopback(limiters: Vec<Box<dyn relay::RateLimiter>>) -> Vec<Box<dyn relay::RateLimiter>> {
    limiters
        .into_iter()
        .map(|mut l| -> Box<dyn relay::RateLimiter> {
            Box::new(move |peer, addr: &Multiaddr, now| {
                let local = match addr.iter().next() {
                    Some(libp2p::multiaddr::Protocol::Ip4(ip)) => ip.is_loopback(),
                    Some(libp2p::multiaddr::Protocol::Ip6(ip)) => ip.is_loopback(),
                    _ => false,
                };
                local || l.try_next(peer, addr, now)
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::NetConfig;
    use libp2p::identity::Keypair;

    #[test]
    fn relay_limits_skip_loopback_only() {
        let mut ls = skip_loopback(relay::Config::default().circuit_src_rate_limiters);
        let now = std::time::Instant::now();
        let peer = PeerId::random();
        let mut allowed = |addr: &str| {
            let a: Multiaddr = addr.parse().unwrap();
            (0..100)
                .filter(|_| ls.iter_mut().all(|l| l.try_next(peer, &a, now)))
                .count()
        };
        assert_eq!(allowed("/ip4/127.0.0.1/tcp/1"), 100);
        assert_eq!(allowed("/ip6/::1/tcp/1"), 100);
        assert!(
            allowed("/ip4/203.0.113.5/tcp/1") < 100,
            "바깥 주소는 그대로 제한"
        );
    }

    #[tokio::test]
    async fn tor_mode_behaviour_has_leaky_parts_disabled() {
        let mut cfg = NetConfig::new(
            NetMode::Tor,
            Keypair::generate_ed25519(),
            "/tmp/yangpa-net-test",
        );
        cfg.mdns = true;
        cfg.relay_server = true;
        let (_, relay_client) = relay::client::new(cfg.keypair.public().to_peer_id());
        let b = YpBehaviour::new(&cfg, Some(relay_client)).unwrap();
        assert!(!b.mdns.is_enabled());
        assert!(!b.autonat.is_enabled());
        assert!(!b.dcutr.is_enabled());
        assert!(!b.identify.is_enabled());
        assert!(!b.relay_client.is_enabled());
        assert!(!b.relay_server.is_enabled());
        assert_eq!(b.kad.mode(), kad::Mode::Client);
    }

    #[tokio::test]
    async fn normal_mode_behaviour_enables_nat_traversal() {
        let mut cfg = NetConfig::new(
            NetMode::Normal,
            Keypair::generate_ed25519(),
            "/tmp/yangpa-net-test",
        );
        cfg.mdns = false;
        cfg.kad_mode = crate::config::KadMode::Server;
        let (_, relay_client) = relay::client::new(cfg.keypair.public().to_peer_id());
        let b = YpBehaviour::new(&cfg, Some(relay_client)).unwrap();
        assert!(b.identify.is_enabled() && b.autonat.is_enabled() && b.dcutr.is_enabled());
        assert!(b.relay_client.is_enabled());
        assert!(!b.relay_server.is_enabled());
        assert!(!b.mdns.is_enabled());
        assert_eq!(b.kad.mode(), kad::Mode::Server);
        assert_eq!(b.kad.protocol_names(), &[StreamProtocol::new(KAD_PROTOCOL)]);
    }

    /// 외부 주소 후보만 차례로 내는 동작.
    struct Candidates(std::collections::VecDeque<Multiaddr>);

    impl NetworkBehaviour for Candidates {
        type ConnectionHandler = dummy::ConnectionHandler;
        type ToSwarm = Infallible;

        fn handle_established_inbound_connection(
            &mut self,
            _: ConnectionId,
            _: PeerId,
            _: &Multiaddr,
            _: &Multiaddr,
        ) -> Result<THandler<Self>, ConnectionDenied> {
            Ok(dummy::ConnectionHandler)
        }

        fn handle_established_outbound_connection(
            &mut self,
            _: ConnectionId,
            _: PeerId,
            _: &Multiaddr,
            _: Endpoint,
            _: PortUse,
        ) -> Result<THandler<Self>, ConnectionDenied> {
            Ok(dummy::ConnectionHandler)
        }

        fn on_swarm_event(&mut self, _: FromSwarm) {}

        fn on_connection_handler_event(
            &mut self,
            _: PeerId,
            _: ConnectionId,
            ev: THandlerOutEvent<Self>,
        ) {
            match ev {}
        }

        fn poll(
            &mut self,
            _: &mut Context<'_>,
        ) -> Poll<ToSwarm<Self::ToSwarm, THandlerInEvent<Self>>> {
            match self.0.pop_front() {
                Some(a) => Poll::Ready(ToSwarm::NewExternalAddrCandidate(a)),
                None => Poll::Pending,
            }
        }
    }

    /// onion으로 걸어 온 상대가 그 주소를 '본 내 주소'로 알려 와도 후보로 남지 않아야 합니다(AutoNAT·DCUtR로 새지 않게).
    #[test]
    fn onion_and_empty_candidates_are_dropped() {
        let onion: Multiaddr =
            "/onion3/duckduckgogg42xjoc72x3sjasowoarfbgcmvfimaftt6twagswzczad:4001"
                .parse()
                .unwrap();
        let tcp: Multiaddr = "/ip4/1.2.3.4/tcp/4001".parse().unwrap();
        let mut b = OnionQuiet::new(Candidates(
            vec![onion, Multiaddr::empty(), tcp.clone()].into(),
        ));
        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        match b.poll(&mut cx) {
            Poll::Ready(ToSwarm::NewExternalAddrCandidate(a)) => assert_eq!(a, tcp),
            _ => panic!("일반 주소 후보는 그대로 나와야 함"),
        }
        assert!(b.poll(&mut cx).is_pending());
    }

    /// onion 연결에는 감싼 동작(identify)의 처리기를 두지 않습니다(그 연결로 내 수신 주소를 알리지 않게).
    #[test]
    fn onion_connections_get_no_inner_handler() {
        let onion: Multiaddr =
            "/onion3/duckduckgogg42xjoc72x3sjasowoarfbgcmvfimaftt6twagswzczad:4001"
                .parse()
                .unwrap();
        let tcp: Multiaddr = "/ip4/1.2.3.4/tcp/4001".parse().unwrap();
        let peer = PeerId::random();
        let mut b = OnionQuiet::new(Candidates(Default::default()));
        let (a, c) = (
            ConnectionId::new_unchecked(1),
            ConnectionId::new_unchecked(2),
        );
        let dialed = b
            .handle_established_outbound_connection(a, peer, &onion, Endpoint::Dialer, PortUse::New)
            .unwrap();
        assert!(matches!(dialed, Either::Right(_)));
        let received = b
            .handle_established_inbound_connection(c, peer, &onion, &Multiaddr::empty())
            .unwrap();
        assert!(matches!(received, Either::Right(_)));
        let direct = b
            .handle_established_outbound_connection(
                ConnectionId::new_unchecked(3),
                peer,
                &tcp,
                Endpoint::Dialer,
                PortUse::New,
            )
            .unwrap();
        assert!(matches!(direct, Either::Left(_)));
        assert_eq!(b.onion.len(), 2);
    }
}
