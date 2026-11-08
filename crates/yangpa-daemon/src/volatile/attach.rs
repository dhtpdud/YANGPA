//! 휘발성 채널 첨부(X1): RAM에서만 사는 첨부를 보내고·받고·내줍니다.
//!
//! - 보내기([`prepare_files`]): 데몬이 고른 파일을 읽어 메모리에서 썸네일·메타데이터를 만들고, 첨부마다 새 키로
//!   조각 암호화해 RAM 저장소에 올립니다(`VolatileStore::stage_blob`). 첨부 키는 세션 키로 암호화한 메시지 본문에만
//!   실리고, 메시지를 만든 뒤 그 메시지에 묶습니다(`claim_blob`). 디스크 블롭 저장소·DB·임시 파일을 거치지 않습니다.
//!   묶기 전의 첨부는 [`Staged`]가 쥐고 있다가 보내기가 어떻게 끝나든(실패·취소 포함) 묶이지 않은 것을 지웁니다.
//!   하나라도 묶지 못하면 메시지를 보내지 않습니다(아무도 받을 수 없는 첨부가 실린 메시지가 나가지 않게).
//! - 받기([`on_ready`], [`fetch`]): 메시지를 풀어 권한을 확인한 뒤 자동 내려받기 조건(피어 설정의 종류·한도, 차단·휴면
//!   아님)이면 대기열(상한 [`MAX_WANTS`])에 넣고, 일꾼 하나([`start`])가 일반 첨부와 같은 동시 내려받기 한도
//!   (`blobs::borrow_slot`, Tor로 돌면 Tor 한도)와 수신 상한(`blobs::admit_chunks`) 안에서 지금
//!   참가자에게 일반 블롭 요청(`BlobManifest`·`BlobHave`·`BlobChunk`)으로 받아 조각마다 매니페스트 해시로 검증해
//!   RAM에 둡니다. 받는 동안만 크기만큼 RAM 예산을 잡고(모자라면 받지 않음), 받기를 쉬는 동안은 가진 만큼으로
//!   줄입니다. 보낸 기기가 나가 있으면 간격을 늘려 가며 다시 시도하고, 오래 진전이 없으면 받다 만 것까지 지웁니다.
//! - 내주기([`serve`]): 지금 참가자인 기기에게, 메시지가 살아 있는 동안만. 차단·시더 설정·대역폭 검사는 일반 첨부와
//!   같습니다(`sync_service::check`). 그 밖에는 없는 블롭과 같은 답(`NotFound`)이라 있는지도 드러나지 않습니다.
//! - 수명: 첨부는 실은 메시지에 묶여, 메시지가 어떤 길로든 지워지면(만료·삭제·퇴장·채널 삭제·종료) 저장소가 0으로
//!   덮어써 해제합니다. 받던 작업도 그때 멈춥니다.
//!
//! 로그에는 블롭 ID 앞부분만 남기고 파일 이름·경로·키·내용은 남기지 않습니다.

use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use base64::Engine;
use futures::StreamExt;
use futures::future::BoxFuture;
use futures::stream::FuturesUnordered;
use parking_lot::Mutex;
use tracing::debug;
use yangpa_core::blob::BlobManifest;
use yangpa_core::body::{Attachment, EventBody, Secret32};
use yangpa_core::event::{Lane, StreamId};
use yangpa_core::ids::{BlobId, DeviceId, EventId, SpaceId, UserId};
use yangpa_core::wire::{WireErrorCode, YpRequest, YpResponse};
use yangpa_crypto::blob::plaintext_len;
use yangpa_crypto::{BlobEncryptor, BlobReader, CryptoError, decrypt_blob_chunk, packed_source};
use yangpa_net::{NetError, PeerId};
use yangpa_rpc::api::{
    AttachmentInput, AttachmentView, BlobReadParams, BlobReadResult, BlobSaveParams,
    BlobStatusView, Empty, Notification, m,
};
use yangpa_rpc::{RpcError, RpcOutput};
use yangpa_store::{StoreError, VolatileStore};
use yangpa_sync::policy::{DataType, SeedRequest};
use yangpa_sync::score::Penalty;
use yangpa_sync::state::limits::{MAX_ATTACHMENTS, MAX_THUMBNAIL_BYTES};
use yangpa_sync::swarm::{Bitmap, ChunkOutcome, ChunkScheduler, RequestError, SwarmConfig};
use zeroize::{Zeroize, Zeroizing};

use super::{ChanKey, ChanRt, Effects, member_device};
use crate::api::{CallCtx, R};
use crate::blobs::media::{
    LOCAL_DECODE, THUMBNAIL_MIMES, has_mp4_duration, image_size, make_thumbnail, mp4_duration_ms,
    sniff_mime,
};
use crate::blobs::{Origin, send_limit, size_text};
use crate::filelink::{self, Route};
use crate::node::Node;
use crate::{blobs, policy, sync_service};

/// 형식 판별·이미지 크기에 쓰는 머리 바이트.
const HEAD_BYTES: usize = 64 * 1024;
/// 원본을 그대로 썸네일로 싣는 이미지 최대 크기(일반 첨부와 같음).
const INLINE_THUMBNAIL_BYTES: u64 = 32 * 1024;
/// 줄인 썸네일 최대 크기(검증 한도 안).
const MADE_THUMBNAIL_BYTES: usize = if MAX_THUMBNAIL_BYTES < 48 * 1024 {
    MAX_THUMBNAIL_BYTES
} else {
    48 * 1024
};
/// 썸네일을 만들려고 풀어 볼 이미지 파일 최대 크기.
const THUMBNAIL_SOURCE_BYTES: u64 = 50 * 1024 * 1024;
/// 받을 휘발성 첨부 대기열 상한(노드 전체). 가득 차면 아직 아무것도 받지 못한 가장 오래된 자동 요청을 밀어냅니다
/// (참가자 하나가 내주지 않을 첨부를 잔뜩 실어 보내도 작업·요청이 한없이 늘지 않게).
const MAX_WANTS: usize = 256;
/// 자리를 기다리는 동안에도 가끔 다시 봅니다(깨움을 놓쳐도 멈추지 않게).
const SLOT_RECHECK: Duration = Duration::from_secs(5);
/// 할 일이 없을 때 일꾼이 다시 보는 간격.
const IDLE_RECHECK: Duration = Duration::from_secs(30);
/// 자동으로 받을 때 메시지에 적힌 크기보다 이만큼 넘게 큰 블롭은 받지 않습니다(크기를 속인 첨부).
const SIZE_SLACK: u64 = 64 * 1024;
/// 참가자 보유 비트맵을 다시 묻는 간격.
const REQUERY_MS: u64 = 2_000;
/// 요청 중인 것 없이 이만큼 진전이 없으면 이번 시도를 끝내고 나중에 다시 합니다.
const STALL_MS: u64 = 15_000;
/// 다시 시도하는 간격(처음부터 두 배씩, 상한까지).
const RETRY_START: Duration = Duration::from_secs(1);
const RETRY_MAX: Duration = Duration::from_secs(10);
/// 이만큼 아무에게서도 조각을 받지 못하면 그만 받습니다(기한 없는 채널에서 보낸 기기가 영영 돌아오지 않을 때
/// 끝없이 묻지 않게). `blob.fetch`로 다시 시작할 수 있습니다.
const GIVE_UP: Duration = Duration::from_secs(10 * 60);
/// `blob.status`의 참가자 보유 확인 시간.
const PROBE: Duration = Duration::from_secs(2);

fn now_ms() -> u64 {
    yangpa_core::time::Timestamp::now().as_millis()
}

/// 휘발성 첨부 영역의 노드 수준 상태(RAM 전용, 내용 없음).
#[derive(Default)]
pub(super) struct AttachState {
    /// 받을 첨부 대기열.
    queue: Mutex<Queue>,
    /// 대기열 일꾼을 깨웁니다(새 요청, 끝난 받기, 빈 내려받기 자리).
    wake: Arc<tokio::sync::Notify>,
    /// 시험용 조절(`testkit::volatile`). 시험이 아니면 늘 기본값.
    tuning: Mutex<Tuning>,
    /// 시험용: 다음 이만큼의 메시지 묶기를 실패로 처리(묶기 전에 첨부가 사라진 경우 흉내).
    fail_claims: AtomicU32,
}

impl AttachState {
    /// 대기열에 있거나 받는 중인 첨부 수(시험 도구용).
    pub(super) fn fetching(&self) -> usize {
        self.queue.lock().wants.len()
    }
}

/// 시험용 조절. `None`이면 기본값.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Tuning {
    /// 요청 중인 것 없이 진전이 없으면 시도를 끝내는 시간([`STALL_MS`]).
    pub(crate) stall: Option<Duration>,
    /// 진전 없이 이만큼 지나면 그만 받는 시간([`GIVE_UP`]).
    pub(crate) give_up: Option<Duration>,
    /// 받은 조각 + 요청 중인 조각이 이 수에 닿으면 더 요청하지 않습니다(보낸 기기가 중간에 나간 경우 흉내).
    pub(crate) hold_at: Option<u32>,
}

impl Tuning {
    fn stall_ms(&self) -> u64 {
        self.stall.map_or(STALL_MS, |d| d.as_millis() as u64)
    }

