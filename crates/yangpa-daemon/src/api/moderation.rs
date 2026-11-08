//! 모더레이션 API(W2): `member.ban`·`member.unban`·`member.bans`, `user.block`·`user.unblock`·`user.blocked`,
//! `message.report`, `report.list`·`report.resolve`. 동작 설명은 `crate::moderation`을 보세요.

use std::sync::Arc;

use yangpa_core::body::EventBody;
use yangpa_core::ids::{ChannelId, DeviceId, EventId, SpaceId, UserId};
use yangpa_core::model::{Permissions, SpaceKind, SpaceState};
use yangpa_core::time::Timestamp;
use yangpa_rpc::RpcError;
use yangpa_rpc::api::{
    BanView, BlockedDeviceView, BlockedView, MemberBanParams, MemberKickParams, MessageRefParams,
    ReportAction, ReportRef, UserDeviceView, m,
};
use yangpa_sync::state::limits::MAX_REASON_CHARS;

use super::{CallCtx, P, R, empty, me, rpc};
use crate::author::{self, KeyChoice};
use crate::error::Error;
use crate::moderation::{self, ReportTarget};
use crate::node::Node;
use crate::{admin, crews, volatile};

// ───────────── 밴 ─────────────

pub(crate) async fn ban(node: &Arc<Node>, _ctx: &CallCtx, p: P<m::MemberBan>) -> R<m::MemberBan> {
    ban_inner(node, p).await?;
    Ok(empty())
}

/// 밴 이벤트를 냅니다(신고 처리에서도 씀). 멤버면 추방과 같이 그 기기들에도 알립니다.
async fn ban_inner(node: &Arc<Node>, p: MemberBanParams) -> Result<(), RpcError> {
    let (state, me) = rpc(crews::crew_state(node, &p.space))?;
    rpc(crews::need(&state, &me, Permissions::BAN_MEMBERS))?;
    if p.user == me {
        return Err(RpcError::invalid_input(
            "자기 자신은 밴할 수 없습니다(crew.leave로 나갑니다)",
        ));
    }
    if p.user == state.owner {
        return Err(RpcError::forbidden("소유자는 밴할 수 없습니다"));
    }
    if state.bans.contains_key(&p.user) {
        return Err(RpcError::conflict("이미 밴된 사용자입니다"));
    }
    if state.rank_of(&me) <= state.rank_of(&p.user) {
        return Err(RpcError::forbidden(
            "자기보다 순위가 높거나 같은 멤버는 밴할 수 없습니다",
        ));
    }
    let reason = p.reason.unwrap_or_default();
    rpc(crews::check_text(&reason, MAX_REASON_CHARS, "밴 사유"))?;
    let was_member = state.is_member(&p.user);
    let devices: Vec<DeviceId> = state
        .devices
        .get(&p.user)
        .map(|l| l.active().map(|d| d.device).collect())
        .unwrap_or_default();
    let window = u64::try_from(moderation::DELETE_RECENT_WINDOW.as_millis()).unwrap_or(u64::MAX);
    let delete_since = p
        .delete_recent
        .then(|| Timestamp(Timestamp::now().0.saturating_sub(window)));
    let body = EventBody::MemberBan {
        user: p.user,
        reason,
        delete_since,
    };
    // 기기 제거·상태 키 회전은 커밋 확정 기기가 상태 변화를 보고 합니다(추방과 같음).
    let ev = rpc(author::publish(node, p.space, None, &body, KeyChoice::State).await)?;
    if was_member {
        crews::tell_removed(node, p.space, devices, ev);
    }
    Ok(())
}

pub(crate) async fn unban(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::MemberUnban>,
) -> R<m::MemberUnban> {
    let (state, me) = rpc(crews::crew_state(node, &p.space))?;
    rpc(crews::need(&state, &me, Permissions::BAN_MEMBERS))?;
    if !state.bans.contains_key(&p.user) {
        return Err(RpcError::not_found("밴 목록에 없는 사용자입니다"));
    }
    let body = EventBody::MemberUnban { user: p.user };
    rpc(author::publish(node, p.space, None, &body, KeyChoice::State).await)?;
    Ok(empty())
}

pub(crate) async fn bans(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::MemberBans>,
) -> R<m::MemberBans> {
    let (state, me) = rpc(crews::crew_state(node, &p.space))?;
    rpc(crews::need(&state, &me, Permissions::BAN_MEMBERS))?;
    let mut out: Vec<BanView> = state
        .bans
        .iter()
        .map(|(user, b)| BanView {
            user: *user,
            profile: known_profile(node, user),
            reason: (!b.reason.is_empty()).then(|| b.reason.clone()),
            banned_by: b.banned_by,
            banned_at: b.banned_at,
        })
        .collect();
    out.sort_by(|a, b| b.banned_at.cmp(&a.banned_at).then(a.user.cmp(&b.user)));
    Ok(out)
}

