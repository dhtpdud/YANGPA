//! 크루·채널·사람별 올리기·받기 속도 제한(이 기기만의 설정), 그리고 실시간 전파로 받은 상태·키 갱신 이벤트의 작성 기기별
//! 제한([`EventLimiter`]).
//!
//! 전체 제한(`seed.set`의 `upload_limit_bps`, `peer.set`의 `download_limit_bps`)과 함께, 걸리는 범위마다 토큰 버킷을
//! 둡니다. 보내거나 요청하려면 걸리는 버킷이 모두 넉넉해야 하므로 실제 속도는 가장 낮은 제한을 따릅니다.
//! - 올리기(시더): 남의 요청에 답할 때(`sync_service::check`). 범위는 요청한 공간·채널·사람.
//! - 받기: 첨부 조각을 요청할 때(`blobs::admit_chunks`). 범위는 첨부의 공간·채널과 조각을 줄 사람.
//! - 0은 그 방향을 막습니다: 올리기는 내주지 않고([`blocked`]), 받기는 첨부를 자동으로도 직접으로도 받지 않습니다.
//!
//! 저장: 설정 `rate.limits`(JSON, 범위 ID와 숫자만). 이벤트 제한은 설정이 없고 RAM에만 둡니다.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Instant;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use yangpa_core::blob::MIN_CHUNK_SIZE;
use yangpa_core::ids::{ChannelId, DeviceId, SpaceId, UserId};
use yangpa_core::model::{Permissions, SpaceState};
use yangpa_net::PeerId;
use yangpa_rpc::api::{RateLimitView, RateScope};
use yangpa_sync::policy::TokenBucket;

use crate::error::{Error, Result};
use crate::node::Node;

const KEY: &str = "rate.limits";
/// 저장하는 제한 수 상한(설정 값 크기를 묶음).
pub(crate) const MAX_LIMITS: usize = 1024;

/// 한 범위의 제한(바이트/초, `None`은 그 방향 제한 없음).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Limit {
    #[serde(default)]
    pub up: Option<u64>,
    #[serde(default)]
    pub down: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Dir {
    Up,
    Down,
}

impl Limit {
    fn rate(&self, dir: Dir) -> Option<u64> {
        match dir {
            Dir::Up => self.up,
            Dir::Down => self.down,
        }
    }
}

/// 저장 모양: 범위를 JSON 키로 쓸 수 있게 목록으로 둡니다.
#[derive(Default, Serialize, Deserialize)]
struct Stored {
    #[serde(default)]
    limits: Vec<(RateScope, Limit)>,
}

#[derive(Default)]
pub(crate) struct RateLimitState {
    /// 읽어 둔 설정(처음 쓸 때 읽음).
    limits: Mutex<Option<Arc<BTreeMap<RateScope, Limit>>>>,
    up: Mutex<HashMap<RateScope, TokenBucket>>,
    down: Mutex<HashMap<RateScope, TokenBucket>>,
    /// 실시간 전파로 받은 상태·키 갱신 이벤트의 작성 기기별 버킷.
    pub(crate) events: Mutex<EventLimiter>,
}

fn limits(node: &Node) -> Arc<BTreeMap<RateScope, Limit>> {
    let mut g = node.ratelimit.limits.lock();
    if let Some(l) = g.as_ref() {
        return l.clone();
    }
    let stored: Stored = node
        .store
        .get_setting(KEY)
        .ok()
        .flatten()
        .unwrap_or_default();
    let l = Arc::new(stored.limits.into_iter().collect::<BTreeMap<_, _>>());
    *g = Some(l.clone());
    l
}

/// `rate.limits`.
pub(crate) fn list(node: &Node) -> Vec<RateLimitView> {
    limits(node)
        .iter()
        .map(|(scope, l)| RateLimitView {
            scope: *scope,
            up_bps: l.up,
            down_bps: l.down,
        })
        .collect()
}

