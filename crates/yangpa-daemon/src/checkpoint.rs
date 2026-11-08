//! 상태 체크포인트(protocol.md "체크포인트", docs/daemon.md "체크포인트").
//!
//! - 내기([`tick`]): 소유자 기기(위임이 있으면 위임받은 기기)가 문턱을 넘고 머리가 가라앉으면, 해소기 초안으로 본문을
//!   만들어 서명하고 본문의 머리를 그대로 부모로 둔 상태 이벤트로 냅니다. 오래 가라앉지 않으면 지금 머리 전부를 부모로
//!   둔 체크포인트가 머리를 합칩니다(따로 합치는 이벤트를 내지 않음).
//! - 받은 뒤([`on_accepted`]): 바탕 직후 상태로 스냅숏 블롭을 결정론적으로 다시 만들어, 본문의 블롭 ID와 같으면 저장해
//!   시드하고(최근 [`KEEP_SNAPSHOTS`]개만), 다르면 `CheckpointDispute`를 한 번 냅니다. 사슬 본문과 바탕 기록을 둡니다.
//! - 재시작([`restore`]): 바탕 기록이 있으면 스냅숏으로 해소기를 만들고 경계 위 이벤트만 다시 넣습니다.
//! - 가입([`prepare_join`], [`try_install`], [`confirm`]): 받아 준 노드의 사슬을 초대 링크의 고정에서 확인하고 스냅숏을
//!   받아 두었다가, 상태 키가 오면 그 체크포인트로 해소기를 세웁니다. 들어온 뒤 다른 멤버와 머리를 견줍니다.
//! - 답하기: `GetCheckpoint`([`handle_get_checkpoint`]), `GetDeviceKeys`([`handle_get_device_keys`]).
//!
//! 스냅숏은 상태 키로 암호화한 블롭이라 로그에 내용·키를 남기지 않습니다.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine;
use parking_lot::{Mutex, RwLock};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};
use yangpa_core::blob::{BlobManifest, DEFAULT_CHUNK_SIZE};
use yangpa_core::body::{EventBody, Secret32};
use yangpa_core::checkpoint::{
    CheckpointBody, CheckpointPin, DelegationCert, KeyHead, MAX_CHAIN_PER_REPLY, MAX_SNAPSHOT_LEN,
    MAX_VALID_STATE_KEYS, RESOLVER_VERSION, SignedCheckpoint,
};
use yangpa_core::device::MemberDevices;
use yangpa_core::event::{EventKind, KeyRef, Lane, SignedEvent, StreamId};
use yangpa_core::ids::{BlobId, CheckpointId, EventId, SpaceId, StateKeyId, UserId};
use yangpa_core::model::{SecurityLevel, SpaceKind, SpaceState};
use yangpa_core::wire::{
    CheckpointReply, DeviceKeysItem, MAX_DEVICE_KEYS_REQUEST, WireErrorCode, YpRequest, YpResponse,
};
use yangpa_crypto::decrypt_body;
use yangpa_net::PeerId;
use yangpa_rpc::api::CheckpointAlertKind;
use yangpa_store::PersistentBlob;
use yangpa_sync::checkpoint::{
    DeviceProofs, DeviceTree, HeadAgreement, accept_device_keys, compare_heads, validate_chain,
};
use yangpa_sync::{CheckpointBase, DecodedEvent, Resolver};

use crate::author::{self, KeyChoice};
use crate::error::{Error, Result};
use crate::ingest::{self, Source};
use crate::node::Node;
use crate::{keys, spaces, sync_service};

/// 바탕 기록(`checkpoint.base.<공간>`).
const BASE_PREFIX: &str = "checkpoint.base.";
/// 사슬 본문(`checkpoint.chain.<공간>.<순번 20자리>`): 사슬을 묻는 가입자에게 답할 때.
const CHAIN_PREFIX: &str = "checkpoint.chain.";
/// 가입 중 받아 둔 체크포인트(`checkpoint.stash.<공간>`).
const STASH_PREFIX: &str = "checkpoint.stash.";
/// 체크포인트로 들어와 아직 다른 출처와 머리를 견주지 못한 공간(`checkpoint.unconfirmed.<공간>`): 재시작해도 이어
/// 견주게.
const UNCONFIRMED_PREFIX: &str = "checkpoint.unconfirmed.";
/// 다시 서명한 내 늦은 이벤트(`checkpoint.resigned.<공간>`): 재시작 뒤 같은 이벤트를 또 다시 내지 않게.
const RESIGNED_PREFIX: &str = "checkpoint.resigned.";
/// 다시 서명한 이벤트를 기억하는 수(공간마다, 오래된 것부터 버림).
const KEEP_RESIGNED: usize = 1024;
/// 이보다 오래 전에 쓴 늦은 이벤트는 다시 서명하지 않습니다(그새 같은 대상이 여러 번 바뀌었을 수 있어, 옛 변경을 지금
/// 되살리지 않게).
const RESIGN_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 3600);
/// 남겨 두는 사슬 본문 수(초대 링크의 고정이 이보다 오래되면 그 링크로는 사슬을 잇지 못함).
const KEEP_CHAIN: usize = 128;
/// 남겨 두는 스냅숏 블롭 수(지금 것과 그 앞 것: 새 체크포인트가 퍼지는 동안 앞 것으로 들어오는 가입자도 받게).
pub(crate) const KEEP_SNAPSHOTS: usize = 2;
/// 스냅숏 블롭 키 파생의 소금.
const SNAPSHOT_SALT: &[u8] = b"yangpa/v3/checkpoint";
/// 가입 뒤 한 번에 머리를 물어볼 피어 수.
const COMPARE_MEMBERS: usize = 4;
/// 스냅숏 조각을 한 번에 묻는 수.
const SNAPSHOT_FETCH_PARALLEL: usize = 4;
/// 받아 둔 체크포인트의 스냅숏을 이만큼 받지 못하면 버리고 처음부터 따라잡습니다(받아 준 노드가 사라지거나 내주지 않아도
/// 가입이 멈추지 않게. 전체 기록은 해소기가 시작부터 확인함).
const STASH_GIVE_UP: Duration = Duration::from_secs(10 * 60);

/// 저장한 바탕 기록.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SavedBase {
    pub pin: CheckpointPin,
    pub event: EventId,
    /// 경계 F.
    pub floor: u64,
    pub snapshot: BlobId,
    /// 체크포인트 이벤트를 암호화한 상태 키(스냅숏 블롭 키의 재료).
    pub key: StateKeyId,
    /// 남겨 둔 스냅숏 블롭(새 것이 앞).
    pub kept: Vec<BlobId>,
}

/// 가입 중 받아 둔 체크포인트(설치 전).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Stash {
    /// 확인한 사슬(마지막이 머리).
    pub chain: Vec<SignedCheckpoint>,
    /// 머리 체크포인트 이벤트(상태 키로 암호화된 그대로).
    pub event: SignedEvent,
    pub manifest: BlobManifest,
    /// 같은 머리를 알려 준 출처(피어 ID 바이트).
    pub sources: Vec<Vec<u8>>,
}

impl Stash {
    fn head(&self) -> &SignedCheckpoint {
        self.chain.last().expect("확인한 사슬은 비지 않음")
    }

    fn floor(&self) -> u64 {
        self.head().body.frontier_lamport
    }
}

/// 공간마다 머리를 지켜본 기록(체크포인트를 낼 때인지).
#[derive(Clone, Debug)]
struct Watch {
    heads: Vec<EventId>,
    since: Instant,
    /// 문턱을 넘은 뒤 아직 내지 못한 동안의 시작.
    due_since: Option<Instant>,
}

/// 노드 수준 상태.
#[derive(Default)]
pub(crate) struct CheckpointState {
    watch: Mutex<HashMap<SpaceId, Watch>>,
    /// 내는 중·받은 뒤 처리 중인 공간(겹치지 않게).
    issuing: Mutex<HashSet<SpaceId>>,
    accepting: Mutex<HashMap<SpaceId, bool>>,
    /// 바탕마다 기기 트리 증명표(`GetDeviceKeys` 답).
    proofs: Mutex<HashMap<SpaceId, Arc<DeviceProofs>>>,
    /// 가입 중 받아 둔 체크포인트와 받아 둔 때(이 실행에서).
    stash: Mutex<HashMap<SpaceId, Arc<Stash>>>,
    stash_since: Mutex<HashMap<SpaceId, Instant>>,
    /// 이견을 낸 체크포인트.
    disputed: Mutex<HashSet<CheckpointId>>,
    /// 다시 서명한 내 늦은 이벤트.
    resigned: Mutex<HashSet<EventId>>,
    /// 체크포인트로 들어와 다른 출처와 머리를 견주는 중인 공간. 그동안 이 공간에 이벤트를 쓰지 않습니다.
    unconfirmed: Mutex<HashMap<SpaceId, Unconfirmed>>,
    /// 공간 스트림 경계(바탕 경계, 가입 중이면 받아 둔 체크포인트 경계). 잠금 없이 따라잡기·작성이 읽습니다.
    floors: RwLock<HashMap<SpaceId, u64>>,
    /// 따라잡다 받은 경계 아래 이벤트 ID와 논리 시계(저장하지 않음): 상대 머리에 남은 늦은 이벤트를 따라잡기마다 다시
    /// 받지 않고, 해소기가 그 부모를 기다리지 않게 알려 줍니다.
    below: Mutex<HashMap<SpaceId, Below>>,
    /// 가입 때 체크포인트를 받지 못해 다시 물을 공간(받아 준 노드, 링크의 고정).
    retry: Mutex<HashMap<SpaceId, Retry>>,
    /// 시험용: 체크포인트를 내지 않음.
    pub(crate) hold: std::sync::atomic::AtomicBool,
}

/// 다시 물을 곳: (받아 준 노드, 링크의 고정).
type Retry = (PeerId, CheckpointPin);

/// 공간 하나의 경계 아래 기억: (기억한 경계, ID → 논리 시계).
type Below = (u64, HashMap<EventId, u64>);

