//! 블롭 하나의 조각 스케줄러(닫힌 스웜).
//!
//! - 희귀 조각 우선: 가진 피어 수가 적은 조각부터 요청합니다. 같은 희귀도는 시드로 정한 의사난수 순서
//!   (조각마다 고정)로 섞어, 모든 받는 쪽이 같은 조각에 몰리지 않게 합니다.
//! - 피어별·전체 동시 요청 상한, 요청마다 기한(호출자가 넘긴 밀리초 시각 기준).
//! - 조각은 받는 즉시 호출자가 매니페스트 해시로 검증하고 결과를 넘깁니다. 검증 실패가 쌓였거나
//!   시간 초과·전송 실패가 연달아 난 피어는 이 블롭에서 제외합니다(전역 점수는 [`crate::score`]에서).
//!   일시적 실패는 성공하면 다시 0부터 세므로, 큰 블롭을 받는 동안 드문 시간 초과로 하나뿐인 시더를
//!   잃지 않습니다. 검증 실패는 조작 가능성이 있어 성공해도 줄지 않습니다.
//! - 남은 조각이 모두 요청 중이면 다른 피어에게도 겹쳐 요청합니다(마지막 조각 지연 줄이기).
//!
//! 피어 식별자 타입은 호출자가 정합니다(libp2p `PeerId` 등). 정렬 가능해야 결과가 결정론적입니다.

use std::collections::{BTreeMap, BTreeSet};

use thiserror::Error;
use yangpa_core::blob::MAX_CHUNKS;

#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum SwarmError {
    #[error("조각 수가 너무 많음")]
    TooManyChunks,
    #[error("비트맵 크기가 조각 수와 맞지 않음")]
    BitmapMismatch,
    #[error("조각 번호 범위 밖")]
    IndexOutOfRange,
}

/// 조각 보유 비트맵. 네트워크 형식(`YpResponse::Have{bitmap, total}`)과 같이 바이트마다 낮은 비트부터
/// (조각 i는 `bitmap[i / 8]`의 `1 << (i % 8)`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bitmap {
    bits: Vec<u8>,
    len: u32,
}

impl Bitmap {
    /// 조각 `total`개를 담는 데 필요한 바이트 수.
    pub fn byte_len(total: u32) -> usize {
        (total as usize).div_ceil(8)
    }

    pub fn new(len: u32) -> Self {
        Bitmap {
            bits: vec![0; Self::byte_len(len)],
            len,
        }
    }

    pub fn full(len: u32) -> Self {
        let mut b = Bitmap {
            bits: vec![0xFF; Self::byte_len(len)],
            len,
        };
        b.mask_tail();
        b
    }

    /// 네트워크에서 받은 비트맵. 크기가 맞지 않으면 거부하고, 끝의 남는 비트는 지웁니다.
    pub fn from_wire(bytes: &[u8], total: u32) -> Result<Self, SwarmError> {
        if total as usize > MAX_CHUNKS {
            return Err(SwarmError::TooManyChunks);
        }
        if bytes.len() != Self::byte_len(total) {
            return Err(SwarmError::BitmapMismatch);
        }
        let mut b = Bitmap {
            bits: bytes.to_vec(),
            len: total,
        };
        b.mask_tail();
        Ok(b)
    }

    /// 네트워크 형식 `(bitmap, total)`.
    pub fn to_wire(&self) -> (Vec<u8>, u32) {
        (self.bits.clone(), self.len)
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bits
    }

    fn mask_tail(&mut self) {
        let rem = self.len % 8;
        if rem != 0
            && let Some(last) = self.bits.last_mut()
        {
            *last &= (1u8 << rem) - 1;
        }
    }

    pub fn len(&self) -> u32 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn get(&self, i: u32) -> bool {
        i < self.len && self.bits[(i / 8) as usize] & (1 << (i % 8)) != 0
    }

    /// 켰으면(바뀌었으면) `true`.
    pub fn set(&mut self, i: u32) -> bool {
        if i >= self.len || self.get(i) {
            return false;
        }
        self.bits[(i / 8) as usize] |= 1 << (i % 8);
        true
    }

    /// 껐으면(바뀌었으면) `true`.
    pub fn clear(&mut self, i: u32) -> bool {
        if !self.get(i) {
            return false;
        }
        self.bits[(i / 8) as usize] &= !(1 << (i % 8));
        true
    }

    pub fn count_ones(&self) -> u32 {
        self.bits.iter().map(|b| b.count_ones()).sum()
    }

    pub fn is_full(&self) -> bool {
        self.count_ones() == self.len
    }

