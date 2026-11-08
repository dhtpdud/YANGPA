//! `role.*`. 역할을 만들고 바꾸려면 역할 관리 권한과 그 역할보다 높은 순위가 필요하고, 자기에게 없는 권한은 줄 수
//! 없습니다(해소기 규칙과 같음, API에서 먼저 확인해 알아보기 쉬운 오류를 냅니다). 순위는 `Role::level`로 비교하므로
//! 관리자 권한 역할은 `rank`와 상관없이 일반 역할보다 위입니다.

use std::collections::BTreeSet;
use std::sync::Arc;

use yangpa_core::body::EventBody;
use yangpa_core::ids::RoleId;
use yangpa_core::ids::UserId;
use yangpa_core::model::{Permissions, Role, SpaceState};
use yangpa_rpc::RpcError;
use yangpa_rpc::api::{RoleRef, m};
use yangpa_sync::state::limits::MAX_ROLE_RANK;

use super::{CallCtx, P, R, empty, rpc};
use crate::author::{self, KeyChoice};
use crate::crews;
use crate::node::Node;

/// 권한 이름 목록 → 비트.
fn parse_permissions(names: &[String]) -> Result<Permissions, RpcError> {
    let mut p = Permissions::NONE;
    for n in names {
        let (_, bit) = Permissions::NAMES
            .iter()
            .find(|(name, _)| *name == n.trim())
            .ok_or_else(|| RpcError::invalid_input(format!("알 수 없는 권한: {n}")))?;
        p = p.union(*bit);
    }
    Ok(p)
}

fn check_color(color: u32) -> Result<(), RpcError> {
    if color > 0x00FF_FFFF {
        return Err(RpcError::invalid_input("색은 0xRRGGBB여야 합니다"));
    }
    Ok(())
}

/// 자기에게 없는 권한을 주거나 뺄 수 없습니다(관리자·소유자는 전부).
fn may_grant(state: &SpaceState, me: &UserId, bits: Permissions) -> Result<(), RpcError> {
    let base = state.base_permissions(me);
    if base == Permissions::ALL || base.contains(bits) {
        Ok(())
    } else {
        Err(RpcError::forbidden(
            "자기에게 없는 권한은 역할에 주거나 뺄 수 없습니다",
        ))
    }
}

/// 순위는 1 이상이어야 합니다.
fn check_rank(rank: u32) -> Result<(), RpcError> {
    if !(1..=MAX_ROLE_RANK).contains(&rank) {
        return Err(RpcError::invalid_input("역할 순위는 1 이상이어야 합니다"));
    }
    Ok(())
}

/// 만들거나 바꾼 역할이 내 순위보다 낮아야 합니다(관리자 권한을 켜면 관리자 역할 사이의 순위로 봄).
fn below_me(state: &SpaceState, me: &UserId, role: &Role) -> Result<(), RpcError> {
    if role.level() >= state.rank_of(me) {
        return Err(RpcError::forbidden(
            "자기 순위보다 낮은 역할만 만들거나 바꿀 수 있습니다",
        ));
    }
    Ok(())
}

async fn publish(
    node: &Arc<Node>,
    space: yangpa_core::ids::SpaceId,
    body: EventBody,
) -> Result<(), RpcError> {
    rpc(author::publish(node, space, None, &body, KeyChoice::State).await)?;
    Ok(())
}

/// 보는 사람에게 보이는 역할(표시 설정에 따라 화면에서만 거름, `SpaceState::visible_roles`).
pub(crate) struct ShownRoles(Option<BTreeSet<RoleId>>);

impl ShownRoles {
    pub(crate) fn of(state: &SpaceState, viewer: &UserId) -> Self {
        ShownRoles(state.visible_roles(viewer))
    }

    pub(crate) fn shows(&self, role: &RoleId) -> bool {
        self.0.as_ref().is_none_or(|v| v.contains(role))
    }

    pub(crate) fn filter(&self, roles: &[RoleId]) -> Vec<RoleId> {
        roles.iter().filter(|r| self.shows(r)).copied().collect()
    }
}

pub(crate) async fn list(node: &Arc<Node>, _ctx: &CallCtx, p: P<m::RoleList>) -> R<m::RoleList> {
    let (state, me) = rpc(crews::member_state(node, &p.space))?;
    let shown = ShownRoles::of(&state, &me);
    let mut roles: Vec<Role> = state
        .roles
        .values()
        .filter(|r| shown.shows(&r.id))
        .cloned()
        .collect();
    roles.sort_by(|a, b| b.level().cmp(&a.level()).then(a.id.cmp(&b.id)));
    Ok(roles)
}