/// 체크포인트로 들어와 아직 다른 출처와 견주지 못한 공간.
#[derive(Clone, Debug)]
struct Unconfirmed {
    since: Instant,
    /// 체크포인트를 준 출처(이들의 답은 두 번째 출처로 치지 않음).
    sources: Vec<PeerId>,
}

/// [`Unconfirmed`]의 저장 형태(시각은 벽시계 밀리초).
#[derive(Clone, Debug, Serialize, Deserialize)]
struct SavedUnconfirmed {
    since_ms: u64,
    sources: Vec<Vec<u8>>,
}

/// 경계 아래로 기억해 두는 ID 수 상한(공간마다).
const MAX_BELOW: usize = 4096;

// ───────────── 경계 ─────────────

/// 공간 스트림 경계 F(없으면 0). 이 아래 머리를 부모로 두지 않고, 이 아래 부모를 따라가 받지 않습니다.
pub(crate) fn floor(node: &Node, space: &SpaceId) -> u64 {
    node.checkpoints
        .floors
        .read()
        .get(space)
        .copied()
        .unwrap_or(0)
}

/// 해소기 바탕이 바뀐 뒤(적용 잠금 안) 경계를 고칩니다.
pub(crate) fn set_floor(node: &Node, space: &SpaceId, f: u64) {
    let mut m = node.checkpoints.floors.write();
    let stash = node
        .checkpoints
        .stash
        .lock()
        .get(space)
        .map_or(0, |s| s.floor());
    let f = f.max(stash);
    if f == 0 {
        m.remove(space);
    } else {
        m.insert(*space, f);
    }
}

/// 따라잡다 받은 경계 아래 이벤트(ID, 논리 시계)를 기억합니다(저장하지 않고, 다시 묻지 않음).
pub(crate) fn note_below(
    node: &Node,
    space: &SpaceId,
    ids: impl IntoIterator<Item = (EventId, u64)>,
) {
    let f = floor(node, space);
    let mut m = node.checkpoints.below.lock();
    let e = m.entry(*space).or_insert_with(|| (f, HashMap::new()));
    if e.0 != f || e.1.len() >= MAX_BELOW {
        *e = (f, HashMap::new());
    }
    e.1.extend(ids);
}

/// [`note_below`]로 기억한 경계 아래 이벤트인지.
pub(crate) fn is_below(node: &Node, space: &SpaceId, id: &EventId) -> bool {
    node.checkpoints
        .below
        .lock()
        .get(space)
        .is_some_and(|(_, s)| s.contains_key(id))
}

/// [`note_below`]로 기억한 경계 아래 이벤트의 논리 시계.
pub(crate) fn below_lamport(node: &Node, space: &SpaceId, id: &EventId) -> Option<u64> {
    node.checkpoints
        .below
        .lock()
        .get(space)
        .and_then(|(_, s)| s.get(id).copied())
}

/// 기억한 경계 아래 이벤트 가운데 `waits`가 참인 것(해소기가 빠진 부모로 기다리는 것).
pub(crate) fn below_waited(
    node: &Node,
    space: &SpaceId,
    waits: impl Fn(&EventId) -> bool,
) -> Vec<(EventId, u64)> {
    node.checkpoints
        .below
        .lock()
        .get(space)
        .map(|(_, s)| {
            s.iter()
                .filter(|(id, _)| waits(id))
                .map(|(id, l)| (*id, *l))
                .collect()
        })
        .unwrap_or_default()
}

/// 경계 아래라 앞으로도 받지 않을 부모를 받지 않은 기록에서 지웁니다. 체크포인트 조상 가운데 경계 이상인 것은 모두
/// `frontier`에 있으므로, `frontier` 이벤트가 가리키는데 `frontier`에 없는 부모는 경계 아래입니다. 체크포인트로 들어온
/// 가입자가 따라잡기마다 빈 구간을 다시 맞추지 않게 합니다.
fn prune_missing(node: &Node, space: &SpaceId, body: &CheckpointBody) {
    let front: HashSet<EventId> = body.frontier.iter().map(|e| e.id).collect();
    let Ok(rows) = node.store.missing_details(space, StreamId::Space) else {
        return;
    };
    let drop: Vec<EventId> = rows
        .into_iter()
        .filter(|m| {
            !front.contains(&m.id)
                && (front.contains(&m.child) || m.child_lamport <= body.frontier_lamport)
        })
        .map(|m| m.id)
        .collect();
    if let Err(e) = node.store.forget_missing(space, StreamId::Space, &drop) {
        debug!(error = %e, "경계 아래 빈 구간 기록을 지우지 못함");
    }
}

// ───────────── 저장 ─────────────

fn base_key(space: &SpaceId) -> String {
    format!("{BASE_PREFIX}{space}")
}

fn chain_prefix(space: &SpaceId) -> String {
    format!("{CHAIN_PREFIX}{space}.")
}

fn chain_key(space: &SpaceId, seq: u64) -> String {
    format!("{}{seq:020}", chain_prefix(space))
}

fn stash_key(space: &SpaceId) -> String {
    format!("{STASH_PREFIX}{space}")
}

fn unconfirmed_key(space: &SpaceId) -> String {
    format!("{UNCONFIRMED_PREFIX}{space}")
}

fn resigned_key(space: &SpaceId) -> String {
    format!("{RESIGNED_PREFIX}{space}")
}

/// 큰 값은 postcard·base64로 둡니다(JSON 숫자 배열이면 몇 배로 커짐).
fn put_bin<T: Serialize>(node: &Node, key: &str, v: &T) -> Result<()> {
    let bytes = postcard::to_stdvec(v)?;
    node.store.put_setting(
        key,
        &base64::engine::general_purpose::STANDARD.encode(bytes),
    )?;
    Ok(())
}

fn get_bin<T: DeserializeOwned>(node: &Node, key: &str) -> Option<T> {
    let s: String = node.store.get_setting(key).ok()??;
    let bytes = base64::engine::general_purpose::STANDARD.decode(s).ok()?;
    postcard::from_bytes(&bytes).ok()
}

pub(crate) fn saved_base(node: &Node, space: &SpaceId) -> Option<SavedBase> {
    get_bin(node, &base_key(space))
}

/// 남겨 둔 사슬 본문: `from`부터 최대 `max`개(순번 순).
fn saved_chain(node: &Node, space: &SpaceId, from: u64, max: usize) -> Vec<SignedCheckpoint> {
    let mut out = Vec::new();
    let Some(base) = saved_base(node, space) else {
        return out;
    };
    let mut seq = from;
    while seq <= base.pin.seq && out.len() < max {
        match get_bin::<SignedCheckpoint>(node, &chain_key(space, seq)) {
            Some(c) if c.body.seq == seq => out.push(c),
            _ => break,
        }
        seq += 1;
    }
    out
}

fn save_chain(node: &Node, space: &SpaceId, chain: &[SignedCheckpoint]) -> Result<()> {
    for c in chain {
        put_bin(node, &chain_key(space, c.body.seq), c)?;
    }
    // 오래된 본문은 지웁니다.
    let rows = node
        .store
        .settings_with_prefix(&chain_prefix(space))
        .unwrap_or_default();
    if rows.len() > KEEP_CHAIN {
        for (k, _) in &rows[..rows.len() - KEEP_CHAIN] {
            let _ = node.store.delete_setting(k);
        }
    }
    Ok(())
}

/// 공간을 지울 때 기록을 지웁니다.
pub(crate) fn forget_space(node: &Node, space: &SpaceId) {
    if let Some(b) = saved_base(node, space) {
        for blob in b.kept {
            let _ = node.store.blob_delete(&blob);
        }
    }
    let _ = node.store.delete_setting(&base_key(space));
    let _ = node.store.delete_setting(&stash_key(space));
    let _ = node.store.delete_setting(&unconfirmed_key(space));
    let _ = node.store.delete_setting(&resigned_key(space));
    for (k, _) in node
        .store
        .settings_with_prefix(&chain_prefix(space))
        .unwrap_or_default()
    {
        let _ = node.store.delete_setting(&k);
    }
    let c = &node.checkpoints;
    c.watch.lock().remove(space);
    c.proofs.lock().remove(space);
    c.stash.lock().remove(space);
    c.stash_since.lock().remove(space);
    c.unconfirmed.lock().remove(space);
    c.floors.write().remove(space);
    c.below.lock().remove(space);
    c.retry.lock().remove(space);
}

// ───────────── 스냅숏 블롭 ─────────────

/// 스냅숏 블롭 키: 체크포인트 이벤트의 상태 키에서 (순번, 상태 해시)로 파생합니다. 상태 해시를 넣어, 같은 순번을 두고
/// 겨룬 두 체크포인트의 서로 다른 평문이 같은 (키, 논스)로 암호화되지 않게 합니다(블롭 논스는 조각 번호).
fn snapshot_key(state_key: &Secret32, seq: u64, state_hash: &[u8; 32]) -> Secret32 {
    let mut info = Vec::with_capacity(40);
    info.extend_from_slice(&seq.to_le_bytes());
    info.extend_from_slice(state_hash);
    yangpa_crypto::kdf::derive(&state_key.0, SNAPSHOT_SALT, &info)
}

/// 결정론적 암호화(조각 크기는 노드 설정이 아니라 프로토콜 값): 모든 멤버가 같은 블롭을 만들어 함께 시드합니다.
pub(crate) fn seal_snapshot(
    state_key: &Secret32,
    seq: u64,
    state_hash: &[u8; 32],
    plain: &[u8],
) -> Result<(Vec<u8>, BlobManifest)> {
    let k = snapshot_key(state_key, seq, state_hash);
    Ok(yangpa_crypto::blob::try_encrypt_blob(
        &k,
        plain,
        DEFAULT_CHUNK_SIZE,
    )?)
}

pub(crate) fn open_snapshot(
    state_key: &Secret32,
    seq: u64,
    state_hash: &[u8; 32],
    manifest: &BlobManifest,
    ct: &[u8],
) -> Result<Vec<u8>> {
    let k = snapshot_key(state_key, seq, state_hash);
    Ok(yangpa_crypto::blob::decrypt_blob(&k, manifest, ct)?)
}

