//! 보관 쪽(`--archive`): 지정 수락, 명단 받기, 이벤트·첨부 받아 두기와 내주기, 삭제 반영, 해제.
//!
//! 받아 둔 공간은 `node.spaces`에 넣지 않습니다(멤버가 아니고 키가 없어 상태를 풀 수 없음). 이벤트는 같은 저장소의
//! `events` 표에 일반 등급(`PersistentEvent`)으로 넣고, 기록(명단·첨부 쓰임·사용량)은 설정 표에 둡니다.
//! 키 없이 할 수 있는 검사만 합니다: 서명·본문 해시·공간·시작 이벤트, 논리 시계 부풀림, 명단의 채널, 보낸 기기, 속도·용량.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine;
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use tracing::{debug, info};
use yangpa_core::archive::{
    ArchiveBlobRef, ArchiveDesignate, ArchiveRequest, ArchiveRoster, MAX_ANCHOR_AHEAD,
    MAX_PRUNE_EVENTS, MAX_REF_BLOBS, MAX_REF_EVENTS, SignedArchiveRoster,
};
use yangpa_core::event::{EventKind, KeyRef, Lane, SignedEvent, StreamId};
use yangpa_core::ids::{BlobId, ChannelId, DeviceId, EventId, SpaceId, UserId};
use yangpa_core::links::{ArchiveOfferLink, token_hash};
use yangpa_core::model::SecurityLevel;
use yangpa_core::time::Timestamp;
use yangpa_core::wire::{
    LamportRange, MAX_EVENTS_PER_MESSAGE, PeerAuth, RangeFingerprint, WireErrorCode, YpRequest,
    YpResponse,
};
use yangpa_crypto::aead::{NONCE_LEN, TAG_LEN};
use yangpa_net::PeerId;
use yangpa_store::{MAX_EXPIRE_BATCH, PersistentBlob, PersistentEvent, StoreError};
use yangpa_sync::archive::{
    DeltaVerdict, HostedRoster, RosterLocal, RosterVerdict, accept_delta, accept_initial,
    accept_roster, archive_may_read_scoped, check_archive_serve,
};
use yangpa_sync::policy::{BUSY_RETRY_MS, TokenBucket};
use yangpa_sync::reconcile::{
    FingerprintSource, MemorySet, ReconcileRequest, ReconcileResponse, Reconciler,
    answer_fingerprints, answer_ids, full_range,
};
use yangpa_sync::swarm::Bitmap;

use crate::error::{Error, Result};
use crate::identity;
use crate::node::Node;
use crate::policy;

const HOSTED_PREFIX: &str = "archive.hosted.";
const OFFER_PREFIX: &str = "archive.offer.";
/// 첨부 쓰임(`archive.ref.<공간>.<블롭>`): 그 블롭을 실은 메시지.
const REF_PREFIX: &str = "archive.ref.";
/// 메시지마다 알린 블롭(`archive.evref.<공간>.<메시지>`): 메시지당 블롭 수를 묶고, 지울 때 바로 찾으려고.
const EVREF_PREFIX: &str = "archive.evref.";
/// 상태 체크포인트 스냅숏(`archive.snap.<공간>`): (체크포인트 이벤트, 논리 시계, 블롭), 최근 것부터.
const SNAP_PREFIX: &str = "archive.snap.";
/// 남겨 두는 스냅숏 수(멤버와 같음: 새 체크포인트가 퍼지는 동안 앞 것으로 들어오는 가입자도 받게).
const KEEP_SNAPSHOTS: usize = 2;
/// 보낸 기기마다 받는 속도(바이트/초).
const PUSH_RATE_BPS: u64 = 4 * 1024 * 1024;
/// 보낸 기기마다 첨부 알림·삭제 반영 요청을 받는 속도와 한 번에 몰아 받을 수 있는 양(요청 크기로 셈).
const CTL_RATE_BPS: u64 = 32 * 1024;
const CTL_BURST_BYTES: u64 = 256 * 1024;
/// 블롭 하나에 기록하는 메시지 수.
const MAX_REFS_PER_BLOB: usize = 8;
/// 동시에 살아 있는 제안 링크 수.
const MAX_OFFERS: usize = 64;
const MAX_AUTH_SKEW_MS: u64 = 5 * 60 * 1000;
/// 부모 따라가기 최대 단계(넘으면 지문 비교).
const CHASE_ROUNDS: usize = 8;
/// 조각 하나를 바쁜 피어에게 다시 묻는 횟수와 최대 대기.
const BUSY_RETRIES: u32 = 3;
const MAX_BUSY_WAIT: Duration = Duration::from_secs(2);
/// 못 받은 첨부를 다시 시도하는 횟수와 간격(두 배씩, 최대). 다 쓰면 새 메시지가 그 첨부를 알릴 때까지 쉽니다.
const MAX_BLOB_ATTEMPTS: u32 = 8;
const BLOB_RETRY_START: Duration = Duration::from_secs(30);
const BLOB_RETRY_MAX: Duration = Duration::from_secs(30 * 60);
/// 명단에서 빠진 채널을 지우기 전에 기다리는 시간. 더 새 순서의 통째 명단도 빠졌다고 하면 바로 지웁니다.
const ERASE_GRACE: Duration = Duration::from_secs(72 * 60 * 60);
/// 사용량을 저장소에서 다시 세는 간격(지운 본문·첨부를 빼고 어긋남을 바로잡음).
const RECOUNT_EVERY: Duration = Duration::from_secs(10 * 60);
/// 이벤트 하나의 헤더 밖 부담(혼합 서명)으로 치는 바이트. 다시 셀 때(`LENGTH(sig)`)와 같아야 사용량이 어긋나지 않습니다.
const EVENT_OVERHEAD: u64 = yangpa_core::sign::Sig::LEN as u64;
/// 저장소가 본문을 봉인할 때 붙는 논스·태그.
const SEAL_OVERHEAD: u64 = (NONCE_LEN + TAG_LEN) as u64;
/// 제안 링크 기본·최대 유효 시간.
pub(crate) const OFFER_DEFAULT_TTL: Duration = Duration::from_secs(24 * 60 * 60);
pub(crate) const OFFER_MAX_TTL: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// 명단에서 빠진 채널(지우기 대기).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Dropped {
    since: Timestamp,
    /// 그 채널이 빠졌다고 한 첫 명단의 순서.
    anchor: u64,
}

/// 받아 둔 공간 기록(설정 표에 postcard·base64로).
#[derive(Clone, Debug, Serialize, Deserialize)]
struct HostedRecord {
    rec: HostedRoster,
    designated_at: Timestamp,
    designated_by: UserId,
    used_bytes: u64,
    dropped: BTreeMap<ChannelId, Dropped>,
}

struct Hosted {
    rec: Arc<HostedRoster>,
    designated_at: Timestamp,
    designated_by: UserId,
    /// 받아 둔 이벤트(모든 레인의 헤더·본문)와 첨부 바이트(용량 상한 계산).
    used_bytes: u64,
    dropped: BTreeMap<ChannelId, Dropped>,
    /// 저장하지 않은 사용량 변화가 있음.
    dirty: bool,
    /// 다음 주기에 저장소에서 사용량을 다시 셈(지운 것이 있음).
    recount: bool,
    last_recount: Instant,
}

/// 첨부 쓰임 하나.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct HostRef {
    event: EventId,
    channel: ChannelId,
    lamport: u64,
}

/// 제안 링크(토큰 해시로 찾음).
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Offer {
    created_at: Timestamp,
    expires_at: Option<Timestamp>,
}

/// 못 받은 블롭의 다시 시도 상태.
#[derive(Clone, Copy, Debug)]
struct Retry {
    attempts: u32,
    next: Instant,
}

impl Retry {
    fn now() -> Self {
        Retry {
            attempts: 0,
            next: Instant::now(),
        }
    }
}

#[derive(Default)]
pub(super) struct HostState {
    hosted: RwLock<HashMap<SpaceId, Hosted>>,
    /// 보낸 기기마다 받는 속도 제한.
    buckets: Mutex<HashMap<DeviceId, TokenBucket>>,
    /// 보낸 기기마다 첨부 알림·삭제 반영 요청 속도 제한.
    ctl_buckets: Mutex<HashMap<DeviceId, TokenBucket>>,
    /// 진행 중인 (공간, 피어) 당겨오기와 다시 돌릴지.
    pulling: Mutex<HashMap<(SpaceId, PeerId), bool>>,
    /// 받는 중인 블롭.
    fetching: Mutex<HashSet<BlobId>>,
    /// 다 받지 못한 블롭(주기 작업이 물러나며 다시 시도, [`MAX_BLOB_ATTEMPTS`]번이면 그만).
    pending: Mutex<HashMap<(SpaceId, BlobId), Retry>>,
    /// 지정·명단·해제를 한 줄로(같은 공간 기록이 엇갈리지 않게).
    lock: Mutex<()>,
}

/// 받아 둔 공간 하나의 보기(`archive.hosted`).
pub(crate) struct HostedInfo {
    pub space: SpaceId,
    pub owner: UserId,
    pub members: u32,
    pub channels: u32,
    pub events: u64,
    pub blobs: u32,
    pub used_bytes: u64,
    pub designated_at: Timestamp,
    pub designated_by: UserId,
    pub roster_issued_at: Timestamp,
}

fn now_ms() -> u64 {
    Timestamp::now().0
}

fn hosted_key(space: &SpaceId) -> String {
    format!("{HOSTED_PREFIX}{space}")
}

fn ref_prefix(space: &SpaceId) -> String {
    format!("{REF_PREFIX}{space}.")
}

fn ref_key(space: &SpaceId, blob: &BlobId) -> String {
    format!("{REF_PREFIX}{space}.{blob}")
}

fn evref_prefix(space: &SpaceId) -> String {
    format!("{EVREF_PREFIX}{space}.")
}

fn evref_key(space: &SpaceId, event: &EventId) -> String {
    format!("{EVREF_PREFIX}{space}.{event}")
}

fn snap_key(space: &SpaceId) -> String {
    format!("{SNAP_PREFIX}{space}")
}

fn offer_key(hash: &[u8; 32]) -> String {
    format!("{OFFER_PREFIX}{}", bs58::encode(hash).into_string())
}

fn encode_record(r: &HostedRecord) -> Result<String> {
    let bytes = postcard::to_stdvec(r).map_err(|_| Error::internal("보관 기록 직렬화 실패"))?;
    Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
}

