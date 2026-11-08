//! Tor·네트워크 설정.
//!
//! - 앱 사용자 기기는 늘 Tor로 돕니다(decisions.md). 직접 연결은 P2P 전용 채널 안에서만 따로 띄운 스웜으로 합니다.
//! - [`net_config`]: 네트워크를 띄우기 직전의 설정. 데몬 시작 옵션과 Tor 설정, 네트워크 키(`secret_names::NET_KEY_TOR`)로
//!   만듭니다. Tor를 준비할 수 없으면 오류로 시작을 멈춥니다(직접 연결로 조용히 떠서 IP가 드러나지 않게).
//!   `NetOptions::direct`(상시 시더·중계 서버·시험)만 직접 연결 스웜으로 뜹니다.
//! - 예전 빌드의 일반 모드·토르망 설정, 없어진 설정(IP 숨기기·저전력·상시 노드·통화 STUN·TURN), 일반 모드 네트워크
//!   키는 시작할 때 지웁니다([`migrate_legacy`]).

use std::sync::atomic::{AtomicBool, Ordering};

use tracing::info;
use yangpa_core::links::ContactInfo;
use yangpa_core::wire::NodeContact;
use yangpa_net::{Keypair, Multiaddr, NetConfig, NetMode, addr};
use yangpa_rpc::api::TorStatus;
use yangpa_store::{Store, secret_names};
use zeroize::Zeroizing;

use crate::config::NodeConfig;
use crate::error::{Error, Result};
use crate::node::Node;

/// 예전 빌드의 설정. 시작할 때 지웁니다: 고른 모드, 토르망, 친구 전용 onion, 일반 모드에서 예약했던 릴레이, 그리고
/// 없어진 설정인 IP 숨기기·저전력·직접 등록한 상시 노드·IPFS 공개 노드 사용·통화 STUN·TURN 서버, 2026-11-07에 없앤 TURN 서버·TURN으로 보낼 종류(decisions.md).
pub(crate) const LEGACY_SETTINGS: [&str; 11] = [
    "net.tor",
    "net.tor_open",
    "net.friend_onion",
    "relay.sticky",
    "net.hide_ip",
    "net.low_power",
    "net.bootstrap",
    "net.public_nodes",
    "net.ice_servers",
    "turn.servers",
    "turn.kinds",
];

/// Tor 영역의 노드 수준 상태(`Node::tor`).
#[derive(Default)]
pub struct TorState {
    /// 시험용: 직접 연결 스웜이지만 통화·SFU 정책은 Tor처럼 봅니다([`simulate_tor`]).
    simulated: AtomicBool,
}

/// 네트워크를 띄울 설정. 저장된 설정을 읽을 수 없으면 시작을 멈춥니다.
pub(crate) fn net_config(cfg: &NodeConfig, store: &Store) -> Result<NetConfig> {
    migrate_legacy(cfg, store);
    let ncfg = if cfg.net.direct {
        direct_config(cfg, store)?
    } else {
        let t = tor_config(cfg, store)?;
        info!("Tor로 시작");
        t
    };
    Ok(ncfg)
}

/// 예전 빌드가 남긴 일반 모드·토르망 설정을 지우고, Tor로 도는 기기는 일반 모드 네트워크 키도 지웁니다(그 PeerId로
/// IP를 드러내던 노드와 이어지지 않게). 지우지 못해도 쓰지 않는 값이라 시작은 계속합니다.
fn migrate_legacy(cfg: &NodeConfig, store: &Store) {
    for key in LEGACY_SETTINGS {
        let _ = store.delete_setting(key);
    }
    if !cfg.net.direct {
        let _ = store.delete_secret(secret_names::NET_KEY);
    }
}

/// 직접 연결 스웜 설정(상시 시더·중계 서버·시험).
fn direct_config(cfg: &NodeConfig, store: &Store) -> Result<NetConfig> {
    let key = load_key(store, secret_names::NET_KEY)?;
    let mut n = NetConfig::new(NetMode::Normal, key, &cfg.data_dir);
    if !cfg.net.listen.is_empty() {
        n.listen = cfg.net.listen.clone();
    }
    n.bootstrap = cfg.net.bootstrap.clone();
    n.mdns = cfg.net.mdns;
    n.relay_server = cfg.net.relay_server;
    n.external_addrs = cfg.net.external_addrs.clone();
    n.kad_mode = cfg.net.kad_mode;
    n.request_timeout = cfg.net.request_timeout;
    Ok(n)
}