/// 암호문 블롭을 저장하고 고정합니다(시드용, 공간에 묶음).
fn store_blob(node: &Node, space: &SpaceId, manifest: &BlobManifest, ct: &[u8]) -> Result<BlobId> {
    let pb = PersistentBlob::new(manifest.clone(), SecurityLevel::Normal)?;
    let id = pb.id();
    node.store.blob_register(space, &pb)?;
    node.store.blob_set_pinned(&id, true)?;
    let cs = manifest.chunk_size as usize;
    for (i, c) in ct.chunks(cs).enumerate() {
        node.store.blob_write_chunk(&id, i as u32, c)?;
    }
    Ok(id)
}

/// 저장된 블롭 전체(다 받았을 때만).
fn read_blob(node: &Node, blob: &BlobId) -> Option<(BlobManifest, Vec<u8>)> {
    let m = node.store.blob_manifest(blob).ok()??;
    let mut out = Vec::with_capacity(m.size as usize);
    for i in 0..m.chunks.len() as u32 {
        out.extend(node.store.blob_read_chunk(blob, i).ok()??);
    }
    Some(((*m).clone(), out))
}

fn blob_complete(node: &Node, blob: &BlobId) -> bool {
    node.store
        .blob_status(blob)
        .ok()
        .flatten()
        .is_some_and(|s| s.complete)
}

/// 지금 바탕의 (체크포인트 이벤트, 스냅숏 블롭). 블롭을 다 가졌을 때만(보관 노드에 알릴 때).
pub(crate) fn current_snapshot(node: &Node, space: &SpaceId) -> Option<(EventId, BlobId)> {
    let b = saved_base(node, space)?;
    (b.kept.contains(&b.snapshot) && blob_complete(node, &b.snapshot))
        .then_some((b.event, b.snapshot))
}

/// 이 공간에서 내주는 스냅숏 블롭인지(`blobs::serve_uses`).
pub(crate) fn is_snapshot(node: &Node, space: &SpaceId, blob: &BlobId) -> bool {
    saved_base(node, space).is_some_and(|b| b.kept.contains(blob))
        || node
            .checkpoints
            .stash
            .lock()
            .get(space)
            .is_some_and(|s| s.head().body.snapshot == *blob)
}

// ───────────── 시작 ─────────────

/// 주기 작업을 띄웁니다.
pub(crate) fn start(node: &Arc<Node>) {
    // 가입 중이던 공간의 받아 둔 체크포인트를 되살립니다.
    for (k, _) in node
        .store
        .settings_with_prefix(STASH_PREFIX)
        .unwrap_or_default()
    {
        let Ok(space) = k[STASH_PREFIX.len()..].parse::<SpaceId>() else {
            continue;
        };
        if let Some(s) = get_bin::<Stash>(node, &k) {
            let f = s.floor();
            node.checkpoints.stash.lock().insert(space, Arc::new(s));
            node.checkpoints
                .stash_since
                .lock()
                .insert(space, Instant::now());
            let cur = floor(node, &space);
            node.checkpoints.floors.write().insert(space, cur.max(f));
        }
    }
    let weak = Arc::downgrade(node);
    let every = node.cfg.checkpoint.check_every;
    node.tasks.spawn(async move {
        loop {
            tokio::time::sleep(every).await;
            let Some(node) = weak.upgrade() else { break };
            if node.is_shutting_down() {
                break;
            }
            tick(&node).await;
        }
    });
}

/// 공간마다: 낼 때인지 보고, 받아 둔 체크포인트를 설치해 보고, 들어온 뒤 머리를 견줍니다.
async fn tick(node: &Arc<Node>) {
    for space in node.space_ids() {
        let stashed = node.checkpoints.stash.lock().contains_key(&space);
        if stashed {
            if let Err(e) = try_install(node, space).await {
                debug!(space = %space.short(), error = %e, "받아 둔 체크포인트 설치 실패");
            }
            continue;
        }
        if node.checkpoints.unconfirmed.lock().contains_key(&space) {
            confirm(node, space).await;
        }
        if node.cfg.checkpoint.enabled
            && let Err(e) = consider(node, space).await
        {
            debug!(space = %space.short(), error = %e, "체크포인트를 내지 못함");
        }
    }
}

// ───────────── 내기 ─────────────

/// 해소기에서 읽은 낼지 판단할 값.
struct Probe {
    heads: Vec<EventId>,
    since_base: usize,
    keys_in_use: usize,
    base: Option<CheckpointBase>,
    delegation: Option<DelegationCert>,
}

fn core_of(node: &Node, space: &SpaceId) -> Option<Arc<Mutex<spaces::SpaceCore>>> {
    node.spaces.read().get(space).map(|rt| rt.core.clone())
}

/// 해소기의 지금 바탕 체크포인트 ID(소유권 이전 인증서를 묶을 곳). 공간 잠금 안에서 부릅니다.
pub(crate) fn base_id(node: &Node, space: &SpaceId) -> Option<CheckpointId> {
    core_of(node, space)?.lock().resolver.base().map(|b| b.id)
}

/// 해소기 머리와 그 논리 시계(경계 위). 공간 스트림 이벤트의 부모로 씁니다: 저장소 머리는 경계 아래 이벤트(낮은 논리
/// 시계로 지어낸 것 포함)가 경계 위 진짜 머리를 가릴 수 있고, 그것을 덮지 않으면 체크포인트를 영영 낼 수 없습니다.
/// 공간 잠금 안에서 부릅니다.
pub(crate) fn resolver_heads(node: &Node, space: &SpaceId) -> Vec<(EventId, u64)> {
    core_of(node, space).map_or_else(Vec::new, |c| c.lock().resolver.head_lamports())
}

async fn probe(node: &Arc<Node>, space: SpaceId) -> Option<Probe> {
    let _l = ingest::lock_space(node, space).await;
    let core = core_of(node, &space)?;
    let c = core.lock();
    Some(Probe {
        heads: c.resolver.heads(),
        since_base: c.resolver.since_base(),
        keys_in_use: c.resolver.state_keys_in_use(),
        base: c.resolver.base(),
        delegation: c.resolver.delegation(),
    })
}

/// 체크포인트를 쓸 기기. 위임이 있고 아직 유효하며 바탕이 밝힌 것이면 위임받은 기기, 아니면 소유자 기기 가운데 확정
/// 기기(소유자의 것일 때) 또는 가장 작은 기기 ID. 새 멤버의 사슬 검증은 바탕 체크포인트의 소유자 목록에서 활성이던
/// 기기의 체크포인트만 받으므로 그런 기기를 먼저 고릅니다(바탕 뒤에 더한 기기가 쓰면 새 멤버가 처음부터 따라잡음). 다른
/// 소유자 기기는 [`CheckpointOptions::merge_after`]만큼 아무도 내지 않을 때만 냅니다.
fn preferred_issuer(
    st: &SpaceState,
    delegation: Option<&DelegationCert>,
    base: Option<&CheckpointBase>,
    next_seq: u64,
) -> Option<yangpa_core::ids::DeviceId> {
    let owner = st.devices.get(&st.owner)?;
    if let Some(d) = delegation
        && usable_delegation(d, base, next_seq)
        && owner.contains_active(&d.cosig.device)
    {
        return Some(d.device);
    }
    let in_base =
        |d: &yangpa_core::ids::DeviceId| base.is_none_or(|b| b.owner_devices.contains_active(d));
    let committer = owner.contains_active(&st.committer).then_some(st.committer);
    committer
        .filter(in_base)
        .or_else(|| owner.active().map(|r| r.device).filter(in_base).min())
        .or(committer)
        .or_else(|| owner.active().map(|r| r.device).min())
}

/// 위임으로 다음 순번을 쓸 수 있는지: 유효 기간 안이고, 바탕이 있으면 바탕이 밝힌 위임이어야 합니다(해소기 규칙).
fn usable_delegation(d: &DelegationCert, base: Option<&CheckpointBase>, next_seq: u64) -> bool {
    d.until_seq >= next_seq && base.is_none_or(|b| b.delegation_hash == Some(d.hash()))
}

/// 낼 때이면 냅니다.
async fn consider(node: &Arc<Node>, space: SpaceId) -> Result<()> {
    let opts = node.cfg.checkpoint.clone();
    let me = node.me()?;
    let Some(st) = node.space_state(&space) else {
        return Ok(());
    };
    if st.kind != SpaceKind::Crew
        || node.checkpoints.issuing.lock().contains(&space)
        || node
            .checkpoints
            .hold
            .load(std::sync::atomic::Ordering::Relaxed)
    {
        return Ok(());
    }
    let owner_device = st.owner == me.user()
        && st
            .devices
            .get(&st.owner)
            .is_some_and(|l| l.contains_active(&me.device_id()));
    let Some(p) = probe(node, space).await else {
        return Ok(());
    };
    let next_seq = p.base.as_ref().map_or(1, |b| b.seq + 1);
    let delegated = p
        .delegation
        .as_ref()
        .filter(|d| d.device == me.device_id() && usable_delegation(d, p.base.as_ref(), next_seq))
        .cloned();
    if !owner_device && delegated.is_none() {
        node.checkpoints.watch.lock().remove(&space);
        return Ok(());
    }
    let due = match &p.base {
        None => {
            p.since_base > opts.start_events || st.active_devices().count() > opts.start_devices
        }
        Some(b) => {
            p.since_base >= opts.every_events
                || p.keys_in_use >= MAX_VALID_STATE_KEYS
                || (p.since_base > 0 && base_age(node, b).is_some_and(|a| a >= opts.every))
        }
    };
    let now = Instant::now();
    let (stable, overdue) = {
        let mut w = node.checkpoints.watch.lock();
        let e = w.entry(space).or_insert_with(|| Watch {
            heads: p.heads.clone(),
            since: now,
            due_since: None,
        });
        if e.heads != p.heads {
            e.heads = p.heads.clone();
            e.since = now;
        }
        if !due {
            e.due_since = None;
            return Ok(());
        }
        let due_since = *e.due_since.get_or_insert(now);
        (
            e.heads.len() == 1 && now.duration_since(e.since) >= opts.settle,
            now.duration_since(due_since) >= opts.merge_after,
        )
    };
    let preferred = preferred_issuer(&st, p.delegation.as_ref(), p.base.as_ref(), next_seq)
        == Some(me.device_id());
    // 가라앉았으면 정한 기기가, 오래 가라앉지 않으면 머리를 합치는 체크포인트를 냅니다(정한 기기가 꺼져 있으면 다른
    // 소유자 기기가 이어 냄).
    let go = (preferred && (stable || overdue))
        || (owner_device && overdue && now_overdue_twice(node, &space, opts.merge_after));
    if !go {
        return Ok(());
    }
    node.checkpoints.issuing.lock().insert(space);
    let r = issue(node, space, if owner_device { None } else { delegated }).await;
    node.checkpoints.issuing.lock().remove(&space);
    if r.is_ok() {
        node.checkpoints.watch.lock().remove(&space);
    }
    r
}