    pub fn iter_ones(&self) -> impl Iterator<Item = u32> + '_ {
        self.bits.iter().enumerate().flat_map(move |(byte, b)| {
            (0..8u32)
                .filter(move |bit| b & (1 << bit) != 0)
                .map(move |bit| byte as u32 * 8 + bit)
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SwarmConfig {
    pub max_in_flight_per_peer: u32,
    pub max_in_flight_total: u32,
    pub request_timeout_ms: u64,
    /// 검증 실패가 모두 합쳐 이만큼이거나, 시간 초과·전송 실패가 성공 없이 연달아 이만큼이면
    /// 이 블롭에서 그 피어를 뺍니다.
    pub max_failures: u32,
    /// 마지막 단계에서 한 조각을 동시에 요청할 최대 피어 수.
    pub endgame_duplicates: u32,
    /// 마지막 단계에서 요청이 이만큼 오래 걸릴 때만 다른 피어에게 겹쳐 요청합니다(작은 블롭을 두 번 받지 않게).
    pub endgame_delay_ms: u64,
    /// 같은 희귀도 조각 사이의 순서를 정하는 시드.
    pub seed: u64,
    /// 한 번의 [`ChunkScheduler::next_requests`]에서 훑는 후보 수 상한.
    pub scan_limit: usize,
}

impl Default for SwarmConfig {
    fn default() -> Self {
        SwarmConfig {
            max_in_flight_per_peer: 4,
            max_in_flight_total: 16,
            request_timeout_ms: 20_000,
            max_failures: 3,
            endgame_duplicates: 2,
            endgame_delay_ms: 2_000,
            seed: 0,
            scan_limit: 4096,
        }
    }
}

/// 직접 연결(Tor를 거치지 않음)에서 블롭 하나가 피어 하나에 겹쳐 보낼 조각 요청 수의 최소·최대·처음 값.
/// 최대는 망 계층의 피어당 요청 상한(`yangpa_net` 32개)보다 작게 두어 같은 피어에게 글·동기화 요청 자리를 남깁니다.
/// 처음 값이 작은 것은 아주 느린 회선에서 처음부터 많이 줄 세우면 뒤쪽 요청이 시간 제한에 걸리기 때문입니다. 첫 답 둘로
/// 속도를 재면 곧 회선에 맞춰 늘어납니다.
pub const DIRECT_WINDOW_MIN: u32 = 2;
pub const DIRECT_WINDOW_MAX: u32 = 24;
pub const DIRECT_WINDOW_START: u32 = 2;
/// 겹쳐 둘 데이터를 잰 속도로 이 시간에 받을 양에 맞춥니다. 왕복보다 길어야 회선을 채우고, 요청 시간 제한(20초 넘음)보다
/// 훨씬 짧아야 느린 회선에서 줄 선 요청이 시간 초과로 버려지지 않습니다.
pub const DIRECT_WINDOW_TARGET_MS: u64 = 500;

/// 잰 수신 속도(바이트/초)에 맞춘 피어당 겹침 수: [`DIRECT_WINDOW_TARGET_MS`] 동안 받을 조각 수 + 2. 겹침이 왕복보다
/// 길게 쌓이면 다음 측정에서 속도가 올라 겹침도 늘어(왕복이 짧을수록 빨리) 회선 속도에서 멈춥니다. 속도를 아직 모르면
/// [`DIRECT_WINDOW_START`].
pub fn direct_window(rate_bps: Option<u64>, chunk_size: u32) -> u32 {
    let Some(rate) = rate_bps else {
        return DIRECT_WINDOW_START;
    };
    let target = rate.saturating_mul(DIRECT_WINDOW_TARGET_MS) / 1000;
    let chunks = target.div_ceil(u64::from(chunk_size.max(1)));
    u32::try_from(chunks.saturating_add(2))
        .unwrap_or(u32::MAX)
        .clamp(DIRECT_WINDOW_MIN, DIRECT_WINDOW_MAX)
}

/// 최근 [`RateMeter::SPAN_MS`] 동안 받은 바이트로 잰 속도. 조각이 드문드문 오는 느린 회선에서도 잴 수 있게 표본 둘은
/// 오래돼도 남깁니다.
#[derive(Clone, Debug, Default)]
pub struct RateMeter {
    samples: std::collections::VecDeque<(u64, u64)>,
    bytes: u64,
}

impl RateMeter {
    pub const SPAN_MS: u64 = 1_000;

    pub fn record(&mut self, now_ms: u64, bytes: u64) {
        self.samples.push_back((now_ms, bytes));
        self.bytes = self.bytes.saturating_add(bytes);
        self.trim(now_ms);
    }

    fn trim(&mut self, now_ms: u64) {
        while let Some(&(at, b)) = self.samples.front() {
            if self.samples.len() <= 2 || now_ms.saturating_sub(at) <= Self::SPAN_MS {
                break;
            }
            self.samples.pop_front();
            self.bytes -= b;
        }
    }

    /// 바이트/초. 표본이 둘 미만이면 `None`(아직 모름). 마지막 표본 뒤로 조용하면 지금까지로 나눠 내려갑니다.
    pub fn rate(&mut self, now_ms: u64) -> Option<u64> {
        self.trim(now_ms);
        if self.samples.len() < 2 {
            return None;
        }
        // 첫 표본은 구간의 시작점: 그 바이트는 그 전부터 오던 것이라 빼야 처음 두 답이 붙어 와도 부풀지 않습니다.
        let &(first_at, first_bytes) = self.samples.front()?;
        let span = now_ms.saturating_sub(first_at).max(1);
        Some((self.bytes - first_bytes).saturating_mul(1000) / span)
    }
}

/// 요청 실패 종류.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestError {
    /// 시더가 바쁨(`YpResponse::Busy`). 감점 없이 그 시간 동안 쉬게 합니다.
    Busy { retry_after_ms: u32 },
    /// 시더에게 그 조각이 없음(`NotFound`). 그 조각만 비트맵에서 지웁니다.
    NotFound,
    /// 시더가 이 블롭을 제공하지 않음(`NotSeeded`·`Forbidden` 등). 그 피어의 보유 정보를 비웁니다.
    Denied,
    /// 연결 끊김 등 전송 실패. 실패로 셉니다.
    Failed,
}

/// 받은 조각 처리 결과.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChunkOutcome<P> {
    /// 검증된 조각. `cancel`은 같은 조각을 겹쳐 요청한 다른 피어(취소해도 됨).
    Accepted {
        complete: bool,
        cancel: Vec<(P, u32)>,
    },
    /// 이미 받은 조각이 겹친 요청으로 또 옴(감점 없음).
    Duplicate,
    /// 검증 실패. `excluded`면 그 피어를 이 블롭에서 뺐고 `cancel`은 그 피어에게 보낸 다른 요청.
    BadChunk {
        excluded: bool,
        cancel: Vec<(P, u32)>,
    },
    /// 요청하지 않은 조각(그 피어에게 요청 중이 아님).
    Unexpected,
}

#[derive(Clone, Debug)]
struct PeerState {
    have: Bitmap,
    in_flight: u32,
    /// 검증에 실패한 조각 수(줄지 않음).
    bad_chunks: u32,
    /// 마지막 성공 뒤 연달아 난 시간 초과·전송 실패 수.
    strikes: u32,
    successes: u64,
    excluded: bool,
    busy_until: u64,
    /// 이 피어만의 동시 요청 한도(`None`이면 [`SwarmConfig::max_in_flight_per_peer`]).
    window: Option<u32>,
}

impl PeerState {
    fn failures(&self) -> u32 {
        self.bad_chunks.saturating_add(self.strikes)
    }
}

/// 실패 종류: 조작 가능성이 있는 검증 실패와 일시적 실패.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Failure {
    BadChunk,
    Transient,
}

/// 블롭 하나를 여러 피어에게서 나눠 받는 스케줄러.
#[derive(Clone, Debug)]
pub struct ChunkScheduler<P: Clone + Ord> {
    cfg: SwarmConfig,
    total: u32,
    have: Bitmap,
    have_count: u32,
    peers: BTreeMap<P, PeerState>,
    /// 조각별로 그 조각을 가진 (제외되지 않은) 피어 수.
    avail: Vec<u32>,
    /// 필요하고, 요청 중이 아니고, 누군가 가진 조각: (희귀도, 섞기 값, 번호).
    queue: BTreeSet<(u32, u64, u32)>,
    /// 조각 → (피어, 기한).
    in_flight: BTreeMap<u32, Vec<(P, u64)>>,
    in_flight_total: u32,
}

fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

impl<P: Clone + Ord> ChunkScheduler<P> {
    pub fn new(total: u32, cfg: SwarmConfig) -> Result<Self, SwarmError> {
        if total as usize > MAX_CHUNKS {
            return Err(SwarmError::TooManyChunks);
        }
        let cfg = SwarmConfig {
            max_in_flight_per_peer: cfg.max_in_flight_per_peer.max(1),
            max_in_flight_total: cfg.max_in_flight_total.max(1),
            max_failures: cfg.max_failures.max(1),
            endgame_duplicates: cfg.endgame_duplicates.max(1),
            scan_limit: cfg.scan_limit.max(1),
            ..cfg
        };
        Ok(ChunkScheduler {
            cfg,
            total,
            have: Bitmap::new(total),
            have_count: 0,
            peers: BTreeMap::new(),
            avail: vec![0; total as usize],
            queue: BTreeSet::new(),
            in_flight: BTreeMap::new(),
            in_flight_total: 0,
        })
    }

    /// 이미 가진 조각(이어 받기)으로 시작합니다.
    pub fn with_local(local: Bitmap, cfg: SwarmConfig) -> Result<Self, SwarmError> {
        let mut s = Self::new(local.len(), cfg)?;
        for i in local.iter_ones() {
            s.mark_have(i)?;
        }
        Ok(s)
    }

    fn tiebreak(&self, i: u32) -> u64 {
        splitmix64(self.cfg.seed ^ u64::from(i).wrapping_mul(0x9E37_79B9_7F4A_7C15))
    }

    fn queue_key(&self, i: u32) -> (u32, u64, u32) {
        (self.avail[i as usize], self.tiebreak(i), i)
    }

    fn eligible(&self, i: u32) -> bool {
        !self.have.get(i) && !self.in_flight.contains_key(&i) && self.avail[i as usize] > 0
    }

    fn queue_remove(&mut self, i: u32) {
        let k = self.queue_key(i);
        self.queue.remove(&k);
    }

    fn queue_add_if_eligible(&mut self, i: u32) {
        if self.eligible(i) {
            let k = self.queue_key(i);
            self.queue.insert(k);
        }
    }

    fn change_avail(&mut self, i: u32, up: bool) {
        self.queue_remove(i);
        let a = &mut self.avail[i as usize];
        *a = if up {
            a.saturating_add(1)
        } else {
            a.saturating_sub(1)
        };
        self.queue_add_if_eligible(i);
    }

    /// 로컬에 이미 있는 조각으로 표시합니다.
    pub fn mark_have(&mut self, i: u32) -> Result<(), SwarmError> {
        if i >= self.total {
            return Err(SwarmError::IndexOutOfRange);
        }
        if self.have.set(i) {
            self.have_count += 1;
            self.queue_remove(i);
        }
        Ok(())
    }

    /// 피어의 보유 비트맵을 넣거나 바꿉니다(`BlobHave` 응답).
    pub fn set_peer_have(&mut self, peer: P, have: Bitmap) -> Result<(), SwarmError> {
        if have.len() != self.total {
            return Err(SwarmError::BitmapMismatch);
        }
        let old = self.peers.remove(&peer);
        let (counted, state) = match old {
            Some(st) => {
                if !st.excluded {
                    for i in st.have.iter_ones() {
                        self.change_avail(i, false);
                    }
                }
                (!st.excluded, PeerState { have, ..st })
            }
            None => (
                true,
                PeerState {
                    have,
                    in_flight: 0,
                    bad_chunks: 0,
                    strikes: 0,
                    successes: 0,
                    excluded: false,
                    busy_until: 0,
                    window: None,
                },
            ),
        };
        if counted {
            for i in state.have.iter_ones() {
                self.change_avail(i, true);
            }
        }
        self.peers.insert(peer, state);
        Ok(())
    }

    /// 피어가 조각 하나를 새로 가졌다고 알림.
    pub fn peer_has(&mut self, peer: &P, i: u32) -> Result<(), SwarmError> {
        if i >= self.total {
            return Err(SwarmError::IndexOutOfRange);
        }
        let Some(st) = self.peers.get_mut(peer) else {
            return Ok(());
        };
        if st.have.set(i) && !st.excluded {
            self.change_avail(i, true);
        }
        Ok(())
    }

    /// 피어가 떠남. 그 피어에게 요청 중이던 조각은 다시 대기열로 돌아갑니다(돌려준 목록).
    pub fn remove_peer(&mut self, peer: &P) -> Vec<u32> {
        let cancelled = self.drop_in_flight_of(peer);
        if let Some(st) = self.peers.remove(peer)
            && !st.excluded
        {
            for i in st.have.iter_ones() {
                self.change_avail(i, false);
            }
        }
        cancelled.into_iter().map(|(_, i)| i).collect()
    }

    fn drop_in_flight_of(&mut self, peer: &P) -> Vec<(P, u32)> {
        let chunks: Vec<u32> = self
            .in_flight
            .iter()
            .filter(|(_, v)| v.iter().any(|(p, _)| p == peer))
            .map(|(i, _)| *i)
            .collect();
        let mut out = Vec::new();
        for i in chunks {
            self.remove_request(peer, i);
            out.push((peer.clone(), i));
        }
        out
    }

    /// 요청 하나를 지우고 카운터를 맞춥니다. 그 조각에 남은 요청이 없으면 대기열로 돌립니다.
    fn remove_request(&mut self, peer: &P, i: u32) -> bool {
        let Some(list) = self.in_flight.get_mut(&i) else {
            return false;
        };
        let Some(pos) = list.iter().position(|(p, _)| p == peer) else {
            return false;
        };
        list.remove(pos);
        if list.is_empty() {
            self.in_flight.remove(&i);
        }
        self.in_flight_total = self.in_flight_total.saturating_sub(1);
        if let Some(st) = self.peers.get_mut(peer) {
            st.in_flight = st.in_flight.saturating_sub(1);
        }
        self.queue_add_if_eligible(i);
        true
    }

    fn exclude(&mut self, peer: &P) -> Vec<(P, u32)> {
        let Some(st) = self.peers.get_mut(peer) else {
            return Vec::new();
        };
        if st.excluded {
            return Vec::new();
        }
        st.excluded = true;
        let ones: Vec<u32> = st.have.iter_ones().collect();
        for i in ones {
            self.change_avail(i, false);
        }
        self.drop_in_flight_of(peer)
    }

