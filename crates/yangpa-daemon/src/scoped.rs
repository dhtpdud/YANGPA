//! 부분 전달 이벤트(받은편지함·키 그룹)를 받는 쪽의 RAM 상태: 기준점 상태로 권한을 확인하기 전까지 두는 대기열,
//! 이미 거부한 이벤트 기억, 따라잡기 진행 표시. 판단은 [`crate::ingest`]가 적용 잠금 안에서 합니다.
//!
//! 확인 전 이벤트는 디스크에 두지 않으므로(키를 나눌 자격이 없는 기기가 보낸 것일 수 있음) 넘치거나 오래되면 버리고
//! 따라잡기로 다시 받습니다. 다만 따라잡기가 같은 이벤트를 매번 다시 내려받지 않게, 대기 중인 것과 결정론적으로 거부한
//! 것은 "가진 것"으로 셉니다([`ScopedQueue::is_known`]).
//!
//! 따라잡기가 읽는 기억(대기 중·거부한 ID, 따라잡기 진행)은 `SpaceCore`가 아닌 잎 잠금 [`ScopedSeen`]에 둡니다.
//! 따라잡기는 동기 지문 비교 안에서 이것을 읽는데, 적용(`ingest::run`)은 `SpaceCore`를 적용 내내 쥐므로 거기서 읽으면
//! 큰 크루의 긴 적용 동안 비동기 작업 스레드가 멈춥니다.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use yangpa_core::event::SignedEvent;
use yangpa_core::ids::{EventId, SpaceId};
use yangpa_core::model::SpaceState;

/// 기다리는 부분 전달 이벤트 상한(공간마다): 개수와 대략의 바이트. 넘치면 오래된 것부터 버립니다.
pub(crate) const MAX_PENDING: usize = 256;
pub(crate) const MAX_PENDING_BYTES: usize = 16 * 1024 * 1024;
/// 작성 기기 하나가 차지할 수 있는 몫. 넘치면 그 기기의 가장 오래된 것부터 버립니다(한 기기가 남의 가입 묶음을 밀어내지
/// 못하게). 큰 크루의 가입 묶음(키 9,000개면 조각 141개)은 들어갑니다.
pub(crate) const MAX_PENDING_PER_AUTHOR: usize = MAX_PENDING * 3 / 4;
/// 기준점 조상의 빈 구간(받지 못한 부모·키를 기다리는 상태 이벤트)을, 이벤트가 온 뒤 시작한 따라잡기 한 바퀴가 끝나지
/// 않았어도 이만큼 기다렸으면 빈 구간으로 보고 검사합니다(따라잡을 피어가 없을 때).
pub(crate) const GAP_GRACE: Duration = Duration::from_secs(120);
/// 빈 구간을 허용해도 기준점을 하나도 모르면 이만큼 기다린 뒤 버립니다(따라잡기로 다시 받음).
pub(crate) const PENDING_TTL: Duration = Duration::from_secs(600);
/// 다시 셀 때가 됐는지 볼 막힌 이벤트 수(이벤트마다). 이 가운데 하나라도 풀리면 다시 셉니다.
pub(crate) const MAX_BLOCKERS: usize = 16;
/// 거부를 기억하는 이벤트 수.
const MAX_REJECTED: usize = 4096;
/// 빈 구간 없이 센 기준점 상태를 기억하는 수.
const MAX_ANCHOR_CACHE: usize = 8;

/// 권한 확인을 기다리는 부분 전달 이벤트.
pub(crate) struct PendingScoped {
    pub(crate) ev: SignedEvent,
    pub(crate) id: EventId,
    pub(crate) since: Instant,
    /// 가입·기기 연결 중 그때 믿는 피어에게서 받았는지(`ingest`의 `trusted_bootstrap`): 그러면 기준점 상태만으로 확인합니다.
    pub(crate) trusted: bool,
    /// 마지막으로 셌을 때 막고 있던 이벤트(`true`면 모르는 이벤트, `false`면 본문을 기다리는 상태 이벤트). 비었으면 아직
    /// 세지 않았음.
    pub(crate) blockers: Vec<(EventId, bool)>,
    /// 마지막으로 셀 때 빈 구간을 허용했는지.
    pub(crate) gaps_tried: bool,
}