/// 정한 기기가 아닌 소유자 기기: 문턱을 넘은 지 `merge_after`의 두 배가 지났는지(정한 기기에게 먼저 기회를 줌).
fn now_overdue_twice(node: &Node, space: &SpaceId, merge_after: Duration) -> bool {
    node.checkpoints
        .watch
        .lock()
        .get(space)
        .and_then(|w| w.due_since)
        .is_some_and(|d| d.elapsed() >= merge_after * 2)
}

/// 바탕 체크포인트 이벤트가 쓰인 뒤 지난 시간.
fn base_age(node: &Node, b: &CheckpointBase) -> Option<Duration> {
    let ev = node.store.get_event(&b.event).ok()??;
    let now = yangpa_core::time::Timestamp::now().0;
    Some(Duration::from_millis(
        now.saturating_sub(ev.header.created_at.0),
    ))
}

/// 체크포인트를 만들어 냅니다. 초안·발행은 공간 잠금 안, 스냅숏 암호화·저장은 잠금 밖에서.
async fn issue(node: &Arc<Node>, space: SpaceId, delegation: Option<DelegationCert>) -> Result<()> {
    let me = node.me()?;
    let (draft, key_id, key) = {
        let _l = ingest::lock_space(node, space).await;
        let (key_id, key) = keys::current_state_key(node, &space)?
            .ok_or_else(|| Error::conflict("상태 키가 없음"))?;
        let core = core_of(node, &space).ok_or_else(|| Error::not_found("없는 공간"))?;
        let mut guard = core.lock();
        let c = &mut *guard;
        let mut d = c
            .resolver
            .checkpoint_draft(key_id)
            .map_err(Error::conflict)?;
        // 비공개·덮개 그룹의 키 머리: 내가 아는 그룹 사슬의 이긴 끝과 그 사슬 정보(머리 이벤트·크루 순번·새 T의 크루
        // 순번·멤버 요약·작성자). 모든 노드가 여기서 같은 바닥을 세우고, 그 뒤 경계 아래 그룹 갱신은 늦은 이벤트입니다.
        c.keys.ensure_loaded(node, &space);
        let heads: Vec<KeyHead> = c
            .keys
            .chains
            .values()
            .filter_map(|ch| ch.key_head())
            .collect();
        d.add_group_heads(&space, heads);
        drop(guard);
        (d, key_id, key)
    };
    let owner_list = if draft.owner == me.user() {
        me.devices.clone()
    } else {
        node.store
            .get_device_list(&draft.owner)?
            .ok_or_else(|| Error::conflict("소유자 기기 목록을 모름"))?
    };
    if MemberDevices::from_list(&owner_list.list) != *draft.owner_record {
        return Err(Error::conflict(
            "소유자 기기 목록이 상태와 다름(갱신을 기다림)",
        ));
    }
    // 암호문은 평문보다 크므로, 평문이 이미 상한을 넘으면 암호화(수십 MB 복사·암호화)를 하지 않고 그만둡니다.
    if draft.snapshot.len() as u64 > MAX_SNAPSHOT_LEN {
        return Err(Error::conflict("스냅숏이 상한보다 큼"));
    }
    let plain = draft.snapshot.clone();
    let (seq, hash) = (draft.seq, draft.state_hash);
    let k = key.clone();
    let (ct, manifest) = tokio::task::spawn_blocking(move || seal_snapshot(&k, seq, &hash, &plain))
        .await
        .map_err(|_| Error::internal("스냅숏 암호화 작업 실패"))??;
    if manifest.size > MAX_SNAPSHOT_LEN {
        return Err(Error::conflict("스냅숏이 상한보다 큼"));
    }
    let blob = {
        let n = node.clone();
        let m = manifest.clone();
        tokio::task::spawn_blocking(move || store_blob(&n, &space, &m, &ct))
            .await
            .map_err(|_| Error::internal("스냅숏 저장 작업 실패"))??
    };
    let mut body = CheckpointBody {
        space,
        seq: draft.seq,
        prev: draft.prev,
        heads: draft.heads.clone(),
        frontier_lamport: draft.frontier_lamport,
        frontier: draft.frontier.clone(),
        state_hash: draft.state_hash,
        user_root: draft.user_root,
        users: draft.users,
        device_root: draft.device_root,
        devices: draft.devices,
        snapshot: blob,
        snapshot_len: manifest.size,
        key_heads: draft.key_heads.clone(),
        valid_state_keys: draft.valid_state_keys.clone(),
        owner: draft.owner,
        owner_devices: owner_list,
        owner_proof: draft.owner_proof.clone(),
        revoked: draft.revoked.clone(),
        revocation_root: draft.revocation_root,
        owner_chain: draft.owner_chain.clone(),
        resolver_version: RESOLVER_VERSION,
        author: me.device_id(),
        delegation: None,
        delegation_hash: draft.delegation_hash,
        signer_proofs: draft.signer_proofs_for(&me.device_id()),
    };
    let signed = match delegation {
        None => SignedCheckpoint::sign_owner(body, &me.identity.signing, &me.device.signing)?,
        Some(d) => {
            body.delegation = Some(d);
            SignedCheckpoint::sign_delegated(body, &me.device.signing)?
        }
    };
    let l = ingest::lock_space(node, space).await;
    let core = core_of(node, &space).ok_or_else(|| Error::not_found("없는 공간"))?;
    if core.lock().resolver.base().map(|b| b.id) != draft.prev {
        // 그새 다른 체크포인트가 바탕이 됨: 이 초안은 낡았습니다.
        drop(l);
        if !is_snapshot(node, &space, &blob) {
            let _ = node.store.blob_delete(&blob);
        }
        return Ok(());
    }
    let parents: Vec<(EventId, u64)> = draft
        .heads
        .iter()
        .map(|h| (*h, draft.lamport.saturating_sub(1)))
        .collect();
    let published = author::publish_with_parents_locked(
        node,
        &l,
        space,
        &EventBody::StateCheckpoint(Box::new(signed)),
        KeyChoice::StateWith(key_id, key),
        &parents,
    );
    drop(l);
    let (ev, report) = match published {
        Ok(x) => x,
        Err(e) => {
            if !is_snapshot(node, &space, &blob) {
                let _ = node.store.blob_delete(&blob);
            }
            return Err(e);
        }
    };
    info!(space = %space.short(), seq = draft.seq, "체크포인트를 냄");
    ingest::after_unlock(node, space, &report);
    sync_service::propagate(node, space, vec![ev]);
    Ok(())
}

// ───────────── 받은 뒤 ─────────────

/// 새 바탕을 받은 뒤(잠금을 푼 뒤). 같은 공간에 겹쳐 부르면 한 번 더 돕니다.
pub(crate) fn on_accepted(node: &Arc<Node>, space: SpaceId) {
    {
        let mut a = node.checkpoints.accepting.lock();
        if let Some(again) = a.get_mut(&space) {
            *again = true;
            return;
        }
        a.insert(space, false);
    }
    node.spawn_detached(move |node| async move {
        loop {
            if let Err(e) = accept_base(&node, space).await {
                debug!(space = %space.short(), error = %e, "받은 체크포인트 처리 실패");
            }
            let mut a = node.checkpoints.accepting.lock();
            match a.get_mut(&space) {
                Some(again) if *again => *again = false,
                _ => {
                    a.remove(&space);
                    break;
                }
            }
        }
    });
}

/// 지금 바탕의 스냅숏 블롭을 만들거나 확인하고, 사슬 본문·바탕 기록을 둡니다.
async fn accept_base(node: &Arc<Node>, space: SpaceId) -> Result<()> {
    let (base, plain) = {
        let _l = ingest::lock_space(node, space).await;
        let Some(core) = core_of(node, &space) else {
            return Ok(());
        };
        let c = core.lock();
        let Some(base) = c.resolver.base() else {
            return Ok(());
        };
        if saved_base(node, &space).is_some_and(|s| s.pin.id == base.id) {
            return Ok(());
        }
        let plain = if blob_complete(node, &base.snapshot) {
            None
        } else {
            c.resolver.base_snapshot()
        };
        (base, plain)
    };
    let key_id = base
        .key
        .ok_or_else(|| Error::internal("체크포인트 이벤트의 상태 키를 모름"))?;
    let key = keys::crew_state_key(node, &space, &key_id)?
        .ok_or_else(|| Error::conflict("체크포인트 이벤트의 상태 키가 없음"))?;
    if let Some(plain) = plain {
        let k = key.clone();
        let (seq, hash) = (base.seq, base.state_hash);
        let (ct, manifest) =
            tokio::task::spawn_blocking(move || seal_snapshot(&k, seq, &hash, &plain))
                .await
                .map_err(|_| Error::internal("스냅숏 암호화 작업 실패"))??;
        if manifest.id() != base.snapshot || manifest.size != base.snapshot_len {
            dispute(node, space, &base).await;
        } else {
            let n = node.clone();
            tokio::task::spawn_blocking(move || store_blob(&n, &space, &manifest, &ct))
                .await
                .map_err(|_| Error::internal("스냅숏 저장 작업 실패"))??;
        }
    }
    // 사슬 본문: 저장된 체크포인트 이벤트를 풀어 둡니다(가입자에게 답할 때).
    let ev = node
        .store
        .get_event(&base.event)?
        .ok_or_else(|| Error::internal("체크포인트 이벤트가 저장돼 있지 않음"))?;
    let sc = decode_checkpoint(&key, &ev)?;
    save_chain(node, &space, std::slice::from_ref(&sc))?;
    record_base(node, &space, &base, key_id)?;
    prune_missing(node, &space, &sc.body);
    Ok(())
}

