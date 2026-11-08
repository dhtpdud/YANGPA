//! `presence.*`: 이 기기의 표시 상태와 마지막 동기화 시각 비공개. 상태 관리는 `crate::presence`가 맡습니다.

use std::sync::Arc;

use yangpa_rpc::api::m;

use super::{CallCtx, P, R};
use crate::node::Node;

pub(crate) async fn get(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    _p: P<m::PresenceGet>,
) -> R<m::PresenceGet> {
    Ok(crate::presence::view(node))
}

pub(crate) async fn set(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::PresenceSet>,
) -> R<m::PresenceSet> {
    crate::presence::set(node, p)
}
