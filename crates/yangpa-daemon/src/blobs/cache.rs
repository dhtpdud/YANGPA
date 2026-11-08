//! 받은 첨부의 저장 공간 정리(LRU).
//!
//! 한도는 "정리 대상"에만 겁니다. 조각 데이터를 가진 블롭을 셋으로 나눕니다.
//! - 늘 남김: 저장소 `pinned`(내가 올린 첨부·이모지·아이콘·프로필 이미지, 받은 이모지), 프로필 이미지
//!   ([`PROFILE_SPACE`]), 아는 공간의 이모지·아이콘. 썸네일은 메시지 본문에 실려 블롭이 아니므로 늘 남습니다.
//! - 고정: 사용자가 `blob.pin`으로 고정한 블롭(설정 `blob.pin.<블롭>`). 저장소 `pinned`와 따로 둬, 고정을 풀어도
//!   내가 올린 첨부가 정리 대상이 되지 않게 합니다.
//! - 정리 대상: 나머지(남에게서 받은 첨부).
//!
//! 정리 대상 중 받는 중이거나 받을 블롭, 최근 [`SERVING_HOLD_MS`] 안에 남에게 내주거나 로컬 API로 읽은 블롭은
//! 건너뛰고, 오래 안 쓴 것(`last_access`)부터 [`Store::blob_drop_data`]로 비웁니다. 등록·매니페스트·공간 소속은
//! 남아 다시 받을 수 있고, 비운 뒤에는 `BlobHave`가 빈 비트맵으로 답하므로 남이 보는 시더 수가 사실대로 줄어듭니다.
//!
//! 언제: 받기 시작할 때 자리를 만들고([`admit_download`]), 정리 주기마다·한도를 바꿀 때 한도와 디스크 여유를
//! 맞춥니다([`enforce`]).
//!
//! [`Store::blob_drop_data`]: yangpa_store::Store::blob_drop_data

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use tracing::debug;
use yangpa_core::blob::BlobManifest;
use yangpa_core::ids::{BlobId, SpaceId};
use yangpa_core::time::Timestamp;
use yangpa_rpc::api::BlobCacheView;
use yangpa_store::BlobStatus;

use super::{PROFILE_SPACE, Want, now_ms};
use crate::error::Result;
use crate::node::Node;

const LIMIT_KEY: &str = "blob.cache.limit";
const MIN_FREE_KEY: &str = "blob.cache.min_free";
const PIN_PREFIX: &str = "blob.pin.";
/// 남에게 내주거나 로컬 API로 읽은 뒤 이만큼은 비우지 않습니다(받아 가는 피어, 재생 중인 동영상).
const SERVING_HOLD_MS: u64 = 30_000;
/// 내주는 중 기록 상한(넘으면 오래된 것부터 버림).
const MAX_SERVING: usize = 4_096;

/// 사용자 고정(`blob.pin.<블롭>`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Pin {
    pub space: SpaceId,
    pub at: Timestamp,
}

/// 저장 공간 정리 상태(`BlobState::cache`).
#[derive(Default)]
pub(crate) struct CacheState {
    pins: RwLock<HashMap<BlobId, Pin>>,
    /// 블롭 → 마지막으로 내주거나 읽은 시각.
    serving: Mutex<HashMap<BlobId, u64>>,
    /// 정리를 한 번에 하나만 합니다(받기 시작과 정리 주기가 같은 자리를 두 번 세지 않게).
    run: Mutex<()>,
    evicted_count: AtomicU64,
    evicted_bytes: AtomicU64,
    /// 시험용: 디스크 여유 공간을 이 값으로 봅니다.
    free_override: Mutex<Option<u64>>,
}

/// 한도(설정 표의 값이 우선, 없으면 시작 옵션).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Limits {
    pub limit: u64,
    pub min_free: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Class {
    /// 정리 대상.
    Cache,
    Pinned,
    Kept,
}

struct Entry {
    st: BlobStatus,
    class: Class,
}

// ───────────── 시작·고정 ─────────────

/// 저장된 고정을 읽습니다(`blobs::start`).
pub(crate) fn load(node: &Node) {
    let rows = node
        .store
        .settings_with_prefix(PIN_PREFIX)
        .unwrap_or_default();
    let mut pins = node.blobs.cache.pins.write();
    for (key, value) in rows {
        if let (Ok(blob), Ok(p)) = (
            key[PIN_PREFIX.len()..].parse::<BlobId>(),
            serde_json::from_value::<Pin>(value),
        ) {
            pins.insert(blob, p);
        }
    }
}

