// SPDX-License-Identifier: LicenseRef-YANGPA-Supporter
//! `supporter.*`: 후원자 확인(GitHub Sponsors·Patreon 정기 후원). 판정과 저장은 `crate::supporter`가 맡습니다.
//! `flair.*`: 후원자 표시(카드 배너·후광, `crate::flair`).
//! `subrole.*`·`crew.set_subscriber_roles`: 크루 구독자 역할(`crate::supporter::subrole`).

use std::sync::Arc;

use yangpa_rpc::api::m;

use super::{CallCtx, P, R, empty, rpc};
use crate::node::Node;
use crate::supporter::subrole;

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

// ───────────── 크루 구독자 역할(`crate::supporter::subrole`) ─────────────

pub(crate) async fn creator_status(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    _p: P<m::SubroleCreatorStatus>,
) -> R<m::SubroleCreatorStatus> {
    Ok(subrole::creator_status(node))
}

pub(crate) async fn creator_github_begin(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    _p: P<m::SubroleCreatorGithubBegin>,
) -> R<m::SubroleCreatorGithubBegin> {
    subrole::creator_github_begin(node).await
}

pub(crate) async fn creator_patreon_begin(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    _p: P<m::SubroleCreatorPatreonBegin>,
) -> R<m::SubroleCreatorPatreonBegin> {
    subrole::creator_patreon_begin(node).await
}

pub(crate) async fn creator_sellapp(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::SubroleCreatorSellapp>,
) -> R<m::SubroleCreatorSellapp> {
    subrole::creator_sellapp(node, &p.store, p.key.expose()).await?;
    Ok(empty())
}

pub(crate) async fn creator_unlink(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::SubroleCreatorUnlink>,
) -> R<m::SubroleCreatorUnlink> {
    rpc(subrole::creator_unlink(node, p.provider))?;
    Ok(empty())
}

pub(crate) async fn member_github_begin(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::SubroleGithubBegin>,
) -> R<m::SubroleGithubBegin> {
    subrole::member_github(node, p.space).await
}

pub(crate) async fn member_patreon_begin(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::SubrolePatreonBegin>,
) -> R<m::SubrolePatreonBegin> {
    subrole::member_patreon(node, p.space, p.login).await
}

pub(crate) async fn member_sellapp(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::SubroleSellapp>,
) -> R<m::SubroleSellapp> {
    subrole::member_sellapp(node, p.space, p.key.expose()).await?;
    Ok(empty())
}

pub(crate) async fn set_subscriber_roles(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::CrewSetSubscriberRoles>,
) -> R<m::CrewSetSubscriberRoles> {
    subrole::set_config(node, p.space, &p.roles).await?;
    Ok(empty())
}

pub(crate) async fn sync_subscriber_roles(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::CrewSyncSubscriberRoles>,
) -> R<m::CrewSyncSubscriberRoles> {
    subrole::sync_now(node, p.space).await?;
    Ok(empty())
}
