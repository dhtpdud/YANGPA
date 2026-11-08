//! `space.list`, `space.get`, `channel.list`, `channel.mark_read`(코어 보기).

use std::collections::HashMap;
use std::sync::Arc;

use yangpa_core::ids::{ChannelId, SpaceId};
use yangpa_core::model::{Permissions, SpaceState};
use yangpa_core::time::Timestamp;
use yangpa_rpc::RpcError;
use yangpa_rpc::api::{ChannelView, MemberView, SpaceSummary, SpaceView, m};

use super::role::ShownRoles;
use super::{CallCtx, P, R, me};
use crate::error::Error;
use crate::node::Node;
use crate::{identity, keygroup, messages, p2p, spaces, voice, volatile};

fn dormant(node: &Node, space: &SpaceId) -> bool {
    node.spaces.read().get(space).is_some_and(|r| r.dormant)
}

fn unread_map(node: &Node, space: &SpaceId) -> Result<HashMap<ChannelId, (u32, u32)>, RpcError> {
    let me = me(node)?;
    Ok(node
        .store
        .unread_counts_excluding(space, &me.user(), &crate::moderation::blocked_users(node))
        .map_err(Error::from)?
        .into_iter()
        .map(|u| (u.channel, (u.unread, u.mentions)))
        .collect())
}

fn channel_views(
    node: &Node,
    space: &SpaceId,
    state: &SpaceState,
) -> Result<Vec<ChannelView>, RpcError> {
    let me = me(node)?;
    let unread = unread_map(node, space)?;
    Ok(state
        .channels
        .iter()
        .filter(|(c, _)| {
            state
                .channel_permissions(&me.user(), c)
                .contains(Permissions::VIEW_CHANNEL)
        })
        .map(|(c, s)| {
            let mut view = ChannelView {
                channel: *c,
                settings: s.clone(),
                unread: unread.get(c).map(|u| u.0).unwrap_or(0),
                joined: None,
                participants: None,
                api_read_allowed: false,
                p2p: false,
                p2p_joined: false,
                anonymous: state.is_anonymous(c),
                featured_post: super::message::featured_post(node, space, c, state),
                attach_limit: state.attach_limits.get(c).copied(),
                no_edit: state.edit_rules(c).no_edit,
                no_delete: state.edit_rules(c).no_delete,
                volatile_cap: None,
                volatile_full: false,
                has_new: false,
            };
            // 입장 여부·참가자·API 읽기 허용은 휘발성·음성 영역이, P2P 전용 여부·참여는 `p2p`가 채웁니다.
            volatile::fill_channel_view(node, space, state, &mut view);
            voice::fill_channel_view(node, space, state, &mut view);
            p2p::fill_channel_view(node, space, state, &mut view);
            view.has_new = crate::chantopics::has_new(node, space, c);
            view
        })
        .collect())
}

fn state_of(node: &Node, space: &SpaceId) -> Result<Arc<SpaceState>, RpcError> {
    node.space_state(space)
        .ok_or_else(|| RpcError::not_found("모르는 공간"))
}

pub(crate) async fn list(node: &Arc<Node>, ctx: &CallCtx, _p: P<m::SpaceList>) -> R<m::SpaceList> {
    me(node)?;
    let mut ids = node.space_ids();
    ids.sort();
    let mut out = Vec::new();
    for space in ids {
        if !ctx.allows_space(&space) {
            continue;
        }
        let Some(state) = node.space_state(&space) else {
            continue;
        };
        let unread = unread_map(node, &space)?;
        out.push(SpaceSummary {
            space,
            kind: state.kind,
            name: spaces::display_name(node, &space, &state),
            icon: state.settings.icon,
            unread: unread.values().map(|u| u.0).sum(),
            mention: unread.values().map(|u| u.1).sum(),
            dormant: dormant(node, &space),
            member_count: state.members.len() as u32,
        });
    }
    Ok(out)
}

pub(crate) async fn get(node: &Arc<Node>, _ctx: &CallCtx, p: P<m::SpaceGet>) -> R<m::SpaceGet> {
    let me = me(node)?;
    let state = state_of(node, &p.space)?;
    let viewer = me.user();
    // 역할 표시 설정은 화면에서만 거릅니다(멤버 기기는 작성자 권한 확인에 전체 상태가 필요).
    let shown = ShownRoles::of(&state, &viewer);
    let manages_roles = state
        .base_permissions(&viewer)
        .contains(Permissions::MANAGE_ROLES);
    let presence = crate::presence::shown_all(node);
    let seen = crate::presence::last_seen_all(node);
    let members = state
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
        .collect();
    let emojis = super::emoji::views(&state);
    let mut settings = state.settings.clone();
    settings.name = spaces::display_name(node, &p.space, &state);
    let mut view = SpaceView {
        space: p.space,
        kind: state.kind,
        settings,
        owner: state.owner,
        committer: state.committer,
        members,
        roles: state
            .roles
            .values()
            .filter(|r| shown.shows(&r.id))
            .cloned()
            .collect(),
        channels: channel_views(node, &p.space, &state)?,
        emojis,
        emoji_limits: state.emoji_limits,
        my_permissions: state
            .base_permissions(&me.user())
            .names()
            .into_iter()
            .map(str::to_owned)
            .collect(),
        pending_commit: keygroup::pending_commit(node, &p.space, &state),
        key_rotation_pending: keygroup::key_rotation_pending(node, &p.space),
        pending_crew_conversion: state.pending_crew_conversion,
        dormant: dormant(node, &p.space),
        trusted: crate::trust::space_switch(node, &p.space),
        role_display: if manages_roles {
            state
                .role_display
                .iter()
                .map(|(r, d)| (*r, d.clone()))
                .collect()
        } else {
            Default::default()
        },
        admins_hidden: !state.sees_admins(&viewer),
        crew_servers: None,
        crew_stun: false,
        use_crew_servers: false,
        disbanded: state.disbanded,
    };
    crate::stun::fill_space_view(node, &p.space, &state, &me.user(), &mut view);
    Ok(view)
}

pub(crate) async fn channel_list(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::ChannelList>,
) -> R<m::ChannelList> {
    let state = state_of(node, &p.space)?;
    channel_views(node, &p.space, &state)
}

pub(crate) async fn mark_read(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::ChannelMarkRead>,
) -> R<m::ChannelMarkRead> {
    let state = state_of(node, &p.space)?;
    let ch = state
        .channels
        .get(&p.channel)
        .ok_or_else(|| RpcError::not_found("없는 채널"))?;
    // 휘발성 채널의 읽음 위치는 디스크에 두지 않습니다(RAM).
    if ch.is_volatile() {
        volatile::mark_read(node, &p.space, &p.channel, p.event.as_ref())?;
        return Ok(super::empty());
    }
    let pos = match &p.event {
        Some(event) => Some(
            messages::position_in_channel(node, event, &p.space, &p.channel)?
                .ok_or_else(|| RpcError::not_found("이 채널에 없는 메시지"))?,
        ),
        // 포럼처럼 글·답글이 여러 갈래인 채널은 채널을 열면 전부 읽은 것으로 봅니다.
        None => node
            .store
            .latest_in_channel(&p.space, &p.channel)
            .map_err(Error::from)?,
    };
    if let Some((lamport, event)) = pos {
        node.store
            .set_read_marker(&p.space, &p.channel, lamport, &event)
            .map_err(Error::from)?;
    }
    node.store
        .touch_space_read(&p.space, Timestamp::now())
        .map_err(Error::from)?;
    crate::chantopics::follow(node, &p.space, &p.channel);
    Ok(super::empty())
}
