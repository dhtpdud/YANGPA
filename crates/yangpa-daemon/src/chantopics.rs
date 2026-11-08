//! 채널 묶음·알림 토픽(선택 기능, 설정 `net.channel_topics`, 기본 꺼짐, protocol.md "채널 토픽").
//!
//! 켜면 gossip을 쓰는 크루에서 모두 보는 채널 이벤트를 크루 토픽 대신 그 채널의 묶음 토픽(`GossipTopic::Bucket`, 크루마다
//! 16개)에 싣고, 멘션·답장이 있는 메시지는 알림 토픽(`GossipTopic::Notify`)에도 싣습니다. 노드는 최근 7일 안에 연
//! 채널(`channel.mark_read`)의 묶음과 알림 토픽만 구독하므로, 따라가지 않는 채널의 새 글은 실시간으로 오지 않고 멘션은
//! 알림 토픽으로, 나머지는 따라잡기와 안 읽음 힌트로 압니다.
//!
//! - 끈 노드도 gossip을 쓰는 크루에서는 묶음 16개를 모두 구독합니다. 켠 노드가 묶음에 올린 글을 끈 노드가 실시간으로
//!   받게 하려는 것으로, 끈 노드가 받는 양은 크루 토픽 하나일 때와 같습니다. 끈 노드는 예전처럼 크루 토픽에 올립니다.
//! - 묶음 토픽에 받을 피어가 없으면(`NoPeers`) 크루 토픽에 싣습니다.
//! - 알림 토픽 중계: 모두 보는 채널의 메시지이고 멘션이나 답장이 있어야 하며, 멘션이 20명을 넘거나 `@everyone`·`@here`가
//!   있으면 그 채널에서 `MENTION_MANY`가 있어야 합니다. 작성 기기마다 몰아서 30개, 분당 10개. 어기면 더 퍼뜨리지 않습니다(`Ignore`).
//! - 안 읽음 힌트: 켠 노드는 1분마다 연결된 멤버 한 곳에 따라가지 않는 채널의 끝(`GetChannelTips`)을 묻고, 이 기기가 가진
//!   것보다 늦은 메시지가 있는 채널에 `ChannelView::has_new`를 켭니다(RAM에만).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use parking_lot::Mutex;
use rand::seq::SliceRandom;
use tracing::warn;
use yangpa_core::event::{EventKind, SignedEvent, StreamId};
use yangpa_core::ids::{ChannelId, SpaceId, UserId};
use yangpa_core::model::{Permissions, SpaceState};
use yangpa_core::time::Timestamp;
use yangpa_core::wire::{
    CHANNEL_BUCKETS, GossipTopic, MAX_TIP_CHANNELS, WireErrorCode, YpRequest, YpResponse,
    channel_bucket,
};
use yangpa_net::PeerId;
use yangpa_rpc::api::Notification;

use crate::node::Node;
use crate::ratelimit::{EventAuthor, EventClass, EventLimiter};
use crate::state_index::StateIndex;
use crate::sync_service::channel_view;

/// 설정 키(`bool`, 없으면 꺼짐).
pub(crate) const SETTING: &str = "net.channel_topics";
/// 공간마다 연 채널 기록(채널 → 마지막으로 연 시각 ms). 메시지 내용은 없고 채널 ID와 시각뿐입니다.
const FOLLOW_PREFIX: &str = "chantopics.follow.";
/// 이 안에 연 채널을 따라갑니다.
const FOLLOW_MS: u64 = 7 * 24 * 60 * 60 * 1000;
/// 공간마다 기억하는 채널 수(크루 채널 상한과 같음).
const FOLLOW_MAX: usize = 500;
/// `MENTION_MANY` 없이 알림 토픽에 실리는 멘션 수.
pub(crate) const PLAIN_MENTIONS: usize = 20;