fn known_profile(node: &Node, user: &UserId) -> Option<yangpa_core::device::Profile> {
    node.store
        .get_profile(user)
        .ok()
        .flatten()
        .map(|p| p.profile)
}

// ───────────── 차단 ─────────────

pub(crate) async fn block(node: &Arc<Node>, _ctx: &CallCtx, p: P<m::UserBlock>) -> R<m::UserBlock> {
    rpc(moderation::block(node, &p.user))?;
    Ok(empty())
}

pub(crate) async fn unblock(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::UserUnblock>,
) -> R<m::UserUnblock> {
    rpc(moderation::unblock(node, &p.user))?;
    Ok(empty())
}

pub(crate) async fn blocked(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    _p: P<m::UserBlocked>,
) -> R<m::UserBlocked> {
    Ok(moderation::blocked_list(node)
        .into_iter()
        .map(|(user, blocked_at)| BlockedView {
            user,
            profile: known_profile(node, &user),
            blocked_at,
        })
        .collect())
}

/// 그 사용자의 서명 기기 목록에서 기기 이름(모르면 빈 문자열).
fn device_name_of(node: &Node, user: &UserId, device: &DeviceId) -> String {
    node.store
        .get_device_list(user)
        .ok()
        .flatten()
        .and_then(|l| l.list.devices.into_iter().find(|d| d.device == *device))
        .map(|d| d.name)
        .unwrap_or_default()
}

pub(crate) async fn devices(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::UserDevices>,
) -> R<m::UserDevices> {
    me(node)?;
    let list = node.store.get_device_list(&p.user).map_err(Error::from)?;
    Ok(list
        .map(|l| l.list.devices)
        .unwrap_or_default()
        .into_iter()
        // 폐기 기록은 인증을 막으려 저장소에 남지만 보여 줄 것은 아닙니다.
        .filter(|d| d.revoked_at.is_none())
        .map(|d| UserDeviceView {
            blocked: moderation::is_device_blocked(node, &d.device),
            device: d.device,
            name: d.name,
            added_at: d.added_at,
        })
        .collect())
}

pub(crate) async fn block_device(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::UserBlockDevice>,
) -> R<m::UserBlockDevice> {
    rpc(moderation::block_device(node, &p.user, &p.device))?;
    Ok(empty())
}

pub(crate) async fn unblock_device(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::UserUnblockDevice>,
) -> R<m::UserUnblockDevice> {
    rpc(moderation::unblock_device(node, &p.device))?;
    Ok(empty())
}

pub(crate) async fn blocked_devices(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    _p: P<m::UserBlockedDevices>,
) -> R<m::UserBlockedDevices> {
    Ok(moderation::blocked_device_list(node)
        .into_iter()
        .map(|(device, user, blocked_at)| BlockedDeviceView {
            name: device_name_of(node, &user, &device),
            profile: known_profile(node, &user),
            user,
            device,
            blocked_at,
        })
        .collect())
}

// ───────────── 신고 ─────────────

/// 이 기기가 아는 메시지의 작성자. 휘발성 채널은 지금 RAM에 있는 것만(호출한 토큰의 휘발성 읽기 규칙을 따름).
fn message_author(
    node: &Arc<Node>,
    ctx: &CallCtx,
    state: &SpaceState,
    space: &SpaceId,
    channel: &ChannelId,
    id: &EventId,
) -> Result<Option<UserId>, RpcError> {
    let volatile = state.channels.get(channel).is_some_and(|c| c.is_volatile());
    if volatile {
        return volatile::author_for_api(node, ctx, space, channel, id);
    }
    Ok(node
        .store
        .get_message(id, None)
        .map_err(Error::from)?
        .filter(|r| r.space == *space && r.channel == *channel)
        .map(|r| r.author))
}

pub(crate) async fn report(
    node: &Arc<Node>,
    ctx: &CallCtx,
    p: P<m::MessageReport>,
) -> R<m::MessageReport> {
    let me = me(node)?;
    let (state, _) = rpc(crews::member_state(node, &p.space))?;
    if state.kind != SpaceKind::Crew {
        return Err(RpcError::invalid_input(
            "신고는 크루에서만 할 수 있습니다(1:1 대화는 user.block을 쓰세요)",
        ));
    }
    let ch = state
        .channels
        .get(&p.channel)
        .ok_or_else(|| RpcError::not_found("없는 채널"))?;
    if !state
        .channel_permissions(&me.user(), &p.channel)
        .contains(Permissions::VIEW_CHANNEL)
    {
        return Err(RpcError::forbidden("이 채널을 볼 권한이 없습니다"));
    }
    let reason = p.reason.trim().to_owned();
    if reason.is_empty() {
        return Err(RpcError::invalid_input("신고 사유를 적어 주세요"));
    }
    rpc(crews::check_text(&reason, MAX_REASON_CHARS, "신고 사유"))?;
    let author = message_author(node, ctx, &state, &p.space, &p.channel, &p.message)?
        .ok_or_else(|| RpcError::not_found("없는 메시지"))?;
    if author == me.user() {
        return Err(RpcError::invalid_input("자기 메시지는 신고할 수 없습니다"));
    }
    let target = ReportTarget {
        space: p.space,
        channel: p.channel,
        message: p.message,
        author,
        volatile: ch.is_volatile(),
    };
    let report_id = rpc(moderation::report(node, &state, target, &reason, p.hide).await)?;
    Ok(ReportRef { report_id })
}

