//! multiaddr 도우미: 릴레이 경유 주소 판별, Tor로 걸 수 있는 주소, onion 주소 변환.

use libp2p::PeerId;
use libp2p::multiaddr::{Multiaddr, Protocol};

/// 주소에 `/p2p-circuit`이 있는지(릴레이 경유).
pub fn is_relayed(addr: &Multiaddr) -> bool {
    addr.iter().any(|p| matches!(p, Protocol::P2pCircuit))
}

/// 링크·위치 기록에 넣어도 되는 릴레이 경유 주소인지.
///
/// 회로(`/p2p-circuit`) 앞은 릴레이의 주소이므로 IP가 있어도 되지만, 회로 뒤에는 PeerId 말고는
/// 아무것도 오면 안 됩니다(자기 IP가 링크로 새지 않게).
pub fn is_link_safe_relay_addr(addr: &Multiaddr) -> bool {
    let mut after_circuit = false;
    let mut saw_circuit = false;
    for p in addr.iter() {
        if after_circuit {
            if !matches!(p, Protocol::P2p(_)) {
                return false;
            }
        } else if matches!(p, Protocol::P2pCircuit) {
            after_circuit = true;
            saw_circuit = true;
        }
    }
    saw_circuit
}

/// 릴레이 경유 주소에서 릴레이의 PeerId(`/p2p-circuit` 바로 앞의 `/p2p/<id>`).
pub fn relay_peer(addr: &Multiaddr) -> Option<PeerId> {
    let mut last = None;
    for p in addr.iter() {
        match p {
            Protocol::P2p(id) => last = Some(id),
            Protocol::P2pCircuit => return last,
            _ => {}
        }
    }
    None
}

/// 릴레이 경유 주소에서 `/p2p-circuit` 앞부분(릴레이 자신의 주소). 릴레이 경유가 아니면 `None`.
pub fn relay_base(addr: &Multiaddr) -> Option<Multiaddr> {
    is_relayed(addr).then(|| {
        addr.iter()
            .take_while(|p| !matches!(p, Protocol::P2pCircuit))
            .collect()
    })
}

/// 릴레이 경유 주소를 우리 전송(TCP·QUIC v1·DNS)으로 걸기 쉬운 정도. 0: IPv4·DNS, 1: IPv6, 2: 못 거는 전송
/// (WebTransport·WebRTC·WebSocket 등)이거나 릴레이 PeerId가 없는 주소. 회로 앞(릴레이 주소)만 봅니다.
pub fn relay_dial_rank(addr: &Multiaddr) -> u8 {
    if relay_peer(addr).is_none() {
        return 2;
    }
    let mut ip6 = false;
    for p in addr.iter() {
        match p {
            Protocol::P2pCircuit => break,
            Protocol::Ip6(_) | Protocol::Dns6(_) => ip6 = true,
            Protocol::Ip4(_)
            | Protocol::Dns(_)
            | Protocol::Dns4(_)
            | Protocol::Dnsaddr(_)
            | Protocol::Tcp(_)
            | Protocol::Udp(_)
            | Protocol::QuicV1
            | Protocol::P2p(_) => {}
            _ => return 2,
        }
    }
    u8::from(ip6)
}

/// 릴레이 경유 주소를 릴레이마다 번갈아 늘어놓습니다(`order`의 릴레이부터, 릴레이 안에서는 걸기 쉬운 주소부터).
/// 친구에게 보내는 주소·위치 기록은 앞 몇 개뿐이라, 한 릴레이가 광고한 주소로만 채워지지 않게 합니다.
pub fn interleave_relay_addrs(addrs: Vec<Multiaddr>, order: &[PeerId]) -> Vec<Multiaddr> {
    let mut groups: Vec<(Option<PeerId>, Vec<Multiaddr>)> = Vec::new();
    for a in addrs {
        let p = relay_peer(&a);
        match groups.iter_mut().find(|(q, _)| *q == p) {
            Some((_, g)) => g.push(a),
            None => groups.push((p, vec![a])),
        }
    }
    for (_, g) in &mut groups {
        g.sort_by_key(relay_dial_rank);
    }
    groups.sort_by_key(|(p, _)| {
        p.and_then(|p| order.iter().position(|q| *q == p))
            .unwrap_or(usize::MAX)
    });
    let mut out = Vec::new();
    for i in 0.. {
        let before = out.len();
        out.extend(groups.iter().filter_map(|(_, g)| g.get(i).cloned()));
        if out.len() == before {
            break;
        }
    }
    out
}