#[derive(Default)]
pub(crate) struct ChannelTopics {
    /// 설정 값(처음 볼 때 읽음).
    enabled: Mutex<Option<bool>>,
    /// 공간마다 구독한 묶음·알림 토픽.
    subscribed: Mutex<HashMap<SpaceId, BTreeSet<GossipTopic>>>,
    /// 알림 토픽 작성 기기별 버킷.
    notify_limit: Mutex<EventLimiter>,
    /// 진단·시험: 묶음 토픽으로 올린 수, 묶음에 받을 피어가 없어 크루 토픽으로 올린 수, 알림 토픽으로 올린 수.
    pub(crate) bucket_sent: AtomicU64,
    pub(crate) bucket_fallback: AtomicU64,
    pub(crate) notify_sent: AtomicU64,
    /// 멤버가 알려 준 따라가지 않는 채널의 끝(공간 → 채널 → 가장 늦은 메시지 논리 시계). RAM에만 둡니다.
    tips: Mutex<HashMap<SpaceId, HashMap<ChannelId, u64>>>,
    /// 끝 묻기가 도는 중(겹치지 않게).
    polling: AtomicBool,
}

/// 채널 토픽을 켰는지.
pub(crate) fn enabled(node: &Node) -> bool {
    let mut g = node.sync.channels.enabled.lock();
    if let Some(on) = *g {
        return on;
    }
    let on = match node.store.get_setting::<bool>(SETTING) {
        Ok(v) => v.unwrap_or(false),
        Err(e) => {
            warn!(error = %e, "채널 토픽 설정을 읽지 못함");
            false
        }
    };
    *g = Some(on);
    on
}

/// 켜고 끕니다. 모든 공간의 구독을 바로 맞춥니다.
pub(crate) fn set_enabled(node: &Node, on: bool) -> crate::error::Result<()> {
    node.store.put_setting(SETTING, &on)?;
    *node.sync.channels.enabled.lock() = Some(on);
    if !on {
        node.sync.channels.tips.lock().clear();
    }
    for space in node.space_ids() {
        crate::sync_service::refresh_space_peers(node, &space);
    }
    Ok(())
}

fn follow_key(space: &SpaceId) -> String {
    format!("{FOLLOW_PREFIX}{space}")
}

fn load_follows(node: &Node, space: &SpaceId) -> BTreeMap<ChannelId, u64> {
    node.store
        .get_setting(&follow_key(space))
        .ok()
        .flatten()
        .unwrap_or_default()
}

/// 채널을 열었습니다(`channel.mark_read`). 켜져 있을 때만 기록하고, 그 묶음을 아직 구독하지 않았으면 구독을 맞춥니다.
/// 새로 따라가게 된 채널은 연결된 멤버 한 곳과 따라잡아 그동안 못 받은 글을 채웁니다.
pub(crate) fn follow(node: &Arc<Node>, space: &SpaceId, channel: &ChannelId) {
    if !enabled(node) {
        return;
    }
    let now = Timestamp::now().0;
    let mut f = load_follows(node, space);
    let fresh = f
        .insert(*channel, now)
        .is_none_or(|t| now.saturating_sub(t) >= FOLLOW_MS);
    f.retain(|_, t| now.saturating_sub(*t) < FOLLOW_MS);
    while f.len() > FOLLOW_MAX {
        let Some(oldest) = f.iter().min_by_key(|(_, t)| **t).map(|(c, _)| *c) else {
            break;
        };
        f.remove(&oldest);
    }
    if let Err(e) = node.store.put_setting(&follow_key(space), &f) {
        warn!(error = %e, "따라가는 채널을 저장하지 못함");
        return;
    }
    // 따라가게 된 채널의 힌트는 지웁니다(아래 따라잡기와 실시간 전파로 받음).
    let hinted = node
        .sync
        .channels
        .tips
        .lock()
        .get_mut(space)
        .and_then(|t| t.remove(channel))
        .is_some();
    if hinted {
        node.notify
            .publish(Notification::SpaceUpdated { space: *space });
    }
    let topic = GossipTopic::Bucket(*space, channel_bucket(space, channel));
    let have = node
        .sync
        .channels
        .subscribed
        .lock()
        .get(space)
        .is_some_and(|s| s.contains(&topic));
    if !have {
        crate::sync_service::refresh_space_peers(node, space);
    }
    if fresh
        && let Some(state) = node.space_state(space)
        && let Some(peer) = node
            .peers
            .authed_peers()
            .into_iter()
            .find(|(_, u, d)| state.is_member(u) && state.device_belongs(u, d))
            .map(|(p, _, _)| p)
    {
        crate::sync_service::spawn_pull(node, *space, peer, crate::sync_service::Pull::Full);
    }
}