/// Tor 설정(수신 주소·mDNS·릴레이 서버는 쓰지 않음).
fn tor_config(cfg: &NodeConfig, store: &Store) -> Result<NetConfig> {
    let key = load_key(store, secret_names::NET_KEY_TOR)?;
    let mut t = NetConfig::new(NetMode::Tor, key, &cfg.data_dir);
    if !t.plan().tor_dial_all {
        return Err(Error::invalid(
            "이 빌드는 Tor를 지원하지 않습니다(직접 연결로 시작하면 IP가 드러나므로 시작하지 않습니다)",
        ));
    }
    // Arti 상태 디렉터리를 미리 만들어 봅니다. Arti는 백그라운드에서 뜨므로, 여기서 확인하지 않으면 Tor 없이
    // 네트워크가 '켜진' 채로 남습니다.
    for dir in [t.arti_state_dir(), t.arti_cache_dir()] {
        create_private_dir(&dir).map_err(|e| {
            Error::invalid(format!(
                "Tor 상태 디렉터리를 만들 수 없어 시작하지 않습니다: {e}"
            ))
        })?;
    }
    t.bootstrap = cfg.net.bootstrap.clone();
    t.tor_bridges = crate::bridges::configured(store);
    t.request_timeout = t.request_timeout.max(cfg.net.request_timeout);
    Ok(t)
}

fn create_private_dir(dir: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// 네트워크 키(Tor는 `NET_KEY_TOR`, 직접 연결은 `NET_KEY`). 처음이면 만들어 저장합니다.
fn load_key(store: &Store, name: &str) -> Result<Keypair> {
    let seed = match store.get_secret(name)? {
        Some(s) if s.len() == 32 => Zeroizing::new(s.to_vec()),
        _ => {
            let s = yangpa_core::body::Secret32::random();
            store.put_secret(name, &s.0)?;
            Zeroizing::new(s.0.to_vec())
        }
    };
    let mut bytes = seed.clone();
    Keypair::ed25519_from_bytes(&mut bytes[..])
        .map_err(|_| Error::internal("네트워크 키 형식이 잘못됨"))
}

/// 이 노드가 Tor로 도는지(직접 연결 스웜은 시험에서 Tor처럼 볼 때만). 통화·내려받기·미리보기 정책은 이것으로 정합니다.
pub(crate) fn tor_active(node: &Node) -> bool {
    !node.cfg.net.direct || node.tor.simulated.load(Ordering::Relaxed)
}

pub(crate) fn status(node: &Node) -> TorStatus {
    let s = node.net.last_status();
    TorStatus {
        enabled: s.mode == NetMode::Tor || tor_active(node),
        bootstrap_progress: s.tor_bootstrap,
        onion_addr: s.onion_addr.as_ref().map(|a| a.to_string()),
    }
}

/// 링크(`ContactInfo`)에서 걸 주소: 릴레이 경유 주소와 onion 주소. onion은 문서 형식(`xxxx.onion:port`)과
/// 예전 빌드가 넣던 multiaddr(`/onion3/…`) 둘 다 읽습니다. 릴레이 경유 주소는 직접 연결 노드가 만든 링크에만 있는데,
/// Tor 스웜은 릴레이 클라이언트가 꺼져 있어 그 주소에는 걸지 못하고 onion만 씁니다.
pub(crate) fn contact_addrs(contact: &ContactInfo) -> Vec<Multiaddr> {
    let mut out: Vec<Multiaddr> = contact
        .relay_addrs
        .iter()
        .filter_map(|a| a.parse().ok())
        .collect();
    if let Some(o) = contact.onion.as_deref().and_then(parse_onion_contact) {
        out.push(o);
    }
    out
}

/// onion 자리의 값은 onion3 하나(끝의 `/p2p/<id>`만 허용)일 때만 씁니다. `/onion3/…/ip4/…/tcp/…`처럼 뒤에 평문
/// 주소를 붙이면 TCP 전송이 끝의 IP·포트만 보고 그곳에 직접 걸므로(링크로 내부망 훑기) 버립니다.
pub(crate) fn parse_onion_contact(s: &str) -> Option<Multiaddr> {
    let a = match s.parse::<Multiaddr>() {
        Ok(a) => a,
        Err(_) => {
            let (host, port) = s.rsplit_once(':')?;
            addr::onion_multiaddr(host, port.parse().ok()?)?
        }
    };
    let onion_only = addr::is_onion(&a) && addr::tor_target(&a).is_some_and(|t| t.is_onion());
    onion_only.then_some(a)
}

/// 링크에 넣을 내 onion 주소(`xxxx.onion:port`).
pub(crate) fn contact_onion(onion_addr: Option<&Multiaddr>) -> Option<String> {
    onion_addr.and_then(addr::onion_contact_string)
}

/// 연락처 링크·친구 카드에 넣을 이 기기의 onion 주소(`xxxx.onion:port`). Tor 부트스트랩 전에도 키에서 나옵니다.
/// 직접 연결 스웜은 onion이 없습니다.
pub(crate) async fn device_onion(node: &Node) -> Option<String> {
    if node.cfg.net.direct {
        return None;
    }
    if let Some(o) = contact_onion(node.net.last_status().onion_addr.as_ref()) {
        return Some(o);
    }
    let host = node.net.device_onion_host().await?;
    Some(format!("{host}:{}", yangpa_net::config::DEFAULT_ONION_PORT))
}

/// Tor로 돌 때의 블롭 내려받기 한도. 피어 연결 하나(회로 하나)를 메시지·동기화와 같이 쓰므로 동시에 받는 블롭은
/// 하나로 둡니다. 조각(256KiB) 요청 하나에 Tor 왕복이 세 번쯤 들므로(스트림 협상, 요청·답, 흐름 창 기다림) 한 번에
/// 하나만 보내면 초당 수십 KiB에 머뭅니다. 피어 하나에 세 개까지 겹쳐 보내 왕복을 겹치게 합니다. 글 답은 yamux 프레임으로
/// 섞여 나가 조각 뒤에 오래 밀리지 않습니다(decisions.md).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TorDownloadLimits {
    /// 동시에 내려받는 블롭 수.
    pub active: usize,
    /// 피어 하나에 동시에 보낸 조각 요청 수.
    pub per_peer: u32,
    /// 블롭 하나에서 동시에 보낸 조각 요청 수.
    pub total: u32,
    /// 조각 요청 시간 제한(Tor 스웜의 네트워크 요청 제한보다 짧으면 늦게 온 답을 버리고 다시 요청하게 됨).
    pub request_timeout: std::time::Duration,
}

