//! 블롭(B3): 첨부 조각 저장·전송, 프로필 이미지, 링크 미리보기, 자동 내려받기.
//!
//! 코어가 부르는 고리:
//! - [`prepare_attachments`]: `message.send`의 첨부 경로 → 암호화된 블롭과 `Attachment`.
//! - [`link_previews`]: 보내는 쪽 링크 미리보기(`preview`).
//! - [`import_profile_image`], [`profile_image_key`]: `profile.set`의 이미지와 서명된 프로필에 실을 키.
//! - [`is_downloaded`]: 메시지 보기의 `downloaded`.
//! - [`on_message_applied`]: 메시지가 보기 행에 들어간 뒤(어느 메시지의 블롭인지 기록, 자동 내려받기 판단).
//! - [`handle_request`]: 들어온 `BlobManifest`·`BlobHave`·`BlobChunk`(`serve`).
//! - [`import_bytes`]: 메모리의 바이트를 블롭으로(B1의 공간 이모지).
//! - [`import_space_image`]: 크루 아이콘 이미지 → 그 공간의 블롭과 키(키는 `SpaceSettings::icon_key`에 실림).
//!
//! 저장(설정 표, JSON):
//! - `blob.ref.<블롭>`: 그 블롭을 실은 메시지(공간·채널·이벤트·논리 시계·종류·크기·키 확인 결과). 제공 검사와
//!   정리에 씀.
//! - `blob.want.<블롭>`: 받을 블롭. 재시작하면 저장소의 조각 비트맵부터 이어 받습니다.
//! - 비밀값 `blob.image.<블롭>`: 내 프로필 이미지 키(서명된 프로필에도 실림).
//!
//! 프로필 이미지는 공간에 속하지 않으므로 가상의 공간 [`PROFILE_SPACE`]에 등록합니다(공간을 지워도 남게).
//! 로그에는 블롭 ID 앞부분만 남기고 내용·키·파일 경로는 남기지 않습니다.
//!
//! 블롭이 어디서 쓰이는지는 다른 멤버가 정할 수 있으므로(메시지 첨부 목록, 이모지, 프로필에 아무 블롭 ID나 적을 수
//! 있음) 남에게 내주거나 지울 때는 그 주장을 그대로 믿지 않습니다.
//! - 블롭은 내가 그 공간에서 올렸거나 그 공간 멤버에게서 처음 받았을 때만 그 공간에 등록합니다
//!   (`blob_spaces`, 프로필 이미지는 [`PROFILE_SPACE`]). 제공은 등록된 공간에서만 합니다.
//! - 첨부를 실은 메시지는 그 메시지의 키로 가진 조각이 풀릴 때만 믿습니다(키를 모르고 ID만 베낀 메시지는 버림).
//!   이모지·프로필 이미지도 상태·서명된 프로필의 키로 풀릴 때만 그 쓰임으로 내줍니다.
//! - 정리는 키가 확인된 메시지가 모두 지워졌을 때만 하고, 이모지·아이콘·프로필 이미지로 쓰이는 블롭은 건드리지 않습니다.

mod fetch;
pub(crate) mod media;
mod prepare;
pub(crate) mod preview;
mod serve;
pub(crate) mod strip;
pub mod testing;

// X4: 저장 공간 정리, 미리 받기, 앱이 넘긴 동영상·음성 썸네일 검사.
pub(crate) mod cache;
mod hints;
pub(crate) mod prefetch;

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;
use tracing::debug;
use yangpa_core::body::{LinkEmbed, MessageBody, Secret32};
use yangpa_core::device::Profile;
use yangpa_core::event::EventHeader;
use yangpa_core::ids::{BlobId, ChannelId, EventId, SpaceId, UserId};
use yangpa_core::model::{SpaceKind, SpaceState};
use yangpa_core::time::Timestamp;
use yangpa_core::wire::{YpRequest, YpResponse};
use yangpa_net::PeerId;
use yangpa_sync::policy::{DataType, PeerPolicy, TokenBucket};
use yangpa_sync::priority::{RequestQueue, Work};
use yangpa_sync::score::{Penalty, ScoreBoard, ScoreConfig, Verdict};
use yangpa_sync::swarm::{ChunkScheduler, RateMeter, SwarmConfig};

use crate::node::Node;
use crate::policy;

pub(crate) use prepare::{
    Origin, check_attach_allowed, import_bytes, import_profile_image, import_space_image,
    prepare_attachments, relink, send_limit, size_text,
};

/// 프로필 이미지를 등록하는 가상의 공간(어느 공간에도 속하지 않음).
pub(crate) const PROFILE_SPACE: SpaceId = SpaceId([0; 16]);
/// 프로필 이미지(아바타·배너) 최대 크기.
pub(crate) const MAX_PROFILE_IMAGE_BYTES: u64 = 8 * 1024 * 1024;

const REF_PREFIX: &str = "blob.ref.";
const WANT_PREFIX: &str = "blob.want.";
const IMAGE_SECRET_PREFIX: &str = "blob.image.";
/// 블롭 하나에 기록하는 메시지 수(첨부마다 키가 달라 보통 하나). 처음 기록은 밀어내지 않습니다.
const MAX_REFS: usize = 8;
/// 쌓아 둘 수 있는 자동 내려받기 수(미룬 것은 빼고 셈, 직접 요청은 넘어도 받음).
const MAX_WANTS: usize = 10_000;
/// 열지 않은 채널이라 미뤄 둔 자동 내려받기 상한(전체·공간마다). 넘으면 가장 오래된 것부터 버립니다.
const MAX_DEFERRED: usize = 10_000;
const MAX_DEFERRED_PER_SPACE: usize = 1_000;
/// 키 확인 결과를 기억해 둘 (블롭, 키 지문) 수.
const MAX_KEY_CHECKS: usize = 4_096;
const SWEEP_INTERVAL: Duration = Duration::from_secs(60);
/// 이만큼의 정리 주기마다 지워진 메시지·떠난 공간의 첨부를 지웁니다.
const CLEANUP_EVERY: u64 = 10;
const RETRY_BASE_MS: u64 = 5_000;
const RETRY_MAX_MS: u64 = 5 * 60 * 1000;
/// 자동 내려받기를 포기하기 전 시도 수(직접 요청은 포기하지 않음).
const MAX_AUTO_ATTEMPTS: u32 = 12;
/// 시험용 통계를 이만큼의 블롭까지만 둡니다.
const MAX_STATS: usize = 256;

fn now_ms() -> u64 {
    Timestamp::now().0
}

/// 블롭을 실은 메시지(`blob.ref.<블롭>`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct BlobRef {
    pub space: SpaceId,
    pub channel: ChannelId,
    pub event: EventId,
    pub lamport: u64,
    pub data_type: DataType,
    /// 보낸 쪽이 적은 평문 크기.
    pub size: u64,
    /// 그 메시지의 키로 가진 조각이 풀리는지. `None`이면 아직 확인 못 함(가진 조각이 없었음).
    #[serde(default)]
    pub verified: Option<bool>,
}

/// 받을 블롭(`blob.want.<블롭>`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Want {
    /// 요청할 공간(그 공간 멤버 기기에게서 받음).
    pub space: SpaceId,
    #[serde(default)]
    pub channel: Option<ChannelId>,
    pub data_type: DataType,
    /// 알려진 평문 크기(모르면 받을 수 있는 최대).
    pub size: u64,
    /// 사용자가 직접 요청함(자동 다운로드 한도·휴면과 무관하게 받고, 포기하지 않음).
    pub manual: bool,
    pub priority: u8,
    /// 열지 않은 채널의 자동 내려받기라 그 채널을 열 때까지 미룸.
    #[serde(default)]
    pub deferred: bool,
    /// 프로필 이미지([`PROFILE_SPACE`]에 등록).
    #[serde(default)]
    pub profile: bool,
    /// 넣은 시각(미룬 것이 넘칠 때 오래된 것부터 버림).
    #[serde(default)]
    pub added_ms: u64,
    /// 미리 받기 보충(지난 메시지의 첨부): 자리를 만들려고 다른 첨부를 비우지 않습니다(`cache`).
    #[serde(default)]
    pub backfill: bool,
}

#[derive(Clone, Copy, Debug)]
struct Retry {
    /// 다시 시도할 시각. `None`이면 이미 대기열로 돌아감.
    at_ms: Option<u64>,
    attempts: u32,
}

/// 내려받기 관리자(대기열·진행 중·재시도).
struct Manager {
    /// 자동 내려받기 순서(피어 설정의 종류별 우선순위, 보고 있는 채널 먼저).
    queue: RequestQueue<BlobId>,
    /// 직접 요청: (우선순위 내림차순, 넣은 순서).
    manual: BTreeMap<(Reverse<u8>, u64), BlobId>,
    seq: u64,
    wants: HashMap<BlobId, Want>,
    active: HashSet<BlobId>,
    /// `active` 중 직접 연결로만 받는 것(Tor 한도와 따로 셈, [`fast_route`]).
    fast: HashSet<BlobId>,
    retry: HashMap<BlobId, Retry>,
    /// 휴면 공간이라 꺼내 둔 자동 내려받기.
    parked: HashSet<BlobId>,
    /// 자동 내려받기를 포기한 블롭(직접 요청하면 다시 받음).
    gave_up: HashSet<BlobId>,
    /// 휘발성 채널 첨부(X1, RAM 전용)에 빌려준 내려받기 자리. 같은 동시 한도를 나눠 씁니다.
    lent: usize,
    /// 그중 직접 연결로만 받는 것에 빌려준 자리(`fast`와 함께 셈).
    lent_fast: usize,
    /// 자리를 기다리는 휘발성 첨부가 있으면 일반 내려받기가 빈자리 하나를 남겨 둡니다(휘발성 첨부는 곧 사라짐).
    lend_wanted: bool,
}

impl Default for Manager {
    fn default() -> Self {
        Manager {
            queue: RequestQueue::new(queue_policy(&PeerPolicy::default())),
            manual: BTreeMap::new(),
            seq: 0,
            wants: HashMap::new(),
            active: HashSet::new(),
            fast: HashSet::new(),
            retry: HashMap::new(),
            parked: HashSet::new(),
            gave_up: HashSet::new(),
            lent: 0,
            lent_fast: 0,
            lend_wanted: false,
        }
    }
}

impl Manager {
    /// 받을 블롭을 대기열에 넣습니다(미룬 것은 빼고). 같은 블롭이 두 번 들어가도 꺼낼 때 걸러집니다.
    fn enqueue(&mut self, blob: BlobId) {
        let Some(w) = self.wants.get(&blob) else {
            return;
        };
        if w.deferred {
            return;
        }
        if w.manual {
            let key = (Reverse(w.priority), self.seq);
            self.seq += 1;
            self.manual.insert(key, blob);
        } else {
            let work = Work::Blob {
                space: w.space,
                channel: w.channel,
                data_type: w.data_type,
                size: w.size,
            };
            // 대기열 정책은 모든 종류를 받으므로 거절되지 않습니다(받을지는 넣기 전에 정함).
            let _ = self.queue.push(work, blob);
        }
    }