    fn give_up(&self) -> Duration {
        self.give_up.unwrap_or(GIVE_UP)
    }
}

/// 받을 첨부 하나(대기열의 한 줄, 내용 없음).
#[derive(Clone, Debug)]
struct Want {
    key: ChanKey,
    owner: EventId,
    /// 메시지에 적힌 평문 크기.
    size: u64,
    manual: bool,
    /// 넣은 순서.
    seq: u64,
    /// 자리를 빌려 받는 중.
    running: bool,
    /// 이때까지 다시 시도하지 않음.
    not_before: Option<Instant>,
    /// 다음 재시도 간격.
    backoff: Duration,
    /// 마지막으로 조각을 받은(또는 넣은·직접 요청한) 시각.
    last_progress: Instant,
    /// 조각을 하나라도 받은 적 있는지.
    progressed: bool,
}

/// 대기열에 넣은 결과.
#[derive(Debug, PartialEq, Eq)]
enum Admit {
    Added,
    /// 이미 있음(직접 요청이면 직접 요청으로 바꾸고 바로 다시 시도).
    Updated,
    /// 가득 찼고 밀어낼 것도 없음.
    Full,
}

/// 받을 첨부 대기열: (공간, 블롭) → 요청. 일꾼 하나가 직접 요청 먼저, 넣은 순서대로 꺼냅니다.
#[derive(Default)]
struct Queue {
    wants: HashMap<(SpaceId, BlobId), Want>,
    seq: u64,
}

impl Queue {
    /// 넣습니다. 가득 차면 받는 중이 아니고 아직 아무것도 받지 못한 가장 오래된 자동 요청을 밀어냅니다.
    fn admit(
        &mut self,
        k: (SpaceId, BlobId),
        key: ChanKey,
        owner: EventId,
        size: u64,
        manual: bool,
        now: Instant,
    ) -> Admit {
        if let Some(w) = self.wants.get_mut(&k) {
            if manual {
                w.manual = true;
                w.not_before = None;
                w.backoff = RETRY_START;
                w.last_progress = now;
            }
            return Admit::Updated;
        }
        if self.wants.len() >= MAX_WANTS {
            let victim = self
                .wants
                .iter()
                .filter(|(_, w)| !w.running && !w.manual && !w.progressed)
                .min_by_key(|(_, w)| w.seq)
                .map(|(k, _)| *k);
            match victim {
                Some(v) => {
                    self.wants.remove(&v);
                }
                None => return Admit::Full,
            }
        }
        self.seq += 1;
        self.wants.insert(
            k,
            Want {
                key,
                owner,
                size,
                manual,
                seq: self.seq,
                running: false,
                not_before: None,
                backoff: RETRY_START,
                last_progress: now,
                progressed: false,
            },
        );
        Admit::Added
    }

    /// 지금 시작할 것(직접 요청 먼저, 넣은 순서).
    fn next_ready(&self, now: Instant) -> Option<(SpaceId, BlobId)> {
        self.wants
            .iter()
            .filter(|(_, w)| !w.running && w.not_before.is_none_or(|t| t <= now))
            .min_by_key(|(_, w)| (!w.manual, w.seq))
            .map(|(k, _)| *k)
    }

    /// 가장 이른 재시도 시각.
    fn next_due(&self) -> Option<Instant> {
        self.wants
            .values()
            .filter(|w| !w.running)
            .filter_map(|w| w.not_before)
            .min()
    }
}

/// 메시지에 실린 첨부 하나(보기에 있는 메시지만).
#[derive(Clone, Copy, Debug)]
pub(super) struct AttRef {
    pub(super) owner: EventId,
    /// 메시지에 적힌 평문 크기.
    pub(super) size: u64,
    pub(super) data_type: DataType,
}

/// 채널 하나의 첨부 색인(블롭 → 실은 메시지). 처음 실은 메시지만 주인으로 봅니다(남의 블롭 ID를 베낀 메시지가
/// 그 첨부의 수명을 늘리거나 가로채지 못하게).
#[derive(Default)]
pub(super) struct Index {
    by_blob: HashMap<BlobId, AttRef>,
    by_owner: HashMap<EventId, Vec<BlobId>>,
}

impl Index {
    pub(super) fn add(&mut self, owner: EventId, blob: BlobId, size: u64, data_type: DataType) {
        if self.by_blob.contains_key(&blob) {
            return;
        }
        self.by_blob.insert(
            blob,
            AttRef {
                owner,
                size,
                data_type,
            },
        );
        self.by_owner.entry(owner).or_default().push(blob);
    }

    pub(super) fn remove_owner(&mut self, owner: &EventId) {
        for b in self.by_owner.remove(owner).unwrap_or_default() {
            if self.by_blob.get(&b).is_some_and(|a| a.owner == *owner) {
                self.by_blob.remove(&b);
            }
        }
    }

    pub(super) fn get(&self, blob: &BlobId) -> Option<AttRef> {
        self.by_blob.get(blob).copied()
    }
}

/// 풀어 확인한 메시지의 첨부(잠금 밖에서 자동 내려받기를 정함).
pub(super) struct Ready {
    pub(super) key: ChanKey,
    pub(super) owner: EventId,
    pub(super) user: UserId,
    pub(super) device: DeviceId,
    pub(super) blobs: Vec<(BlobId, u64, DataType)>,
}

/// 메시지 본문의 첨부 목록(`try_ready`가 내용을 지우기 전에 뽑음).
pub(super) fn listed(body: &EventBody) -> Vec<(BlobId, u64, DataType)> {
    match body {
        EventBody::Message(mb) => mb
            .attachments
            .iter()
            .map(|a| (a.blob, a.size, DataType::of_attachment(a)))
            .collect(),
        _ => Vec::new(),
    }
}

/// 평문 크기 → 암호문 크기 어림(조각마다 태그 16바이트, 가장 작은 조각 16KiB 기준으로 넉넉히).
fn ciphertext_estimate(size: u64) -> u64 {
    size.saturating_add(size / 512).saturating_add(16)
}

fn b64(bytes: &[u8]) -> Option<String> {
    (!bytes.is_empty()).then(|| base64::engine::general_purpose::STANDARD.encode(bytes))
}

/// 메시지 보기의 첨부. `downloaded`는 RAM에 모두 받았는지(보내는 쪽이 올린 것 포함).
pub(super) fn views(
    store: &VolatileStore,
    key: ChanKey,
    owner: &EventId,
    atts: &[Attachment],
) -> Vec<AttachmentView> {
    atts.iter()
        .map(|a| AttachmentView {
            blob: a.blob,
            name: a.name.clone(),
            mime: a.mime.clone(),
            size: a.size,
            width: a.width,
            height: a.height,
            duration_ms: a.duration_ms,
            thumbnail_base64: b64(&a.thumbnail),
            downloaded: store
                .blob_info(&key.0, &key.1, &a.blob)
                .is_some_and(|i| i.owner.is_none_or(|o| o == *owner) && i.have == i.total),
        })
        .collect()
}

// ───────────── 보내기 ─────────────

fn changed() -> RpcError {
    RpcError::conflict("파일이 읽는 중에 바뀌었습니다. 다시 시도하세요")
}

fn crypto_err(e: CryptoError) -> RpcError {
    match e {
        CryptoError::BlobLength => changed(),
        CryptoError::TooLarge(_) => RpcError::invalid_input("파일이 너무 큽니다"),
        _ => RpcError::invalid_input("파일을 읽을 수 없습니다"),
    }
}

fn no_room(need: u64, room: u64) -> RpcError {
    RpcError::conflict(format!(
        "휘발성 첨부에 쓸 RAM이 모자랍니다(필요 {}, 남은 {}). 다른 휘발성 첨부가 만료되면 다시 시도하세요",
        size_text(need),
        size_text(room)
    ))
}

fn too_big(max: u64) -> RpcError {
    RpcError::invalid_input(format!(
        "휘발성 채널의 첨부는 하나에 {}까지입니다",
        size_text(max)
    ))
}

/// 이 휘발성 채널에서 첨부 하나의 한도: 이 기기 설정과 채널 한도 중 작은 쪽(RAM 예산은 따로 확인).
fn channel_max(node: &Node, key: ChanKey) -> u64 {
    let limit = node
        .space_state(&key.0)
        .and_then(|st| st.attach_limit(&key.1));
    send_limit(node.cfg.volatile.max_attachment_bytes, limit)
}

fn read_err() -> RpcError {
    RpcError::invalid_input("파일을 읽을 수 없습니다")
}

fn rewind<S: Seek>(f: &mut S) -> Result<(), RpcError> {
    f.seek(SeekFrom::Start(0))
        .map(|_| ())
        .map_err(|_| read_err())
}

/// 메모리에서 만든 첨부 정보(형식·크기·썸네일·재생 길이). 일반 첨부와 같은 규칙입니다.
struct Described {
    mime: String,
    width: Option<u32>,
    height: Option<u32>,
    duration_ms: Option<u64>,
    thumbnail: Vec<u8>,
}

