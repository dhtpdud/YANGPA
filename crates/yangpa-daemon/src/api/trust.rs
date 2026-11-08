//! `trust.*`: 신뢰 스위치(이 기기의 IP를 보여도 되는 사람·크루). 실제 일은 `crate::trust`, 연결은 `crate::p2p`.

use std::sync::Arc;

use yangpa_rpc::api::{Notification, TrustView, m};

use super::{CallCtx, P, R, empty, me, rpc};
use crate::node::Node;
use crate::{p2p, trust};

pub(crate) async fn get(node: &Arc<Node>, _ctx: &CallCtx, p: P<m::TrustGet>) -> R<m::TrustGet> {
    me(node)?;
    Ok(TrustView {
        user: p.user,
        trusted: trust::user_switch(node, &p.user),
        via_crews: trust::via_crews(node, &p.user),
        connection: p2p::connection_mode(node, &p.user),
    })
}

/// 사람 토큰만(메서드 표가 검사).
pub(crate) async fn set_user(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::TrustSetUser>,
) -> R<m::TrustSetUser> {
    rpc(trust::set_user(node, p.user, p.enabled))?;
    Ok(empty())
}

/// 사람 토큰만(메서드 표가 검사).
pub(crate) async fn set_space(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::TrustSetSpace>,
) -> R<m::TrustSetSpace> {
    if rpc(trust::set_space(node, p.space, p.enabled))? {
        node.notify
            .publish(Notification::SpaceUpdated { space: p.space });
    }
    Ok(empty())
}