/// `rate.set_limit`: 범위의 제한을 바꾸고(둘 다 없으면 지움) 저장합니다.
/// 0은 그 방향을 막습니다(올리기 0 = 버킷이 늘 기다리라고 답해 내주지 않음, 받기 0 = 첨부를 받지 않음).
pub(crate) fn set(node: &Node, scope: RateScope, limit: Limit) -> Result<()> {
    let mut next = (*limits(node)).clone();
    if limit == Limit::default() {
        next.remove(&scope);
    } else {
        if !next.contains_key(&scope) && next.len() >= MAX_LIMITS {
            return Err(Error::invalid(format!(
                "속도 제한은 {MAX_LIMITS}개까지입니다"
            )));
        }
        next.insert(scope, limit);
    }
    let stored = Stored {
        limits: next.iter().map(|(s, l)| (*s, *l)).collect(),
    };
    node.store.put_setting(KEY, &stored)?;
    *node.ratelimit.limits.lock() = Some(Arc::new(next));
    // 바뀐 범위의 버킷은 새 속도로 다시 만듭니다.
    node.ratelimit.up.lock().remove(&scope);
    node.ratelimit.down.lock().remove(&scope);
    Ok(())
}

/// 공간이 사라지면(탈퇴·추방) 그 공간과 채널의 제한, 그 공간의 이벤트 버킷을 지웁니다. 사람별 제한은 남깁니다.
pub(crate) fn forget_space(node: &Node, space: &SpaceId) {
    node.ratelimit.events.lock().forget_space(space);
    let l = limits(node);
    let gone: Vec<RateScope> = l
        .keys()
        .filter(|s| match s {
            RateScope::Crew { space: sp } | RateScope::Channel { space: sp, .. } => sp == space,
            RateScope::User { .. } => false,
        })
        .copied()
        .collect();
    for s in gone {
        let _ = set(node, s, Limit::default());
    }
}

/// 이 전송에 걸리는 범위와 그 속도.
fn applicable(
    node: &Node,
    dir: Dir,
    space: &SpaceId,
    channel: Option<&ChannelId>,
    user: Option<&UserId>,
) -> Vec<(RateScope, u64)> {
    let l = limits(node);
    if l.is_empty() {
        return Vec::new();
    }
    let mut scopes = vec![RateScope::Crew { space: *space }];
    if let Some(c) = channel {
        scopes.push(RateScope::Channel {
            space: *space,
            channel: *c,
        });
    }
    if let Some(u) = user {
        scopes.push(RateScope::User { user: *u });
    }
    scopes
        .into_iter()
        .filter_map(|s| l.get(&s).and_then(|x| x.rate(dir)).map(|r| (s, r)))
        .collect()
}

/// 걸리는 범위 중 하나라도 이 방향을 0으로 막았는지.
pub(crate) fn blocked(
    node: &Node,
    dir: Dir,
    space: &SpaceId,
    channel: Option<&ChannelId>,
    user: Option<&UserId>,
) -> bool {
    applicable(node, dir, space, channel, user)
        .iter()
        .any(|(_, r)| *r == 0)
}

/// 범위별 버킷: 버스트는 1초 분량(조각 하나보다 작아도 됨). 전체 제한의 버킷(버스트 최소 4MiB)처럼 두면 몇 MB짜리
/// 파일은 낮은 제한을 걸어도 한 번에 지나가 버립니다. 버스트보다 큰 조각은 버킷이 차 있을 때 보내고 빚으로 둡니다.
fn bucket(rate: u64, now: u64) -> TokenBucket {
    TokenBucket::with_burst(rate, rate.max(u64::from(MIN_CHUNK_SIZE)), now)
}

fn buckets(node: &Node, dir: Dir) -> &Mutex<HashMap<RateScope, TokenBucket>> {
    match dir {
        Dir::Up => &node.ratelimit.up,
        Dir::Down => &node.ratelimit.down,
    }
}

/// 걸리는 제한 안에서 `bytes`를 지금 보낼(받을) 수 있으면 0, 아니면 다시 해 볼 때까지의 밀리초(가장 긴 것).
/// 쓰지 않습니다.
pub(crate) fn wait_ms(
    node: &Node,
    dir: Dir,
    space: &SpaceId,
    channel: Option<&ChannelId>,
    user: Option<&UserId>,
    bytes: u64,
    now: u64,
) -> u32 {
    let scopes = applicable(node, dir, space, channel, user);
    if scopes.is_empty() {
        return 0;
    }
    let mut g = buckets(node, dir).lock();
    scopes
        .into_iter()
        .map(|(s, rate)| {
            g.entry(s)
                .or_insert_with(|| bucket(rate, now))
                .retry_after_ms(bytes, now)
        })
        .max()
        .unwrap_or(0)
}

