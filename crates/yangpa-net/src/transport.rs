//! 스웜 종류별 전송 조립.
//!
//! - 직접 연결 스웜(P2P 스웜, `--direct` 주 스웜): 릴레이 회로(+tls+yamux) | DNS(TCP+tls·noise+yamux | QUIC).
//!   Arti는 쓰지 않습니다.
//! - Tor 스웜: Arti(+tls+yamux)만. 직접 TCP·QUIC·시스템 DNS·릴레이 없음.
//!
//! 보안 계층은 libp2p TLS 1.3(QUIC 안의 TLS도 같은 설정)으로, rustls(aws-lc-rs)가 X25519MLKEM768 혼합 키 교환을 먼저
//! 씁니다. 지금 모아 둔 연결을 나중에 양자 컴퓨터로 풀어도 안의 peer ID·크루·이벤트 ID가 드러나지 않게 합니다
//! (Noise는 X25519뿐).

#[cfg(feature = "tor")]
use std::sync::Arc;

use futures::future::{BoxFuture, Either};
use futures::{AsyncRead, AsyncWrite, FutureExt, TryFutureExt};
use libp2p::core::muxing::StreamMuxerBox;
use libp2p::core::transport::{Boxed, Transport};
use libp2p::core::upgrade::{
    InboundConnectionUpgrade, OutboundConnectionUpgrade, UpgradeInfo, Version,
};
use libp2p::{PeerId, dns, noise, quic, relay, tcp, tls, yamux};
use tracing::warn;

use crate::NetError;
use crate::config::NetConfig;
#[cfg(feature = "tor")]
use crate::config::TOR_DIAL_TIMEOUT;

pub(crate) struct BuiltTransport {
    pub transport: Boxed<(PeerId, StreamMuxerBox)>,
    /// 조립한 전송 이름(진단·누출 시험의 구조 검사용, [`crate::NetInspection::transports`]).
    pub names: Vec<&'static str>,
    pub relay_client: Option<relay::client::Behaviour>,
    #[cfg(feature = "tor")]
    pub arti: Option<Arc<crate::tor::ArtiShared>>,
}

/// Arti로 모든 나가는 연결(Tor 스웜).
pub const TRANSPORT_ARTI_ALL: &str = "arti";
/// onion 서비스 수신(Tor 스웜).
pub const TRANSPORT_ONION_LISTEN: &str = "onion-listen";
pub const TRANSPORT_RELAY: &str = "relay";
pub const TRANSPORT_QUIC: &str = "quic";
pub const TRANSPORT_TCP: &str = "tcp";
/// 시스템 DNS로 `/dns` 주소 풀기.
pub const TRANSPORT_SYSTEM_DNS: &str = "dns";

/// `reuse`는 지금 스웜의 Arti 상태입니다. 같은 데이터 경로면 새로 만들지 않고 이어 씁니다: Arti 0.47은 클라이언트를
/// 놓아도 내부 작업이 상태·디렉터리 캐시 잠금을 계속 쥐어, 같은 경로로 새로 만든 클라이언트는 읽기 전용으로 떠
/// 다른 프로세스의 부트스트랩을 끝없이 기다립니다(스웜을 다시 띄운 뒤 진행률 0%).
pub(crate) fn build(
    cfg: &NetConfig,
    #[cfg(feature = "tor")] reuse: Option<Arc<crate::tor::ArtiShared>>,
) -> Result<BuiltTransport, NetError> {
    let plan = cfg.plan();
    let keypair = &cfg.keypair;
    let peer_id = keypair.public().to_peer_id();
    let tls_cfg = tls::Config::new(keypair).map_err(|e| NetError::Transport(e.to_string()))?;
    let noise_cfg = noise::Config::new(keypair).map_err(|e| NetError::Transport(e.to_string()))?;
    let dial_timeout = cfg.dial_timeout();

    #[cfg(feature = "tor")]
    let arti = if plan.onion_dial || plan.tor_dial_all || plan.onion_listen {
        let (state, cache) = (cfg.arti_state_dir(), cfg.arti_cache_dir());
        Some(match &cfg.tor_share {
            // 다른 스웜이 넘긴 Arti(데몬의 출구 스웜이 주 스웜 것을 씀).
            Some(share) => share.0.clone(),
            None => {
                let a = reuse
                    .filter(|a| a.uses_dirs(&state, &cache))
                    .unwrap_or_else(|| crate::tor::ArtiShared::new(state, cache));
                if let Err(e) = a.set_bridges(cfg.tor_bridges.clone()) {
                    warn!(error = %e, "브리지 설정을 쓰지 못함");
                }
                a
            }
        })
    } else {
        None
    };
    #[cfg(feature = "tor")]
    let arti_transport =
        |listen: crate::tor::OnionListen| -> Option<Boxed<(PeerId, StreamMuxerBox)>> {
            let shared = arti.clone()?;
            Some(
                crate::tor::ArtiTransport::new(shared, listen, cfg.tor_isolated)
                    .upgrade(Version::V1Lazy)
                    .authenticate(tls_cfg.clone())
                    .multiplex(yamux::Config::default())
                    .timeout(TOR_DIAL_TIMEOUT)
                    .boxed(),
            )
        };

    if plan.tor_dial_all {
        // Tor 스웜: Arti 하나뿐.
        #[cfg(feature = "tor")]
        {
            let listen = if plan.onion_listen {
                crate::tor::OnionListen::Public
            } else {
                crate::tor::OnionListen::Off
            };
            let transport = arti_transport(listen)
                .ok_or_else(|| NetError::Tor("Arti를 준비하지 못함".into()))?;
            let mut names = vec![TRANSPORT_ARTI_ALL];
            if plan.onion_listen {
                names.push(TRANSPORT_ONION_LISTEN);
            }
            return Ok(BuiltTransport {
                transport,
                names,
                relay_client: None,
                arti,
            });
        }
        #[cfg(not(feature = "tor"))]
        return Err(NetError::Tor("tor 기능 없이 빌드됨".into()));
    }

    // 직접 연결 스웜.
    let make_direct = || {
        let tcp = tcp::tokio::Transport::new(tcp::Config::default().nodelay(true))
            .upgrade(Version::V1Lazy)
            .authenticate(TlsOrNoise(tls_cfg.clone(), noise_cfg.clone()))
            .multiplex(yamux::Config::default())
            .timeout(dial_timeout)
            .boxed();
        let mut quic_cfg = quic::Config::new(keypair);
        quic_cfg.handshake_timeout = dial_timeout;
        let quic = quic::tokio::Transport::new(quic_cfg)
            .map(|(p, c), _| (p, StreamMuxerBox::new(c)))
            .boxed();
        quic.or_transport(tcp)
            .map(|either, _| either.into_inner())
            .boxed()
    };
    let mut names = vec![TRANSPORT_RELAY, TRANSPORT_QUIC, TRANSPORT_TCP];
    let direct = match dns::tokio::Transport::system(make_direct()) {
        Ok(t) => {
            names.push(TRANSPORT_SYSTEM_DNS);
            t.boxed()
        }
        Err(e) => {
            // 일부 플랫폼(안드로이드 등)은 시스템 DNS 설정 파일이 없습니다. 이름 주소만 못 쓸 뿐 나머지는 동작합니다.
            warn!(error = %e, "시스템 DNS 설정을 읽지 못해 /dns 주소 없이 시작");
            make_direct()
        }
    };

    let (relay_transport, relay_behaviour) = relay::client::new(peer_id);
    // 릴레이 회로 너머는 늘 YANGPA 노드라 TLS만 씁니다.
    let relayed = relay_transport
        .upgrade(Version::V1Lazy)
        .authenticate(tls_cfg.clone())
        .multiplex(yamux::Config::default())
        .timeout(dial_timeout)
        .boxed();

    let transport = relayed
        .or_transport(direct)
        .map(|either, _| either.into_inner())
        .boxed();
    Ok(BuiltTransport {
        transport,
        names,
        relay_client: plan.relay_client.then_some(relay_behaviour),
        #[cfg(feature = "tor")]
        arti,
    })
}