    fn pop(&mut self) -> Option<BlobId> {
        if let Some(key) = self.manual.keys().next().copied() {
            return self.manual.remove(&key);
        }
        self.queue.pop().map(|(_, b)| b)
    }
}

/// 대기열 순서만 정하는 정책: 받을지는 이미 정했으므로 종류·크기 제한 없이 우선순위만 씁니다.
fn queue_policy(p: &PeerPolicy) -> PeerPolicy {
    PeerPolicy {
        types: DataType::ALL.into_iter().collect(),
        priorities: p.priorities.clone(),
        download_limit_bps: None,
        auto_download_max_bytes: u64::MAX,
    }
}

/// 시험용 고리와 통계.
#[derive(Default)]
pub(crate) struct TestHooks {
    /// 보내는 조각의 한 바이트를 바꿉니다(받는 쪽 검증 시험).
    corrupt_uploads: AtomicBool,
    /// 받은 조각 + 요청 중인 조각이 이 수에 닿으면 더 요청하지 않습니다(이어 받기 시험).
    hold_at: Mutex<Option<u32>>,
    stats: Mutex<HashMap<BlobId, FetchStats>>,
    /// 받는 중인 블롭마다 조각을 가졌다고 답한 피어 수.
    seeders_seen: Mutex<HashMap<BlobId, usize>>,
    /// 올리는 첨부가 첫 조각을 쓴 뒤 멈출 곳(정리와 겹치는 시험, `testing::pause_uploads`).
    upload_pause: Mutex<Option<Arc<testing::UploadPause>>>,
    /// 미리 받기가 "새 메시지"로 보는 범위를 바꿉니다(`Some(0)`이면 모두 지난 메시지).
    live_ms: Mutex<Option<u64>>,
    /// 조각 답을 이만큼 늦춥니다(밀리초, 왕복이 긴 연결 흉내, `testing::delay_serving`).
    serve_delay_ms: AtomicU64,
    /// 늦춰 답하는 중인 조각 요청 수와, 늦춘 요청마다 왔을 때의 그 수(`testing::serving_overlaps`).
    serving: AtomicU32,
    serving_overlaps: Mutex<Vec<u32>>,
}

/// 블롭 하나를 받은 기록(시험용).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FetchStats {
    /// 피어별로 받아 저장한 조각 수.
    pub chunks_from: BTreeMap<PeerId, u32>,
    /// 검증에 실패한 조각 수.
    pub bad_chunks: u32,
    /// 시더가 바쁘다고 답한 조각 요청 수.
    pub busy_replies: u32,
}

/// 블롭 영역의 노드 수준 상태(`Node::blobs`).
#[derive(Default)]
pub struct BlobState {
    mgr: Mutex<Manager>,
    wake: Arc<Notify>,
    score: Mutex<Option<ScoreBoard<PeerId>>>,
    /// 수신 대역폭(키: 공간 피어 설정이 있으면 그 공간, 없으면 전역).
    buckets: Mutex<HashMap<Option<SpaceId>, TokenBucket>>,
    /// 프로필 이미지 → 그 이미지를 쓰는 사용자(저장된 프로필에서, 정리 주기마다 다시 만듦).
    profile_index: RwLock<HashMap<BlobId, BTreeSet<UserId>>>,
    /// `blob.status`의 온라인 시더 수 캐시: (공간, 블롭) → (확인 시각, 수).
    seeders: Mutex<HashMap<(SpaceId, BlobId), (u64, u32)>>,
    /// 키 확인 결과: (블롭, 키 지문) → 풀리는지. 블롭 ID가 내용 해시라 결과가 바뀌지 않습니다.
    key_checks: Mutex<HashMap<(BlobId, [u8; 32]), bool>>,
    sweeps: AtomicU64,
    /// 피어마다 지금 보내 둔 조각 요청 수(모든 내려받기 합계, [`chunk_ticket`]).
    chunks_out: Mutex<HashMap<PeerId, u32>>,
    /// 받은 첨부의 저장 공간 정리(고정·한도·내주는 중).
    cache: cache::CacheState,
    test: TestHooks,
}

// ───────────── 시작·대기열 ─────────────

pub(crate) fn start(node: &Arc<Node>) {
    let global = policy::peer_policy(node, None);
    let rows = node
        .store
        .settings_with_prefix(WANT_PREFIX)
        .unwrap_or_default();
    {
        let mut m = node.blobs.mgr.lock();
        m.queue.set_policy(queue_policy(&global));
        for (key, value) in rows {
            let Ok(blob) = key[WANT_PREFIX.len()..].parse::<BlobId>() else {
                continue;
            };
            let Ok(w) = serde_json::from_value::<Want>(value) else {
                continue;
            };
            m.wants.insert(blob, w);
            m.enqueue(blob);
        }
    }
    cache::load(node);
    spawn_pump(node);
    spawn_sweep(node);
}

/// 대기열에서 꺼내 내려받기를 시작하는 작업. 바뀐 것이 있으면 `wake`로 깨우고, 재시도 시각에도 깹니다.
fn spawn_pump(node: &Arc<Node>) {
    let weak = Arc::downgrade(node);
    let wake = node.blobs.wake.clone();
    node.tasks.spawn(async move {
        loop {
            let next = {
                let Some(node) = weak.upgrade() else { break };
                if node.is_shutting_down() {
                    break;
                }
                schedule(&node)
            };
            let wait = next.map_or(Duration::from_secs(30), |at| {
                Duration::from_millis(at.saturating_sub(now_ms()).clamp(10, 30_000))
            });
            tokio::select! {
                _ = wake.notified() => {}
                _ = tokio::time::sleep(wait) => {}
            }
        }
    });
}

/// 동시 내려받기 한도. 휘발성 첨부(X1)도 이 한도 안에서 자리를 빌립니다([`borrow_slot`]).
fn max_active(node: &Node) -> usize {
    let max = node.cfg.sharing.max_active_downloads.max(1);
    // Tor로 돌면 글이 파일 조각에 밀리지 않게 한 번에 하나만.
    crate::tor::tor_download_limits(node).map_or(max, |t| max.min(t.active.max(1)))
}

/// 블롭 하나의 조각 요청 겹침 수를 정하는 방식.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChunkWindow {
    /// 받을 피어가 모두 Tor를 거치지 않는 직접 연결(P2P 전용 채널·서로 신뢰): 잰 속도에 맞춤([`Pacer`]).
    Direct,
    /// 고정 (피어마다, 블롭 하나 전체).
    Fixed(u32, u32),
}

/// 받을 피어들로 정한 겹침 방식. 모두 직접 연결이면 잰 속도에 맞추고, 하나라도 Tor(단방향 포함)로 받으면 Tor 한도
/// (`TorDownloadLimits`), 직접 연결 스웜이면 기본값.
pub(crate) fn chunk_window(node: &Node, peers: &[PeerId]) -> ChunkWindow {
    if !peers.is_empty() && peers.iter().all(|p| crate::p2p::data_goes_direct(node, p)) {
        return ChunkWindow::Direct;
    }
    let d = SwarmConfig::default();
    match crate::tor::tor_download_limits(node) {
        Some(t) => ChunkWindow::Fixed(
            d.max_in_flight_per_peer.min(t.per_peer),
            d.max_in_flight_total.min(t.total),
        ),
        None => ChunkWindow::Fixed(d.max_in_flight_per_peer, d.max_in_flight_total),
    }
}

/// 직접 연결 블롭 하나가 모든 피어에 겹쳐 보낼 조각 요청 합계 상한(256KiB 조각이면 16MiB).
const DIRECT_TOTAL_MAX: u32 = 64;

/// 내려받기 하나의 겹침 수(일반 첨부·휘발성 첨부). 직접 연결이면 받는 속도를 재어 그 속도로 0.5초 받을 만큼 겹칩니다
/// (`yangpa_sync::swarm::direct_window`): 고정 한도로는 왕복이 긴 회선을 못 채우거나, 느린 회선에서 줄 선 요청이
/// 시간 제한에 걸립니다. 속도는 사용자가 정한 받기·올리기 한도와 회선만 묶습니다.
///
/// 직접 연결 상대와 Tor 상대에게서 함께 받으면(섞인 받기) 겹침은 Tor 한도대로 두되, 직접 연결 상대의 주 연결만
/// 잰 속도로 따로 겹칩니다. 그 상대에게는 파일 회로도 열지 않습니다(`filelink::Routes`).
pub(crate) struct Pacer {
    window: ChunkWindow,
    meter: RateMeter,
    chunk_size: u32,
    /// 섞인 받기에서 직접 연결로 닿는 상대(모두 직접 연결이면 비움).
    direct: Vec<PeerId>,
}

impl Pacer {
    pub(crate) fn new(chunk_size: u32) -> Self {
        let d = SwarmConfig::default();
        Pacer {
            window: ChunkWindow::Fixed(d.max_in_flight_per_peer, d.max_in_flight_total),
            meter: RateMeter::default(),
            chunk_size,
            direct: Vec::new(),
        }
    }

    /// 받을 상대들로 겹침 방식을 다시 정합니다(상대를 다시 물을 때마다: 신뢰가 바뀌어 직접 연결이 생기거나 끊김).
    pub(crate) fn update(&mut self, node: &Node, peers: &[PeerId]) {
        self.window = chunk_window(node, peers);
        self.direct = match self.window {
            ChunkWindow::Direct => Vec::new(),
            ChunkWindow::Fixed(..) => peers
                .iter()
                .copied()
                .filter(|p| crate::p2p::data_goes_direct(node, p))
                .collect(),
        };
    }

    /// 섞인 받기에서 직접 연결로 닿는 상대.
    pub(crate) fn direct_peers(&self) -> &[PeerId] {
        &self.direct
    }

    /// 조각 답이 닿을 때마다(검증 전, 겹쳐 받은 것도 회선을 쓴 것이라 셈).
    pub(crate) fn record(&mut self, now: u64, bytes: usize) {
        self.meter.record(now, bytes as u64);
    }

    /// 직접 연결로만 받는 중인지. 이때는 Tor 파일 회로가 필요 없습니다.
    pub(crate) fn is_direct(&self) -> bool {
        self.window == ChunkWindow::Direct
    }

    /// 다음 요청을 내기 전에 부릅니다. `fixed`는 Tor 한도 대신 쓸 고정 한도(파일 회로가 이어졌을 때 회로별 한도).
    pub(crate) fn apply(
        &mut self,
        sched: &mut ChunkScheduler<crate::filelink::Route>,
        now: u64,
        fixed: Option<(u32, u32)>,
    ) {
        let (per_peer, total) = match (self.window, fixed) {
            (ChunkWindow::Fixed(..), Some(f)) => f,
            (ChunkWindow::Fixed(p, t), None) => (p, t),
            (ChunkWindow::Direct, _) => {
                let w = self.direct_window(now);
                let peers = u32::try_from(sched.usable_peers().max(1)).unwrap_or(u32::MAX);
                (w, w.saturating_mul(peers).min(DIRECT_TOTAL_MAX))
            }
        };
        let extra = if self.direct.is_empty() {
            sched.set_peer_windows(|_| None)
        } else {
            let w = self.direct_window(now);
            let direct = &self.direct;
            sched.set_peer_windows(|(p, route)| {
                (*route == crate::filelink::MAIN && direct.contains(p)).then_some(w)
            })
        };
        sched.set_window(per_peer, total.saturating_add(extra.min(DIRECT_TOTAL_MAX)));
    }