    fn record_failure(&mut self, peer: &P, kind: Failure) -> (bool, Vec<(P, u32)>) {
        let Some(st) = self.peers.get_mut(peer) else {
            return (false, Vec::new());
        };
        let count = match kind {
            Failure::BadChunk => &mut st.bad_chunks,
            Failure::Transient => &mut st.strikes,
        };
        *count = count.saturating_add(1);
        if *count >= self.cfg.max_failures && !st.excluded {
            let cancel = self.exclude(peer);
            return (true, cancel);
        }
        (false, Vec::new())
    }

    /// 조각을 맡을 피어: 쓸 수 있고 그 조각을 가진 피어 중 (진행 중 요청, 실패, 성공 많은 순, 피어 순).
    fn pick_peer(&self, i: u32, now_ms: u64, skip: &[(P, u64)]) -> Option<P> {
        self.peers
            .iter()
            .filter(|(p, st)| {
                !st.excluded
                    && st.busy_until <= now_ms
                    && st.in_flight < st.window.unwrap_or(self.cfg.max_in_flight_per_peer)
                    && st.have.get(i)
                    && !skip.iter().any(|(q, _)| q == *p)
            })
            .min_by(|(pa, a), (pb, b)| {
                (
                    a.in_flight,
                    a.failures(),
                    std::cmp::Reverse(a.successes),
                    *pa,
                )
                    .cmp(&(
                        b.in_flight,
                        b.failures(),
                        std::cmp::Reverse(b.successes),
                        *pb,
                    ))
            })
            .map(|(p, _)| p.clone())
    }

    fn assign(&mut self, peer: P, i: u32, now_ms: u64) {
        self.queue_remove(i);
        let deadline = now_ms.saturating_add(self.cfg.request_timeout_ms);
        if let Some(st) = self.peers.get_mut(&peer) {
            st.in_flight += 1;
        }
        self.in_flight.entry(i).or_default().push((peer, deadline));
        self.in_flight_total += 1;
    }

    /// 지금 보낼 요청들(피어, 조각 번호). 보낸 것으로 기록되므로 그대로 보내면 됩니다.
    pub fn next_requests(&mut self, now_ms: u64) -> Vec<(P, u32)> {
        let mut out = Vec::new();
        if self.is_complete() {
            return out;
        }
        let mut free = self
            .cfg
            .max_in_flight_total
            .saturating_sub(self.in_flight_total);
        let candidates: Vec<u32> = self
            .queue
            .iter()
            .take(self.cfg.scan_limit)
            .map(|(_, _, i)| *i)
            .collect();
        for i in candidates {
            if free == 0 {
                break;
            }
            if let Some(p) = self.pick_peer(i, now_ms, &[]) {
                self.assign(p.clone(), i, now_ms);
                out.push((p, i));
                free -= 1;
            }
        }
        // 마지막 단계: 새로 요청할 조각이 없으면 진행 중인 조각을 다른 피어에게도 요청합니다.
        if self.queue.is_empty() && free > 0 {
            let chunks: Vec<u32> = self.in_flight.keys().copied().collect();
            for i in chunks {
                if free == 0 {
                    break;
                }
                let current = self.in_flight.get(&i).cloned().unwrap_or_default();
                if current.len() as u32 >= self.cfg.endgame_duplicates {
                    continue;
                }
                let oldest_start = current
                    .iter()
                    .map(|(_, d)| d.saturating_sub(self.cfg.request_timeout_ms))
                    .min()
                    .unwrap_or(now_ms);
                if now_ms < oldest_start.saturating_add(self.cfg.endgame_delay_ms) {
                    continue;
                }
                if let Some(p) = self.pick_peer(i, now_ms, &current) {
                    self.assign(p.clone(), i, now_ms);
                    out.push((p, i));
                    free -= 1;
                }
            }
        }
        out
    }

    /// 받은 조각의 검증 결과를 넣습니다(`verified` = 매니페스트 해시 일치).
    pub fn on_chunk(&mut self, peer: &P, i: u32, verified: bool) -> ChunkOutcome<P> {
        let requested = self
            .in_flight
            .get(&i)
            .is_some_and(|v| v.iter().any(|(p, _)| p == peer));
        if !requested {
            return ChunkOutcome::Unexpected;
        }
        self.remove_request(peer, i);
        if verified {
            if let Some(st) = self.peers.get_mut(peer) {
                st.successes += 1;
                st.strikes = 0;
                // 조각을 보냈으니 가진 것이 확실합니다.
                if st.have.set(i) && !st.excluded {
                    self.change_avail(i, true);
                }
            }
            if self.have.get(i) {
                return ChunkOutcome::Duplicate;
            }
            self.have.set(i);
            self.have_count += 1;
            self.queue_remove(i);
            let others: Vec<(P, u64)> = self.in_flight.get(&i).cloned().unwrap_or_default();
            let mut cancel = Vec::new();
            for (q, _) in others {
                self.remove_request(&q, i);
                cancel.push((q, i));
            }
            ChunkOutcome::Accepted {
                complete: self.is_complete(),
                cancel,
            }
        } else {
            let (excluded, cancel) = self.record_failure(peer, Failure::BadChunk);
            ChunkOutcome::BadChunk { excluded, cancel }
        }
    }

    /// 요청이 실패했을 때. 실패로 제외됐으면 그 피어에게 보낸 다른 요청 목록을 돌려줍니다.
    pub fn on_error(&mut self, peer: &P, i: u32, err: RequestError, now_ms: u64) -> Vec<(P, u32)> {
        if !self.remove_request(peer, i) {
            return Vec::new();
        }
        match err {
            RequestError::Busy { retry_after_ms } => {
                if let Some(st) = self.peers.get_mut(peer) {
                    st.busy_until = now_ms.saturating_add(u64::from(retry_after_ms));
                }
                Vec::new()
            }
            RequestError::NotFound => {
                let cleared = self
                    .peers
                    .get_mut(peer)
                    .is_some_and(|st| st.have.clear(i) && !st.excluded);
                if cleared {
                    self.change_avail(i, false);
                }
                Vec::new()
            }
            RequestError::Denied => {
                let ones: Vec<u32> = match self.peers.get_mut(peer) {
                    Some(st) if !st.excluded => {
                        let ones = st.have.iter_ones().collect();
                        st.have = Bitmap::new(self.total);
                        ones
                    }
                    _ => Vec::new(),
                };
                for j in ones {
                    self.change_avail(j, false);
                }
                self.drop_in_flight_of(peer)
            }
            RequestError::Failed => self.record_failure(peer, Failure::Transient).1,
        }
    }