/// 운영진(추방·밴·메시지 관리 권한)인 크루와 내 사용자 ID.
fn moderator_state(node: &Node, space: &SpaceId) -> Result<(Arc<SpaceState>, UserId), RpcError> {
    let (state, me) = rpc(crews::crew_state(node, space))?;
    if !moderation::is_moderator(&state, &me) {
        return Err(RpcError::forbidden(
            "신고는 운영진(추방·밴·메시지 관리 권한)만 볼 수 있습니다",
        ));
    }
    Ok((state, me))
}

pub(crate) async fn report_list(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::ReportList>,
) -> R<m::ReportList> {
    moderator_state(node, &p.space)?;
    Ok(moderation::list_reports(node, &p.space, p.status))
}

pub(crate) async fn report_resolve(
    node: &Arc<Node>,
    ctx: &CallCtx,
    p: P<m::ReportResolve>,
) -> R<m::ReportResolve> {
    let (state, _) = moderator_state(node, &p.space)?;
    let note = p.note.clone().unwrap_or_default();
    rpc(crews::check_text(
        &note,
        moderation::MAX_NOTE_CHARS,
        "처리 메모",
    ))?;
    let (report, _) = rpc(moderation::open_report(node, &p.space, &p.report_id))?;
    let mut author = None;
    if p.action != ReportAction::Dismiss {
        // 신고 내용의 작성자를 믿지 않고 이 기기의 메시지 사본으로 확인합니다(남을 엮어 넣지 못하게).
        author = message_author(
            node,
            ctx,
            &state,
            &p.space,
            &report.channel,
            &report.message,
        )?;
        if author.is_none() {
            return Err(RpcError::not_found(
                "신고된 메시지를 이 기기에서 찾지 못해 처리할 수 없습니다(dismiss만 가능)",
            ));
        }
        // 확인 대기열이 휘발성 채널 신고(신고 ID·메모)를 디스크에 쓰지 않게 신고된 채널을 함께 넘깁니다(승인 때
        // 다시 부를 때는 쓰지 않는 칸).
        let mut params = serde_json::to_value(&p).map_err(|e| RpcError::internal(e.to_string()))?;
        if let Some(obj) = params.as_object_mut() {
            obj.insert("channel".to_owned(), serde_json::json!(report.channel));
        }
        admin::confirm_when_others(node, ctx, "report.resolve", &params).await?;
    }
    match (p.action, author) {
        (ReportAction::Dismiss, _) | (_, None) => {}
        (ReportAction::DeleteMessage, Some(_)) => {
            super::message::delete(
                node,
                ctx,
                MessageRefParams {
                    space: p.space,
                    channel: report.channel,
                    id: report.message,
                },
            )
            .await?;
        }
        // 추방·밴 사유는 멤버 모두가 읽는 상태 레인에 남으므로 운영진 메모를 싣지 않습니다.
        (ReportAction::Kick, Some(user)) => {
            // 이미 나간 사람이면 할 일이 없으므로 처리 기록만 남깁니다.
            if state.is_member(&user) {
                super::member::kick(
                    node,
                    ctx,
                    MemberKickParams {
                        space: p.space,
                        user,
                        reason: None,
                    },
                )
                .await?;
            }
        }
        (ReportAction::Ban, Some(user)) => {
            if !state.bans.contains_key(&user) {
                ban_inner(
                    node,
                    MemberBanParams {
                        space: p.space,
                        user,
                        reason: None,
                        delete_recent: false,
                    },
                )
                .await?;
            }
        }
    }
    let resolution = if note.is_empty() {
        p.action.as_str().to_owned()
    } else {
        format!("{}: {note}", p.action.as_str())
    };
    // 처리하는 사이 상태가 바뀌었을 수 있어 다시 읽어 운영진에게 보냅니다.
    let state = node.space_state(&p.space).unwrap_or(state);
    rpc(moderation::mark_resolved(
        node,
        &state,
        &p.space,
        &p.report_id,
        resolution,
        author.as_ref(),
    ))?;
    Ok(empty())
}