/// 바탕 기록을 남기고 스냅숏 블롭을 최근 [`KEEP_SNAPSHOTS`]개로 줄입니다.
fn record_base(node: &Node, space: &SpaceId, base: &CheckpointBase, key: StateKeyId) -> Result<()> {
    let prev = saved_base(node, space);
    let mut kept = vec![base.snapshot];
    if blob_complete(node, &base.snapshot) {
        // 받지 못한(이견) 블롭은 남길 것으로 세지 않습니다.
    } else {
        kept.clear();
    }
    for b in prev.iter().flat_map(|p| p.kept.iter()) {
        if !kept.contains(b) {
            kept.push(*b);
        }
    }
    for b in kept.split_off(KEEP_SNAPSHOTS.min(kept.len())) {
        let _ = node.store.blob_delete(&b);
    }
    let saved = SavedBase {
        pin: CheckpointPin {
            seq: base.seq,
            id: base.id,
        },
        event: base.event,
        floor: base.floor,
        snapshot: base.snapshot,
        key,
        kept,
    };
    put_bin(node, &base_key(space), &saved)?;
    node.checkpoints.proofs.lock().remove(space);
    Ok(())
}

fn decode_checkpoint(key: &Secret32, ev: &SignedEvent) -> Result<SignedCheckpoint> {
    let bytes = ev
        .body
        .as_deref()
        .ok_or_else(|| Error::internal("체크포인트 본문 없음"))?;
    match decrypt_body(key, &ev.header, bytes)? {
        EventBody::StateCheckpoint(c) => Ok(*c),
        _ => Err(Error::invalid("체크포인트 이벤트가 아님")),
    }
}

/// 내 스냅숏 블롭이 본문과 다름: 정보용 이견을 한 번 냅니다(해소기가 같은 기기의 두 번째 이견을 거부).
async fn dispute(node: &Arc<Node>, space: SpaceId, base: &CheckpointBase) {
    if !node.checkpoints.disputed.lock().insert(base.id) {
        return;
    }
    warn!(space = %space.short(), seq = base.seq, "체크포인트 스냅숏 블롭이 내 계산과 다름: 이견을 냄");
    let body = EventBody::CheckpointDispute {
        checkpoint: base.id,
        seq: base.seq,
        state_hash: base.state_hash,
    };
    if let Err(e) = author::publish(node, space, None, &body, KeyChoice::State).await {
        debug!(error = %e, "체크포인트 이견을 내지 못함");
    }
}

/// 해소기가 늦은 이벤트·은퇴한 키로 거부한 내 상태 이벤트를 지금 머리·지금 키로 다시 냅니다. 한 이벤트는 한 번만
/// 다시 냅니다(재시작해도: 기억을 저장하고, 저장소에서 다시 적용한 판정으로는 부르지 않음). [`RESIGN_MAX_AGE`]보다
/// 오래된 이벤트는 다시 내지 않습니다.
pub(crate) fn resign(node: &Arc<Node>, space: SpaceId, ids: Vec<EventId>) {
    node.spawn_detached(move |node| async move {
        let Ok(me) = node.me() else { return };
        for id in ids {
            if !node.checkpoints.resigned.lock().insert(id) || was_resigned(&node, &space, &id) {
                continue;
            }
            let Ok(Some(ev)) = node.store.get_event(&id) else {
                continue;
            };
            let h = &ev.header;
            let age = yangpa_core::time::Timestamp::now()
                .0
                .saturating_sub(h.created_at.0);
            if h.author != me.device_id()
                || h.kind.lane() != Lane::State
                || matches!(
                    h.kind,
                    EventKind::StateCheckpoint | EventKind::CheckpointDispute
                )
                || age > RESIGN_MAX_AGE.as_millis() as u64
            {
                continue;
            }
            // 내기 전에 기억합니다: 내다 멈춰도 다음 시작에 같은 것을 또 내지 않게(한 번 못 내는 쪽이 낫습니다).
            remember_resigned(&node, &space, id);
            let KeyRef::State(kid) = h.key else { continue };
            let (Ok(Some(key)), Some(bytes)) = (
                keys::crew_state_key(&node, &space, &kid),
                ev.body.as_deref(),
            ) else {
                continue;
            };
            let Ok(body) = decrypt_body(&key, h, bytes) else {
                continue;
            };
            match author::publish(&node, space, h.channel, &body, KeyChoice::State).await {
                Ok(_) => debug!(space = %space.short(), "경계 아래로 밀린 내 이벤트를 다시 냄"),
                Err(e) => debug!(error = %e, "늦은 이벤트를 다시 내지 못함"),
            }
        }
    });
}

fn was_resigned(node: &Node, space: &SpaceId, id: &EventId) -> bool {
    get_bin::<Vec<EventId>>(node, &resigned_key(space)).is_some_and(|v| v.contains(id))
}

fn remember_resigned(node: &Node, space: &SpaceId, id: EventId) {
    let key = resigned_key(space);
    let mut v: Vec<EventId> = get_bin(node, &key).unwrap_or_default();
    v.push(id);
    if v.len() > KEEP_RESIGNED {
        v.drain(..v.len() - KEEP_RESIGNED);
    }
    if let Err(e) = put_bin(node, &key, &v) {
        debug!(error = %e, "다시 낸 이벤트 기록을 저장하지 못함");
    }
}

// ───────────── 재시작 ─────────────

/// 저장된 바탕으로 해소기를 만듭니다(시작 때). 못 쓰면 `None`(처음부터 다시 해소).
pub(crate) fn restore(node: &Node, space: &SpaceId, genesis: EventId) -> Option<(Resolver, u64)> {
    let saved = saved_base(node, space)?;
    let built = (|| -> Result<Resolver> {
        let key = keys::crew_state_key(node, space, &saved.key)?
            .ok_or_else(|| Error::conflict("체크포인트 상태 키 없음"))?;
        let ev = node
            .store
            .get_event(&saved.event)?
            .ok_or_else(|| Error::conflict("체크포인트 이벤트 없음"))?;
        let sc = decode_checkpoint(&key, &ev)?;
        let (manifest, ct) = read_blob(node, &sc.body.snapshot)
            .ok_or_else(|| Error::conflict("스냅숏 블롭 없음"))?;
        let plain = open_snapshot(&key, sc.body.seq, &sc.body.state_hash, &manifest, &ct)?;
        let decoded = DecodedEvent {
            id: ev.id(),
            header: ev.header.clone(),
            body: EventBody::StateCheckpoint(Box::new(sc)),
        };
        Resolver::from_checkpoint(genesis, decoded, &plain)
            .map_err(|e| Error::conflict(e.to_string()))
    })();
    match built {
        Ok(r) => {
            node.checkpoints.floors.write().insert(*space, saved.floor);
            reload_unconfirmed(node, space);
            Some((r, saved.floor))
        }
        Err(e) => {
            warn!(space = %space.short(), error = %e, "저장된 체크포인트로 시작하지 못해 처음부터 다시 해소");
            None
        }
    }
}

// ───────────── 답하기 ─────────────

/// 요청한 피어가 이 공간의 멤버 기기면 (사용자, 상태).
#[allow(clippy::result_large_err)]
pub(crate) fn member_peer(
    node: &Node,
    peer: &PeerId,
    space: &SpaceId,
) -> std::result::Result<(UserId, Arc<SpaceState>), YpResponse> {
    let Some((user, device)) = node.peers.identity_of(peer) else {
        return Err(YpResponse::error(WireErrorCode::Unauthenticated, "인증 전"));
    };
    let Some(state) = node.space_state(space) else {
        return Err(YpResponse::error(WireErrorCode::NotFound, "모르는 공간"));
    };
    if !node
        .sync
        .index
        .get(space, &state)
        .is_member_device(&user, &device)
    {
        return Err(YpResponse::error(WireErrorCode::NotMember, "멤버가 아님"));
    }
    Ok((user, state))
}

/// `GetCheckpoint`: 남겨 둔 사슬, 최신 체크포인트 이벤트, 스냅숏 매니페스트.
pub(crate) fn handle_get_checkpoint(
    node: &Node,
    peer: PeerId,
    space: SpaceId,
    from_seq: u64,
) -> YpResponse {
    if let Err(r) = member_peer(node, &peer, &space) {
        return r;
    }
    let Some(base) = saved_base(node, &space) else {
        return YpResponse::Checkpoint(Box::default());
    };
    let chain = if from_seq <= base.pin.seq {
        saved_chain(node, &space, from_seq.max(1), MAX_CHAIN_PER_REPLY)
    } else {
        Vec::new()
    };
    let event = node.store.get_event(&base.event).ok().flatten();
    let manifest = node
        .store
        .blob_manifest(&base.snapshot)
        .ok()
        .flatten()
        .filter(|_| blob_complete(node, &base.snapshot))
        .map(|m| (*m).clone());
    YpResponse::Checkpoint(Box::new(CheckpointReply {
        head: Some(base.pin),
        chain,
        event,
        manifest,
    }))
}