    fn direct_window(&mut self, now: u64) -> u32 {
        yangpa_sync::swarm::direct_window(self.meter.rate(now), self.chunk_size)
    }
}

/// 이 노드가 피어 하나에 동시에 보내 둘 조각 요청 상한(모든 내려받기 합계). 망 계층의 피어당 요청 상한
/// (`yangpa_net` 32개)보다 작게 두어, 파일 여러 개를 함께 받는 동안에도 같은 피어에게 글·동기화 요청 자리가 남습니다.
const CHUNKS_PER_PEER: u32 = yangpa_sync::swarm::DIRECT_WINDOW_MAX;

/// 피어당 조각 상한이 찼거나 망 계층이 바쁘다고 했을 때(`NetError::Busy`) 그 피어를 쉬게 하는 시간. 이 노드 안의
/// 사정이라 감점하지 않습니다.
pub(crate) const LOCAL_BUSY_MS: u32 = 50;

/// 피어 하나에 보내 둔 조각 요청 하나. 답이 오면(놓으면) 자리를 돌려줍니다.
pub(crate) struct ChunkTicket {
    node: Arc<Node>,
    peer: PeerId,
}

impl Drop for ChunkTicket {
    fn drop(&mut self) {
        let mut out = self.node.blobs.chunks_out.lock();
        if let Some(n) = out.get_mut(&self.peer) {
            *n -= 1;
            if *n == 0 {
                out.remove(&self.peer);
            }
        }
    }
}

/// 조각 요청 자리를 얻습니다. 그 피어에게 이미 [`CHUNKS_PER_PEER`]개를 보내 두었으면 `None`.
pub(crate) fn chunk_ticket(node: &Arc<Node>, peer: PeerId) -> Option<ChunkTicket> {
    let mut out = node.blobs.chunks_out.lock();
    let n = out.entry(peer).or_default();
    if *n >= CHUNKS_PER_PEER {
        return None;
    }
    *n += 1;
    Some(ChunkTicket {
        node: node.clone(),
        peer,
    })
}

/// Tor로 돌 때 이 내려받기가 직접 연결로만 받을 수 있는지. 그런 내려받기는 Tor 한 번에 하나 한도와 따로
/// `max_active_downloads`까지 함께 받습니다(P2P 전용 채널의 영상이 Tor 첨부 뒤에서 기다리지 않게).
fn fast_route(node: &Node, space: &SpaceId, channel: Option<&ChannelId>) -> bool {
    let peers = member_peers(node, space, channel);
    !peers.is_empty() && peers.iter().all(|p| crate::p2p::data_goes_direct(node, p))
}

/// 동시 내려받기 수만큼 시작합니다. 다음 재시도 시각을 돌려줍니다.
fn schedule(node: &Arc<Node>) -> Option<u64> {
    let now = now_ms();
    let max = max_active(node);
    let dormant: HashSet<SpaceId> = node
        .space_ids()
        .into_iter()
        .filter(|s| policy::is_marked_dormant(node, s))
        .collect();
    // Tor로 돌면 직접 연결로만 받는 내려받기는 따로 셉니다. 피어 상태는 관리자 잠금 밖에서 봅니다.
    let fast_max = node.cfg.sharing.max_active_downloads.max(1);
    let fast_keys: HashSet<(SpaceId, Option<ChannelId>)> =
        if crate::tor::tor_download_limits(node).is_some() {
            let keys: HashSet<(SpaceId, Option<ChannelId>)> = node
                .blobs
                .mgr
                .lock()
                .wants
                .values()
                .map(|w| (w.space, w.channel))
                .collect();
            keys.into_iter()
                .filter(|(s, c)| fast_route(node, s, c.as_ref()))
                .collect()
        } else {
            HashSet::new()
        };
    let mut start = Vec::new();
    let (next_retry, lend) = {
        let mut guard = node.blobs.mgr.lock();
        let m = &mut *guard;
        let due: Vec<BlobId> = m
            .retry
            .iter()
            .filter(|(_, r)| r.at_ms.is_some_and(|at| at <= now))
            .map(|(b, _)| *b)
            .collect();
        for b in due {
            if let Some(r) = m.retry.get_mut(&b) {
                r.at_ms = None;
            }
            m.enqueue(b);
        }
        // 휘발성 첨부가 자리를 기다리면 하나를 남겨 두고, 남은 자리가 있으면 그쪽을 깨웁니다.
        let slow_room =
            |m: &Manager| m.active.len() - m.fast.len() + m.lent + usize::from(m.lend_wanted) < max;
        let fast_room =
            |m: &Manager| !fast_keys.is_empty() && m.fast.len() + m.lent_fast < fast_max;
        // 자리가 없는 쪽의 블롭은 꺼낸 순서대로 다시 넣습니다.
        let mut skipped = Vec::new();
        while slow_room(m) || fast_room(m) {
            let Some(b) = m.pop() else { break };
            // 받는 중인 블롭: 그 작업이 끝날 때 바뀐 요청(예: 그사이 들어온 직접 요청)을 `finish`가 다시 봅니다.
            if m.active.contains(&b) {
                continue;
            }
            let Some(w) = m.wants.get(&b).cloned() else {
                continue;
            };
            if w.deferred || m.retry.get(&b).is_some_and(|r| r.at_ms.is_some()) {
                continue;
            }
            if !w.manual && dormant.contains(&w.space) {
                m.parked.insert(b);
                continue;
            }
            let fast = fast_keys.contains(&(w.space, w.channel));
            if !(if fast { fast_room(m) } else { slow_room(m) }) {
                skipped.push(b);
                continue;
            }
            if fast {
                m.fast.insert(b);
            }
            m.active.insert(b);
            start.push((b, w));
        }
        for b in skipped {
            m.enqueue(b);
        }
        (
            m.retry.values().filter_map(|r| r.at_ms).min(),
            m.lend_wanted && m.active.len() - m.fast.len() + m.lent < max,
        )
    };
    if lend {
        crate::volatile::attach::kick(node);
    }
    for (blob, w) in start {
        node.spawn_detached(move |node| async move {
            let ran_manual = w.manual;
            let outcome = fetch::download(&node, blob, w).await;
            finish(&node, blob, outcome, ran_manual);
        });
    }
    next_retry
}

/// 휘발성 첨부(X1)가 내려받기 자리 하나를 빌립니다. 일반 내려받기와 같은 동시 한도(`max_active_downloads`, Tor로
/// 돌면 Tor 한도)를 나눠 씁니다. 자리가 없으면 false이고, 기다린다고 적어 두어 다음 빈자리를 일반 대기열이
/// 가져가지 않게 합니다([`schedule`]이 그때 휘발성 쪽을 깨움). `fast`(참가자가 모두 직접 연결)면 Tor로 돌 때
/// 직접 연결 몫([`fast_route`])에서 빌립니다. 돌려줄 때 같은 값을 넘깁니다.
pub(crate) fn borrow_slot(node: &Node, fast: bool) -> bool {
    let max = max_active(node);
    let fast = fast && crate::tor::tor_download_limits(node).is_some();
    let mut m = node.blobs.mgr.lock();
    if fast {
        let ok = m.fast.len() + m.lent_fast < node.cfg.sharing.max_active_downloads.max(1);
        m.lent_fast += usize::from(ok);
        return ok;
    }
    if m.active.len() - m.fast.len() + m.lent < max {
        m.lent += 1;
        true
    } else {
        m.lend_wanted = true;
        false
    }
}

/// 빌린 자리를 돌려줍니다.
pub(crate) fn return_slot(node: &Node, fast: bool) {
    let fast = fast && crate::tor::tor_download_limits(node).is_some();
    {
        let mut m = node.blobs.mgr.lock();
        if fast {
            m.lent_fast = m.lent_fast.saturating_sub(1);
        } else {
            m.lent = m.lent.saturating_sub(1);
        }
    }
    node.blobs.wake.notify_one();
    // 직접 연결 몫을 기다리는 휘발성 첨부는 `lend_wanted`로 적어 두지 않으므로 여기서 바로 깨웁니다(안 그러면 다시 보는
    // 간격만큼 쉼).
    if fast {
        crate::volatile::attach::kick(node);
    }
}

/// 자리를 기다리는 휘발성 첨부가 더는 없을 때. 남겨 둔 자리를 일반 대기열이 쓰게 깨웁니다.
pub(crate) fn stop_waiting_for_slot(node: &Node) {
    let was = std::mem::take(&mut node.blobs.mgr.lock().lend_wanted);
    if was {
        node.blobs.wake.notify_one();
    }
}

/// (일반 내려받기 수, 휘발성 첨부에 빌려준 자리 수, 자리를 기다리는 휘발성 첨부가 있는지). 시험용.
pub(crate) fn slot_stats(node: &Node) -> (usize, usize, bool) {
    let m = node.blobs.mgr.lock();
    (m.active.len(), m.lent, m.lend_wanted)
}

/// 수신 상한 안에서 보낼 조각 요청을 고릅니다. 수신 대역폭 버킷(공간 피어 설정이 따로 있으면 그 공간, 아니면 전역)과
/// 크루·채널·사람(조각을 줄 상대)별 받기 제한을 모두 지켜야 보냅니다. 휘발성 첨부(X1)도 일반 첨부와 같이 씁니다.
/// (보낼 것, (넘친 것, 기다릴 밀리초)).
#[allow(clippy::type_complexity)]
pub(crate) fn admit_chunks<T>(
    node: &Node,
    space: &SpaceId,
    channel: Option<&ChannelId>,
    now: u64,
    reqs: Vec<(crate::filelink::Route, u32, T)>,
    chunk_len: impl Fn(u32) -> u64,
) -> (
    Vec<(crate::filelink::Route, u32, T)>,
    Vec<(crate::filelink::Route, u32, u32)>,
) {
    use crate::ratelimit::{self, Dir};
    let limit = policy::peer_policy(node, Some(space)).download_limit_bps;
    let scope = policy::has_space_peer_policy(node, space).then_some(*space);
    let mut send = Vec::with_capacity(reqs.len());
    let mut over = Vec::new();
    let mut buckets = node.blobs.buckets.lock();
    let b = buckets
        .entry(scope)
        .or_insert_with(|| TokenBucket::new(limit, now));
    b.configure(limit, now);
    for (route, i, t) in reqs {
        let len = chunk_len(i);
        let user = node.peers.identity_of(&route.0).map(|(u, _)| u);
        // 범위별 제한은 먼저 보기만 합니다(넘치면 전체 버킷도 쓰지 않게).
        let wait = ratelimit::wait_ms(node, Dir::Down, space, channel, user.as_ref(), len, now);
        if wait > 0 {
            over.push((route, i, wait));
            continue;
        }
        match b.try_consume(len, now) {
            Ok(()) => {
                ratelimit::consume(node, Dir::Down, space, channel, user.as_ref(), len, now);
                send.push((route, i, t));
            }
            Err(ms) => over.push((route, i, ms)),
        }
    }
    (send, over)
}

