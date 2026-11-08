//! `channel.join`/`leave`/`allow_api_read`: 휘발성 채널 참가·퇴장, 자동화 토큰 읽기 허용.
//!
//! 담당: B2(휘발성). 실제 일은 `crate::volatile`이 합니다.

use std::sync::Arc;

use yangpa_rpc::api::m;

use super::{CallCtx, P, R};
use crate::node::Node;
use crate::volatile;

/// 입장·퇴장 방식 휘발성 채널에 들어갑니다(온라인 참가자와 입장을 주고받고, 과거 기록 공개면 스냅숏).
pub(crate) async fn join(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::ChannelJoin>,
) -> R<m::ChannelJoin> {
    volatile::join(node, p.space, p.channel).await
}

/// 나갑니다. 이 기기의 그 채널 내용·세션 키를 바로 지웁니다.
pub(crate) async fn leave(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::ChannelLeave>,
) -> R<m::ChannelLeave> {
    volatile::leave(node, p.space, p.channel).await
}

/// 휘발성 채널을 자동화 토큰이 읽을 수 있게 하거나 막습니다(사람·소유자만, 메서드 표가 검사).
pub(crate) async fn allow_api_read(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::ChannelAllowApiRead>,
) -> R<m::ChannelAllowApiRead> {
    volatile::set_api_read(node, p.space, p.channel, p.allowed)
}
