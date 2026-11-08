//! 보관 전용 노드 API(X3): 보관 노드 쪽 `archive.offer`·`archive.hosted`·`archive.drop`, 크루 쪽
//! `archive.designate`·`archive.revoke`·`archive.list`. 동작 설명은 `crate::archive`와 docs/protocol.md를 보세요.

use std::sync::Arc;
use std::time::Duration;

use yangpa_core::archive::ArchiveEntry;
use yangpa_rpc::RpcError;
use yangpa_rpc::api::{ArchiveHostedView, ArchiveOfferResult, ArchiveView, m};

use super::{CallCtx, P, R, empty, rpc};
use crate::archive;
use crate::node::Node;

fn view(e: ArchiveEntry, online: bool) -> ArchiveView {
    ArchiveView {
        device: e.grant.device,
        user: e.grant.user,
        name: e.grant.name,
        private_channels: e.grant.private_channels,
        granted_by: e.granted_by,
        granted_at: e.granted_at,
        online,
    }
}

// ───────────── 보관 노드 쪽 ─────────────

pub(crate) async fn offer(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::ArchiveOffer>,
) -> R<m::ArchiveOffer> {
    if p.expires_in_secs == Some(0) {
        return Err(RpcError::invalid_input("유효 시간은 1초 이상이어야 합니다"));
    }
    let ttl = p.expires_in_secs.map(Duration::from_secs);
    let (url, expires_at) = rpc(archive::offer(node, ttl, p.name).await)?;
    Ok(ArchiveOfferResult { url, expires_at })
}

pub(crate) async fn hosted(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    _p: P<m::ArchiveHosted>,
) -> R<m::ArchiveHosted> {
    Ok(archive::hosted(node)
        .into_iter()
        .map(|h| ArchiveHostedView {
            space: h.space,
            owner: h.owner,
            members: h.members,
            channels: h.channels,
            events: h.events,
            blobs: h.blobs,
            used_bytes: h.used_bytes,
            designated_by: h.designated_by,
            designated_at: h.designated_at,
            roster_issued_at: h.roster_issued_at,
        })
        .collect())
}

pub(crate) async fn drop(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::ArchiveDrop>,
) -> R<m::ArchiveDrop> {
    if !archive::drop_hosted(node, &p.space) {
        return Err(RpcError::not_found("받아 둔 공간이 아닙니다"));
    }
    Ok(empty())
}

// ───────────── 크루 쪽 ─────────────

pub(crate) async fn designate(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::ArchiveDesignate>,
) -> R<m::ArchiveDesignate> {
    let e = rpc(archive::designate(node, p.space, &p.url, p.private_channels, p.name).await)?;
    Ok(view(e, true))
}

pub(crate) async fn revoke(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::ArchiveRevoke>,
) -> R<m::ArchiveRevoke> {
    rpc(archive::revoke(node, p.space, p.device).await)?;
    Ok(empty())
}

pub(crate) async fn list(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::ArchiveList>,
) -> R<m::ArchiveList> {
    let mut out: Vec<ArchiveView> = rpc(archive::list(node, &p.space))?
        .into_iter()
        .map(|(e, online)| view(e, online))
        .collect();
    out.sort_by_key(|v| v.granted_at);
    Ok(out)
}