/// 따라잡기에서 맞출 채널 스트림인지 보는 함수: 채널 토픽을 쓰지 않거나, 묶음에 싣지 않는 채널이거나, 따라가는 채널.
pub(crate) fn sync_filter<'a>(
    node: &Node,
    space: &SpaceId,
    state: &'a SpaceState,
    index: &'a StateIndex,
) -> impl Fn(&ChannelId) -> bool + 'a {
    let using = in_use(node, index);
    let now = Timestamp::now().0;
    let follows = if using {
        load_follows(node, space)
    } else {
        BTreeMap::new()
    };
    move |c| {
        !using
            || !bucket_channel(index, state, c)
            || follows
                .get(c)
                .is_some_and(|t| now.saturating_sub(*t) < FOLLOW_MS)
    }
}

/// 모두 보는 일반 채널(묶음 토픽에 싣는 채널). 휘발성·P2P 전용 채널은 gossip에 싣지 않습니다.
fn bucket_channel(index: &StateIndex, state: &SpaceState, channel: &ChannelId) -> bool {
    state
        .channels
        .get(channel)
        .is_some_and(|ch| !ch.is_volatile())
        && !state.is_p2p(channel)
        && channel_view(index, channel).everyone
}

/// 구독할 묶음: 최근 7일 안에 연, 내가 볼 수 있는 모두 보는 채널의 묶음.
fn followed_buckets(
    node: &Node,
    space: &SpaceId,
    state: &SpaceState,
    index: &StateIndex,
    me: &UserId,
) -> BTreeSet<u8> {
    let now = Timestamp::now().0;
    load_follows(node, space)
        .into_iter()
        .filter(|(c, t)| {
            now.saturating_sub(*t) < FOLLOW_MS
                && bucket_channel(index, state, c)
                && state
                    .channel_permissions(me, c)
                    .contains(Permissions::VIEW_CHANNEL)
        })
        .map(|(c, _)| channel_bucket(space, &c))
        .collect()
}

/// gossip을 쓰는 크기(활성 기기가 `push_max_devices`보다 많음).
fn gossip_size(node: &Node, index: &StateIndex) -> bool {
    index.active_count() > node.cfg.push_max_devices
}

/// 이 공간에서 채널 토픽을 쓰는지: 켜져 있고 gossip을 쓰는 크기.
fn in_use(node: &Node, index: &StateIndex) -> bool {
    enabled(node) && gossip_size(node, index)
}

fn all_topics(space: &SpaceId) -> impl Iterator<Item = GossipTopic> + '_ {
    (0..CHANNEL_BUCKETS)
        .map(|b| GossipTopic::Bucket(*space, b))
        .chain([GossipTopic::Notify(*space)])
}

