//! `profile.set_crew`: 크루별 별명. `profile.get`/`set`은 코어(`api/identity.rs`).
//!
//! 별명은 크루 상태의 내 멤버 항목에 둡니다(`MemberProfile{nickname}`: 내가 내 항목에만 내는 이벤트로 역할을 싣지
//! 않으므로, 동시에 바뀐 역할을 되돌리거나 역할 변경 권한 검사에 걸리지 않음). 그 크루 멤버에게만 보이고 상태
//! 이벤트로 함께 동기화됩니다.

use std::sync::Arc;

use yangpa_core::body::EventBody;
use yangpa_core::model::SpaceKind;
use yangpa_rpc::RpcError;
use yangpa_rpc::api::m;
use yangpa_sync::state::limits::MAX_NICKNAME_CHARS;

use super::{CallCtx, P, R, me, rpc};
use crate::author::{self, KeyChoice};
use crate::node::Node;

pub(crate) async fn set_crew(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::ProfileSetCrew>,
) -> R<m::ProfileSetCrew> {
    let me = me(node)?;
    let st = node
        .space_state(&p.space)
        .ok_or_else(|| RpcError::not_found("모르는 공간입니다"))?;
    if st.kind != SpaceKind::Crew {
        return Err(RpcError::invalid_input(
            "크루에서만 별명을 정할 수 있습니다",
        ));
    }
    if !st.is_member(&me.user()) {
        return Err(RpcError::forbidden("이 크루의 멤버가 아닙니다"));
    }
    // 빈 문자열은 지움(상태 기계 규칙). 상태 기계와 같은 규칙을 미리 보아 잘못된 입력은 1002로 돌려줍니다.
    let nickname = p.nickname.unwrap_or_default();
    let chars = nickname.chars().count();
    if !nickname.is_empty()
        && (chars > MAX_NICKNAME_CHARS
            || nickname.trim().is_empty()
            || nickname.chars().any(char::is_control))
    {
        return Err(RpcError::invalid_input(format!(
            "별명은 1~{MAX_NICKNAME_CHARS}자, 제어 문자 없이 적어야 합니다"
        )));
    }
    // 뱃지와 헷갈리지 않게(받는 쪽 상태 기계는 막지 않음: 이미 있는 별명과 옛 빌드).
    if yangpa_core::badge::has_emoji(&nickname) {
        return Err(RpcError::invalid_input("별명에 이모지를 넣을 수 없습니다"));
    }
    let body = EventBody::MemberProfile {
        nickname: Some(nickname),
        profile: None,
    };
    rpc(author::publish(node, p.space, None, &body, KeyChoice::State).await)?;
    Ok(super::empty())
}

pub(crate) async fn badge_get(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::BadgeGet>,
) -> R<m::BadgeGet> {
    // 내 뱃지는 주기 작업을 기다리지 않고 지금 기준으로 봅니다.
    if p.user == me(node)?.user() {
        crate::badges::refresh(node, None);
    }
    Ok(crate::badges::view(node, &p.user))
}

pub(crate) async fn badge_set_shown(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::BadgeSetShown>,
) -> R<m::BadgeSetShown> {
    rpc(crate::badges::set_shown(node, &p.shown))
}

pub(crate) async fn badge_grant_founder(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::BadgeGrantFounder>,
) -> R<m::BadgeGrantFounder> {
    rpc(crate::badges::grant_founder(node, p.user))?;
    Ok(super::empty())
}
