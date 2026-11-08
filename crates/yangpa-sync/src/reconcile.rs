//! 논리 시계 구간 지문 비교로 빠진 이벤트만 찾기.
//!
//! 지문 = (구간 안 이벤트 수, 이벤트 ID 32바이트의 XOR). 저장소의 `fingerprints`와 같은 정의입니다.
//! 지문이 다르면 구간을 16등분해 다시 비교하고, 상대 쪽 이벤트가 64개 이하인 구간은 ID 목록을 받아
//! 차집합만 가져옵니다. [`Reconciler`]는 입출력 없는 상태 기계라서 요청을 내주고 응답을 받아 넣기만 합니다.
//!
//! ```text
//! let mut r = Reconciler::new(full_range(max_lamport));
//! while let Some((token, req)) = r.next_request() {
//!     let resp = 네트워크로_보내기(req)?;             // GetFingerprints / GetIds
//!     r.on_response(&local, token, resp.as_ref())?;
//!     받을_목록.extend(r.take_fetch());               // GetEvents로 가져옴
//! }
//! ```

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::convert::Infallible;

use thiserror::Error;
use yangpa_core::ids::EventId;
use yangpa_core::wire::{LamportRange, MAX_EVENTS_PER_MESSAGE, RangeFingerprint};

use crate::dag::StreamDag;

/// 지문이 다른 구간을 나누는 조각 수.
pub const SPLIT_PARTS: u32 = 16;
/// 상대 이벤트가 이 수 이하인 구간은 더 나누지 않고 ID 목록을 받습니다.
pub const LIST_THRESHOLD: u32 = 64;
/// 지문 요청 하나에 담는 구간 수.
pub const MAX_RANGES_PER_REQUEST: usize = 256;
/// 응답할 때 받아 주는 구간 수 상한(저장소 한도와 같음).
pub const MAX_RANGES_TO_ANSWER: usize = 4096;
/// ID 목록 요청 하나의 최대 개수.
pub const MAX_IDS_PER_REQUEST: u32 = MAX_EVENTS_PER_MESSAGE as u32;

/// 빈 구간 지문.
pub fn empty_fingerprint(range: LamportRange) -> RangeFingerprint {
    RangeFingerprint {
        range,
        count: 0,
        xor: [0; 32],
    }
}

/// 지문에 이벤트 하나를 더합니다(XOR이라 빼기도 같은 연산).
pub fn add_to_fingerprint(fp: &mut RangeFingerprint, id: &EventId) {
    for (x, b) in fp.xor.iter_mut().zip(id.as_bytes()) {
        *x ^= b;
    }
    fp.count = fp.count.saturating_add(1);
}

/// (논리 시계, ID) 목록에서 구간 [start, end)의 지문을 계산합니다.
pub fn fingerprint<'a>(
    items: impl IntoIterator<Item = (u64, &'a EventId)>,
    range: LamportRange,
) -> RangeFingerprint {
    let mut fp = empty_fingerprint(range);
    for (l, id) in items {
        if l >= range.start && l < range.end {
            add_to_fingerprint(&mut fp, id);
        }
    }
    fp
}

/// 구간을 최대 `parts`개의 비어 있지 않은 연속 구간으로 나눕니다(폭이 작으면 폭 1짜리 여러 개).
pub fn split_range(range: LamportRange, parts: u32) -> Vec<LamportRange> {
    if range.start >= range.end {
        return Vec::new();
    }
    let w = u128::from(range.end - range.start);
    let n = u128::from(parts.max(1)).min(w);
    (0..n)
        .map(|i| {
            // w ≥ n이라 각 조각 폭이 1 이상이고, 경계가 겹치지 않고 전체를 덮습니다.
            let s = range.start + (w * i / n) as u64;
            let e = range.start + (w * (i + 1) / n) as u64;
            LamportRange { start: s, end: e }
        })
        .collect()
}

/// 논리 시계 0부터 `max_lamport`까지 덮는 구간.
pub fn full_range(max_lamport: u64) -> LamportRange {
    LamportRange {
        start: 0,
        end: max_lamport.saturating_add(1),
    }
}