    /// 기한이 지난 요청을 일시적 실패로 처리합니다. 돌려주는 목록은 더 이상 추적하지 않는 요청 전부:
    /// 기한이 지난 것과, 그 때문에 제외된 피어에게 보낸 아직 기한 전인 요청(취소해도 됨).
    pub fn check_timeouts(&mut self, now_ms: u64) -> Vec<(P, u32)> {
        let expired: Vec<(P, u32)> = self
            .in_flight
            .iter()
            .flat_map(|(i, v)| {
                v.iter()
                    .filter(|(_, d)| *d <= now_ms)
                    .map(move |(p, _)| (p.clone(), *i))
            })
            .collect();
        let mut out = Vec::with_capacity(expired.len());
        for (p, i) in expired {
            // 앞선 시간 초과로 피어가 제외되며 이미 지워진 요청이면 아래 취소 목록에 들어 있습니다.
            if self.remove_request(&p, i) {
                let (_, cancel) = self.record_failure(&p, Failure::Transient);
                out.push((p, i));
                out.extend(cancel);
            }
        }
        out
    }

    /// 가장 이른 요청 기한(타이머용).
    pub fn next_deadline(&self) -> Option<u64> {
        self.in_flight
            .values()
            .flat_map(|v| v.iter().map(|(_, d)| *d))
            .min()
    }

    /// 동시 요청 한도를 바꿉니다(받을 피어가 바뀌어 연결 종류가 달라질 때). 이미 보낸 요청은 그대로 두고, 한도를 넘는
    /// 만큼은 답이 와서 줄어들 때까지 새로 보내지 않습니다.
    pub fn set_window(&mut self, per_peer: u32, total: u32) {
        self.cfg.max_in_flight_per_peer = per_peer.max(1);
        self.cfg.max_in_flight_total = total.max(1);
    }

    /// 피어마다 따로 둘 동시 요청 한도를 정합니다(`None`이면 [`set_window`](Self::set_window)의 피어 한도). 한 블롭을
    /// 빠른 연결과 느린 연결로 섞어 받을 때 빠른 피어가 느린 쪽 한도에 묶이지 않게 합니다. 따로 둔 (제외되지 않은)
    /// 피어들의 한도 합을 돌려줍니다(전체 한도에 더할 몫).
    pub fn set_peer_windows(&mut self, f: impl Fn(&P) -> Option<u32>) -> u32 {
        let mut sum = 0u32;
        for (p, st) in self.peers.iter_mut() {
            st.window = f(p).map(|w| w.max(1));
            if !st.excluded {
                sum = sum.saturating_add(st.window.unwrap_or(0));
            }
        }
        sum
    }

    /// `now_ms` 뒤에 [`next_requests`](Self::next_requests)가 새 요청을 낼 수 있게 되는 가장 이른 시각(타이머용):
    /// 바쁘다고 답해 쉬는 피어가 풀리는 때와, 마지막 단계에서 진행 중인 조각을 다른 피어에게도 요청할 수 있게 되는 때.
    /// 이 시각에 깨지 않으면 진행 중인 요청이 없을 때 다음 비트맵 조회(최대 10초)까지 멈춥니다.
    pub fn next_wake(&self, now_ms: u64) -> Option<u64> {
        if self.is_complete() {
            return None;
        }
        let busy = self
            .peers
            .values()
            .filter(|st| !st.excluded && st.busy_until > now_ms)
            .map(|st| st.busy_until)
            .min();
        let endgame = if self.queue.is_empty() {
            self.in_flight
                .values()
                .filter(|v| (v.len() as u32) < self.cfg.endgame_duplicates)
                .filter_map(|v| {
                    v.iter()
                        .map(|(_, d)| d.saturating_sub(self.cfg.request_timeout_ms))
                        .min()
                })
                .map(|start| start.saturating_add(self.cfg.endgame_delay_ms))
                .filter(|at| *at > now_ms)
                .min()
        } else {
            None
        };
        busy.into_iter().chain(endgame).min()
    }

    pub fn is_complete(&self) -> bool {
        self.have_count == self.total
    }

    /// (받은 조각 수, 전체 조각 수).
    pub fn progress(&self) -> (u32, u32) {
        (self.have_count, self.total)
    }

    /// 내 보유 비트맵(다른 피어의 `BlobHave`에 답할 때).
    pub fn local_bitmap(&self) -> &Bitmap {
        &self.have
    }

    /// 아직 필요한데 가진 피어가 없는 조각 수('시더 없음' 표시용).
    pub fn unavailable_chunks(&self) -> u32 {
        (0..self.total)
            .filter(|&i| !self.have.get(i) && self.avail[i as usize] == 0)
            .count() as u32
    }

    pub fn in_flight(&self) -> u32 {
        self.in_flight_total
    }

    /// 제외되지 않은 피어 수.
    pub fn usable_peers(&self) -> usize {
        self.peers.values().filter(|s| !s.excluded).count()
    }