/// `GetDeviceKeys`: 아는 활성 기기의 키와, 바탕이 있으면 기기 트리 잎 증명.
pub(crate) async fn handle_get_device_keys(
    node: &Arc<Node>,
    peer: PeerId,
    space: SpaceId,
    devices: Vec<yangpa_core::ids::DeviceId>,
) -> YpResponse {
    let state = match member_peer(node, &peer, &space) {
        Ok((_, s)) => s,
        Err(r) => return r,
    };
    if is_unconfirmed(node, &space) {
        return YpResponse::error(WireErrorCode::NotFound, "아직 확인하는 중인 상태");
    }
    if devices.len() > MAX_DEVICE_KEYS_REQUEST {
        return YpResponse::error(WireErrorCode::BadRequest, "기기가 너무 많음");
    }
    let index = node.sync.index.get(&space, &state);
    let recs: Vec<(yangpa_core::ids::DeviceId, [u8; 32])> = devices
        .iter()
        .filter_map(|d| {
            let u = index.device_user(d)?;
            let r = state.devices.get(&u)?.get(d)?;
            Some((*d, r.key_commit))
        })
        .collect();
    let commits: Vec<[u8; 32]> = recs.iter().map(|(_, c)| *c).collect();
    let found = node.devkeys.get_many(node, &commits);
    let proofs = device_proofs(node, space).await;
    let items = recs
        .into_iter()
        .filter_map(|(d, c)| {
            let keys = found.get(&c)?;
            Some(DeviceKeysItem {
                keys: (**keys).clone(),
                proof: proofs.as_ref().and_then(|p| p.proof(&d)),
            })
        })
        .collect();
    YpResponse::DeviceKeys(items)
}

/// 지금 바탕의 기기 트리 증명표(바탕마다 한 번 셈).
async fn device_proofs(node: &Arc<Node>, space: SpaceId) -> Option<Arc<DeviceProofs>> {
    let base = saved_base(node, &space)?;
    if let Some(p) = node.checkpoints.proofs.lock().get(&space)
        && p.tree.checkpoint == base.pin.id
    {
        return Some(p.clone());
    }
    let p = {
        let _l = ingest::lock_space(node, space).await;
        let core = core_of(node, &space)?;
        let c = core.lock();
        c.resolver.device_proofs()?
    };
    if p.tree.checkpoint != base.pin.id {
        return None;
    }
    let p = Arc::new(p);
    node.checkpoints.proofs.lock().insert(space, p.clone());
    Some(p)
}

/// 기기 키를 피어에게 묻고, 상태의 키 약속(또는 아는 체크포인트의 기기 트리)과 맞는 것만 키 표에 넣습니다. 확정 기기가
/// 봉인하기 전에(체크포인트로 들어와 가입 이벤트의 키 목록을 본 적 없는 기기) 씁니다. 넣은 수를 돌려줍니다.
pub(crate) async fn prefetch_device_keys(
    node: &Arc<Node>,
    space: SpaceId,
    want: Vec<yangpa_core::ids::DeviceId>,
) -> usize {
    let Some(state) = node.space_state(&space) else {
        return 0;
    };
    let Ok(me) = node.me() else { return 0 };
    let index = node.sync.index.get(&space, &state);
    let tree = saved_base(node, &space)
        .and_then(|b| get_bin::<SignedCheckpoint>(node, &chain_key(&space, b.pin.seq)))
        .map(|c| DeviceTree::of(&c));
    let peers: Vec<PeerId> = node
        .peers
        .authed_peers()
        .into_iter()
        .filter(|(_, u, d)| *d != me.device_id() && index.is_member_device(u, d))
        .map(|(p, _, _)| p)
        .take(3)
        .collect();
    let mut left: Vec<_> = want;
    let mut stored = 0;
    for peer in peers {
        if left.is_empty() {
            break;
        }
        for chunk in left.clone().chunks(MAX_DEVICE_KEYS_REQUEST) {
            let req = YpRequest::GetDeviceKeys {
                space,
                devices: chunk.to_vec(),
            };
            let Ok(YpResponse::DeviceKeys(items)) = crate::p2p::request(node, peer, req).await
            else {
                continue;
            };
            let good = check_device_keys(&space, &state, &index, tree.as_ref(), chunk, items);
            if good.is_empty() {
                continue;
            }
            if let Err(e) = node.store.put_device_keys(&space, &good) {
                debug!(error = %e, "기기 키 저장 실패");
                continue;
            }
            stored += good.len();
            left.retain(|d| !good.iter().any(|k| k.device == *d));
        }
    }
    stored
}

/// `GetDeviceKeys` 답 가운데 물은 기기이고 상태의 키 약속(또는 체크포인트 기기 트리 증명)과 맞는 키만.
fn check_device_keys(
    space: &SpaceId,
    state: &SpaceState,
    index: &crate::state_index::StateIndex,
    tree: Option<&DeviceTree>,
    asked: &[yangpa_core::ids::DeviceId],
    items: Vec<DeviceKeysItem>,
) -> Vec<yangpa_core::device::DeviceKeys> {
    let mut good = Vec::new();
    for it in items {
        let d = it.keys.device;
        if !asked.contains(&d) {
            continue;
        }
        let known = index
            .device_user(&d)
            .and_then(|u| state.devices.get(&u)?.get(&d).cloned());
        if accept_device_keys(&it, known.as_ref(), tree) {
            good.push(it.keys);
        } else {
            warn!(space = %space.short(), "키 약속과 맞지 않는 기기 키 답을 버림");
        }
    }
    good
}

/// 시험용: `GetDeviceKeys` 답을 가입한 쪽이 받아들이는 규칙 그대로 거릅니다.
pub(crate) fn accept_reply(
    node: &Node,
    space: &SpaceId,
    asked: &[yangpa_core::ids::DeviceId],
    items: Vec<DeviceKeysItem>,
) -> Vec<yangpa_core::device::DeviceKeys> {
    let Some(state) = node.space_state(space) else {
        return Vec::new();
    };
    let index = node.sync.index.get(space, &state);
    let tree = saved_base(node, space)
        .and_then(|b| get_bin::<SignedCheckpoint>(node, &chain_key(space, b.pin.seq)))
        .map(|c| DeviceTree::of(&c));
    check_device_keys(space, &state, &index, tree.as_ref(), asked, items)
}

// ───────────── 가입 ─────────────

/// 받아 준 노드에게서 체크포인트 사슬을 받아 확인하고 스냅숏을 받아 둡니다(가입 묶음을 적용하기 전). 고정이 있는
/// 링크인데 사슬이 이어지지 않으면 오류입니다. 고정이 없는 링크(체크포인트 전에 만든 것)는 체크포인트로 들어오지 않고
/// 처음부터 받습니다: 시작 이벤트만으로는 그 뒤 소유권 이전을 알 수 없어, 받아 준 노드가 지어낸 첫 체크포인트를 가려낼
/// 수 없습니다.
pub(crate) async fn prepare_join(
    node: &Arc<Node>,
    space: SpaceId,
    gateway: PeerId,
    pin: Option<CheckpointPin>,
) -> Result<()> {
    let Some(pin) = pin else {
        return Ok(());
    };
    // 받아 준 노드는 응답 뒤에야 나를 인증하므로, 묻기 전에 인증을 맞춥니다(멤버만 답함).
    if let Err(e) = crate::peers::ensure_auth(node, gateway).await {
        debug!(error = %e, "받아 준 노드와 인증하지 못함");
    }
    let reply = match crate::p2p::request(
        node,
        gateway,
        YpRequest::GetCheckpoint {
            space,
            from_seq: pin.seq,
        },
    )
    .await
    {
        Ok(YpResponse::Checkpoint(r)) => *r,
        Ok(r) => {
            return Err(Error::Network(format!(
                "받아 준 노드가 체크포인트에 답하지 않음: {}",
                response_kind(&r)
            )));
        }
        Err(e) => return Err(Error::Network(e.to_string())),
    };
    let Some(head) = reply.head else {
        return Err(Error::invalid("받아 준 노드가 링크의 체크포인트를 모름"));
    };
    let (Some(event), Some(manifest)) = (reply.event, reply.manifest) else {
        return Err(Error::Network("체크포인트 이벤트·스냅숏이 빠진 답".into()));
    };
    let checked = validate_chain(&pin, &reply.chain, None)
        .map_err(|e| Error::invalid(format!("체크포인트 사슬: {e}")))?;
    let last = reply.chain.last().expect("확인한 사슬은 비지 않음");
    if checked != head || last.body.space != space {
        // 사슬이 상한에서 끊김(아주 오래된 링크): 알려 준 머리까지 잇지 못했지만 확인한 끝으로 시작하면 그 뒤 체크포인트는
        // 해소기가 이어서 받습니다.
        if last.body.space != space {
            return Err(Error::invalid("다른 공간의 체크포인트 사슬"));
        }
    }
    if event.header.space != space
        || event.header.kind != EventKind::StateCheckpoint
        || event.verify().is_err()
        || manifest.id() != last.body.snapshot
        || manifest.size != last.body.snapshot_len
        || manifest.size > MAX_SNAPSHOT_LEN
    {
        return Err(Error::invalid(
            "체크포인트 이벤트·스냅숏 매니페스트가 사슬과 다름",
        ));
    }
    let stash = Stash {
        chain: reply.chain,
        event,
        manifest,
        sources: vec![gateway.to_bytes()],
    };
    // 경계를 먼저 세워, 묶음을 적용하는 동안 경계 아래 부모를 따라가 받지 않게 합니다.
    let f = stash.floor();
    put_bin(node, &stash_key(&space), &stash)?;
    node.checkpoints.stash.lock().insert(space, Arc::new(stash));
    node.checkpoints
        .stash_since
        .lock()
        .insert(space, Instant::now());
    {
        let mut m = node.checkpoints.floors.write();
        let cur = m.get(&space).copied().unwrap_or(0);
        m.insert(space, cur.max(f));
    }
    if let Err(e) = fetch_snapshot(node, space, gateway).await {
        debug!(error = %e, "스냅숏을 아직 다 받지 못함(감시 작업이 다시 받음)");
    }
    Ok(())
}