fn decode_record(s: &str) -> Option<HostedRecord> {
    let bytes = base64::engine::general_purpose::STANDARD.decode(s).ok()?;
    postcard::from_bytes(&bytes).ok()
}

fn save(node: &Node, space: &SpaceId, h: &Hosted) -> Result<()> {
    let r = HostedRecord {
        rec: (*h.rec).clone(),
        designated_at: h.designated_at,
        designated_by: h.designated_by,
        used_bytes: h.used_bytes,
        dropped: h.dropped.clone(),
    };
    node.store
        .put_setting(&hosted_key(space), &encode_record(&r)?)?;
    Ok(())
}

/// 저장된 기록을 읽고, 다 받지 못한 첨부를 다시 받을 목록에 넣습니다.
pub(super) fn load(node: &Arc<Node>) {
    let rows = node
        .store
        .settings_with_prefix(HOSTED_PREFIX)
        .unwrap_or_default();
    let mut hosted = HashMap::new();
    for (key, v) in rows {
        let Ok(space) = key[HOSTED_PREFIX.len()..].parse::<SpaceId>() else {
            continue;
        };
        let Some(r) = v.as_str().and_then(decode_record) else {
            debug!(space = %space.short(), "보관 기록을 읽을 수 없음");
            continue;
        };
        hosted.insert(
            space,
            Hosted {
                rec: Arc::new(r.rec),
                designated_at: r.designated_at,
                designated_by: r.designated_by,
                used_bytes: r.used_bytes,
                dropped: r.dropped,
                dirty: false,
                // 재시작 뒤 한 번은 저장소에서 다시 셉니다.
                recount: true,
                last_recount: Instant::now(),
            },
        );
    }
    let spaces: Vec<SpaceId> = hosted.keys().copied().collect();
    *node.archive.host.hosted.write() = hosted;
    let mut pending = node.archive.host.pending.lock();
    for space in spaces {
        for (blob, _) in refs_in_space(node, &space) {
            if !node
                .store
                .blob_status(&blob)
                .ok()
                .flatten()
                .is_some_and(|s| s.complete)
            {
                pending.insert((space, blob), Retry::now());
            }
        }
    }
}

// ───────────── 조회 ─────────────

pub(super) fn is_hosted(node: &Node, space: &SpaceId) -> bool {
    node.archive.host.hosted.read().contains_key(space)
}

/// 해제·`archive.drop`과 엇갈려 받아 둔 공간이 아니게 된 뒤 쓴 것이 남았는지(멤버 공간은 건드리지 않음).
fn orphaned(node: &Node, space: &SpaceId) -> bool {
    !is_hosted(node, space) && !node.spaces.read().contains_key(space)
}

fn roster_of(node: &Node, space: &SpaceId) -> Option<Arc<HostedRoster>> {
    node.archive
        .host
        .hosted
        .read()
        .get(space)
        .map(|h| h.rec.clone())
}

/// 받아 둔 공간 중 하나의 명단에 있는 멤버 기기인지.
pub(super) fn trusts(node: &Node, user: &UserId, device: &DeviceId) -> bool {
    node.archive
        .host
        .hosted
        .read()
        .values()
        .any(|h| h.rec.roster.has_device(user, device))
}

pub(super) fn hosted_info(node: &Node) -> Vec<HostedInfo> {
    let rows: Vec<(SpaceId, Arc<HostedRoster>, Timestamp, UserId, u64)> = node
        .archive
        .host
        .hosted
        .read()
        .iter()
        .map(|(s, h)| {
            (
                *s,
                h.rec.clone(),
                h.designated_at,
                h.designated_by,
                h.used_bytes,
            )
        })
        .collect();
    let mut out: Vec<HostedInfo> = rows
        .into_iter()
        .map(|(space, rec, designated_at, designated_by, used_bytes)| {
            let r = &rec.roster;
            HostedInfo {
                space,
                owner: r.owner,
                members: r.members.len() as u32,
                channels: r.channels.len() as u32,
                events: node.store.event_count(&space).unwrap_or(0),
                blobs: refs_in_space(node, &space).len() as u32,
                used_bytes,
                designated_at,
                designated_by,
                roster_issued_at: r.issued_at,
            }
        })
        .collect();
    out.sort_by_key(|h| h.designated_at);
    out
}

/// 시험용: 이 공간 명단의 멤버 사용자.
pub(super) fn roster_members(node: &Node, space: &SpaceId) -> Option<Vec<UserId>> {
    roster_of(node, space).map(|r| r.roster.members.iter().map(|m| m.user).collect())
}

pub(super) fn roster_since(node: &Node, space: &SpaceId, user: &UserId) -> Option<Option<u64>> {
    roster_of(node, space).and_then(|r| r.roster.member(user).map(|m| m.since))
}

/// 시험용: 지금 명단의 보관 채널.
pub(super) fn roster_channels(node: &Node, space: &SpaceId) -> Option<Vec<ChannelId>> {
    roster_of(node, space).map(|r| r.roster.channels.iter().map(|c| c.channel).collect())
}

/// 시험용: 이 공간의 사용량(바이트).
pub(super) fn used_bytes(node: &Node, space: &SpaceId) -> Option<u64> {
    node.archive
        .host
        .hosted
        .read()
        .get(space)
        .map(|h| h.used_bytes)
}

// ───────────── 요청 나누기 ─────────────

/// 동기화·블롭 요청의 공간.
pub(super) fn request_space(req: &YpRequest) -> Option<SpaceId> {
    match req {
        YpRequest::PushEvents { space, .. }
        | YpRequest::GetHeads { space, .. }
        | YpRequest::GetFingerprints { space, .. }
        | YpRequest::GetIds { space, .. }
        | YpRequest::GetEvents { space, .. }
        | YpRequest::BlobManifest { space, .. }
        | YpRequest::BlobHave { space, .. }
        | YpRequest::BlobChunk { space, .. } => Some(*space),
        _ => None,
    }
}

pub(super) async fn handle_request(
    node: &Arc<Node>,
    peer: PeerId,
    req: ArchiveRequest,
) -> YpResponse {
    let Ok(me) = node.me() else {
        return YpResponse::error(WireErrorCode::Internal, "신원 없음");
    };
    let me = me.device_id();
    match req {
        ArchiveRequest::Designate(d) => designate(node, peer, me, *d),
        ArchiveRequest::Release { space, auth } => release(node, peer, space, &auth),
        ArchiveRequest::Roster(r) => roster_update(node, peer, me, *r),
        ArchiveRequest::BlobRefs { space, refs } => blob_refs(node, peer, space, refs),
        ArchiveRequest::Prune { space, events } => prune(node, peer, space, events),
        ArchiveRequest::RosterDelta(d) => roster_delta(node, peer, me, *d),
    }
}

/// 인증된 요청자가 명단의 멤버 기기인지. 맞으면 (명단, 사용자, 기기).
#[allow(clippy::result_large_err)]
fn member_of(
    node: &Node,
    peer: &PeerId,
    space: &SpaceId,
) -> std::result::Result<(Arc<HostedRoster>, UserId, DeviceId), YpResponse> {
    let Some((user, device)) = node.peers.identity_of(peer) else {
        return Err(YpResponse::error(WireErrorCode::Unauthenticated, "인증 전"));
    };
    let Some(rec) = roster_of(node, space) else {
        return Err(YpResponse::error(
            WireErrorCode::NotFound,
            "보관하지 않는 공간",
        ));
    };
    if !rec.roster.has_device(&user, &device) {
        return Err(YpResponse::error(
            WireErrorCode::NotMember,
            "명단의 멤버 기기가 아님",
        ));
    }
    Ok((rec, user, device))
}

/// 보낸 기기마다 명단·첨부 알림·삭제 반영 요청의 속도 제한(요청 크기로 셈).
#[allow(clippy::result_large_err)]
fn ctl_limit(node: &Node, device: DeviceId, bytes: u64) -> std::result::Result<(), YpResponse> {
    let now = now_ms();
    let mut buckets = node.archive.host.ctl_buckets.lock();
    if buckets.len() > 4096 {
        buckets.clear();
    }
    let b = buckets
        .entry(device)
        .or_insert_with(|| TokenBucket::with_burst(CTL_RATE_BPS, CTL_BURST_BYTES, now));
    b.try_consume(bytes, now)
        .map_err(|retry_after_ms| YpResponse::Busy { retry_after_ms })
}

/// 명단 순서 검사에 쓰는 이 노드의 논리 시계.
fn roster_local(node: &Node, space: &SpaceId) -> RosterLocal {
    RosterLocal {
        max_lamport: node.store.max_lamport(space).unwrap_or(0),
        space_max: node
            .store
            .stream_max_lamport(space, StreamId::Space)
            .unwrap_or(0),
    }
}

/// 받은 명단이 지금 명단보다 오래됨: 보낸 쪽이 따라잡은 뒤 다시 보내게(받은 것으로 기록하지 않게) 알립니다.
fn stale_roster() -> YpResponse {
    YpResponse::error(
        WireErrorCode::Expired,
        "보관 노드의 명단이 더 새로움(따라잡은 뒤 다시 보내세요)",
    )
}

// ───────────── 지정·명단·해제 ─────────────