fn backoff_ms(attempts: u32) -> u64 {
    RETRY_BASE_MS
        .saturating_mul(1u64 << attempts.saturating_sub(1).min(16))
        .min(RETRY_MAX_MS)
}

/// 내려받기 하나가 끝났을 때. `ran_manual`은 끝난 작업이 직접 요청으로 돌았는지입니다.
fn finish(node: &Arc<Node>, blob: BlobId, outcome: fetch::Outcome, ran_manual: bool) {
    use fetch::Outcome;
    let now = now_ms();
    let failed = matches!(outcome, Outcome::Retry | Outcome::Abandon);
    // 저장소를 읽는 일은 관리자 잠금 밖에서 합니다.
    let current = node.blobs.mgr.lock().wants.get(&blob).cloned();
    let alt = match &current {
        Some(w) if failed && !w.manual => next_space(node, &blob, w),
        _ => None,
    };
    let mut remove = false;
    let mut save = None;
    {
        let mut guard = node.blobs.mgr.lock();
        let m = &mut *guard;
        m.active.remove(&blob);
        m.fast.remove(&blob);
        let manual_now = m.wants.get(&blob).is_some_and(|w| w.manual);
        match outcome {
            Outcome::Complete => remove = true,
            // 자동으로 돌던 작업 중에 직접 요청이 들어왔으면 포기하지 않고 바로 직접 요청으로 다시 받습니다.
            Outcome::Retry | Outcome::Abandon | Outcome::NoRoom if manual_now && !ran_manual => {
                m.retry.remove(&blob);
                m.gave_up.remove(&blob);
                m.enqueue(blob);
            }
            Outcome::Abandon if alt.is_none() => {
                remove = true;
                m.gave_up.insert(blob);
            }
            Outcome::Retry | Outcome::Abandon => {
                if let Some(w) = m.wants.get_mut(&blob) {
                    // 같은 블롭을 실은 다른 공간의 메시지가 있으면 다음엔 그 공간에서 받습니다.
                    if let Some(a) = &alt
                        && !w.manual
                    {
                        *w = a.clone();
                        save = Some(a.clone());
                    }
                    let manual = w.manual;
                    let r = m.retry.entry(blob).or_insert(Retry {
                        at_ms: None,
                        attempts: 0,
                    });
                    r.attempts += 1;
                    if !manual && r.attempts >= MAX_AUTO_ATTEMPTS {
                        remove = true;
                        save = None;
                        m.gave_up.insert(blob);
                    } else {
                        r.at_ms = Some(now + backoff_ms(r.attempts));
                    }
                }
            }
            // 저장 공간 한도: 포기로 남기지 않아, 같은 블롭을 실은 새 메시지나 직접 요청이면 다시 받습니다.
            Outcome::NoRoom => remove = true,
            Outcome::Cancelled => {}
        }
        if remove {
            m.wants.remove(&blob);
            m.retry.remove(&blob);
            m.parked.remove(&blob);
        }
    }
    if remove {
        let _ = node.store.delete_setting(&want_key(&blob));
    }
    if let Some(w) = save {
        let _ = node.store.put_setting(&want_key(&blob), &w);
    }
    node.blobs.wake.notify_one();
}

/// 자동 내려받기가 실패했을 때 같은 블롭을 실은 다른 공간의 메시지(키가 틀렸다고 확인된 것은 빼고)가 있으면 그
/// 공간에서 받을 요청을 만듭니다. 블롭 ID를 베낀 메시지가 먼저 와서 받을 공간을 차지해도 진짜 공간에서 받게 합니다.
fn next_space(node: &Node, blob: &BlobId, w: &Want) -> Option<Want> {
    if w.manual || w.profile || w.channel.is_none() {
        return None;
    }
    let mut cands: Vec<BlobRef> = Vec::new();
    for r in refs_of(node, blob) {
        if r.verified == Some(false) || cands.iter().any(|c| c.space == r.space) {
            continue;
        }
        let ok = node.space_state(&r.space).is_some_and(|st| {
            st.channels
                .get(&r.channel)
                .is_some_and(|c| !c.is_volatile())
        }) && !policy::is_marked_dormant(node, &r.space)
            && !node.store.is_deleted(&r.event).unwrap_or(true)
            && policy::peer_policy(node, Some(&r.space)).auto_download(r.data_type, r.size);
        if ok {
            cands.push(r);
        }
    }
    let next = match cands.iter().position(|r| r.space == w.space) {
        Some(p) => cands.get((p + 1) % cands.len()),
        None => cands.first(),
    }?;
    (next.space != w.space).then(|| Want {
        space: next.space,
        channel: Some(next.channel),
        data_type: next.data_type,
        size: next.size,
        manual: false,
        priority: policy::peer_policy(node, Some(&next.space)).priority(next.data_type),
        deferred: false,
        profile: false,
        added_ms: w.added_ms,
        backfill: false,
    })
}

fn want_key(blob: &BlobId) -> String {
    format!("{WANT_PREFIX}{blob}")
}

/// 미룬 자동 내려받기를 넣을 자리를 만듭니다: 공간마다·전체 상한을 넘으면 가장 오래된 미룬 것을 버립니다.
/// 버린 블롭을 돌려줍니다(설정 표에서 지울 것).
fn make_room_deferred(m: &mut Manager, space: &SpaceId) -> Vec<BlobId> {
    let mut dropped = Vec::new();
    // 전체 수가 상한보다 작으면 공간마다의 수도 볼 필요가 없습니다(대부분 여기서 끝남).
    if m.wants.len() < MAX_DEFERRED_PER_SPACE {
        return dropped;
    }
    loop {
        let (mut total, mut here) = (0usize, 0usize);
        let mut oldest_all: Option<(u64, BlobId)> = None;
        let mut oldest_here: Option<(u64, BlobId)> = None;
        for (b, w) in m.wants.iter().filter(|(_, w)| w.deferred) {
            total += 1;
            let k = (w.added_ms, *b);
            if oldest_all.is_none_or(|o| k < o) {
                oldest_all = Some(k);
            }
            if w.space == *space {
                here += 1;
                if oldest_here.is_none_or(|o| k < o) {
                    oldest_here = Some(k);
                }
            }
        }
        let victim = if here >= MAX_DEFERRED_PER_SPACE {
            oldest_here
        } else if total >= MAX_DEFERRED {
            oldest_all
        } else {
            None
        };
        let Some((_, b)) = victim else { break };
        m.wants.remove(&b);
        m.retry.remove(&b);
        m.parked.remove(&b);
        dropped.push(b);
    }
    dropped
}

/// 미루지 않은 자동 내려받기가 상한에 닿았는지(미룬 것·직접 요청은 세지 않음).
fn auto_full(m: &Manager) -> bool {
    m.wants.len() >= MAX_WANTS
        && m.wants
            .values()
            .filter(|x| !x.deferred && !x.manual)
            .count()
            >= MAX_WANTS
}

/// 받을 블롭을 더합니다(이미 있으면 직접 요청·미룸 해제만 반영). 새로 넣거나 바꿨으면 true.
pub(crate) fn add_want(node: &Node, blob: BlobId, mut w: Want) -> bool {
    if is_downloaded(node, &blob) {
        return false;
    }
    if w.added_ms == 0 {
        w.added_ms = now_ms();
    }
    let mut dropped = Vec::new();
    let saved = {
        let mut guard = node.blobs.mgr.lock();
        let m = &mut *guard;
        if !w.manual && m.gave_up.contains(&blob) {
            return false;
        }
        let merged = match m.wants.get(&blob) {
            Some(old) => {
                let mut n = old.clone();
                if w.manual {
                    n.manual = true;
                    n.priority = if old.manual {
                        old.priority.max(w.priority)
                    } else {
                        w.priority
                    };
                    n.deferred = false;
                    // 직접 요청은 요청한 공간에서 받습니다(자동 요청과 다를 수 있음).
                    n.space = w.space;
                }
                if !w.deferred {
                    if old.deferred {
                        // 미뤄 둔 요청은 깨운 쪽을 따릅니다(보충이 깨웠으면 다른 첨부를 비우지 않게).
                        n.backfill = w.backfill;
                    }
                    n.deferred = false;
                }
                if !w.backfill {
                    n.backfill = false;
                }
                if n == *old {
                    return false;
                }
                n
            }
            None => {
                if !w.manual && w.deferred {
                    // 미룬 것은 따로 세어, 열지 않은 채널이 쌓여도 다른 자동 내려받기를 막지 않게 합니다.
                    dropped = make_room_deferred(m, &w.space);
                } else if !w.manual && auto_full(m) {
                    return false;
                }
                w.clone()
            }
        };
        if w.manual {
            // 직접 요청은 기다리던 재시도·포기를 지웁니다.
            m.gave_up.remove(&blob);
            m.retry.remove(&blob);
            m.parked.remove(&blob);
        }
        m.wants.insert(blob, merged.clone());
        m.enqueue(blob);
        merged
    };
    for b in dropped {
        let _ = node.store.delete_setting(&want_key(&b));
    }
    if let Err(e) = node.store.put_setting(&want_key(&blob), &saved) {
        debug!(blob = %blob.short(), error = %e, "받을 블롭 저장 실패");
    }
    node.blobs.wake.notify_one();
    true
}

/// 받을 블롭에서 뺍니다(진행 중이면 그 작업이 곧 멈춤).
fn remove_want(node: &Node, blob: &BlobId) {
    let had = {
        let mut m = node.blobs.mgr.lock();
        m.retry.remove(blob);
        m.parked.remove(blob);
        m.wants.remove(blob).is_some()
    };
    if had {
        let _ = node.store.delete_setting(&want_key(blob));
    }
}

/// 이 기기에 둔 첨부 데이터를 지웁니다(`blob.evict`, 고정도 풂). 받는 중이면 받기만 멈추고 `false`: 조각을 쓰던 작업이
/// 끝나기 전에 비우면 늦게 온 조각이 다시 남으므로, 멈춘 뒤 다시 부르게 합니다.
pub(crate) fn evict(node: &Node, blob: &BlobId) -> crate::error::Result<bool> {
    remove_want(node, blob);
    if node.blobs.mgr.lock().active.contains(blob) {
        return Ok(false);
    }
    cache::unpin(node, blob)?;
    node.store.blob_drop_data(blob)?;
    Ok(true)
}

