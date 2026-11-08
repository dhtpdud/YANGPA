//! 전파와 따라잡기, 그리고 남의 동기화 요청에 답하기(시더).
//!
//! - 전파: 공간의 활성 기기가 8대(`NodeConfig::push_max_devices`) 이하면 멤버 노드마다 `PushEvents`, 많으면 크루 gossip
//!   토픽. 볼 수 있는 사람이 정해진 채널의 이벤트는 볼 수 있는 기기가 32대(`group_topic_min`) 이하면 그 기기들에게 직접,
//!   많으면 그 키 그룹의 gossip 토픽([`crate::topics`]). gossip으로 보내지 못할 때는 연결된 곳 먼저 8곳
//!   (`NOPEERS_FANOUT`)까지만 직접 보내고 나머지는 따라잡기에 맡깁니다.
//! - 따라잡기(연결·인증 직후와 주기적으로, 받은 이벤트의 빈 부모를 채울 때): 스트림마다 `GetHeads` → 모르는 머리부터
//!   `GetEvents`로 부모를 따라가고 → 지문 비교(`yangpa_sync::reconcile`)로 나머지를 찾습니다. 공간 스트림(상태·키 갱신·키 묶음)을
//!   먼저 맞춥니다. 빈 부모 채우기는 작성 기기별 제한을 탑니다([`Pull`]).
//! - 시더: `GetHeads`·`GetFingerprints`·`GetIds`·`GetEvents`는 `policy::check_seed_request`를 통과해야 답합니다.
//! - 부분 전달 스트림(받은편지함·키 그룹)은 gossip에 싣지 않고 받을 자격이 있는 기기에게만 직접 보냅니다
//!   ([`deliver_scoped`]). 시더는 자격 없는 기기의 요청에 빈 스트림과 똑같이 답합니다(`policy::may_read_scoped`).
//! - P2P 전용 채널의 채널 스트림은 P2P 스웜으로만 전파·따라잡기·응답하고, 나머지는 Tor로만 합니다(`crate::p2p`).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;
use tracing::debug;
use yangpa_core::event::{Lane, Scope, SignedEvent, StreamId};
use yangpa_core::ids::{ChannelId, DeviceId, EventId, SpaceId, UserId};
use yangpa_core::model::{Permissions, SpaceState};
use yangpa_core::time::Timestamp;
use yangpa_core::wire::{
    GossipMessage, GossipTopic, LamportRange, MAX_EVENTS_PER_MESSAGE, RangeFingerprint,
    WireErrorCode, YpRequest, YpResponse,
};
use yangpa_net::{GossipVerdict, MessageId, Multiaddr, PeerId};
use yangpa_store::StoreError;
use yangpa_sync::policy::{
    DataType, SeedRefusal, SeedRequest, TokenBucket, check_seed_request, may_read_scoped,
};
use yangpa_sync::reconcile::{
    FingerprintSource, MemorySet, ReconcileRequest, ReconcileResponse, Reconciler,
    answer_fingerprints, answer_ids, full_range,
};

use crate::error::{Error, Result};
use crate::ingest::{self, Source};
use crate::node::Node;
use crate::state_index::{ChannelView, StateIndex};
use crate::{checkpoint, crews, keygroup, locator, peers, policy};

/// 부모 따라가기 최대 단계(넘으면 지문 비교로).
const CHASE_ROUNDS: usize = 8;
/// 확정 기기와 따라잡을 때 전달자로서 받은편지함을 당겨 올 사용자: 이만큼 지난 가입·기기 목록 갱신까지, 최근 것부터
/// 이만큼. 가입자·연결한 기기는 처음 며칠 안에 그 묶음을 받아 가므로 오래된 것까지 매번 맞추지 않습니다.
const RELAY_PULL_WINDOW_MS: u64 = 24 * 60 * 60 * 1000;
const RELAY_PULL_USERS: usize = 4;

/// 따라잡기로 받은 묶음을 이만큼 모았다가 한 번에 적용합니다([`PullBuffer`]).
const PULL_FLUSH_BATCHES: usize = 4;

/// 모은 묶음을 이보다 오래 두지 않습니다(느린 피어에게서 받을 때도 상태가 너무 늦지 않게).
const PULL_FLUSH_EVERY: std::time::Duration = std::time::Duration::from_secs(2);

/// 모아 둔 이벤트가 대략 이만큼(본문 + 서명·머리) 되면 묶음 수와 무관하게 적용합니다. 묶음 하나가 메시지 한도(8MB)까지
/// 될 수 있어, 묶음 수로만 끊으면 따라잡기 하나가 32MB 넘게 들고 있게 됩니다.
const PULL_FLUSH_BYTES: usize = 8 * 1024 * 1024;

/// 이벤트 하나의 본문 밖 크기 어림(혼합 서명 1,659B + 머리).
const PULL_EVENT_OVERHEAD: usize = 2 * 1024;

/// 따라잡기 종류.
///
/// 실시간으로 받은 이벤트의 빈 부모·보류를 곧바로 채우는 따라잡기(`Gap`)는 상태·키 갱신 이벤트에 작성 기기별 속도
/// 제한(`ratelimit::EventLimiter`)을 실시간 전파와 같은 버킷으로 겁니다. 그러지 않으면 제한에 걸려 버린 이벤트(또는
/// 아예 실시간으로 보내지 않은 이벤트)를 자식 이벤트 하나로 모두 끌어와 제한이 뚫립니다. 걸리면 그 스트림은 거기서
/// 멈추고, 나머지는 제한 없는 따라잡기(`Full`: 연결·인증 직후, `sync_interval`마다, 그 밖)가 묶음으로 가져옵니다.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Pull {
    Gap,
    Full,
}

/// 동기화 영역의 노드 수준 상태.
#[derive(Default)]
pub(crate) struct SyncState {
    /// 진행 중인 (공간, 피어) 따라잡기와, 끝난 뒤 다시 돌릴 종류(없으면 그대로 끝).
    running: Mutex<HashMap<(SpaceId, PeerId), Option<Pull>>>,
    /// 시더 업로드 대역폭(블롭 제공도 같이 씀).
    pub(crate) bucket: Mutex<Option<TokenBucket>>,
    /// 처리 중인 시더 요청을 보낸 기기마다의 요청 수(블롭 제공도 같이 셈, [`serving`]).
    serving: Mutex<HashMap<DeviceId, u32>>,
    /// 상태 색인(활성 기기 수·목록, 기기 → 사용자, 채널을 누가 볼 수 있는지). 상태 사본마다 한 번 계산합니다.
    pub(crate) index: crate::state_index::StateIndexes,
    /// 받을 자격도 전달할 일도 없는데 받은 남의 부분 전달 이벤트(남의 받은편지함, 볼 수 없는 키 그룹)의 바이트 수
    /// (진단용: 정상 피어는 보내지 않으므로 0이어야 함, [`note_foreign_scoped`]).
    pub(crate) foreign_scoped_bytes: AtomicU64,
    /// 구독한 그룹 토픽.
    pub(crate) groups: crate::topics::GroupTopics,
    /// 채널 묶음·알림 토픽(선택 기능).
    pub(crate) channels: crate::chantopics::ChannelTopics,
}

/// 남의 받은편지함 이벤트를 받았으면 그 크기를 셉니다. 전달자로서 받을 것은 부르는 쪽이 미리 거릅니다.
pub(crate) fn note_foreign_scoped(node: &Node, ev: &SignedEvent) {
    let foreign = match ev.header.scope {
        Some(Scope::Inbox(u)) => node.me().is_ok_and(|me| me.user() != u),
        // 부르는 쪽이 이 기기가 받을 것이 아니라고 본 것(gossip에 실렸거나 볼 수 없는 그룹).
        Some(Scope::KeyGroup(_)) => true,
        None => false,
    };
    if foreign {
        let size = postcard::to_stdvec(ev).map_or(0, |b| b.len() as u64);
        node.sync
            .foreign_scoped_bytes
            .fetch_add(size, Ordering::Relaxed);
    }
}

/// 시더 요청 하나를 처리하는 동안 잡아 두는 표. 동시 전송 수는 기기 단위로 셉니다: 받는 쪽은 피어마다 겹쳐 보내는
/// 요청 수를 이미 묶으므로, 한 기기가 겹쳐 보낸 조각 요청끼리 서로를 바쁨으로 만들면 빠른 연결일수록(요청이 한꺼번에
/// 닿아) 오히려 느려집니다.
pub(crate) struct Serving<'a> {
    node: &'a Node,
    device: DeviceId,
    /// 지금 함께 처리 중인 다른 기기 수(`SeedRequest::in_flight`).
    pub others: u32,
}

pub(crate) fn serving(node: &Node, device: DeviceId) -> Serving<'_> {
    let mut map = node.sync.serving.lock();
    *map.entry(device).or_default() += 1;
    let others = (map.len() - 1) as u32;
    Serving {
        node,
        device,
        others,
    }
}

impl Drop for Serving<'_> {
    fn drop(&mut self) {
        let mut map = self.node.sync.serving.lock();
        if let Some(n) = map.get_mut(&self.device) {
            *n -= 1;
            if *n == 0 {
                map.remove(&self.device);
            }
        }
    }
}

// ───────────── 전파 ─────────────

/// 직접 보낼 곳이 많을 때 고르는 기기 수: gossip 메시가 비어 `NoPeers`일 때 대신 보낼 곳, 그리고 일부만 볼 수 있는
/// 채널을 볼 수 있는 기기가 [`DIRECT_VIEWERS_MAX`]보다 많을 때 보낼 곳. 4.5만 대 크루에서 모두에게 다이얼하면 Tor 회로
/// 폭주가 되므로 몇 곳에만 보내고, 나머지는 그 기기들과의 따라잡기(연결·인증 직후와 `sync_interval`마다, 받은 쪽이 빈
/// 부모를 채울 때)가 채웁니다.
pub(crate) const NOPEERS_FANOUT: usize = 8;

/// 그룹 토픽이 없는 키 그룹(구독 전·휴면)의 키 그룹 스트림 이벤트는 그 그룹 채널을 볼 수 있는 기기(이 기기 빼고)가 이 수
/// 이하면 모두에게 직접 보냅니다. 볼 수 없는 멤버는 그 스트림을 갖고 있지도 내주지도 않아, 연결된 몇 곳에만 보내면
/// 나머지가 굶습니다. 채널 상태 색인의 목록 상한이기도 합니다([`channel_view`]). 채널 이벤트는 `group_topic_min`을 넘으면
/// 그룹 토픽으로 갑니다.
pub(crate) const DIRECT_VIEWERS_MAX: usize = 256;

/// gossip을 쓰는 크루에서 `NoPeers`일 때, 받을 기기가 이 수 이하면 줄이지 않고 모두에게 직접 보냅니다.
fn direct_limit(node: &Node) -> usize {
    node.cfg.push_max_devices.max(NOPEERS_FANOUT)
}

/// 채널을 누가 볼 수 있는지(상태 색인, 사본마다 한 번). 목록은 [`DIRECT_VIEWERS_MAX`]에 이 기기 몫 하나를 더한 수까지
/// 담습니다.
pub(crate) fn channel_view(index: &StateIndex, channel: &ChannelId) -> Arc<ChannelView> {
    index.channel(channel, DIRECT_VIEWERS_MAX + 1)
}