fn designate(node: &Arc<Node>, peer: PeerId, me: DeviceId, d: ArchiveDesignate) -> YpResponse {
    if !node.cfg.archive.enabled {
        return YpResponse::error(
            WireErrorCode::Forbidden,
            "보관 노드로 띄운 노드가 아님(--archive)",
        );
    }
    if d.auth
        .verify(
            &peer.to_bytes(),
            &node.peer_id().to_bytes(),
            MAX_AUTH_SKEW_MS,
        )
        .is_err()
    {
        return YpResponse::error(WireErrorCode::Unauthenticated, "기기 증명이 올바르지 않음");
    }
    if !matches!(
        identity::auth_status(node, &d.auth),
        identity::AuthStatus::Active
    ) {
        return YpResponse::error(WireErrorCode::Forbidden, "활성 기기가 아님");
    }
    let space = d.roster.roster.space;
    if node.spaces.read().contains_key(&space) {
        return YpResponse::error(
            WireErrorCode::Forbidden,
            "이 노드가 멤버인 공간은 보관할 수 없음",
        );
    }
    let key = offer_key(&token_hash(&d.token));
    let offer: Option<Offer> = node.store.get_setting(&key).ok().flatten();
    match offer {
        None => return YpResponse::error(WireErrorCode::InvalidToken, "모르는 제안 링크"),
        Some(o) if o.expires_at.is_some_and(|t| t.is_past()) => {
            let _ = node.store.delete_setting(&key);
            return YpResponse::error(WireErrorCode::Expired, "만료된 제안 링크");
        }
        Some(_) => {}
    }
    let now = Timestamp::now();
    let _g = node.archive.host.lock.lock();
    // (새 기록, 통째 명단인지). 다시 지정에서 반영할 것이 없으면 `None`.
    let accepted = match roster_of(node, &space) {
        None => accept_initial(&d.roster, &me, (&d.auth.user, &d.auth.device), now)
            .map(|r| Some((r, true))),
        // 이미 받아 둔 공간을 다시 지정(예: 함께 보관할 채널을 바꿈): 지금 명단의 멤버 기기가 낸 명단을 보통 규칙으로 받습니다.
        Some(cur) => {
            if d.roster.signer != d.auth.device
                || !cur.roster.has_device(&d.auth.user, &d.auth.device)
            {
                return YpResponse::error(WireErrorCode::Forbidden, "지정한 기기가 명단에 없음");
            }
            match accept_roster(&cur, &d.roster, &me, roster_local(node, &space), now) {
                RosterVerdict::Accepted(r) => Ok(Some((*r, true))),
                RosterVerdict::Narrowed(r) | RosterVerdict::Removed(r) => Ok(Some((*r, false))),
                // 지정은 이미 있음: 명단은 지정한 기기가 따라잡은 뒤 보통 명단으로 다시 보냅니다.
                RosterVerdict::Stale | RosterVerdict::Unchanged => Ok(None),
                RosterVerdict::Refused(why) => Err(why),
            }
        }
    };
    let accepted = match accepted {
        Ok(r) => r,
        Err(why) => return YpResponse::error(WireErrorCode::Forbidden, why),
    };
    let _ = node.store.delete_setting(&key);
    if !is_hosted(node, &space) {
        let Some((rec, _)) = accepted else {
            return YpResponse::error(WireErrorCode::Internal, "첫 명단 없음");
        };
        let h = Hosted {
            rec: Arc::new(rec),
            designated_at: now,
            designated_by: d.auth.user,
            used_bytes: 0,
            dropped: BTreeMap::new(),
            dirty: false,
            recount: false,
            last_recount: Instant::now(),
        };
        if let Err(e) = save(node, &space, &h) {
            debug!(error = %e, "보관 기록 저장 실패");
            return YpResponse::error(WireErrorCode::Internal, "저장 실패");
        }
        node.archive.host.hosted.write().insert(space, h);
        info!(space = %space.short(), "보관 지정을 받음");
    } else if let Some((rec, full)) = accepted {
        apply_roster(node, &space, rec, full);
    }
    YpResponse::Ok
}

/// 명단 하나를 받는 데 드는 요청량(속도 제한용).
const ROSTER_COST: u64 = 8 * 1024;

fn roster_update(
    node: &Arc<Node>,
    peer: PeerId,
    me: DeviceId,
    signed: SignedArchiveRoster,
) -> YpResponse {
    let space = signed.roster.space;
    let (_, _, device) = match member_of(node, &peer, &space) {
        Ok(x) => x,
        Err(r) => return r,
    };
    if let Err(r) = ctl_limit(node, device, ROSTER_COST) {
        return r;
    }
    let _g = node.archive.host.lock.lock();
    // 잠그기 전에 다른 명단이 반영됐을 수 있어 다시 읽습니다.
    let Some(cur) = roster_of(node, &space) else {
        return YpResponse::error(WireErrorCode::NotFound, "보관하지 않는 공간");
    };
    match accept_roster(
        &cur,
        &signed,
        &me,
        roster_local(node, &space),
        Timestamp::now(),
    ) {
        RosterVerdict::Accepted(r) => {
            apply_roster(node, &space, *r, true);
            YpResponse::Ok
        }
        RosterVerdict::Removed(r) => {
            apply_roster(node, &space, *r, false);
            YpResponse::Ok
        }
        RosterVerdict::Unchanged => YpResponse::Ok,
        // 일부만 반영함: 소유자가 따라잡은 뒤 다시 보내면 통째로 받습니다.
        RosterVerdict::Narrowed(r) => {
            apply_roster(node, &space, *r, false);
            stale_roster()
        }
        RosterVerdict::Stale => stale_roster(),
        RosterVerdict::Refused(why) => YpResponse::error(WireErrorCode::Forbidden, why),
    }
}

/// 명단 차이 하나를 받는 데 드는 요청량(속도 제한용): 기본 몫 + 바꾸는 멤버마다.
const DELTA_COST: u64 = 512;
const DELTA_CHANGE_COST: u64 = 128;

/// 바탕 명단 위의 차이(`yangpa_sync::archive::accept_delta`). 바탕이 다르거나 앞 차이가 없으면 `Expired`로 알려
/// 보낸 쪽이 바탕 명단(권한자)을 다시 보내게 합니다.
fn roster_delta(
    node: &Arc<Node>,
    peer: PeerId,
    me: DeviceId,
    signed: yangpa_core::archive::SignedArchiveRosterDelta,
) -> YpResponse {
    let space = signed.delta.space;
    let (_, _, device) = match member_of(node, &peer, &space) {
        Ok(x) => x,
        Err(r) => return r,
    };
    let changes = (signed.delta.add.len() + signed.delta.remove.len()) as u64;
    if let Err(r) = ctl_limit(node, device, DELTA_COST + DELTA_CHANGE_COST * changes) {
        return r;
    }
    let _g = node.archive.host.lock.lock();
    let Some(cur) = roster_of(node, &space) else {
        return YpResponse::error(WireErrorCode::NotFound, "보관하지 않는 공간");
    };
    match accept_delta(&cur, &signed, &me, Timestamp::now()) {
        DeltaVerdict::Accepted(r) => {
            apply_roster(node, &space, *r, false);
            YpResponse::Ok
        }
        DeltaVerdict::Unchanged => YpResponse::Ok,
        DeltaVerdict::Expired(why) => YpResponse::error(WireErrorCode::Expired, why),
        DeltaVerdict::Refused(why) => YpResponse::error(WireErrorCode::Forbidden, why),
    }
}

/// 새 명단을 반영합니다(지정·명단 잠금 안에서): 저장하고, 어느 명단에도 없는 기기의 인증을 지웁니다.
///
/// 빠진 채널은 바로 지우지 않고 기다립니다([`ERASE_GRACE`]). 따라잡지 못한 권한자 기기의 명단이 채널을 빼먹어도
/// 그 기기가 다시 보낸 명단이 되살리게 하려는 것입니다. 빠졌다고 한 명단보다 순서가 높은 통째 명단(`full`)도 빠졌다고
/// 하면 그때 지웁니다.
fn apply_roster(node: &Node, space: &SpaceId, new: HostedRoster, full: bool) {
    let now = Timestamp::now();
    let (old, erase) = {
        let mut hosted = node.archive.host.hosted.write();
        let Some(h) = hosted.get_mut(space) else {
            return;
        };
        let r = &new.roster;
        for c in &h.rec.roster.channels {
            if r.channel(&c.channel).is_none() {
                h.dropped.entry(c.channel).or_insert(Dropped {
                    since: now,
                    anchor: r.anchor,
                });
            }
        }
        h.dropped.retain(|c, _| r.channel(c).is_none());
        let erase: Vec<ChannelId> = if full {
            h.dropped
                .iter()
                .filter(|(_, d)| r.anchor > d.anchor)
                .map(|(c, _)| *c)
                .collect()
        } else {
            Vec::new()
        };
        for c in &erase {
            h.dropped.remove(c);
        }
        let old = std::mem::replace(&mut h.rec, Arc::new(new));
        if let Err(e) = save(node, space, h) {
            debug!(error = %e, "보관 기록 저장 실패");
        }
        (old, erase)
    };
    for c in &erase {
        erase_channel(node, space, c);
    }
    let gone: Vec<DeviceId> = old
        .roster
        .members
        .iter()
        .flat_map(|m| m.devices.iter().map(move |d| (m.user, *d)))
        .filter(|(u, d)| !super::trusts(node, u, d))
        .map(|(_, d)| d)
        .collect();
    node.peers.forget_devices(&gone);
}

/// 보관하지 않게 된 채널: 그 채널 스트림의 이벤트를 헤더까지 지우고 첨부도 지웁니다. 다시 보관하게 되면 처음부터
/// 다시 받습니다(본문만 지우면 같은 ID의 본문을 다시 받을 수 없어서).
fn erase_channel(node: &Node, space: &SpaceId, channel: &ChannelId) {
    if let Err(e) = node.store.purge_stream(space, StreamId::Channel(*channel)) {
        debug!(error = %e, "보관 채널 지우기 실패");
    }
    drop_channel_refs(node, space, channel);
    mark_recount(node, space);
}

fn release(node: &Arc<Node>, peer: PeerId, space: SpaceId, auth: &PeerAuth) -> YpResponse {
    if auth
        .verify(
            &peer.to_bytes(),
            &node.peer_id().to_bytes(),
            MAX_AUTH_SKEW_MS,
        )
        .is_err()
    {
        return YpResponse::error(WireErrorCode::Unauthenticated, "기기 증명이 올바르지 않음");
    }
    let Some(rec) = roster_of(node, &space) else {
        return YpResponse::error(WireErrorCode::NotFound, "보관하지 않는 공간");
    };
    let r = &rec.roster;
    let allowed = (r.has_device(&auth.user, &auth.device) && r.is_authority(&auth.user))
        || rec.owner_signers.contains(&auth.device);
    if !allowed {
        return YpResponse::error(WireErrorCode::Forbidden, "해제할 수 있는 기기가 아님");
    }
    let _g = node.archive.host.lock.lock();
    purge(node, &space);
    info!(space = %space.short(), "보관 해제: 기록을 지움");
    YpResponse::Ok
}

/// 공간의 첨부 쓰임 기록을 모두 지웁니다.
fn delete_ref_settings(node: &Node, space: &SpaceId) {
    let _ = node.store.delete_setting(&snap_key(space));
    for prefix in [ref_prefix(space), evref_prefix(space)] {
        for (key, _) in node.store.settings_with_prefix(&prefix).unwrap_or_default() {
            let _ = node.store.delete_setting(&key);
        }
    }
    node.archive
        .host
        .pending
        .lock()
        .retain(|(s, _), _| s != space);
}