/// 다시 잇기(`blob.relink`)로 조각을 채운 뒤: 다 모였으면 받을 목록에서 빼고(포기한 것도 풀고) 진행을 알립니다.
pub(crate) fn on_relinked(node: &Node, space: &SpaceId, blob: &BlobId) {
    let Ok(Some(st)) = node.store.blob_status(blob) else {
        return;
    };
    if st.complete {
        remove_want(node, blob);
        node.blobs.mgr.lock().gave_up.remove(blob);
    }
    node.notify
        .publish(yangpa_rpc::api::Notification::BlobProgress {
            space: *space,
            blob: *blob,
            have: st.have_count,
            total: st.chunk_count,
        });
}

/// 아직 받을 블롭인지(내려받기 작업이 멈출지 볼 때).
fn is_wanted(node: &Node, blob: &BlobId) -> bool {
    node.blobs.mgr.lock().wants.contains_key(blob)
}

/// 대기열에 있거나 받는 중인지(`blob.status`의 `downloading`).
pub(crate) fn is_fetching(node: &Node, blob: &BlobId) -> bool {
    node.blobs
        .mgr
        .lock()
        .wants
        .get(blob)
        .is_some_and(|w| !w.deferred)
}

/// 이 공간의 인증된 멤버 기기 노드(나 빼고)와 지정된 보관 노드. 블롭을 요청할 수 있는 피어입니다. P2P 전용 채널의
/// 첨부는 그 채널에 함께 참가해 P2P 스웜에서 인증된 기기에게만 묻습니다.
fn member_peers(node: &Node, space: &SpaceId, channel: Option<&ChannelId>) -> Vec<PeerId> {
    let Some(state) = node.space_state(space) else {
        return Vec::new();
    };
    if let Some(c) = channel
        && state.is_p2p(c)
    {
        return crate::p2p::channel_peers(node, space, &state, c)
            .into_iter()
            .map(|(p, _, _)| p)
            .collect();
    }
    let me = node.peer_id();
    let mut out: Vec<PeerId> = node
        .peers
        .authed_peers()
        .into_iter()
        .filter(|(p, u, d)| *p != me && state.is_member(u) && state.device_belongs(u, d))
        .map(|(p, _, _)| p)
        .collect();
    // 지정된 보관 노드도 시더로 씁니다(받은 조각은 늘 그렇듯 해시로 확인).
    for p in crate::archive::archive_peers(node, &state) {
        if !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

/// 조각 검증 실패 등으로 피어를 감점합니다. 점수가 끊을 만큼 내려가면 네트워크에서 차단합니다.
pub(crate) fn penalize(node: &Node, peer: PeerId, p: Penalty) {
    let verdict = node
        .blobs
        .score
        .lock()
        .get_or_insert_with(|| ScoreBoard::new(ScoreConfig::default()))
        .penalize(&peer, p, now_ms());
    if verdict == Verdict::Disconnect {
        node.net.penalize(peer, "블롭 조각 검증 실패 누적");
    }
}

fn reward(node: &Node, peer: PeerId) {
    node.blobs
        .score
        .lock()
        .get_or_insert_with(|| ScoreBoard::new(ScoreConfig::default()))
        .reward(&peer, now_ms());
}

// ───────────── 코어 고리 ─────────────

/// 보내는 쪽이 만드는 링크 미리보기(채널 설정이 허용할 때만 코어가 부름).
pub(crate) async fn link_previews(node: &Arc<Node>, text: &str) -> Vec<LinkEmbed> {
    preview::link_previews(node, text).await
}

pub(crate) fn is_downloaded(node: &Node, blob: &BlobId) -> bool {
    node.store
        .blob_status(blob)
        .ok()
        .flatten()
        .is_some_and(|s| s.complete)
}

/// 메시지가 보기 행에 들어간 뒤: 첨부마다 실은 메시지를 기록하고(제공 검사), 받을지 정합니다.
///
/// 자동으로 받는 조건: 다른 기기가 보낸 첨부, 휴면이 아닌 공간, 피어 설정의 종류·자동 다운로드 한도 안.
/// 내가 열지 않은 크루 채널이면 그 채널을 열 때까지 미룹니다(보지 않은 채널의 파일을 쌓아 두지 않게).
pub(crate) fn on_message_applied(
    node: &Arc<Node>,
    space: &SpaceId,
    channel: &ChannelId,
    id: &EventId,
    header: &EventHeader,
    body: &MessageBody,
) {
    if body.attachments.is_empty() {
        return;
    }
    // 휘발성 채널은 적용 경로가 따로라 여기 오지 않지만, 혹시 와도 디스크(설정 표)에 아무것도 남기지 않습니다.
    let Some(state) = node.space_state(space) else {
        return;
    };
    if state.channels.get(channel).is_none_or(|c| c.is_volatile()) {
        return;
    }
    for a in &body.attachments {
        let r = BlobRef {
            space: *space,
            channel: *channel,
            event: *id,
            lamport: header.lamport,
            data_type: DataType::of_attachment(a),
            size: a.size,
            verified: None,
        };
        if let Err(e) = add_ref(node, &a.blob, &a.key, r) {
            debug!(blob = %a.blob.short(), error = %e, "첨부 기록 실패");
        }
    }
    let Ok(me) = node.me() else { return };
    // 차단한 사용자·기기의 첨부는 자동으로 받지 않습니다(모더레이션).
    if header.author == me.device_id()
        || policy::is_marked_dormant(node, space)
        || crate::moderation::is_author_blocked(node, &header.user, &header.author)
    {
        return;
    }
    let opened = policy::channel_opened(node, space, &state, channel);
    let pp = policy::peer_policy(node, Some(space));
    // 미리 받기는 막 쓴 메시지만 바로 받습니다. 따라잡기로 들어온 지난 메시지는 미뤄 두고 보충(`prefetch::backfill`)이
    // 최근 것부터 남은 자리만큼 받게 해, 한도가 찬 시더가 지난 첨부를 받고 비우기를 되풀이하지 않게 합니다.
    let live_ms = node.blobs.test.live_ms.lock().unwrap_or(prefetch::LIVE_MS);
    let live = now_ms().saturating_sub(header.created_at.0) < live_ms;
    let limit = state.attach_limit(channel);
    // 이 크루·채널의 받기 제한이 0이면 첨부를 받지 않습니다.
    if crate::ratelimit::blocked(
        node,
        crate::ratelimit::Dir::Down,
        space,
        Some(channel),
        None,
    ) {
        return;
    }
    for a in &body.attachments {
        let t = DataType::of_attachment(a);
        // 채널 한도를 넘는 첨부(한도를 지키지 않는 클라이언트가 보낸 것)는 저절로 받지 않습니다.
        if !pp.auto_download(t, a.size) || limit.is_some_and(|l| a.size > l) {
            continue;
        }
        // 미리 받기(헤드리스 시더): 열지 않은 채널이어도 새 메시지면 미루지 않습니다.
        let deferred =
            !opened && !(live && policy::prefetches(node, space, &state, channel, id, t));
        add_want(
            node,
            a.blob,
            Want {
                space: *space,
                channel: Some(*channel),
                data_type: t,
                size: a.size,
                manual: false,
                priority: pp.priority(t),
                deferred,
                profile: false,
                added_ms: 0,
                backfill: false,
            },
        );
    }
}

/// 채널을 열었을 때(`policy::mark_channel_opened`): 보고 있는 채널을 먼저 받고, 미뤄 둔 자동 내려받기를 시작합니다.
pub(crate) fn on_channel_opened(node: &Arc<Node>, space: &SpaceId, channel: &ChannelId) {
    let changed: Vec<(BlobId, Want)> = {
        let mut guard = node.blobs.mgr.lock();
        let m = &mut *guard;
        m.queue.set_viewed(Some((*space, *channel)));
        let ids: Vec<BlobId> = m
            .wants
            .iter()
            .filter(|(_, w)| w.deferred && w.space == *space && w.channel == Some(*channel))
            .map(|(b, _)| *b)
            .collect();
        let mut out = Vec::with_capacity(ids.len());
        for b in ids {
            if let Some(w) = m.wants.get_mut(&b) {
                w.deferred = false;
                out.push((b, w.clone()));
            }
            m.enqueue(b);
        }
        out
    };
    if changed.is_empty() {
        return;
    }
    for (b, w) in &changed {
        let _ = node.store.put_setting(&want_key(b), w);
    }
    node.blobs.wake.notify_one();
}

/// 공간이 휴면이 됨: 그 공간의 자동 내려받기를 대기열에서 꺼내 두고(깨어나면 돌려놓음), 열지 않은 채널이라
/// 미뤄 둔 것은 버립니다(오래 열지 않은 공간의 첨부를 쌓아 두지 않게).
pub(crate) fn on_space_dormant(node: &Node, space: &SpaceId) {
    let dropped = {
        let mut guard = node.blobs.mgr.lock();
        let m = &mut *guard;
        let ids: Vec<BlobId> = m
            .wants
            .iter()
            .filter(|(_, w)| w.space == *space && !w.manual && !w.deferred)
            .map(|(b, _)| *b)
            .collect();
        m.parked.extend(ids);
        m.queue.retain(|w, _| w.space() != *space);
        let deferred: Vec<BlobId> = m
            .wants
            .iter()
            .filter(|(_, w)| w.space == *space && !w.manual && w.deferred)
            .map(|(b, _)| *b)
            .collect();
        for b in &deferred {
            m.wants.remove(b);
            m.retry.remove(b);
        }
        deferred
    };
    for b in dropped {
        let _ = node.store.delete_setting(&want_key(&b));
    }
}

/// 공간이 깨어남: 꺼내 둔 자동 내려받기를 대기열로 돌립니다.
pub(crate) fn on_space_woken(node: &Node, space: &SpaceId) {
    {
        let mut guard = node.blobs.mgr.lock();
        let m = &mut *guard;
        let ids: Vec<BlobId> = m
            .parked
            .iter()
            .filter(|b| m.wants.get(b).is_some_and(|w| w.space == *space))
            .copied()
            .collect();
        for b in ids {
            m.parked.remove(&b);
            m.enqueue(b);
        }
    }
    node.blobs.wake.notify_one();
}

/// 피어 설정이 바뀜: 이제 받지 않는 종류·크기의 자동 내려받기를 버리고 대기열 순서를 다시 정합니다.
pub(crate) fn on_peer_policy_changed(node: &Arc<Node>) {
    let snapshot: Vec<(BlobId, Want)> = node
        .blobs
        .mgr
        .lock()
        .wants
        .iter()
        .filter(|(_, w)| !w.manual)
        .map(|(b, w)| (*b, w.clone()))
        .collect();
    for (b, w) in snapshot {
        let pp = policy::peer_policy(node, Some(&w.space));
        // 프로필 사진·공간 아이콘·이모지(채널 없는 그림)는 자동 다운로드 크기 한도를 보지 않습니다(정리 주기와 같게).
        let keep =
            if w.channel.is_none() && matches!(w.data_type, DataType::Image | DataType::Emoji) {
                pp.wants(w.data_type)
            } else {
                pp.auto_download(w.data_type, w.size)
            };
        if !keep {
            remove_want(node, &b);
        }
    }
    let global = policy::peer_policy(node, None);
    node.blobs
        .mgr
        .lock()
        .queue
        .set_policy(queue_policy(&global));
    node.blobs.wake.notify_one();
}

/// `BlobManifest`·`BlobHave`·`BlobChunk` 요청. 인증된 멤버인지는 이 함수가 확인합니다.
pub(crate) async fn handle_request(node: &Arc<Node>, peer: PeerId, req: YpRequest) -> YpResponse {
    let delay = node.blobs.test.serve_delay_ms.load(Ordering::Relaxed);
    if delay > 0 && matches!(req, YpRequest::BlobChunk { .. }) {
        let t = &node.blobs.test;
        let n = t.serving.fetch_add(1, Ordering::Relaxed) + 1;
        t.serving_overlaps.lock().push(n);
        tokio::time::sleep(Duration::from_millis(delay)).await;
        t.serving.fetch_sub(1, Ordering::Relaxed);
    }
    // 휘발성 채널 첨부는 RAM에만 있으므로 휘발성 영역이 답합니다(디스크 저장소를 보지 않음).
    if let Some(r) = crate::volatile::attach::serve(node, peer, &req) {
        return r;
    }
    serve::handle(node, peer, req)
}

// ───────────── 블롭 쓰임 ─────────────

fn ref_key(blob: &BlobId) -> String {
    format!("{REF_PREFIX}{blob}")
}

/// 첨부 쓰임 기록이 있는 블롭(보관 노드에 첨부를 알릴 때 훑음).
pub(crate) fn referenced_blobs(node: &Node) -> Vec<BlobId> {
    node.store
        .settings_with_prefix(REF_PREFIX)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(k, _)| k[REF_PREFIX.len()..].parse().ok())
        .collect()
}

/// 모든 첨부 쓰임 기록을 한 번에(라이브러리 탭).
pub(crate) fn all_refs(node: &Node) -> Vec<BlobRef> {
    node.store
        .settings_with_prefix(REF_PREFIX)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(_, v)| serde_json::from_value::<Vec<BlobRef>>(v).ok())
        .flatten()
        .collect()
}