/// 공간 멤버들에게 이벤트를 보냅니다(백그라운드).
///
/// 활성 기기가 `cfg.push_max_devices` 이하면 기기마다 `PushEvents`, 넘으면 공간 gossip 토픽. 볼 수 있는 사람이
/// 정해진 채널(일부 멤버에게 `VIEW_CHANNEL`이 없음)의 이벤트는 gossip이 공간 전체로 퍼지므로, 볼 수 있는 멤버의
/// 기기에게만 직접 보냅니다. 직접 보낼 곳이 너무 많으면 [`NOPEERS_FANOUT`]곳까지만 고르고([`pick_targets`]) 나머지는
/// 따라잡기에 맡깁니다.
pub(crate) fn propagate(node: &Arc<Node>, space: SpaceId, events: Vec<SignedEvent>) {
    if events.is_empty() {
        return;
    }
    // P2P 전용 채널의 채널 스트림은 P2P 스웜으로만(gossip·보관 노드·Tor 직접 전송에 싣지 않음).
    let (direct, events) = crate::p2p::split(node, &space, events);
    if !direct.is_empty() {
        crate::p2p::push(node, space, direct);
    }
    if events.is_empty() {
        return;
    }
    // 보관 노드에는 내가 쓴 이벤트를 바로 보냅니다(보관 범위만). 보관 노드는 멤버가 아니라 아래 대상에 들지 않습니다.
    crate::archive::forward(node, space, &events);
    // 부분 전달 이벤트는 받을 자격이 있는 기기에게만 직접 보냅니다(gossip은 공간 전체로 퍼짐).
    let (scoped, events): (Vec<SignedEvent>, Vec<SignedEvent>) = events
        .into_iter()
        .partition(|e| e.header.stream().is_scoped());
    if !scoped.is_empty() {
        deliver_scoped(node, space, scoped);
    }
    if events.is_empty() {
        return;
    }
    let Some(state) = node.space_state(&space) else {
        return;
    };
    let Ok(me) = node.me() else { return };
    let (me_user, me) = (me.user(), me.device_id());
    let me_active = state.is_member(&me_user) && state.device_belongs(&me_user, &me);
    let index = node.sync.index.get(&space, &state);
    // 채널마다 볼 수 있는지는 이 사본(`state`)의 색인 값만 씁니다(`state_index`): 권한을 막은 새 상태가 적용돼도
    // 옛 사본의 '모두 볼 수 있음'이 새 사본에 쓰이는 일은 없습니다.
    let mut open = Vec::new();
    let mut restricted: BTreeMap<ChannelId, Vec<SignedEvent>> = BTreeMap::new();
    for ev in events {
        match ev.header.channel {
            Some(c) if !channel_view(&index, &c).everyone => {
                restricted.entry(c).or_default().push(ev)
            }
            _ => open.push(ev),
        }
    }
    if !open.is_empty() {
        let others = index.active_count() - usize::from(me_active);
        if others + 1 > node.cfg.push_max_devices {
            let limit = direct_limit(node);
            let state = state.clone();
            let index = index.clone();
            // 채널 토픽을 쓰면 모두 보는 채널 이벤트는 그 묶음 토픽으로, 멘션·답장은 알림 토픽에도(`chantopics`).
            let mut routes: BTreeMap<GossipTopic, Vec<SignedEvent>> = BTreeMap::new();
            let mut notify = Vec::new();
            for ev in open {
                if crate::chantopics::notify_worthy(node, &state, &index, &ev) {
                    notify.push(ev.clone());
                }
                let t = crate::chantopics::bucket_topic(node, &space, &state, &index, &ev)
                    .unwrap_or(GossipTopic::Crew(space));
                routes.entry(t).or_default().push(ev);
            }
            node.spawn_detached(move |node| async move {
                for chunk in gossip_chunks(notify) {
                    let msg = GossipMessage::Events {
                        space,
                        events: chunk,
                    };
                    // 알림 토픽에 받을 피어가 없으면 그만둡니다(같은 이벤트가 묶음·크루 토픽으로도 감).
                    if node
                        .net
                        .try_publish(GossipTopic::Notify(space), msg)
                        .await
                        .is_ok()
                    {
                        node.sync
                            .channels
                            .notify_sent
                            .fetch_add(1, Ordering::Relaxed);
                    }
                }
                for (chunk, topic) in routes
                    .into_iter()
                    .flat_map(|(t, evs)| gossip_chunks(evs).into_iter().map(move |c| (c, t)))
                {
                    let salt = chunk[0].id();
                    let msg = GossipMessage::Events {
                        space,
                        events: chunk.clone(),
                    };
                    let mut sent = node.net.try_publish(topic, msg.clone()).await;
                    if matches!(topic, GossipTopic::Bucket(..)) {
                        let c = &node.sync.channels;
                        if let Err(yangpa_net::NetError::NoPeers) = sent {
                            // 그 묶음을 구독한 연결 피어가 없음: 크루 토픽으로(끈 노드도 받음).
                            c.bucket_fallback.fetch_add(1, Ordering::Relaxed);
                            sent = node.net.try_publish(GossipTopic::Crew(space), msg).await;
                        } else {
                            c.bucket_sent.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    // 토픽에 받을 피어가 아직 없으면(막 시작했거나 메시가 비었음) 직접 보냅니다. 큰 크루면 연결된 멤버
                    // 먼저 8곳까지만 보내고 나머지는 따라잡기로 받습니다(보관 노드는 위 `archive::forward`가 이미 보냄).
                    if let Err(yangpa_net::NetError::NoPeers) = sent {
                        let targets = if others <= limit {
                            index
                                .devices()
                                .iter()
                                .filter(|(_, d)| *d != me)
                                .copied()
                                .collect()
                        } else {
                            let live = connected_targets(
                                &state,
                                me,
                                &node.peers.authed_peers(),
                                &salt,
                                |_| true,
                            );
                            let pool = index.devices();
                            pick_targets(&pool, |_| true, live, NOPEERS_FANOUT, me, &salt)
                        };
                        push_to(&node, space, targets, chunk);
                    }
                }
            });
        } else {
            let all = index
                .devices()
                .iter()
                .filter(|(_, d)| *d != me)
                .copied()
                .collect();
            push_to(node, space, all, open);
        }
    }
    for (c, evs) in restricted {
        let view = channel_view(&index, &c);
        // 볼 수 있는 기기가 많으면 그 그룹 토픽으로 올립니다(작성자 업로드가 메시 크기로 묶임).
        if view.viewer_devices > node.cfg.group_topic_min
            && let Some(topic) = crate::topics::channel_topic(node, &space, &state, &c)
        {
            let to = GroupSend {
                topic,
                state: state.clone(),
                index: index.clone(),
                view,
                channel: c,
                me,
            };
            publish_group(node, to, evs);
            continue;
        }
        let targets = restricted_targets(
            &state,
            me,
            &view,
            &c,
            node.cfg.group_topic_min,
            &node.peers.authed_peers(),
            &evs[0].id(),
            || index.devices(),
        );
        push_to(node, space, targets, evs);
    }
}

/// 그룹 토픽으로 보낼 곳과, 받을 피어가 없을 때 직접 보낼 곳을 고를 재료.
struct GroupSend {
    topic: GossipTopic,
    state: Arc<SpaceState>,
    index: Arc<StateIndex>,
    /// 그룹 채널 하나의 열람(같은 그룹의 채널은 볼 수 있는 사람이 같음).
    view: Arc<ChannelView>,
    channel: ChannelId,
    me: DeviceId,
}

/// 그룹 토픽으로 올립니다. 받을 피어가 없거나 보내지 못하면 그 그룹 채널을 볼 수 있는 기기 몇 곳([`NOPEERS_FANOUT`])에만
/// 직접 보내고 나머지는 따라잡기에 맡깁니다(모두에게 다이얼하지 않음).
fn publish_group(node: &Arc<Node>, to: GroupSend, events: Vec<SignedEvent>) {
    node.spawn_detached(move |node| async move {
        let space = to.topic.space();
        for chunk in gossip_chunks(events) {
            let salt = chunk[0].id();
            let msg = GossipMessage::Events {
                space,
                events: chunk.clone(),
            };
            if let Err(e) = node.net.try_publish(to.topic, msg).await {
                debug!(error = %e, "그룹 토픽으로 보내지 못해 몇 곳에 직접 보냄");
                let targets = restricted_targets(
                    &to.state,
                    to.me,
                    &to.view,
                    &to.channel,
                    0,
                    &node.peers.authed_peers(),
                    &salt,
                    || to.index.devices(),
                );
                push_to(&node, space, targets, chunk);
            }
        }
    });
}

/// 부분 전달 이벤트를 받을 자격이 있는 기기에게 직접 보냅니다. 받은편지함 u는 u의 활성 기기와, 내가 쓴 것이면 u의
/// 전달자(`crews::InboxRelay`: 가입자·연결한 기기는 처음에 그 기기만 믿어 그쪽이 넘겨줌)에게. 키 그룹은 그 그룹 채널을 볼
/// 수 있는 멤버의 기기에게(많으면 일부만, 나머지는 따라잡기). 보관 노드에는 [`propagate`]가 `archive::forward`로 보냅니다.
pub(crate) fn deliver_scoped(node: &Arc<Node>, space: SpaceId, events: Vec<SignedEvent>) {
    let Some(state) = node.space_state(&space) else {
        return;
    };
    let Ok(me) = node.me() else { return };
    let me = me.device_id();
    let mut by_stream: BTreeMap<StreamId, Vec<SignedEvent>> = BTreeMap::new();
    for ev in events {
        by_stream.entry(ev.header.stream()).or_default().push(ev);
    }
    for (stream, evs) in by_stream {
        let targets: Vec<(UserId, DeviceId)> = match stream {
            StreamId::Inbox(u) => {
                let mut t: Vec<(UserId, DeviceId)> = state
                    .devices
                    .get(&u)
                    .filter(|_| state.is_member(&u))
                    .map(|l| l.active().map(|d| (u, d.device)).collect())
                    .unwrap_or_default();
                if evs.iter().any(|e| e.header.author == me) {
                    for relay in crews::inbox_relays(node, &space, &u) {
                        if state.is_member(&relay.user)
                            && state.device_belongs(&relay.user, &relay.device)
                        {
                            t.push((relay.user, relay.device));
                        }
                    }
                }
                t
            }
            StreamId::KeyGroup(g) => {
                // 같은 그룹의 채널은 열람 규칙이 같아 볼 수 있는 사람도 같습니다(`ChannelSettings::view_rule`). 그래서
                // 그룹 채널 하나의 상태 색인 값을 씁니다(사본마다 한 번 계산: 3만 명에서 이벤트마다 멤버 수만큼 권한을
                // 다시 세지 않게). 그 채널이 없으면 받을 기기도 없습니다.
                let Some(c) = state
                    .channels
                    .iter()
                    .find(|(_, ch)| state.channel_key_group(&space, ch).as_ref() == Some(&g))
                    .map(|(c, _)| *c)
                else {
                    continue;
                };
                let index = node.sync.index.get(&space, &state);
                let view = channel_view(&index, &c);
                if !view.everyone
                    && view.viewer_devices > node.cfg.group_topic_min
                    && crate::topics::subscribed(node, &space, &g)
                {
                    let to = GroupSend {
                        topic: GossipTopic::Group(space, g),
                        state: state.clone(),
                        index,
                        view,
                        channel: c,
                        me,
                    };
                    publish_group(node, to, evs);
                    continue;
                }
                let salt = evs[0].id();
                let connected = node.peers.authed_peers();
                if view.everyone && view.few.is_none() {
                    // 모두 볼 수 있는데 많으면 목록·비트가 없으므로 활성 기기 전체에서 고릅니다.
                    let live = connected_targets(&state, me, &connected, &salt, |_| true);
                    pick_targets(
                        &index.devices(),
                        |_| true,
                        live,
                        NOPEERS_FANOUT / 2,
                        me,
                        &salt,
                    )
                } else {
                    restricted_targets(
                        &state,
                        me,
                        &view,
                        &c,
                        DIRECT_VIEWERS_MAX,
                        &connected,
                        &salt,
                        || index.devices(),
                    )
                }
            }
            StreamId::Space | StreamId::Channel(_) => continue,
        };
        let mut targets: Vec<(UserId, DeviceId)> =
            targets.into_iter().filter(|(_, d)| *d != me).collect();
        targets.sort_unstable();
        targets.dedup();
        if !targets.is_empty() {
            push_to(node, space, targets, evs);
        }
    }
}

/// 일부만 볼 수 있는 채널의 이벤트를 직접 보낼 기기. 볼 수 있는 기기(이 기기 빼고)가 `direct_max` 이하면 모두, 많으면
/// [`NOPEERS_FANOUT`]곳: 연결된 기기에서 절반까지, 나머지는 볼 수 있는 기기 전체에서 이벤트마다 다른 자리로
/// 골라(연결이 없는 기기도 언젠가 직접 받고, 받으면 빈 부모를 보낸 쪽에서 따라잡음). 볼 수 없는 멤버의 기기는 어느
/// 쪽에서도 고르지 않습니다(같은 사본의 `view`와 권한 계산만 씀). `devices`는 같은 사본의 [`StateIndex::devices`]
/// (`view.mask`의 자리 순서)이고 많을 때만 부릅니다.
///
/// [`StateIndex::devices`]: crate::state_index::StateIndex::devices
#[allow(clippy::too_many_arguments)]
fn restricted_targets(
    state: &SpaceState,
    me: DeviceId,
    view: &ChannelView,
    channel: &ChannelId,
    direct_max: usize,
    connected: &[(PeerId, UserId, DeviceId)],
    salt: &EventId,
    devices: impl FnOnce() -> crate::state_index::Devices,
) -> Vec<(UserId, DeviceId)> {
    let viewer = |u: &UserId| {
        state
            .channel_permissions(u, channel)
            .contains(Permissions::VIEW_CHANNEL)
    };
    if let Some(few) = &view.few {
        let others: Vec<(UserId, DeviceId)> =
            few.iter().filter(|(_, d)| *d != me).copied().collect();
        if others.len() <= direct_max {
            return others;
        }
        let live = connected_targets(state, me, connected, salt, viewer);
        return pick_targets(&others, |_| true, live, NOPEERS_FANOUT / 2, me, salt);
    }
    if view.mask.is_none() {
        // 모두 볼 수 있는 채널은 이 길로 오지 않습니다.
        return Vec::new();
    }
    let live = connected_targets(state, me, connected, salt, viewer);
    let pool = devices();
    pick_targets(
        &pool,
        |i| view.mask_allows(i),
        live,
        NOPEERS_FANOUT / 2,
        me,
        salt,
    )
}

/// 지금 인증된 연결이 있는 멤버 기기 중 `eligible`인 사용자의 것(나 빼고), 이벤트마다 다른 랑데부 해시 순서. 연결
/// 수만큼만 돌므로 크루 크기와 무관합니다.
fn connected_targets(
    state: &SpaceState,
    me: DeviceId,
    connected: &[(PeerId, UserId, DeviceId)],
    salt: &EventId,
    eligible: impl Fn(&UserId) -> bool,
) -> Vec<(UserId, DeviceId)> {
    let rank = |d: &DeviceId| {
        let mut h = blake3::Hasher::new();
        h.update(b"yangpa/v1/push-pick\0");
        h.update(&salt.0);
        h.update(d.as_bytes());
        *h.finalize().as_bytes()
    };
    let mut live: Vec<([u8; 32], UserId, DeviceId)> = connected
        .iter()
        .filter(|(_, u, d)| {
            *d != me && state.is_member(u) && state.device_belongs(u, d) && eligible(u)
        })
        .map(|(_, u, d)| (rank(d), *u, *d))
        .collect();
    live.sort_unstable_by_key(|a| a.0);
    // 한 기기가 두 연결(예전·새 PeerId)로 보일 수 있습니다.
    live.dedup_by(|a, b| a.2 == b.2);
    live.into_iter().map(|(_, u, d)| (u, d)).collect()
}

/// 직접 보낼 최대 [`NOPEERS_FANOUT`]곳: `live`(연결된 곳, 고른 순서)에서 `live_max`곳까지 먼저, 모자라면 `pool`에서
/// `eligible`인 자리를 이벤트 ID로 정한 위치부터 찾아 채웁니다(다이얼은 이 몇 곳만). 한 곳에 O(볼 수 있는 기기 사이
/// 간격)이라 4.5만 대 전체를 정렬하지 않습니다. 같은 기기를 두 번 고르지 않고 `me`는 빼며, 모든 기기가 뽑힐 수
/// 있습니다(고르게는 아님: 볼 수 없는 기기가 길게 이어진 바로 뒤 자리가 조금 더 자주 뽑힘).
fn pick_targets(
    pool: &[(UserId, DeviceId)],
    eligible: impl Fn(usize) -> bool,
    live: Vec<(UserId, DeviceId)>,
    live_max: usize,
    me: DeviceId,
    salt: &EventId,
) -> Vec<(UserId, DeviceId)> {
    let mut out: Vec<(UserId, DeviceId)> = live.into_iter().take(live_max).collect();
    let n = pool.len();
    let seed = u64::from_le_bytes(salt.0[..8].try_into().expect("8바이트"));
    let mut j = 0u64;
    while out.len() < NOPEERS_FANOUT && n > 0 {
        let start = (splitmix64(seed.wrapping_add(j)) % n as u64) as usize;
        j += 1;
        let found = (0..n).map(|k| (start + k) % n).find(|&i| {
            let d = pool[i].1;
            d != me && eligible(i) && !out.iter().any(|t| t.1 == d)
        });
        match found {
            Some(i) => out.push(pool[i]),
            None => break,
        }
    }
    out
}

/// 자리를 흩는 섞기(SplitMix64). 이벤트 ID가 이미 BLAKE3 해시라 암호 해시는 필요 없습니다.
fn splitmix64(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// gossip 메시지 하나에 담을 이벤트 묶음. 개수뿐 아니라 크기도 한도 안으로 나눕니다. 키 갱신 조각·키 묶음은 하나가
/// 수백 KiB일 수 있어, 개수로만 묶으면 메시지가 한도를 넘어 통째로 보내지지 않습니다.
fn gossip_chunks(events: Vec<SignedEvent>) -> Vec<Vec<SignedEvent>> {
    // 머리·부모 해시와 직렬화 몫을 넉넉히 잡고, 혼합 서명(공개키 포함 1.6KiB 남짓)은 따로 더합니다.
    const BUDGET: usize = yangpa_net::MAX_GOSSIP_MESSAGE - 32 * 1024;
    let size = |ev: &SignedEvent| {
        ev.body.as_ref().map_or(0, Vec::len)
            + 1024
            + yangpa_core::sign::Sig::LEN
            + 64 * ev.header.parents.len()
    };
    let mut out: Vec<Vec<SignedEvent>> = Vec::new();
    let mut used = 0usize;
    for ev in events {
        let sz = size(&ev);
        match out.last_mut() {
            Some(cur) if cur.len() < MAX_EVENTS_PER_MESSAGE && used + sz <= BUDGET => {
                used += sz;
                cur.push(ev);
            }
            _ => {
                used = sz;
                out.push(vec![ev]);
            }
        }
    }
    out
}

/// 기기마다 `PushEvents`(주소를 모르면 위치 기록에서 찾아 연결).
fn push_to(
    node: &Arc<Node>,
    space: SpaceId,
    devices: Vec<(UserId, DeviceId)>,
    events: Vec<SignedEvent>,
) {
    let events = Arc::new(events);
    for (user, device) in devices {
        let known = peers::peer_for_device(node, &device);
        let events = events.clone();
        node.spawn_detached(move |node| async move {
            // 주소를 모르는 기기는 위치 기록(DHT)·멤버 소개로 찾아 봅니다.
            let peer = match known {
                Some(p) => p,
                None => match peers::reach_found(&node, &user, &device).await {
                    Some(p) => p,
                    None => return,
                },
            };
            if let Err(e) = push(&node, space, peer, &events).await {
                // 기록된 주소가 낡았으면(상대가 다른 주소·PeerId로 다시 떴음) 위치 기록으로 찾아 한 번 더 보냅니다.
                if known.is_some()
                    && !node.peers.is_connected(&peer)
                    && let Some(p) = peers::reach_found(&node, &user, &device).await
                    && push(&node, space, p, &events).await.is_ok()
                {
                    locator::note_reroute(&node);
                    return;
                }
                debug!(peer = %peer, error = %e, "이벤트 직접 전송 실패(따라잡기로 받음)");
            }
        });
    }
}

async fn push(
    node: &Arc<Node>,
    space: SpaceId,
    peer: PeerId,
    events: &[SignedEvent],
) -> Result<()> {
    peers::ensure_auth(node, peer).await?;
    let mut reconfirmed = false;
    for chunk in events.chunks(MAX_EVENTS_PER_MESSAGE) {
        let req = YpRequest::PushEvents {
            space,
            events: chunk.to_vec(),
        };
        // 이 기기와 일반 내용을 나르는 빠른 연결이 있으면 그쪽으로 갑니다(`p2p::request`).
        let mut r = crate::p2p::request(node, peer, req.clone()).await?;
        // 이쪽만 상대를 인증된 피어로 아는 경우(`catch_up` 참고): 내 인증을 한 번 다시 보내고 받아 주면 다시 보냅니다.
        if !reconfirmed
            && matches!(
                r,
                YpResponse::Error {
                    code: WireErrorCode::Unauthenticated,
                    ..
                }
            )
        {
            reconfirmed = true;
            if peers::reconfirm_auth(node, peer).await {
                r = crate::p2p::request(node, peer, req).await?;
            }
        }
        match r {
            YpResponse::Ok => {}
            YpResponse::Error { code, .. } => {
                return Err(Error::Network(format!("상대가 거절함: {code:?}")));
            }
            _ => {}
        }
    }
    Ok(())
}

/// gossip 토픽을 구독하고 허용 피어(멤버 기기의 노드)를 갱신합니다.
pub(crate) fn refresh_space_peers(node: &Node, space: &SpaceId) {
    let Some(state) = node.space_state(space) else {
        return;
    };
    // 만료된 연락처와의 공간은 gossip을 받지도 보내지도 않습니다.
    if crate::friends::space_deactivated(node, space) {
        node.net.unsubscribe(GossipTopic::Crew(*space));
        node.net
            .set_allowed_peers(GossipTopic::Crew(*space), Vec::new());
        crate::topics::refresh(node, space, &state, false);
        crate::chantopics::refresh(node, space, &state, &[], false);
        return;
    }
    let allowed = member_peers(node, &state);
    // 휴면 공간은 gossip을 구독하지 않습니다(깨어날 때 다시 부름).
    let crew = GossipTopic::Crew(*space);
    let dormant = policy::is_marked_dormant(node, space);
    if dormant {
        node.net.unsubscribe(crew);
    } else {
        node.net.subscribe(crew);
    }
    crate::chantopics::refresh(node, space, &state, &allowed, !dormant);
    node.net.set_allowed_peers(crew, allowed);
    crate::topics::refresh(node, space, &state, !dormant);
}

/// 공간 멤버 기기의 노드: 기기의 인증된 연결, 없으면 그 기기가 묶인 노드 기록([`peers::peer_for_device`]와 같은 규칙).
///
/// 활성 기기마다 찾지 않고, 이 기기가 아는 노드(인증된 연결과 기기가 묶인 노드 기록)마다 그 기기가 멤버 기기인지
/// 봅니다. 4.5만 대 크루에서 기기마다 DB를 찾으면 상태가 바뀔 때마다(적용 잠금 안) 0.1초가 들지만, 아는 노드는 그보다
/// 훨씬 적습니다.
pub(crate) fn member_peers(node: &Node, state: &SpaceState) -> Vec<PeerId> {
    member_peers_where(node, state, |_| true)
}

/// [`member_peers`] 가운데 `keep`인 사용자의 기기 노드(그룹 토픽 허용 목록: 그 그룹 채널을 볼 수 있는 멤버).
pub(crate) fn member_peers_where(
    node: &Node,
    state: &SpaceState,
    keep: impl Fn(&UserId) -> bool,
) -> Vec<PeerId> {
    let me = node.peer_id();
    let member =
        |u: &UserId, d: &DeviceId| state.is_member(u) && state.device_belongs(u, d) && keep(u);
    let mut out = BTreeSet::new();
    let mut online = HashSet::new();
    for (p, u, d) in node.peers.authed_peers() {
        online.insert(d);
        if p != me && member(&u, &d) {
            out.insert(p);
        }
    }
    let rows = match node.store.identified_peers() {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "노드 기록을 읽지 못함");
            Vec::new()
        }
    };
    for rec in rows {
        let (Some(u), Some(d)) = (rec.user, rec.device) else {
            continue;
        };
        if online.contains(&d) || !member(&u, &d) {
            continue;
        }
        let Ok(p) = PeerId::from_bytes(&rec.peer_id) else {
            continue;
        };
        if p == me || !out.insert(p) {
            continue;
        }
        // 다시 걸 수 있게 기록된 주소를 알려 둡니다(기기마다 찾던 때와 같게).
        let addrs: Vec<Multiaddr> = rec.addrs.iter().filter_map(|a| a.parse().ok()).collect();
        if !addrs.is_empty() {
            node.net.add_addresses(p, addrs);
        }
    }
    out.into_iter().collect()
}

/// 받은 gossip.
pub(crate) async fn on_gossip(
    node: &Arc<Node>,
    from: PeerId,
    msg_id: MessageId,
    topic: GossipTopic,
    msg: GossipMessage,
) {
    let verdict = gossip_verdict(node, from, topic, msg).await;
    node.net.report_gossip(msg_id, from, verdict);
}

/// 받은 gossip을 적용하고 판정합니다. 거부(`Reject`)는 전달한 피어를 감점하므로, 전달한 피어도 걸렀어야 하는 것
/// (서명·형식·암호·레인 오류, gossip에 실린 휘발성 이벤트)에만 씁니다. 지금 상태로 갈린 거부(권한·멤버십·상태
/// 규칙)는 무시(`Ignore`): 전달한 멤버가 아직 권한 회수를 받지 못했거나 내가 부여를 받지 못했을 수 있습니다.
/// 작성 기기별 속도 제한에 걸린 이벤트가 있어도 무시합니다(더 퍼뜨리지 않음, 전달한 피어 잘못이 아님). 받은 이벤트의
/// 부모가 빠졌으면 받아들였든 지금 상태로 거부했든 그 피어와 빈 부모를 채웁니다(`Pull::Gap`: 같은 제한을 탐).
pub(crate) async fn gossip_verdict(
    node: &Arc<Node>,
    from: PeerId,
    topic: GossipTopic,
    msg: GossipMessage,
) -> GossipVerdict {
    let GossipMessage::Events { space, events } = msg;
    let Some(state) = node.space_state(&space) else {
        return GossipVerdict::Ignore;
    };
    // 내가 나간 공간·만료된 연락처와의 공간은 받지 않습니다(남은 멤버가 계속 밀어 넣지 못하게).
    if !node.me().is_ok_and(|me| state.is_member(&me.user()))
        || crate::friends::space_deactivated(node, &space)
    {
        return GossipVerdict::Ignore;
    };
    if events.len() > MAX_EVENTS_PER_MESSAGE {
        return GossipVerdict::Ignore;
    }
    // 휘발성 이벤트는 참가자에게 직접 보낸 것만 받습니다. 섞인 묶음은 통째로 버려 더 퍼지지도 않게 합니다
    // (정상 노드는 휘발성 이벤트를 gossip에 싣지 않음).
    if events
        .iter()
        .any(|e| crate::volatile::is_direct_only(&state, e))
    {
        return GossipVerdict::Reject;
    }
    // 그룹 토픽에는 그 그룹 채널 이벤트와 그 그룹 키 스트림만 실립니다. 다른 것은 전달한 멤버가 그 채널의 그룹이
    // 바뀐 것(열람 규칙 변경)을 아직 모를 수 있어 감점 없이 버립니다.
    if let GossipTopic::Group(_, g) = topic {
        let in_group = |e: &SignedEvent| match e.header.stream() {
            StreamId::KeyGroup(x) => x == g,
            StreamId::Channel(c) => {
                state
                    .channels
                    .get(&c)
                    .and_then(|ch| state.channel_key_group(&space, ch))
                    == Some(g)
            }
            _ => false,
        };
        if !events.iter().all(in_group) {
            return GossipVerdict::Ignore;
        }
    }
    // 묶음 토픽에는 그 묶음의 모두 보는 채널 이벤트만, 알림 토픽에는 모두 보는 채널 메시지만 실립니다. 열람 규칙이
    // 바뀐 것을 전달한 멤버가 아직 모를 수 있어 감점 없이 버립니다.
    let index = node.sync.index.get(&space, &state);
    let shaped = match topic {
        GossipTopic::Bucket(_, b) => events.iter().all(|e| {
            crate::chantopics::event_bucket(&space, &state, &index, e)
                == Some(GossipTopic::Bucket(space, b))
        }),
        GossipTopic::Notify(_) => events.iter().all(|e| {
            e.header.kind == yangpa_core::event::EventKind::Message
                && crate::chantopics::event_bucket(&space, &state, &index, e).is_some()
        }),
        _ => true,
    };
    if !shaped {
        return GossipVerdict::Ignore;
    }
    // 부분 전달 이벤트도 받을 자격이 있는 기기에게만 갑니다: 직접, 또는 그 그룹 토픽(정상 노드는 크루 토픽에 싣지 않음).
    let scoped_ok = |e: &SignedEvent| match (e.header.stream(), topic) {
        (StreamId::KeyGroup(x), GossipTopic::Group(_, g)) => x == g,
        (s, _) => !s.is_scoped(),
    };
    if !events.iter().all(scoped_ok) {
        for e in &events {
            note_foreign_scoped(node, e);
        }
        return GossipVerdict::Reject;
    }
    // 판정은 이 메시지의 이벤트로만 합니다(다시 해소하며 거부로 바뀐 옛 이벤트·깨어난 대기 이벤트는 다른 피어가 준 것).
    let batch: HashSet<EventId> = events.iter().map(SignedEvent::id).collect();
    let notify = matches!(topic, GossipTopic::Notify(_)).then(|| events.clone());
    match ingest::ingest_live(node, space, events, from).await {
        Ok(r) if r.has_invalid(&batch) => GossipVerdict::Reject,
        Ok(r) if !r.throttled.is_empty() => {
            debug!(
                space = %space.short(),
                dropped = r.throttled.len(),
                "작성 기기 속도 제한으로 gossip 상태 이벤트를 버림(따라잡기로 받음)"
            );
            GossipVerdict::Ignore
        }
        Ok(r) if r.has_rejected_by_state(&batch) => {
            // 부모가 빠져 지금 상태로 거부됐을 수 있습니다(가입 기록보다 먼저 온 크루 키 갱신 등). 빈 부모를 채우면 다시
            // 해소해 받아들이므로 판정은 무시여도 채웁니다. 안 채우면 주기 따라잡기까지 멈춥니다.
            if r.missing_parents {
                spawn_pull(node, space, from, Pull::Gap);
            }
            GossipVerdict::Ignore
        }
        Ok(r) if !r.accepted.is_empty() => {
            // 알림 토픽으로 온 멘션의 빈 부모(따라가지 않는 채널의 앞 글)는 채널을 열 때 받습니다.
            if r.missing_parents && notify.is_none() {
                spawn_pull(node, space, from, Pull::Gap);
            }
            // 알림 토픽: 받아 연 본문에 멘션·답장이 있고(멘션이 많으면 권한), 작성 기기 몫 안이어야 더 퍼뜨립니다.
            if let Some(evs) = notify {
                let state = node.space_state(&space).unwrap_or(state);
                let index = node.sync.index.get(&space, &state);
                let ok = evs.iter().all(|e| {
                    crate::chantopics::notify_worthy(node, &state, &index, e)
                        && crate::chantopics::notify_admit(node, space, e)
                });
                if !ok {
                    return GossipVerdict::Ignore;
                }
            }
            GossipVerdict::Accept
        }
        _ => GossipVerdict::Ignore,
    }
}

/// 받은 `PushEvents`. 내가 멤버인 공간에, 인증된 멤버 기기만.
pub(crate) async fn handle_push(
    node: &Arc<Node>,
    peer: PeerId,
    space: SpaceId,
    events: Vec<SignedEvent>,
) -> YpResponse {
    let Some((user, device)) = node.peers.identity_of(&peer) else {
        return YpResponse::error(WireErrorCode::Unauthenticated, "인증 전");
    };
    let Ok(me) = node.me() else {
        return YpResponse::error(WireErrorCode::Internal, "신원 없음");
    };
    match node.space_state(&space) {
        Some(state) => {
            // 내가 나간 공간(친구 삭제 등)·만료된 연락처와의 공간은 기록만 남기고 더 받지 않습니다.
            if !state.is_member(&me.user())
                || !node
                    .sync
                    .index
                    .get(&space, &state)
                    .is_member_device(&user, &device)
                || crate::friends::space_deactivated(node, &space)
            {
                return YpResponse::error(WireErrorCode::NotMember, "멤버가 아님");
            }
        }
        // 받아들인 공간: 상태가 생기기 전에도 내 다른 기기의 전파(기기 목록 전달·크루 키 갱신)를 받습니다.
        None if node.spaces.read().contains_key(&space)
            && crate::identity::is_adopting(node, &space, &user) => {}
        // 초대 가입 대기 중(상태를 아직 못 풂): 받아 준 노드·공간을 만든 사용자의 기기만.
        None if crews::join_peer_ok(node, &space, &peer, &user) => {}
        None => return YpResponse::error(WireErrorCode::NotFound, "모르는 공간"),
    }
    if events.len() > MAX_EVENTS_PER_MESSAGE {
        return YpResponse::error(WireErrorCode::BadRequest, "이벤트가 너무 많음");
    }
    // 직접 전파는 공간 활동으로 셉니다(휴면이면 깨움). gossip은 세지 않습니다.
    policy::on_push(node, &space);
    match ingest::ingest_live(node, space, events, peer).await {
        Ok(r) => {
            if !r.throttled.is_empty() {
                debug!(
                    space = %space.short(),
                    dropped = r.throttled.len(),
                    "작성 기기 속도 제한으로 받은 상태 이벤트를 버림(따라잡기로 받음)"
                );
            }
            if r.missing_parents || r.held > 0 {
                spawn_pull(node, space, peer, Pull::Gap);
            }
            YpResponse::Ok
        }
        Err(e) => {
            debug!(error = %e, "받은 이벤트 적용 실패");
            YpResponse::error(WireErrorCode::Internal, "적용 실패")
        }
    }
}

// ───────────── 따라잡기 ─────────────

/// 이 피어(사용자)와 내가 함께 멤버인 모든 공간을 따라잡습니다(나간 공간은 기록만 남기고 동기화하지 않음, 휴면 공간은
/// 깨어날 때 따라잡음). 받아들여 아직 상태가 없는 공간은 내 다른 기기에게서 받습니다(재시작
/// 뒤에도 주기 동기화가 이어 감).
pub(crate) fn catch_up_peer(node: &Arc<Node>, peer: PeerId) {
    let Some((user, device)) = node.peers.identity_of(&peer) else {
        return;
    };
    let Ok(me) = node.me() else { return };
    // 보관 쪽: 이 멤버 기기에게서 받아 둔 공간을 당겨옵니다.
    crate::archive::on_peer_authed(node, peer, &user, &device);
    for space in node.space_ids() {
        if policy::is_marked_dormant(node, &space)
            || crate::friends::space_deactivated(node, &space)
        {
            continue;
        }
        let wanted = match node.space_state(&space) {
            // 지정된 보관 노드도 시더처럼 따라잡기 상대로 씁니다.
            Some(s) => {
                s.is_member(&me.user())
                    && ((s.is_member(&user) && s.device_belongs(&user, &device))
                        || crate::archive::is_active(&s, &user, &device))
            }
            // 상태를 아직 못 푼 공간: 받아들인 공간이면 내 다른 기기, 초대 가입 대기 중이면 받아 준 노드·
            // 공간을 만든 사용자의 기기에게서 받습니다.
            None => {
                crate::identity::is_adopting(node, &space, &user)
                    || crews::join_peer_ok(node, &space, &peer, &user)
            }
        };
        if wanted {
            spawn_catch_up(node, space, peer);
        }
    }
}

/// 그 피어에게서 공간 스트림만 한 바퀴 맞춥니다. 상대를 인증하지 않고 부르므로(놓친 가입 확인, `peers::handle_check_join`)
/// 받은 이벤트는 늘 하는 검사만 거칩니다.
pub(crate) async fn pull_space_stream(
    node: &Arc<Node>,
    space: SpaceId,
    peer: PeerId,
) -> Result<()> {
    sync_stream(node, space, peer, StreamId::Space, Pull::Full).await
}

/// 진행 중인 따라잡기가 없는지.
pub(crate) fn catch_up_idle(node: &Node) -> bool {
    node.sync.running.lock().is_empty()
}

/// 한 (공간, 피어) 따라잡기(제한 없음). 이미 도는 중이면 끝난 뒤 한 번 더 돕니다.
pub(crate) fn spawn_catch_up(node: &Arc<Node>, space: SpaceId, peer: PeerId) {
    spawn_pull(node, space, peer, Pull::Full);
}

/// 한 (공간, 피어) 따라잡기. 이미 도는 중이면 끝난 뒤 한 번 더 돌고, 그사이 여러 번 부르면 그중 넓은 종류(`Full`)로
/// 돕니다.
pub(crate) fn spawn_pull(node: &Arc<Node>, space: SpaceId, peer: PeerId, pull: Pull) {
    {
        let mut running = node.sync.running.lock();
        if let Some(rerun) = running.get_mut(&(space, peer)) {
            *rerun = (*rerun).max(Some(pull));
            return;
        }
        running.insert((space, peer), None);
    }
    node.spawn_detached(move |node| async move {
        let mut pull = pull;
        loop {
            if let Err(e) = catch_up(&node, space, peer, pull).await {
                debug!(space = %space.short(), peer = %peer, error = %e, "따라잡기 실패");
            }
            let again = {
                let mut running = node.sync.running.lock();
                match running.get_mut(&(space, peer)) {
                    Some(rerun) if rerun.is_some() => rerun.take(),
                    _ => {
                        running.remove(&(space, peer));
                        None
                    }
                }
            };
            match again {
                Some(p) => pull = p,
                None => break,
            }
        }
    });
}

async fn catch_up(node: &Arc<Node>, space: SpaceId, peer: PeerId, pull: Pull) -> Result<()> {
    // P2P 피어와는 함께 참여한 P2P 전용 채널의 채널 스트림만 맞춥니다(공간 스트림은 Tor로).
    if crate::p2p::is_p2p_peer(node, &peer) {
        if node.peers.p2p_identity(&peer).is_none() {
            return Err(Error::PeerUnauthenticated);
        }
        for c in crate::p2p::shared_channels(node, &peer, &space) {
            if let Err(e) = sync_stream(node, space, peer, StreamId::Channel(c), pull).await {
                debug!(error = %e, "P2P 채널 따라잡기 실패");
            }
        }
        // 신뢰로 일반 내용도 이 연결로 나르면 나머지는 같은 기기의 Tor 피어로 맞춥니다(`p2p::request`가 이 연결로 돌림).
        if let Some(tor) = crate::p2p::normal_tor_peer(node, &peer) {
            spawn_pull(node, space, tor, pull);
        }
        return Ok(());
    }
    peers::ensure_auth(node, peer).await?;
    let started = std::time::Instant::now();
    if let Err(e) = sync_stream(node, space, peer, StreamId::Space, pull).await {
        // 상대가 걸어 온 인증에 답만 하고 상대는 이 기기를 받지 않았으면 이쪽만 상대를 인증된 피어로 압니다. 내 인증을
        // 한 번 다시 보내 확인합니다: 상대가 아직 답을 처리하지 못했으면 받아 주고, 이 기기를 폐기했으면 `Revoked`로
        // 알려 와 지우게 됩니다(그러지 않으면 다음에 이쪽이 먼저 걸 때까지 모름).
        if !matches!(e, Error::PeerUnauthenticated) || !peers::reconfirm_auth(node, peer).await {
            return Err(e);
        }
        sync_stream(node, space, peer, StreamId::Space, pull).await?;
    }
    // 내 받은편지함: 가입자는 여기 든 키가 있어야 공간 스트림을 열므로 상태를 세우기 전에도 맞춥니다(상대가 내 기기를
    // 아직 모르면 빈 스트림으로 답해 다음 따라잡기에서 다시 받음).
    if let Ok(me) = node.me()
        && let Err(e) = sync_stream(node, space, peer, StreamId::Inbox(me.user()), pull).await
    {
        debug!(error = %e, "받은편지함 따라잡기 실패");
    }
    let state = node.space_state(&space);
    // 상대가 확정 기기이면 내가 전달자인 최근 가입자·연결한 기기의 받은편지함도 당겨 옵니다(확정 기기가 보낼 때 내가 꺼져
    // 있었으면 가입자는 그 묶음을 받을 길이 없음: 처음에는 나만 믿음).
    if let (Some(st), Ok(me), Some((_, device))) =
        (&state, node.me(), node.peers.identity_of(&peer))
        && device == st.committer
    {
        let since = Timestamp(Timestamp::now().0.saturating_sub(RELAY_PULL_WINDOW_MS));
        for u in crews::relayed_users(node, &space, &me.device_id(), since, RELAY_PULL_USERS) {
            if let Err(e) = sync_stream(node, space, peer, StreamId::Inbox(u), pull).await {
                debug!(error = %e, "전달할 받은편지함 따라잡기 실패");
            }
        }
    }
    // 제한 없는 한 바퀴만 셉니다(빈 부모 채우기는 작성 기기별 제한으로 중간에 멈출 수 있음).
    if pull == Pull::Full {
        ingest::note_caught_up(node, space, started);
    }
    let Some(state) = state else {
        return Ok(());
    };
    let index = node.sync.index.get(&space, &state);
    // 채널 토픽을 쓰면 따라가지 않는 모두 보는 채널은 열 때 받습니다(`chantopics::follow`).
    let wanted = crate::chantopics::sync_filter(node, &space, &state, &index);
    for (c, ch) in &state.channels {
        if ch.is_volatile() || state.is_p2p(c) || !wanted(c) {
            continue;
        }
        if let Err(e) = sync_stream(node, space, peer, StreamId::Channel(*c), pull).await {
            debug!(error = %e, "채널 따라잡기 실패");
        }
    }
    for g in my_key_groups(node, &space, &state) {
        if let Err(e) = sync_stream(node, space, peer, StreamId::KeyGroup(g), pull).await {
            debug!(error = %e, "키 그룹 따라잡기 실패");
        }
    }
    Ok(())
}

/// 따라잡을 키 그룹: 내가 볼 수 있는 채널 가운데 모두가 볼 수 있지는 않은 채널의 그룹(모두가 보는 채널의 키는 크루
/// 그룹으로 충분하고 그 갱신은 공간 스트림이라 키 그룹 스트림을 쓸 일이 없음).
fn my_key_groups(
    node: &Node,
    space: &SpaceId,
    state: &Arc<SpaceState>,
) -> Vec<yangpa_core::ids::KeyGroupId> {
    let Ok(me) = node.me() else {
        return Vec::new();
    };
    let me = me.user();
    // 모든 멤버가 보는 채널(보기 덮어쓰기 없음)은 크루 그룹을 써 비공개 그룹이 없습니다(`channel_key_group`). 규칙이
    // 있는데 마침 모두가 보는 채널의 그룹은 따라잡습니다: 많으면 전달이 일부 기기에만 밀기 때문입니다.
    let mut out: Vec<_> = state
        .channels
        .iter()
        .filter(|(c, ch)| {
            !ch.is_volatile()
                && !state.is_p2p(c)
                && state
                    .channel_permissions(&me, c)
                    .contains(Permissions::VIEW_CHANNEL)
        })
        .filter_map(|(_, ch)| state.channel_key_group(space, ch))
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// 공간 스트림만 이 피어와 바로 맞춥니다(보관 노드에 명단을 보내기 전, 따라잡지 못한 상태로 명단을 내지 않게).
pub(crate) async fn sync_space_stream(
    node: &Arc<Node>,
    space: SpaceId,
    peer: PeerId,
) -> Result<()> {
    sync_stream(node, space, peer, StreamId::Space, Pull::Full).await
}

async fn request(node: &Node, peer: PeerId, req: YpRequest) -> Result<YpResponse> {
    match crate::p2p::request(node, peer, req).await? {
        YpResponse::Error {
            code: WireErrorCode::Unauthenticated,
            ..
        } => Err(Error::PeerUnauthenticated),
        YpResponse::Error { code, .. } => Err(Error::Network(format!("상대가 거절함: {code:?}"))),
        YpResponse::Busy { .. } => Err(Error::Network("상대가 바쁨".into())),
        r => Ok(r),
    }
}

async fn fetch_events(
    node: &Arc<Node>,
    space: SpaceId,
    peer: PeerId,
    ids: &[EventId],
) -> Result<Vec<SignedEvent>> {
    let mut out = Vec::new();
    for chunk in ids.chunks(MAX_EVENTS_PER_MESSAGE) {
        let req = YpRequest::GetEvents {
            space,
            ids: chunk.to_vec(),
        };
        match request(node, peer, req).await? {
            YpResponse::Events(evs) => {
                // 요청하지 않은 이벤트는 버립니다.
                let wanted: BTreeSet<&EventId> = chunk.iter().collect();
                out.extend(evs.into_iter().filter(|e| wanted.contains(&e.id())));
            }
            _ => return Err(Error::Network("이벤트 응답이 아님".into())),
        }
    }
    Ok(out)
}

/// 따라잡기로 받은 묶음을 적용합니다. 계속 맞춰도 되면 true, `Pull::Gap`에서 작성 기기별 제한에 걸렸으면 false(이
/// 스트림은 여기서 멈춤: 더 받아 봐야 또 버리므로).
async fn apply_pulled(
    node: &Arc<Node>,
    space: SpaceId,
    peer: PeerId,
    pull: Pull,
    events: Vec<SignedEvent>,
) -> Result<bool> {
    if pull == Pull::Full {
        ingest::ingest(node, space, events, Source::Peer(peer)).await?;
        return Ok(true);
    }
    let r = ingest::ingest_live(node, space, events, peer).await?;
    if r.throttled.is_empty() {
        return Ok(true);
    }
    debug!(
        space = %space.short(),
        dropped = r.throttled.len(),
        "작성 기기 속도 제한으로 빈 부모 채우기를 멈춤(제한 없는 따라잡기가 가져옴)"
    );
    Ok(false)
}

/// 따라잡기로 받았지만 아직 적용하지 않은 묶음들.
///
/// 적용(`ingest`)마다 해소·바뀐 상태 행 저장·상태 색인 다시 셈·알림이 한 번씩 드는데, 큰 크루를 따라잡을 때 512개짜리
/// 묶음마다 그러면 같은 일을 수백 번 합니다. 그래서 묶음 [`PULL_FLUSH_BATCHES`]개나 [`PULL_FLUSH_EVERY`]가 지나면,
/// 모은 크기가 [`PULL_FLUSH_BYTES`]를 넘으면, 또는 빠진 부모가 없어(받은 갈래가 다 이어져) 해소하면 상태가 끝까지
/// 나아갈 때 한 번에 적용합니다. 한 번에 적용하는 것은 한 `ingest`에 더 많이 넣는 것과 같아 결과는 같습니다(해소기는
/// 받는 순서와 무관). 모아 둔 이벤트는 아직 저장소에 없으므로, 더 받을 것을 고를 때([`unknown_pending`]·지문 비교의
/// `has_events`) 여기 있는 것도 아는 것으로 셉니다.
///
/// 모으는 것은 `Pull::Full`뿐입니다. `Pull::Gap`은 작성 기기별 제한을 적용할 때 확인하므로, 모으면 제한에 걸린 작성자의
/// 묶음을 몇 개 더 받아 적용한 뒤에야 멈춥니다. 그래서 묶음마다 적용합니다.
struct PullBuffer {
    pull: Pull,
    events: Vec<SignedEvent>,
    ids: HashSet<EventId>,
    batches: usize,
    bytes: usize,
    since: std::time::Instant,
}

impl PullBuffer {
    fn new(pull: Pull) -> Self {
        PullBuffer {
            pull,
            events: Vec::new(),
            ids: HashSet::new(),
            batches: 0,
            bytes: 0,
            since: std::time::Instant::now(),
        }
    }

    fn push(&mut self, evs: Vec<SignedEvent>) {
        for e in evs {
            if self.ids.insert(e.id()) {
                self.bytes += e.body.as_ref().map_or(0, Vec::len) + PULL_EVENT_OVERHEAD;
                self.events.push(e);
            }
        }
        self.batches += 1;
    }

    /// 쌓인 만큼 적용할 때인지.
    fn due(&self) -> bool {
        self.pull == Pull::Gap
            || self.batches >= PULL_FLUSH_BATCHES
            || self.bytes >= PULL_FLUSH_BYTES
            || self.since.elapsed() >= PULL_FLUSH_EVERY
    }

    /// 모은 것을 적용합니다. 반환값은 [`apply_pulled`]와 같음.
    async fn flush(&mut self, node: &Arc<Node>, space: SpaceId, peer: PeerId) -> Result<bool> {
        self.batches = 0;
        self.bytes = 0;
        self.since = std::time::Instant::now();
        self.ids.clear();
        let events = std::mem::take(&mut self.events);
        if events.is_empty() {
            return Ok(true);
        }
        apply_pulled(node, space, peer, self.pull, events).await
    }

    /// 받기가 도중에 실패했을 때: 이미 받은 묶음은 적용하고 원래 오류를 돌려줍니다. 버리면 회로가 자주 끊기는 피어에게서
    /// 따라잡을 때 매번 같은 앞 묶음을 다시 받고 또 버려 나아가지 못합니다.
    async fn flush_on_error(
        &mut self,
        node: &Arc<Node>,
        space: SpaceId,
        peer: PeerId,
        err: Error,
    ) -> Error {
        if let Err(e) = self.flush(node, space, peer).await {
            debug!(space = %space.short(), "따라잡기 실패 전에 받은 이벤트 적용 실패: {e}");
        }
        err
    }
}

/// 한 스트림 맞추기: 머리 → 부모 따라가기 → 지문 비교.
async fn sync_stream(
    node: &Arc<Node>,
    space: SpaceId,
    peer: PeerId,
    stream: StreamId,
    pull: Pull,
) -> Result<()> {
    let (heads, remote_max) =
        match request(node, peer, YpRequest::GetHeads { space, stream }).await? {
            YpResponse::Heads { heads, max_lamport } => (heads, max_lamport),
            _ => return Err(Error::Network("머리 응답이 아님".into())),
        };
    let mut buf = PullBuffer::new(pull);
    // 체크포인트 경계: 공간 스트림에서 경계 아래 이벤트는 받지 않고 그 부모를 따라가지 않습니다(체크포인트가 대신함).
    let floor = if stream == StreamId::Space {
        checkpoint::floor(node, &space)
    } else {
        0
    };
    let below = |ids: Vec<EventId>| -> Vec<EventId> {
        if floor == 0 {
            return ids;
        }
        ids.into_iter()
            .filter(|id| !checkpoint::is_below(node, &space, id))
            .collect()
    };
    let mut want = below(unknown(node, space, stream, &heads)?);
    let mut rounds = 0;
    while !want.is_empty() && rounds < CHASE_ROUNDS {
        rounds += 1;
        let mut evs = match fetch_events(node, space, peer, &want).await {
            Ok(evs) => evs,
            Err(e) => return Err(buf.flush_on_error(node, space, peer, e).await),
        };
        if evs.is_empty() {
            break;
        }
        if floor > 0 {
            let (keep, drop): (Vec<_>, Vec<_>) =
                evs.into_iter().partition(|e| e.header.lamport >= floor);
            // 저장하지 않지만 논리 시계는 기억해, 해소기가 이 부모를 빠진 부모로 기다리지 않게 알려 줍니다.
            checkpoint::note_below(
                node,
                &space,
                drop.iter().map(|e| (e.id(), e.header.lamport)),
            );
            evs = keep;
        }
        // 경계에 있는 이벤트의 부모는 모두 경계 아래입니다.
        let parents: Vec<EventId> = evs
            .iter()
            .filter(|e| e.header.lamport > floor || floor == 0)
            .flat_map(|e| e.header.parents.iter().copied())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        buf.push(evs);
        want = below(unknown_pending(node, space, stream, &parents, &buf.ids)?);
        want.truncate(MAX_EVENTS_PER_MESSAGE);
        // 빠진 부모가 없으면 받은 갈래가 다 이어졌으므로 바로 적용합니다(해소가 끝까지 나아감).
        if (want.is_empty() || buf.due()) && !buf.flush(node, space, peer).await? {
            return Ok(());
        }
    }
    if !buf.flush(node, space, peer).await? {
        return Ok(());
    }
    // 머리가 같고 빠진 부모도 없으면 끝.
    let local_heads = node.store.heads(&space, stream)?;
    let local_missing = below(node.store.missing_parents(&space, stream)?);
    let mut same: Vec<EventId> = heads.clone();
    same.sort_unstable();
    let mut mine = local_heads.clone();
    mine.sort_unstable();
    if same == mine && local_missing.is_empty() {
        return Ok(());
    }
    reconcile(node, space, peer, stream, remote_max, pull).await
}

fn unknown(node: &Node, space: SpaceId, stream: StreamId, ids: &[EventId]) -> Result<Vec<EventId>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let have = have_events(node, space, stream, ids)?;
    Ok(ids
        .iter()
        .zip(have)
        .filter(|(_, h)| !h)
        .map(|(id, _)| *id)
        .collect())
}

/// [`unknown`]에서 아직 적용하지 않고 모아 둔 것([`PullBuffer`])도 뺀 것.
fn unknown_pending(
    node: &Node,
    space: SpaceId,
    stream: StreamId,
    ids: &[EventId],
    pending: &HashSet<EventId>,
) -> Result<Vec<EventId>> {
    let ids: Vec<EventId> = ids
        .iter()
        .filter(|id| !pending.contains(id))
        .copied()
        .collect();
    unknown(node, space, stream, &ids)
}

/// 이미 가진 것으로 볼 이벤트. 부분 전달 스트림은 확인을 기다리며 RAM에 둔 것과 거부를 기억하는 것도 셉니다(따라잡기마다
/// 같은 이벤트를 다시 내려받지 않게).
fn have_events(
    node: &Node,
    space: SpaceId,
    stream: StreamId,
    ids: &[EventId],
) -> std::result::Result<Vec<bool>, StoreError> {
    let mut have = node.store.has_events(ids)?;
    if stream.is_scoped() {
        for (h, k) in have
            .iter_mut()
            .zip(crate::scoped::known_ids(node, &space, ids))
        {
            *h |= k;
        }
    }
    Ok(have)
}

/// 저장소를 지문 비교 원본으로.
struct StoreSource<'a> {
    node: &'a Node,
    space: SpaceId,
    stream: StreamId,
    /// 받았지만 아직 적용하지 않은 이벤트([`PullBuffer`], 따라잡는 쪽만). 있는 것으로 셉니다.
    pending: Option<&'a HashSet<EventId>>,
}