/// 받아 둔 공간의 이벤트·첨부·기록을 모두 지웁니다. 받아 둔 공간이었으면 true.
pub(super) fn purge(node: &Node, space: &SpaceId) -> bool {
    let removed = node.archive.host.hosted.write().remove(space);
    let Some(h) = removed else {
        return false;
    };
    let _ = node.store.delete_setting(&hosted_key(space));
    delete_ref_settings(node, space);
    if let Err(e) = node.store.purge_space(space) {
        debug!(error = %e, "보관 공간 지우기 실패");
    }
    let gone: Vec<DeviceId> = h
        .rec
        .roster
        .members
        .iter()
        .flat_map(|m| m.devices.iter().map(move |d| (m.user, *d)))
        .filter(|(u, d)| !super::trusts(node, u, d))
        .map(|(_, d)| d)
        .collect();
    node.peers.forget_devices(&gone);
    true
}

/// 해제·`archive.drop`과 엇갈려 받아 둔 공간이 아니게 된 뒤 쓴 것(이벤트·첨부·쓰임 기록)을 지웁니다.
fn purge_leftovers(node: &Node, space: &SpaceId) {
    let _g = node.archive.host.lock.lock();
    if !orphaned(node, space) {
        return;
    }
    delete_ref_settings(node, space);
    if let Err(e) = node.store.purge_space(space) {
        debug!(error = %e, "남은 보관 공간 지우기 실패");
    }
}

// ───────────── 받기 ─────────────

#[derive(Default)]
struct Ingested {
    accepted: Vec<EventId>,
    missing_parents: bool,
}

/// 용량 상한 (메시지 본문·첨부, 그 밖의 모든 것). 헤더와 상태·키 갱신 이벤트가 들어갈 자리를 남기려고 메시지 본문·첨부는
/// 상한의 7/8까지만 받습니다.
fn quotas(node: &Node) -> (u64, u64) {
    let hard = node.cfg.archive.quota_bytes;
    (hard - hard / 8, hard)
}

/// 이벤트 헤더와 서명·봉인 부담(본문 빼고) 바이트.
fn head_cost(ev: &SignedEvent) -> u64 {
    ev.header.canonical_bytes().len() as u64 + EVENT_OVERHEAD
}

/// 받은 이벤트를 검사해 저장합니다. `pusher`가 있으면 그 사용자·기기가 쓴 이벤트만 받습니다.
fn ingest(
    node: &Node,
    space: &SpaceId,
    events: Vec<SignedEvent>,
    pusher: Option<(UserId, DeviceId)>,
) -> Ingested {
    let mut out = Ingested::default();
    let Some(rec) = roster_of(node, space) else {
        return out;
    };
    let roster = &rec.roster;
    let (soft, hard) = quotas(node);
    let used = node
        .archive
        .host
        .hosted
        .read()
        .get(space)
        .map_or(0, |h| h.used_bytes);
    let local_max = node
        .store
        .max_lamport(space)
        .unwrap_or(0)
        .max(roster.anchor);
    let mut batch = Vec::with_capacity(events.len());
    // 용량 때문에 본문을 빼고 받은 메시지(내주지 않음).
    let mut stripped = HashSet::new();
    let mut planned = 0u64;
    for mut ev in events {
        let (lane, stream, kind, lamport) = {
            let h = &ev.header;
            (h.kind.lane(), h.stream(), h.kind, h.lamport)
        };
        // 쓴 사용자: 밀어 넣은 기기 것이어야 하고, 당겨온 것은 명단에 있는 기기면 그 사용자와 맞아야 합니다
        // (나간 멤버의 기기는 확인할 수 없어 용량 상한으로만 묶임).
        let author_ok = match pusher {
            Some((u, d)) => ev.header.author == d && ev.header.user == u,
            None => roster
                .user_of(&ev.header.author)
                .is_none_or(|u| u == ev.header.user),
        };
        if ev.header.space != *space
            || matches!(ev.header.key, KeyRef::Volatile { .. })
            || !author_ok
            || (local_max > 0 && lamport > local_max.saturating_add(MAX_ANCHOR_AHEAD))
        {
            continue;
        }
        let in_scope = match stream {
            StreamId::Channel(c) => roster.channel(&c).is_some(),
            StreamId::Space => lane != Lane::Message,
            // 받은편지함은 쓴 기기가 직접 밀어 넣은 것만(멤버는 보관 노드에 내주지 않음). 명단에 없는 사용자 것은 권한자
            // (확정 기기는 소유자·관리자의 기기)가 민 것만 받습니다: 가입 키 묶음은 명단이 가입자를 싣기 전에 오고, 다시
            // 밀어 주는 이가 없습니다. 내줄 때는 명단에 오른 그 사용자의 기기에게만 줍니다(`archive_may_read_scoped`).
            StreamId::Inbox(u) => pusher
                .is_some_and(|(pu, _)| roster.member(&u).is_some() || roster.is_authority(&pu)),
            StreamId::KeyGroup(_) => false,
        };
        // 짧은 서명(채널 스트림)은 작성 기기 키가 없어 여기서 확인할 수 없습니다(명단은 기기 ID만 실음). 밀어 넣은 것은
        // 연결로 확인된 작성 기기 자신의 것이고, 당겨온 것은 명단에 있는 기기의 것만 받습니다. 받아 가는 멤버가 키로
        // 확인하므로 위조는 저장 몫만 씁니다.
        let sig_ok = if ev.sig.is_slim() {
            ev.check_integrity().is_ok()
                && (pusher.is_some() || roster.user_of(&ev.header.author).is_some())
        } else {
            ev.verify().is_ok()
        };
        if !in_scope || !sig_ok {
            continue;
        }
        if kind == EventKind::SpaceCreate && ev.id() != roster.genesis {
            continue;
        }
        let head = head_cost(&ev);
        let total = used.saturating_add(planned);
        if total.saturating_add(head) > hard {
            continue;
        }
        let mut cost = head;
        if ev.body.is_some() {
            let n = u64::from(ev.header.body_len) + SEAL_OVERHEAD;
            let limit = if lane == Lane::Message { soft } else { hard };
            if total.saturating_add(head).saturating_add(n) <= limit {
                cost += n;
            } else if lane == Lane::Message {
                ev = ev.without_body();
                stripped.insert(ev.id());
            } else {
                continue;
            }
        }
        planned += cost;
        if let Ok(p) = PersistentEvent::new(ev, SecurityLevel::Normal) {
            batch.push(p);
        }
    }
    if batch.is_empty() {
        return out;
    }
    let outcomes = match node.store.insert_events(&batch) {
        Ok(o) => o,
        Err(e) => {
            debug!(error = %e, "보관 이벤트 저장 실패");
            return out;
        }
    };
    let mut added = 0u64;
    let mut parents = Vec::new();
    let (mut flag, mut unflag) = (Vec::new(), Vec::new());
    for (p, o) in batch.iter().zip(outcomes) {
        let ev = p.event();
        let id = p.id();
        let body = ev
            .body
            .as_ref()
            .map_or(0, |_| u64::from(ev.header.body_len) + SEAL_OVERHEAD);
        match o {
            yangpa_store::InsertOutcome::Inserted => {
                added += head_cost(ev) + body;
                if stripped.contains(&id) {
                    flag.push(id);
                }
            }
            yangpa_store::InsertOutcome::BodyAdded => {
                added += body;
                unflag.push(id);
            }
            yangpa_store::InsertOutcome::Duplicate => continue,
        }
        parents.extend(ev.header.parents.iter().copied());
        out.accepted.push(id);
    }
    for (ids, v) in [(&flag, true), (&unflag, false)] {
        if !ids.is_empty()
            && let Err(e) = node.store.set_seed_excluded_many(ids, v)
        {
            debug!(error = %e, "보관 본문 표시 실패");
        }
    }
    if added > 0
        && let Some(h) = node.archive.host.hosted.write().get_mut(space)
    {
        h.used_bytes = h.used_bytes.saturating_add(added);
        h.dirty = true;
    }
    if orphaned(node, space) {
        purge_leftovers(node, space);
        return Ingested::default();
    }
    parents.sort_unstable();
    parents.dedup();
    if !parents.is_empty() {
        out.missing_parents = node
            .store
            .has_events(&parents)
            .map(|v| v.iter().any(|h| !h))
            .unwrap_or(false);
    }
    out
}

fn push(node: &Arc<Node>, peer: PeerId, space: SpaceId, events: Vec<SignedEvent>) -> YpResponse {
    let (_, user, device) = match member_of(node, &peer, &space) {
        Ok(x) => x,
        Err(r) => return r,
    };
    if events.len() > MAX_EVENTS_PER_MESSAGE {
        return YpResponse::error(WireErrorCode::BadRequest, "이벤트가 너무 많음");
    }
    let bytes: u64 = events
        .iter()
        .map(|e| u64::from(e.header.body_len) + 256)
        .sum();
    {
        let now = now_ms();
        let mut buckets = node.archive.host.buckets.lock();
        if buckets.len() > 4096 {
            buckets.clear();
        }
        let b = buckets
            .entry(device)
            .or_insert_with(|| TokenBucket::new(Some(PUSH_RATE_BPS), now));
        if let Err(retry_after_ms) = b.try_consume(bytes, now) {
            return YpResponse::Busy { retry_after_ms };
        }
    }
    let r = ingest(node, &space, events, Some((user, device)));
    if r.missing_parents {
        spawn_pull(node, space, peer);
    }
    YpResponse::Ok
}

// ───────────── 당겨오기 ─────────────

/// 인증된 멤버 기기와 연결됨: 그 기기가 명단에 있는 공간을 그 피어에게서 당겨 옵니다(못 받은 첨부는 주기 작업이
/// 물러나며 다시 시도).
pub(super) fn on_peer_authed(node: &Arc<Node>, peer: PeerId, user: &UserId, device: &DeviceId) {
    let spaces: Vec<SpaceId> = node
        .archive
        .host
        .hosted
        .read()
        .iter()
        .filter(|(_, h)| h.rec.roster.has_device(user, device))
        .map(|(s, _)| *s)
        .collect();
    for space in spaces {
        spawn_pull(node, space, peer);
    }
}

