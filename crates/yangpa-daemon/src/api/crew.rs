//! `crew.*`: 크루 만들기·설정·1:1→크루 전환·나가기·소유권 이전.

use std::sync::Arc;

use yangpa_core::body::EventBody;
use yangpa_core::model::{Member, Permissions, SpaceKind, SpaceSettings};
use yangpa_rpc::RpcError;
use yangpa_rpc::api::{CommitterCandidate, SpaceRef, m};
use yangpa_sync::state::limits::{MAX_DESCRIPTION_CHARS, MAX_EMOJIS};

use super::{CallCtx, P, R, empty, me, rpc};
use crate::author::{self, KeyChoice};
use crate::crews;
use crate::node::Node;
use crate::spaces::{self, NewSpace};
use crate::{blobs, ingest, sync_service};

/// 상태 이벤트 하나를 내고 전파합니다.
async fn publish(
    node: &Arc<Node>,
    space: yangpa_core::ids::SpaceId,
    body: EventBody,
) -> Result<(), RpcError> {
    rpc(author::publish(node, space, None, &body, KeyChoice::State).await)?;
    Ok(())
}

pub(crate) async fn create(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::CrewCreate>,
) -> R<m::CrewCreate> {
    let me = me(node)?;
    let name = rpc(crews::check_name(&p.name, "크루 이름"))?;
    let description = p.description.unwrap_or_default();
    rpc(crews::check_text(
        &description,
        MAX_DESCRIPTION_CHARS,
        "크루 설명",
    ))?;
    let spec = NewSpace {
        kind: SpaceKind::Crew,
        settings: SpaceSettings {
            name,
            description,
            icon: None,
            icon_key: None,
            allow_external_emoji: false,
            strict_rotation: false,
        },
        everyone: Permissions::DEFAULT_EVERYONE,
        members: vec![Member {
            user: me.user(),
            roles: Vec::new(),
            nickname: None,
            automation: false,
        }],
        devices: vec![me.devices.clone()],
        first_channel: Some(crews::FIRST_CHANNEL.into()),
    };
    let (space, _evs, report) = rpc(spaces::create_space(node, spec).await)?;
    ingest::after_unlock(node, space, &report);
    sync_service::refresh_space_peers(node, &space);
    Ok(SpaceRef { space })
}

pub(crate) async fn update(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::CrewUpdate>,
) -> R<m::CrewUpdate> {
    let (state, me) = rpc(crews::crew_state(node, &p.space))?;
    rpc(crews::need(&state, &me, Permissions::MANAGE_SPACE))?;
    let mut settings = state.settings.clone();
    if let Some(n) = &p.name {
        settings.name = rpc(crews::check_name(n, "크루 이름"))?;
    }
    if let Some(d) = p.description {
        rpc(crews::check_text(&d, MAX_DESCRIPTION_CHARS, "크루 설명"))?;
        settings.description = d;
    }
    if let Some(path) = &p.icon_path {
        // 아이콘은 이 크루 공간에 등록한 블롭이고, 키는 상태 레인(멤버만 엶)의 공간 설정에 싣습니다.
        if path.trim().is_empty() {
            settings.icon = None;
            settings.icon_key = None;
        } else {
            let (blob, key) = blobs::import_space_image(node, &p.space, path).await?;
            settings.icon = Some(blob);
            settings.icon_key = Some(key);
        }
    }
    if let Some(strict) = p.strict_rotation {
        settings.strict_rotation = strict;
    }
    // 해소기 규칙과 같게 먼저 확인해, 한도가 틀리면 설정도 바꾸지 않습니다.
    let limits = p.emoji_limits.filter(|l| *l != state.emoji_limits);
    if limits.is_some_and(|l| {
        l.max_count
            .is_some_and(|n| n == 0 || n as usize > MAX_EMOJIS)
            || l.max_bytes == Some(0)
    }) {
        return Err(RpcError::invalid_input(format!(
            "이모지 수 한도는 1~{MAX_EMOJIS}개, 크기 한도는 1바이트 이상이어야 합니다"
        )));
    }
    if settings != state.settings {
        publish(node, p.space, EventBody::SpaceUpdate { settings }).await?;
    }
    if let Some(limits) = limits {
        publish(node, p.space, EventBody::EmojiLimitsSet { limits }).await?;
    }
    Ok(empty())
}