pub(crate) async fn create(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::RoleCreate>,
) -> R<m::RoleCreate> {
    let (state, me) = rpc(crews::crew_state(node, &p.space))?;
    rpc(crews::need(&state, &me, Permissions::MANAGE_ROLES))?;
    let name = rpc(crews::check_name(&p.name, "역할 이름"))?;
    let permissions = parse_permissions(&p.permissions)?;
    let color = p.color.unwrap_or(0);
    check_color(color)?;
    // 기본 순위는 @everyone 바로 위(가장 낮은 역할).
    let rank = p.rank.unwrap_or(1);
    check_rank(rank)?;
    may_grant(&state, &me, permissions)?;
    let role = Role {
        id: RoleId::random(),
        name,
        color,
        rank,
        permissions,
        automation: false,
    };
    below_me(&state, &me, &role)?;
    let id = role.id;
    publish(node, p.space, EventBody::RoleCreate { role }).await?;
    Ok(RoleRef { role: id })
}

pub(crate) async fn update(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::RoleUpdate>,
) -> R<m::RoleUpdate> {
    let (state, me) = rpc(crews::crew_state(node, &p.space))?;
    rpc(crews::need(&state, &me, Permissions::MANAGE_ROLES))?;
    let old = state
        .roles
        .get(&p.role)
        .ok_or_else(|| RpcError::not_found("없는 역할"))?;
    let mut role = old.clone();
    let everyone = p.role == RoleId::EVERYONE;
    if let Some(n) = &p.name {
        if everyone {
            return Err(RpcError::invalid_input(
                "@everyone의 이름은 바꿀 수 없습니다",
            ));
        }
        role.name = rpc(crews::check_name(n, "역할 이름"))?;
    }
    if let Some(c) = p.color {
        check_color(c)?;
        role.color = c;
    }
    if let Some(perms) = &p.permissions {
        role.permissions = parse_permissions(perms)?;
    }
    if let Some(r) = p.rank {
        if everyone {
            return Err(RpcError::invalid_input(
                "@everyone의 순위는 바꿀 수 없습니다",
            ));
        }
        check_rank(r)?;
        role.rank = r;
    }
    if state.rank_of(&me) <= old.level() {
        return Err(RpcError::forbidden(
            "자기보다 순위가 높거나 같은 역할은 바꿀 수 없습니다",
        ));
    }
    may_grant(
        &state,
        &me,
        Permissions(old.permissions.0 ^ role.permissions.0),
    )?;
    if role != *old {
        below_me(&state, &me, &role)?;
    }
    let old_display = state.role_display(&p.role);
    let mut display = old_display.clone();
    if let Some(v) = &p.visible_to {
        display.visible_to = visible_to(&state, &p.role, v.as_deref())?;
    }
    if let Some(s) = p.see_admins {
        display.see_admins = s;
    }
    if role != *old {
        publish(node, p.space, EventBody::RoleUpdate { role }).await?;
    }
    if display != old_display {
        publish(
            node,
            p.space,
            EventBody::RoleDisplaySet {
                role: p.role,
                display,
            },
        )
        .await?;
    }
    Ok(empty())
}

/// `visible_to` 인자 확인. @everyone 역할에는 쓸 수 없고(모두가 가진 역할), 목록의 @everyone은 '모두'와 같습니다.
fn visible_to(
    state: &SpaceState,
    role: &RoleId,
    list: Option<&[RoleId]>,
) -> Result<Option<Vec<RoleId>>, RpcError> {
    let Some(list) = list else {
        return Ok(None);
    };
    if *role == RoleId::EVERYONE {
        return Err(RpcError::invalid_input(
            "@everyone 역할은 늘 모든 멤버에게 보입니다",
        ));
    }
    if list.contains(&RoleId::EVERYONE) {
        return Ok(None);
    }
    if list.iter().any(|r| !state.roles.contains_key(r)) {
        return Err(RpcError::invalid_input("없는 역할입니다"));
    }
    let mut v = list.to_vec();
    v.sort_unstable();
    v.dedup();
    Ok(Some(v))
}

pub(crate) async fn delete(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::RoleDelete>,
) -> R<m::RoleDelete> {
    let (state, me) = rpc(crews::crew_state(node, &p.space))?;
    rpc(crews::need(&state, &me, Permissions::MANAGE_ROLES))?;
    if p.role == RoleId::EVERYONE {
        return Err(RpcError::invalid_input("@everyone은 지울 수 없습니다"));
    }
    let old = state
        .roles
        .get(&p.role)
        .ok_or_else(|| RpcError::not_found("없는 역할"))?;
    if state.rank_of(&me) <= old.level() {
        return Err(RpcError::forbidden(
            "자기보다 순위가 높거나 같은 역할은 지울 수 없습니다",
        ));
    }
    publish(node, p.space, EventBody::RoleDelete { role: p.role }).await?;
    Ok(empty())
}