/// 주소 끝의 `/p2p/<id>`를 떼어 냅니다.
pub fn split_peer_id(addr: &Multiaddr) -> (Multiaddr, Option<PeerId>) {
    let mut a = addr.clone();
    match a.iter().last() {
        Some(Protocol::P2p(id)) => {
            a.pop();
            (a, Some(id))
        }
        _ => (a, None),
    }
}

/// 첫 프로토콜이 onion3인지.
pub fn is_onion(addr: &Multiaddr) -> bool {
    matches!(addr.iter().next(), Some(Protocol::Onion3(_)))
}

/// QUIC·UDP 주소인지(Tor로는 걸 수 없음).
pub fn is_udp(addr: &Multiaddr) -> bool {
    addr.iter()
        .any(|p| matches!(p, Protocol::Udp(_) | Protocol::Quic | Protocol::QuicV1))
}

/// Arti로 접속할 대상.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TorTarget {
    /// `xxxx.onion`과 가상 포트.
    Onion {
        host: String,
        port: u16,
    },
    /// 이름(DNS)은 출구 노드가 풀게 합니다(로컬 DNS 조회로 새지 않게).
    Dns {
        host: String,
        port: u16,
    },
    Ip(std::net::SocketAddr),
}

impl TorTarget {
    pub fn is_onion(&self) -> bool {
        matches!(self, TorTarget::Onion { .. })
    }
}

/// multiaddr를 Arti 접속 대상으로 바꿉니다. `/p2p/<id>` 꼬리는 허용하고, 그 밖의 꼬리가 있으면 거부합니다.
pub fn tor_target(addr: &Multiaddr) -> Option<TorTarget> {
    let (base, _) = split_peer_id(addr);
    let protos: Vec<Protocol<'_>> = base.iter().collect();
    match protos.as_slice() {
        [Protocol::Onion3(o)] => {
            let (host, port) = onion3_host_port(&Protocol::Onion3(o.clone()))?;
            Some(TorTarget::Onion { host, port })
        }
        [
            Protocol::Dns(h) | Protocol::Dns4(h) | Protocol::Dns6(h),
            Protocol::Tcp(port),
        ] => {
            let host = h.to_string();
            // 이름 끝이 .onion이면 onion 서비스로 취급(출구로 보내면 실패만 함).
            if host.to_ascii_lowercase().ends_with(".onion") {
                return Some(TorTarget::Onion {
                    host: host.to_ascii_lowercase(),
                    port: *port,
                });
            }
            Some(TorTarget::Dns { host, port: *port })
        }
        [Protocol::Ip4(ip), Protocol::Tcp(port)] => Some(TorTarget::Ip((*ip, *port).into())),
        [Protocol::Ip6(ip), Protocol::Tcp(port)] => Some(TorTarget::Ip((*ip, *port).into())),
        _ => None,
    }
}

/// onion3 프로토콜에서 `xxxx.onion` 이름과 포트를 꺼냅니다.
fn onion3_host_port(p: &Protocol<'_>) -> Option<(String, u16)> {
    // multiaddr의 표기는 `/onion3/<base32 소문자>:<포트>`로 고정돼 있어 그대로 해석합니다.
    let s = p.to_string();
    let rest = s.strip_prefix("/onion3/")?;
    let (b32, port) = rest.rsplit_once(':')?;
    if b32.len() != 56
        || !b32
            .bytes()
            .all(|c| c.is_ascii_lowercase() || (b'2'..=b'7').contains(&c))
    {
        return None;
    }
    Some((format!("{b32}.onion"), port.parse().ok()?))
}