impl FingerprintSource for StoreSource<'_> {
    type Error = StoreError;

    fn fingerprints(
        &self,
        ranges: &[LamportRange],
    ) -> std::result::Result<Vec<RangeFingerprint>, StoreError> {
        self.node
            .store
            .fingerprints(&self.space, self.stream, ranges)
    }

    fn ids_in_range(
        &self,
        range: LamportRange,
        limit: u32,
    ) -> std::result::Result<Vec<EventId>, StoreError> {
        Ok(self
            .node
            .store
            .ids_in_range(&self.space, self.stream, range, limit)?
            .into_iter()
            .map(|(_, id)| id)
            .collect())
    }

    fn has_events(&self, ids: &[EventId]) -> std::result::Result<Vec<bool>, StoreError> {
        let mut have = have_events(self.node, self.space, self.stream, ids)?;
        if let Some(p) = self.pending {
            for (h, id) in have.iter_mut().zip(ids) {
                *h |= p.contains(id);
            }
        }
        Ok(have)
    }
}

async fn reconcile(
    node: &Arc<Node>,
    space: SpaceId,
    peer: PeerId,
    stream: StreamId,
    remote_max: u64,
    pull: Pull,
) -> Result<()> {
    let local_max = node.store.stream_max_lamport(&space, stream)?;
    let mut range = full_range(local_max.max(remote_max));
    if stream == StreamId::Space {
        // 경계 아래는 맞추지 않습니다(체크포인트로 들어온 가입자에게는 없고, 받아도 해소기가 쓰지 않음).
        range.start = checkpoint::floor(node, &space).min(range.end);
    }
    let mut rec = Reconciler::new(range);
    let mut buf = PullBuffer::new(pull);
    while let Some((token, req)) = rec.next_request() {
        let src = StoreSource {
            node,
            space,
            stream,
            pending: Some(&buf.ids),
        };
        let resp = match req {
            ReconcileRequest::Fingerprints(ranges) => {
                request(
                    node,
                    peer,
                    YpRequest::GetFingerprints {
                        space,
                        stream,
                        ranges,
                    },
                )
                .await
            }
            ReconcileRequest::Ids { range, limit } => {
                request(
                    node,
                    peer,
                    YpRequest::GetIds {
                        space,
                        stream,
                        range,
                        limit,
                    },
                )
                .await
            }
        };
        let r = match &resp {
            Ok(YpResponse::Fingerprints(fps)) => {
                rec.on_response(&src, token, ReconcileResponse::Fingerprints(fps))
            }
            Ok(YpResponse::Ids(ids)) => rec.on_response(&src, token, ReconcileResponse::Ids(ids)),
            _ => {
                rec.on_failure(token);
                Ok(())
            }
        };
        if r.is_err() {
            break;
        }
        let fetch = rec.take_fetch();
        if !fetch.is_empty() {
            let evs = match fetch_events(node, space, peer, &fetch).await {
                Ok(evs) => evs,
                Err(e) => return Err(buf.flush_on_error(node, space, peer, e).await),
            };
            buf.push(evs);
            if buf.due() && !buf.flush(node, space, peer).await? {
                return Ok(());
            }
        }
    }
    buf.flush(node, space, peer).await?;
    Ok(())
}

