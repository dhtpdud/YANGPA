//! `auth.hello`, `system.*`, `net.status`, `net.peers`, `events.*`(코어).

use std::sync::Arc;
use std::time::Duration;

use yangpa_rpc::RpcError;
use yangpa_rpc::api::{self as rapi, StatusResult, VersionResult, m};

use super::{CallCtx, P, R};
use crate::node::Node;
use crate::tor;

pub(crate) async fn auth_hello(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::AuthHello>,
) -> R<m::AuthHello> {
    super::hello(node, p)
}

pub(crate) async fn version(
    _node: &Arc<Node>,
    _ctx: &CallCtx,
    _p: P<m::SystemVersion>,
) -> R<m::SystemVersion> {
    Ok(VersionResult {
        version: env!("CARGO_PKG_VERSION").into(),
        protocol: yangpa_rpc::PROTOCOL_VERSION,
    })
}

pub(crate) async fn status(
    node: &Arc<Node>,
    ctx: &CallCtx,
    _p: P<m::SystemStatus>,
) -> R<m::SystemStatus> {
    let me = node.me.read().clone();
    let net = net_status_for(node, &node.net.status().await);
    let spaces = node
        .space_ids()
        .iter()
        .filter(|s| ctx.allows_space(s))
        .count() as u32;
    Ok(StatusResult {
        identity: me.as_ref().map(|m| m.user()),
        device: me.as_ref().map(|m| m.device_id()),
        net,
        spaces,
        tor: tor::status(node),
    })
}

pub(crate) async fn shutdown(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    _p: P<m::SystemShutdown>,
) -> R<m::SystemShutdown> {
    // 응답이 먼저 나가도록 조금 뒤에 멈춥니다.
    let n = node.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        n.shutdown().await;
    });
    Ok(super::empty())
}

pub(crate) async fn net_status(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    _p: P<m::NetStatusGet>,
) -> R<m::NetStatusGet> {
    Ok(net_status_for(node, &node.net.status().await))
}

pub(crate) async fn net_peers(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    _p: P<m::NetPeers>,
) -> R<m::NetPeers> {
    Ok(node
        .net
        .peers()
        .await
        .into_iter()
        .map(|p| rapi::PeerView {
            peer_id: p.peer_id.to_string(),
            user: node.peers.identity_of(&p.peer_id).map(|(u, _)| u),
            rtt_ms: p.rtt_ms,
            relayed: p.relayed_only,
        })
        .collect())
}

/// 서버가 직접 처리하는 메서드. 프로세스 안 호출에는 연결이 없어 구독할 수 없습니다.
pub(crate) async fn events_subscribe(
    _node: &Arc<Node>,
    _ctx: &CallCtx,
    _p: P<m::EventsSubscribe>,
) -> R<m::EventsSubscribe> {
    Err(RpcError::invalid_request(
        "events.subscribe는 연결에서만 쓸 수 있습니다(프로세스 안에서는 Node::subscribe)",
    ))
}

pub(crate) async fn events_unsubscribe(
    _node: &Arc<Node>,
    _ctx: &CallCtx,
    _p: P<m::EventsUnsubscribe>,
) -> R<m::EventsUnsubscribe> {
    Err(RpcError::invalid_request(
        "events.unsubscribe는 연결에서만 쓸 수 있습니다",
    ))
}

/// 네트워크 계층 상태 → API DTO.
pub(crate) fn net_status_for(node: &Node, s: &yangpa_net::NetStatus) -> rapi::NetStatus {
    let addrs = |v: &[yangpa_net::Multiaddr]| v.iter().map(|a| a.to_string()).collect::<Vec<_>>();
    rapi::NetStatus {
        peer_id: s.peer_id.to_string(),
        mode: match s.mode {
            yangpa_net::NetMode::Normal => rapi::NetMode::Normal,
            yangpa_net::NetMode::Tor => rapi::NetMode::Tor,
        },
        listen_addrs: addrs(&s.listen_addrs),
        relay_addrs: addrs(&s.relay_addrs),
        nat: match s.nat {
            yangpa_net::NatStatus::Unknown => rapi::NatStatus::Unknown,
            yangpa_net::NatStatus::Public => rapi::NatStatus::Public,
            yangpa_net::NatStatus::Private => rapi::NatStatus::Private,
            yangpa_net::NatStatus::Disabled => rapi::NatStatus::Disabled,
        },
        connections: s.connections,
        bootstrap_progress: s.tor_bootstrap,
        external_addrs: addrs(&s.external_addrs),
        onion_addr: s.onion_addr.as_ref().map(|a| a.to_string()),
        peers: s.peers,
        connecting: node.peers.connecting(),
        offline: crate::presence::offline(node),
    }
}