/// `xxxx.onion` 이름과 포트로 `/onion3/xxxx:port` 주소를 만듭니다.
pub fn onion_multiaddr(onion_host: &str, port: u16) -> Option<Multiaddr> {
    let b32 = onion_host.strip_suffix(".onion").unwrap_or(onion_host);
    format!("/onion3/{b32}:{port}").parse().ok()
}

/// Tor 스웜의 onion 수신 요청을 뜻하는 표식 주소(해시가 모두 0인 onion3). 실제 onion 주소는
/// 서비스가 뜬 뒤 전송 계층이 새 수신 주소로 알려 줍니다.
pub fn onion_listen_marker(port: u16) -> Multiaddr {
    Multiaddr::empty().with(Protocol::Onion3(([0u8; 35], port).into()))
}

/// 표식 주소이면 가상 포트를 돌려줍니다.
pub fn onion_marker_port(addr: &Multiaddr) -> Option<u16> {
    let mut it = addr.iter();
    match (it.next(), it.next()) {
        (Some(Protocol::Onion3(o)), None) if o.hash() == &[0u8; 35] => Some(o.port()),
        _ => None,
    }
}

/// `xxxx.onion:port` 형식(링크의 `ContactInfo.onion`)으로 바꿉니다.
pub fn onion_contact_string(addr: &Multiaddr) -> Option<String> {
    match tor_target(addr)? {
        TorTarget::Onion { host, port } => Some(format!("{host}:{port}")),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ONION: &str = "duckduckgogg42xjoc72x3sjasowoarfbgcmvfimaftt6twagswzczad";

    #[test]
    fn relay_addr_safety() {
        let relay = PeerId::random();
        let me = PeerId::random();
        let ok: Multiaddr = format!("/ip4/1.2.3.4/tcp/4001/p2p/{relay}/p2p-circuit/p2p/{me}")
            .parse()
            .unwrap();
        assert!(is_relayed(&ok));
        assert!(is_link_safe_relay_addr(&ok));
        assert_eq!(relay_peer(&ok), Some(relay));
        assert_eq!(
            relay_base(&ok).unwrap().to_string(),
            format!("/ip4/1.2.3.4/tcp/4001/p2p/{relay}")
        );
        let bad: Multiaddr =
            format!("/ip4/1.2.3.4/tcp/4001/p2p/{relay}/p2p-circuit/ip4/10.0.0.1/tcp/1")
                .parse()
                .unwrap();
        assert!(!is_link_safe_relay_addr(&bad));
        let direct: Multiaddr = "/ip4/10.0.0.1/tcp/1".parse().unwrap();
        assert!(!is_relayed(&direct));
        assert!(!is_link_safe_relay_addr(&direct));
        assert_eq!(relay_peer(&direct), None);
    }

    #[test]
    fn interleave_keeps_both_relays_in_first_eight() {
        let me = PeerId::random();
        let (a, b) = (PeerId::random(), PeerId::random());
        let circ = |relay: &PeerId, base: &str| -> Multiaddr {
            format!("{base}/p2p/{relay}/p2p-circuit/p2p/{me}")
                .parse()
                .unwrap()
        };
        // Kubo 노드처럼 한 릴레이(a)가 걸 수 없는 전송까지 많이 광고함.
        let a_addrs = [
            "/ip4/1.1.1.1/udp/4001/quic-v1/webtransport",
            "/ip4/1.1.1.1/udp/4001/webrtc-direct",
            "/ip4/1.1.1.1/tcp/4001/ws",
            "/ip6/::1/tcp/4001",
            "/ip6/::1/udp/4001/quic-v1",
            "/ip4/1.1.1.1/tcp/4001",
            "/ip4/1.1.1.1/udp/4001/quic-v1",
            "/dns4/a.example/tcp/4001",
            "/ip4/1.1.1.1/tcp/4002",
            "/ip4/1.1.1.1/tcp/4003",
        ];
        let b_addrs = [
            "/ip4/2.2.2.2/tcp/4001",
            "/ip4/2.2.2.2/udp/4001/quic-v1",
            "/ip6/::2/tcp/4001",
        ];
        let mut all: Vec<Multiaddr> = a_addrs.iter().map(|x| circ(&a, x)).collect();
        all.extend(b_addrs.iter().map(|x| circ(&b, x)));
        let out = interleave_relay_addrs(all.clone(), &[b, a]);
        assert_eq!(out.len(), all.len(), "버리는 주소가 없어야 함");
        assert_eq!(out[0], circ(&b, b_addrs[0]));
        let first: Vec<_> = out.iter().take(8).filter_map(relay_peer).collect();
        assert!(first.contains(&a) && first.contains(&b));
        for relay in [a, b] {
            let ranks: Vec<u8> = out
                .iter()
                .filter(|x| relay_peer(x) == Some(relay))
                .map(relay_dial_rank)
                .collect();
            assert!(ranks.windows(2).all(|w| w[0] <= w[1]), "{ranks:?}");
        }
        assert_eq!(relay_dial_rank(&circ(&a, a_addrs[0])), 2);
        assert_eq!(relay_dial_rank(&circ(&a, a_addrs[3])), 1);
        assert_eq!(relay_dial_rank(&circ(&a, a_addrs[7])), 0);
    }

    #[test]
    fn tor_targets() {
        let onion: Multiaddr = format!("/onion3/{ONION}:4001").parse().unwrap();
        assert_eq!(
            tor_target(&onion),
            Some(TorTarget::Onion {
                host: format!("{ONION}.onion"),
                port: 4001
            })
        );
        assert!(is_onion(&onion));
        assert_eq!(
            onion_contact_string(&onion),
            Some(format!("{ONION}.onion:4001"))
        );
        assert_eq!(
            onion_multiaddr(&format!("{ONION}.onion"), 4001),
            Some(onion)
        );

        let dns: Multiaddr = "/dns4/relay.example.org/tcp/443/p2p/12D3KooWD3eckifWpRn9wQpMG9R9hX3sD158z7EqHWmweQAJU5SA".parse().unwrap();
        assert_eq!(
            tor_target(&dns),
            Some(TorTarget::Dns {
                host: "relay.example.org".into(),
                port: 443
            })
        );
        let ip: Multiaddr = "/ip4/1.2.3.4/tcp/80".parse().unwrap();
        assert_eq!(
            tor_target(&ip),
            Some(TorTarget::Ip("1.2.3.4:80".parse().unwrap()))
        );
        let quic: Multiaddr = "/ip4/1.2.3.4/udp/80/quic-v1".parse().unwrap();
        assert!(is_udp(&quic));
        assert_eq!(tor_target(&quic), None);
        let circuit: Multiaddr = "/ip4/1.2.3.4/tcp/80/p2p-circuit".parse().unwrap();
        assert_eq!(tor_target(&circuit), None);
    }

    #[test]
    fn marker_roundtrip() {
        let m = onion_listen_marker(4001);
        assert_eq!(onion_marker_port(&m), Some(4001));
        let real: Multiaddr = format!("/onion3/{ONION}:4001").parse().unwrap();
        assert_eq!(onion_marker_port(&real), None);
    }

    #[test]
    fn split_p2p_suffix() {
        let id = PeerId::random();
        let a: Multiaddr = format!("/ip4/1.2.3.4/tcp/1/p2p/{id}").parse().unwrap();
        let (base, pid) = split_peer_id(&a);
        assert_eq!(pid, Some(id));
        assert_eq!(base.to_string(), "/ip4/1.2.3.4/tcp/1");
    }
}