fn describe<S: Read + Seek>(f: &mut S, path: &Path, len: u64) -> Result<Described, RpcError> {
    let mut head = Zeroizing::new(Vec::with_capacity(HEAD_BYTES));
    Read::by_ref(f)
        .take(HEAD_BYTES as u64)
        .read_to_end(&mut head)
        .map_err(|_| read_err())?;
    rewind(f)?;
    let mime = sniff_mime(&head, path);
    let (mut width, mut height) = if mime.starts_with("image/") {
        image_size(&head).map_or((None, None), |(w, h)| (Some(w), Some(h)))
    } else {
        (None, None)
    };
    let thumbable = THUMBNAIL_MIMES.contains(&mime.as_str());
    let mut thumbnail = if thumbable && len <= INLINE_THUMBNAIL_BYTES && head.len() as u64 == len {
        head.to_vec()
    } else {
        Vec::new()
    };
    // 썸네일·길이는 있으면 좋은 정보라, 만들지 못해도 첨부는 그대로 보냅니다.
    if thumbnail.is_empty() && thumbable && len <= THUMBNAIL_SOURCE_BYTES {
        if let Some(t) = make_thumbnail(
            BufReader::new(&mut *f),
            &mime,
            MADE_THUMBNAIL_BYTES,
            LOCAL_DECODE,
        ) {
            thumbnail = t.jpeg;
            (width, height) = (Some(t.width), Some(t.height));
        }
        rewind(f)?;
    }
    let duration_ms = if has_mp4_duration(&mime) {
        let d = mp4_duration_ms(f, len);
        rewind(f)?;
        d
    } else {
        None
    };
    Ok(Described {
        mime,
        width,
        height,
        duration_ms,
        thumbnail,
    })
}

/// 파일 하나를 새 키로 조각 암호화해 RAM 저장소에 올립니다(한 번만 읽음: 매니페스트와 조각을 같은 읽기에서 만듦).
fn stage_one(
    store: &VolatileStore,
    key: ChanKey,
    origin: Origin,
    name: String,
    max: u64,
    strip: bool,
    chunk_size: u32,
) -> Result<Attachment, RpcError> {
    let (mut f, len, path) = origin.open(&name, max, strip, too_big)?;
    let d = describe(&mut f, &path, len)?;
    let secret = Secret32::random();
    // 평문은 압축 버퍼와 암호화기의 조각 버퍼(둘 다 해제 시 0으로 덮어씀)에만 잠깐 있습니다.
    let (src, plen) = packed_source(&mut f, len).map_err(crypto_err)?;
    let mut enc = BlobEncryptor::new(&secret, src, plen, chunk_size).map_err(crypto_err)?;
    let mut chunks = Vec::with_capacity(enc.total_chunks() as usize);
    while let Some((_, data)) = enc.next_chunk().map_err(crypto_err)? {
        chunks.push(data);
    }
    let manifest = enc.finish().map_err(crypto_err)?;
    let need = manifest.size;
    let blob = store
        .stage_blob(&key.0, &key.1, manifest, chunks)
        .map_err(|e| match e {
            StoreError::LimitExceeded(_) => no_room(need, store.blob_room()),
            _ => RpcError::internal("휘발성 첨부를 RAM에 올리지 못했습니다"),
        })?;
    Ok(Attachment {
        blob,
        key: secret,
        name,
        mime: d.mime,
        size: len,
        width: d.width,
        height: d.height,
        duration_ms: d.duration_ms,
        thumbnail: d.thumbnail,
    })
}

/// 보내는 메시지의 첨부 중 RAM에 올렸지만 아직 메시지에 묶지 않은 것. 저장소는 이것을 시간으로 지우지 않으므로
/// (파일을 천천히 읽는 동안 먼저 올린 것이 사라지지 않게), 이 값이 사라질 때 묶이지 않은 것을 지웁니다. 보내기
/// 성공·실패, 호출 취소, 읽기 작업의 패닉 어느 쪽이든 같습니다(묶인 것은 메시지와 함께 지워짐).
pub(super) struct Staged {
    node: Arc<Node>,
    key: ChanKey,
    pub(super) blobs: Vec<BlobId>,
}

impl Staged {
    fn new(node: Arc<Node>, key: ChanKey) -> Self {
        Staged {
            node,
            key,
            blobs: Vec::new(),
        }
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        for b in &self.blobs {
            self.node
                .volatile
                .store
                .remove_staged(&self.key.0, &self.key.1, b);
        }
    }
}

/// `message.send`의 첨부 경로(절대 경로) → RAM에 올린 첨부와 그것을 쥔 [`Staged`]. 하나라도 실패하면 이번에 올린
/// 것을 모두 지웁니다. 파일을 읽기 전에 크기 한도와 RAM 예산부터 확인해 깔끔하게 거절합니다.
/// 앱이 만든 동영상·음성 미디어 힌트(`thumbnail_base64` 등)는 쓰지 않습니다: 데스크톱 앱은 휘발성 채널에서 웹뷰로 미디어를
/// 미리 읽지 않고(웹뷰 캐시를 거치지 않게), 다른 클라이언트가 보내도 무시합니다.
pub(super) async fn prepare_files(
    node: &Arc<Node>,
    key: ChanKey,
    inputs: &[AttachmentInput],
) -> Result<(Vec<Attachment>, Staged), RpcError> {
    if inputs.is_empty() {
        return Ok((Vec::new(), Staged::new(node.clone(), key)));
    }
    crate::blobs::check_attach_allowed(
        node.space_state(&key.0)
            .and_then(|st| st.attach_limit(&key.1)),
    )?;
    if inputs.len() > MAX_ATTACHMENTS {
        return Err(RpcError::invalid_input(format!(
            "첨부는 {MAX_ATTACHMENTS}개까지입니다"
        )));
    }
    let mut jobs = Vec::with_capacity(inputs.len());
    for inp in inputs {
        let (origin, name) = Origin::of(node, inp)?;
        jobs.push((origin, name, !inp.keep_metadata));
    }
    let max = channel_max(node, key);
    let chunk_size = node.cfg.sharing.chunk_size;
    let node2 = node.clone();
    tokio::task::spawn_blocking(move || {
        let store = &node2.volatile.store;
        let mut need = 0u64;
        for (origin, _, _) in &jobs {
            let len = origin.len()?;
            if len > max {
                return Err(too_big(max));
            }
            need = need.saturating_add(ciphertext_estimate(len));
        }
        let room = store.blob_room();
        if need > room {
            return Err(no_room(need, room));
        }
        // 실패하면(`?`) `staged`가 사라지며 이번에 올린 것을 지웁니다.
        let mut staged = Staged::new(node2.clone(), key);
        let mut out: Vec<Attachment> = Vec::with_capacity(jobs.len());
        for (origin, name, strip) in jobs {
            let a = stage_one(store, key, origin, name, max, strip, chunk_size)?;
            staged.blobs.push(a.blob);
            out.push(a);
        }
        Ok((out, staged))
    })
    .await
    .map_err(|_| RpcError::internal("첨부 준비 작업이 멈췄습니다"))?
}

/// 미리 올린 첨부를 막 적용한 메시지에 묶습니다. 묶지 못하면 false(메시지가 그새 밀려났거나 채널을 나감).
pub(super) fn claim(node: &Node, key: ChanKey, blob: &BlobId, owner: &EventId) -> bool {
    let store = &node.volatile.store;
    if take_one(&node.volatile.attach.fail_claims) {
        store.remove_staged(&key.0, &key.1, blob);
    }
    store.claim_blob(&key.0, &key.1, blob, owner)
}

/// 0보다 크면 하나 줄이고 true. `fetch_update`는 최신 안정판에서 폐기 예고되었고 대체 함수(`try_update`)는
/// MSRV(1.91)에 없어 직접 CAS 반복으로 씁니다.
fn take_one(n: &AtomicU32) -> bool {
    let mut cur = n.load(Ordering::SeqCst);
    loop {
        let Some(next) = cur.checked_sub(1) else {
            return false;
        };
        match n.compare_exchange_weak(cur, next, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return true,
            Err(actual) => cur = actual,
        }
    }
}

// ───────────── 받기 ─────────────

/// 풀어 확인한 메시지의 첨부: 자동 내려받기 조건이면 대기열에 넣습니다. 내가 보낸 것, 차단한 사용자의 것, 휴면 공간,
/// 피어 설정(종류·자동 한도, Tor로 돌면 더 작은 한도) 밖이면 받지 않습니다(`blob.fetch`로 직접 받을 수 있음).
pub(super) fn on_ready(node: &Arc<Node>, items: Vec<Ready>) {
    let Ok(me) = node.me() else { return };
    let mut added = false;
    for r in items {
        if r.device == me.device_id()
            || crate::moderation::is_author_blocked(node, &r.user, &r.device)
            || policy::is_marked_dormant(node, &r.key.0)
        {
            continue;
        }
        let pp = policy::peer_policy(node, Some(&r.key.0));
        let max = channel_max(node, r.key);
        // 이 크루·채널의 받기 제한이 0이면 첨부를 받지 않습니다.
        if crate::ratelimit::blocked(
            node,
            crate::ratelimit::Dir::Down,
            &r.key.0,
            Some(&r.key.1),
            None,
        ) {
            continue;
        }
        for (blob, size, t) in r.blobs {
            if size > max || !pp.auto_download(t, size) {
                continue;
            }
            added |= enqueue(node, r.key, r.owner, blob, size, false) != Admit::Full;
        }
    }
    if added {
        kick(node);
    }
}

