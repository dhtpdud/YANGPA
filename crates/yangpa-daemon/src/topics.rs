//! gossip 토픽 고르기: 크루 토픽 하나와, 볼 수 있는 기기가 많은 비공개 키 그룹마다 그룹 토픽(protocol.md "실시간 전파").
//!
//! - 그룹 토픽은 그 그룹 채널을 볼 수 있는 멤버의 노드끼리만 메시를 이룹니다(허용 목록이 토픽마다 따로). 그 그룹
//!   채널 이벤트와 그룹 키 스트림(`KeyGroup`) 이벤트를 싣습니다.
//! - 볼 수 있는 기기가 [`NodeConfig::group_topic_min`](crate::config::NodeConfig) 이하인 그룹은 토픽 없이 모두에게 직접
//!   보냅니다(작은 그룹은 메시가 서지 않아 gossip이 직접 보내기보다 느림).
//! - 같은 그룹의 채널은 열람 규칙이 같아 볼 수 있는 사람도 같습니다(`ChannelSettings::view_rule`). 그래서 그룹마다 채널
//!   하나의 상태 색인 값을 씁니다.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use parking_lot::Mutex;
use yangpa_core::ids::{ChannelId, KeyGroupId, SpaceId, UserId};
use yangpa_core::model::{Permissions, SpaceState};
use yangpa_core::wire::GossipTopic;

use crate::node::Node;
use crate::state_index::StateIndex;
use crate::sync_service::{channel_view, member_peers_where};

/// 노드가 지금 구독한 그룹 토픽(공간마다). 상태가 바뀌어 빠진 그룹은 다음 [`refresh`]에서 구독을 풉니다.
#[derive(Default)]
pub(crate) struct GroupTopics {
    subscribed: Mutex<HashMap<SpaceId, BTreeSet<KeyGroupId>>>,
}

/// 이 사본에서 토픽을 둘 그룹(볼 수 있는 기기가 `min`보다 많은 비공개 그룹)과 그 그룹 채널 하나.
pub(crate) fn topic_groups(
    space: &SpaceId,
    state: &SpaceState,
    index: &StateIndex,
    min: usize,
) -> BTreeMap<KeyGroupId, ChannelId> {
    let mut out = BTreeMap::new();
    for (c, ch) in &state.channels {
        let Some(g) = state.channel_key_group(space, ch) else {
            continue;
        };
        if out.contains_key(&g) {
            continue;
        }
        let view = channel_view(index, c);
        if !view.everyone && view.viewer_devices > min {
            out.insert(g, *c);
        }
    }
    out
}

fn can_view(state: &SpaceState, user: &UserId, channel: &ChannelId) -> bool {
    state
        .channel_permissions(user, channel)
        .contains(Permissions::VIEW_CHANNEL)
}

/// 그룹 토픽 구독과 허용 목록을 지금 상태에 맞춥니다(크루 토픽과 같은 때, `sync_service::refresh_space_peers`).
/// `active`가 거짓(휴면·만료된 연락처)이면 모두 풉니다.
pub(crate) fn refresh(node: &Node, space: &SpaceId, state: &Arc<SpaceState>, active: bool) {
    let mut want: BTreeMap<KeyGroupId, ChannelId> = BTreeMap::new();
    if active && let Ok(me) = node.me() {
        let index = node.sync.index.get(space, state);
        want = topic_groups(space, state, &index, node.cfg.group_topic_min);
        want.retain(|_, c| can_view(state, &me.user(), c));
    }
    let old = {
        let mut subs = node.sync.groups.subscribed.lock();
        let now: BTreeSet<KeyGroupId> = want.keys().copied().collect();
        let old = if now.is_empty() {
            subs.remove(space)
        } else {
            subs.insert(*space, now)
        };
        old.unwrap_or_default()
    };
    for g in old.iter().filter(|g| !want.contains_key(g)) {
        let t = GossipTopic::Group(*space, *g);
        node.net.unsubscribe(t);
        node.net.set_allowed_peers(t, Vec::new());
    }
    for (g, c) in want {
        let t = GossipTopic::Group(*space, g);
        let allowed = member_peers_where(node, state, |u| can_view(state, u, &c));
        node.net.subscribe(t);
        node.net.set_allowed_peers(t, allowed);
    }
}

/// 공간의 그룹 토픽을 모두 풉니다(공간을 지울 때).
pub(crate) fn forget(node: &Node, space: &SpaceId) {
    let old = node.sync.groups.subscribed.lock().remove(space);
    for g in old.unwrap_or_default() {
        let t = GossipTopic::Group(*space, g);
        node.net.unsubscribe(t);
        node.net.set_allowed_peers(t, Vec::new());
    }
}

/// 이 그룹의 토픽을 지금 구독 중인지(보낼 때 그 토픽에 실을지).
pub(crate) fn subscribed(node: &Node, space: &SpaceId, group: &KeyGroupId) -> bool {
    node.sync
        .groups
        .subscribed
        .lock()
        .get(space)
        .is_some_and(|s| s.contains(group))
}

/// 채널 이벤트를 실을 그룹 토픽(그 채널 그룹의 토픽을 구독 중일 때).
pub(crate) fn channel_topic(
    node: &Node,
    space: &SpaceId,
    state: &SpaceState,
    channel: &ChannelId,
) -> Option<GossipTopic> {
    let ch = state.channels.get(channel)?;
    let g = state.channel_key_group(space, ch)?;
    subscribed(node, space, &g).then_some(GossipTopic::Group(*space, g))
}