    pub fn is_excluded(&self, peer: &P) -> bool {
        self.peers.get(peer).is_some_and(|s| s.excluded)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn peer_window_overrides_the_shared_per_peer_limit() {
        let cfg = SwarmConfig {
            max_in_flight_per_peer: 1,
            max_in_flight_total: 2,
            ..SwarmConfig::default()
        };
        let mut s = ChunkScheduler::<u8>::new(32, cfg).unwrap();
        s.set_peer_have(1, Bitmap::full(32)).unwrap();
        s.set_peer_have(2, Bitmap::full(32)).unwrap();
        let extra = s.set_peer_windows(|p| (*p == 2).then_some(8));
        assert_eq!(extra, 8);
        s.set_window(1, 1 + extra);
        let reqs = s.next_requests(0);
        let count = |p: u8| reqs.iter().filter(|(q, _)| *q == p).count();
        assert_eq!((count(1), count(2)), (1, 8), "{reqs:?}");
        // 풀면 다시 함께 쓰는 한도.
        assert_eq!(s.set_peer_windows(|_| None), 0);
        s.set_window(1, 9);
        assert!(s.next_requests(0).is_empty(), "이미 한도를 넘게 보내 둠");
    }

    fn bm(total: u32, ones: &[u32]) -> Bitmap {
        let mut b = Bitmap::new(total);
        for i in ones {
            b.set(*i);
        }
        b
    }

    #[test]
    fn bitmap_wire_format_is_lsb_first() {
        let b = bm(10, &[0, 3, 8]);
        let (bytes, total) = b.to_wire();
        assert_eq!(total, 10);
        assert_eq!(bytes, vec![0b0000_1001, 0b0000_0001]);
        let back = Bitmap::from_wire(&bytes, 10).unwrap();
        assert_eq!(back, b);
        assert_eq!(back.iter_ones().collect::<Vec<_>>(), vec![0, 3, 8]);
        // 남는 비트는 지움.
        let noisy = Bitmap::from_wire(&[0xFF, 0xFF], 10).unwrap();
        assert_eq!(noisy.count_ones(), 10);
        assert!(noisy.is_full());
        assert_eq!(Bitmap::full(10), noisy);
        assert!(!noisy.get(10));
        assert_eq!(Bitmap::from_wire(&[0], 10), Err(SwarmError::BitmapMismatch));
        assert_eq!(
            Bitmap::from_wire(&[], MAX_CHUNKS as u32 + 1),
            Err(SwarmError::TooManyChunks)
        );
        let empty = Bitmap::from_wire(&[], 0).unwrap();
        assert!(empty.is_empty() && empty.is_full());
    }

    #[test]
    fn rarest_first() {
        let mut s: ChunkScheduler<u8> = ChunkScheduler::new(
            4,
            SwarmConfig {
                max_in_flight_total: 1,
                ..SwarmConfig::default()
            },
        )
        .unwrap();
        s.set_peer_have(1, bm(4, &[0, 1, 2, 3])).unwrap();
        s.set_peer_have(2, bm(4, &[0, 1, 2])).unwrap();
        s.set_peer_have(3, bm(4, &[0, 1])).unwrap();
        // 조각 3은 피어 1만 가짐 → 가장 희귀.
        assert_eq!(s.next_requests(0), vec![(1, 3)]);
        assert!(s.next_requests(0).is_empty());
        assert!(matches!(
            s.on_chunk(&1, 3, true),
            ChunkOutcome::Accepted {
                complete: false,
                ..
            }
        ));
        // 다음은 조각 2(두 피어).
        let next = s.next_requests(0);
        assert_eq!(next.len(), 1);
        assert_eq!(next[0].1, 2);
    }

    #[test]
    fn respects_in_flight_limits() {
        let cfg = SwarmConfig {
            max_in_flight_per_peer: 2,
            max_in_flight_total: 3,
            ..SwarmConfig::default()
        };
        let mut s: ChunkScheduler<u8> = ChunkScheduler::new(10, cfg).unwrap();
        s.set_peer_have(1, Bitmap::full(10)).unwrap();
        s.set_peer_have(2, Bitmap::full(10)).unwrap();
        let reqs = s.next_requests(0);
        assert_eq!(reqs.len(), 3);
        let to1 = reqs.iter().filter(|(p, _)| *p == 1).count();
        let to2 = reqs.iter().filter(|(p, _)| *p == 2).count();
        assert!(to1 <= 2 && to2 <= 2);
        assert_eq!(s.in_flight(), 3);
        assert!(s.next_requests(0).is_empty());
    }

    #[test]
    fn downloads_to_completion_with_bad_peer_excluded() {
        let mut s: ChunkScheduler<u8> = ChunkScheduler::new(20, SwarmConfig::default()).unwrap();
        s.set_peer_have(1, Bitmap::full(20)).unwrap();
        s.set_peer_have(66, Bitmap::full(20)).unwrap(); // 66은 늘 깨진 조각을 보냄
        let mut now = 0;
        let mut excluded = false;
        for _ in 0..100 {
            let reqs = s.next_requests(now);
            for (p, i) in reqs {
                match s.on_chunk(&p, i, p != 66) {
                    ChunkOutcome::BadChunk { excluded: e, .. } => excluded |= e,
                    ChunkOutcome::Unexpected => {} // 제외될 때 취소된 요청
                    _ => {}
                }
            }
            if s.is_complete() {
                break;
            }
            now += 10;
        }
        assert!(s.is_complete());
        assert!(excluded);
        assert!(s.is_excluded(&66));
        assert_eq!(s.usable_peers(), 1);
        assert_eq!(s.progress(), (20, 20));
        assert!(s.local_bitmap().is_full());
    }

    #[test]
    fn timeouts_requeue_and_count_as_failures() {
        let cfg = SwarmConfig {
            request_timeout_ms: 100,
            max_failures: 2,
            ..SwarmConfig::default()
        };
        let mut s: ChunkScheduler<u8> = ChunkScheduler::new(2, cfg).unwrap();
        s.set_peer_have(1, Bitmap::full(2)).unwrap();
        s.set_peer_have(2, Bitmap::full(2)).unwrap();
        let first = s.next_requests(0);
        assert_eq!(first.len(), 2);
        assert_eq!(s.next_deadline(), Some(100));
        assert!(s.check_timeouts(99).is_empty());
        let expired = s.check_timeouts(100);
        assert_eq!(expired.len(), 2);
        assert_eq!(s.in_flight(), 0);
        // 다시 요청 가능.
        assert_eq!(s.next_requests(100).len(), 2);
        s.check_timeouts(200);
        // 두 번 실패 → 둘 다 제외 → 더 요청할 곳 없음.
        assert_eq!(s.usable_peers(), 0);
        assert!(s.next_requests(200).is_empty());
        assert_eq!(s.unavailable_chunks(), 2);
    }

    /// 하나뿐인 시더가 큰 블롭을 주는 동안 가끔 시간 초과가 나도 제외되지 않습니다
    /// (일시적 실패는 성공하면 다시 0부터). 연달아 나면 제외됩니다.
    #[test]
    fn sporadic_timeouts_do_not_exclude_only_seeder() {
        let cfg = SwarmConfig {
            request_timeout_ms: 100,
            max_failures: 3,
            max_in_flight_per_peer: 1,
            max_in_flight_total: 1,
            ..SwarmConfig::default()
        };
        let total = 50;
        let mut s: ChunkScheduler<u8> = ChunkScheduler::new(total, cfg).unwrap();
        s.set_peer_have(1, Bitmap::full(total)).unwrap();
        let mut now = 0;
        let mut round = 0;
        while !s.is_complete() {
            let reqs = s.next_requests(now);
            assert_eq!(reqs.len(), 1, "시더가 제외되면 안 됨");
            let (p, i) = reqs[0];
            round += 1;
            if round % 3 == 0 {
                // 세 번에 한 번은 응답이 늦음(실패 2번 연속은 아님).
                now += 100;
                assert_eq!(s.check_timeouts(now), vec![(p, i)]);
            } else {
                s.on_chunk(&p, i, true);
            }
            assert!(round < 1000);
        }
        assert!(!s.is_excluded(&1));

        // 성공 없이 연달아 세 번이면 제외.
        let mut s: ChunkScheduler<u8> = ChunkScheduler::new(5, cfg).unwrap();
        s.set_peer_have(1, Bitmap::full(5)).unwrap();
        let mut now = 0;
        for _ in 0..3 {
            assert_eq!(s.next_requests(now).len(), 1);
            now += 100;
            s.check_timeouts(now);
        }
        assert!(s.is_excluded(&1));
        assert!(s.next_requests(now).is_empty());
    }

    /// 검증 실패는 사이사이 성공해도 줄지 않습니다(깨진 조각을 섞어 보내는 피어도 결국 제외).
    #[test]
    fn bad_chunks_accumulate_despite_successes() {
        let cfg = SwarmConfig {
            max_failures: 3,
            max_in_flight_per_peer: 1,
            max_in_flight_total: 1,
            ..SwarmConfig::default()
        };
        let mut s: ChunkScheduler<u8> = ChunkScheduler::new(100, cfg).unwrap();
        s.set_peer_have(1, Bitmap::full(100)).unwrap();
        let mut bad = 0;
        for n in 0.. {
            let reqs = s.next_requests(0);
            if reqs.is_empty() {
                break;
            }
            let (p, i) = reqs[0];
            let ok = n % 2 == 0;
            if !ok {
                bad += 1;
            }
            s.on_chunk(&p, i, ok);
        }
        assert_eq!(bad, 3);
        assert!(s.is_excluded(&1));
        assert!(!s.is_complete());
    }

    /// 시간 초과로 피어가 제외되면 그 피어에게 보낸, 아직 기한 전인 요청도 취소 목록에 담깁니다.
    #[test]
    fn timeout_exclusion_reports_cancelled_requests() {
        let cfg = SwarmConfig {
            request_timeout_ms: 100,
            max_failures: 1,
            max_in_flight_per_peer: 2,
            ..SwarmConfig::default()
        };
        let mut s: ChunkScheduler<u8> = ChunkScheduler::new(4, cfg).unwrap();
        s.set_peer_have(1, Bitmap::full(4)).unwrap();
        let first = s.next_requests(0);
        assert_eq!(first.len(), 2);
        // 하나는 받고, 빈자리에 기한이 더 늦은 요청을 하나 더 보냄.
        assert!(matches!(
            s.on_chunk(&1, first[0].1, true),
            ChunkOutcome::Accepted { .. }
        ));
        let later = s.next_requests(50);
        assert_eq!(later.len(), 1);
        // 첫 묶음의 남은 요청이 시간 초과 → 제외 → 기한 전인 요청도 취소 목록에.
        assert_eq!(s.check_timeouts(100), vec![first[1], later[0]]);
        assert!(s.is_excluded(&1));
        assert_eq!(s.in_flight(), 0);
        // 제외된 피어의 늦은 응답은 요청하지 않은 것으로 처리.
        assert_eq!(s.on_chunk(&1, later[0].1, true), ChunkOutcome::Unexpected);
    }

    #[test]
    fn busy_peer_rests_without_penalty() {
        let mut s: ChunkScheduler<u8> = ChunkScheduler::new(3, SwarmConfig::default()).unwrap();
        s.set_peer_have(1, Bitmap::full(3)).unwrap();
        let reqs = s.next_requests(0);
        assert_eq!(reqs.len(), 3);
        for (p, i) in reqs {
            assert!(
                s.on_error(
                    &p,
                    i,
                    RequestError::Busy {
                        retry_after_ms: 500
                    },
                    0
                )
                .is_empty()
            );
        }
        // 진행 중인 요청이 없어도 쉬는 피어가 돌아올 때 깨웁니다.
        assert_eq!(s.next_deadline(), None);
        assert_eq!(s.next_wake(0), Some(500));
        assert!(s.next_requests(499).is_empty());
        assert_eq!(s.next_wake(500), None);
        assert_eq!(s.next_requests(500).len(), 3);
        assert!(!s.is_excluded(&1));
    }

    #[test]
    fn wakes_when_endgame_duplicate_is_allowed() {
        let mut s: ChunkScheduler<u8> = ChunkScheduler::new(1, SwarmConfig::default()).unwrap();
        s.set_peer_have(1, Bitmap::full(1)).unwrap();
        s.set_peer_have(2, Bitmap::full(1)).unwrap();
        assert_eq!(s.next_requests(100).len(), 1);
        assert_eq!(s.next_wake(100), Some(2_100));
        assert!(s.next_requests(2_099).is_empty());
        assert_eq!(s.next_requests(2_100).len(), 1);
        assert_eq!(s.next_wake(2_100), None);
    }

    #[test]
    fn direct_window_follows_measured_rate() {
        const CHUNK: u32 = 256 * 1024;
        assert_eq!(direct_window(None, CHUNK), DIRECT_WINDOW_START);
        // 느린 회선(초당 100KiB): 최소에 가깝게 두어 줄 선 요청이 시간 제한에 걸리지 않게.
        assert_eq!(direct_window(Some(100 * 1024), CHUNK), 3);
        // 초당 10MiB: 0.5초 분량 20조각 + 2.
        assert_eq!(direct_window(Some(10 * 1024 * 1024), CHUNK), 22);
        // 기가비트: 상한.
        assert_eq!(
            direct_window(Some(120 * 1024 * 1024), CHUNK),
            DIRECT_WINDOW_MAX
        );
        assert_eq!(direct_window(Some(0), CHUNK), DIRECT_WINDOW_MIN);
    }

    #[test]
    fn rate_meter_measures_recent_bytes() {
        let mut m = RateMeter::default();
        assert_eq!(m.rate(0), None);
        m.record(0, 1000);
        assert_eq!(m.rate(0), None);
        m.record(500, 1000);
        assert_eq!(m.rate(500), Some(2000));
        // 1초가 지난 표본은 빠지고(둘은 남김), 조용하면 내려갑니다.
        m.record(1_400, 1000);
        assert_eq!(m.rate(1_400), Some(1000 * 1000 / 900));
        assert_eq!(m.rate(5_000), Some(1000 * 1000 / 4_500));
    }

    #[test]
    fn window_change_applies_to_next_requests() {
        let mut s: ChunkScheduler<u8> = ChunkScheduler::new(40, SwarmConfig::default()).unwrap();
        s.set_peer_have(1, Bitmap::full(40)).unwrap();
        assert_eq!(s.next_requests(0).len(), 4);
        s.set_window(16, 32);
        assert_eq!(s.next_requests(0).len(), 12);
        // 줄이면 이미 보낸 것은 두고 더 보내지 않습니다.
        s.set_window(3, 4);
        assert!(s.next_requests(0).is_empty());
        assert_eq!(s.in_flight(), 16);
    }

    #[test]
    fn not_found_and_denied_update_availability() {
        let mut s: ChunkScheduler<u8> = ChunkScheduler::new(2, SwarmConfig::default()).unwrap();
        s.set_peer_have(1, Bitmap::full(2)).unwrap();
        s.set_peer_have(2, bm(2, &[1])).unwrap();
        let reqs = s.next_requests(0);
        // 조각 0은 피어 1만, 조각 1은 둘 다 → 0이 먼저, 1은 덜 바쁜 쪽.
        assert_eq!(reqs, vec![(1, 0), (2, 1)]);
        s.on_error(&1, 0, RequestError::NotFound, 0);
        assert_eq!(s.unavailable_chunks(), 1);
        s.on_error(&2, 1, RequestError::Denied, 0);
        // 피어 1은 조각 1을 여전히 가짐.
        assert_eq!(s.next_requests(0), vec![(1, 1)]);
        assert!(matches!(
            s.on_chunk(&1, 1, true),
            ChunkOutcome::Accepted { .. }
        ));
        assert!(!s.is_complete());
        // 피어 2가 나중에 조각 0을 얻음.
        s.peer_has(&2, 0).unwrap();
        assert_eq!(s.next_requests(0), vec![(2, 0)]);
        assert!(matches!(
            s.on_chunk(&2, 0, true),
            ChunkOutcome::Accepted { complete: true, .. }
        ));
    }

    #[test]
    fn endgame_duplicates_and_cancel() {
        let mut s: ChunkScheduler<u8> = ChunkScheduler::new(1, SwarmConfig::default()).unwrap();
        s.set_peer_have(1, Bitmap::full(1)).unwrap();
        s.set_peer_have(2, Bitmap::full(1)).unwrap();
        s.set_peer_have(3, Bitmap::full(1)).unwrap();
        let reqs = s.next_requests(0);
        assert_eq!(reqs.len(), 1);
        // 오래 걸리면 다른 피어 하나에게 더 요청(최대 두 피어).
        assert!(s.next_requests(1_999).is_empty());
        let more = s.next_requests(2_000);
        assert_eq!(more.len(), 1);
        assert!(s.next_requests(10_000).is_empty());
        let (first, second) = (reqs[0].0, more[0].0);
        assert_ne!(first, second);
        match s.on_chunk(&second, 0, true) {
            ChunkOutcome::Accepted { complete, cancel } => {
                assert!(complete);
                assert_eq!(cancel, vec![(first, 0)]);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(s.on_chunk(&first, 0, true), ChunkOutcome::Unexpected);
        assert_eq!(s.in_flight(), 0);
    }

    #[test]
    fn unexpected_and_resume() {
        let mut s: ChunkScheduler<u8> =
            ChunkScheduler::with_local(bm(4, &[0, 1]), SwarmConfig::default()).unwrap();
        assert_eq!(s.progress(), (2, 4));
        assert_eq!(s.on_chunk(&9, 2, true), ChunkOutcome::Unexpected);
        s.set_peer_have(9, Bitmap::full(4)).unwrap();
        let mut got: Vec<u32> = s.next_requests(0).into_iter().map(|(_, i)| i).collect();
        got.sort();
        assert_eq!(got, vec![2, 3]);
        assert_eq!(s.remove_peer(&9).len(), 2);
        assert_eq!(s.in_flight(), 0);
        assert!(s.next_requests(0).is_empty());
        assert_eq!(
            s.set_peer_have(1, Bitmap::new(5)),
            Err(SwarmError::BitmapMismatch)
        );
        assert_eq!(s.mark_have(4), Err(SwarmError::IndexOutOfRange));
    }

    #[test]
    fn tie_break_is_deterministic_per_seed() {
        let order = |seed: u64| {
            let cfg = SwarmConfig {
                seed,
                max_in_flight_total: 64,
                max_in_flight_per_peer: 64,
                ..SwarmConfig::default()
            };
            let mut s: ChunkScheduler<u8> = ChunkScheduler::new(64, cfg).unwrap();
            s.set_peer_have(1, Bitmap::full(64)).unwrap();
            s.next_requests(0)
                .into_iter()
                .map(|(_, i)| i)
                .collect::<Vec<_>>()
        };
        assert_eq!(order(7), order(7));
        assert_ne!(order(7), order(8));
        let o = order(7);
        assert_ne!(o, (0..64).collect::<Vec<_>>(), "같은 희귀도는 섞여야 함");
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(48))]

        /// 무작위 피어 보유·실패가 있어도, 모든 조각을 가진 정직한 피어가 있으면 결국 다 받습니다.
        #[test]
        fn eventually_completes(
            total in 1u32..200,
            seed in any::<u64>(),
            peers in proptest::collection::vec((any::<u64>(), 0u8..4), 1..6),
        ) {
            let mut s: ChunkScheduler<u8> =
                ChunkScheduler::new(total, SwarmConfig { seed, ..SwarmConfig::default() }).unwrap();
            s.set_peer_have(0, Bitmap::full(total)).unwrap();
            for (n, (mask, behavior)) in peers.iter().enumerate() {
                let mut b = Bitmap::new(total);
                for i in 0..total {
                    if splitmix64(mask ^ u64::from(i)).is_multiple_of(2) {
                        b.set(i);
                    }
                }
                s.set_peer_have(n as u8 + 1, b).unwrap();
                let _ = behavior;
            }
            let mut now = 0u64;
            for _ in 0..10_000 {
                if s.is_complete() {
                    break;
                }
                for (p, i) in s.next_requests(now) {
                    // 0번은 정직, 나머지는 행동 값에 따라 깨진 조각·없음·바쁨·정상.
                    let behavior = if p == 0 { 3 } else { peers[(p - 1) as usize].1 };
                    match behavior {
                        0 => { s.on_chunk(&p, i, false); }
                        1 => { s.on_error(&p, i, RequestError::NotFound, now); }
                        2 => { s.on_error(&p, i, RequestError::Busy { retry_after_ms: 50 }, now); }
                        _ => { s.on_chunk(&p, i, true); }
                    }
                }
                now += 25;
                s.check_timeouts(now);
            }
            prop_assert!(s.is_complete());
            prop_assert_eq!(s.in_flight(), 0);
        }
    }
}