/// 주기적 따라잡기(휴면이 아닌 공간, 인증된 멤버 피어마다)와 커밋 확정 재시도.
pub(crate) fn spawn_periodic(node: &Arc<Node>) {
    let weak = Arc::downgrade(node);
    let interval = node.cfg.sync_interval;
    node.tasks.spawn(async move {
        let mut tick = tokio::time::interval(interval);
        tick.tick().await;
        loop {
            tick.tick().await;
            let Some(node) = weak.upgrade() else { break };
            if node.is_shutting_down() {
                break;
            }
            for space in node.space_ids() {
                if policy::is_dormant(&node, &space) {
                    continue;
                }
                keygroup::on_state_changed(&node, space);
            }
            for (peer, _, _) in node.peers.authed_peers() {
                catch_up_peer(&node, peer);
            }
            crate::p2p::tick(&node);
            crate::filelink::tick(&node);
            crate::identity::adoption_tick(&node);
        }
    });
}

// ───────────── 시더 ─────────────

/// 요청한 피어의 사용자·기기와 공간 상태. 실패하면 그대로 보낼 거절 응답.
#[allow(clippy::result_large_err)]
pub(crate) fn requester(
    node: &Node,
    peer: &PeerId,
    space: &SpaceId,
) -> std::result::Result<(Arc<SpaceState>, UserId, DeviceId), YpResponse> {
    let Some((user, device)) = node.peers.identity_of(peer) else {
        return Err(YpResponse::error(WireErrorCode::Unauthenticated, "인증 전"));
    };
    let Some(state) = node.space_state(space) else {
        return Err(YpResponse::error(WireErrorCode::NotFound, "모르는 공간"));
    };
    if crate::friends::space_deactivated(node, space) {
        return Err(YpResponse::error(WireErrorCode::NotMember, "만료된 연락처"));
    }
    Ok((state, user, device))
}