/// 블롭을 실은 메시지들.
pub(crate) fn refs_of(node: &Node, blob: &BlobId) -> Vec<BlobRef> {
    node.store
        .get_setting::<Vec<BlobRef>>(&ref_key(blob))
        .ok()
        .flatten()
        .unwrap_or_default()
}

fn save_refs(node: &Node, blob: &BlobId, v: &[BlobRef]) -> crate::error::Result<()> {
    if v.is_empty() {
        node.store.delete_setting(&ref_key(blob))?;
    } else {
        node.store.put_setting(&ref_key(blob), &v)?;
    }
    Ok(())
}

/// 블롭을 실은 메시지를 기록합니다. 블롭을 (일부라도) 가졌으면 그 메시지의 키로 바로 확인해, 키가 틀린 메시지
/// (남의 블롭 ID만 베낀 것)는 기록하지 않습니다. 꽉 차면 키가 틀린 것 → 처음 것이 아닌 확인 못 한 것 중 가장 최근
/// 것 순으로 비우고, 그래도 없으면 새 기록을 버립니다(처음 기록을 베낀 메시지들이 밀어내지 못하게).
fn add_ref(node: &Node, blob: &BlobId, key: &Secret32, mut r: BlobRef) -> crate::error::Result<()> {
    let mut v = refs_of(node, blob);
    if v.iter().any(|x| x.event == r.event) {
        return Ok(());
    }
    r.verified = key_check(node, blob, key);
    if r.verified == Some(false) {
        debug!(blob = %blob.short(), "키가 맞지 않는 첨부 기록은 버림");
        return Ok(());
    }
    if v.len() >= MAX_REFS {
        v.retain(|x| x.verified != Some(false));
    }
    if v.len() >= MAX_REFS {
        match v.iter().skip(1).rposition(|x| x.verified.is_none()) {
            Some(i) => {
                v.remove(i + 1);
            }
            None => return Ok(()),
        }
    }
    v.push(r);
    save_refs(node, blob, &v)
}

/// 키 지문(캐시 열쇠). 키 자체를 메모리 캐시에 두지 않습니다.
fn key_fingerprint(key: &Secret32) -> [u8; 32] {
    blake3::derive_key("yangpa/v1 daemon blob key check", &key.0)
}

/// 이 키가 이 블롭의 키인지: 가진 조각 하나(끝에서부터 찾아 보통 작은 마지막 조각)를 풀어 봅니다.
/// 가진 조각이 없으면 `None`. 블롭 ID가 매니페스트 해시라 한 번 낸 결과는 바뀌지 않아 기억해 둡니다.
pub(crate) fn key_check(node: &Node, blob: &BlobId, key: &Secret32) -> Option<bool> {
    let fp = key_fingerprint(key);
    if let Some(ok) = node.blobs.key_checks.lock().get(&(*blob, fp)) {
        return Some(*ok);
    }
    let manifest = node.store.blob_manifest(blob).ok().flatten()?;
    let (bm, total) = node.store.blob_have(blob).ok().flatten()?;
    let index = (0..total).rev().find(|i| fetch::store_bit(&bm, *i))?;
    let chunk = node.store.blob_read_chunk(blob, index).ok().flatten()?;
    let ok = yangpa_crypto::decrypt_blob_chunk(key, &manifest, index, &chunk).is_ok();
    let mut cache = node.blobs.key_checks.lock();
    if cache.len() >= MAX_KEY_CHECKS {
        cache.clear();
    }
    cache.insert((*blob, fp), ok);
    Some(ok)
}

/// 메시지(첨부·게시글 첫 메시지)에 실린 이 블롭의 키.
fn attachment_key(node: &Node, event: &EventId, blob: &BlobId) -> Option<Secret32> {
    let rec = node.store.get_message(event, None).ok().flatten()?;
    let c = rec.content?;
    c.body
        .attachments
        .iter()
        .find(|a| a.blob == *blob)
        .map(|a| a.key.clone())
}

/// 키가 확인된(그 메시지의 키로 가진 조각이 풀리는) 기록. 아직 확인하지 못한 기록은 지금 확인해 저장하고,
/// 키가 틀린 기록은 지웁니다. 가진 조각이 없으면 아무것도 확인되지 않습니다.
pub(crate) fn verified_refs(node: &Node, blob: &BlobId) -> Vec<BlobRef> {
    let mut v = refs_of(node, blob);
    let mut changed = false;
    for r in v.iter_mut().filter(|r| r.verified.is_none()) {
        if let Some(k) = attachment_key(node, &r.event, blob)
            && let Some(ok) = key_check(node, blob, &k)
        {
            r.verified = Some(ok);
            changed = true;
        }
    }
    if changed {
        v.retain(|r| r.verified != Some(false));
        if let Err(e) = save_refs(node, blob, &v) {
            debug!(blob = %blob.short(), error = %e, "첨부 기록 저장 실패");
        }
    }
    v.retain(|r| r.verified == Some(true));
    v
}

/// 공간 안에서 블롭이 쓰인 곳.
#[derive(Clone, Debug)]
pub(crate) enum Use {
    Attachment(BlobRef),
    Emoji,
    /// 공간 아이콘(키는 공간 설정 `icon_key`).
    Icon,
    /// 이 공간 멤버의 프로필 이미지.
    Profile,
    /// 상태 체크포인트 스냅숏(상태 키로 암호화, 최근 두 개). 체크포인트로 들어오는 멤버가 받습니다.
    Snapshot,
}

/// 로컬 API(`blob.status`·`blob.fetch`)가 보는 쓰임: 이 공간의 메시지·이모지·아이콘·멤버 프로필이 가리키는 블롭.
/// 남에게 내줄 때는 [`serve_uses`]를 씁니다.
pub(crate) fn uses_in_space(
    node: &Node,
    space: &SpaceId,
    state: &SpaceState,
    blob: &BlobId,
) -> Vec<Use> {
    let mut out: Vec<Use> = refs_of(node, blob)
        .into_iter()
        .filter(|r| r.space == *space)
        .map(Use::Attachment)
        .collect();
    if state.emojis.values().any(|e| e.blob == *blob) {
        out.push(Use::Emoji);
    }
    if state.settings.icon == Some(*blob) {
        out.push(Use::Icon);
    }
    if !profile_users(node, state, blob).is_empty() {
        out.push(Use::Profile);
    }
    out
}

/// 남의 요청에 내줄 수 있는 쓰임(닫힌 스웜). 주장만으로 정해지지 않게:
/// - 첨부·이모지·아이콘: 블롭이 이 공간에 등록돼 있어야 합니다(여기서 올렸거나 이 공간 멤버에게서 처음 받음).
///   첨부는 키가 확인된 메시지만, 이모지·아이콘은 상태에 실린 키(`CustomEmoji::key`, `SpaceSettings::icon_key`)로
///   풀릴 때만.
/// - 프로필 이미지: [`PROFILE_SPACE`]에 등록돼 있고, 이 공간 멤버 중 그 이미지를 쓰는 사람의 서명된 프로필 키로
///   풀릴 때만. 프로필을 들춰 보는 일은 프로필 이미지로 등록된 블롭에만 합니다(보통 첨부 요청마다 하지 않게).
pub(crate) fn serve_uses(
    node: &Node,
    space: &SpaceId,
    state: &SpaceState,
    blob: &BlobId,
) -> Vec<Use> {
    let mut out = Vec::new();
    if node.store.blob_in_space(blob, space).unwrap_or(false) {
        out.extend(
            verified_refs(node, blob)
                .into_iter()
                .filter(|r| r.space == *space)
                .map(Use::Attachment),
        );
        if state
            .emojis
            .values()
            .any(|e| e.blob == *blob && key_check(node, blob, &e.key) == Some(true))
        {
            out.push(Use::Emoji);
        }
        if state.settings.icon == Some(*blob)
            && state
                .settings
                .icon_key
                .as_ref()
                .is_some_and(|k| key_check(node, blob, k) == Some(true))
        {
            out.push(Use::Icon);
        }
    }
    if node
        .store
        .blob_in_space(blob, &PROFILE_SPACE)
        .unwrap_or(false)
        && profile_users(node, state, blob).iter().any(|u| {
            profile_key(node, u, blob).is_some_and(|k| key_check(node, blob, &k) == Some(true))
        })
    {
        out.push(Use::Profile);
    }
    if crate::checkpoint::is_snapshot(node, space, blob) {
        out.push(Use::Snapshot);
    }
    out
}