/// `crew.set_servers`: 크루 STUN 서버 목록 전체를 바꿉니다(`manage_space`).
pub(crate) async fn set_servers(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::CrewSetServers>,
) -> R<m::CrewSetServers> {
    let (state, me) = rpc(crews::crew_state(node, &p.space))?;
    rpc(crews::need(&state, &me, Permissions::MANAGE_SPACE))?;
    let stun = rpc(crate::stun::normalize_crew(&p.stun))?;
    // 예전 크루 TURN 줄이 남아 있으면 빈 목록으로 덮어 씁니다(이벤트 형식은 그대로, 2026-11-07 TURN 삭제).
    if state.crew_servers.stun == stun && state.crew_servers.turn.is_empty() {
        return Ok(empty());
    }
    let turn = Vec::new();
    publish(node, p.space, EventBody::CrewServersSet { stun, turn }).await?;
    Ok(empty())
}

/// `crew.use_servers`: 이 기기가 그 크루의 STUN 서버를 쓸지(이 기기에만 저장).
pub(crate) async fn use_servers(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::CrewUseServers>,
) -> R<m::CrewUseServers> {
    let (state, _me) = rpc(crews::member_state(node, &p.space))?;
    if state.kind != SpaceKind::Crew {
        return Err(RpcError::invalid_input("크루가 아님"));
    }
    rpc(crate::stun::set_crew_use(node, p.space, p.enabled))?;
    Ok(empty())
}

pub(crate) async fn convert_request(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::CrewConvertRequest>,
) -> R<m::CrewConvertRequest> {
    let (state, me) = rpc(crews::member_state(node, &p.space))?;
    if state.kind != SpaceKind::Space {
        return Err(RpcError::invalid_input(
            "1대1 공간만 크루로 바꿀 수 있습니다",
        ));
    }
    let name = rpc(crews::check_name(&p.name, "크루 이름"))?;
    if state.pending_crew_conversion.is_some_and(|u| u != me) {
        return Err(RpcError::conflict(
            "상대가 이미 크루 전환을 요청했습니다(crew.convert_accept로 수락)",
        ));
    }
    publish(node, p.space, EventBody::CrewConvertRequest { name }).await?;
    Ok(empty())
}

pub(crate) async fn convert_accept(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::CrewConvertAccept>,
) -> R<m::CrewConvertAccept> {
    let (state, me) = rpc(crews::member_state(node, &p.space))?;
    if state.kind != SpaceKind::Space {
        return Err(RpcError::invalid_input("1대1 공간이 아닙니다"));
    }
    match state.pending_crew_conversion {
        None => Err(RpcError::conflict("크루 전환 요청이 없습니다")),
        // 양쪽 동의: 요청한 사람이 스스로 수락할 수 없습니다.
        Some(u) if u == me => Err(RpcError::conflict(
            "내가 보낸 전환 요청은 상대가 수락해야 합니다",
        )),
        Some(_) => {
            publish(node, p.space, EventBody::CrewConvertAccept).await?;
            Ok(empty())
        }
    }
}

pub(crate) async fn leave(node: &Arc<Node>, _ctx: &CallCtx, p: P<m::CrewLeave>) -> R<m::CrewLeave> {
    me(node)?;
    let Some(state) = node.space_state(&p.space) else {
        // 상태를 풀기 전인 가입 대기: 가입을 그만두고 받은 것을 지웁니다.
        if crews::is_pending_join(node, &p.space) {
            rpc(crews::purge_local(node, &p.space).await)?;
            return Ok(empty());
        }
        return Err(RpcError::not_found("모르는 공간"));
    };
    if state.kind != SpaceKind::Crew {
        return Err(RpcError::invalid_input(
            "크루만 나갈 수 있습니다(1대1 공간은 friend.remove)",
        ));
    }
    let friend = node.store.friend_by_space(&p.space).ok().flatten();
    rpc(crews::leave(node, p.space, &state).await)?;
    reopen_friend(node, friend).await;
    Ok(empty())
}

/// 크루 해산(소유자만).
pub(crate) async fn disband(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::CrewDisband>,
) -> R<m::CrewDisband> {
    let (state, me) = rpc(crews::crew_state(node, &p.space))?;
    if state.owner != me {
        return Err(RpcError::forbidden("소유자만 크루를 해산할 수 있습니다"));
    }
    let friend = node.store.friend_by_space(&p.space).ok().flatten();
    rpc(crews::disband(node, p.space, &state).await)?;
    reopen_friend(node, friend).await;
    Ok(empty())
}