/// 걸리는 제한마다 `bytes`를 씁니다([`wait_ms`]가 0이었을 때).
pub(crate) fn consume(
    node: &Node,
    dir: Dir,
    space: &SpaceId,
    channel: Option<&ChannelId>,
    user: Option<&UserId>,
    bytes: u64,
    now: u64,
) {
    let scopes = applicable(node, dir, space, channel, user);
    if scopes.is_empty() {
        return;
    }
    let mut g = buckets(node, dir).lock();
    for (s, rate) in scopes {
        let b = g.entry(s).or_insert_with(|| bucket(rate, now));
        // 방금 확인했으니 넘쳐도 빚으로 둡니다(다음 요청이 그만큼 기다림).
        if b.try_consume(bytes, now).is_err() {
            b.force_consume(bytes, now);
        }
    }
}

// ───────────── 받은 상태·키 갱신 이벤트: 작성 기기별 제한 ─────────────
//
// 상태 이벤트는 받을 때마다 해소기를 다시 돌리고 gossip으로 바로 온 크루에 퍼지므로, 큰 크루(활성 기기가
// `NodeConfig::event_limit_devices`를 넘음)에서 한 기기가 쏟아내는 상태·키 갱신 이벤트를 곧바로 받는 길(gossip,
// `PushEvents`, 그렇게 받은 이벤트의 빈 부모를 채우는 따라잡기 `sync_service::Pull::Gap`)에서 작성 기기마다 한 버킷으로
// 묶습니다. 넘친 것은 버리기만 하고(gossip은 더 퍼뜨리지 않고, 전달한 피어 잘못이 아님) 제한 없는 따라잡기
// (연결·인증 직후, `sync_interval`마다)가 묶음으로 가져옵니다. 그래서 이 제한은 이벤트 수를 막지 않고, 넘친 것을
// 그 주기로 미뤄 한꺼번에 받게 합니다.

/// 작성 기기마다 몰아서 받는 상태·키 갱신 이벤트 수.
pub(crate) const EVENT_BURST: u64 = 30;
/// 초대 증명이 맞는 `MemberJoin`을 몰아서 받는 수(초대를 받아 주는 멤버는 가입을 한꺼번에 냅니다).
pub(crate) const JOIN_BURST: u64 = 200;
/// 버킷을 채우는 속도(분당 이벤트 수).
pub(crate) const EVENTS_PER_MINUTE: u64 = 10;
/// 버킷 표 상한. 새 기기 ID를 마구 만들어도 메모리가 늘지 않게 합니다.
pub(crate) const MAX_EVENT_BUCKETS: usize = 4096;
/// 이벤트 하나의 토큰 수. `TokenBucket`은 초당 정수 속도라 '분당 10개'를 '초당 1토큰, 이벤트 하나에 6토큰'으로 셉니다.
const TOKENS_PER_EVENT: u64 = 60 / EVENTS_PER_MINUTE;
/// 관리 권한: 이 중 하나라도 있으면 제한하지 않습니다(밴 물결·대량 관리 작업).
const MODERATION: [Permissions; 5] = [
    Permissions::KICK_MEMBERS,
    Permissions::BAN_MEMBERS,
    Permissions::MANAGE_ROLES,
    Permissions::MANAGE_CHANNELS,
    Permissions::ADMINISTRATOR,
];

/// 버킷 주인. 지금 상태에서 멤버의 활성 기기가 아닌 작성 기기(아직 모르는 가입자, 지어낸 기기)의 이벤트는 그것을
/// 넘겨준 피어의 몫으로 셉니다: 새 기기 ID를 만들 때마다 새 버스트를 얻지 못하고, 한 멤버가 지어낸 기기로 공용 버킷을
/// 비워 다른 길로 온 새 기기의 이벤트까지 막지도 못하게.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum EventAuthor {
    Device(DeviceId),
    /// 모르는 작성 기기의 이벤트를 넘겨준 피어(gossip을 전달한 피어, `PushEvents`·따라잡기 상대).
    Relay(PeerId),
    /// 넘겨준 피어도 모를 때(실시간 전파에는 늘 피어가 있어 쓰이지 않음).
    Unknown,
}

/// 어느 버킷에서 쓸지.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EventClass {
    Base,
    /// 초대 증명을 확인한 `MemberJoin`.
    Join,
}

