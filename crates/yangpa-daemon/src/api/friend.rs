//! `friend.*`(코어).

use std::sync::Arc;

use yangpa_rpc::RpcError;
use yangpa_rpc::api::{
    ContactKind, FriendAddResult, FriendAddStatus, FriendLinkCreateResult, FriendLinkView,
    FriendProposeResult, FriendProposeStatus, FriendRequestView, FriendView, SafetyNumberView,
    SpaceRef, m,
};
use yangpa_store::RequestDirection;

use super::{CallCtx, P, R, me, rpc};
use crate::error::Error;
use crate::friends::{self, AddOutcome};
use crate::identity;
use crate::node::Node;

fn store_err(e: yangpa_store::StoreError) -> RpcError {
    Error::from(e).into()
}

pub(crate) async fn link_create(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::FriendLinkCreate>,
) -> R<m::FriendLinkCreate> {
    me(node)?;
    let l =
        rpc(friends::create_link(node, p.expires_in_secs, p.single_use.unwrap_or(false)).await)?;
    Ok(FriendLinkCreateResult {
        url: l.url,
        token_id: l.token_id,
        expires_at: l.expires_at,
    })
}

pub(crate) async fn link_list(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    _p: P<m::FriendLinkList>,
) -> R<m::FriendLinkList> {
    Ok(node
        .store
        .list_friend_links()
        .map_err(store_err)?
        .into_iter()
        .filter(|l| !l.revoked)
        .map(|l| FriendLinkView {
            token_id: friends::token_id(&l.token_hash),
            created_at: l.created_at,
            expires_at: l.expires_at,
            single_use: l.single_use,
            uses: l.uses,
        })
        .collect())
}

pub(crate) async fn link_revoke(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::FriendLinkRevoke>,
) -> R<m::FriendLinkRevoke> {
    rpc(friends::revoke_link(node, &p.token_id))?;
    Ok(super::empty())
}

pub(crate) async fn add(node: &Arc<Node>, _ctx: &CallCtx, p: P<m::FriendAdd>) -> R<m::FriendAdd> {
    match rpc(friends::add(node, &p.url).await)? {
        AddOutcome::Pending(user) => Ok(FriendAddResult {
            status: FriendAddStatus::Pending,
            user,
        }),
        AddOutcome::AlreadyFriend(user) => Ok(FriendAddResult {
            status: FriendAddStatus::Connected,
            user,
        }),
    }
}

pub(crate) async fn requests(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    _p: P<m::FriendRequests>,
) -> R<m::FriendRequests> {
    Ok(node
        .store
        .list_friend_requests(RequestDirection::Incoming)
        .map_err(store_err)?
        .into_iter()
        .filter(|r| !friends::rejected(r))
        .map(|r| FriendRequestView {
            request_id: friends::request_id_str(&r.request_id),
            user: r.user,
            profile: r.profile.map(|p| p.profile).unwrap_or_default(),
            received_at: r.received_at,
        })
        .collect())
}

pub(crate) async fn accept(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::FriendAccept>,
) -> R<m::FriendAccept> {
    let space = rpc(friends::accept(node, &p.request_id).await)?;
    Ok(SpaceRef { space })
}

pub(crate) async fn reject(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::FriendReject>,
) -> R<m::FriendReject> {
    rpc(friends::reject(node, &p.request_id))?;
    Ok(super::empty())
}

pub(crate) async fn list(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    _p: P<m::FriendList>,
) -> R<m::FriendList> {
    let shown = crate::presence::shown_all(node);
    let seen = crate::presence::last_seen_all(node);
    Ok(node
        .store
        .list_friends()
        .map_err(store_err)?
        .into_iter()
        .filter_map(|f| {
            let s = shown.get(&f.user).copied();
            Some(FriendView {
                user: f.user,
                profile: identity::profile_of(node, &f.user),
                space: f.space?,
                online: s.is_some(),
                away: s.is_some_and(|s| s.away()),
                last_seen: seen.get(&f.user).copied(),
                safety_number_verified: f.verified,
                kind: match f.kind {
                    yangpa_store::ContactKind::Friend => ContactKind::Friend,
                    yangpa_store::ContactKind::Temporary => ContactKind::Temporary,
                    yangpa_store::ContactKind::Expired => ContactKind::Expired,
                },
                expires_at: f.expires_at,
                proposed: friends::proposed(node, &f.user),
            })
        })
        .collect())
}

/// 임시 연락처에게 친구 추가를 요청합니다(상대도 요청했으면 바로 친구).
pub(crate) async fn propose(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::FriendPropose>,
) -> R<m::FriendPropose> {
    let status = match rpc(friends::propose(node, &p.user).await)? {
        friends::ProposeOutcome::Requested => FriendProposeStatus::Requested,
        friends::ProposeOutcome::Friends => FriendProposeStatus::Friends,
    };
    Ok(FriendProposeResult { status })
}

/// 그 연락처와 대화할 1:1 공간. 1:1을 크루로 바꿨으면 둘만의 1:1 공간을 새로 만듭니다(`friends::open`).
pub(crate) async fn open(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::FriendOpen>,
) -> R<m::FriendOpen> {
    let space = rpc(friends::open(node, &p.user).await)?;
    Ok(SpaceRef { space })
}

/// 친구 관계를 끊고 양쪽의 1:1 공간과 그 기록을 지웁니다(`friends::remove`).
pub(crate) async fn remove(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::FriendRemove>,
) -> R<m::FriendRemove> {
    friends::remove(node, &p.user).await?;
    Ok(super::empty())
}

pub(crate) async fn safety_number(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::FriendSafetyNumber>,
) -> R<m::FriendSafetyNumber> {
    let me = me(node)?;
    let f = node
        .store
        .get_friend(&p.user)
        .map_err(store_err)?
        .ok_or_else(|| RpcError::not_found("친구가 아닙니다"))?;
    Ok(SafetyNumberView {
        digits: yangpa_crypto::safety_number(&me.user(), &p.user),
        verified: f.verified,
    })
}

pub(crate) async fn verify(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::FriendVerify>,
) -> R<m::FriendVerify> {
    if !node
        .store
        .set_friend_verified(&p.user, p.verified)
        .map_err(store_err)?
    {
        return Err(RpcError::not_found("친구가 아닙니다"));
    }
    Ok(super::empty())
}