/// 받아 둔 체크포인트를 버리고 경계를 해소기 것으로 되돌립니다(처음부터 따라잡음). 적용 잠금 안에서 부릅니다. 그새 다른
/// 것으로 바뀌었으면 그대로 둡니다.
fn abandon_stash(node: &Node, space: &SpaceId, stash: &Arc<Stash>, why: &str) {
    {
        let mut m = node.checkpoints.stash.lock();
        if !m.get(space).is_some_and(|s| Arc::ptr_eq(s, stash)) {
            return;
        }
        m.remove(space);
    }
    warn!(space = %space.short(), why, "받아 둔 체크포인트를 버리고 처음부터 따라잡음");
    node.checkpoints.stash_since.lock().remove(space);
    // 경계 아래로 기억해 둔 ID도 이제 받아야 합니다.
    node.checkpoints.below.lock().remove(space);
    let _ = node.store.delete_setting(&stash_key(space));
    let f = core_of(node, space).map_or(0, |c| c.lock().resolver.floor());
    set_floor(node, space, f);
}

fn response_kind(r: &YpResponse) -> String {
    match r {
        YpResponse::Error { code, .. } => format!("{code:?}"),
        _ => "예상하지 못한 응답".into(),
    }
}

/// 받아 둔 체크포인트의 스냅숏 블롭을 피어에게서 받습니다(조각마다 해시 확인).
async fn fetch_snapshot(node: &Arc<Node>, space: SpaceId, peer: PeerId) -> Result<()> {
    let stash = node
        .checkpoints
        .stash
        .lock()
        .get(&space)
        .cloned()
        .ok_or_else(|| Error::not_found("받아 둔 체크포인트 없음"))?;
    let m = &stash.manifest;
    let pb = PersistentBlob::new(m.clone(), SecurityLevel::Normal)?;
    let blob = pb.id();
    if blob_complete(node, &blob) {
        return Ok(());
    }
    node.store.blob_register(&space, &pb)?;
    node.store.blob_set_pinned(&blob, true)?;
    let have = node
        .store
        .blob_have(&blob)?
        .map(|(bm, _)| bm)
        .unwrap_or_default();
    let missing: Vec<u32> = (0..m.chunks.len() as u32)
        .filter(|i| {
            let (byte, bit) = ((*i / 8) as usize, i % 8);
            have.get(byte).is_none_or(|b| b & (1 << bit) == 0)
        })
        .collect();
    use futures::StreamExt;
    let results: Vec<Result<()>> = futures::stream::iter(missing)
        .map(|index| {
            let node = node.clone();
            async move {
                let req = YpRequest::BlobChunk { space, blob, index };
                match crate::p2p::request(&node, peer, req).await {
                    Ok(YpResponse::Chunk { data }) => {
                        node.store.blob_write_chunk(&blob, index, &data)?;
                        Ok(())
                    }
                    Ok(r) => Err(Error::Network(format!(
                        "스냅숏 조각을 받지 못함: {}",
                        response_kind(&r)
                    ))),
                    Err(e) => Err(Error::Network(e.to_string())),
                }
            }
        })
        .buffer_unordered(SNAPSHOT_FETCH_PARALLEL)
        .collect()
        .await;
    results.into_iter().collect::<Result<Vec<()>>>()?;
    Ok(())
}

/// 받아 둔 체크포인트를 설치합니다(상태 키가 왔을 때). 체크포인트 이벤트를 풀어 사슬 머리와 같은지 보고, 스냅숏을 풀어
/// 해소기를 그 체크포인트로 세운 뒤 저장된 공간 스트림 이벤트(경계 위)를 다시 넣습니다. 설치했으면 true.
pub(crate) async fn try_install(node: &Arc<Node>, space: SpaceId) -> Result<bool> {
    let Some(stash) = node.checkpoints.stash.lock().get(&space).cloned() else {
        return Ok(false);
    };
    let head = stash.head().clone();
    let blob = head.body.snapshot;
    if !blob_complete(node, &blob) {
        let mut last = None;
        for src in &stash.sources {
            if let Ok(peer) = PeerId::from_bytes(src) {
                match fetch_snapshot(node, space, peer).await {
                    Ok(()) => break,
                    Err(e) => last = Some(e),
                }
            }
        }
        if !blob_complete(node, &blob) {
            let since = *node
                .checkpoints
                .stash_since
                .lock()
                .entry(space)
                .or_insert_with(Instant::now);
            if since.elapsed() >= STASH_GIVE_UP {
                let _l = ingest::lock_space(node, space).await;
                abandon_stash(node, &space, &stash, "스냅숏을 받지 못함");
            }
            return match last {
                Some(e) => Err(e),
                None => Ok(false),
            };
        }
    }
    let KeyRef::State(kid) = stash.event.header.key else {
        return Err(Error::invalid("체크포인트 이벤트의 키가 상태 키가 아님"));
    };
    let genesis =
        spaces::load_genesis(node, &space)?.ok_or_else(|| Error::conflict("시작 이벤트를 모름"))?;
    let l = ingest::lock_space(node, space).await;
    // 가입·감시 작업·주기 작업이 겹쳐 부를 수 있습니다: 잠금을 쥔 뒤 아직 받아 둔 그대로인지 봅니다.
    if !node
        .checkpoints
        .stash
        .lock()
        .get(&space)
        .is_some_and(|s| Arc::ptr_eq(s, &stash))
    {
        return Ok(false);
    }
    let core = core_of(node, &space).ok_or_else(|| Error::not_found("없는 공간"))?;
    let key = match keys::crew_state_key(node, &space, &kid)? {
        Some(k) => k,
        None => match core.lock().provisional_state_key(&kid) {
            Some(k) => k,
            None => return Ok(false),
        },
    };
    let built = (|| -> Result<Resolver> {
        let sc = decode_checkpoint(&key, &stash.event)?;
        if sc != head {
            return Err(Error::invalid(
                "체크포인트 이벤트가 확인한 사슬 머리와 다름",
            ));
        }
        let (manifest, ct) =
            read_blob(node, &blob).ok_or_else(|| Error::conflict("스냅숏 없음"))?;
        let plain = open_snapshot(&key, head.body.seq, &head.body.state_hash, &manifest, &ct)?;
        let decoded = DecodedEvent {
            id: stash.event.id(),
            header: stash.event.header.clone(),
            body: EventBody::StateCheckpoint(Box::new(sc)),
        };
        Resolver::from_checkpoint(genesis, decoded, &plain)
            .map_err(|e| Error::invalid(e.to_string()))
    })();
    let resolver = match built {
        Ok(r) => r,
        Err(e) => {
            // 키까지 있는데 맞지 않음: 받아 둔 체크포인트를 버리고 처음부터 받습니다(전체 기록은 해소기가 스스로 확인).
            abandon_stash(node, &space, &stash, "받아 둔 체크포인트를 쓸 수 없음");
            return Err(e);
        }
    };
    let f = head.body.frontier_lamport;
    // 체크포인트 이벤트는 저장해 두어야 재시작·답하기에 씁니다.
    let pe = yangpa_store::PersistentEvent::space_level(stash.event.clone())?;
    node.store.insert_events(&[pe])?;
    // 키·대상을 기다리던 이벤트(`waiting`)는 그대로 둡니다: 공간 스트림 것은 아래에서 다시 넣고, 채널 것은 키가 오면 깹니다.
    core.lock().resolver = resolver;
    node.checkpoints.floors.write().insert(space, f);
    let evs: Vec<SignedEvent> = node
        .store
        .events_by_lane_from(&space, &[Lane::State, Lane::KeyAgreement, Lane::Key], f)?
        .into_iter()
        .filter(|e| e.header.stream() == StreamId::Space)
        .collect();
    let report = ingest::ingest_stored_locked(node, &l, space, evs, Source::Replay);
    node.checkpoints.stash.lock().remove(&space);
    node.checkpoints.stash_since.lock().remove(&space);
    drop(l);
    let report = report?;
    let _ = node.store.delete_setting(&stash_key(&space));
    save_chain(node, &space, &stash.chain)?;
    if let Some(base) = core.lock().resolver.base() {
        record_base(node, &space, &base, kid)?;
    }
    prune_missing(node, &space, &head.body);
    let sources = stash
        .sources
        .iter()
        .filter_map(|b| PeerId::from_bytes(b).ok())
        .collect();
    mark_unconfirmed(node, space, sources);
    info!(space = %space.short(), seq = head.body.seq, "체크포인트로 공간 상태를 세움");
    ingest::after_unlock(node, space, &report);
    crate::cards::need_pull(node, space);
    node.notify
        .publish(yangpa_rpc::api::Notification::SpaceUpdated { space });
    Ok(true)
}

/// 체크포인트로 들어온 상태를 다른 출처와 견주는 중으로 표시합니다(저장해 재시작해도 이어 견줌). 그동안 이 공간에
/// 이벤트를 쓰지 않습니다([`is_unconfirmed`]).
fn mark_unconfirmed(node: &Node, space: SpaceId, sources: Vec<PeerId>) {
    let saved = SavedUnconfirmed {
        since_ms: yangpa_core::time::Timestamp::now().0,
        sources: sources.iter().map(|p| p.to_bytes()).collect(),
    };
    if let Err(e) = put_bin(node, &unconfirmed_key(&space), &saved) {
        debug!(error = %e, "견줄 체크포인트 기록을 저장하지 못함");
    }
    node.checkpoints.unconfirmed.lock().insert(
        space,
        Unconfirmed {
            since: Instant::now(),
            sources,
        },
    );
}

/// 재시작: 저장해 둔 견줄 기록을 다시 올립니다(지난 시간도 셈).
fn reload_unconfirmed(node: &Node, space: &SpaceId) {
    let Some(saved) = get_bin::<SavedUnconfirmed>(node, &unconfirmed_key(space)) else {
        return;
    };
    let ago = yangpa_core::time::Timestamp::now()
        .0
        .saturating_sub(saved.since_ms);
    let since = Instant::now()
        .checked_sub(Duration::from_millis(ago))
        .unwrap_or_else(Instant::now);
    let sources = saved
        .sources
        .iter()
        .filter_map(|b| PeerId::from_bytes(b).ok())
        .collect();
    node.checkpoints
        .unconfirmed
        .lock()
        .insert(*space, Unconfirmed { since, sources });
}