/// 공간마다 하나(`spaces::SpaceCore::scoped`).
#[derive(Default)]
pub(crate) struct ScopedQueue {
    pub(crate) pending: Vec<PendingScoped>,
    /// `pending`의 대략적인 바이트 수.
    pub(crate) bytes: usize,
    /// 마지막으로 `pending`을 검사했을 때 해소기의 갱신 횟수(그대로면 다시 볼 까닭이 없음, `u64::MAX`면 꼭 다시 봄).
    pub(crate) checked_at: u64,
    /// 따라잡기가 읽는 기억(잎 잠금, `SpaceRuntime::scoped_seen`과 같은 것). `pending`과 늘 같은 ID를 담습니다.
    seen: Arc<Mutex<ScopedSeen>>,
    /// 유예 시각에 다시 보는 작업이 돌고 있음.
    pub(crate) timer: bool,
    /// 빈 구간 없이 센 기준점 상태(기준점, 해소기 재해소 횟수, 상태). 조상 집합이 해시로 고정돼 이어 붙이는 해소로는
    /// 바뀌지 않으므로, 본문을 받은 순서 연결 등으로 다시 해소하기 전까지 그대로 씁니다.
    anchor_cache: VecDeque<(Vec<EventId>, u64, Option<Arc<SpaceState>>)>,
}

/// 따라잡기가 적용 잠금 없이 읽는 부분 전달 기억(공간마다 하나, 잎 잠금: 쥔 채로 다른 잠금을 잡지 않음).
#[derive(Default)]
pub(crate) struct ScopedSeen {
    /// 결정론적으로 거부한 이벤트(빈 구간 없는 기준점 상태·지금 상태로 자격이 없음, 받을 까닭이 없음). 따라잡기가 다시
    /// 내려받지 않고, 다시 와도 검사하지 않습니다.
    rejected: RecentIds,
    /// 대기 중인 이벤트와 받은 시각([`ScopedQueue::pending`]과 같은 ID).
    pending: HashMap<EventId, Instant>,
    /// 끝난 따라잡기 한 바퀴(공간 스트림과 받은편지함) 가운데 가장 늦게 시작한 것의 시작 시각. 이보다 먼저 온 대기
    /// 이벤트는 받을 수 있는 것을 다 받아 본 것이므로 남은 빈 구간을 빈 구간으로 보고 검사합니다.
    caught_up: Option<Instant>,
}

impl ScopedSeen {
    /// 대기 중이거나 거부를 기억하는 이벤트인지(따라잡기가 이미 가진 것으로 셈).
    pub(crate) fn is_known(&self, id: &EventId) -> bool {
        self.rejected.contains(id) || self.pending.contains_key(id)
    }

    /// 따라잡기 한 바퀴가 끝났음을 적고(`started`에 시작), 그 전에 온 대기 이벤트가 있는지 돌려줍니다(다시 셀 때).
    pub(crate) fn note_caught_up(&mut self, started: Instant) -> bool {
        let t = self.caught_up.map_or(started, |t| t.max(started));
        self.caught_up = Some(t);
        self.pending.values().any(|since| *since <= started)
    }
}

impl ScopedQueue {
    /// 따라잡기가 읽는 기억(런타임에 같은 것을 둠).
    pub(crate) fn seen(&self) -> Arc<Mutex<ScopedSeen>> {
        self.seen.clone()
    }

    /// 대기 중이거나 거부를 기억하는 이벤트인지(따라잡기가 이미 가진 것으로 셈).
    pub(crate) fn is_known(&self, id: &EventId) -> bool {
        self.seen.lock().is_known(id)
    }

    pub(crate) fn remember_rejected(&mut self, id: EventId) {
        self.seen.lock().rejected.insert(id);
    }

    /// 대기열에 넣습니다. 그 작성 기기 몫이나 전체 상한을 넘으면 오래된 것부터 버리고, 버린 ID를 돌려줍니다(임시 키도
    /// 버리게).
    pub(crate) fn push(&mut self, p: PendingScoped) -> Vec<EventId> {
        let mut dropped = Vec::new();
        let author = p.ev.header.author;
        let mine = self
            .pending
            .iter()
            .filter(|x| x.ev.header.author == author)
            .count();
        if mine >= MAX_PENDING_PER_AUTHOR
            && let Some(i) = self
                .pending
                .iter()
                .position(|x| x.ev.header.author == author)
        {
            dropped.push(self.remove(i));
        }
        let size = size_of(&p.ev);
        while !self.pending.is_empty()
            && (self.pending.len() >= MAX_PENDING
                || self.bytes.saturating_add(size) > MAX_PENDING_BYTES)
        {
            dropped.push(self.remove(0));
        }
        self.bytes = self.bytes.saturating_add(size);
        self.seen.lock().pending.insert(p.id, p.since);
        self.pending.push(p);
        dropped
    }