fn stream_lane(stream: &StreamId) -> (Lane, Option<ChannelId>) {
    match stream {
        StreamId::Space => (Lane::State, None),
        StreamId::Channel(c) => (Lane::Message, Some(*c)),
        // 부분 전달 스트림은 키 레인. 누가 받을 수 있는지는 [`scoped_hidden`]이 따로 봅니다.
        StreamId::KeyGroup(_) | StreamId::Inbox(_) => (Lane::Key, None),
    }
}

/// 이 요청자에게 부분 전달 스트림을 숨겨야 하는지(받을 자격이 없음). 숨기면 빈 스트림과 똑같이 답합니다: 거절 코드를
/// 보내면 남의 받은편지함·키 그룹이 있는지, 누가 받는지 드러납니다. 보통 검사(멤버십·대역폭)는 그 전에 똑같이 합니다.
fn scoped_hidden(
    node: &Node,
    state: &SpaceState,
    space: &SpaceId,
    user: &UserId,
    device: &DeviceId,
    stream: &StreamId,
) -> bool {
    let relay = match stream {
        StreamId::Inbox(u) => crews::is_inbox_relay(node, space, u, device),
        _ => false,
    };
    may_read_scoped(state, space, user, device, stream, relay) == Some(false)
}

/// 이벤트 내용 없이 ID·머리·지문만 주는 요청의 검사(입장 이전 여부는 내용 요청에서 봄).
#[allow(clippy::result_large_err)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn check_meta(
    node: &Node,
    peer: &PeerId,
    space: &SpaceId,
    state: &SpaceState,
    user: UserId,
    device: DeviceId,
    stream: &StreamId,
    bytes: u64,
) -> std::result::Result<(), YpResponse> {
    let (lane, channel) = stream_lane(stream);
    if let Some(c) = &channel
        && state.channels.get(c).is_none_or(|ch| ch.is_volatile())
    {
        return Err(YpResponse::error(WireErrorCode::NotFound, "없는 채널"));
    }
    // 다른 길(P2P 전용 채널을 Tor로, 그 밖의 것을 P2P로)로 묻는 것은 없는 것처럼 답합니다.
    if !crate::p2p::route_ok(node, peer, space, state, stream) {
        return Err(YpResponse::error(WireErrorCode::NotFound, "없는 스트림"));
    }
    let opened = channel
        .as_ref()
        .is_none_or(|c| policy::channel_opened(node, space, state, c));
    let req = SeedRequest {
        state,
        requester: user,
        requester_device: device,
        lane,
        channel,
        data_type: DataType::Text,
        event: None,
        event_lamport: None,
        requester_since: None,
        channel_opened: opened,
        bytes,
        in_flight: 0,
        now_ms: Timestamp::now().0,
    };
    check(node, space, &req)
}

