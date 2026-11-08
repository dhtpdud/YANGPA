//! `member.*`: 멤버 목록·추방·역할 지정.

use std::collections::BTreeSet;
use std::sync::Arc;

use yangpa_core::body::EventBody;
use yangpa_core::ids::{DeviceId, RoleId, UserId};
use yangpa_core::model::{Permissions, SpaceState};
use yangpa_core::time::Timestamp;
use yangpa_rpc::RpcError;
use yangpa_rpc::api::{MemberFilter, MemberPage, MemberSearchParams, MemberView, m};
use yangpa_sync::state::limits::MAX_REASON_CHARS;

use super::role::ShownRoles;
use super::{CallCtx, P, R, empty, rpc};
use crate::author::{self, KeyChoice};
use crate::node::Node;
use crate::{crews, identity};

pub(crate) async fn list(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::MemberList>,
) -> R<m::MemberList> {
    let (state, me) = rpc(crews::member_state(node, &p.space))?;
    let shown = ShownRoles::of(&state, &me);
    let presence = crate::presence::shown_all(node);
    let seen = crate::presence::last_seen_all(node);
    Ok(state
        .members
        .values()
        .map(|m| {
            let s = presence.get(&m.user).copied();
            MemberView {
                user: m.user,
                profile: identity::profile_of(node, &m.user),
                nickname: m.nickname.clone(),
                roles: shown.filter(&m.roles),
                online: s.is_some(),
                away: s.is_some_and(|s| s.away()),
                automation: m.automation,
                blocked: crate::moderation::is_blocked(node, &m.user),
                last_seen: seen.get(&m.user).copied(),
            }
        })
        .collect())
}

/// `member.search`의 "최근": 이 안에 연결된 적이 있는 멤버.
const RECENT_MS: u64 = 7 * 24 * 60 * 60 * 1000;
const SEARCH_LIMIT: u32 = 100;
const SEARCH_LIMIT_MAX: u32 = 200;

/// 큰 크루의 멤버 목록 쪽. 온라인 → 최근(마지막 연결 최신 순) → 나머지, 같은 묶음은 표시 이름 순. 3만 명이면 프로필을
/// 모두 읽어야 해서 블로킹 스레드에서 돕니다.
pub(crate) async fn search(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::MemberSearch>,
) -> R<m::MemberSearch> {
    let (state, me) = rpc(crews::member_state(node, &p.space))?;
    let node = node.clone();
    tokio::task::spawn_blocking(move || search_blocking(&node, &state, &me, p))
        .await
        .map_err(|e| RpcError::internal(format!("멤버 검색 작업 실패: {e}")))
}

fn search_blocking(
    node: &Node,
    state: &SpaceState,
    me: &UserId,
    p: MemberSearchParams,
) -> MemberPage {
    let shown = ShownRoles::of(state, me);
    let limit = p.limit.unwrap_or(SEARCH_LIMIT).clamp(1, SEARCH_LIMIT_MAX) as usize;
    let offset = p.offset.unwrap_or(0) as usize;
    let query = p
        .query
        .as_deref()
        .map(str::trim)
        .filter(|q| !q.is_empty())
        .map(str::to_lowercase);
    // 숨기 중인 사람은 오프라인으로, 마지막 동기화를 비공개로 한 사람은 "최근"에 넣지 않습니다(`presence`).
    let online = crate::presence::shown_all(node);
    let seen = crate::presence::last_seen_all(node);
    let since = Timestamp::now().0.saturating_sub(RECENT_MS);
    let online_count = state
        .members
        .keys()
        .filter(|u| online.contains_key(u))
        .count();
    // (묶음, 마지막 연결 역순, 이름) 순.
    let mut rows: Vec<(u8, std::cmp::Reverse<u64>, String, MemberView)> = Vec::new();
    for m in state.members.values() {
        let presence = online.get(&m.user).copied();
        let is_online = presence.is_some();
        let last = seen.get(&m.user).copied();
        let recent = is_online || last.is_some_and(|t| t >= since);
        match p.filter {
            MemberFilter::Online if !is_online => continue,
            MemberFilter::Recent if !recent => continue,
            _ => {}
        }
        let profile = identity::profile_of(node, &m.user);
        let name = m
            .nickname
            .clone()
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| profile.name.clone())
            .to_lowercase();
        if let Some(q) = &query
            && !name.contains(q.as_str())
            && !profile.name.to_lowercase().contains(q.as_str())
            && !m.user.to_string().to_lowercase().starts_with(q.as_str())
        {
            continue;
        }
        let group = if is_online {
            0
        } else if recent {
            1
        } else {
            2
        };
        rows.push((
            group,
            std::cmp::Reverse(if group == 1 { last.unwrap_or(0) } else { 0 }),
            name,
            MemberView {
                user: m.user,
                profile,
                nickname: m.nickname.clone(),
                roles: shown.filter(&m.roles),
                online: is_online,
                away: presence.is_some_and(|s| s.away()),
                automation: m.automation,
                blocked: crate::moderation::is_blocked(node, &m.user),
                last_seen: last,
            },
        ));
    }
    rows.sort_by(|a, b| (a.0, a.1, &a.2, a.3.user).cmp(&(b.0, b.1, &b.2, b.3.user)));
    let total = rows.len() as u32;
    MemberPage {
        members: rows
            .into_iter()
            .skip(offset)
            .take(limit)
            .map(|r| r.3)
            .collect(),
        total,
        online: online_count as u32,
    }
}