/// 사용자의 프로필 이미지 키: 나면 비밀값, 남이면 저장된 서명 프로필에서.
fn profile_key(node: &Node, user: &UserId, blob: &BlobId) -> Option<Secret32> {
    if node.me().is_ok_and(|me| me.user() == *user)
        && let Some(k) = profile_image_key(node, blob)
    {
        return Some(k);
    }
    let p = node.store.get_profile(user).ok().flatten()?.profile;
    if p.avatar == Some(*blob)
        && let Some(k) = p.avatar_key
    {
        return Some(k);
    }
    if p.banner == Some(*blob)
        && let Some(k) = p.banner_key
    {
        return Some(k);
    }
    None
}

/// 내 프로필 이미지 키(`profile.set`이 서명된 프로필에 싣고, 내 이미지를 읽을 때 씀).
pub(crate) fn profile_image_key(node: &Node, blob: &BlobId) -> Option<Secret32> {
    let s = node
        .store
        .get_secret(&format!("{IMAGE_SECRET_PREFIX}{blob}"))
        .ok()
        .flatten()?;
    let k: [u8; 32] = s.as_slice().try_into().ok()?;
    Some(Secret32(k))
}

/// 작은 공간이면 프로필 색인이 늦어도 멤버 프로필을 직접 봅니다.
const DIRECT_PROFILE_SCAN: usize = 64;

/// 이 공간 멤버 중 이 블롭을 아바타·배너로 쓰는 사용자(나 포함).
fn profile_users(node: &Node, state: &SpaceState, blob: &BlobId) -> Vec<UserId> {
    let mut out = Vec::new();
    if let Ok(me) = node.me() {
        let p = &me.profile.profile;
        if (p.avatar == Some(*blob) || p.banner == Some(*blob)) && state.is_member(&me.user()) {
            out.push(me.user());
        }
    }
    if let Some(users) = node.blobs.profile_index.read().get(blob) {
        out.extend(
            users
                .iter()
                .filter(|u| state.is_member(u) && !out.contains(u))
                .copied()
                .collect::<Vec<_>>(),
        );
    }
    if out.is_empty() && state.members.len() <= DIRECT_PROFILE_SCAN {
        for u in state.members.keys() {
            if let Ok(Some(sp)) = node.store.get_profile(u)
                && (sp.profile.avatar == Some(*blob) || sp.profile.banner == Some(*blob))
            {
                out.push(*u);
            }
        }
    }
    out
}

/// 블롭 복호화 키. `key_from` 메시지의 첨부 → 공간 이모지 → 공간 아이콘 → 멤버 프로필 이미지 순으로 찾습니다
/// (이모지·아이콘·프로필 이미지는 `key_from`과 무관).
pub(crate) fn key_for(
    node: &Node,
    space: &SpaceId,
    state: &SpaceState,
    blob: &BlobId,
    key_from: &EventId,
) -> Option<Secret32> {
    if let Ok(Some(rec)) = node.store.get_message(key_from, None)
        && rec.space == *space
        && let Some(c) = &rec.content
        && let Some(a) = c.body.attachments.iter().find(|a| a.blob == *blob)
    {
        return Some(a.key.clone());
    }
    image_key(node, state, blob)
}

/// 이모지 블롭을 받을 때의 크기 상한: 크루 한도가 있으면 그것, 없으면 무제한. 한도를 낮추면 그보다 큰 옛 이모지는
/// 아직 받지 않은 기기에서 받지 않습니다(한도가 받는 쪽도 지키게 하려고 일부러).
fn emoji_fetch_cap(state: &SpaceState) -> u64 {
    state.emoji_limits.max_bytes.unwrap_or(u64::MAX)
}

/// 메시지에 실리지 않는 이미지(공간 이모지·아이콘·프로필 이미지)의 키.
fn image_key(node: &Node, state: &SpaceState, blob: &BlobId) -> Option<Secret32> {
    if let Some(e) = state.emojis.values().find(|e| e.blob == *blob) {
        return Some(e.key.clone());
    }
    if state.settings.icon == Some(*blob)
        && let Some(k) = &state.settings.icon_key
    {
        return Some(k.clone());
    }
    profile_users(node, state, blob)
        .iter()
        .find_map(|u| profile_key(node, u, blob))
}

/// 직접 요청(`blob.fetch`)에 쓸 받을 블롭 정보. 이 공간에서 쓰이지 않는 블롭이면 `None`.
pub(crate) fn manual_want(
    node: &Node,
    space: &SpaceId,
    state: &SpaceState,
    blob: &BlobId,
    priority: u8,
) -> Option<Want> {
    let uses = uses_in_space(node, space, state, blob);
    let (channel, data_type, size, profile) = match uses.first()? {
        Use::Attachment(r) => (Some(r.channel), r.data_type, r.size, false),
        Use::Emoji => (None, DataType::Emoji, emoji_fetch_cap(state), false),
        Use::Icon => (None, DataType::Image, MAX_PROFILE_IMAGE_BYTES, false),
        Use::Profile => (None, DataType::Image, MAX_PROFILE_IMAGE_BYTES, true),
        Use::Snapshot => return None,
    };
    Some(Want {
        space: *space,
        channel,
        data_type,
        size,
        manual: true,
        priority,
        deferred: false,
        profile,
        added_ms: 0,
        backfill: false,
    })
}

/// 원본(내용) 크기: 매니페스트가 있으면 그것으로([`content_size`]), 없으면 메시지에 적힌 크기.
pub(crate) fn plaintext_size(node: &Node, space: &SpaceId, blob: &BlobId) -> u64 {
    let declared = refs_of(node, blob)
        .into_iter()
        .find(|r| r.space == *space)
        .map(|r| r.size);
    if let Ok(Some(m)) = node.store.blob_manifest(blob) {
        return content_size(&m, declared.unwrap_or(0));
    }
    declared.unwrap_or(0)
}

/// 메시지에 실리지 않는 이미지(적힌 크기가 없음)의 정확한 내용 크기: 첫 조각을 풀어 압축 머리글을 봅니다.
/// 키가 없거나 첫 조각을 아직 받지 않았으면 `None`.
pub(crate) fn image_content_size(node: &Node, state: &SpaceState, blob: &BlobId) -> Option<u64> {
    let key = image_key(node, state, blob)?;
    let m = node.store.blob_manifest(blob).ok()??;
    let ct = node.store.blob_read_chunk(blob, 0).ok()??;
    let first = zeroize::Zeroizing::new(yangpa_crypto::decrypt_blob_chunk(&key, &m, 0, &ct).ok()?);
    yangpa_crypto::blobpack::parse_layout(&first, &m)
        .ok()
        .map(|(_, len)| len)
}

/// 매니페스트로 어림한 내용 크기(키 없이). 블롭 평문은 압축 머리글 + 몸통이라, 압축하지 않은 블롭이면 평문에서
/// 머리글을 뺀 것이 정확하고, 압축본(평문이 그보다 작음)이면 적힌 크기(`declared`)를 믿습니다. 예전 판 블롭(머리글
/// 없음)은 머리글 길이만큼 작게 보일 수 있으나 표시용이라 괜찮습니다.
pub(crate) fn content_size(m: &yangpa_core::blob::BlobManifest, declared: u64) -> u64 {
    let pt = yangpa_crypto::blob::plaintext_len(m);
    let raw = pt.saturating_sub(yangpa_crypto::blobpack::HEADER_LEN as u64);
    if declared > raw { declared } else { raw }
}

/// 지금 이 블롭을 (일부라도) 가진 온라인 멤버 기기 수. 짧게 캐시합니다.
pub(crate) async fn seeders_online(node: &Arc<Node>, space: &SpaceId, blob: &BlobId) -> u32 {
    const CACHE_MS: u64 = 10_000;
    const PROBE: Duration = Duration::from_secs(2);
    let now = now_ms();
    if let Some((at, n)) = node.blobs.seeders.lock().get(&(*space, *blob))
        && now.saturating_sub(*at) < CACHE_MS
    {
        return *n;
    }
    let channel = refs_of(node, blob)
        .into_iter()
        .find(|r| r.space == *space)
        .map(|r| r.channel);
    let probes = member_peers(node, space, channel.as_ref())
        .into_iter()
        .map(|peer| {
            let node = node.clone();
            let req = YpRequest::BlobHave {
                space: *space,
                blob: *blob,
            };
            async move {
                match tokio::time::timeout(PROBE, crate::p2p::request(&node, peer, req)).await {
                    Ok(Ok(YpResponse::Have { bitmap, .. })) => bitmap.iter().any(|b| *b != 0),
                    // 바쁘다는 답도 제공 중인 시더입니다.
                    Ok(Ok(YpResponse::Busy { .. })) => true,
                    _ => false,
                }
            }
        });
    let n = futures::future::join_all(probes)
        .await
        .into_iter()
        .filter(|x| *x)
        .count() as u32;
    let mut cache = node.blobs.seeders.lock();
    if cache.len() > 1024 {
        cache.clear();
    }
    cache.insert((*space, *blob), (now, n));
    n
}

// ───────────── 정리 주기 ─────────────

fn spawn_sweep(node: &Arc<Node>) {
    let weak = Arc::downgrade(node);
    node.tasks.spawn(async move {
        let mut tick = tokio::time::interval(SWEEP_INTERVAL);
        loop {
            tick.tick().await;
            let Some(node) = weak.upgrade() else { break };
            if node.is_shutting_down() {
                break;
            }
            // 저장소를 많이 읽고 쓰며(정리·보충·지우기) 정리 잠금을 기다릴 수 있어 블로킹 스레드에서 돌립니다.
            let _ = tokio::task::spawn_blocking(move || sweep(&node)).await;
        }
    });
}

/// 프로필 색인을 다시 만들고, 휴면이 아닌 공간의 이모지·아이콘과 친구 프로필 이미지를 받을 블롭에 넣습니다.
/// 가끔 지워진 메시지·떠난 공간의 첨부를 지웁니다.
fn sweep(node: &Arc<Node>) {
    let n = node.blobs.sweeps.fetch_add(1, Ordering::Relaxed);
    rebuild_profile_index(node);
    let Ok(me) = node.me() else { return };
    for space in node.space_ids() {
        if policy::is_marked_dormant(node, &space) {
            continue;
        }
        let Some(state) = node.space_state(&space) else {
            continue;
        };
        if !state.is_member(&me.user()) {
            continue;
        }
        let pp = policy::peer_policy(node, Some(&space));
        // 이모지도 아이콘처럼 늘 보이므로 크기 한도 없이 종류만 봅니다(크기는 크루 한도가 있으면 그것으로 묶음).
        if pp.wants(DataType::Emoji) {
            for e in state.emojis.values() {
                add_want(
                    node,
                    e.blob,
                    Want {
                        space,
                        channel: None,
                        data_type: DataType::Emoji,
                        size: emoji_fetch_cap(&state),
                        manual: false,
                        priority: pp.priority(DataType::Emoji),
                        deferred: false,
                        profile: false,
                        added_ms: 0,
                        backfill: false,
                    },
                );
            }
        }
        // 공간 아이콘(목록에 늘 보이는 작은 이미지)도 미리 받습니다. 아이콘·프로필 이미지는 가져올 때 8MiB로 묶이고
        // 늘 보이므로 첨부 자동 받기 크기 한도(Tor 기본 2MiB)로 막지 않고 종류만 봅니다.
        if let Some(icon) = state.settings.icon
            && pp.wants(DataType::Image)
        {
            add_want(
                node,
                icon,
                Want {
                    space,
                    channel: None,
                    data_type: DataType::Image,
                    size: MAX_PROFILE_IMAGE_BYTES,
                    manual: false,
                    priority: pp.priority(DataType::Image),
                    deferred: false,
                    profile: false,
                    added_ms: 0,
                    backfill: false,
                },
            );
        }
        // 친구(1:1 공간 상대)의 프로필 이미지만 미리 받습니다. 크루 멤버의 것은 화면이 직접 요청합니다(blob.fetch).
        if state.kind == SpaceKind::Space && pp.wants(DataType::Image) {
            for u in state.members.keys().filter(|u| **u != me.user()) {
                if let Ok(Some(sp)) = node.store.get_profile(u) {
                    want_profile_images(node, space, &pp, &sp.profile);
                }
            }
        }
    }
    cache::enforce(node);
    prefetch::backfill(node);
    if n % CLEANUP_EVERY == CLEANUP_EVERY - 1 {
        cleanup(node);
    }
}