fn pin_key(blob: &BlobId) -> String {
    format!("{PIN_PREFIX}{blob}")
}

/// 고정합니다. 이미 고정돼 있으면 시각만 그대로 둡니다.
pub(crate) fn pin(node: &Node, space: SpaceId, blob: BlobId) -> Result<()> {
    if node.blobs.cache.pins.read().contains_key(&blob) {
        return Ok(());
    }
    let p = Pin {
        space,
        at: Timestamp::now(),
    };
    node.store.put_setting(&pin_key(&blob), &p)?;
    node.blobs.cache.pins.write().insert(blob, p);
    Ok(())
}

/// 고정을 풉니다. 고정돼 있었으면 true.
pub(crate) fn unpin(node: &Node, blob: &BlobId) -> Result<bool> {
    let had = node.blobs.cache.pins.write().remove(blob).is_some();
    if had {
        node.store.delete_setting(&pin_key(blob))?;
    }
    Ok(had)
}

/// 블롭을 지웠을 때(`blobs::cleanup`) 고정 기록도 지웁니다.
pub(crate) fn forget(node: &Node, blob: &BlobId) {
    let _ = unpin(node, blob);
}

pub(crate) fn is_pinned(node: &Node, blob: &BlobId) -> bool {
    node.blobs.cache.pins.read().contains_key(blob)
}

pub(crate) fn pins(node: &Node) -> Vec<(BlobId, Pin)> {
    let mut v: Vec<(BlobId, Pin)> = node
        .blobs
        .cache
        .pins
        .read()
        .iter()
        .map(|(b, p)| (*b, p.clone()))
        .collect();
    v.sort_by(|a, b| b.1.at.cmp(&a.1.at).then(a.0.cmp(&b.0)));
    v
}

/// 늘 남기는 블롭인지(`blob.status`의 `kept`).
pub(crate) fn is_kept(node: &Node, blob: &BlobId) -> bool {
    node.store
        .blob_status(blob)
        .ok()
        .flatten()
        .is_some_and(|s| s.pinned)
        || in_fixed_use(node, blob)
}

/// 프로필 이미지·공간 아이콘·이모지로 쓰이는지(그 쓰임에 늘 필요해 비우지 않음).
pub(crate) fn in_fixed_use(node: &Node, blob: &BlobId) -> bool {
    node.store
        .blob_in_space(blob, &PROFILE_SPACE)
        .unwrap_or(false)
        || node.space_ids().iter().any(|s| {
            node.space_state(s).is_some_and(|st| {
                st.settings.icon == Some(*blob) || st.emojis.values().any(|e| e.blob == *blob)
            })
        })
}

/// 남에게 조각을 내주거나 로컬 API로 읽었음을 기록합니다(잠시 정리하지 않음).
pub(crate) fn touch(node: &Node, blob: &BlobId) {
    let now = now_ms();
    let mut m = node.blobs.cache.serving.lock();
    if m.len() >= MAX_SERVING && !m.contains_key(blob) {
        m.retain(|_, at| now.saturating_sub(*at) < SERVING_HOLD_MS);
        if m.len() >= MAX_SERVING {
            m.clear();
        }
    }
    m.insert(*blob, now);
}

/// 시험용: 내주거나 읽은 기록이 있는지(시각과 무관).
pub(super) fn serving_recorded(node: &Node, blob: &BlobId) -> bool {
    node.blobs.cache.serving.lock().contains_key(blob)
}

/// 시험용: 풀 때까지 내주는 중으로 둡니다(`false`면 기록을 지움).
pub(super) fn hold_serving(node: &Node, blob: &BlobId, on: bool) {
    let mut m = node.blobs.cache.serving.lock();
    if on {
        // 앞날의 시각이면 `now - at`이 0이라 보류 시간과 무관하게 계속 건너뜁니다.
        m.insert(*blob, u64::MAX);
    } else {
        m.remove(blob);
    }
}

// ───────────── 한도 ─────────────

pub(crate) fn limits(node: &Node) -> Limits {
    let s = &node.cfg.sharing;
    Limits {
        limit: node
            .store
            .get_setting(LIMIT_KEY)
            .ok()
            .flatten()
            .unwrap_or(s.cache_limit_bytes),
        min_free: node
            .store
            .get_setting(MIN_FREE_KEY)
            .ok()
            .flatten()
            .unwrap_or(s.cache_min_free_bytes),
    }
}

