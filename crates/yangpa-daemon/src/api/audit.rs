//! `audit.list`: 감사 로그(메서드·토큰 이름·결과·시각만, 최신순). 쓰기는 `admin::after_call`.

use std::sync::Arc;

use yangpa_rpc::RpcError;
use yangpa_rpc::api::m;

use super::{CallCtx, P, R};
use crate::admin;
use crate::error::Error;
use crate::node::Node;

/// 한 번에 돌려주는 기본 줄 수.
const DEFAULT_LIMIT: u32 = 100;

pub(crate) async fn list(node: &Arc<Node>, _ctx: &CallCtx, p: P<m::AuditList>) -> R<m::AuditList> {
    let limit = p.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, 1000);
    let before = p.before.map(|b| i64::try_from(b).unwrap_or(i64::MAX));
    let rows = node
        .store
        .list_audit(limit, before)
        .map_err(|e| RpcError::from(Error::from(e)))?;
    Ok(rows.into_iter().map(admin::audit_view).collect())
}