/// 프로필 이미지(아바타·배너)를 이 공간에서 받을 블롭에 넣습니다.
fn want_profile_images(node: &Node, space: SpaceId, pp: &PeerPolicy, p: &Profile) {
    for b in [p.avatar, p.banner].into_iter().flatten() {
        add_want(
            node,
            b,
            Want {
                space,
                channel: None,
                data_type: DataType::Image,
                size: MAX_PROFILE_IMAGE_BYTES,
                manual: false,
                priority: pp.priority(DataType::Image),
                deferred: false,
                profile: true,
                added_ms: 0,
                backfill: false,
            },
        );
    }
}

/// 받은 프로필이 더 새 판으로 바뀌었을 때(`identity::accept_profile`): 프로필 색인에 새 이미지를 넣고, 친구면 새 사진을
/// 바로 받을 블롭에 넣습니다(60초 정리를 기다리지 않게).
pub(crate) fn on_profile_changed(node: &Node, user: &UserId, p: &Profile) {
    {
        let mut idx = node.blobs.profile_index.write();
        for b in [p.avatar, p.banner].into_iter().flatten() {
            idx.entry(b).or_default().insert(*user);
        }
    }
    let Ok(me) = node.me() else { return };
    if *user == me.user() {
        return;
    }
    for space in node.space_ids() {
        if policy::is_marked_dormant(node, &space) {
            continue;
        }
        let Some(state) = node.space_state(&space) else {
            continue;
        };
        if state.kind != SpaceKind::Space || !state.is_member(user) || !state.is_member(&me.user())
        {
            continue;
        }
        let pp = policy::peer_policy(node, Some(&space));
        if pp.wants(DataType::Image) {
            want_profile_images(node, space, &pp, p);
        }
    }
}

fn rebuild_profile_index(node: &Node) {
    let mut idx: HashMap<BlobId, BTreeSet<UserId>> = HashMap::new();
    let mut seen = HashSet::new();
    for space in node.space_ids() {
        let Some(state) = node.space_state(&space) else {
            continue;
        };
        for u in state.members.keys() {
            if !seen.insert(*u) {
                continue;
            }
            if let Ok(Some(sp)) = node.store.get_profile(u) {
                for b in [sp.profile.avatar, sp.profile.banner].into_iter().flatten() {
                    idx.entry(b).or_default().insert(*u);
                }
            }
        }
    }
    *node.blobs.profile_index.write() = idx;
}

/// 실은 메시지가 모두 지워졌거나 공간을 떠난 첨부를 지웁니다. 다른 멤버가 남의 블롭 ID를 실은 메시지를 보내고
/// 지워서 내 블롭을 지우게 하지 못하도록:
/// - 이모지·아이콘으로 쓰이거나 프로필 이미지로 등록된 블롭은 건드리지 않습니다.
/// - 조각을 가진 블롭은 키가 확인된 메시지(그 블롭이 등록된 공간의 것)만 보고, 그런 메시지가 하나도 없으면
///   (어디서 온 것인지 모름) 두며, 모두 지워졌을 때만 지웁니다.
/// - 조각이 없는 블롭은 잃을 것이 없으니 기록이 모두 죽었으면 등록과 받을 요청만 지웁니다.
fn cleanup(node: &Node) {
    let known: HashSet<SpaceId> = node.space_ids().into_iter().collect();
    let mut in_use: HashSet<BlobId> = HashSet::new();
    for s in &known {
        if let Some(st) = node.space_state(s) {
            in_use.extend(st.emojis.values().map(|e| e.blob));
            in_use.extend(st.settings.icon);
        }
    }
    let rows = node
        .store
        .settings_with_prefix(REF_PREFIX)
        .unwrap_or_default();
    for (key, value) in rows {
        let Ok(blob) = key[REF_PREFIX.len()..].parse::<BlobId>() else {
            continue;
        };
        if serde_json::from_value::<Vec<BlobRef>>(value).is_err() {
            continue;
        }
        if in_use.contains(&blob)
            || node
                .store
                .blob_in_space(&blob, &PROFILE_SPACE)
                .unwrap_or(true)
        {
            continue;
        }
        let alive = |r: &BlobRef| {
            known.contains(&r.space) && !node.store.is_deleted(&r.event).unwrap_or(true)
        };
        let status = node.store.blob_status(&blob).ok().flatten();
        let has_data = status.as_ref().is_some_and(|s| s.have_count > 0);
        if has_data {
            let trusted: Vec<BlobRef> = verified_refs(node, &blob)
                .into_iter()
                .filter(|r| node.store.blob_in_space(&blob, &r.space).unwrap_or(false))
                .collect();
            if trusted.is_empty() || trusted.iter().any(alive) {
                continue;
            }
        } else if refs_of(node, &blob)
            .iter()
            .any(|r| r.verified != Some(false) && alive(r))
        {
            continue;
        }
        remove_want(node, &blob);
        if status.is_some() {
            let _ = node.store.blob_delete(&blob);
        }
        cache::forget(node, &blob);
        let _ = node.store.delete_setting(&key);
        debug!(blob = %blob.short(), "쓰이지 않는 첨부 지움");
    }
}

/// 보내지 못한 메시지를 위해 만든 첨부 블롭을 지웁니다(아무 메시지도 가리키지 않음).
pub(crate) fn discard_prepared(node: &Node, blobs: &[BlobId]) {
    for b in blobs {
        if refs_of(node, b).is_empty() {
            let _ = node.store.blob_delete(b);
        }
    }
}

// ───────────── 시험 고리 ─────────────

fn record_chunk(node: &Node, blob: &BlobId, peer: PeerId) {
    let mut s = node.blobs.test.stats.lock();
    if s.len() >= MAX_STATS && !s.contains_key(blob) {
        s.clear();
    }
    *s.entry(*blob)
        .or_default()
        .chunks_from
        .entry(peer)
        .or_default() += 1;
}

fn record_bad(node: &Node, blob: &BlobId) {
    let mut s = node.blobs.test.stats.lock();
    if s.len() >= MAX_STATS && !s.contains_key(blob) {
        s.clear();
    }
    s.entry(*blob).or_default().bad_chunks += 1;
}

fn record_busy(node: &Node, blob: &BlobId) {
    let mut s = node.blobs.test.stats.lock();
    if s.len() >= MAX_STATS && !s.contains_key(blob) {
        s.clear();
    }
    s.entry(*blob).or_default().busy_replies += 1;
}

fn corrupt_uploads(node: &Node) -> bool {
    node.blobs.test.corrupt_uploads.load(Ordering::Relaxed)
}

fn hold_at(node: &Node) -> Option<u32> {
    *node.blobs.test.hold_at.lock()
}

fn record_seeders(node: &Node, blob: &BlobId, n: usize) {
    let mut s = node.blobs.test.seeders_seen.lock();
    if s.len() >= MAX_STATS && !s.contains_key(blob) {
        s.clear();
    }
    s.insert(*blob, n);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn want(space: SpaceId, deferred: bool, added_ms: u64) -> Want {
        Want {
            space,
            channel: Some(ChannelId([1; 16])),
            data_type: DataType::File,
            size: 1,
            manual: false,
            priority: 1,
            deferred,
            profile: false,
            added_ms,
            backfill: false,
        }
    }

    fn blob(n: usize) -> BlobId {
        let mut b = [0u8; 32];
        b[..8].copy_from_slice(&(n as u64).to_be_bytes());
        BlobId(b)
    }

    #[test]
    fn deferred_wants_do_not_block_other_downloads() {
        let mut m = Manager::default();
        // 열지 않은 채널의 미룬 내려받기가 아무리 쌓여도 다른 자동 내려받기는 막히지 않습니다.
        for i in 0..MAX_WANTS {
            let space = SpaceId([(i % 16) as u8; 16]);
            m.wants.insert(blob(i), want(space, true, i as u64 + 1));
        }
        assert!(!auto_full(&m));
        // 미루지 않은 자동 내려받기만 상한까지 셉니다.
        let mut m = Manager::default();
        for i in 0..MAX_WANTS {
            m.wants.insert(blob(i), want(SpaceId([1; 16]), false, 1));
        }
        assert!(auto_full(&m));
    }

    #[test]
    fn deferred_wants_are_capped_oldest_first() {
        // 공간마다 상한: 그 공간의 가장 오래된 미룬 것을 버립니다(다른 공간 것은 그대로).
        let busy = SpaceId([1; 16]);
        let other = SpaceId([2; 16]);
        let mut m = Manager::default();
        m.wants.insert(blob(999_999), want(other, true, 0));
        for i in 0..MAX_DEFERRED_PER_SPACE {
            m.wants.insert(blob(i), want(busy, true, 10 + i as u64));
        }
        let dropped = make_room_deferred(&mut m, &busy);
        assert_eq!(dropped, vec![blob(0)]);
        assert!(m.wants.contains_key(&blob(999_999)));
        assert_eq!(m.wants.len(), MAX_DEFERRED_PER_SPACE);
        // 전체 상한: 공간마다는 넉넉해도 전체가 넘치면 가장 오래된 것부터.
        let mut m = Manager::default();
        for i in 0..MAX_DEFERRED {
            let space = SpaceId([(i % 20) as u8 + 10; 16]);
            m.wants.insert(blob(i), want(space, true, 5 + i as u64));
        }
        let dropped = make_room_deferred(&mut m, &SpaceId([200; 16]));
        assert_eq!(dropped, vec![blob(0)]);
        assert_eq!(m.wants.len(), MAX_DEFERRED - 1);
        // 미루지 않은 것은 버리지 않습니다.
        let mut m = Manager::default();
        for i in 0..MAX_DEFERRED_PER_SPACE {
            m.wants.insert(blob(i), want(busy, false, 1));
        }
        assert!(make_room_deferred(&mut m, &busy).is_empty());
    }
}