/// 로컬 이벤트 집합(저장소, 메모리 집합, DAG 색인 등).
pub trait FingerprintSource {
    type Error;
    /// 구간마다 지문(요청 순서대로).
    fn fingerprints(&self, ranges: &[LamportRange]) -> Result<Vec<RangeFingerprint>, Self::Error>;
    /// 구간 안 ID를 (논리 시계, ID) 순으로 최대 `limit`개.
    fn ids_in_range(&self, range: LamportRange, limit: u32) -> Result<Vec<EventId>, Self::Error>;
    /// 각 ID를 가지고 있는지.
    fn has_events(&self, ids: &[EventId]) -> Result<Vec<bool>, Self::Error>;
}

/// 메모리에 둔 (논리 시계, ID) 집합. 휘발성 스트림과 테스트에 씁니다.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MemorySet {
    items: BTreeSet<(u64, EventId)>,
    ids: BTreeSet<EventId>,
}

impl MemorySet {
    pub fn new() -> Self {
        Self::default()
    }

    /// 새로 넣었으면 `true`.
    pub fn insert(&mut self, lamport: u64, id: EventId) -> bool {
        if !self.ids.insert(id) {
            return false;
        }
        self.items.insert((lamport, id));
        true
    }

    pub fn contains(&self, id: &EventId) -> bool {
        self.ids.contains(id)
    }

    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    pub fn max_lamport(&self) -> u64 {
        self.items.last().map_or(0, |(l, _)| *l)
    }

    pub fn iter(&self) -> impl Iterator<Item = (u64, EventId)> + '_ {
        self.items.iter().copied()
    }

    fn range(&self, range: LamportRange) -> impl Iterator<Item = &(u64, EventId)> {
        let end = range.end.max(range.start);
        self.items
            .range((range.start, EventId([0; 32]))..(end, EventId([0; 32])))
    }
}

impl FromIterator<(u64, EventId)> for MemorySet {
    fn from_iter<T: IntoIterator<Item = (u64, EventId)>>(iter: T) -> Self {
        let mut s = MemorySet::new();
        for (l, id) in iter {
            s.insert(l, id);
        }
        s
    }
}

impl FingerprintSource for MemorySet {
    type Error = Infallible;

    fn fingerprints(&self, ranges: &[LamportRange]) -> Result<Vec<RangeFingerprint>, Infallible> {
        Ok(ranges
            .iter()
            .map(|r| fingerprint(self.range(*r).map(|(l, id)| (*l, id)), *r))
            .collect())
    }

    fn ids_in_range(&self, range: LamportRange, limit: u32) -> Result<Vec<EventId>, Infallible> {
        Ok(self
            .range(range)
            .take(limit as usize)
            .map(|(_, id)| *id)
            .collect())
    }

    fn has_events(&self, ids: &[EventId]) -> Result<Vec<bool>, Infallible> {
        Ok(ids.iter().map(|id| self.ids.contains(id)).collect())
    }
}

impl FingerprintSource for StreamDag {
    type Error = Infallible;

    fn fingerprints(&self, ranges: &[LamportRange]) -> Result<Vec<RangeFingerprint>, Infallible> {
        Ok(ranges
            .iter()
            .map(|r| {
                let mut fp = empty_fingerprint(*r);
                for (_, id) in self.range(r.start, r.end) {
                    add_to_fingerprint(&mut fp, &id);
                }
                fp
            })
            .collect())
    }

    fn ids_in_range(&self, range: LamportRange, limit: u32) -> Result<Vec<EventId>, Infallible> {
        Ok(self
            .range(range.start, range.end)
            .take(limit as usize)
            .map(|(_, id)| id)
            .collect())
    }