/// 대기열에 넣습니다(깨우지는 않음).
fn enqueue(
    node: &Node,
    key: ChanKey,
    owner: EventId,
    blob: BlobId,
    size: u64,
    manual: bool,
) -> Admit {
    let r = node.volatile.attach.queue.lock().admit(
        (key.0, blob),
        key,
        owner,
        size,
        manual,
        Instant::now(),
    );
    if r == Admit::Full {
        debug!(blob = %blob.short(), "휘발성 첨부 대기열이 가득 차 받지 않음");
    }
    r
}

/// 시험용 조절을 바꿉니다(`testkit::volatile`).
pub(super) fn set_tuning(node: &Node, tuning: Tuning) {
    *node.volatile.attach.tuning.lock() = tuning;
}

/// 시험용: 다음 `n`번의 메시지 묶기를 실패로 처리합니다.
pub(super) fn fail_claims(node: &Node, n: u32) {
    node.volatile
        .attach
        .fail_claims
        .fetch_add(n, Ordering::SeqCst);
}

/// 대기열 일꾼을 깨웁니다(일반 내려받기 자리가 나면 `blobs`도 부름).
pub(crate) fn kick(node: &Node) {
    node.volatile.attach.wake.notify_one();
}

/// 채널을 나가거나 채널이 없어질 때: 그 채널의 대기 중인 요청을 버립니다(받는 중인 것은 첨부가 지워져 곧 멈춤).
/// `None`이면 모두(종료).
pub(super) fn forget(node: &Node, key: Option<ChanKey>) {
    node.volatile
        .attach
        .queue
        .lock()
        .wants
        .retain(|_, w| w.running || key.is_some_and(|k| w.key != k));
}

/// 그 첨부를 실은 메시지가 아직 보기에 있는지(참가 중인 채널에서).
fn alive_in(
    chans: &HashMap<ChanKey, ChanRt>,
    key: ChanKey,
    owner: &EventId,
    blob: &BlobId,
) -> bool {
    chans
        .get(&key)
        .filter(|rt| rt.participating())
        .is_some_and(|rt| {
            rt.ready_message(owner).is_some()
                && rt.attach.get(blob).is_some_and(|a| a.owner == *owner)
        })
}

fn alive(node: &Node, key: ChanKey, owner: &EventId, blob: &BlobId) -> bool {
    alive_in(&node.volatile.chans.lock(), key, owner, blob)
}

/// 지금 참가자 피어(첨부를 가진 기기는 참가자뿐).
fn participant_peers(node: &Node, key: ChanKey) -> Vec<PeerId> {
    let chans = node.volatile.chans.lock();
    chans
        .get(&key)
        .filter(|rt| rt.participating())
        .map(super::ChanRt::peers)
        .unwrap_or_default()
}

/// 대기열 일꾼을 띄웁니다(`volatile::start`). 첨부마다 작업을 두지 않고 일꾼 하나가 자리가 나는 만큼만 시작합니다.
pub(super) fn start(node: &Arc<Node>) {
    let weak = Arc::downgrade(node);
    let wake = node.volatile.attach.wake.clone();
    let mut stop = node.shutdown_signal();
    node.tasks.spawn(async move {
        loop {
            let wait = {
                let Some(node) = weak.upgrade() else { break };
                if node.is_shutting_down() {
                    break;
                }
                pump(&node)
            };
            tokio::select! {
                _ = wake.notified() => {}
                _ = tokio::time::sleep(wait) => {}
                _ = stop.wait_for(|v| *v) => break,
            }
        }
    });
}

/// 빌린 내려받기 자리(놓으면 돌려줌). 둘째 값은 직접 연결 몫에서 빌렸는지.
struct Lent(Arc<Node>, bool);

impl Drop for Lent {
    fn drop(&mut self) {
        blobs::return_slot(&self.0, self.1);
    }
}

/// 대기열에서 자리가 나는 만큼 받기를 시작합니다. 다음에 다시 볼 때까지의 시간을 돌려줍니다.
fn pump(node: &Arc<Node>) -> Duration {
    let st = &node.volatile.attach;
    let now = Instant::now();
    // 메시지가 사라졌거나 채널을 나간 요청은 자리를 빌리기 전에 버립니다.
    let idle: Vec<((SpaceId, BlobId), ChanKey, EventId)> = st
        .queue
        .lock()
        .wants
        .iter()
        .filter(|(_, w)| !w.running)
        .map(|(k, w)| (*k, w.key, w.owner))
        .collect();
    let dead: Vec<(SpaceId, BlobId)> = {
        let chans = node.volatile.chans.lock();
        idle.into_iter()
            .filter(|(k, key, owner)| !alive_in(&chans, *key, owner, &k.1))
            .map(|(k, ..)| k)
            .collect()
    };
    if !dead.is_empty() {
        let mut q = st.queue.lock();
        for k in dead {
            if q.wants.get(&k).is_some_and(|w| !w.running) {
                q.wants.remove(&k);
            }
        }
    }
    let mut waiting = false;
    loop {
        let Some(k) = st.queue.lock().next_ready(now) else {
            break;
        };
        // 참가자가 모두 직접 연결이면 Tor 한도와 따로 받습니다(일반 첨부와 같음). 대기열 잠금은 놓고 봅니다.
        let chan = st.queue.lock().wants.get(&k).map(|w| w.key);
        let fast = chan.is_some_and(|key| {
            let peers = participant_peers(node, key);
            !peers.is_empty() && peers.iter().all(|p| crate::p2p::data_goes_direct(node, p))
        });
        if !blobs::borrow_slot(node, fast) {
            waiting = true;
            break;
        }
        let lent = Lent(node.clone(), fast);
        let picked = {
            let mut q = st.queue.lock();
            q.wants.get_mut(&k).filter(|w| !w.running).map(|w| {
                w.running = true;
                (w.key, w.owner, w.size, w.manual)
            })
        };
        let Some((key, owner, size, manual)) = picked else {
            continue;
        };
        node.spawn_detached(move |node| async move {
            let outcome = if alive(&node, key, &owner, &k.1) {
                attempt(&node, key, owner, k.1, size, manual).await
            } else {
                Attempt::Stop
            };
            drop(lent);
            finish(&node, key, owner, k.1, outcome);
        });
    }
    if !waiting {
        blobs::stop_waiting_for_slot(node);
    }
    let due = st
        .queue
        .lock()
        .next_due()
        .map_or(IDLE_RECHECK, |t| t.saturating_duration_since(now));
    if waiting {
        due.min(SLOT_RECHECK)
    } else {
        due.min(IDLE_RECHECK)
    }
}

/// 시도 하나가 끝났을 때: 다 받았거나 그만두면 대기열에서 빼고, 아니면 간격을 늘려 다시 넣습니다. 그만두면 받다 만
/// 조각도 지웁니다(예산·RAM을 붙잡지 않게, `blob.fetch`로 처음부터 다시 받음).
fn finish(node: &Arc<Node>, key: ChanKey, owner: EventId, blob: BlobId, outcome: Attempt) {
    let st = &node.volatile.attach;
    let give_up = st.tuning.lock().give_up();
    let now = Instant::now();
    let drop_partial = {
        let mut q = st.queue.lock();
        let k = (key.0, blob);
        match q.wants.get_mut(&k) {
            // 그사이 대기열에서 빠짐(채널을 나감 등).
            None => !matches!(outcome, Attempt::Done),
            Some(w) => {
                w.running = false;
                let stop = match outcome {
                    Attempt::Done => Some(false),
                    Attempt::Stop => Some(true),
                    Attempt::Retry { progressed } => {
                        if progressed {
                            w.progressed = true;
                            w.backoff = RETRY_START;
                            w.last_progress = now;
                        }
                        if now.saturating_duration_since(w.last_progress) >= give_up {
                            debug!(blob = %blob.short(), "휘발성 첨부를 가진 참가자가 없어 받기를 멈춤");
                            Some(true)
                        } else {
                            w.not_before = Some(now + w.backoff);
                            w.backoff = (w.backoff * 2).min(RETRY_MAX);
                            None
                        }
                    }
                };
                if stop.is_some() {
                    q.wants.remove(&k);
                }
                stop.unwrap_or(false)
            }
        }
    };
    if drop_partial {
        let store = &node.volatile.store;
        if store
            .blob_info(&key.0, &key.1, &blob)
            .is_some_and(|i| i.owner == Some(owner) && i.have < i.total)
        {
            store.remove_blob(&key.0, &key.1, &blob);
        }
    }
    kick(node);
}

enum Attempt {
    Done,
    /// 받지 않음(메시지가 지워짐, 한도·예산 밖, 맞지 않는 첨부).
    Stop,
    /// 나중에 다시(가진 참가자 없음, 진전 없음). `progressed`면 이번에 조각을 받음.
    Retry {
        progressed: bool,
    },
}