/// Tor로 돌면 블롭 내려받기 한도, 직접 연결 스웜이면 `None`.
pub(crate) fn tor_download_limits(node: &Node) -> Option<TorDownloadLimits> {
    tor_active(node).then(|| TorDownloadLimits {
        active: 1,
        per_peer: 3,
        total: 4,
        // yangpa-net의 Tor 스웜 요청 제한(90초)과 설정값 중 큰 쪽(`tor_config`와 같은 규칙) + 여유.
        request_timeout: node
            .cfg
            .net
            .request_timeout
            .max(std::time::Duration::from_secs(90))
            + std::time::Duration::from_secs(5),
    })
}

/// 시험용: 직접 연결 스웜을 Tor처럼 봅니다(네트워크는 그대로). 통화·SFU 정책을 Tor 망 없이 볼 때.
pub(crate) fn simulate_tor(node: &Node, on: bool) {
    node.tor.simulated.store(on, Ordering::Relaxed);
}

/// 이 기기의 노드(PeerId와 그 노드가 연결받는 onion 주소). 친구·내 기기에게만 줍니다. 예전 빌드는 두 모드의 노드를
/// 실었으므로 받는 쪽은 여럿을 읽습니다(`peers::record_node_contacts`).
pub(crate) async fn node_contacts(node: &Node) -> Vec<NodeContact> {
    vec![NodeContact {
        peer_id: node.peer_id().to_bytes(),
        onion: device_onion(node).await,
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use yangpa_net::PeerId;

    const ONION: &str = "duckduckgogg42xjoc72x3sjasowoarfbgcmvfimaftt6twagswzczad";

    #[test]
    fn contact_onion_is_dialed() {
        let relay = format!("/ip4/1.2.3.4/tcp/4001/p2p/{}/p2p-circuit", PeerId::random());
        let onion_ma: Multiaddr = format!("/onion3/{ONION}:4001").parse().unwrap();
        // 문서 형식(`xxxx.onion:port`)과 예전 multiaddr 형식 모두 걸 주소가 된다.
        for onion in [format!("{ONION}.onion:4001"), onion_ma.to_string()] {
            let c = ContactInfo {
                peer_id: Vec::new(),
                relay_addrs: vec![relay.clone()],
                onion: Some(onion),
            };
            let addrs = contact_addrs(&c);
            assert_eq!(addrs.len(), 2, "{addrs:?}");
            assert_eq!(addrs[1], onion_ma);
        }
        // 끝의 `/p2p/<id>`는 허용.
        let with_peer = format!("{onion_ma}/p2p/{}", PeerId::random());
        let c = ContactInfo {
            peer_id: Vec::new(),
            relay_addrs: Vec::new(),
            onion: Some(with_peer.clone()),
        };
        assert_eq!(
            contact_addrs(&c),
            vec![with_peer.parse::<Multiaddr>().unwrap()]
        );
        // onion 자리에 평문 주소를 넣거나 onion 뒤에 평문 주소를 붙인 링크는 무시한다.
        let with_tail = format!("/onion3/{ONION}:1/ip4/192.168.1.10/tcp/80");
        let with_dns_tail = format!("/onion3/{ONION}:1/dns4/intranet/tcp/80");
        let two_onions = format!("/onion3/{ONION}:1/onion3/{ONION}:2");
        for bad in [
            "/ip4/127.0.0.1/tcp/1",
            "127.0.0.1:80",
            "example.com:80",
            "x.onion:99999",
            &with_tail,
            &with_dns_tail,
            &two_onions,
        ] {
            let c = ContactInfo {
                peer_id: Vec::new(),
                relay_addrs: Vec::new(),
                onion: Some(bad.into()),
            };
            assert!(contact_addrs(&c).is_empty(), "{bad}");
        }
        assert_eq!(contact_onion(None), None);
        assert_eq!(
            contact_onion(Some(&onion_ma)),
            Some(format!("{ONION}.onion:4001"))
        );
        assert_eq!(
            contact_onion(Some(&"/ip4/1.2.3.4/tcp/1".parse().unwrap())),
            None
        );
    }
}