fn spawn_pull(node: &Arc<Node>, space: SpaceId, peer: PeerId) {
    {
        let mut running = node.archive.host.pulling.lock();
        if let Some(rerun) = running.get_mut(&(space, peer)) {
            *rerun = true;
            return;
        }
        running.insert((space, peer), false);
    }
    node.spawn_detached(move |node| async move {
        loop {
            if let Err(e) = pull(&node, space, peer).await {
                debug!(space = %space.short(), peer = %peer, error = %e, "보관 당겨오기 실패");
            }
            let again = {
                let mut running = node.archive.host.pulling.lock();
                match running.get_mut(&(space, peer)) {
                    Some(rerun) if *rerun => {
                        *rerun = false;
                        true
                    }
                    _ => {
                        running.remove(&(space, peer));
                        false
                    }
                }
            };
            if !again {
                break;
            }
        }
    });
}

/// 진행 중인 당겨오기가 없는지(시험용).
pub(super) fn pull_idle(node: &Node) -> bool {
    node.archive.host.pulling.lock().is_empty() && node.archive.host.fetching.lock().is_empty()
}

async fn pull(node: &Arc<Node>, space: SpaceId, peer: PeerId) -> Result<()> {
    let Some(rec) = roster_of(node, &space) else {
        return Ok(());
    };
    let Some((user, device)) = node.peers.identity_of(&peer) else {
        return Ok(());
    };
    if !rec.roster.has_device(&user, &device) {
        return Ok(());
    }
    // 상대가 건 인증에 막 답한 직후면 상대는 아직 나를 인증된 피어로 기록하지 않았을 수 있습니다. 그러면 내 인증을
    // 한 번 다시 보내고 당겨옵니다(멤버는 남의 이벤트를 보내 주지 않으므로 다음 주기까지 미루지 않게).
    if let Ok(YpResponse::Error {
        code: WireErrorCode::Unauthenticated,
        ..
    }) = node
        .net
        .request(
            peer,
            YpRequest::GetHeads {
                space,
                stream: StreamId::Space,
            },
        )
        .await
        && !crate::peers::reconfirm_auth(node, peer).await
    {
        return Ok(());
    }
    sync_stream(node, space, peer, StreamId::Space).await?;
    let channels: Vec<ChannelId> = rec.roster.channels.iter().map(|c| c.channel).collect();
    for c in channels {
        if !roster_of(node, &space).is_some_and(|r| r.roster.channel(&c).is_some()) {
            continue;
        }
        if let Err(e) = sync_stream(node, space, peer, StreamId::Channel(c)).await {
            debug!(error = %e, "보관 채널 당겨오기 실패");
        }
    }
    Ok(())
}

async fn request(node: &Node, peer: PeerId, req: YpRequest) -> Result<YpResponse> {
    match node.net.request(peer, req).await? {
        YpResponse::Error { code, .. } => Err(Error::Network(format!("상대가 거절함: {code:?}"))),
        YpResponse::Busy { .. } => Err(Error::Network("상대가 바쁨".into())),
        r => Ok(r),
    }
}

fn unknown(node: &Node, ids: &[EventId]) -> Result<Vec<EventId>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let have = node.store.has_events(ids)?;
    Ok(ids
        .iter()
        .zip(have)
        .filter(|(_, h)| !h)
        .map(|(id, _)| *id)
        .collect())
}

async fn fetch_events(
    node: &Node,
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
                let wanted: HashSet<&EventId> = chunk.iter().collect();
                out.extend(evs.into_iter().filter(|e| wanted.contains(&e.id())));
            }
            _ => return Err(Error::Network("이벤트 응답이 아님".into())),
        }
    }
    Ok(out)
}

/// 한 스트림 맞추기(멤버의 따라잡기와 같은 순서: 머리 → 부모 따라가기 → 지문 비교).
async fn sync_stream(node: &Node, space: SpaceId, peer: PeerId, stream: StreamId) -> Result<()> {
    let (heads, remote_max) =
        match request(node, peer, YpRequest::GetHeads { space, stream }).await? {
            YpResponse::Heads { heads, max_lamport } => (heads, max_lamport),
            _ => return Err(Error::Network("머리 응답이 아님".into())),
        };
    let mut want = unknown(node, &heads)?;
    let mut rounds = 0;
    while !want.is_empty() && rounds < CHASE_ROUNDS {
        rounds += 1;
        let evs = fetch_events(node, space, peer, &want).await?;
        if evs.is_empty() {
            break;
        }
        let parents: Vec<EventId> = evs
            .iter()
            .flat_map(|e| e.header.parents.iter().copied())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        ingest(node, &space, evs, None);
        want = unknown(node, &parents)?;
        want.truncate(MAX_EVENTS_PER_MESSAGE);
    }
    let mut remote = heads;
    remote.sort_unstable();
    let mut local = node.store.heads(&space, stream)?;
    local.sort_unstable();
    if remote == local && node.store.missing_parents(&space, stream)?.is_empty() {
        return Ok(());
    }
    reconcile(node, space, peer, stream, remote_max).await
}

/// 저장소를 지문 비교 원본으로.
struct HostSource<'a> {
    node: &'a Node,
    space: SpaceId,
    stream: StreamId,
}

impl FingerprintSource for HostSource<'_> {
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
        self.node.store.has_events(ids)
    }
}

async fn reconcile(
    node: &Node,
    space: SpaceId,
    peer: PeerId,
    stream: StreamId,
    remote_max: u64,
) -> Result<()> {
    let local_max = node.store.stream_max_lamport(&space, stream)?;
    let mut rec = Reconciler::new(full_range(local_max.max(remote_max)));
    while let Some((token, req)) = rec.next_request() {
        let src = HostSource {
            node,
            space,
            stream,
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
            let evs = fetch_events(node, space, peer, &fetch).await?;
            ingest(node, &space, evs, None);
        }
    }
    Ok(())
}

// ───────────── 첨부 ─────────────

pub(super) fn snapshots_of(node: &Node, space: &SpaceId) -> Vec<(EventId, u64, BlobId)> {
    node.store
        .get_setting(&snap_key(space))
        .ok()
        .flatten()
        .unwrap_or_default()
}

/// 보관하는 상태 스냅숏인지.
fn is_snapshot(node: &Node, space: &SpaceId, blob: &BlobId) -> bool {
    snapshots_of(node, space).iter().any(|(_, _, b)| b == blob)
}

/// 체크포인트 스냅숏 알림을 기록합니다(최근 [`KEEP_SNAPSHOTS`]개, 체크포인트마다 하나). 밀려난 스냅숏 블롭은 지웁니다.
/// 기록했거나 이미 있으면 true.
fn note_snapshot(node: &Node, space: &SpaceId, event: EventId, lamport: u64, blob: BlobId) -> bool {
    let mut v = snapshots_of(node, space);
    if v.iter().any(|(e, _, _)| *e == event) {
        return true;
    }
    v.push((event, lamport, blob));
    v.sort_by(|a, b| b.1.cmp(&a.1).then(b.0.cmp(&a.0)));
    let gone = v.split_off(KEEP_SNAPSHOTS.min(v.len()));
    if !v.iter().any(|(e, _, _)| *e == event) {
        return false;
    }
    if node.store.put_setting(&snap_key(space), &v).is_err() {
        return false;
    }
    for (_, _, b) in gone {
        if v.iter().any(|(_, _, k)| *k == b) || !refs_of(node, space, &b).is_empty() {
            continue;
        }
        node.archive.host.pending.lock().remove(&(*space, b));
        if !used_elsewhere(node, space, &b) {
            let _ = node.store.blob_delete(&b);
            mark_recount(node, space);
        }
    }
    node.archive
        .host
        .pending
        .lock()
        .insert((*space, blob), Retry::now());
    true
}

fn refs_of(node: &Node, space: &SpaceId, blob: &BlobId) -> Vec<HostRef> {
    node.store
        .get_setting::<Vec<HostRef>>(&ref_key(space, blob))
        .ok()
        .flatten()
        .unwrap_or_default()
}

/// 공간의 모든 첨부 쓰임.
fn refs_in_space(node: &Node, space: &SpaceId) -> Vec<(BlobId, Vec<HostRef>)> {
    let prefix = ref_prefix(space);
    node.store
        .settings_with_prefix(&prefix)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(k, v)| {
            let blob = k[prefix.len()..].parse::<BlobId>().ok()?;
            let refs = serde_json::from_value::<Vec<HostRef>>(v).ok()?;
            Some((blob, refs))
        })
        .collect()
}

/// 첨부 쓰임 기록 결과.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RefAdd {
    Added,
    /// 이미 기록한 (메시지, 블롭).
    Exists,
    /// 블롭당 메시지 수나 메시지당 블롭 수가 찼음.
    Full,
}

fn add_ref(node: &Node, space: &SpaceId, blob: &BlobId, r: HostRef) -> RefAdd {
    let mut v = refs_of(node, space, blob);
    if v.iter().any(|x| x.event == r.event) {
        return RefAdd::Exists;
    }
    // 같은 메시지로 여러 번 알려도 메시지당 블롭 수를 넘지 않게 메시지 쪽 기록으로 셉니다.
    let ekey = evref_key(space, &r.event);
    let mut blobs: Vec<BlobId> = node
        .store
        .get_setting(&ekey)
        .ok()
        .flatten()
        .unwrap_or_default();
    if v.len() >= MAX_REFS_PER_BLOB || (!blobs.contains(blob) && blobs.len() >= MAX_REF_BLOBS) {
        return RefAdd::Full;
    }
    if !blobs.contains(blob) {
        blobs.push(*blob);
    }
    v.push(r);
    if node.store.put_setting(&ekey, &blobs).is_err()
        || node.store.put_setting(&ref_key(space, blob), &v).is_err()
    {
        return RefAdd::Full;
    }
    RefAdd::Added
}

/// 한 블롭에서 조건에 맞는 쓰임을 지우고, 쓰임이 없어지면 (다른 공간에서 쓰지 않을 때) 블롭을 지웁니다.
fn remove_refs(node: &Node, space: &SpaceId, blob: &BlobId, pred: impl Fn(&HostRef) -> bool) {
    let mut refs = refs_of(node, space, blob);
    let before = refs.len();
    refs.retain(|r| !pred(r));
    if refs.len() == before {
        return;
    }
    let key = ref_key(space, blob);
    if !refs.is_empty() {
        let _ = node.store.put_setting(&key, &refs);
        return;
    }
    let _ = node.store.delete_setting(&key);
    node.archive.host.pending.lock().remove(&(*space, *blob));
    if used_elsewhere(node, space, blob) {
        return;
    }
    let _ = node.store.blob_delete(blob);
    mark_recount(node, space);
}