/// 참가자에게 매니페스트를 묻고 처음 온 올바른 것을 씁니다.
async fn ask_manifest(
    node: &Arc<Node>,
    space: SpaceId,
    blob: BlobId,
    peers: &[PeerId],
) -> Option<BlobManifest> {
    let mut asks: FuturesUnordered<_> = peers
        .iter()
        .map(|&peer| {
            let node = node.clone();
            async move {
                // 다른 요청과 같은 길로(P2P 전용 채널 참가자는 P2P 연결의 피어라 Tor 스웜으로는 닿지 않음).
                let r =
                    crate::p2p::request(&node, peer, YpRequest::BlobManifest { space, blob }).await;
                (peer, r)
            }
        })
        .collect();
    while let Some((peer, r)) = asks.next().await {
        if let Ok(YpResponse::Manifest(m)) = r {
            if m.id() == blob && m.check().is_ok() && !m.chunks.is_empty() {
                return Some(m);
            }
            blobs::penalize(node, peer, Penalty::BadData);
        }
    }
    None
}

async fn attempt(
    node: &Arc<Node>,
    key: ChanKey,
    owner: EventId,
    blob: BlobId,
    size: u64,
    manual: bool,
) -> Attempt {
    let (space, channel) = key;
    let store = &node.volatile.store;
    let manifest = match store.blob_manifest(&space, &channel, &blob) {
        // 받다 쉰 첨부: 쉬는 동안 줄여 둔 예산을 다시 전체로 잡습니다.
        Some(m) => match store.register_blob(&space, &channel, &owner, m.clone()) {
            Ok(_) => m,
            Err(StoreError::LimitExceeded(_)) => {
                debug!(blob = %blob.short(), "휘발성 첨부 RAM 예산이 모자라 받기를 그만둠");
                return Attempt::Stop;
            }
            Err(_) => return Attempt::Stop,
        },
        None => {
            let peers = participant_peers(node, key);
            let Some(m) = ask_manifest(node, space, blob, &peers).await else {
                return Attempt::Retry { progressed: false };
            };
            let pt = plaintext_len(&m);
            // 직접 받기는 채널 한도와 무관하게(받는 사람이 고른 것) 이 기기 설정만 지킵니다.
            let max = if manual {
                node.cfg.volatile.max_attachment_bytes
            } else {
                channel_max(node, key)
            };
            if pt > max || (!manual && pt > size.saturating_add(SIZE_SLACK)) {
                debug!(blob = %blob.short(), "휘발성 첨부가 한도나 적힌 크기보다 큼: 받지 않음");
                return Attempt::Stop;
            }
            match store.register_blob(&space, &channel, &owner, m.clone()) {
                Ok(_) => m,
                Err(StoreError::LimitExceeded(_)) => {
                    debug!(blob = %blob.short(), "휘발성 첨부 RAM 예산이 모자라 받지 않음");
                    return Attempt::Stop;
                }
                // 메시지가 그새 지워졌거나 다른 메시지에 묶인 첨부.
                Err(_) => return Attempt::Stop,
            }
        }
    };
    let r = swarm(node, key, blob, &manifest).await;
    // 다시 시도할 때까지는 예산을 붙잡지 않습니다(보낸 기기가 나가 있는 동안 다른 첨부를 막지 않게): 하나도 받지
    // 못했으면 지우고, 받다 만 것은 가진 만큼으로 줄입니다. 다음 시도에서 다시 잡습니다.
    if matches!(r, Attempt::Retry { .. }) {
        match store.blob_info(&space, &channel, &blob) {
            Some(i) if i.have == 0 => {
                store.remove_blob(&space, &channel, &blob);
            }
            Some(_) => {
                store.trim_blob_reservation(&space, &channel, &blob);
            }
            None => {}
        }
    }
    r
}

/// 저장소 비트맵(높은 비트부터) → 스웜 비트맵(낮은 비트부터).
fn swarm_bitmap(bytes: &[u8], total: u32) -> Bitmap {
    let mut bm = Bitmap::new(total);
    for i in 0..total {
        if bytes
            .get((i / 8) as usize)
            .is_some_and(|b| b & (0x80 >> (i % 8)) != 0)
        {
            bm.set(i);
        }
    }
    bm
}

enum Reply {
    Have(PeerId, Result<YpResponse, NetError>),
    Chunk(Route, u32, Result<YpResponse, NetError>),
}

fn ask_have(node: &Arc<Node>, peer: PeerId, req: YpRequest) -> BoxFuture<'static, Reply> {
    let node = node.clone();
    Box::pin(async move { Reply::Have(peer, crate::p2p::request(&node, peer, req).await) })
}

/// `ticket`: 주 연결로 보내는 조각의 피어당 자리(`blobs::chunk_ticket`). 자리는 답이 오면 돌려줍니다. 파일 회로는
/// 스웜이 따로라 자리를 쓰지 않습니다.
fn ask_chunk(
    node: &Arc<Node>,
    route: Route,
    index: u32,
    req: YpRequest,
    ticket: Option<blobs::ChunkTicket>,
) -> BoxFuture<'static, Reply> {
    let node = node.clone();
    Box::pin(async move {
        let r = filelink::send(&node, route, req).await;
        drop(ticket);
        Reply::Chunk(route, index, r)
    })
}

