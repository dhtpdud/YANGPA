//! `invite.*`: 초대 링크 만들기·목록·폐기·참가.
//!
//! 링크에는 공간 ID, 시작 이벤트(가입자가 고정), 토큰, 만료·사용 횟수, 이 노드에 닿는 방법(PeerId·릴레이 경유 주소,
//! 자기 IP 없음)만 들어갑니다. 크루 이름 힌트는 넣지 않습니다(링크가 새도 어느 크루인지 드러나지 않게).
//! 상태에는 토큰 원문 대신 해시(식별)와 토큰에서 만든 서명 키의 공개 키(가입 증명 확인)만 기록합니다. 가입자는
//! 토큰을 보내지 않고 그 키로 서명한 증명을 보내므로, 상태를 읽는 멤버도 남을 그 초대로 들이지 못합니다.

use std::sync::Arc;
use std::time::Duration;

use yangpa_core::body::{EventBody, InviteInfo};
use yangpa_core::ids::RoleId;
use yangpa_core::links::{InviteLink, invite_verify_key, token_hash};
use yangpa_core::model::Permissions;
use yangpa_core::time::Timestamp;
use yangpa_rpc::RpcError;
use yangpa_rpc::api::serde_util::TokenHash;
use yangpa_rpc::api::{InviteJoinResult, InviteJoinStatus, InviteView, UrlResult, m};

use super::{CallCtx, P, R, empty, rpc};

/// 만료를 주지 않은 초대의 수명(링크가 새도 오래 쓰이지 않게). 더 길게 쓰려면 `expires_in_secs`를 줍니다.
const DEFAULT_INVITE_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);
use crate::author::{self, KeyChoice};
use crate::error::Error;
use crate::node::Node;
use crate::{crews, spaces};

pub(crate) async fn create(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::InviteCreate>,
) -> R<m::InviteCreate> {
    let (state, me) = rpc(crews::crew_state(node, &p.space))?;
    rpc(crews::need(&state, &me, Permissions::CREATE_INVITE))?;
    if p.max_uses == Some(0) {
        return Err(RpcError::invalid_input("max_uses는 1 이상이어야 합니다"));
    }
    if p.expires_in_secs == Some(0) {
        return Err(RpcError::invalid_input(
            "expires_in_secs는 1 이상이어야 합니다",
        ));
    }
    let mut roles: Vec<RoleId> = Vec::new();
    for r in &p.roles {
        if *r == RoleId::EVERYONE || !state.roles.contains_key(r) {
            return Err(RpcError::invalid_input("없는 역할입니다"));
        }
        if !roles.contains(r) {
            roles.push(*r);
        }
    }
    if !roles.is_empty() {
        // 가입 때 줄 역할은 지금 내가 줄 수 있는 것이어야 합니다(해소기 규칙과 같음).
        rpc(crews::need(&state, &me, Permissions::MANAGE_ROLES))?;
        let my_rank = state.rank_of(&me);
        if roles
            .iter()
            .any(|r| state.roles.get(r).is_some_and(|x| x.level() >= my_rank))
        {
            return Err(RpcError::forbidden(
                "자기보다 순위가 높거나 같은 역할은 초대에 넣을 수 없습니다",
            ));
        }
    }
    let ttl = p
        .expires_in_secs
        .map_or(DEFAULT_INVITE_TTL, Duration::from_secs);
    let expires_at = Some(Timestamp::now().saturating_add(ttl));
    let genesis = rpc(spaces::load_genesis(node, &p.space))?
        .ok_or_else(|| RpcError::conflict("이 공간의 시작 이벤트를 아직 모릅니다"))?;
    let contact = crews::my_contact(node).await;
    let mut link = InviteLink::new(p.space, genesis, contact, expires_at, p.max_uses);
    // 지금 바탕 체크포인트를 고정해, 가입자가 받아 준 노드의 체크포인트 사슬을 그 고정에서 확인하게 합니다.
    link.checkpoint = crate::checkpoint::saved_base(node, &p.space).map(|b| b.pin);
    let url = link.to_url().map_err(|e| RpcError::from(Error::from(e)))?;
    let invite = InviteInfo {
        token_hash: token_hash(&link.token),
        verify_key: invite_verify_key(&link.token),
        expires_at,
        max_uses: p.max_uses,
        roles,
    };
    rpc(author::publish(
        node,
        p.space,
        None,
        &EventBody::InviteCreate { invite },
        KeyChoice::State,
    )
    .await)?;
    Ok(UrlResult { url })
}

pub(crate) async fn list(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::InviteList>,
) -> R<m::InviteList> {
    let (state, me) = rpc(crews::crew_state(node, &p.space))?;
    // 초대를 만들 수 없는 멤버에게는 자기가 만든 초대만 보입니다.
    let all = state
        .base_permissions(&me)
        .contains(Permissions::CREATE_INVITE);
    Ok(state
        .invites
        .iter()
        .filter(|i| all || i.created_by == me)
        .map(|i| InviteView {
            token_hash: TokenHash(i.info.token_hash),
            expires_at: i.info.expires_at,
            max_uses: i.info.max_uses,
            uses: i.uses,
            roles: i.info.roles.clone(),
            created_by: i.created_by,
        })
        .collect())
}

pub(crate) async fn revoke(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::InviteRevoke>,
) -> R<m::InviteRevoke> {
    let (state, me) = rpc(crews::crew_state(node, &p.space))?;
    let inv = state
        .invites
        .iter()
        .find(|i| i.info.token_hash == p.token_hash.0)
        .ok_or_else(|| RpcError::not_found("없는 초대"))?;
    if inv.created_by != me {
        rpc(crews::need(&state, &me, Permissions::CREATE_INVITE))?;
    }
    rpc(author::publish(
        node,
        p.space,
        None,
        &EventBody::InviteRevoke {
            token_hash: p.token_hash.0,
        },
        KeyChoice::State,
    )
    .await)?;
    Ok(empty())
}

pub(crate) async fn join(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::InviteJoin>,
) -> R<m::InviteJoin> {
    let (space, joined) = rpc(crews::join(node, &p.url).await)?;
    Ok(InviteJoinResult {
        space,
        status: if joined {
            InviteJoinStatus::Joined
        } else {
            InviteJoinStatus::Pending
        },
    })
}