/// 이 메시지들의 첨부 쓰임을 지웁니다(메시지 쪽 기록으로 바로 찾음).
fn drop_event_refs(node: &Node, space: &SpaceId, events: &[EventId]) {
    let mut by_blob: BTreeMap<BlobId, HashSet<EventId>> = BTreeMap::new();
    for e in events {
        let key = evref_key(space, e);
        let Ok(Some(blobs)) = node.store.get_setting::<Vec<BlobId>>(&key) else {
            continue;
        };
        let _ = node.store.delete_setting(&key);
        for b in blobs {
            by_blob.entry(b).or_default().insert(*e);
        }
    }
    for (blob, evs) in by_blob {
        remove_refs(node, space, &blob, |r| evs.contains(&r.event));
    }
}

/// 한 채널의 첨부 쓰임을 모두 지웁니다.
fn drop_channel_refs(node: &Node, space: &SpaceId, channel: &ChannelId) {
    for (blob, refs) in refs_in_space(node, space) {
        let gone: Vec<EventId> = refs
            .iter()
            .filter(|r| r.channel == *channel)
            .map(|r| r.event)
            .collect();
        if gone.is_empty() {
            continue;
        }
        for e in &gone {
            let _ = node.store.delete_setting(&evref_key(space, e));
        }
        remove_refs(node, space, &blob, |r| r.channel == *channel);
    }
}

/// 이 노드의 다른 공간(멤버 공간·다른 보관 공간)에도 등록된 블롭인지.
fn used_elsewhere(node: &Node, space: &SpaceId, blob: &BlobId) -> bool {
    let others: Vec<SpaceId> = node
        .space_ids()
        .into_iter()
        .chain(node.archive.host.hosted.read().keys().copied())
        .filter(|s| s != space)
        .chain(std::iter::once(crate::blobs::PROFILE_SPACE))
        .collect();
    others
        .iter()
        .any(|s| node.store.blob_in_space(blob, s).unwrap_or(true))
}

fn blob_refs(
    node: &Arc<Node>,
    peer: PeerId,
    space: SpaceId,
    refs: Vec<ArchiveBlobRef>,
) -> YpResponse {
    let (rec, user, device) = match member_of(node, &peer, &space) {
        Ok(x) => x,
        Err(r) => return r,
    };
    if refs.len() > MAX_REF_EVENTS || refs.iter().any(|r| r.blobs.len() > MAX_REF_BLOBS) {
        return YpResponse::error(WireErrorCode::BadRequest, "첨부 알림이 너무 큼");
    }
    let cost = 64
        + refs
            .iter()
            .map(|r| 32 + 32 * r.blobs.len() as u64)
            .sum::<u64>();
    if let Err(r) = ctl_limit(node, device, cost) {
        return r;
    }
    let roster = &rec.roster;
    let authority = roster.is_authority(&user);
    let mut accepted = Vec::new();
    for r in refs {
        let Ok(Some(ev)) = node.store.get_event(&r.event) else {
            continue;
        };
        let h = &ev.header;
        if h.kind == EventKind::StateCheckpoint {
            // 상태 스냅숏: 내용을 열지 못하므로(상태 키) 체크포인트를 쓴 사용자나 권한자의 알림만 받습니다.
            if h.space == space
                && h.stream() == StreamId::Space
                && r.blobs.len() == 1
                && (h.user == user || authority)
                && note_snapshot(node, &space, r.event, h.lamport, r.blobs[0])
            {
                accepted.push(r.event);
            }
            continue;
        }
        let StreamId::Channel(channel) = h.stream() else {
            continue;
        };
        // 본문이 없는(지워졌거나 용량 때문에 빼고 받은) 메시지의 첨부는 내줄 일이 없어 받지 않습니다.
        if h.space != space
            || h.kind.lane() != Lane::Message
            || roster.channel(&channel).is_none()
            || (h.user != user && !authority)
            || ev.body.is_none()
            || node.store.is_deleted(&r.event).unwrap_or(true)
        {
            continue;
        }
        let hr = HostRef {
            event: r.event,
            channel,
            lamport: h.lamport,
        };
        for b in &r.blobs {
            // 새 쓰임만 다시 시도를 처음부터 겁니다(같은 알림을 되풀이해도 늘지 않게).
            if add_ref(node, &space, b, hr.clone()) == RefAdd::Added {
                node.archive
                    .host
                    .pending
                    .lock()
                    .insert((space, *b), Retry::now());
            }
        }
        accepted.push(r.event);
    }
    if orphaned(node, &space) {
        purge_leftovers(node, &space);
        return YpResponse::error(WireErrorCode::NotFound, "보관하지 않는 공간");
    }
    start_due_fetches(node);
    YpResponse::Ids(accepted)
}

/// 명단의 멤버 기기 중 지금 인증된 피어.
fn member_peers(node: &Node, space: &SpaceId) -> Vec<PeerId> {
    let Some(rec) = roster_of(node, space) else {
        return Vec::new();
    };
    node.peers
        .authed_peers()
        .into_iter()
        .filter(|(_, u, d)| rec.roster.has_device(u, d))
        .map(|(p, _, _)| p)
        .collect()
}

/// 동시에 받는 첨부 수.
const MAX_CONCURRENT_FETCHES: usize = 16;

/// 때가 된 첨부 받기를 동시 상한까지 시작합니다(주기 작업과 첨부 알림·받기 끝에서).
fn start_due_fetches(node: &Arc<Node>) {
    let now = Instant::now();
    let room = MAX_CONCURRENT_FETCHES.saturating_sub(node.archive.host.fetching.lock().len());
    if room == 0 {
        return;
    }
    let due: Vec<(SpaceId, BlobId)> = {
        let fetching = node.archive.host.fetching.lock();
        node.archive
            .host
            .pending
            .lock()
            .iter()
            .filter(|((_, b), r)| r.next <= now && !fetching.contains(b))
            .map(|(k, _)| *k)
            .take(room)
            .collect()
    };
    for (space, blob) in due {
        spawn_fetch(node, space, blob);
    }
}

/// 다시 시도 간격: 처음 [`BLOB_RETRY_START`]에서 두 배씩, 최대 [`BLOB_RETRY_MAX`].
fn retry_delay(attempts: u32) -> Duration {
    BLOB_RETRY_START
        .saturating_mul(1u32 << attempts.saturating_sub(1).min(16))
        .min(BLOB_RETRY_MAX)
}

fn spawn_fetch(node: &Arc<Node>, space: SpaceId, blob: BlobId) {
    if !node.archive.host.fetching.lock().insert(blob) {
        return;
    }
    node.spawn_detached(move |node| async move {
        let r = fetch_blob(&node, space, blob).await;
        node.archive.host.fetching.lock().remove(&blob);
        {
            let mut pending = node.archive.host.pending.lock();
            let key = (space, blob);
            match r {
                Fetch::Done => {
                    pending.remove(&key);
                }
                // 받을 상대가 없으면 횟수를 쓰지 않고 조금 뒤에 다시.
                Fetch::NoPeers => {
                    if let Some(p) = pending.get_mut(&key) {
                        p.next = Instant::now() + BLOB_RETRY_START;
                    }
                }
                Fetch::Failed => {
                    let attempts = pending.get(&key).map_or(0, |p| p.attempts) + 1;
                    if attempts >= MAX_BLOB_ATTEMPTS {
                        pending.remove(&key);
                        debug!("보관 첨부 받기를 그만둠(새 알림이 오면 다시)");
                    } else {
                        pending.insert(
                            key,
                            Retry {
                                attempts,
                                next: Instant::now() + retry_delay(attempts),
                            },
                        );
                    }
                }
            }
        }
        if !node.is_shutting_down() {
            start_due_fetches(&node);
        }
    });
}

/// 블롭 받기 결과.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fetch {
    /// 다 받았거나 더 할 일이 없음(기록에서 뺌).
    Done,
    /// 받지 못함(다시 시도 횟수를 씀).
    Failed,
    /// 지금 인증된 멤버 기기가 없음.
    NoPeers,
}

/// 저장소 비트맵(높은 비트부터)에서 조각 `i`를 가졌는지.
fn store_bit(bytes: &[u8], i: u32) -> bool {
    bytes
        .get((i / 8) as usize)
        .is_some_and(|b| b & (0x80 >> (i % 8)) != 0)
}