/// 참가자들에게 희귀 조각 우선으로 나눠 요청해 조각마다 검증해 RAM에 둡니다(`blobs::fetch`와 같은 방식). 큰 첨부를
/// Tor로 받으면 조각은 파일 회로로 나눠 받습니다(`filelink`, 회로는 메모리에만 있고 조각을 나르기만 함).
async fn swarm(node: &Arc<Node>, key: ChanKey, blob: BlobId, manifest: &BlobManifest) -> Attempt {
    let (space, channel) = key;
    let store = &node.volatile.store;
    let total = manifest.chunks.len() as u32;
    let local = match store.blob_have(&space, &channel, &blob) {
        Some((bm, t)) if t == total => swarm_bitmap(&bm, total),
        _ => return Attempt::Stop,
    };
    if local.is_full() {
        return Attempt::Done;
    }
    let mut cfg = SwarmConfig {
        request_timeout_ms: node.cfg.net.request_timeout.as_millis() as u64 + 5_000,
        seed: rand::random(),
        ..SwarmConfig::default()
    };
    // 일반 첨부와 같은 한도: 시간 제한은 Tor 스웜에 맞추고, 동시 요청 수는 참가자의 연결 종류로 정합니다
    // (`blobs::chunk_window`, Tor면 같은 회로의 메시지가 먼저 가게 작게, 직접 연결이면 잰 속도에 맞춤).
    if let Some(t) = crate::tor::tor_download_limits(node) {
        cfg.request_timeout_ms = cfg
            .request_timeout_ms
            .max(t.request_timeout.as_millis() as u64);
    }
    let file_link = filelink::wanted(node, manifest.chunks.len());
    let Ok(mut sched) = ChunkScheduler::<Route>::with_local(local, cfg) else {
        return Attempt::Stop;
    };
    let mut routes = filelink::Routes::new(total);
    let mut seeders: HashSet<PeerId> = HashSet::new();
    let tuning = *node.volatile.attach.tuning.lock();
    let stall_ms = tuning.stall_ms();
    let mut pending: FuturesUnordered<BoxFuture<'static, Reply>> = FuturesUnordered::new();
    let mut asking: HashSet<PeerId> = HashSet::new();
    let mut next_query = 0u64;
    let mut last_progress = now_ms();
    let mut throttled_until = 0u64;
    let mut progressed = false;
    let mut pacer = blobs::Pacer::new(manifest.chunk_size);
    let mut members: Vec<PeerId> = Vec::new();
    let mut rewindow = true;
    loop {
        if node.is_shutting_down() {
            return Attempt::Stop;
        }
        // 메시지가 지워지면 저장소가 첨부도 지웁니다. 그때 받기를 멈춥니다.
        if store.blob_info(&space, &channel, &blob).is_none() {
            return Attempt::Stop;
        }
        let now = now_ms();
        sched.check_timeouts(now);
        if now >= next_query {
            members = participant_peers(node, key);
            rewindow = true;
            if file_link && !pacer.is_direct() {
                filelink::prepare(node, &filelink::targets(node, &seeders));
            }
            for peer in members.iter().copied() {
                if asking.insert(peer) {
                    pending.push(ask_have(node, peer, YpRequest::BlobHave { space, blob }));
                }
            }
            next_query = now + REQUERY_MS;
        }
        if std::mem::take(&mut rewindow) {
            // 일반 첨부와 같게, 가진 참가자가 알려지면 그들의 연결로 동시 요청 수를 정합니다(첨부가 없는 참가자 하나가
            // Tor로만 닿아도 직접 연결 받기가 Tor 한도로 떨어지지 않게).
            let peers: Vec<PeerId> = if seeders.is_empty() {
                members.clone()
            } else {
                seeders.iter().copied().collect()
            };
            pacer.update(node, &peers);
        }
        routes.steer(node, &mut pacer, &mut sched, file_link, now);
        let asked = sched.progress().0 + sched.in_flight();
        let holding = tuning.hold_at.is_some_and(|n| asked >= n);
        if !holding && now >= throttled_until {
            let mut reqs = sched.next_requests(now);
            // 시험용 멈춤: 정확히 그 수까지만 요청하고 나머지는 바쁜 것으로 돌립니다.
            if let Some(n) = tuning.hold_at {
                let room = n.saturating_sub(asked) as usize;
                for (route, i) in reqs.split_off(room.min(reqs.len())) {
                    let busy = RequestError::Busy {
                        retry_after_ms: 1_000,
                    };
                    sched.on_error(&route, i, busy, now);
                }
            }
            // 일반 첨부와 같은 피어당 조각 상한(`blobs::chunk_ticket`, 주 연결만): 차면 감점 없이 잠깐 쉽니다.
            let reqs: Vec<_> = reqs
                .into_iter()
                .filter_map(|(route, i)| {
                    if route.1 != filelink::MAIN {
                        return Some((route, i, None));
                    }
                    match blobs::chunk_ticket(node, route.0) {
                        Some(t) => Some((route, i, Some(t))),
                        None => {
                            let busy = RequestError::Busy {
                                retry_after_ms: blobs::LOCAL_BUSY_MS,
                            };
                            sched.on_error(&route, i, busy, now);
                            None
                        }
                    }
                })
                .collect();
            if !reqs.is_empty() {
                // 일반 첨부와 같은 수신 상한(`download_limit_bps`, 범위별 받기 제한을 함께 지킴).
                let (send, over) =
                    blobs::admit_chunks(node, &space, Some(&channel), now, reqs, |i| {
                        manifest.chunk_len(i)
                    });
                for (route, i, ms) in over {
                    sched.on_error(&route, i, RequestError::Busy { retry_after_ms: ms }, now);
                    throttled_until = throttled_until.max(now + u64::from(ms));
                }
                for (route, i, t) in send {
                    let req = YpRequest::BlobChunk {
                        space,
                        blob,
                        index: i,
                    };
                    pending.push(ask_chunk(node, route, i, req, t));
                }
            }
        }
        let idle = sched.in_flight() == 0;
        if idle && now.saturating_sub(last_progress) >= stall_ms {
            return Attempt::Retry { progressed };
        }
        let mut wake_at = next_query;
        if idle {
            wake_at = wake_at.min(last_progress + stall_ms);
        }
        if let Some(d) = sched.next_deadline() {
            wake_at = wake_at.min(d);
        }
        // 바쁘다고 답한 피어가 쉬는 동안·마지막 단계에서 보낼 요청이 없어도, 보낼 수 있게 되면 바로 이어 요청합니다.
        if let Some(d) = sched.next_wake(now) {
            wake_at = wake_at.min(d);
        }
        if throttled_until > now {
            wake_at = wake_at.min(throttled_until);
        }
        let sleep = tokio::time::sleep(Duration::from_millis(
            wake_at.saturating_sub(now).clamp(5, REQUERY_MS),
        ));
        let reply = tokio::select! {
            r = pending.next(), if !pending.is_empty() => r,
            _ = sleep => None,
        };
        let Some(reply) = reply else { continue };
        let now = now_ms();
        match reply {
            Reply::Have(peer, r) => {
                asking.remove(&peer);
                match r {
                    Ok(YpResponse::Have { bitmap, total: t }) if t == total => {
                        if let Ok(bm) = Bitmap::from_wire(&bitmap, total) {
                            rewindow |= if bm.count_ones() > 0 {
                                seeders.insert(peer)
                            } else {
                                seeders.remove(&peer)
                            };
                            routes.set_have(&mut sched, peer, bm);
                        }
                    }
                    Ok(YpResponse::Error { .. }) => {
                        rewindow |= seeders.remove(&peer);
                        routes.set_have(&mut sched, peer, Bitmap::new(total));
                    }
                    _ => {}
                }
                if file_link && !pacer.is_direct() {
                    filelink::prepare(node, &filelink::targets(node, &seeders));
                }
            }
            Reply::Chunk(route, i, r) => match r {
                Ok(YpResponse::Chunk { data }) => {
                    let peer = route.0;
                    pacer.record(now, data.len());
                    let verified = manifest.verify_chunk(i, &data);
                    let fresh = match sched.on_chunk(&route, i, verified) {
                        ChunkOutcome::Accepted { .. } => true,
                        ChunkOutcome::BadChunk { .. } => {
                            blobs::penalize(node, peer, Penalty::BadData);
                            debug!(blob = %blob.short(), %peer, index = i, "검증 실패 조각: 다른 참가자에게 다시 받음");
                            false
                        }
                        // 시간 초과로 버린 요청의 늦은 답: 검증됐으면 그대로 씁니다.
                        ChunkOutcome::Unexpected if verified && !sched.local_bitmap().get(i) => {
                            let _ = sched.mark_have(i);
                            true
                        }
                        ChunkOutcome::Duplicate | ChunkOutcome::Unexpected => false,
                    };
                    if fresh {
                        match store.write_blob_chunk(&space, &channel, &blob, i, &data) {
                            Ok(_) => {}
                            Err(StoreError::NotFound) => return Attempt::Stop,
                            Err(e) => {
                                debug!(blob = %blob.short(), error = %e, "휘발성 조각을 RAM에 두지 못함");
                                return Attempt::Retry { progressed };
                            }
                        }
                        last_progress = now;
                        progressed = true;
                        let (have, total) = sched.progress();
                        node.notify.publish(Notification::BlobProgress {
                            space,
                            blob,
                            have,
                            total,
                        });
                    }
                }
                Ok(YpResponse::Busy { retry_after_ms }) => {
                    sched.on_error(&route, i, RequestError::Busy { retry_after_ms }, now);
                }
                // 이 노드의 망 계층이 그 피어에게 보내 둔 요청 상한을 채움: 상대 잘못이 아님(`blobs::fetch`와 같음).
                Err(NetError::Busy) => {
                    let busy = RequestError::Busy {
                        retry_after_ms: blobs::LOCAL_BUSY_MS,
                    };
                    sched.on_error(&route, i, busy, now);
                }
                Ok(YpResponse::Error {
                    code: WireErrorCode::NotFound,
                    ..
                }) => {
                    sched.on_error(&route, i, RequestError::NotFound, now);
                }
                Ok(YpResponse::Error { .. }) => {
                    sched.on_error(&route, i, RequestError::Denied, now);
                }
                _ => {
                    sched.on_error(&route, i, RequestError::Failed, now);
                }
            },
        }
        if sched.is_complete() {
            return Attempt::Done;
        }
    }
}

// ───────────── 내주기 ─────────────

#[derive(Clone, Copy)]
enum Ask {
    Manifest,
    Have,
    Chunk(u32),
}

/// 들어온 블롭 요청 중 RAM에 있는 휘발성 첨부면 답합니다. 아니면 `None`(일반 블롭 경로가 처리).
pub(crate) fn serve(node: &Arc<Node>, peer: PeerId, req: &YpRequest) -> Option<YpResponse> {
    let (space, blob, ask) = match *req {
        YpRequest::BlobManifest { space, blob } => (space, blob, Ask::Manifest),
        YpRequest::BlobHave { space, blob } => (space, blob, Ask::Have),
        YpRequest::BlobChunk { space, blob, index } => (space, blob, Ask::Chunk(index)),
        _ => return None,
    };
    let channel = node.volatile.store.find_blob(&space, &blob)?;
    // 디스크에도 있는 블롭(일반 첨부의 ID를 베낀 휘발성 메시지)은 일반 경로가 답합니다(일반 첨부를 가리지 않게).
    if node.store.blob_status(&blob).ok().flatten().is_some() {
        return None;
    }
    Some(serve_in(node, peer, (space, channel), blob, ask))
}