/// 제한 없이 받는 작성 기기: 지금 확정 기기, 소유자의 기기, 관리 권한(추방·밴·역할 관리·채널 관리·관리자)이 있는
/// 멤버의 기기.
pub(crate) fn event_exempt(state: &SpaceState, user: &UserId, device: &DeviceId) -> bool {
    if *device == state.committer {
        return true;
    }
    if !(state.is_member(user) || *user == state.owner) || !state.device_belongs(user, device) {
        return false;
    }
    let p = state.base_permissions(user);
    MODERATION.iter().any(|m| p.contains(*m))
}

/// 버킷 주인(멤버의 활성 기기면 그 기기, 아니면 넘겨준 피어).
pub(crate) fn event_author(
    state: &SpaceState,
    user: &UserId,
    device: &DeviceId,
    via: Option<PeerId>,
) -> EventAuthor {
    if state.is_member(user) && state.device_belongs(user, device) {
        EventAuthor::Device(*device)
    } else {
        via.map_or(EventAuthor::Unknown, EventAuthor::Relay)
    }
}

struct AuthorBuckets {
    base: TokenBucket,
    /// 처음 쓸 때 만듭니다(가입을 받아 주지 않는 기기는 없음).
    join: Option<TokenBucket>,
    last_ms: u64,
}

impl AuthorBuckets {
    /// 가득 찼으면 지워도 새로 만든 것과 같습니다.
    fn idle(&mut self, now_ms: u64) -> bool {
        full(&mut self.base, now_ms) && self.join.as_mut().is_none_or(|j| full(j, now_ms))
    }
}

fn full(b: &mut TokenBucket, now_ms: u64) -> bool {
    b.available(now_ms) >= b.burst()
}

fn bucket_for(burst_events: u64, now_ms: u64) -> TokenBucket {
    TokenBucket::with_burst(1, burst_events * TOKENS_PER_EVENT, now_ms)
}

/// 작성 기기별 상태·키 갱신 이벤트 버킷(공간마다 따로).
pub(crate) struct EventLimiter {
    start: Instant,
    buckets: HashMap<(SpaceId, EventAuthor), AuthorBuckets>,
}

impl Default for EventLimiter {
    fn default() -> Self {
        EventLimiter {
            start: Instant::now(),
            buckets: HashMap::new(),
        }
    }
}

impl EventLimiter {
    /// 버킷 시계(단조 증가, 밀리초).
    pub(crate) fn now_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }

    /// 이벤트 하나를 받아도 되면 토큰을 쓰고 true.
    pub(crate) fn try_take(
        &mut self,
        space: SpaceId,
        author: EventAuthor,
        class: EventClass,
        now_ms: u64,
    ) -> bool {
        let b = self.slot(space, author, now_ms);
        b.last_ms = b.last_ms.max(now_ms);
        let bucket = match class {
            EventClass::Base => &mut b.base,
            EventClass::Join => b.join.get_or_insert_with(|| bucket_for(JOIN_BURST, now_ms)),
        };
        bucket.try_consume(TOKENS_PER_EVENT, now_ms).is_ok()
    }

    pub(crate) fn forget_space(&mut self, space: &SpaceId) {
        self.buckets.retain(|(s, _), _| s != space);
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.buckets.len()
    }

    fn slot(&mut self, space: SpaceId, author: EventAuthor, now_ms: u64) -> &mut AuthorBuckets {
        let key = (space, author);
        if !self.buckets.contains_key(&key) && self.buckets.len() >= MAX_EVENT_BUCKETS {
            // 가득 찬(한동안 쓰지 않은) 버킷은 지워도 결과가 같습니다. 그래도 넘치면 가장 오래 쓰지 않은 것부터.
            self.buckets.retain(|_, b| !b.idle(now_ms));
            while self.buckets.len() >= MAX_EVENT_BUCKETS {
                let Some(oldest) = self
                    .buckets
                    .iter()
                    .min_by_key(|(_, b)| b.last_ms)
                    .map(|(k, _)| *k)
                else {
                    break;
                };
                self.buckets.remove(&oldest);
            }
        }
        self.buckets.entry(key).or_insert_with(|| AuthorBuckets {
            base: bucket_for(EVENT_BURST, now_ms),
            join: None,
            last_ms: now_ms,
        })
    }
}