/// 직접 TCP의 보안 협상: TLS를 먼저 내밀고, TLS를 모르는 남의 libp2p 노드(`--bootstrap`·지난번 릴레이)에만 Noise로
/// 물러납니다. YANGPA 노드끼리는 늘 TLS가 됩니다.
/// shortcut: 경로 위에서 협상을 가로채는 능동 공격자는 Noise로 낮출 수 있음(모아 두고 나중에 푸는 수동 수집만 막음).
/// 남의 노드를 쓰지 않게 되면 TLS만 남깁니다.
#[derive(Clone)]
struct TlsOrNoise(tls::Config, noise::Config);

type TlsOrNoiseOutput<C> = (PeerId, Either<tls::TlsStream<C>, noise::Output<C>>);

impl UpgradeInfo for TlsOrNoise {
    type Info = &'static str;
    type InfoIter = std::iter::Chain<std::iter::Once<&'static str>, std::iter::Once<&'static str>>;

    fn protocol_info(&self) -> Self::InfoIter {
        self.0.protocol_info().chain(self.1.protocol_info())
    }
}

impl TlsOrNoise {
    fn is_noise(&self, info: &str) -> bool {
        self.1.protocol_info().any(|p| p == info)
    }
}

impl<C> InboundConnectionUpgrade<C> for TlsOrNoise
where
    C: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    type Output = TlsOrNoiseOutput<C>;
    type Error = std::io::Error;
    type Future = BoxFuture<'static, Result<Self::Output, Self::Error>>;

    fn upgrade_inbound(self, sock: C, info: Self::Info) -> Self::Future {
        if self.is_noise(info) {
            self.1
                .upgrade_inbound(sock, info)
                .map_ok(|(p, s)| (p, Either::Right(s)))
                .map_err(std::io::Error::other)
                .boxed()
        } else {
            self.0
                .upgrade_inbound(sock, info)
                .map_ok(|(p, s)| (p, Either::Left(s)))
                .map_err(std::io::Error::other)
                .boxed()
        }
    }
}

impl<C> OutboundConnectionUpgrade<C> for TlsOrNoise
where
    C: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    type Output = TlsOrNoiseOutput<C>;
    type Error = std::io::Error;
    type Future = BoxFuture<'static, Result<Self::Output, Self::Error>>;

    fn upgrade_outbound(self, sock: C, info: Self::Info) -> Self::Future {
        if self.is_noise(info) {
            self.1
                .upgrade_outbound(sock, info)
                .map_ok(|(p, s)| (p, Either::Right(s)))
                .map_err(std::io::Error::other)
                .boxed()
        } else {
            self.0
                .upgrade_outbound(sock, info)
                .map_ok(|(p, s)| (p, Either::Left(s)))
                .map_err(std::io::Error::other)
                .boxed()
        }
    }
}