fn clear_unconfirmed(node: &Node, space: &SpaceId) {
    node.checkpoints.unconfirmed.lock().remove(space);
    let _ = node.store.delete_setting(&unconfirmed_key(space));
}

/// 체크포인트로 들어온 뒤 받아 준 노드가 아닌 피어(최대 [`COMPARE_MEMBERS`])에게 최신 체크포인트를 물어 머리를
/// 견줍니다([`compare_heads`]). 받아 준 노드 하나는 링크 고정 뒤의 소유권 이전을 숨긴 사슬을 보일 수 있으므로:
/// - 같거나 이어지는 머리를 하나라도 받고 갈라진 답이 없으면 확인을 마칩니다.
/// - 같은 순번에 서명이 맞는 다른 체크포인트가 보이면 받아 둔 상태를 버리고 처음부터 따라잡습니다(전체 기록은
///   해소기가 시작 이벤트부터 확인).
/// - [`CheckpointOptions::second_source_wait`] 안에 확인하지 못해도 처음부터 따라잡습니다.
///
/// 물을 피어는 설치한 상태의 멤버로 거르지 않습니다(지어낸 상태가 정직한 멤버를 빼고 자기 편만 남길 수 있음). 그래서
/// 이 공간 ID를 인증된 다른 피어에게도 묻게 되지만, 멤버가 아니면 답하지 않습니다. 이어졌지만 아직 인증하지 않은
/// 피어에게는 먼저 인증을 겁니다(확인 전에만).
///
/// [`CheckpointOptions::second_source_wait`]: crate::config::CheckpointOptions::second_source_wait
pub(crate) async fn confirm(node: &Arc<Node>, space: SpaceId) {
    let Some(u) = node.checkpoints.unconfirmed.lock().get(&space).cloned() else {
        return;
    };
    let Some(mine) = saved_base(node, &space) else {
        clear_unconfirmed(node, &space);
        return;
    };
    let Ok(me) = node.me() else { return };
    // 인증된 피어를 먼저, 그다음 이어졌지만 아직 인증하지 않은 피어(새 멤버는 받아 준 노드 말고는 아는 피어가 없어,
    // 다른 멤버와 이어져도 서로 인증하지 않은 채로 남음).
    let mut peers: Vec<PeerId> = node
        .peers
        .authed_peers()
        .into_iter()
        .filter(|(p, _, d)| *d != me.device_id() && !u.sources.contains(p))
        .map(|(p, _, _)| p)
        .collect();
    peers.extend(
        node.peers
            .connected_peers()
            .into_iter()
            .filter(|p| node.peers.identity_of(p).is_none() && !u.sources.contains(p)),
    );
    let mut agreed = 0;
    let mut asked = 0;
    for peer in peers.into_iter().take(COMPARE_MEMBERS * 2) {
        if asked >= COMPARE_MEMBERS {
            break;
        }
        match crate::peers::ensure_auth(node, peer).await {
            Ok((_, d)) if d != me.device_id() => {}
            Ok(_) => continue,
            Err(e) => {
                debug!(space = %space.short(), %peer, error = %e, "머리를 견줄 피어와 인증하지 못함");
                continue;
            }
        }
        asked += 1;
        let req = YpRequest::GetCheckpoint {
            space,
            from_seq: mine.pin.seq,
        };
        let r = match crate::p2p::request(node, peer, req).await {
            Ok(YpResponse::Checkpoint(r)) => r,
            Ok(YpResponse::Error { code, .. }) => {
                debug!(space = %space.short(), %peer, ?code, "머리를 견줄 답 대신 오류");
                continue;
            }
            Ok(_) => continue,
            Err(e) => {
                debug!(space = %space.short(), %peer, error = %e, "머리를 묻지 못함");
                continue;
            }
        };
        let Some(head) = r.head else { continue };
        let agreement = compare_heads(&mine.pin, &space, &head, &r.chain);
        debug!(space = %space.short(), %peer, mine = mine.pin.seq, theirs = head.seq, ?agreement, "체크포인트 머리 견줌");
        match agreement {
            HeadAgreement::Same | HeadAgreement::Extends => agreed += 1,
            HeadAgreement::Conflict => {
                fall_back(
                    node,
                    space,
                    "다른 출처가 같은 순번의 다른 체크포인트를 가짐",
                )
                .await;
                return;
            }
            HeadAgreement::Unknown => {}
        }
    }
    if agreed > 0 {
        clear_unconfirmed(node, &space);
        debug!(space = %space.short(), agreed, "들어온 뒤 다른 출처와 체크포인트 머리가 같음");
        // 확인 전에 미뤄 둔 키 일(바퀴·표 넘기기)을 다시 봅니다.
        crate::keygroup::on_state_changed(node, space);
    } else if u.since.elapsed() >= node.cfg.checkpoint.second_source_wait {
        fall_back(
            node,
            space,
            "다른 출처에게서 체크포인트 머리를 확인하지 못함",
        )
        .await;
    }
}

/// 체크포인트로 세운 상태를 버리고 처음부터 따라잡습니다: 해소기를 시작 이벤트로 되돌리고, 바탕 기록·사슬·경계를
/// 지우고, 저장된 공간 스트림 이벤트를 다시 넣은 뒤 피어들과 따라잡습니다. 체크포인트는 해소기가 전체 기록으로 다시
/// 확인해 받아들입니다.
async fn fall_back(node: &Arc<Node>, space: SpaceId, why: &'static str) {
    warn!(space = %space.short(), why, "체크포인트로 세운 상태를 버리고 처음부터 따라잡음");
    alert(node, space, CheckpointAlertKind::Fallback);
    let genesis = match spaces::load_genesis(node, &space) {
        Ok(Some(g)) => g,
        _ => return,
    };
    let l = ingest::lock_space(node, space).await;
    let Some(core) = core_of(node, &space) else {
        return;
    };
    clear_unconfirmed(node, &space);
    core.lock().resolver = Resolver::with_genesis(genesis);
    let _ = node.store.delete_setting(&base_key(&space));
    for (k, _) in node
        .store
        .settings_with_prefix(&chain_prefix(&space))
        .unwrap_or_default()
    {
        let _ = node.store.delete_setting(&k);
    }
    {
        let c = &node.checkpoints;
        c.floors.write().remove(&space);
        c.below.lock().remove(&space);
        c.proofs.lock().remove(&space);
        c.watch.lock().remove(&space);
    }
    let evs = match node.store.events_by_lane_from(
        &space,
        &[Lane::State, Lane::KeyAgreement, Lane::Key],
        0,
    ) {
        Ok(evs) => evs
            .into_iter()
            .filter(|e| e.header.stream() == StreamId::Space)
            .collect(),
        Err(e) => {
            debug!(error = %e, "다시 넣을 이벤트를 읽지 못함");
            Vec::new()
        }
    };
    let report = ingest::ingest_stored_locked(node, &l, space, evs, Source::Replay);
    drop(l);
    match report {
        Ok(r) => ingest::after_unlock(node, space, &r),
        Err(e) => debug!(error = %e, "처음부터 다시 넣지 못함"),
    }
    for (peer, _, _) in node.peers.authed_peers() {
        sync_service::spawn_catch_up(node, space, peer);
    }
    node.notify
        .publish(yangpa_rpc::api::Notification::SpaceUpdated { space });
}

/// 체크포인트 경보를 앱에 알립니다(갈라진 체크포인트, 처음부터 따라잡기).
pub(crate) fn alert(node: &Node, space: SpaceId, kind: CheckpointAlertKind) {
    node.notify
        .publish(yangpa_rpc::api::Notification::CheckpointAlert { space, kind });
}

/// 가입 때 체크포인트를 받지 못함: 상태를 풀기 전까지 감시 작업이 다시 묻습니다([`retry_prepare`]). 고정이 없는
/// 링크는 처음부터 받으므로 다시 묻지 않습니다.
pub(crate) fn prepare_later(
    node: &Node,
    space: SpaceId,
    gateway: PeerId,
    pin: Option<CheckpointPin>,
) {
    if let Some(pin) = pin {
        node.checkpoints.retry.lock().insert(space, (gateway, pin));
    }
}

/// [`prepare_later`]로 남긴 공간이면 체크포인트를 다시 묻습니다. 그새 처음부터 상태를 풀었으면 그만둡니다.
pub(crate) async fn retry_prepare(node: &Arc<Node>, space: SpaceId) {
    let Some((gateway, pin)) = node.checkpoints.retry.lock().get(&space).copied() else {
        return;
    };
    if node.space_state(&space).is_some() || has_stash(node, &space) {
        node.checkpoints.retry.lock().remove(&space);
        return;
    }
    match prepare_join(node, space, gateway, Some(pin)).await {
        Ok(()) => {
            node.checkpoints.retry.lock().remove(&space);
        }
        Err(e) => debug!(error = %e, "가입 체크포인트를 다시 묻지 못함"),
    }
}

/// 가입 중 받아 둔 체크포인트가 있는지(감시 작업이 설치를 시도함).
pub(crate) fn has_stash(node: &Node, space: &SpaceId) -> bool {
    node.checkpoints.stash.lock().contains_key(space)
}

/// 체크포인트로 들어와 아직 다른 출처와 머리를 견주지 못했는지. 그동안 이 공간에 이벤트를 쓰지 않습니다(지어낸
/// 상태로 키를 봉인하거나 멤버십을 바꾸지 않게).
pub(crate) fn is_unconfirmed(node: &Node, space: &SpaceId) -> bool {
    node.checkpoints.unconfirmed.lock().contains_key(space)
}

/// 가입 묶음에 넣을 경계(받아 준 쪽). 바탕이 없으면 `None`.
pub(crate) fn bootstrap_floor(node: &Node, space: &SpaceId) -> Option<(u64, EventId)> {
    saved_base(node, space).map(|b| (b.floor, b.event))
}
