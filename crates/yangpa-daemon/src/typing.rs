//! 글 쓰는 중 알림(`channel.typing`): 디스코드처럼 채널에서 누가 글을 쓰고 있는지 잠깐 보여 줍니다.
//!
//! - 보내기([`send`]): 앱이 입력하는 동안 몇 초마다 부릅니다. 같은 채널에는 [`SEND_EVERY`]에 한 번만 보내고, 지금
//!   이어진 기기 중 그 채널을 볼 수 있는 멤버 기기에게만 갑니다. P2P 전용 채널은 함께 참여한 P2P 피어에게만,
//!   휘발성 채널은 지금 참가 중인 기기에게만 보냅니다(내용과 같은 길).
//! - 받기([`handle`]): 보낸 기기가 그 채널에 글을 쓸 수 있는 멤버이고 길이 맞을 때만 `channel.typing`을 알립니다.
//!   차단한 사람은 알리지 않고, 익명 채널이면 사용자를 가리고 가명을 붙입니다.
//!
//! 저장하지 않고(RAM의 마지막 보낸·받은 시각만) 로그에도 남기지 않습니다.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use yangpa_core::event::StreamId;
use yangpa_core::ids::{ChannelId, DeviceId, SpaceId, UserId};
use yangpa_core::model::{Permissions, SpaceState};
use yangpa_core::wire::{WireErrorCode, YpRequest, YpResponse};
use yangpa_net::PeerId;
use yangpa_rpc::api::{ANONYMOUS_USER, Notification};

use crate::error::{Error, Result};
use crate::node::Node;

/// 같은 채널에 다시 보내는 최소 간격. 앱은 이 간격 안에서 몇 번 불러도 됩니다(받는 쪽은 몇 초 동안 보여 줌).
pub(crate) const SEND_EVERY: Duration = Duration::from_secs(2);
/// 같은 기기·채널에서 받은 알림을 앱에 다시 알리는 최소 간격(많이 보내는 기기가 앱을 흔들지 못하게).
const RECV_EVERY: Duration = Duration::from_secs(1);
/// 기억하는 시각 수 상한(넘으면 비움).
const MAX_ENTRIES: usize = 4096;

/// 마지막으로 보낸·받은 시각(RAM).
#[derive(Default)]
pub(crate) struct TypingState {
    sent: Mutex<HashMap<(SpaceId, ChannelId), Instant>>,
    seen: Mutex<HashMap<(DeviceId, SpaceId, ChannelId), Instant>>,
}

/// 간격 안이면 거짓, 아니면 시각을 적고 참.
fn due<K: std::hash::Hash + Eq>(map: &Mutex<HashMap<K, Instant>>, key: K, every: Duration) -> bool {
    let now = Instant::now();
    let mut m = map.lock();
    if m.get(&key).is_some_and(|t| now.duration_since(*t) < every) {
        return false;
    }
    if m.len() >= MAX_ENTRIES {
        m.retain(|_, t| now.duration_since(*t) < every);
    }
    m.insert(key, now);
    true
}

fn can_write(state: &SpaceState, user: &UserId, channel: &ChannelId) -> bool {
    state.is_member(user)
        && state
            .channel_permissions(user, channel)
            .contains(Permissions::VIEW_CHANNEL | Permissions::SEND_MESSAGES)
}

/// 이 채널의 글을 받을 다른 기기의 피어.
fn targets(node: &Node, space: &SpaceId, state: &SpaceState, channel: &ChannelId) -> Vec<PeerId> {
    if state.is_p2p(channel) {
        return crate::p2p::channel_peers(node, space, state, channel)
            .into_iter()
            .map(|(p, ..)| p)
            .collect();
    }
    if state.channels.get(channel).is_some_and(|c| c.is_volatile()) {
        return crate::volatile::participant_peers(node, space, channel);
    }
    let my_device = node.me().ok().map(|m| m.device_id());
    node.peers
        .authed_peers()
        .into_iter()
        .filter(|(_, user, device)| {
            Some(*device) != my_device
                && state.device_belongs(user, device)
                && !crate::archive::archive_only(node, user, device)
                && state
                    .channel_permissions(user, channel)
                    .contains(Permissions::VIEW_CHANNEL)
        })
        .map(|(p, ..)| p)
        .collect()
}

/// `channel.typing`: 내가 이 채널에 글을 쓰는 중이라고 알립니다.
pub(crate) fn send(node: &Arc<Node>, space: SpaceId, channel: ChannelId) -> Result<()> {
    let (state, me) = crate::crews::member_state(node, &space)?;
    if !state.channels.contains_key(&channel) {
        return Err(Error::not_found("없는 채널"));
    }
    if !can_write(&state, &me, &channel) {
        return Err(Error::forbidden("이 채널에 글을 쓸 권한이 없습니다"));
    }
    if state.is_p2p(&channel) && !crate::p2p::p2p_joined(node, &space, &channel) {
        return Err(Error::forbidden("P2P 전용 채널에 참여하지 않았습니다"));
    }
    if !due(&node.typing.sent, (space, channel), SEND_EVERY) {
        return Ok(());
    }
    for peer in targets(node, &space, &state, &channel) {
        node.spawn_detached(move |node| async move {
            // 못 닿아도 다음 입력 때 다시 보내므로 따로 다루지 않습니다.
            let _ = crate::p2p::request(&node, peer, YpRequest::Typing { space, channel }).await;
        });
    }
    Ok(())
}

/// 받은 `Typing`.
pub(crate) fn handle(node: &Node, peer: PeerId, space: SpaceId, channel: ChannelId) -> YpResponse {
    let Some((user, device)) = node
        .peers
        .identity_of(&peer)
        .or_else(|| node.peers.p2p_identity(&peer))
    else {
        return YpResponse::error(WireErrorCode::Unauthenticated, "인증 전");
    };
    let Some(state) = node.space_state(&space) else {
        return YpResponse::Ok;
    };
    let Ok(me) = node.me() else {
        return YpResponse::Ok;
    };
    let shown = state.device_belongs(&user, &device)
        && user != me.user()
        && can_write(&state, &user, &channel)
        && state
            .channel_permissions(&me.user(), &channel)
            .contains(Permissions::VIEW_CHANNEL)
        && crate::p2p::route_ok(node, &peer, &space, &state, &StreamId::Channel(channel))
        && state.channels.get(&channel).is_some_and(|c| {
            !c.is_volatile() || crate::volatile::is_participant(node, &space, &channel, &device)
        })
        && !crate::moderation::is_author_blocked(node, &user, &device);
    if shown && due(&node.typing.seen, (device, space, channel), RECV_EVERY) {
        let anonymous = state.is_anonymous(&channel);
        node.notify.publish(Notification::Typing {
            space,
            channel,
            user: if anonymous { ANONYMOUS_USER } else { user },
            anon_name: anonymous.then(|| crate::anon::name(&space, &channel, &user)),
        });
    }
    // 보여 주지 않을 때도 같은 답(채널이 있는지·참가 중인지 드러내지 않게).
    YpResponse::Ok
}
