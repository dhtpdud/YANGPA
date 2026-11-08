//! `identity.*`, `device.*`, `profile.get`/`profile.set`(코어).

use std::sync::Arc;

use yangpa_rpc::RpcError;
use yangpa_rpc::api::{DeviceLinkJoined, DeviceView, IdentityRef, IdentityView, m};

use super::{CallCtx, P, R, me, rpc};
use crate::identity as id;
use crate::node::Node;
use crate::{blobs, devlink, error::Error};

pub(crate) async fn create(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::IdentityCreate>,
) -> R<m::IdentityCreate> {
    let me = rpc(id::create(node, &p.name))?;
    Ok(IdentityRef {
        user: me.user(),
        device: me.device_id(),
    })
}

pub(crate) async fn get(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    _p: P<m::IdentityGet>,
) -> R<m::IdentityGet> {
    let me = me(node)?;
    Ok(IdentityView {
        user: me.user(),
        device: me.device_id(),
        profile: me.profile.profile.clone(),
        devices: me.devices.list.devices.clone(),
        first_syncing: id::first_syncing(node),
    })
}

pub(crate) async fn device_list(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    _p: P<m::DeviceList>,
) -> R<m::DeviceList> {
    let me = me(node)?;
    Ok(me
        .devices
        .list
        .devices
        .iter()
        .map(|entry| DeviceView {
            mobile: id::is_mobile(node, &me, &entry.device),
            entry: entry.clone(),
        })
        .collect())
}

pub(crate) async fn device_rename(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::DeviceRename>,
) -> R<m::DeviceRename> {
    rpc(id::rename_device(node, p.device, &p.name).await)?;
    Ok(super::empty())
}

/// 기기 폐기: 새 서명 목록을 모든 공간에 넣고(확정 기기가 키 그룹에서 빼고 곧바로 회전), 그 기기의 인증을
/// 끊고 다시 받지 않습니다(`identity::revoke_device`).
pub(crate) async fn device_revoke(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::DeviceRevoke>,
) -> R<m::DeviceRevoke> {
    rpc(id::revoke_device(node, p.device).await)?;
    Ok(super::empty())
}

/// 새 기기를 들일 링크를 띄웁니다.
pub(crate) async fn device_link_create(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    _p: P<m::DeviceLinkCreate>,
) -> R<m::DeviceLinkCreate> {
    devlink::create(node).await
}

pub(crate) async fn device_link_pending(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    _p: P<m::DeviceLinkPending>,
) -> R<m::DeviceLinkPending> {
    Ok(devlink::pending(node))
}

pub(crate) async fn device_link_decide(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::DeviceLinkDecide>,
) -> R<m::DeviceLinkDecide> {
    devlink::decide(node, p.approve).await?;
    Ok(super::empty())
}

pub(crate) async fn device_link_cancel(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    _p: P<m::DeviceLinkCancel>,
) -> R<m::DeviceLinkCancel> {
    devlink::cancel(node);
    Ok(super::empty())
}

pub(crate) async fn device_link_join(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::DeviceLinkJoin>,
) -> R<m::DeviceLinkJoin> {
    let code = devlink::join(node, p.url.expose()).await?;
    Ok(DeviceLinkJoined { code })
}

pub(crate) async fn device_link_abort(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    _p: P<m::DeviceLinkAbort>,
) -> R<m::DeviceLinkAbort> {
    devlink::abort(node);
    Ok(super::empty())
}

/// 계정 삭제: 이 기기의 신원과 로컬 데이터를 지웁니다. 다른 기기의 계정이나 남이 이미 받은 메시지는 그대로입니다.
pub(crate) async fn delete(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    _p: P<m::IdentityDelete>,
) -> R<m::IdentityDelete> {
    rpc(id::wipe(node).await)?;
    Ok(super::empty())
}

pub(crate) async fn profile_get(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::ProfileGet>,
) -> R<m::ProfileGet> {
    let me = me(node)?;
    match p.user {
        None => Ok(me.profile.profile.clone()),
        Some(u) if u == me.user() => Ok(me.profile.profile.clone()),
        Some(u) => match node
            .store
            .get_profile(&u)
            .map_err(Error::from)
            .map_err(RpcError::from)?
        {
            Some(sp) => Ok(sp.profile),
            None => Err(RpcError::not_found("모르는 사용자입니다")),
        },
    }
}

pub(crate) async fn profile_set(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::ProfileSet>,
) -> R<m::ProfileSet> {
    let user = me(node)?.user();
    // 이미지 경로는 블롭 영역이 읽어 블롭으로 만듭니다(빈 문자열이면 지움).
    let avatar = match p.avatar_path.as_deref() {
        None => None,
        Some("") => Some(None),
        Some(path) => Some(Some(blobs::import_profile_image(node, path).await?)),
    };
    let banner = match p.banner_path.as_deref() {
        None => None,
        Some("") => Some(None),
        Some(path) => Some(Some(blobs::import_profile_image(node, path).await?)),
    };
    let profile = rpc(id::set_profile(
        node,
        id::ProfileChange {
            name: p.name,
            bio: p.bio,
            theme_color: p.theme_color,
            status: p.status,
            avatar,
            banner,
        },
    ))?;
    // 지금 인증된 친구·멤버에게 보내고, 여럿의 공간에는 상태 이벤트로 실어 직접 연결되지 않은 멤버도 받게 합니다.
    id::push_profile(node);
    // 내 화면의 멤버 목록·메시지 이름도 바로 바뀌게 알립니다(상태 이벤트가 없는 1대1·작은 공간은 다른 알림이 없음).
    node.notify
        .publish(yangpa_rpc::api::Notification::ProfileUpdated { user });
    Ok(profile)
}