/// 한도를 바꿉니다(빠진 값은 그대로, `reset`이면 시작 옵션으로).
pub(crate) fn set_limits(
    node: &Node,
    limit: Option<u64>,
    min_free: Option<u64>,
    reset: bool,
) -> Result<()> {
    if reset {
        node.store.delete_setting(LIMIT_KEY)?;
        node.store.delete_setting(MIN_FREE_KEY)?;
    }
    if let Some(v) = limit {
        node.store.put_setting(LIMIT_KEY, &v)?;
    }
    if let Some(v) = min_free {
        node.store.put_setting(MIN_FREE_KEY, &v)?;
    }
    Ok(())
}

/// 바꾸면 정리 대상에 쓸 자리가 줄어드는지(한도를 낮추거나 최소 여유 공간을 올림). 자동화 토큰은 이때 사람
/// 확인을 받습니다(`blob.cache_set_limit`이 바로 정리하므로 `blob.cache_clear`와 같은 결과를 낼 수 있음).
pub(crate) fn lowers_budget(
    node: &Node,
    limit: Option<u64>,
    min_free: Option<u64>,
    reset: bool,
) -> bool {
    let now = limits(node);
    let base = if reset {
        Limits {
            limit: node.cfg.sharing.cache_limit_bytes,
            min_free: node.cfg.sharing.cache_min_free_bytes,
        }
    } else {
        now
    };
    let next = Limits {
        limit: limit.unwrap_or(base.limit),
        min_free: min_free.unwrap_or(base.min_free),
    };
    next.limit < now.limit || next.min_free > now.min_free
}

/// 데이터 경로 디스크의 여유 공간.
pub(crate) fn disk_free(node: &Node) -> Option<u64> {
    if let Some(v) = *node.blobs.cache.free_override.lock() {
        return Some(v);
    }
    free_bytes(node.data_dir())
}

#[cfg(unix)]
fn free_bytes(path: &Path) -> Option<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: 모두 0인 statvfs는 올바른 값이고, 아래 호출이 성공하면 채워집니다.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: c는 NUL로 끝나는 경로, st는 호출 동안 살아 있는 쓰기 가능한 버퍼입니다.
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    // 필드 타입이 플랫폼마다 달라(u32·u64) 그대로 넓힙니다.
    #[allow(clippy::unnecessary_cast, clippy::useless_conversion)]
    let (avail, frsize) = (st.f_bavail as u64, st.f_frsize as u64);
    Some(avail.saturating_mul(frsize))
}

#[cfg(windows)]
fn free_bytes(path: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut avail = 0u64;
    // SAFETY: wide는 NUL로 끝나는 UTF-16 경로, avail은 호출 동안 살아 있고, 쓰지 않는 출력은 null입니다.
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut avail,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    (ok != 0).then_some(avail)
}

#[cfg(not(any(unix, windows)))]
fn free_bytes(_path: &Path) -> Option<u64> {
    None
}

// ───────────── 분류·비우기 ─────────────

/// 늘 남기는 블롭(저장소 `pinned`는 [`scan`]이 따로 봄): 프로필 이미지, 아는 공간의 이모지·아이콘.
fn kept_set(node: &Node) -> HashSet<BlobId> {
    let mut out: HashSet<BlobId> = node
        .store
        .blobs_in_space(&PROFILE_SPACE)
        .unwrap_or_default()
        .into_iter()
        .collect();
    for s in node.space_ids() {
        if let Some(st) = node.space_state(&s) {
            out.extend(st.emojis.values().map(|e| e.blob));
            out.extend(st.settings.icon);
        }
    }
    out
}

/// 데이터를 가진 블롭, 오래 안 쓴 것부터.
fn scan(node: &Node) -> Vec<Entry> {
    let entries = match node.store.blob_cache_entries() {
        Ok(v) => v,
        Err(e) => {
            debug!(error = %e, "저장 공간 목록 읽기 실패");
            return Vec::new();
        }
    };
    let kept = kept_set(node);
    let pins = node.blobs.cache.pins.read();
    entries
        .into_iter()
        .map(|st| {
            let class = if st.pinned || kept.contains(&st.id) {
                Class::Kept
            } else if pins.contains_key(&st.id) {
                Class::Pinned
            } else {
                Class::Cache
            };
            Entry { st, class }
        })
        .collect()
}