fn serve_in(node: &Arc<Node>, peer: PeerId, key: ChanKey, blob: BlobId, ask: Ask) -> YpResponse {
    let (space, channel) = key;
    // 참가자가 아니거나 메시지가 없으면 없는 블롭과 같은 답입니다(휘발성 첨부가 있는지 드러내지 않게).
    let not_found = || YpResponse::error(WireErrorCode::NotFound, "없는 블롭");
    let Some((user, device)) = node.peers.identity_of(&peer) else {
        return YpResponse::error(WireErrorCode::Unauthenticated, "인증 전");
    };
    let serving = crate::sync_service::serving(node, device);
    let in_flight = serving.others;
    let Some(state) = node.space_state(&space) else {
        return YpResponse::error(WireErrorCode::NotFound, "모르는 공간");
    };
    if !member_device(&state, &user, &device) {
        return YpResponse::error(WireErrorCode::NotMember, "멤버가 아님");
    }
    // 일반 첨부와 같게: P2P 전용 채널의 첨부는 함께 참가한 P2P 피어에게만, 나머지는 Tor 피어(또는 일반 내용을
    // 나르는 신뢰 연결)에게만 줍니다.
    if !crate::p2p::route_ok(node, &peer, &space, &state, &StreamId::Channel(channel)) {
        return not_found();
    }
    let Some(settings) = state
        .channels
        .get(&channel)
        .filter(|c| c.is_volatile())
        .cloned()
    else {
        return not_found();
    };
    let Ok(me) = node.me() else {
        return not_found();
    };
    let store = &node.volatile.store;
    let Some(owner) = store
        .blob_info(&space, &channel, &blob)
        .and_then(|i| i.owner)
    else {
        return not_found();
    };
    let mut fx = Effects::default();
    let found = {
        let mut chans = node.volatile.chans.lock();
        match chans
            .get_mut(&key)
            .filter(|rt| rt.participating() && rt.mode == settings.join_mode)
        {
            Some(rt) => {
                let present = super::author_present(
                    node,
                    &state,
                    key,
                    &settings,
                    rt,
                    me.device_id(),
                    &user,
                    &device,
                    &mut fx,
                );
                let lamport = rt.ready_message(&owner).map(|m| m.lamport);
                let data_type = rt
                    .attach
                    .get(&blob)
                    .filter(|a| a.owner == owner)
                    .map(|a| a.data_type);
                if present {
                    lamport.zip(data_type)
                } else {
                    None
                }
            }
            None => None,
        }
    };
    fx.run(node);
    let Some((lamport, data_type)) = found else {
        return not_found();
    };
    let Some(manifest) = store.blob_manifest(&space, &channel, &blob) else {
        return not_found();
    };
    let chunks = manifest.chunks.len() as u64;
    let bytes = match ask {
        Ask::Manifest => 64 + 32 * chunks,
        Ask::Have => 64 + chunks.div_ceil(8),
        Ask::Chunk(i) if u64::from(i) < chunks => manifest.chunk_len(i),
        Ask::Chunk(_) => return YpResponse::error(WireErrorCode::BadRequest, "조각 번호 범위 밖"),
    };
    // 차단한 사용자·시더 설정 밖·대역폭·동시 전송은 일반 첨부와 같은 검사입니다.
    let req = SeedRequest {
        state: &state,
        requester: user,
        requester_device: device,
        lane: Lane::Message,
        channel: Some(channel),
        data_type,
        event: Some(owner),
        event_lamport: Some(lamport),
        requester_since: sync_service::requester_since(node, &space, &user),
        // 참가 중인 채널은 연 채널입니다.
        channel_opened: true,
        bytes,
        in_flight,
        now_ms: now_ms(),
    };
    if let Err(r) = sync_service::check(node, &space, &req) {
        return r;
    }
    match ask {
        Ask::Manifest => YpResponse::Manifest(manifest),
        Ask::Have => match store.blob_have(&space, &channel, &blob) {
            Some((bm, total)) => YpResponse::Have {
                bitmap: swarm_bitmap(&bm, total).to_wire().0,
                total,
            },
            None => not_found(),
        },
        Ask::Chunk(i) => match store.read_blob_chunk(&space, &channel, &blob, i) {
            Some(c) => {
                // 받는 사람별 속도(메모리에만).
                crate::transfer::record_upload(
                    node,
                    &space,
                    Some(&channel),
                    &blob,
                    &user,
                    c.len() as u64,
                );
                YpResponse::Chunk { data: c.to_vec() }
            }
            None => YpResponse::error(WireErrorCode::NotFound, "없는 조각"),
        },
    }
}

// ───────────── API(`blob.*`) ─────────────

/// API가 다루는 휘발성 첨부(참가 중인 채널에서 보기에 있는 메시지에 실린 것).
pub(crate) struct Found {
    key: ChanKey,
    att: AttRef,
    blob: BlobId,
}

/// 이 공간의 휘발성 첨부면 그 채널과 실은 메시지. 아니면 `None`(일반 블롭 경로).
///
/// 남의 블롭 ID를 베낀 휘발성 메시지가 일반 첨부·이모지를 가리지 못하게, `key_from`이 있으면 그 메시지가 주인일
/// 때만, 없으면(`blob.status`·`blob.fetch`) 이 공간의 디스크 쪽 쓰임이 없을 때만 휘발성으로 봅니다.
pub(crate) fn find(
    node: &Node,
    space: &SpaceId,
    blob: &BlobId,
    key_from: Option<&EventId>,
) -> Option<Found> {
    let found = {
        let chans = node.volatile.chans.lock();
        chans
            .iter()
            .filter(|((s, _), rt)| s == space && rt.participating())
            .find_map(|(k, rt)| {
                rt.attach
                    .get(blob)
                    .filter(|a| rt.ready_message(&a.owner).is_some())
                    .map(|att| Found {
                        key: *k,
                        att,
                        blob: *blob,
                    })
            })?
    };
    match key_from {
        Some(k) => (*k == found.att.owner).then_some(found),
        None => {
            let state = node.space_state(space)?;
            blobs::uses_in_space(node, space, &state, blob)
                .is_empty()
                .then_some(found)
        }
    }
}

/// 휘발성 읽기 허용(자동화 토큰)과 채널 권한.
fn check_access(node: &Node, ctx: &CallCtx, key: ChanKey) -> Result<(), RpcError> {
    if ctx.is_automation() && !node.volatile.api_read.lock().contains(&key) {
        return Err(RpcError::volatile_read_denied());
    }
    super::api_channel(node, &key.0, &key.1)?;
    Ok(())
}

/// 지금 참가자 중 이 첨부를 (일부라도) 가진 기기 수.
async fn seeders_online(node: &Arc<Node>, key: ChanKey, blob: BlobId) -> u32 {
    let probes = participant_peers(node, key).into_iter().map(|peer| {
        let node = node.clone();
        let req = YpRequest::BlobHave { space: key.0, blob };
        async move {
            match tokio::time::timeout(PROBE, crate::p2p::request(&node, peer, req)).await {
                Ok(Ok(YpResponse::Have { bitmap, .. })) => bitmap.iter().any(|b| *b != 0),
                Ok(Ok(YpResponse::Busy { .. })) => true,
                _ => false,
            }
        }
    });
    futures::future::join_all(probes)
        .await
        .into_iter()
        .filter(|x| *x)
        .count() as u32
}

pub(crate) async fn status(node: &Arc<Node>, ctx: &CallCtx, f: Found) -> R<m::BlobStatus> {
    check_access(node, ctx, f.key)?;
    let store = &node.volatile.store;
    let (space, channel) = f.key;
    let (have_chunks, total_chunks) = store
        .blob_info(&space, &channel, &f.blob)
        .map_or((0, 0), |i| (i.have, i.total));
    let size = store
        .blob_manifest(&space, &channel, &f.blob)
        .map_or(f.att.size, |m| content_size(&m, f.att.size));
    let seeders_online = seeders_online(node, f.key, f.blob).await;
    Ok(BlobStatusView {
        have_chunks,
        total_chunks,
        seeders_online,
        size,
        downloading: node
            .volatile
            .attach
            .queue
            .lock()
            .wants
            .contains_key(&(space, f.blob)),
        // 휘발성 첨부는 RAM에만 있어 디스크 저장 공간 정리·고정과 무관합니다.
        pinned: false,
        kept: false,
        volatile: true,
    })
}

/// 직접 받기. 자동 한도와 무관하게 받되, 첨부 크기 한도와 RAM 예산은 넘지 않습니다(넘으면 바로 오류).
pub(crate) fn fetch(node: &Arc<Node>, ctx: &CallCtx, f: Found) -> R<m::BlobFetch> {
    check_access(node, ctx, f.key)?;
    let max = node.cfg.volatile.max_attachment_bytes;
    if f.att.size > max {
        return Err(too_big(max));
    }
    let store = &node.volatile.store;
    let (space, channel) = f.key;
    match store.blob_info(&space, &channel, &f.blob) {
        Some(i) if i.have == i.total => return Ok(Empty),
        Some(_) => {}
        None => {
            let need = ciphertext_estimate(f.att.size);
            let room = store.blob_room();
            if need > room {
                return Err(no_room(need, room));
            }
        }
    }
    if enqueue(node, f.key, f.att.owner, f.blob, f.att.size, true) == Admit::Full {
        return Err(RpcError::conflict(
            "받는 중인 휘발성 첨부가 너무 많습니다. 잠시 뒤 다시 시도하세요",
        ));
    }
    kick(node);
    Ok(Empty)
}

/// 첨부 키: 실은 메시지 본문(세션 키 암호문)을 풀어 꺼냅니다. 본문의 다른 내용은 바로 덮어씁니다.
fn attachment_key(node: &Node, f: &Found) -> Option<Secret32> {
    let generation = {
        let chans = node.volatile.chans.lock();
        chans
            .get(&f.key)?
            .ready_message(&f.att.owner)
            .map(|m| m.generation)?
    };
    let (mut body, _) = super::open_body(&node.volatile.store, f.key, generation, &f.att.owner)?;
    let key = match &body {
        EventBody::Message(mb) => mb
            .attachments
            .iter()
            .find(|a| a.blob == f.blob)
            .map(|a| a.key.clone()),
        _ => None,
    };
    super::scrub(&mut body);
    key
}

/// 보일 내용 크기. 압축하지 않은 블롭이면 평문에서 머리글을 뺀 것, 압축본이면(평문이 적힌 크기보다 작음) 적힌 크기.
fn content_size(m: &BlobManifest, declared: u64) -> u64 {
    blobs::content_size(m, declared)
}

fn open_blob(node: &Node, f: &Found) -> Result<(Secret32, BlobManifest), RpcError> {
    let key = attachment_key(node, f)
        .ok_or_else(|| RpcError::not_found("이 블롭의 키를 찾을 수 없습니다"))?;
    let manifest = node
        .volatile
        .store
        .blob_manifest(&f.key.0, &f.key.1, &f.blob)
        .ok_or_else(|| RpcError::not_found("아직 받지 않은 블롭"))?;
    Ok((key, manifest))
}

