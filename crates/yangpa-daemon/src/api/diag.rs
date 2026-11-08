//! `diag.*`: 지표·휘발성 누출 검사.
//!
//! 담당: B2(진단). 실제 일은 `crate::diag`가 합니다.

use std::sync::Arc;

use yangpa_rpc::api::m;

use super::{CallCtx, P, R};
use crate::diag;
use crate::node::Node;

pub(crate) async fn metrics(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    _p: P<m::DiagMetricsGet>,
) -> R<m::DiagMetricsGet> {
    Ok(diag::metrics(node))
}

pub(crate) async fn leakcheck(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::DiagLeakcheck>,
) -> R<m::DiagLeakcheck> {
    diag::leakcheck(node, p.marker).await
}