#[allow(clippy::result_large_err)]
pub(crate) fn check(
    node: &Node,
    space: &SpaceId,
    req: &SeedRequest<'_>,
) -> std::result::Result<(), YpResponse> {
    // 지정된 보관 노드의 요청은 보관 규칙으로(보관 범위의 채널만, 입장 이전 기록 검사는 보관 노드가 대신 함).
    if let Some(r) = crate::archive::check_seed(node, space, req) {
        return r;
    }
    let policy = policy::seeder_policy(node, space);
    // 크루·채널·사람별 올리기 제한은 먼저 보기만 하고, 전체 검사를 통과한 뒤에 씁니다(거절한 요청에 쓰지 않게).
    let scoped = crate::ratelimit::wait_ms(
        node,
        crate::ratelimit::Dir::Up,
        space,
        req.channel.as_ref(),
        Some(&req.requester),
        req.bytes,
        req.now_ms,
    );
    if scoped > 0 {
        return Err(YpResponse::Busy {
            retry_after_ms: scoped,
        });
    }
    {
        let mut bucket = node.sync.bucket.lock();
        let bucket =
            bucket.get_or_insert_with(|| TokenBucket::new(policy.upload_limit_bps, req.now_ms));
        check_seed_request(&policy, req, bucket).map_err(|r| r.to_response())?;
    }
    // 차단한 사용자에게는 메시지를 주지 않습니다. 정책이 빼 둔 데이터와 같은 답이라 차단이 드러나지 않습니다.
    if crate::moderation::withholds(node, &req.requester, req.lane) {
        return Err(SeedRefusal::Denied(WireErrorCode::NotSeeded).to_response());
    }
    crate::ratelimit::consume(
        node,
        crate::ratelimit::Dir::Up,
        space,
        req.channel.as_ref(),
        Some(&req.requester),
        req.bytes,
        req.now_ms,
    );
    Ok(())
}

