//! `net.stun_servers`/`net.set_stun_servers`,
//! `net.file_circuits`/`net.set_file_circuits`, `net.channel_topics`/`net.set_channel_topics`,
//! `net.bridges`/`net.set_bridges`: 네트워크 설정.
//!
//! 저장과 검증은 `stun`·`filelink` 모듈이 맡습니다.

use std::sync::Arc;

use yangpa_rpc::RpcError;
use yangpa_rpc::api::{
    BridgeKind, ChannelTopics, FileCircuits, MAX_BRIDGE_LINE, MAX_CUSTOM_BRIDGES, m,
};

use super::{CallCtx, P, R, empty, rpc};
use crate::node::Node;
use crate::stun;

pub(crate) async fn stun_servers(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    _p: P<m::NetStunServers>,
) -> R<m::NetStunServers> {
    Ok(stun::view(node))
}

pub(crate) async fn set_stun_servers(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::NetSetStunServers>,
) -> R<m::NetSetStunServers> {
    rpc(stun::set(node, p.servers))?;
    Ok(empty())
}

pub(crate) async fn file_circuits(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    _p: P<m::NetFileCircuits>,
) -> R<m::NetFileCircuits> {
    let count = crate::filelink::circuits(node);
    Ok(FileCircuits {
        enabled: count > 0,
        count,
    })
}

pub(crate) async fn set_file_circuits(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::NetSetFileCircuits>,
) -> R<m::NetSetFileCircuits> {
    let n = p.count.unwrap_or(if p.enabled {
        crate::filelink::MAX_CIRCUITS
    } else {
        0
    });
    rpc(crate::filelink::set_circuits(node, n))?;
    Ok(empty())
}

pub(crate) async fn bridges(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    _p: P<m::NetBridges>,
) -> R<m::NetBridges> {
    Ok(crate::bridges::view(node))
}

pub(crate) async fn set_bridges(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::NetSetBridges>,
) -> R<m::NetSetBridges> {
    let custom: Vec<String> = p
        .custom
        .iter()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect();
    if custom.len() > MAX_CUSTOM_BRIDGES {
        return Err(RpcError::invalid_input(
            "브리지 줄은 32줄까지 넣을 수 있습니다",
        ));
    }
    if custom
        .iter()
        .any(|l| l.len() > MAX_BRIDGE_LINE || yangpa_net::check_bridge_line(l).is_err())
    {
        return Err(RpcError::invalid_input("읽을 수 없는 브리지 줄이 있습니다"));
    }
    if p.enabled && p.kind == BridgeKind::Custom && custom.is_empty() {
        return Err(RpcError::invalid_input("브리지 줄을 하나 이상 넣어 주세요"));
    }
    if p.enabled && crate::bridges::lyrebird().is_none() {
        return Err(RpcError::invalid_input(
            "브리지 전송 프로그램(lyrebird)이 없어 브리지를 쓸 수 없습니다",
        ));
    }
    let s = crate::bridges::Stored {
        enabled: p.enabled,
        kind: p.kind,
        custom,
        auto: false,
    };
    rpc(crate::bridges::set(node, s))?;
    Ok(empty())
}

pub(crate) async fn channel_topics(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    _p: P<m::NetChannelTopics>,
) -> R<m::NetChannelTopics> {
    Ok(ChannelTopics {
        enabled: crate::chantopics::enabled(node),
    })
}

pub(crate) async fn set_channel_topics(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::NetSetChannelTopics>,
) -> R<m::NetSetChannelTopics> {
    rpc(crate::chantopics::set_enabled(node, p.enabled))?;
    Ok(empty())
}
