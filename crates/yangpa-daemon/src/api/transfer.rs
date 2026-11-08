//! `transfer.rates`, `rate.*`: 전송 속도와 크루·채널·사람별 속도 제한(이 기기 설정).

use std::sync::Arc;

use yangpa_rpc::api::m;

use super::{CallCtx, P, R};
use crate::node::Node;
use crate::ratelimit::{self, Limit};

pub(crate) async fn rates(
    node: &Arc<Node>,
    ctx: &CallCtx,
    _p: P<m::TransferRatesGet>,
) -> R<m::TransferRatesGet> {
    let mut r = crate::transfer::current(node);
    // 누가 무엇을 받는지는 사람에게만(알림과 같음).
    if ctx.is_automation() {
        r.uploads.clear();
    }
    Ok(r)
}

pub(crate) async fn limits(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    _p: P<m::RateLimits>,
) -> R<m::RateLimits> {
    Ok(ratelimit::list(node))
}

pub(crate) async fn set_limit(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::RateSetLimit>,
) -> R<m::RateSetLimit> {
    ratelimit::set(
        node,
        p.scope,
        Limit {
            up: p.up_bps,
            down: p.down_bps,
        },
    )?;
    Ok(super::empty())
}