fn decrypt_chunk(
    node: &Node,
    f: &Found,
    key: &Secret32,
    manifest: &BlobManifest,
    index: u32,
) -> Result<Zeroizing<Vec<u8>>, RpcError> {
    let chunk = node
        .volatile
        .store
        .read_blob_chunk(&f.key.0, &f.key.1, &f.blob, index)
        .ok_or_else(|| RpcError::conflict("그 조각을 아직 받지 않았습니다(만료됐을 수 있음)"))?;
    decrypt_blob_chunk(key, manifest, index, &chunk)
        .map(Zeroizing::new)
        .map_err(|_| RpcError::internal("블롭을 복호화할 수 없습니다"))
}

/// `blob.read`: RAM의 조각을 풀어 돌려줍니다(일반 첨부와 같은 인자·한도).
pub(crate) fn read(
    node: &Arc<Node>,
    ctx: &CallCtx,
    f: Found,
    p: BlobReadParams,
) -> Result<RpcOutput, RpcError> {
    check_access(node, ctx, f.key)?;
    let (key, manifest) = open_blob(node, &f)?;
    let data: Vec<u8> = crate::api::blob::read_content(
        &manifest,
        |i| decrypt_chunk(node, &f, &key, &manifest, i),
        p.index,
        p.offset,
        p.len,
    )?
    .to_vec();
    if p.binary == Some(true) {
        Ok(RpcOutput::Binary(bytes::Bytes::from(data)))
    } else {
        let mut data = data;
        let out = RpcOutput::json(&BlobReadResult {
            data_base64: Some(base64::engine::general_purpose::STANDARD.encode(&data)),
        });
        data.zeroize();
        out
    }
}

/// `blob.save`: 휘발성 첨부는 사람 토큰이 `allow_volatile`로 확인했을 때만 파일로 씁니다(decisions.md). 저장한
/// 파일은 메시지가 사라져도 남으므로, 데스크톱 앱은 경고를 보이고 확인을 받은 뒤에만 이 인자를 켭니다.
pub(crate) async fn save(
    node: &Arc<Node>,
    ctx: &CallCtx,
    f: Found,
    p: BlobSaveParams,
) -> R<m::BlobSave> {
    if ctx.is_automation() {
        return Err(RpcError::out_of_scope(
            "휘발성 첨부는 사람만 저장할 수 있습니다",
        ));
    }
    check_access(node, ctx, f.key)?;
    if !p.allow_volatile {
        return Err(RpcError::conflict(
            "휘발성 첨부입니다. 저장한 파일은 메시지가 사라져도 남습니다. 확인했으면 allow_volatile을 켜고 다시 요청하세요",
        ));
    }
    let path = PathBuf::from(&p.path);
    if !path.is_absolute() {
        return Err(RpcError::invalid_input("저장 경로는 절대 경로여야 합니다"));
    }
    let (key, manifest) = open_blob(node, &f)?;
    let complete = node
        .volatile
        .store
        .blob_info(&f.key.0, &f.key.1, &f.blob)
        .is_some_and(|i| i.have == i.total);
    if !complete {
        return Err(RpcError::conflict("아직 다 받지 않은 블롭입니다"));
    }
    let node2 = node.clone();
    tokio::task::spawn_blocking(move || {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::AlreadyExists => {
                    RpcError::conflict("이미 있는 파일은 덮어쓰지 않습니다")
                }
                _ => RpcError::invalid_input("저장할 파일을 만들 수 없습니다"),
            })?;
        let written = (|| -> Result<(), RpcError> {
            // 쓰는 사이 만료되면 조각이 없어 실패하고, 쓰다 만 파일은 지웁니다.
            let mut reader =
                BlobReader::open(&manifest, |i| decrypt_chunk(&node2, &f, &key, &manifest, i))
                    .map_err(crate::api::blob::read_err)?;
            reader
                .for_each(|piece| {
                    file.write_all(piece)
                        .map_err(|_| RpcError::internal("파일에 쓸 수 없습니다"))
                })
                .map_err(crate::api::blob::read_err)?;
            file.sync_all()
                .map_err(|_| RpcError::internal("파일에 쓸 수 없습니다"))?;
            crate::api::blob::mark_from_internet(&path);
            Ok(())
        })();
        if written.is_err() {
            drop(file);
            let _ = std::fs::remove_file(&path);
        }
        written
    })
    .await
    .map_err(|_| RpcError::internal("저장 작업이 멈췄습니다"))??;
    Ok(Empty)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_keeps_first_owner() {
        let mut ix = Index::default();
        let (a, b) = (EventId::from_bytes([1; 32]), EventId::from_bytes([2; 32]));
        let blob = BlobId([7; 32]);
        ix.add(a, blob, 10, DataType::File);
        ix.add(b, blob, 99, DataType::Image);
        assert_eq!(ix.get(&blob).map(|r| (r.owner, r.size)), Some((a, 10)));
        // 베낀 메시지가 지워져도 원래 주인은 그대로.
        ix.remove_owner(&b);
        assert!(ix.get(&blob).is_some());
        ix.remove_owner(&a);
        assert!(ix.get(&blob).is_none());
    }

    fn queue_key(n: u16) -> ((SpaceId, BlobId), ChanKey, EventId) {
        use yangpa_core::ids::ChannelId;
        let space = SpaceId::from_bytes([1; SpaceId::LEN]);
        let key = (space, ChannelId::from_bytes([2; ChannelId::LEN]));
        let mut id = [0u8; 32];
        id[..2].copy_from_slice(&n.to_be_bytes());
        ((space, BlobId(id)), key, EventId::from_bytes(id))
    }

    #[test]
    fn queue_is_bounded_and_evicts_oldest_untouched_auto_want() {
        let now = Instant::now();
        let mut q = Queue::default();
        for n in 0..MAX_WANTS as u16 {
            let (k, key, owner) = queue_key(n);
            assert_eq!(q.admit(k, key, owner, 10, false, now), Admit::Added);
        }
        // 가장 오래된 것은 받는 중, 그다음은 조각을 받은 적 있음: 밀려나지 않음.
        let (k0, ..) = queue_key(0);
        let (k1, ..) = queue_key(1);
        q.wants.get_mut(&k0).unwrap().running = true;
        q.wants.get_mut(&k1).unwrap().progressed = true;
        let (k_new, key, owner) = queue_key(1000);
        assert_eq!(q.admit(k_new, key, owner, 10, false, now), Admit::Added);
        assert_eq!(q.wants.len(), MAX_WANTS);
        let (k2, ..) = queue_key(2);
        assert!(
            !q.wants.contains_key(&k2),
            "받은 적 없는 가장 오래된 자동 요청이 밀려남"
        );
        assert!(q.wants.contains_key(&k0) && q.wants.contains_key(&k1));
        // 같은 첨부를 다시 넣으면 늘지 않고, 직접 요청이면 바로 다시 시도.
        let (k3, key3, owner3) = queue_key(3);
        q.wants.get_mut(&k3).unwrap().not_before = Some(now + Duration::from_secs(60));
        assert_eq!(q.admit(k3, key3, owner3, 10, true, now), Admit::Updated);
        assert_eq!(q.wants.len(), MAX_WANTS);
        assert_eq!(q.next_ready(now), Some(k3), "직접 요청 먼저");
        // 밀어낼 것이 없으면(모두 직접 요청이거나 받는 중이거나 받은 적 있음) 가득 참.
        for w in q.wants.values_mut() {
            w.manual = true;
        }
        let (k_full, key, owner) = queue_key(1001);
        assert_eq!(q.admit(k_full, key, owner, 10, false, now), Admit::Full);
    }

    #[test]
    fn queue_order_and_due_times() {
        let now = Instant::now();
        let mut q = Queue::default();
        let (a, key, oa) = queue_key(1);
        let (b, _, ob) = queue_key(2);
        q.admit(a, key, oa, 1, false, now);
        q.admit(b, key, ob, 1, false, now);
        assert_eq!(q.next_ready(now), Some(a), "넣은 순서");
        q.wants.get_mut(&a).unwrap().running = true;
        assert_eq!(q.next_ready(now), Some(b));
        let later = now + Duration::from_secs(3);
        q.wants.get_mut(&b).unwrap().not_before = Some(later);
        assert_eq!(q.next_ready(now), None, "받는 중이거나 재시도 전");
        assert_eq!(q.next_due(), Some(later));
        assert_eq!(q.next_ready(later), Some(b));
    }

    #[test]
    fn bitmap_order_and_estimate() {
        let bm = swarm_bitmap(&[0x80, 0x40], 12);
        assert_eq!(bm.iter_ones().collect::<Vec<_>>(), vec![0, 9]);
        // 가장 작은 조각(16KiB)의 태그 비용(약 0.1%)보다 넉넉함.
        let size = 100 * 1024 * 1024u64;
        let worst = size + size.div_ceil(16 * 1024 - 16) * 16;
        assert!(ciphertext_estimate(size) >= worst);
    }
}