fn cache_bytes(entries: &[Entry]) -> u64 {
    entries
        .iter()
        .filter(|e| e.class == Class::Cache)
        .map(|e| e.st.stored_bytes)
        .sum()
}

/// 시험용: 정리 목록을 읽은 뒤에 `blob`을 고정하고 모두 비웁니다(고정과 정리가 겹치는 순서를 그대로 재현).
pub(super) fn clear_pinning_midway(node: &Node, space: SpaceId, blob: BlobId) -> (u32, u64) {
    let _run = node.blobs.cache.run.lock();
    let entries = scan(node);
    let _ = pin(node, space, blob);
    evict(node, &entries, u64::MAX, None)
}

/// 지금 비우면 안 되는 블롭: 받는 중·받을 블롭, 최근 내주거나 읽은 블롭.
fn busy(node: &Node) -> HashSet<BlobId> {
    let mut out: HashSet<BlobId> = {
        let m = node.blobs.mgr.lock();
        m.active.iter().chain(m.wants.keys()).copied().collect()
    };
    let now = now_ms();
    out.extend(
        node.blobs
            .cache
            .serving
            .lock()
            .iter()
            .filter(|(_, at)| now.saturating_sub(**at) < SERVING_HOLD_MS)
            .map(|(b, _)| *b),
    );
    out
}

/// 정리 대상 중 오래 안 쓴 것부터 `need`바이트 이상 비웁니다. 비운 (수, 바이트).
fn evict(node: &Node, entries: &[Entry], need: u64, except: Option<&BlobId>) -> (u32, u64) {
    if need == 0 {
        return (0, 0);
    }
    let busy = busy(node);
    let (mut count, mut freed) = (0u32, 0u64);
    for e in entries.iter().filter(|e| e.class == Class::Cache) {
        if freed >= need {
            break;
        }
        if busy.contains(&e.st.id) || except == Some(&e.st.id) {
            continue;
        }
        // 목록을 읽은 뒤 고정했을 수 있어 비우기 직전에 다시 봅니다. 읽기 잠금을 쥔 채 비워, 그 사이 들어온
        // `pin`은 비운 뒤에 기록되고 이어지는 `add_want`가 다시 받습니다(고정했는데 빈 채로 남지 않게).
        let pins = node.blobs.cache.pins.read();
        if pins.contains_key(&e.st.id) {
            continue;
        }
        let dropped = node.store.blob_drop_data(&e.st.id);
        drop(pins);
        match dropped {
            Ok(0) => {}
            Ok(n) => {
                count += 1;
                freed += n;
                debug!(blob = %e.st.id.short(), "저장 공간 정리: 오래 안 본 첨부를 비움");
            }
            Err(err) => debug!(blob = %e.st.id.short(), error = %err, "첨부 비우기 실패"),
        }
    }
    let c = &node.blobs.cache;
    c.evicted_count
        .fetch_add(u64::from(count), Ordering::Relaxed);
    c.evicted_bytes.fetch_add(freed, Ordering::Relaxed);
    (count, freed)
}

/// 한도·디스크 여유에 맞게 비웁니다(정리 주기, 한도를 바꾼 뒤). 블롭마다 저장소에 쓰고 파일을 지우므로
/// 비동기 작업 스레드가 아니라 `spawn_blocking`에서 부릅니다([`run_blocking`]).
pub(crate) fn enforce(node: &Node) -> (u32, u64) {
    let _run = node.blobs.cache.run.lock();
    let lim = limits(node);
    let entries = scan(node);
    let over = cache_bytes(&entries).saturating_sub(lim.limit);
    let short = disk_free(node).map_or(0, |f| lim.min_free.saturating_sub(f));
    evict(node, &entries, over.max(short), None)
}

/// 정리 대상을 모두 비웁니다(받는 중·내주는 중인 것은 빼고). `spawn_blocking`에서 부릅니다.
pub(crate) fn clear(node: &Node) -> (u32, u64) {
    let _run = node.blobs.cache.run.lock();
    let entries = scan(node);
    evict(node, &entries, u64::MAX, None)
}

/// 정리 작업을 블로킹 스레드에서 돌립니다(정리 잠금을 기다리거나 파일을 많이 지우는 동안 비동기 작업 스레드를
/// 막지 않게). 작업이 멈추면(패닉) `None`.
pub(crate) async fn run_blocking<T: Send + 'static>(
    node: &Arc<Node>,
    f: impl FnOnce(&Node) -> T + Send + 'static,
) -> Option<T> {
    let node = node.clone();
    tokio::task::spawn_blocking(move || f(&node)).await.ok()
}