    fn remove(&mut self, i: usize) -> EventId {
        let old = self.pending.remove(i);
        self.bytes = self.bytes.saturating_sub(size_of(&old.ev));
        self.seen.lock().pending.remove(&old.id);
        old.id
    }

    /// 대기열을 비우고 넘깁니다(다시 볼 것은 [`Self::push`]로 다시 넣음). 그사이 따라잡기는 이것들을 모르는 것으로
    /// 보고 다시 받을 수 있지만(받으면 중복으로 건너뜀), 버린 것을 아는 것으로 남기지는 않습니다.
    pub(crate) fn take(&mut self) -> Vec<PendingScoped> {
        self.bytes = 0;
        self.seen.lock().pending.clear();
        std::mem::take(&mut self.pending)
    }

    /// 마지막으로 끝난 따라잡기 한 바퀴의 시작 시각([`ScopedSeen::note_caught_up`]).
    pub(crate) fn caught_up(&self) -> Option<Instant> {
        self.seen.lock().caught_up
    }

    /// 이 이벤트를 지금 빈 구간을 허용해 검사해도 되는지: 이벤트가 온 뒤(`caught_up`이 그 뒤에 시작한 바퀴) 따라잡기
    /// 한 바퀴가 끝났거나, 유예가 지났음.
    pub(crate) fn gaps_allowed(
        caught_up: Option<Instant>,
        p: &PendingScoped,
        now: Instant,
    ) -> bool {
        caught_up.is_some_and(|t| t >= p.since) || now.duration_since(p.since) >= GAP_GRACE
    }

    pub(crate) fn cached_anchor(
        &self,
        anchor: &[EventId],
        generation: u64,
    ) -> Option<Option<Arc<SpaceState>>> {
        self.anchor_cache
            .iter()
            .find(|(a, g, _)| a == anchor && *g == generation)
            .map(|(_, _, s)| s.clone())
    }

    pub(crate) fn cache_anchor(
        &mut self,
        anchor: Vec<EventId>,
        generation: u64,
        state: Option<Arc<SpaceState>>,
    ) {
        self.anchor_cache.retain(|(a, _, _)| *a != anchor);
        if self.anchor_cache.len() >= MAX_ANCHOR_CACHE {
            self.anchor_cache.pop_front();
        }
        self.anchor_cache.push_back((anchor, generation, state));
    }
}

/// 크기 상한이 있는 ID 집합(넘으면 가장 먼저 넣은 것부터 잊음).
#[derive(Default)]
struct RecentIds {
    set: HashSet<EventId>,
    order: VecDeque<EventId>,
}

impl RecentIds {
    fn contains(&self, id: &EventId) -> bool {
        self.set.contains(id)
    }

    fn insert(&mut self, id: EventId) {
        if !self.set.insert(id) {
            return;
        }
        self.order.push_back(id);
        while self.order.len() > MAX_REJECTED {
            if let Some(old) = self.order.pop_front() {
                self.set.remove(&old);
            }
        }
    }
}

fn size_of(ev: &SignedEvent) -> usize {
    ev.body.as_ref().map_or(0, |b| b.len()) + 512
}