/// 구독과 허용 목록을 맞춥니다(크루 토픽과 같은 때, `sync_service::refresh_space_peers`). 켠 노드는 알림 토픽과 따라가는
/// 묶음을, 끈 노드는 묶음 16개 모두를 구독합니다. 묶음 토픽은 구독하지 않아도 올릴 수 있어야 하므로 gossip을 쓰는 동안은
/// 모든 토픽에 허용 목록(크루와 같은 멤버 노드)을 둡니다.
pub(crate) fn refresh(
    node: &Node,
    space: &SpaceId,
    state: &Arc<SpaceState>,
    allowed: &[PeerId],
    active: bool,
) {
    let index = node.sync.index.get(space, state);
    let using = active && gossip_size(node, &index);
    let mut want: BTreeSet<GossipTopic> = BTreeSet::new();
    if using && !enabled(node) {
        want.extend((0..CHANNEL_BUCKETS).map(|b| GossipTopic::Bucket(*space, b)));
    } else if using && let Ok(me) = node.me() {
        want.insert(GossipTopic::Notify(*space));
        for b in followed_buckets(node, space, state, &index, &me.user()) {
            want.insert(GossipTopic::Bucket(*space, b));
        }
    }
    let old = {
        let mut subs = node.sync.channels.subscribed.lock();
        if want.is_empty() {
            subs.remove(space)
        } else {
            subs.insert(*space, want.clone())
        }
        .unwrap_or_default()
    };
    if !using && old.is_empty() {
        return;
    }
    for t in old.difference(&want) {
        node.net.unsubscribe(*t);
    }
    for t in all_topics(space) {
        node.net
            .set_allowed_peers(t, if using { allowed.to_vec() } else { Vec::new() });
    }
    for t in want.difference(&old) {
        node.net.subscribe(*t);
    }
}

/// 휴면·공간 지우기 때 모두 풉니다(연 채널 기록은 [`forget_follows`]).
pub(crate) fn forget(node: &Node, space: &SpaceId) {
    node.sync.channels.tips.lock().remove(space);
    let old = node.sync.channels.subscribed.lock().remove(space);
    for t in old.unwrap_or_default() {
        node.net.unsubscribe(t);
    }
    for t in all_topics(space) {
        node.net.set_allowed_peers(t, Vec::new());
    }
}

/// 공간을 지울 때 연 채널 기록도 지웁니다.
pub(crate) fn forget_follows(node: &Node, space: &SpaceId) {
    if let Err(e) = node.store.delete_setting(&follow_key(space)) {
        warn!(error = %e, "따라가는 채널 기록을 지우지 못함");
    }
}

/// 이 이벤트를 올릴 묶음 토픽(쓰는 중이고 모두 보는 일반 채널 이벤트일 때).
pub(crate) fn bucket_topic(
    node: &Node,
    space: &SpaceId,
    state: &SpaceState,
    index: &StateIndex,
    ev: &SignedEvent,
) -> Option<GossipTopic> {
    if !in_use(node, index) {
        return None;
    }
    event_bucket(space, state, index, ev)
}

/// 이 이벤트가 실릴 수 있는 묶음 토픽(모두 보는 일반 채널 이벤트). 받은 것을 검사할 때는 이 노드가 켰는지·크루 크기를
/// 어떻게 보는지와 상관없이 이것만 봅니다.
pub(crate) fn event_bucket(
    space: &SpaceId,
    state: &SpaceState,
    index: &StateIndex,
    ev: &SignedEvent,
) -> Option<GossipTopic> {
    let StreamId::Channel(c) = ev.header.stream() else {
        return None;
    };
    bucket_channel(index, state, &c).then(|| GossipTopic::Bucket(*space, channel_bucket(space, &c)))
}

/// 알림 토픽에 실을 메시지인지: 모두 보는 채널의 메시지, 멘션·답장·모두 멘션(`@everyone`·`@here`)이 있음, 멘션이
/// 많거나 모두 멘션이면 `MENTION_MANY`. 본문은 받은(또는 쓴) 뒤 저장된 메시지 행에서 봅니다(아직 열지 못했으면 싣지 않음).
pub(crate) fn notify_worthy(
    node: &Node,
    state: &SpaceState,
    index: &StateIndex,
    ev: &SignedEvent,
) -> bool {
    if ev.header.kind != EventKind::Message {
        return false;
    }
    let StreamId::Channel(c) = ev.header.stream() else {
        return false;
    };
    if !bucket_channel(index, state, &c) {
        return false;
    }
    let Ok(Some(rec)) = node.store.get_message(&ev.id(), None) else {
        return false;
    };
    let Some(content) = rec.content else {
        return false;
    };
    let m = &content.body;
    let broadcast = crate::messages::broadcast_mention(&m.text).is_some();
    if m.mentions.is_empty() && m.reply_to.is_none() && !broadcast {
        return false;
    }
    (m.mentions.len() <= PLAIN_MENTIONS && !broadcast)
        || state
            .channel_permissions(&ev.header.user, &c)
            .contains(Permissions::MENTION_MANY)
}

