//! `voice.*`: 음성·화면공유 신호. 실제 처리는 `voice` 모듈이 맡습니다.

use std::sync::Arc;

use yangpa_rpc::api::m;
use yangpa_rpc::api::serde_util::SecretString;

use super::{CallCtx, P, R, empty, rpc};
use crate::node::Node;
use crate::voice;

/// 참가. 자동화 토큰에는 프레임 키를 주지 않습니다(통화 내용은 휘발성 채널처럼 AI에게 열지 않음,
/// `voice.participants` 알림도 같음).
pub(crate) async fn join(node: &Arc<Node>, ctx: &CallCtx, p: P<m::VoiceJoin>) -> R<m::VoiceJoin> {
    let mut r = rpc(voice::join(node, p.space, p.channel).await)?;
    if ctx.is_automation() {
        r.media_key = SecretString::default();
        r.generation = 0;
    }
    Ok(r)
}

pub(crate) async fn leave(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::VoiceLeave>,
) -> R<m::VoiceLeave> {
    rpc(voice::leave(node, p.space, p.channel).await)?;
    Ok(empty())
}

pub(crate) async fn signal(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::VoiceSignal>,
) -> R<m::VoiceSignal> {
    rpc(voice::relay_from_app(node, p.space, p.channel, p.call, p.to, p.payload).await)?;
    Ok(empty())
}

/// Tor 통화의 소리 패킷(봉인됨)을 지금 참가자에게 보냅니다.
pub(crate) async fn media(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::VoiceMedia>,
) -> R<m::VoiceMedia> {
    rpc(voice::media::send(
        node,
        p.space,
        p.channel,
        p.call,
        &p.data,
        p.to.as_deref(),
    ))?;
    Ok(empty())
}

pub(crate) async fn state(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::VoiceState>,
) -> R<m::VoiceState> {
    rpc(voice::set_state(node, p.space, p.channel, p.muted, p.deafened, p.screen).await)?;
    Ok(empty())
}

pub(crate) async fn watch(
    node: &Arc<Node>,
    _ctx: &CallCtx,
    p: P<m::VoiceWatch>,
) -> R<m::VoiceWatch> {
    rpc(voice::watch(node, p.space, p.channel, p.device, p.watching).await)?;
    Ok(empty())
}