/// 따라잡기가 이 ID들 가운데 무엇을 이미 가진 것으로 볼지(대기 중·거부를 기억하는 부분 전달 이벤트). 적용 잠금 없이
/// 잎 잠금만 잠깐 쥡니다(같은 공간의 긴 적용을 기다리며 작업 스레드를 멈추지 않게).
pub(crate) fn known_ids(node: &crate::node::Node, space: &SpaceId, ids: &[EventId]) -> Vec<bool> {
    let seen = node
        .spaces
        .read()
        .get(space)
        .map(|rt| rt.scoped_seen.clone());
    let Some(seen) = seen else {
        return vec![false; ids.len()];
    };
    let seen = seen.lock();
    ids.iter().map(|id| seen.is_known(id)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use yangpa_core::event::{EVENT_VERSION, EventHeader, EventKind, KeyRef, Scope};
    use yangpa_core::ids::{DeviceId, UserId};
    use yangpa_core::time::Timestamp;

    fn ev(author: u8, n: u64) -> SignedEvent {
        let header = EventHeader {
            v: EVENT_VERSION,
            space: SpaceId([1; 16]),
            channel: None,
            kind: EventKind::KeyShare,
            author: DeviceId([author; 32]),
            user: UserId([author; 32]),
            parents: Vec::new(),
            lamport: n,
            created_at: Timestamp(n),
            key: KeyRef::Sealed,
            body_hash: [0; 32],
            body_len: 0,
            scope: Some(Scope::Inbox(UserId([9; 32]))),
            state_anchor: vec![EventId::hash_of(&n.to_le_bytes())],
        };
        SignedEvent {
            header,
            sig: yangpa_core::sign::Sig::from_bytes(&vec![0; yangpa_core::sign::Sig::LEN]).unwrap(),
            body: Some(vec![0; 16]),
        }
    }

    fn pending(e: SignedEvent) -> PendingScoped {
        PendingScoped {
            id: e.id(),
            ev: e,
            since: Instant::now(),
            trusted: false,
            blockers: Vec::new(),
            gaps_tried: false,
        }
    }

    /// 한 작성 기기가 대기열을 채워도 다른 기기의 대기 이벤트(가입 묶음)는 밀려나지 않습니다.
    #[test]
    fn one_author_cannot_evict_others() {
        let mut q = ScopedQueue::default();
        let victim = pending(ev(1, 0));
        let victim_id = victim.id;
        assert!(q.push(victim).is_empty());
        for n in 0..(MAX_PENDING as u64 * 2) {
            q.push(pending(ev(2, n + 1)));
        }
        assert!(q.pending.iter().any(|p| p.id == victim_id));
        let flood = q
            .pending
            .iter()
            .filter(|p| p.ev.header.author == DeviceId([2; 32]))
            .count();
        assert_eq!(flood, MAX_PENDING_PER_AUTHOR);
        assert!(q.is_known(&victim_id));
    }

    /// 따라잡기가 읽는 잎 기억은 대기열과 늘 같은 ID를 담습니다: 밀려난 것·꺼낸 것은 "가진 것"으로 남지 않습니다
    /// (남으면 따라잡기가 다시 받지 않아 영영 못 받음).
    #[test]
    fn seen_mirrors_the_queue() {
        let mut q = ScopedQueue::default();
        let seen = q.seen();
        let first = pending(ev(2, 0));
        let first_id = first.id;
        q.push(first);
        for n in 1..=(MAX_PENDING_PER_AUTHOR as u64) {
            q.push(pending(ev(2, n)));
        }
        assert!(!seen.lock().is_known(&first_id), "밀려난 것");
        let ids: Vec<EventId> = q.pending.iter().map(|p| p.id).collect();
        assert!(ids.iter().all(|id| seen.lock().is_known(id)));
        let mut taken = q.take();
        assert!(ids.iter().all(|id| !seen.lock().is_known(id)), "꺼낸 것");
        let back = taken.pop().unwrap();
        let back_id = back.id;
        q.push(back);
        assert!(seen.lock().is_known(&back_id));
        assert_eq!(seen.lock().pending.len(), q.pending.len());
        // 끝난 따라잡기 한 바퀴보다 먼저 온 대기 이벤트가 있으면 다시 셀 때입니다.
        assert!(seen.lock().note_caught_up(Instant::now()));
        assert!(q.caught_up().is_some());
        q.take();
        assert!(!seen.lock().note_caught_up(Instant::now()));
    }

    #[test]
    fn rejected_ids_are_bounded() {
        let mut q = ScopedQueue::default();
        let first = EventId::hash_of(b"first");
        q.remember_rejected(first);
        assert!(q.is_known(&first));
        for i in 0..MAX_REJECTED as u64 {
            q.remember_rejected(EventId::hash_of(&i.to_le_bytes()));
        }
        assert!(!q.is_known(&first));
    }
}