pub(crate) fn handle_get_heads(
    node: &Node,
    peer: PeerId,
    space: SpaceId,
    stream: StreamId,
) -> YpResponse {
    let (state, user, device) = match requester(node, &peer, &space) {
        Ok(x) => x,
        Err(r) => return r,
    };
    if let Err(r) = check_meta(node, &peer, &space, &state, user, device, &stream, 64) {
        return r;
    }
    if scoped_hidden(node, &state, &space, &user, &device, &stream) {
        return YpResponse::Heads {
            heads: Vec::new(),
            max_lamport: 0,
        };
    }
    match (
        node.store.heads(&space, stream),
        node.store.stream_max_lamport(&space, stream),
    ) {
        (Ok(heads), Ok(max_lamport)) => YpResponse::Heads { heads, max_lamport },
        _ => YpResponse::error(WireErrorCode::Internal, "저장소 오류"),
    }
}

pub(crate) fn handle_get_fingerprints(
    node: &Node,
    peer: PeerId,
    space: SpaceId,
    stream: StreamId,
    ranges: Vec<LamportRange>,
) -> YpResponse {
    let (state, user, device) = match requester(node, &peer, &space) {
        Ok(x) => x,
        Err(r) => return r,
    };
    if let Err(r) = check_meta(
        node,
        &peer,
        &space,
        &state,
        user,
        device,
        &stream,
        40 * ranges.len() as u64,
    ) {
        return r;
    }
    if scoped_hidden(node, &state, &space, &user, &device, &stream) {
        return match answer_fingerprints(&MemorySet::new(), &ranges) {
            Ok(fps) => YpResponse::Fingerprints(fps),
            Err(_) => YpResponse::error(WireErrorCode::BadRequest, "잘못된 구간"),
        };
    }
    let src = StoreSource {
        node,
        space,
        stream,
        pending: None,
    };
    match answer_fingerprints(&src, &ranges) {
        Ok(fps) => YpResponse::Fingerprints(fps),
        Err(_) => YpResponse::error(WireErrorCode::BadRequest, "잘못된 구간"),
    }
}

pub(crate) fn handle_get_ids(
    node: &Node,
    peer: PeerId,
    space: SpaceId,
    stream: StreamId,
    range: LamportRange,
    limit: u32,
) -> YpResponse {
    let (state, user, device) = match requester(node, &peer, &space) {
        Ok(x) => x,
        Err(r) => return r,
    };
    if let Err(r) = check_meta(
        node,
        &peer,
        &space,
        &state,
        user,
        device,
        &stream,
        32 * u64::from(limit.min(512)),
    ) {
        return r;
    }
    if scoped_hidden(node, &state, &space, &user, &device, &stream) {
        return match answer_ids(&MemorySet::new(), range, limit) {
            Ok(ids) => YpResponse::Ids(ids),
            Err(_) => YpResponse::error(WireErrorCode::BadRequest, "잘못된 구간"),
        };
    }
    let src = StoreSource {
        node,
        space,
        stream,
        pending: None,
    };
    match answer_ids(&src, range, limit) {
        Ok(ids) => YpResponse::Ids(ids),
        Err(_) => YpResponse::error(WireErrorCode::BadRequest, "잘못된 구간"),
    }
}

/// `GetEvents`: 이벤트마다 시더 검사. 통과한 것만 돌려주고, 하나도 없으면 첫 거절 이유.
pub(crate) fn handle_get_events(
    node: &Node,
    peer: PeerId,
    space: SpaceId,
    ids: Vec<EventId>,
) -> YpResponse {
    let (state, user, device) = match requester(node, &peer, &space) {
        Ok(x) => x,
        Err(r) => return r,
    };
    if ids.len() > MAX_EVENTS_PER_MESSAGE {
        return YpResponse::error(WireErrorCode::BadRequest, "ID가 너무 많음");
    }
    let since = requester_since(node, &space, &user);
    let serving = serving(node, device);
    let in_flight = serving.others;
    let r = (|| {
        let evs = match node.store.get_events(&ids) {
            Ok(e) => e,
            Err(_) => return YpResponse::error(WireErrorCode::Internal, "저장소 오류"),
        };
        let mut out = Vec::with_capacity(evs.len());
        let mut first_refusal = None;
        for ev in evs {
            let h = &ev.header;
            // 요청에 적은 공간의 이벤트만(다른 공간의 이벤트를 이 공간 멤버 자격으로 내주지 않음).
            if h.space != space {
                first_refusal.get_or_insert_with(|| {
                    YpResponse::error(WireErrorCode::NotFound, "없는 이벤트")
                });
                continue;
            }
            if !crate::p2p::route_ok(node, &peer, &space, &state, &h.stream()) {
                first_refusal.get_or_insert_with(|| {
                    YpResponse::error(WireErrorCode::NotFound, "없는 이벤트")
                });
                continue;
            }
            // 받을 자격이 없는 부분 전달 이벤트는 모르는 이벤트처럼 뺍니다(거절 이유도 남기지 않음).
            if scoped_hidden(node, &state, &space, &user, &device, &h.stream()) {
                continue;
            }
            let lane = h.kind.lane();
            let channel = match h.stream() {
                StreamId::Channel(c) => Some(c),
                StreamId::Space | StreamId::KeyGroup(_) | StreamId::Inbox(_) => None,
            };
            let opened = channel
                .as_ref()
                .is_none_or(|c| policy::channel_opened(node, &space, &state, c));
            let req = SeedRequest {
                state: &state,
                requester: user,
                requester_device: device,
                lane,
                channel,
                data_type: DataType::Text,
                event: Some(ev.id()),
                event_lamport: Some(h.lamport),
                requester_since: since,
                channel_opened: opened,
                bytes: u64::from(h.body_len) + 256,
                in_flight,
                now_ms: Timestamp::now().0,
            };
            match check(node, &space, &req) {
                Ok(()) => out.push(ev),
                Err(r) => {
                    first_refusal.get_or_insert(r);
                }
            }
        }
        match (out.is_empty(), first_refusal) {
            (true, Some(r)) => r,
            _ => YpResponse::Events(out),
        }
    })();
    drop(serving);
    r
}