    fn has_events(&self, ids: &[EventId]) -> Result<Vec<bool>, Infallible> {
        Ok(ids.iter().map(|id| self.contains(id)).collect())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum ReconcileError<E> {
    #[error("로컬 이벤트 조회 실패")]
    Source(E),
    #[error("상대 응답이 요청과 맞지 않음")]
    BadResponse,
    #[error("잘못된 요청")]
    BadRequest,
    #[error("모르는 요청 번호")]
    UnknownToken,
}

/// 상대에게 보낼 요청(`YpRequest::GetFingerprints` / `YpRequest::GetIds`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReconcileRequest {
    Fingerprints(Vec<LamportRange>),
    Ids { range: LamportRange, limit: u32 },
}

/// 상대의 응답(`YpResponse::Fingerprints` / `YpResponse::Ids`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReconcileResponse<'a> {
    Fingerprints(&'a [RangeFingerprint]),
    Ids(&'a [EventId]),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReconcileConfig {
    pub parts: u32,
    pub list_threshold: u32,
    pub max_ranges_per_request: usize,
    /// 상대가 거짓 지문으로 끝없이 쪼개게 만드는 것을 막는 요청 수 예산.
    pub max_requests: u32,
    pub id_limit: u32,
}

impl Default for ReconcileConfig {
    fn default() -> Self {
        ReconcileConfig {
            parts: SPLIT_PARTS,
            list_threshold: LIST_THRESHOLD,
            max_ranges_per_request: MAX_RANGES_PER_REQUEST,
            max_requests: 4096,
            id_limit: MAX_IDS_PER_REQUEST,
        }
    }
}

/// 한 상대와의 지문 비교 상태 기계. 상대가 가졌고 내가 없는 ID를 모읍니다
/// (반대 방향은 상대가 같은 과정을 돌려 가져갑니다).
#[derive(Clone, Debug)]
pub struct Reconciler {
    cfg: ReconcileConfig,
    pending_fp: VecDeque<LamportRange>,
    pending_ids: VecDeque<(LamportRange, u32)>,
    outstanding: BTreeMap<u64, ReconcileRequest>,
    next_token: u64,
    sent: u32,
    fetch: BTreeSet<EventId>,
    exhausted: bool,
    truncated: bool,
    failed: bool,
}

impl Reconciler {
    pub fn new(range: LamportRange) -> Self {
        Self::with_config(range, ReconcileConfig::default())
    }

    pub fn with_config(range: LamportRange, cfg: ReconcileConfig) -> Self {
        let cfg = ReconcileConfig {
            parts: cfg.parts.max(2),
            list_threshold: cfg.list_threshold.max(1),
            max_ranges_per_request: cfg.max_ranges_per_request.clamp(1, MAX_RANGES_TO_ANSWER),
            id_limit: cfg.id_limit.clamp(1, MAX_IDS_PER_REQUEST),
            ..cfg
        };
        // 처음부터 나눠 보내도 왕복 한 번이고, 다를 때 한 단계를 아낍니다.
        let pending_fp = split_range(range, cfg.parts).into();
        Reconciler {
            cfg,
            pending_fp,
            pending_ids: VecDeque::new(),
            outstanding: BTreeMap::new(),
            next_token: 0,
            sent: 0,
            fetch: BTreeSet::new(),
            exhausted: false,
            truncated: false,
            failed: false,
        }
    }

    /// 다음 요청. ID 목록 요청(결과가 바로 나오는 것)을 먼저 냅니다. 동시에 여러 개를 보내도 됩니다.
    pub fn next_request(&mut self) -> Option<(u64, ReconcileRequest)> {
        if self.pending_ids.is_empty() && self.pending_fp.is_empty() {
            return None;
        }
        if self.sent >= self.cfg.max_requests {
            self.exhausted = true;
            self.pending_ids.clear();
            self.pending_fp.clear();
            return None;
        }
        let req = if let Some((range, limit)) = self.pending_ids.pop_front() {
            ReconcileRequest::Ids { range, limit }
        } else {
            let n = self.pending_fp.len().min(self.cfg.max_ranges_per_request);
            // 대기 구간은 서로 겹치지 않습니다. 정렬해 보내면 응답하는 쪽이 한 번 훑어 답할 수 있고,
            // [`answer_fingerprints`]의 겹침 검사도 통과합니다.
            let mut ranges: Vec<LamportRange> = self.pending_fp.drain(..n).collect();
            ranges.sort_unstable_by_key(|r| (r.start, r.end));
            ReconcileRequest::Fingerprints(ranges)
        };
        let token = self.next_token;
        self.next_token += 1;
        self.sent += 1;
        self.outstanding.insert(token, req.clone());
        Some((token, req))
    }

    /// 응답을 반영합니다. 요청과 맞지 않는 응답은 [`ReconcileError::BadResponse`](그 상대 점수를 깎을 근거).
    pub fn on_response<S: FingerprintSource>(
        &mut self,
        local: &S,
        token: u64,
        resp: ReconcileResponse<'_>,
    ) -> Result<(), ReconcileError<S::Error>> {
        let req = self
            .outstanding
            .remove(&token)
            .ok_or(ReconcileError::UnknownToken)?;
        match (req, resp) {
            (ReconcileRequest::Fingerprints(ranges), ReconcileResponse::Fingerprints(remote)) => {
                if remote.len() != ranges.len()
                    || remote.iter().zip(&ranges).any(|(f, r)| f.range != *r)
                {
                    self.failed = true;
                    return Err(ReconcileError::BadResponse);
                }
                let local_fps = local
                    .fingerprints(&ranges)
                    .map_err(ReconcileError::Source)?;
                for (i, theirs) in remote.iter().enumerate() {
                    let same = local_fps
                        .get(i)
                        .is_some_and(|mine| mine.count == theirs.count && mine.xor == theirs.xor);
                    // 상대가 0개면 상대에게 없는 것이 나에게만 있을 뿐, 받을 것은 없습니다.
                    if same || theirs.count == 0 {
                        continue;
                    }
                    let width = theirs.range.end - theirs.range.start;
                    if theirs.count <= self.cfg.list_threshold || width <= 1 {
                        if theirs.count > self.cfg.id_limit {
                            // 논리 시계 하나에 이벤트가 너무 많으면 프로토콜상 나눠 받을 수 없습니다.
                            self.truncated = true;
                        }
                        self.pending_ids
                            .push_back((theirs.range, theirs.count.min(self.cfg.id_limit)));
                    } else {
                        self.pending_fp
                            .extend(split_range(theirs.range, self.cfg.parts));
                    }
                }
                Ok(())
            }
            (ReconcileRequest::Ids { limit, .. }, ReconcileResponse::Ids(ids)) => {
                if ids.len() > limit as usize {
                    self.failed = true;
                    return Err(ReconcileError::BadResponse);
                }
                let have = local.has_events(ids).map_err(ReconcileError::Source)?;
                for (id, h) in ids.iter().zip(have) {
                    if !h {
                        self.fetch.insert(*id);
                    }
                }
                Ok(())
            }
            _ => {
                self.failed = true;
                Err(ReconcileError::BadResponse)
            }
        }
    }

    /// 요청이 실패(거절·시간 초과)했을 때. 그 구간은 이 상대와는 맞추지 못한 채 남습니다.
    pub fn on_failure(&mut self, token: u64) {
        if self.outstanding.remove(&token).is_some() {
            self.failed = true;
        }
    }

    /// 지금까지 찾은, 상대에게만 있는 ID를 꺼냅니다.
    pub fn take_fetch(&mut self) -> Vec<EventId> {
        std::mem::take(&mut self.fetch).into_iter().collect()
    }

    /// 꺼내지 않은 받을 목록.
    pub fn to_fetch(&self) -> &BTreeSet<EventId> {
        &self.fetch
    }

    /// 더 보낼 요청도, 기다리는 응답도 없음.
    pub fn is_done(&self) -> bool {
        self.pending_ids.is_empty() && self.pending_fp.is_empty() && self.outstanding.is_empty()
    }

    /// 끝났고, 예산 초과·실패·잘림 없이 전 구간을 맞췄는지.
    pub fn is_complete(&self) -> bool {
        self.is_done() && !self.exhausted && !self.truncated && !self.failed
    }

    pub fn requests_sent(&self) -> u32 {
        self.sent
    }
}

/// 상대의 지문 요청에 답합니다(구간 수·형식 검사 포함).
///
/// 구간끼리 겹치면 거부합니다. 겹침을 허용하면 상대가 '전체 구간'을 수천 번 담아 보내 요청 하나로
/// 저장소 전체를 수천 번 훑게 만들 수 있기 때문입니다. 겹치지 않으면 일의 양이 이벤트 수에 묶입니다.
pub fn answer_fingerprints<S: FingerprintSource>(
    local: &S,
    ranges: &[LamportRange],
) -> Result<Vec<RangeFingerprint>, ReconcileError<S::Error>> {
    if !ranges_are_valid(ranges) {
        return Err(ReconcileError::BadRequest);
    }
    local.fingerprints(ranges).map_err(ReconcileError::Source)
}

/// 지문 요청의 구간 목록 검사: 개수 상한, 시작 ≤ 끝, 서로 겹치지 않음(순서는 자유).
pub fn ranges_are_valid(ranges: &[LamportRange]) -> bool {
    if ranges.len() > MAX_RANGES_TO_ANSWER || ranges.iter().any(|r| r.start > r.end) {
        return false;
    }
    let mut sorted: Vec<(u64, u64)> = ranges
        .iter()
        .filter(|r| r.start < r.end)
        .map(|r| (r.start, r.end))
        .collect();
    sorted.sort_unstable();
    sorted.windows(2).all(|w| w[0].1 <= w[1].0)
}

/// 상대의 ID 목록 요청에 답합니다(개수 상한 적용).
pub fn answer_ids<S: FingerprintSource>(
    local: &S,
    range: LamportRange,
    limit: u32,
) -> Result<Vec<EventId>, ReconcileError<S::Error>> {
    if range.start > range.end {
        return Err(ReconcileError::BadRequest);
    }
    local
        .ids_in_range(range, limit.min(MAX_IDS_PER_REQUEST))
        .map_err(ReconcileError::Source)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn id(n: u64) -> EventId {
        EventId::hash_of(&n.to_le_bytes())
    }

    /// `local`이 `remote`와 맞춰 받을 ID를 돌려줍니다(요청을 한 번에 하나씩 처리).
    fn reconcile(
        local: &MemorySet,
        remote: &MemorySet,
        cfg: ReconcileConfig,
    ) -> (Vec<EventId>, Reconciler) {
        let max = local.max_lamport().max(remote.max_lamport());
        let mut r = Reconciler::with_config(full_range(max), cfg);
        let mut out = Vec::new();
        while let Some((token, req)) = r.next_request() {
            match req {
                ReconcileRequest::Fingerprints(ranges) => {
                    let fps = answer_fingerprints(remote, &ranges).unwrap();
                    r.on_response(local, token, ReconcileResponse::Fingerprints(&fps))
                        .unwrap();
                }
                ReconcileRequest::Ids { range, limit } => {
                    let ids = answer_ids(remote, range, limit).unwrap();
                    r.on_response(local, token, ReconcileResponse::Ids(&ids))
                        .unwrap();
                }
            }
            out.extend(r.take_fetch());
        }
        assert!(r.is_done());
        (out, r)
    }

    #[test]
    fn split_covers_range_exactly() {
        let r = LamportRange {
            start: 10,
            end: 1000,
        };
        let parts = split_range(r, 16);
        assert_eq!(parts.len(), 16);
        assert_eq!(parts[0].start, 10);
        assert_eq!(parts[15].end, 1000);
        assert!(
            parts
                .windows(2)
                .all(|w| w[0].end == w[1].start && w[0].start < w[0].end)
        );
        // 폭이 조각 수보다 작으면 폭 1짜리로.
        let small = split_range(LamportRange { start: 5, end: 8 }, 16);
        assert_eq!(small.len(), 3);
        assert!(small.iter().all(|p| p.end - p.start == 1));
        assert!(split_range(LamportRange { start: 5, end: 5 }, 16).is_empty());
        // 넘침 없이 큰 구간도 나눔.
        let huge = split_range(
            LamportRange {
                start: 0,
                end: u64::MAX,
            },
            16,
        );
        assert_eq!(huge.len(), 16);
        assert_eq!(huge[15].end, u64::MAX);
    }

    #[test]
    fn fingerprint_is_order_independent_xor() {
        let r = LamportRange { start: 0, end: 100 };
        let a = fingerprint([(1, &id(1)), (2, &id(2)), (500, &id(3))], r);
        let b = fingerprint([(2, &id(2)), (1, &id(1))], r);
        assert_eq!(a, b);
        assert_eq!(a.count, 2);
        let mut c = empty_fingerprint(r);
        add_to_fingerprint(&mut c, &id(1));
        add_to_fingerprint(&mut c, &id(2));
        assert_eq!(c.xor, a.xor);
    }

    #[test]
    fn identical_sets_need_one_round_trip() {
        let s: MemorySet = (0..1000u64).map(|i| (i / 3, id(i))).collect();
        let (fetch, r) = reconcile(&s, &s.clone(), ReconcileConfig::default());
        assert!(fetch.is_empty());
        assert_eq!(r.requests_sent(), 1);
        assert!(r.is_complete());
    }

    #[test]
    fn finds_single_missing_event_in_large_set() {
        let remote: MemorySet = (0..5000u64).map(|i| (i, id(i))).collect();
        let local: MemorySet = (0..5000u64)
            .filter(|i| *i != 4321)
            .map(|i| (i, id(i)))
            .collect();
        let (fetch, r) = reconcile(&local, &remote, ReconcileConfig::default());
        assert_eq!(fetch, vec![id(4321)]);
        // 16등분 재귀라서 요청 수가 작음.
        assert!(r.requests_sent() < 10, "요청 {}", r.requests_sent());
    }

    #[test]
    fn many_events_at_one_lamport_are_listed() {
        let remote: MemorySet = (0..100u64).map(|i| (7, id(i))).collect();
        let local = MemorySet::new();
        let (mut fetch, r) = reconcile(&local, &remote, ReconcileConfig::default());
        fetch.sort();
        let mut want: Vec<EventId> = (0..100u64).map(id).collect();
        want.sort();
        assert_eq!(fetch, want);
        assert!(r.is_complete());
    }

    #[test]
    fn rejects_mismatched_responses() {
        let local = MemorySet::new();
        let mut r = Reconciler::new(full_range(100));
        let (token, req) = r.next_request().unwrap();
        let ReconcileRequest::Fingerprints(ranges) = req else {
            panic!("지문 요청이어야 함")
        };
        // 구간 수가 다름.
        let bad = vec![empty_fingerprint(ranges[0])];
        assert_eq!(
            r.on_response(&local, token, ReconcileResponse::Fingerprints(&bad)),
            Err(ReconcileError::BadResponse)
        );
        assert_eq!(
            r.on_response(&local, token, ReconcileResponse::Fingerprints(&bad)),
            Err(ReconcileError::UnknownToken)
        );
        assert!(!r.is_complete());
    }

    #[test]
    fn ids_response_over_limit_is_rejected() {
        let local = MemorySet::new();
        let remote: MemorySet = (0..10u64).map(|i| (1, id(i))).collect();
        let mut r = Reconciler::new(LamportRange { start: 1, end: 2 });
        let (t, req) = r.next_request().unwrap();
        let ReconcileRequest::Fingerprints(ranges) = req else {
            panic!()
        };
        let fps = answer_fingerprints(&remote, &ranges).unwrap();
        r.on_response(&local, t, ReconcileResponse::Fingerprints(&fps))
            .unwrap();
        let (t, req) = r.next_request().unwrap();
        assert_eq!(
            req,
            ReconcileRequest::Ids {
                range: LamportRange { start: 1, end: 2 },
                limit: 10
            }
        );
        let too_many: Vec<EventId> = (0..11u64).map(id).collect();
        assert_eq!(
            r.on_response(&local, t, ReconcileResponse::Ids(&too_many)),
            Err(ReconcileError::BadResponse)
        );
    }

    #[test]
    fn budget_stops_lying_peer() {
        // 항상 '많이 다르다'고 답하는 상대.
        let local = MemorySet::new();
        let cfg = ReconcileConfig {
            max_requests: 20,
            ..ReconcileConfig::default()
        };
        let mut r = Reconciler::with_config(full_range(u64::MAX - 1), cfg);
        let mut n = 0;
        while let Some((token, req)) = r.next_request() {
            n += 1;
            let ReconcileRequest::Fingerprints(ranges) = req else {
                r.on_response(&local, token, ReconcileResponse::Ids(&[]))
                    .unwrap();
                continue;
            };
            let fps: Vec<RangeFingerprint> = ranges
                .iter()
                .map(|r| RangeFingerprint {
                    range: *r,
                    count: 1_000_000,
                    xor: [1; 32],
                })
                .collect();
            r.on_response(&local, token, ReconcileResponse::Fingerprints(&fps))
                .unwrap();
        }
        assert_eq!(n, 20);
        assert!(r.is_done());
        assert!(!r.is_complete());
    }

    #[test]
    fn answer_validates_requests() {
        let s = MemorySet::new();
        assert_eq!(
            answer_fingerprints(&s, &[LamportRange { start: 5, end: 1 }]),
            Err(ReconcileError::BadRequest)
        );
        let many = vec![LamportRange { start: 0, end: 1 }; MAX_RANGES_TO_ANSWER + 1];
        assert_eq!(
            answer_fingerprints(&s, &many),
            Err(ReconcileError::BadRequest)
        );
        assert_eq!(
            answer_ids(&s, LamportRange { start: 5, end: 1 }, 10),
            Err(ReconcileError::BadRequest)
        );
        // 겹치는 구간(같은 구간 반복 포함)은 거부, 맞닿거나 빈 구간·순서 섞임은 허용.
        let r = |start, end| LamportRange { start, end };
        assert_eq!(
            answer_fingerprints(&s, &[r(0, 100), r(0, 100)]),
            Err(ReconcileError::BadRequest)
        );
        assert_eq!(
            answer_fingerprints(&s, &[r(50, 60), r(0, 51)]),
            Err(ReconcileError::BadRequest)
        );
        assert!(answer_fingerprints(&s, &[r(10, 20), r(0, 10), r(5, 5), r(5, 5)]).is_ok());
        assert!(ranges_are_valid(&[]));
    }

    /// 상태 기계가 보내는 지문 요청은 항상 겹치지 않고 정렬돼 있어 상대의 검사를 통과합니다.
    #[test]
    fn requests_are_sorted_and_disjoint() {
        let remote: MemorySet = (0..20_000u64).map(|i| (i, id(i))).collect();
        let local: MemorySet = (0..20_000u64)
            .filter(|i| i % 97 != 0)
            .map(|i| (i, id(i)))
            .collect();
        let cfg = ReconcileConfig {
            max_ranges_per_request: 7,
            ..ReconcileConfig::default()
        };
        let mut r = Reconciler::with_config(full_range(20_000), cfg);
        let mut fetched = 0;
        while let Some((token, req)) = r.next_request() {
            match req {
                ReconcileRequest::Fingerprints(ranges) => {
                    assert!(ranges.windows(2).all(|w| w[0].end <= w[1].start));
                    let fps = answer_fingerprints(&remote, &ranges).unwrap();
                    r.on_response(&local, token, ReconcileResponse::Fingerprints(&fps))
                        .unwrap();
                }
                ReconcileRequest::Ids { range, limit } => {
                    let ids = answer_ids(&remote, range, limit).unwrap();
                    r.on_response(&local, token, ReconcileResponse::Ids(&ids))
                        .unwrap();
                }
            }
            fetched += r.take_fetch().len();
        }
        assert!(r.is_complete());
        assert_eq!(fetched, (0..20_000u64).filter(|i| i % 97 == 0).count());
    }

    #[test]
    fn stream_dag_is_a_fingerprint_source() {
        let mut dag = StreamDag::new();
        let mut set = MemorySet::new();
        for i in 1..200u64 {
            dag.insert(id(i), &[], i).unwrap();
            set.insert(i, id(i));
        }
        let ranges = split_range(full_range(200), 16);
        assert_eq!(
            dag.fingerprints(&ranges).unwrap(),
            set.fingerprints(&ranges).unwrap()
        );
        assert_eq!(
            dag.ids_in_range(LamportRange { start: 10, end: 20 }, 5)
                .unwrap(),
            set.ids_in_range(LamportRange { start: 10, end: 20 }, 5)
                .unwrap()
        );
        assert_eq!(
            dag.has_events(&[id(1), id(999)]).unwrap(),
            vec![true, false]
        );
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        /// 서로 다른 두 집합이 양방향 지문 비교 뒤 같아집니다.
        #[test]
        fn two_random_sets_converge(
            shared in proptest::collection::vec((0u64..2000, any::<u64>()), 0..800),
            only_a in proptest::collection::vec((0u64..2000, any::<u64>()), 0..300),
            only_b in proptest::collection::vec((0u64..2000, any::<u64>()), 0..300),
            parts in 2u32..20,
            threshold in 1u32..80,
        ) {
            let mut a = MemorySet::new();
            let mut b = MemorySet::new();
            let mut all = BTreeMap::new();
            for (l, n) in &shared {
                // 같은 ID가 두 번 나오면 처음 논리 시계를 씁니다(집합과 같은 규칙).
                let l = *all.entry(id(*n)).or_insert(*l);
                a.insert(l, id(*n));
                b.insert(l, id(*n));
            }
            for (l, n) in &only_a {
                let i = id(n.wrapping_add(1 << 63));
                let l = *all.entry(i).or_insert(*l);
                a.insert(l, i);
            }
            for (l, n) in &only_b {
                let i = id(n.wrapping_add(1 << 62));
                let l = *all.entry(i).or_insert(*l);
                b.insert(l, i);
            }
            let cfg = ReconcileConfig { parts, list_threshold: threshold, ..ReconcileConfig::default() };
            let (for_a, ra) = reconcile(&a, &b, cfg);
            let (for_b, rb) = reconcile(&b, &a, cfg);
            prop_assert!(ra.is_complete() && rb.is_complete());
            for i in for_a {
                prop_assert!(!a.contains(&i));
                a.insert(all[&i], i);
            }
            for i in for_b {
                prop_assert!(!b.contains(&i));
                b.insert(all[&i], i);
            }
            prop_assert_eq!(&a, &b);
            prop_assert_eq!(a.len(), all.len());
        }
    }
}