/// 1:1에서 바꾼 크루면 친구 기록이 이 크루를 가리킵니다. 나가거나 해산해도 그 사람과는 둘만의 1:1 공간으로 이어 갑니다.
async fn reopen_friend(node: &Arc<Node>, friend: Option<yangpa_store::FriendRecord>) {
    if let Some(f) = friend
        && let Err(e) = crate::friends::open(node, &f.user).await
    {
        tracing::debug!(error = %e, "크루를 나간 뒤 1:1 공간 만들기 실패");
    }
}

/// 확정 기기 지정(크루와 1:1 공간 모두, 소유자 기기만). 옛 확정 기기가 키 그룹 표를 새 확정 기기에게 봉인해 넘기고
/// (`KeyTableHandover`), 넘겨받지 못하면 새 확정 기기가 공개 끝으로 이어받아 전체 재키합니다(`keygroup`).
pub(crate) async fn set_committer(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::CrewSetCommitter>,
) -> R<m::CrewSetCommitter> {
    rpc(crate::keygroup::set_committer(node, p.space, p.device).await)?;
    Ok(empty())
}

/// 확정 기기가 될 수 있는 기기(`set_committer`와 같은 조건: 소유자·관리자의 활성 기기)와 지금 확정 기기.
/// 지금 확정 기기는 조건 밖이어도(관리자 권한을 잃음) 보여 줍니다. 멤버면 누구나 볼 수 있지만, 관리자가
/// 누구인지 볼 수 없는 멤버(역할 표시 설정)에게는 빈 목록입니다(후보가 곧 관리자 목록이라).
pub(crate) async fn committer_candidates(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::CrewCommitterCandidates>,
) -> R<m::CrewCommitterCandidates> {
    let me = me(node)?;
    let state = node
        .space_state(&p.space)
        .ok_or_else(|| RpcError::not_found("모르는 공간"))?;
    if !state.is_member(&me.user()) {
        return Err(RpcError::forbidden("이 공간의 멤버가 아닙니다"));
    }
    if !state.sees_admins(&me.user()) {
        return Ok(Vec::new());
    }
    let online: std::collections::HashSet<_> = node
        .peers
        .authed_peers()
        .into_iter()
        .map(|(_, _, d)| d)
        .collect();
    let mut out: Vec<CommitterCandidate> = state
        .active_devices()
        .filter(|(u, d)| {
            d.device == state.committer
                || **u == state.owner
                || state
                    .base_permissions(u)
                    .contains(Permissions::ADMINISTRATOR)
        })
        .map(|(user, d)| {
            let (user, device) = (*user, d.device);
            // 기기 이름은 공간 상태에 두지 않습니다: 그 사용자의 서명된 기기 목록에서, 키 약속이 같은 항목의 이름.
            let name = crate::identity::device_name(node, &user, d);
            let mine = device == me.device_id();
            CommitterCandidate {
                user,
                device,
                name,
                online: mine || online.contains(&device),
                current: device == state.committer,
                mine,
            }
        })
        .collect();
    // 지금 확정 기기 → 소유자 기기 → 나머지, 같은 묶음 안에서는 이름순.
    out.sort_by(|a, b| {
        (!a.current, a.user != state.owner, &a.name, a.device).cmp(&(
            !b.current,
            b.user != state.owner,
            &b.name,
            b.device,
        ))
    });
    Ok(out)
}

pub(crate) async fn transfer_ownership(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::CrewTransferOwnership>,
) -> R<m::CrewTransferOwnership> {
    let (state, me) = rpc(crews::crew_state(node, &p.space))?;
    if state.owner != me {
        return Err(RpcError::forbidden("소유자만 소유권을 넘길 수 있습니다"));
    }
    if p.user == me {
        return Err(RpcError::invalid_input("이미 소유자입니다"));
    }
    if !state.is_member(&p.user) {
        return Err(RpcError::not_found("이 크루의 멤버가 아닙니다"));
    }
    // 확정 기기가 옮겨 갈 때 비공개 채널 그룹을 넘기는 일까지 크루 영역이 합니다.
    rpc(crews::transfer_ownership(node, p.space, p.user).await)?;
    Ok(empty())
}