/// 요청자가 처음 멤버면 `None`, 나중에 들어왔으면 들어온 논리 시계. 모르면 가장 보수적으로(방금 들어옴).
pub(crate) fn requester_since(node: &Node, space: &SpaceId, user: &UserId) -> Option<u64> {
    let spaces = node.spaces.read();
    let Some(rt) = spaces.get(space) else {
        return Some(u64::MAX);
    };
    match (rt.member_since.get(user), rt.genesis_lamport) {
        (Some(since), Some(g)) if *since <= g => None,
        (Some(since), _) => Some(*since),
        (None, _) => Some(u64::MAX),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state_index::StateIndexes;
    use crate::state_index::tests_support::{crew, deny_view, overwrite};
    use yangpa_core::event::{EVENT_VERSION, EventHeader, EventKind, KeyRef, MAX_PARENTS};
    use yangpa_core::ids::ChannelRootId;
    use yangpa_core::ids::RoleId;
    use yangpa_core::model::OverwriteTarget;
    use yangpa_core::sign::SigningKeypair;

    fn devices(st: &SpaceState) -> Vec<(UserId, DeviceId)> {
        crate::state_index::member_devices(st).collect()
    }

    /// 이 기기들과 인증된 연결이 있다고 칩니다(PeerId는 아무것이나).
    fn connected(of: &[(UserId, DeviceId)]) -> Vec<(PeerId, UserId, DeviceId)> {
        of.iter().map(|(u, d)| (PeerId::random(), *u, *d)).collect()
    }

    /// `propagate`의 `NoPeers` 대체 경로와 같은 고르기(큰 크루).
    fn nopeers(
        st: &SpaceState,
        me: DeviceId,
        live: &[(PeerId, UserId, DeviceId)],
        salt: &EventId,
    ) -> Vec<(UserId, DeviceId)> {
        let pool = devices(st);
        let live = connected_targets(st, me, live, salt, |_| true);
        pick_targets(&pool, |_| true, live, NOPEERS_FANOUT, me, salt)
    }

    fn distinct(t: &[(UserId, DeviceId)]) -> bool {
        t.iter().map(|(_, d)| *d).collect::<HashSet<_>>().len() == t.len()
    }

    #[test]
    fn nopeers_fallback_pushes_to_few_connected_members() {
        let (st, _) = crew(200);
        let all = devices(&st);
        let me = all[0].1;
        let salt = EventId([1; 32]);
        // 연결된 멤버 20곳(나 포함)과 멤버가 아닌 피어(보관 노드 등) 하나: 연결된 멤버 기기만 8곳.
        let mut live = connected(&all[..20]);
        live.push((PeerId::random(), UserId::random(), DeviceId::random()));
        let got = nopeers(&st, me, &live, &salt);
        assert_eq!(got.len(), NOPEERS_FANOUT);
        let live_set: HashSet<DeviceId> = all[1..20].iter().map(|(_, d)| *d).collect();
        assert!(
            got.iter().all(|(_, d)| live_set.contains(d)),
            "연결된 멤버 기기만"
        );
        // 연결된 곳이 적으면 그곳 모두에 더해 나머지를 멤버 기기에서 채웁니다(모두 합쳐 8곳, 나는 빼고).
        let got = nopeers(&st, me, &connected(&all[..4]), &salt);
        assert_eq!(got.len(), NOPEERS_FANOUT);
        assert!(all[1..4].iter().all(|t| got.contains(t)));
        assert!(got.iter().all(|t| all.contains(t) && t.1 != me));
        assert!(distinct(&got));
    }

    #[test]
    fn nopeers_fallback_without_connections_dials_few_deterministically() {
        let (st, _) = crew(200);
        let all = devices(&st);
        let me = all[0].1;
        let a = nopeers(&st, me, &[], &EventId([1; 32]));
        assert_eq!(a.len(), NOPEERS_FANOUT);
        assert!(a.iter().all(|t| all.contains(t) && t.1 != me));
        assert!(distinct(&a));
        assert_eq!(a, nopeers(&st, me, &[], &EventId([1; 32])));
        // 이벤트가 다르면 다른 곳으로 퍼집니다.
        assert_ne!(a, nopeers(&st, me, &[], &EventId([2; 32])));
        // 나 말고 멤버가 적으면 있는 만큼만.
        let (small, _) = crew(5);
        let s = devices(&small);
        let got = nopeers(&small, s[0].1, &[], &EventId([1; 32]));
        assert_eq!(got.len(), 4);
        assert!(distinct(&got) && got.iter().all(|t| t.1 != s[0].1));
    }

    /// (상태, 채널, 볼 수 없는 사용자, 그 채널의 캐시 값, 캐시).
    type RestrictedCrew = (
        Arc<SpaceState>,
        ChannelId,
        HashSet<UserId>,
        Arc<ChannelView>,
        Arc<StateIndex>,
    );

    /// `blocked(i)`인 `i`번째 멤버(기기 하나씩, 0번이 나)에게 채널을 막은 크루와 그 캐시 값. 덮어쓰기는 적은 쪽으로
    /// 겁니다(볼 수 있는 사람이 적으면 @everyone을 막고 그 사람들만 엶).
    fn restricted_crew(n: usize, blocked: impl Fn(usize) -> bool) -> RestrictedCrew {
        let (mut st, ch) = crew(n);
        let users: Vec<UserId> = devices(&st).into_iter().map(|(u, _)| u).collect();
        let mut open: Vec<(usize, UserId)> = users.iter().copied().enumerate().collect();
        let blocked: Vec<(usize, UserId)> = open.extract_if(.., |(i, _)| blocked(*i)).collect();
        if open.len() < blocked.len() {
            overwrite(
                &mut st,
                &ch,
                OverwriteTarget::Role(RoleId::EVERYONE),
                Permissions::NONE,
                Permissions::VIEW_CHANNEL,
            );
            for (_, u) in &open {
                overwrite(
                    &mut st,
                    &ch,
                    OverwriteTarget::Member(*u),
                    Permissions::VIEW_CHANNEL,
                    Permissions::NONE,
                );
            }
        } else {
            for (_, u) in &blocked {
                deny_view(&mut st, &ch, *u);
            }
        }
        let blocked: HashSet<UserId> = blocked.into_iter().map(|(_, u)| u).collect();
        let st = Arc::new(st);
        let cache = StateIndexes::default().get(&SpaceId::default(), &st);
        let view = cache.channel(&ch, DIRECT_VIEWERS_MAX + 1);
        (st, ch, blocked, view, cache)
    }

    fn restricted(
        st: &Arc<SpaceState>,
        me: DeviceId,
        view: &ChannelView,
        ch: &ChannelId,
        cache: &StateIndex,
        live: &[(PeerId, UserId, DeviceId)],
        salt: &EventId,
    ) -> Vec<(UserId, DeviceId)> {
        restricted_targets(st, me, view, ch, DIRECT_VIEWERS_MAX, live, salt, || {
            cache.devices()
        })
    }

    #[test]
    fn restricted_push_is_capped_and_never_reaches_non_viewers() {
        // 600명 중 짝수 번째(나 빼고)는 볼 수 없음: 볼 수 있는 기기 300대 > 256이라 줄여 고릅니다.
        let (st, ch, blocked, view, cache) = restricted_crew(600, |i| i % 2 == 0 && i > 0);
        let all = devices(&st);
        let me = all[0].1;
        assert!(!view.everyone && view.few.is_none() && view.mask.is_some());
        let salt = EventId([3; 32]);
        let viewer = |t: &(UserId, DeviceId)| !blocked.contains(&t.0) && t.1 != me;
        // 연결된 곳에 볼 수 없는 멤버가 섞여 있어도 볼 수 있는 기기만, 8곳. 연결된 곳에서는 절반까지만 고르고 나머지는
        // 연결과 무관하게 고릅니다(연결이 없는 기기도 직접 받을 차례가 옴).
        let live = connected(&all[..60]);
        let got = restricted(&st, me, &view, &ch, &cache, &live, &salt);
        assert_eq!(got.len(), NOPEERS_FANOUT);
        assert!(got.iter().all(viewer));
        assert!(distinct(&got));
        let from_live = got.iter().filter(|t| all[..60].contains(t)).count();
        assert!(
            from_live >= NOPEERS_FANOUT / 2,
            "연결된 볼 수 있는 기기 먼저"
        );
        // 연결된 곳이 볼 수 없는 멤버뿐이어도 볼 수 있는 기기 8곳에 다이얼합니다.
        let only_blocked: Vec<(UserId, DeviceId)> = all
            .iter()
            .filter(|t| blocked.contains(&t.0))
            .copied()
            .collect();
        let got = restricted(
            &st,
            me,
            &view,
            &ch,
            &cache,
            &connected(&only_blocked),
            &salt,
        );
        assert_eq!(got.len(), NOPEERS_FANOUT);
        assert!(got.iter().all(viewer));
        // 볼 수 없는 멤버가 대부분이어도(3,000대 중 볼 수 있는 기기 258대, 하나는 나) 볼 수 없는 기기는 고르지
        // 않습니다.
        let (st, ch, blocked, view, cache) = restricted_crew(3000, |i| i % 11 != 0 || i > 2827);
        let me = devices(&st)[0].1;
        assert_eq!(view.viewer_devices, 258);
        assert!(view.mask.is_some());
        for k in 0..200u8 {
            let got = restricted(&st, me, &view, &ch, &cache, &[], &EventId([k; 32]));
            assert_eq!(got.len(), NOPEERS_FANOUT);
            assert!(got.iter().all(|t| !blocked.contains(&t.0) && t.1 != me));
        }
    }

    #[test]
    fn restricted_push_to_viewers_under_the_cap_reaches_all_of_them() {
        // 100대 크루의 직원 채널(볼 수 있는 기기 15대)에서 연결은 한 곳뿐이어도 볼 수 있는 기기 모두(나 빼고)에게
        // 보냅니다: 볼 수 없는 멤버는 이 채널을 내주지 않으므로 연결된 곳에만 보내면 나머지가 따라잡을 길이 없습니다.
        let (st, ch, blocked, view, cache) = restricted_crew(100, |i| i >= 15);
        let all = devices(&st);
        let me = all[0].1;
        assert_eq!(view.viewer_devices, 15);
        let mut got = restricted(
            &st,
            me,
            &view,
            &ch,
            &cache,
            &connected(&all[1..2]),
            &EventId([4; 32]),
        );
        got.sort();
        let mut want: Vec<(UserId, DeviceId)> = all[1..15]
            .iter()
            .filter(|t| !blocked.contains(&t.0))
            .copied()
            .collect();
        want.sort();
        assert_eq!(want.len(), 14);
        assert_eq!(got, want);
        // 상한(256대)까지는 모두에게: 나를 빼면 꼭 256대.
        let (big, big_ch, _, big_view, big_cache) = restricted_crew(400, |i| i > 256);
        let big_me = devices(&big)[0].1;
        let got = restricted(
            &big,
            big_me,
            &big_view,
            &big_ch,
            &big_cache,
            &[],
            &EventId([5; 32]),
        );
        assert_eq!(got.len(), DIRECT_VIEWERS_MAX);
        assert!(distinct(&got) && got.iter().all(|t| t.1 != big_me));
        // 목록이 있어도 나를 뺀 수가 상한을 넘으면 그 목록 안에서 줄여 고릅니다.
        let few = ChannelView {
            everyone: false,
            viewer_devices: 6,
            few: Some(all[..6].to_vec().into()),
            mask: None,
        };
        let got = restricted_targets(&st, me, &few, &ch, 3, &[], &EventId([6; 32]), || {
            unreachable!("목록이 있으면 기기 목록을 만들지 않음")
        });
        assert_eq!(got.len(), 5);
        assert!(got.iter().all(|t| all[1..6].contains(t)));
    }

    #[test]
    fn every_viewer_is_eventually_picked_even_without_a_connection() {
        // 볼 수 있는 기기 300대 중 연결된 곳은 6곳뿐: 이벤트마다 자리를 바꿔 고르므로 연결이 없는 볼 수 있는 기기도 모두
        // 언젠가 직접 받습니다(받으면 보낸 쪽에서 빈 부모를 따라잡음).
        let (st, ch, blocked, view, cache) = restricted_crew(450, |i| i % 3 == 1);
        let all = devices(&st);
        let me = all[0].1;
        let viewers: HashSet<DeviceId> = all
            .iter()
            .filter(|t| !blocked.contains(&t.0) && t.1 != me)
            .map(|(_, d)| *d)
            .collect();
        assert!(viewers.len() > DIRECT_VIEWERS_MAX);
        let live: Vec<(UserId, DeviceId)> = all
            .iter()
            .filter(|t| viewers.contains(&t.1))
            .take(6)
            .copied()
            .collect();
        let live = connected(&live);
        let mut seen = HashSet::new();
        for k in 0..4000u32 {
            let mut salt = [0u8; 32];
            salt[..4].copy_from_slice(&k.to_le_bytes());
            let salt = EventId(*blake3::hash(&salt).as_bytes());
            for (_, d) in restricted(&st, me, &view, &ch, &cache, &live, &salt) {
                assert!(viewers.contains(&d));
                seen.insert(d);
            }
        }
        assert_eq!(seen.len(), viewers.len(), "굶는 볼 수 있는 기기가 없음");
    }

    #[test]
    fn gossip_chunks_fit_message_limit() {
        // 본문이 작은 이벤트를 개수 한도보다 많이: 혼합 서명(공개키 포함)까지 실어도 묶음마다 메시지 한도 안이어야 함.
        let dev = SigningKeypair::generate();
        let user = SigningKeypair::generate();
        let space = SpaceId::random();
        let header = EventHeader {
            v: EVENT_VERSION,
            space,
            channel: Some(ChannelId::random()),
            kind: EventKind::Message,
            author: DeviceId(dev.public()),
            user: UserId(user.public()),
            parents: (0..MAX_PARENTS as u8).map(|i| EventId([i; 32])).collect(),
            lamport: 1,
            created_at: Timestamp::now(),
            key: KeyRef::Record {
                root: ChannelRootId::random(),
            },
            body_hash: [0; 32],
            body_len: 0,
            scope: None,
            state_anchor: Vec::new(),
        };
        let ev = SignedEvent::sign(header, vec![7; 64], &dev).unwrap();
        let events = vec![ev; MAX_EVENTS_PER_MESSAGE * 2];
        let chunks = gossip_chunks(events);
        assert!(chunks.len() >= 2);
        for chunk in chunks {
            let msg = yangpa_core::wire::GossipMessage::Events {
                space,
                events: chunk,
            };
            let bytes = postcard::to_stdvec(&msg).unwrap();
            assert!(
                bytes.len() <= yangpa_net::MAX_GOSSIP_MESSAGE,
                "{}",
                bytes.len()
            );
        }
    }

    /// 따라잡기 묶음 모으기: 같은 이벤트는 한 번만, 묶음 4개나 크기 한도면 적용할 때, 모아 둔 것은 아는 것으로 셈.
    /// 빈 부모 채우기(`Pull::Gap`)는 작성 기기별 제한을 묶음마다 확인해야 하므로 모으지 않음.
    #[test]
    fn pull_buffer_dedups_and_becomes_due_after_four_batches() {
        let dev = SigningKeypair::generate();
        let user = SigningKeypair::generate();
        let space = SpaceId::random();
        let channel = ChannelId::random();
        let ev = |lamport: u64| {
            let header = EventHeader {
                v: EVENT_VERSION,
                space,
                channel: Some(channel),
                kind: EventKind::Message,
                author: DeviceId(dev.public()),
                user: UserId(user.public()),
                parents: Vec::new(),
                lamport,
                created_at: Timestamp::now(),
                key: KeyRef::Record {
                    root: ChannelRootId::random(),
                },
                body_hash: [0; 32],
                body_len: 0,
                scope: None,
                state_anchor: Vec::new(),
            };
            SignedEvent::sign(header, vec![1; 8], &dev).unwrap()
        };
        let (a, b) = (ev(1), ev(2));
        let mut buf = PullBuffer::new(Pull::Full);
        assert!(!buf.due());
        buf.push(vec![a.clone(), b.clone()]);
        buf.push(vec![a.clone()]);
        assert_eq!(buf.events.len(), 2, "같은 이벤트는 한 번만");
        assert_eq!(
            buf.bytes,
            2 * (8 + PULL_EVENT_OVERHEAD),
            "겹친 것은 크기에도 한 번만"
        );
        assert!(buf.ids.contains(&a.id()) && buf.ids.contains(&b.id()));
        assert!(!buf.due());
        buf.push(Vec::new());
        buf.push(Vec::new());
        assert!(buf.due(), "묶음 4개");
        let mut late = PullBuffer::new(Pull::Full);
        if let Some(t) = late.since.checked_sub(PULL_FLUSH_EVERY) {
            late.since = t;
            assert!(late.due(), "2초가 지남");
        }
        let mut big = PullBuffer::new(Pull::Full);
        big.bytes = PULL_FLUSH_BYTES - PULL_EVENT_OVERHEAD;
        assert!(!big.due());
        big.push(vec![a.clone()]);
        assert!(big.due(), "묶음 하나라도 크기 한도를 넘으면");
        let mut gap = PullBuffer::new(Pull::Gap);
        gap.push(vec![a.clone()]);
        assert!(gap.due(), "빈 부모 채우기는 묶음마다");
    }
}
