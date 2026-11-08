//! `confirm.*`: 자동화 토큰의 파괴적 작업 사람 확인. 승인하면 `api::dispatch`로 원래 호출을 실행합니다.
//!
//! 세 메서드 모두 사람 토큰만 씁니다(`approve`·`deny`는 메서드 표의 사람 전용, `list`는 여기서 거절).

use std::sync::Arc;

use yangpa_rpc::RpcError;
use yangpa_rpc::api::{ConfirmApproveResult, m};

use super::{CallCtx, P, R};
use crate::admin;
use crate::node::Node;

pub(crate) async fn list(
    node: &Arc<Node>,
    ctx: &CallCtx,
    _p: P<m::ConfirmList>,
) -> R<m::ConfirmList> {
    if ctx.is_automation() {
        return Err(RpcError::out_of_scope(
            "사람 토큰만 확인 요청을 볼 수 있습니다",
        ));
    }
    Ok(admin::pending(node)?
        .iter()
        .filter(|p| admin::visible_to(ctx, p))
        .map(|p| admin::view(node, p))
        .collect())
}

pub(crate) async fn approve(
    node: &Arc<Node>,
    ctx: &CallCtx,
    p: P<m::ConfirmApprove>,
) -> R<m::ConfirmApprove> {
    let result = admin::approve(node, ctx, &p.id).await?;
    Ok(ConfirmApproveResult { result })
}

pub(crate) async fn deny(
    node: &Arc<Node>,
    ctx: &CallCtx,
    p: P<m::ConfirmDeny>,
) -> R<m::ConfirmDeny> {
    admin::deny(node, ctx, &p.id)?;
    Ok(super::empty())
}