pub(crate) async fn kick(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::MemberKick>,
) -> R<m::MemberKick> {
    let (state, me) = rpc(crews::crew_state(node, &p.space))?;
    rpc(crews::need(&state, &me, Permissions::KICK_MEMBERS))?;
    if p.user == me {
        return Err(RpcError::invalid_input("자기 자신은 crew.leave로 나갑니다"));
    }
    if !state.is_member(&p.user) {
        return Err(RpcError::not_found("이 크루의 멤버가 아닙니다"));
    }
    if p.user == state.owner {
        return Err(RpcError::forbidden("소유자는 추방할 수 없습니다"));
    }
    if state.rank_of(&me) <= state.rank_of(&p.user) {
        return Err(RpcError::forbidden(
            "자기보다 순위가 높거나 같은 멤버는 추방할 수 없습니다",
        ));
    }
    let reason = p.reason.unwrap_or_default();
    rpc(crews::check_text(&reason, MAX_REASON_CHARS, "추방 사유"))?;
    let devices: Vec<DeviceId> = state
        .devices
        .get(&p.user)
        .map(|l| l.active().map(|d| d.device).collect())
        .unwrap_or_default();
    let body = EventBody::MemberKick {
        user: p.user,
        reason,
    };
    // 기기 제거·상태 키 회전은 커밋 확정 기기가 상태 변화를 보고 합니다(committer).
    let ev = rpc(author::publish(node, p.space, None, &body, KeyChoice::State).await)?;
    crews::tell_removed(node, p.space, devices, ev);
    Ok(empty())
}

pub(crate) async fn set_roles(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::MemberSetRoles>,
) -> R<m::MemberSetRoles> {
    let (state, me) = rpc(crews::crew_state(node, &p.space))?;
    rpc(crews::need(&state, &me, Permissions::MANAGE_ROLES))?;
    let cur = state
        .members
        .get(&p.user)
        .ok_or_else(|| RpcError::not_found("이 크루의 멤버가 아닙니다"))?;
    let mut roles = Vec::new();
    for r in &p.roles {
        if *r == RoleId::EVERYONE || !state.roles.contains_key(r) {
            return Err(RpcError::invalid_input("없는 역할입니다"));
        }
        if !roles.contains(r) {
            roles.push(*r);
        }
    }
    roles.sort_unstable();
    let old: BTreeSet<RoleId> = cur.roles.iter().copied().collect();
    let new: BTreeSet<RoleId> = roles.iter().copied().collect();
    if old == new {
        return Ok(empty());
    }
    let my_rank = state.rank_of(&me);
    let too_high = old
        .symmetric_difference(&new)
        .any(|r| state.roles.get(r).is_some_and(|x| x.level() >= my_rank));
    if too_high {
        return Err(RpcError::forbidden(
            "자기보다 순위가 높거나 같은 역할은 주거나 뺄 수 없습니다",
        ));
    }
    if p.user != me && state.rank_of(&p.user) >= my_rank {
        return Err(RpcError::forbidden(
            "자기보다 순위가 높거나 같은 멤버의 역할은 바꿀 수 없습니다",
        ));
    }
    let body = EventBody::MemberUpdate {
        user: p.user,
        roles,
        nickname: None,
    };
    rpc(author::publish(node, p.space, None, &body, KeyChoice::State).await)?;
    Ok(empty())
}