/// 받기 시작 전 자리 확인(`fetch::download`, 매니페스트를 안 뒤, [`run_blocking`]으로). 아직 받지 않은 바이트만큼
/// 한도·디스크 여유를 넘으면 오래 안 본 첨부를 비웁니다. 자리가 생기지 않으면 false(자동 내려받기는 그만둠).
/// - 늘 남기거나 고정한 블롭(채널에 묶이지 않은 이모지·아이콘·프로필 이미지 포함)은 한도와 무관하게 받습니다.
/// - 미리 받기 보충(`backfill`)은 다른 첨부를 비우지 않습니다(비운 것을 다시 받는 일이 되풀이되지 않게).
pub(super) fn admit_download(
    node: &Node,
    blob: &BlobId,
    manifest: &BlobManifest,
    w: &Want,
) -> bool {
    let st = node.store.blob_status(blob).ok().flatten();
    let need = manifest
        .size
        .saturating_sub(st.as_ref().map_or(0, |s| s.stored_bytes));
    if need == 0 {
        return true;
    }
    let exempt = w.channel.is_none()
        || w.profile
        || st.as_ref().is_some_and(|s| s.pinned)
        || is_pinned(node, blob);
    let _run = node.blobs.cache.run.lock();
    let lim = limits(node);
    let entries = scan(node);
    let short = disk_free(node).map_or(0, |f| lim.min_free.saturating_add(need).saturating_sub(f));
    let over = if exempt {
        0
    } else {
        cache_bytes(&entries)
            .saturating_add(need)
            .saturating_sub(lim.limit)
    };
    let want = over.max(short);
    if want == 0 {
        return true;
    }
    if w.backfill {
        return exempt;
    }
    let (_, freed) = evict(node, &entries, want, Some(blob));
    exempt || freed >= want
}

/// 미리 받기 보충이 더 받아도 되는 바이트(한도 - 정리 대상 - 받을 자동 내려받기 크기). 디스크 여유도 봅니다.
pub(super) fn room_for_backfill(node: &Node) -> u64 {
    let lim = limits(node);
    let used = cache_bytes(&scan(node));
    let pending: u64 = node
        .blobs
        .mgr
        .lock()
        .wants
        .values()
        .filter(|w| !w.deferred && w.channel.is_some())
        .map(|w| w.size)
        .sum();
    let by_limit = lim.limit.saturating_sub(used).saturating_sub(pending);
    match disk_free(node) {
        Some(f) => by_limit.min(f.saturating_sub(lim.min_free).saturating_sub(pending)),
        None => by_limit,
    }
}

/// `blob.cache_status`.
pub(crate) fn status(node: &Node) -> BlobCacheView {
    let lim = limits(node);
    let entries = scan(node);
    let mut v = BlobCacheView {
        limit_bytes: lim.limit,
        min_free_bytes: lim.min_free,
        default_limit_bytes: node.cfg.sharing.cache_limit_bytes,
        default_min_free_bytes: node.cfg.sharing.cache_min_free_bytes,
        disk_free_bytes: disk_free(node),
        evicted_count: node.blobs.cache.evicted_count.load(Ordering::Relaxed),
        evicted_bytes: node.blobs.cache.evicted_bytes.load(Ordering::Relaxed),
        ..BlobCacheView::default()
    };
    for e in &entries {
        let (bytes, count) = match e.class {
            Class::Cache => (&mut v.cache_bytes, &mut v.cache_count),
            Class::Pinned => (&mut v.pinned_bytes, &mut v.pinned_count),
            Class::Kept => (&mut v.kept_bytes, &mut v.kept_count),
        };
        *bytes += e.st.stored_bytes;
        *count += 1;
    }
    v
}

// ───────────── 시험 고리 ─────────────

pub(super) fn set_free_override(node: &Node, v: Option<u64>) {
    *node.blobs.cache.free_override.lock() = v;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_bytes_of_existing_dir() {
        let d = std::env::temp_dir();
        // 유닉스·윈도우에서는 값이 나와야 함(용량은 기계마다 달라 확인하지 않음).
        if cfg!(any(unix, windows)) {
            assert!(free_bytes(&d).is_some());
        }
        assert!(free_bytes(&d.join("없는-경로-yangpa-x4")).is_none());
    }
}