/// 블롭 하나를 멤버들에게서 받습니다(매니페스트 해시 = 블롭 ID, 조각마다 해시 확인).
async fn fetch_blob(node: &Arc<Node>, space: SpaceId, blob: BlobId) -> Fetch {
    if !is_hosted(node, &space)
        || (refs_of(node, &space, &blob).is_empty() && !is_snapshot(node, &space, &blob))
    {
        return Fetch::Done;
    }
    let registered = node.store.blob_in_space(&blob, &space).unwrap_or(false);
    if registered
        && node
            .store
            .blob_status(&blob)
            .ok()
            .flatten()
            .is_some_and(|s| s.complete)
    {
        return Fetch::Done;
    }
    let peers = member_peers(node, &space);
    if peers.is_empty() {
        return Fetch::NoPeers;
    }
    if !registered {
        let mut found = None;
        for p in &peers {
            let req = YpRequest::BlobManifest { space, blob };
            if let Ok(YpResponse::Manifest(m)) = node.net.request(*p, req).await
                && m.id() == blob
                && m.check().is_ok()
            {
                found = Some(m);
                break;
            }
        }
        let Some(m) = found else {
            return Fetch::Failed;
        };
        let size = m.size;
        let (soft, _) = quotas(node);
        {
            let mut hosted = node.archive.host.hosted.write();
            let Some(h) = hosted.get_mut(&space) else {
                return Fetch::Done;
            };
            if h.used_bytes.saturating_add(size) > soft {
                debug!(space = %space.short(), "보관 용량 상한: 첨부를 받지 않음");
                return Fetch::Done;
            }
            h.used_bytes += size;
            h.dirty = true;
        }
        let stored = PersistentBlob::new(m, SecurityLevel::Normal)
            .map_err(Error::from)
            .and_then(|pb| Ok(node.store.blob_register(&space, &pb)?));
        if let Err(e) = stored {
            debug!(error = %e, "보관 블롭 등록 실패");
            mark_recount(node, &space);
            return Fetch::Failed;
        }
        // 보관한 첨부는 캐시 정리 대상이 아닙니다.
        let _ = node.store.blob_set_pinned(&blob, true);
        if orphaned(node, &space) {
            purge_leftovers(node, &space);
            return Fetch::Done;
        }
    }
    let Ok(Some((bm, total))) = node.store.blob_have(&blob) else {
        return Fetch::Failed;
    };
    let missing: Vec<u32> = (0..total).filter(|i| !store_bit(&bm, *i)).collect();
    for (n, i) in missing.into_iter().enumerate() {
        if node.is_shutting_down() || !is_hosted(node, &space) {
            return Fetch::Failed;
        }
        let mut got = false;
        'peers: for j in 0..peers.len() {
            let p = peers[(n + j) % peers.len()];
            for _ in 0..BUSY_RETRIES {
                let req = YpRequest::BlobChunk {
                    space,
                    blob,
                    index: i,
                };
                match node.net.request(p, req).await {
                    Ok(YpResponse::Chunk { data }) => {
                        if node.store.blob_write_chunk(&blob, i, &data).is_ok() {
                            got = true;
                            break 'peers;
                        }
                        continue 'peers;
                    }
                    Ok(YpResponse::Busy { retry_after_ms }) => {
                        let wait =
                            Duration::from_millis(u64::from(retry_after_ms)).min(MAX_BUSY_WAIT);
                        tokio::time::sleep(wait).await;
                    }
                    _ => continue 'peers,
                }
            }
        }
        if !got {
            return Fetch::Failed;
        }
    }
    Fetch::Done
}

// ───────────── 삭제 반영 ─────────────

fn prune(node: &Arc<Node>, peer: PeerId, space: SpaceId, events: Vec<EventId>) -> YpResponse {
    let (rec, user, device) = match member_of(node, &peer, &space) {
        Ok(x) => x,
        Err(r) => return r,
    };
    if events.len() > MAX_PRUNE_EVENTS {
        return YpResponse::error(WireErrorCode::BadRequest, "삭제 반영이 너무 많음");
    }
    if let Err(r) = ctl_limit(node, device, 64 + 32 * events.len() as u64) {
        return r;
    }
    let moderator = rec.roster.is_moderator(&user);
    let mut pruned = Vec::new();
    for id in events {
        // 이미 반영한 삭제는 건너뜁니다(같은 요청을 되풀이해도 다시 찾지 않게).
        if node.store.is_deleted(&id).unwrap_or(true) {
            continue;
        }
        match node.store.get_event(&id) {
            Ok(Some(ev)) => {
                let h = &ev.header;
                if h.space != space
                    || h.kind.lane() != Lane::Message
                    || (h.user != user && !moderator)
                {
                    continue;
                }
            }
            // 아직 받지 않은 메시지: 운영진의 삭제만 표시로 남깁니다(나중에 와도 본문을 저장하지 않음).
            Ok(None) if moderator => {}
            _ => continue,
        }
        if node
            .store
            .mark_message_deleted(&space, &id, Timestamp::now())
            .is_ok()
        {
            pruned.push(id);
        }
    }
    if !pruned.is_empty() {
        drop_event_refs(node, &space, &pruned);
        mark_recount(node, &space);
    }
    YpResponse::Ok
}

// ───────────── 내주기 ─────────────

/// 대역폭·동시 전송(이 노드의 시더 설정).
#[allow(clippy::result_large_err)]
fn bandwidth(
    node: &Node,
    space: &SpaceId,
    bytes: u64,
    in_flight: u32,
) -> std::result::Result<(), YpResponse> {
    let p = policy::seeder_policy(node, space);
    if in_flight >= p.max_concurrent.max(1) {
        return Err(YpResponse::Busy {
            retry_after_ms: BUSY_RETRY_MS,
        });
    }
    let now = now_ms();
    let mut bucket = node.sync.bucket.lock();
    let b = bucket.get_or_insert_with(|| TokenBucket::new(p.upload_limit_bps, now));
    b.configure(p.upload_limit_bps, now);
    b.try_consume(bytes, now)
        .map_err(|retry_after_ms| YpResponse::Busy { retry_after_ms })
}

fn channel_of(stream: &StreamId) -> Option<ChannelId> {
    match stream {
        StreamId::Channel(c) => Some(*c),
        StreamId::Space | StreamId::KeyGroup(_) | StreamId::Inbox(_) => None,
    }
}

/// 이 요청자에게 부분 전달 스트림을 숨겨야 하는지(빈 스트림과 똑같이 답함, `archive_may_read_scoped`).
fn scoped_hidden(
    roster: &ArchiveRoster,
    user: &UserId,
    device: &DeviceId,
    stream: &StreamId,
) -> bool {
    archive_may_read_scoped(roster, user, device, stream) == Some(false)
}

/// 보관 노드가 본문을 빼고 받아 둔 이벤트(키 갱신 레인에서 본문 없이 둔 것, 용량 때문에 뺀 메시지): 내주지 않습니다. 멤버는 본문 없는
/// 메시지를 지워진 것으로 여겨 다른 피어에게서 다시 받지 않으므로, 내주지 않아야 다른 시더에게서 온전히 받습니다.
/// 일부러 지운(삭제 표시가 있는) 메시지는 헤더만 내줍니다.
fn withheld(node: &Node, ev: &SignedEvent) -> bool {
    if ev.body.is_some() || ev.header.body_len == 0 {
        return false;
    }
    let id = ev.id();
    match ev.header.kind.lane() {
        Lane::KeyAgreement => true,
        Lane::Message => {
            node.store.is_seed_excluded(&id).unwrap_or(false)
                && !node.store.is_deleted(&id).unwrap_or(false)
        }
        _ => false,
    }
}