/// 이 기기의 데이터를 지울 때 이벤트 버킷도 비웁니다.
pub(crate) fn wipe_events(node: &Node) {
    *node.ratelimit.events.lock() = EventLimiter::default();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_shape_round_trips() {
        let s = Stored {
            limits: vec![
                (
                    RateScope::Crew {
                        space: SpaceId([1; 16]),
                    },
                    Limit {
                        up: Some(1024),
                        down: None,
                    },
                ),
                (
                    RateScope::User {
                        user: UserId([2; 32]),
                    },
                    Limit {
                        up: None,
                        down: Some(2048),
                    },
                ),
            ],
        };
        let v = serde_json::to_value(&s).unwrap();
        let back: Stored = serde_json::from_value(v).unwrap();
        assert_eq!(back.limits, s.limits);
    }

    const SIX_SECS: u64 = 6_000;

    fn dev() -> EventAuthor {
        EventAuthor::Device(DeviceId::random())
    }

    #[test]
    fn event_bucket_allows_burst_then_refills_ten_per_minute() {
        let mut l = EventLimiter::default();
        let (space, a) = (SpaceId::random(), dev());
        let t0 = 1_000;
        for i in 0..EVENT_BURST {
            assert!(
                l.try_take(space, a, EventClass::Base, t0),
                "{i}번째는 버스트 안"
            );
        }
        assert!(
            !l.try_take(space, a, EventClass::Base, t0),
            "버스트를 넘으면 버림"
        );
        // 다른 기기·다른 공간은 따로 셉니다.
        assert!(l.try_take(space, dev(), EventClass::Base, t0));
        assert!(l.try_take(SpaceId::random(), a, EventClass::Base, t0));
        // 6초에 하나씩 찹니다(분당 10개).
        assert!(!l.try_take(space, a, EventClass::Base, t0 + SIX_SECS - 1));
        assert!(l.try_take(space, a, EventClass::Base, t0 + SIX_SECS));
        assert!(!l.try_take(space, a, EventClass::Base, t0 + SIX_SECS));
        let t1 = t0 + SIX_SECS + 60_000;
        for _ in 0..EVENTS_PER_MINUTE {
            assert!(l.try_take(space, a, EventClass::Base, t1));
        }
        assert!(!l.try_take(space, a, EventClass::Base, t1));
    }

    #[test]
    fn invite_joins_get_their_own_larger_burst() {
        let mut l = EventLimiter::default();
        let (space, a) = (SpaceId::random(), dev());
        for _ in 0..EVENT_BURST {
            assert!(l.try_take(space, a, EventClass::Base, 0));
        }
        assert!(!l.try_take(space, a, EventClass::Base, 0));
        for i in 0..JOIN_BURST {
            assert!(l.try_take(space, a, EventClass::Join, 0), "{i}번째 가입");
        }
        assert!(!l.try_take(space, a, EventClass::Join, 0));
    }

    #[test]
    fn event_bucket_map_stays_bounded() {
        let mut l = EventLimiter::default();
        let space = SpaceId::random();
        // 쓰는 중인(가득 차지 않은) 기기로 표를 채웁니다.
        for _ in 0..MAX_EVENT_BUCKETS + 100 {
            assert!(l.try_take(space, dev(), EventClass::Base, 0));
        }
        assert!(l.len() <= MAX_EVENT_BUCKETS);
        // 오래 쓰지 않은 버킷(다시 가득 참)은 넘칠 때 먼저 지웁니다.
        let later = 60 * 60 * 1000;
        let fresh = dev();
        assert!(l.try_take(space, fresh, EventClass::Base, later));
        assert_eq!(l.len(), 1, "가득 찬 버킷은 모두 지움");
        // 공간을 지우면 그 공간의 버킷도 지웁니다.
        l.forget_space(&space);
        assert_eq!(l.len(), 0);
    }

    #[test]
    fn evicting_the_oldest_keeps_recent_authors() {
        let mut l = EventLimiter::default();
        let space = SpaceId::random();
        let first = dev();
        // 모두 버스트를 다 써서 가득 차려면 오래 걸리는 버킷들.
        for t in 0..MAX_EVENT_BUCKETS as u64 {
            let a = if t == 0 { first } else { dev() };
            for _ in 0..EVENT_BURST {
                l.try_take(space, a, EventClass::Base, t);
            }
        }
        assert_eq!(l.len(), MAX_EVENT_BUCKETS);
        let last = MAX_EVENT_BUCKETS as u64;
        assert!(l.try_take(space, dev(), EventClass::Base, last));
        assert_eq!(l.len(), MAX_EVENT_BUCKETS);
        // 가장 오래 쓰지 않은 `first`가 밀려나 새 버킷(가득 참)으로 다시 시작합니다.
        assert!(!l.buckets.contains_key(&(space, first)));
    }

    #[test]
    fn exemptions_cover_committer_owner_and_moderators() {
        use crate::state_index::tests_support::crew;
        use yangpa_core::ids::RoleId;
        use yangpa_core::model::Role;

        let (mut st, _) = crew(4);
        let users: Vec<UserId> = st.members.keys().copied().collect();
        let device_of = |st: &SpaceState, u: &UserId| st.devices[u].devices[0].device;
        // 평범한 멤버는 묶습니다.
        let plain = users[0];
        assert!(!event_exempt(&st, &plain, &device_of(&st, &plain)));
        assert_eq!(
            event_author(&st, &plain, &device_of(&st, &plain), None),
            EventAuthor::Device(device_of(&st, &plain))
        );
        // 지금 확정 기기.
        st.committer = device_of(&st, &users[1]);
        assert!(event_exempt(&st, &users[1], &st.committer.clone()));
        // 소유자의 기기.
        st.owner = users[2];
        assert!(event_exempt(&st, &users[2], &device_of(&st, &users[2])));
        // 관리 권한(추방)이 있는 역할.
        let mods = RoleId::random();
        st.roles.insert(
            mods,
            Role {
                id: mods,
                name: "운영진".into(),
                color: 0,
                rank: 10,
                permissions: Permissions::KICK_MEMBERS,
                automation: false,
            },
        );
        Arc::make_mut(st.members.get_mut(&users[3]).unwrap())
            .roles
            .push(mods);
        assert!(event_exempt(&st, &users[3], &device_of(&st, &users[3])));
        // 관리 권한이 있어도 그 사용자의 기기가 아니면 면제하지 않습니다(머리의 사용자만 적어 사칭).
        assert!(!event_exempt(&st, &users[3], &DeviceId::random()));
        // 멤버의 활성 기기가 아닌 작성자는 넘겨준 피어의 몫.
        let relay = PeerId::random();
        assert_eq!(
            event_author(&st, &UserId::random(), &DeviceId::random(), Some(relay)),
            EventAuthor::Relay(relay)
        );
        assert_eq!(
            event_author(&st, &plain, &DeviceId::random(), Some(relay)),
            EventAuthor::Relay(relay)
        );
        assert_eq!(
            event_author(&st, &plain, &DeviceId::random(), None),
            EventAuthor::Unknown
        );
        // 멤버의 기기는 어느 피어가 넘겨줘도 그 기기 몫.
        assert_eq!(
            event_author(&st, &plain, &device_of(&st, &plain), Some(relay)),
            EventAuthor::Device(device_of(&st, &plain))
        );
    }

    #[test]
    fn made_up_authors_only_drain_the_relay_that_sent_them() {
        // 한 멤버(의 피어)가 지어낸 기기로 이벤트를 쏟아내도 그 피어 몫만 비고, 다른 피어가 넘겨준 모르는 작성자의
        // 이벤트(막 들어온 가입자의 첫 이벤트 등)는 그대로 받습니다.
        let (st, _) = crate::state_index::tests_support::crew(3);
        let mut l = EventLimiter::default();
        let space = SpaceId::random();
        let (bad, good) = (PeerId::random(), PeerId::random());
        for _ in 0..EVENT_BURST * 3 {
            let a = event_author(&st, &UserId::random(), &DeviceId::random(), Some(bad));
            l.try_take(space, a, EventClass::Base, 0);
        }
        let a = event_author(&st, &UserId::random(), &DeviceId::random(), Some(bad));
        assert!(
            !l.try_take(space, a, EventClass::Base, 0),
            "지어낸 기기는 넘겨준 피어 몫에서 셈"
        );
        let a = event_author(&st, &UserId::random(), &DeviceId::random(), Some(good));
        assert!(
            l.try_take(space, a, EventClass::Base, 0),
            "다른 피어 몫은 그대로"
        );
        assert_eq!(l.len(), 2, "지어낸 기기마다 버킷을 만들지 않음");
    }
}