/// 알림 토픽으로 받은 이벤트를 더 퍼뜨려도 되는지(작성 기기별 버킷).
pub(crate) fn notify_admit(node: &Node, space: SpaceId, ev: &SignedEvent) -> bool {
    let mut l = node.sync.channels.notify_limit.lock();
    let now = l.now_ms();
    l.try_take(
        space,
        EventAuthor::Device(ev.header.author),
        EventClass::Base,
        now,
    )
}

/// 진단·시험용 수(묶음으로 올림, 묶음 대신 크루로 올림, 알림으로 올림).
pub(crate) fn sent_counts(node: &Node) -> (u64, u64, u64) {
    let c = &node.sync.channels;
    (
        c.bucket_sent.load(Ordering::Relaxed),
        c.bucket_fallback.load(Ordering::Relaxed),
        c.notify_sent.load(Ordering::Relaxed),
    )
}

/// 지금 구독한 묶음 번호(시험용).
pub(crate) fn subscribed_buckets(node: &Node, space: &SpaceId) -> Vec<u8> {
    node.sync
        .channels
        .subscribed
        .lock()
        .get(space)
        .map(|s| {
            s.iter()
                .filter_map(|t| match t {
                    GossipTopic::Bucket(_, b) => Some(*b),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

// ───────────── 안 읽음 힌트(채널 끝 묻기) ─────────────

/// 따라가지 않는 이 채널에 이 기기가 아직 받지 않은 새 글이 있는지(멤버가 알려 준 끝이 가진 것보다 늦음).
pub(crate) fn has_new(node: &Node, space: &SpaceId, channel: &ChannelId) -> bool {
    let Some(tip) = node
        .sync
        .channels
        .tips
        .lock()
        .get(space)
        .and_then(|t| t.get(channel).copied())
    else {
        return false;
    };
    match node.store.latest_in_channel(space, channel) {
        Ok(local) => local.is_none_or(|(l, _)| l < tip),
        Err(_) => false,
    }
}

/// 따라가지 않는, 내가 볼 수 있는 모두 보는 채널(끝을 물을 채널).
fn unfollowed_channels(
    node: &Node,
    space: &SpaceId,
    state: &SpaceState,
    index: &StateIndex,
    me: &UserId,
) -> Vec<ChannelId> {
    let now = Timestamp::now().0;
    let follows = load_follows(node, space);
    state
        .channels
        .keys()
        .filter(|c| {
            bucket_channel(index, state, c)
                && !follows
                    .get(*c)
                    .is_some_and(|t| now.saturating_sub(*t) < FOLLOW_MS)
                && state
                    .channel_permissions(me, c)
                    .contains(Permissions::VIEW_CHANNEL | Permissions::READ_HISTORY)
        })
        .take(MAX_TIP_CHANNELS)
        .copied()
        .collect()
}

/// 다시 걸기 주기(`peers::spawn_redial`)마다 끝 묻기를 띄웁니다. 앞의 것이 아직 돌면 건너뜁니다.
pub(crate) fn spawn_poll_tips(node: &Arc<Node>) {
    if !enabled(node) || node.sync.channels.polling.swap(true, Ordering::AcqRel) {
        return;
    }
    let node2 = Arc::clone(node);
    node.tasks.spawn(async move {
        poll_tips(&node2).await;
        node2.sync.channels.polling.store(false, Ordering::Release);
    });
}

/// 채널 토픽을 쓰는 공간마다 연결된 멤버 기기 한 곳(무작위)에 따라가지 않는 채널의 끝을 묻고, 새 글이 생긴 채널이
/// 있으면 `space.updated`를 냅니다. 답하는 멤버도 그 채널을 따라가지 않으면 늦을 수 있어 매번 다른 곳에 묻고 큰 값을 씁니다.
pub(crate) async fn poll_tips(node: &Arc<Node>) {
    if !enabled(node) {
        return;
    }
    let Ok(me) = node.me() else { return };
    for space in node.space_ids() {
        if crate::policy::is_dormant(node, &space) {
            continue;
        }
        let Some(state) = node.space_state(&space) else {
            continue;
        };
        let index = node.sync.index.get(&space, &state);
        if !in_use(node, &index) {
            node.sync.channels.tips.lock().remove(&space);
            continue;
        }
        let channels = unfollowed_channels(node, &space, &state, &index, &me.user());
        if channels.is_empty() {
            node.sync.channels.tips.lock().remove(&space);
            continue;
        }
        let peers: Vec<PeerId> = node
            .peers
            .authed_peers()
            .into_iter()
            .filter(|(_, u, d)| state.is_member(u) && state.device_belongs(u, d))
            .map(|(p, _, _)| p)
            .collect();
        let Some(peer) = peers.choose(&mut rand::thread_rng()).copied() else {
            continue;
        };
        let asked: BTreeSet<ChannelId> = channels.iter().copied().collect();
        let req = YpRequest::GetChannelTips { space, channels };
        let Ok(YpResponse::ChannelTips(list)) = crate::p2p::request(node, peer, req).await else {
            continue;
        };
        let before: BTreeSet<ChannelId> = asked
            .iter()
            .filter(|c| has_new(node, &space, c))
            .copied()
            .collect();
        {
            let mut tips = node.sync.channels.tips.lock();
            let t = tips.entry(space).or_default();
            t.retain(|c, _| asked.contains(c));
            for (c, l) in list {
                if asked.contains(&c) {
                    let e = t.entry(c).or_insert(l);
                    *e = (*e).max(l);
                }
            }
        }
        let grew = asked
            .iter()
            .any(|c| !before.contains(c) && has_new(node, &space, c));
        if grew {
            node.notify.publish(Notification::SpaceUpdated { space });
        }
    }
}

/// `GetChannelTips`에 답합니다. 따라잡기의 `GetHeads`와 같은 검사(멤버십·시더 정책·올리기 제한·길)를 채널마다 하고
/// (그래서 보통 노드는 열어 본 채널만 답함),
/// 모두 보는 일반 채널 중 요청자가 기록을 볼 수 있는 것만 싣습니다. 걸러진 채널은 빼기만 해 무엇이 있는지 드러내지 않습니다.
pub(crate) fn handle_get_tips(
    node: &Node,
    peer: PeerId,
    space: SpaceId,
    channels: Vec<ChannelId>,
) -> YpResponse {
    if channels.len() > MAX_TIP_CHANNELS {
        return YpResponse::error(WireErrorCode::BadRequest, "채널이 너무 많음");
    }
    let (state, user, device) = match crate::sync_service::requester(node, &peer, &space) {
        Ok(x) => x,
        Err(r) => return r,
    };
    if !state.is_member(&user) || !state.device_belongs(&user, &device) {
        return YpResponse::error(WireErrorCode::NotMember, "멤버가 아님");
    }
    let index = node.sync.index.get(&space, &state);
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    for c in channels {
        if !seen.insert(c)
            || !bucket_channel(&index, &state, &c)
            || !state
                .channel_permissions(&user, &c)
                .contains(Permissions::VIEW_CHANNEL | Permissions::READ_HISTORY)
        {
            continue;
        }
        match crate::sync_service::check_meta(
            node,
            &peer,
            &space,
            &state,
            user,
            device,
            &StreamId::Channel(c),
            40,
        ) {
            Ok(()) => {}
            Err(r @ YpResponse::Busy { .. }) => return r,
            Err(_) => continue,
        }
        if let Ok(Some((lamport, _))) = node.store.latest_in_channel(&space, &c) {
            out.push((c, lamport));
        }
    }
    YpResponse::ChannelTips(out)
}