/// 받아 둔 공간에 온 동기화·블롭 요청.
pub(super) fn serve(node: &Arc<Node>, peer: PeerId, req: YpRequest) -> YpResponse {
    if let YpRequest::PushEvents { space, events } = req {
        return push(node, peer, space, events);
    }
    let Some(space) = request_space(&req) else {
        return YpResponse::error(WireErrorCode::BadRequest, "보관 요청이 아님");
    };
    let (rec, user, device) = match member_of(node, &peer, &space) {
        Ok(x) => x,
        Err(r) => return r,
    };
    let roster = &rec.roster;
    let refuse = |code: WireErrorCode| YpResponse::error(code, "보관 노드가 내주지 않음");
    let serving = crate::sync_service::serving(node, device);
    let in_flight = serving.others;
    let resp = (|| match req {
        YpRequest::GetHeads { stream, .. } => {
            if let Err(c) =
                check_archive_serve(roster, &user, &device, channel_of(&stream).as_ref(), None)
            {
                return refuse(c);
            }
            if let Err(r) = bandwidth(node, &space, 64, in_flight) {
                return r;
            }
            if scoped_hidden(roster, &user, &device, &stream) {
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
        YpRequest::GetFingerprints { stream, ranges, .. } => {
            if let Err(c) =
                check_archive_serve(roster, &user, &device, channel_of(&stream).as_ref(), None)
            {
                return refuse(c);
            }
            if let Err(r) = bandwidth(node, &space, 40 * ranges.len() as u64, in_flight) {
                return r;
            }
            if scoped_hidden(roster, &user, &device, &stream) {
                return match answer_fingerprints(&MemorySet::new(), &ranges) {
                    Ok(fps) => YpResponse::Fingerprints(fps),
                    Err(_) => YpResponse::error(WireErrorCode::BadRequest, "잘못된 구간"),
                };
            }
            let src = HostSource {
                node,
                space,
                stream,
            };
            match answer_fingerprints(&src, &ranges) {
                Ok(fps) => YpResponse::Fingerprints(fps),
                Err(_) => YpResponse::error(WireErrorCode::BadRequest, "잘못된 구간"),
            }
        }
        YpRequest::GetIds {
            stream,
            range,
            limit,
            ..
        } => {
            if let Err(c) =
                check_archive_serve(roster, &user, &device, channel_of(&stream).as_ref(), None)
            {
                return refuse(c);
            }
            if let Err(r) = bandwidth(node, &space, 32 * u64::from(limit.min(512)), in_flight) {
                return r;
            }
            if scoped_hidden(roster, &user, &device, &stream) {
                return match answer_ids(&MemorySet::new(), range, limit) {
                    Ok(ids) => YpResponse::Ids(ids),
                    Err(_) => YpResponse::error(WireErrorCode::BadRequest, "잘못된 구간"),
                };
            }
            let src = HostSource {
                node,
                space,
                stream,
            };
            match answer_ids(&src, range, limit) {
                Ok(ids) => YpResponse::Ids(ids),
                Err(_) => YpResponse::error(WireErrorCode::BadRequest, "잘못된 구간"),
            }
        }
        YpRequest::GetEvents { ids, .. } => {
            if ids.len() > MAX_EVENTS_PER_MESSAGE {
                return YpResponse::error(WireErrorCode::BadRequest, "ID가 너무 많음");
            }
            let evs = match node.store.get_events(&ids) {
                Ok(e) => e,
                Err(_) => return YpResponse::error(WireErrorCode::Internal, "저장소 오류"),
            };
            let mut out = Vec::with_capacity(evs.len());
            let mut first = None;
            for ev in evs {
                let h = &ev.header;
                if h.space != space {
                    first.get_or_insert_with(|| refuse(WireErrorCode::NotFound));
                    continue;
                }
                // 받을 자격이 없는 받은편지함 이벤트는 모르는 이벤트처럼 뺍니다.
                if scoped_hidden(roster, &user, &device, &h.stream()) {
                    continue;
                }
                let channel = channel_of(&h.stream());
                if let Err(c) = check_archive_serve(
                    roster,
                    &user,
                    &device,
                    channel.as_ref(),
                    Some(Some(h.lamport)),
                ) {
                    first.get_or_insert_with(|| refuse(c));
                    continue;
                }
                if withheld(node, &ev) {
                    continue;
                }
                if let Err(r) = bandwidth(node, &space, u64::from(h.body_len) + 256, in_flight) {
                    first.get_or_insert(r);
                    break;
                }
                out.push(ev);
            }
            match (out.is_empty(), first) {
                (true, Some(r)) => r,
                _ => YpResponse::Events(out),
            }
        }
        YpRequest::BlobManifest { blob, .. }
        | YpRequest::BlobHave { blob, .. }
        | YpRequest::BlobChunk { blob, .. } => {
            serve_blob(node, roster, &user, &device, &space, &blob, &req, in_flight)
        }
        _ => YpResponse::error(WireErrorCode::BadRequest, "보관 요청이 아님"),
    })();
    drop(serving);
    resp
}

/// 첨부 내주기: 그 첨부를 알린 메시지 중 하나라도 요청자에게 내줄 수 있어야 합니다(지워진 메시지는 뺌).
#[allow(clippy::too_many_arguments)]
fn serve_blob(
    node: &Node,
    roster: &ArchiveRoster,
    user: &UserId,
    device: &DeviceId,
    space: &SpaceId,
    blob: &BlobId,
    req: &YpRequest,
    in_flight: u32,
) -> YpResponse {
    let not_found = || YpResponse::error(WireErrorCode::NotFound, "없는 블롭");
    if !node.store.blob_in_space(blob, space).unwrap_or(false) {
        return not_found();
    }
    let mut first = None;
    // 상태 스냅숏은 명단의 멤버 기기 누구에게나(상태 키로 암호화돼 보관 노드도 열지 못함).
    let mut allowed = is_snapshot(node, space, blob)
        && check_archive_serve(roster, user, device, None, None).is_ok();
    for r in refs_of(node, space, blob)
        .into_iter()
        .take_while(|_| !allowed)
    {
        if node.store.is_deleted(&r.event).unwrap_or(true) {
            continue;
        }
        match check_archive_serve(
            roster,
            user,
            device,
            Some(&r.channel),
            Some(Some(r.lamport)),
        ) {
            Ok(()) => {
                allowed = true;
                break;
            }
            Err(c) => {
                first.get_or_insert(c);
            }
        }
    }
    if !allowed {
        return first.map_or_else(not_found, |c| {
            YpResponse::error(c, "보관 노드가 내주지 않음")
        });
    }
    let manifest = match node.store.blob_manifest(blob) {
        Ok(Some(m)) => m,
        _ => return not_found(),
    };
    let chunks = manifest.chunks.len() as u64;
    let bytes = match req {
        YpRequest::BlobManifest { .. } => 64 + 32 * chunks,
        YpRequest::BlobHave { .. } => 64 + chunks.div_ceil(8),
        YpRequest::BlobChunk { index, .. } if u64::from(*index) < chunks => {
            manifest.chunk_len(*index)
        }
        _ => return YpResponse::error(WireErrorCode::BadRequest, "조각 번호 범위 밖"),
    };
    if let Err(r) = bandwidth(node, space, bytes, in_flight) {
        return r;
    }
    match req {
        YpRequest::BlobManifest { .. } => YpResponse::Manifest((*manifest).clone()),
        YpRequest::BlobHave { .. } => match node.store.blob_have(blob) {
            Ok(Some((bm, total))) => {
                let mut wire = Bitmap::new(total);
                for i in (0..total).filter(|i| store_bit(&bm, *i)) {
                    wire.set(i);
                }
                YpResponse::Have {
                    bitmap: wire.to_wire().0,
                    total,
                }
            }
            _ => not_found(),
        },
        YpRequest::BlobChunk { index, .. } => match node.store.blob_read_chunk(blob, *index) {
            Ok(Some(data)) => YpResponse::Chunk { data },
            Ok(None) => YpResponse::error(WireErrorCode::NotFound, "없는 조각"),
            Err(_) => YpResponse::error(WireErrorCode::Internal, "저장소 오류"),
        },
        _ => YpResponse::error(WireErrorCode::BadRequest, "블롭 요청이 아님"),
    }
}

// ───────────── 주기 작업 ─────────────

/// 다음 주기에 사용량을 저장소에서 다시 셉니다(지운 본문·첨부를 빼려고).
fn mark_recount(node: &Node, space: &SpaceId) {
    if let Some(h) = node.archive.host.hosted.write().get_mut(space) {
        h.recount = true;
    }
}

/// 저장소에서 잰 사용량: 이벤트(헤더·서명·봉인 본문)와 이 공간에 등록된 첨부 크기의 합.
fn measure(node: &Node, space: &SpaceId) -> Option<u64> {
    let events = node.store.space_event_bytes(space).ok()?;
    let blobs: u64 = node
        .store
        .blobs_in_space(space)
        .ok()?
        .iter()
        .filter_map(|b| node.store.blob_manifest(b).ok().flatten())
        .map(|m| m.size)
        .sum();
    Some(events.saturating_add(blobs))
}

/// 채팅 수명이 지난 본문 지우기(작성 시각 기준), 기다린 채널 지우기, 사용량 다시 세기·저장, 못 받은 첨부 다시 시도.
pub(super) fn tick(node: &Arc<Node>) {
    let spaces: Vec<(SpaceId, Arc<HostedRoster>)> = node
        .archive
        .host
        .hosted
        .read()
        .iter()
        .map(|(s, h)| (*s, h.rec.clone()))
        .collect();
    let now = Timestamp::now();
    for (space, rec) in &spaces {
        let mut erased = Vec::new();
        for c in &rec.roster.channels {
            let Some(ttl) = c.ttl_secs else { continue };
            let cutoff = Timestamp(now.0.saturating_sub(u64::from(ttl) * 1000));
            loop {
                match node.store.expire_channel_messages_authored(
                    space,
                    &c.channel,
                    cutoff,
                    MAX_EXPIRE_BATCH,
                ) {
                    Ok(ids) if !ids.is_empty() => erased.extend(ids),
                    _ => break,
                }
            }
        }
        if !erased.is_empty() {
            drop_event_refs(node, space, &erased);
            mark_recount(node, space);
        }
        erase_overdue(node, space, now);
    }
    recount_due(node);
    let dirty: Vec<SpaceId> = node
        .archive
        .host
        .hosted
        .read()
        .iter()
        .filter(|(_, h)| h.dirty)
        .map(|(s, _)| *s)
        .collect();
    for space in dirty {
        let mut hosted = node.archive.host.hosted.write();
        if let Some(h) = hosted.get_mut(&space) {
            h.dirty = false;
            if let Err(e) = save(node, &space, h) {
                debug!(error = %e, "보관 사용량 저장 실패");
            }
        }
    }
    start_due_fetches(node);
}

/// 명단에서 빠진 지 [`ERASE_GRACE`]가 지난 채널을 지웁니다.
fn erase_overdue(node: &Node, space: &SpaceId, now: Timestamp) {
    let grace = u64::try_from(ERASE_GRACE.as_millis()).unwrap_or(u64::MAX);
    let due: Vec<ChannelId> = node
        .archive
        .host
        .hosted
        .read()
        .get(space)
        .map(|h| {
            h.dropped
                .iter()
                .filter(|(_, d)| now.0.saturating_sub(d.since.0) >= grace)
                .map(|(c, _)| *c)
                .collect()
        })
        .unwrap_or_default();
    if due.is_empty() {
        return;
    }
    let _g = node.archive.host.lock.lock();
    for c in due {
        // 기다리는 사이 다시 보관하게 됐으면 그만둡니다.
        let erase = {
            let mut hosted = node.archive.host.hosted.write();
            let Some(h) = hosted.get_mut(space) else {
                return;
            };
            let still = h.rec.roster.channel(&c).is_none() && h.dropped.remove(&c).is_some();
            h.dirty |= still;
            still
        };
        if erase {
            erase_channel(node, space, &c);
        }
    }
}

/// 표시됐거나 [`RECOUNT_EVERY`]가 지난 공간의 사용량을 저장소에서 다시 셉니다.
fn recount_due(node: &Node) {
    let due: Vec<SpaceId> = node
        .archive
        .host
        .hosted
        .read()
        .iter()
        .filter(|(_, h)| h.recount || h.last_recount.elapsed() >= RECOUNT_EVERY)
        .map(|(s, _)| *s)
        .collect();
    for space in due {
        let Some(n) = measure(node, &space) else {
            continue;
        };
        if let Some(h) = node.archive.host.hosted.write().get_mut(&space) {
            h.recount = false;
            h.last_recount = Instant::now();
            if h.used_bytes != n {
                h.used_bytes = n;
                h.dirty = true;
            }
        }
    }
}

// ───────────── API ─────────────

/// `archive.offer`: 1회용 제안 링크를 만듭니다.
pub(super) async fn offer(
    node: &Arc<Node>,
    ttl: Duration,
    name: Option<String>,
) -> Result<(String, Option<Timestamp>)> {
    if !node.cfg.archive.enabled {
        return Err(Error::forbidden(
            "보관 전용 노드로 띄운 데몬이 아닙니다(yangpad --archive 또는 YANGPA_ARCHIVE=1)",
        ));
    }
    let me = node.me()?;
    // 만료된 제안을 지우고 수를 묶습니다.
    let offers = node.store.settings_with_prefix(OFFER_PREFIX)?;
    let mut live = 0;
    for (key, v) in offers {
        match serde_json::from_value::<Offer>(v) {
            Ok(o) if !o.expires_at.is_some_and(|t| t.is_past()) => live += 1,
            _ => {
                let _ = node.store.delete_setting(&key);
            }
        }
    }
    if live >= MAX_OFFERS {
        return Err(Error::conflict(format!(
            "아직 쓰지 않은 제안 링크가 {MAX_OFFERS}개입니다(만료를 기다리세요)"
        )));
    }
    let ttl = ttl.min(OFFER_MAX_TTL);
    let expires_at =
        Some(Timestamp(Timestamp::now().0.saturating_add(
            u64::try_from(ttl.as_millis()).unwrap_or(u64::MAX),
        )));
    let contact = crate::crews::my_contact(node).await;
    let mut link = ArchiveOfferLink::new(me.user(), me.device_id(), contact, expires_at);
    if let Some(n) = name {
        let n = n.trim().to_owned();
        crate::crews::check_text(&n, yangpa_core::archive::MAX_NAME_CHARS, "보관 노드 이름")?;
        link.name_hint = n;
    }
    let url = link.to_url()?;
    node.store.put_setting(
        &offer_key(&token_hash(&link.token)),
        &Offer {
            created_at: Timestamp::now(),
            expires_at,
        },
    )?;
    Ok((url, expires_at))
}
