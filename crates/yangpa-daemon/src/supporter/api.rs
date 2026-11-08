// SPDX-License-Identifier: LicenseRef-YANGPA-Supporter
//! `supporter.*`: 후원자 확인(GitHub Sponsors·Patreon 정기 후원). 판정과 저장은 `crate::supporter`가 맡습니다.
//! `flair.*`: 후원자 표시(카드 배너·후광, `crate::flair`).

use std::sync::Arc;

use yangpa_rpc::api::m;

use super::{CallCtx, P, R, empty, rpc};
use crate::node::Node;

pub(crate) async fn status(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    _p: P<m::SupporterStatusGet>,
) -> R<m::SupporterStatusGet> {
    Ok(crate::supporter::status(node))
}

pub(crate) async fn github_begin(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    _p: P<m::SupporterGithubBegin>,
) -> R<m::SupporterGithubBegin> {
    crate::supporter::github_begin(node).await
}

pub(crate) async fn patreon_begin(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    _p: P<m::SupporterPatreonBegin>,
) -> R<m::SupporterPatreonBegin> {
    crate::supporter::patreon_begin(node).await
}

pub(crate) async fn refresh(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    _p: P<m::SupporterRefresh>,
) -> R<m::SupporterRefresh> {
    crate::supporter::check_due(node, true).await;
    Ok(crate::supporter::status(node))
}

pub(crate) async fn unlink(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::SupporterUnlink>,
) -> R<m::SupporterUnlink> {
    rpc(crate::supporter::unlink(node, p.provider))?;
    Ok(empty())
}

pub(crate) async fn flair_list(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    _p: P<m::FlairList>,
) -> R<m::FlairList> {
    Ok(crate::flair::list(node))
}

pub(crate) async fn flair_set(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::FlairSet>,
) -> R<m::FlairSet> {
    crate::flair::set(node, p)
}
