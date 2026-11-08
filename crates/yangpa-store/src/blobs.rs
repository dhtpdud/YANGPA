//! 블롭 저장소: 이미 암호화된 블롭 조각을 `<데이터>/blobs/<앞 2 hex>/<나머지 hex>` 파일에 두고,
//! 가진 조각 비트맵·크기·마지막 접근 시각은 DB에 둡니다.
//!
//! 비트맵은 BitTorrent처럼 조각 i가 바이트 i/8의 최상위 비트부터(`0x80 >> (i % 8)`) 차례로 들어갑니다.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rusqlite::{OptionalExtension, params};
use yangpa_core::blob::BlobManifest;
use yangpa_core::ids::{BlobId, SpaceId};
use yangpa_core::model::SecurityLevel;
use yangpa_core::time::Timestamp;

use crate::db::{Store, from_sql_u64, to_sql_u64, ts_from_sql, ts_to_sql};
use crate::error::{Result, StoreError};

/// 마지막 접근 시각을 이보다 자주 갱신하지 않습니다(읽을 때마다 쓰기가 생기지 않게).
const ACCESS_TOUCH_MS: i64 = 60_000;

/// 디스크에 저장해도 되는 블롭. 휘발성 채널의 첨부로는 만들 수 없습니다(휘발성 첨부는 [`crate::VolatileStore`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PersistentBlob {
    manifest: BlobManifest,
    id: BlobId,
}

impl PersistentBlob {
    /// `security`는 첨부가 속한 채널의 보안 등급입니다(이모지·프로필 이미지처럼 채널과 무관하면 `Normal`).
    pub fn new(manifest: BlobManifest, security: SecurityLevel) -> Result<Self> {
        if security == SecurityLevel::Volatile {
            return Err(StoreError::VolatileRejected);
        }
        manifest
            .check()
            .map_err(|_| StoreError::InvalidInput("블롭 매니페스트"))?;
        let id = manifest.id();
        Ok(PersistentBlob { manifest, id })
    }

    pub fn id(&self) -> BlobId {
        self.id
    }

    pub fn manifest(&self) -> &BlobManifest {
        &self.manifest
    }
}

/// 조각 쓰기 결과.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChunkWrite {
    /// 새로 저장함. `complete`면 이 조각으로 블롭이 다 모였습니다.
    Stored { complete: bool },
    /// 이미 가진 조각.
    AlreadyHave,
}

/// 블롭 상태.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlobStatus {
    pub id: BlobId,
    pub size: u64,
    pub chunk_size: u32,
    pub chunk_count: u32,
    pub have_count: u32,
    /// 가진 조각의 바이트 합.
    pub stored_bytes: u64,
    pub complete: bool,
    /// 고정(내가 올린 첨부, 쓰는 이모지 등): 자동 정리 대상에서 뺌.
    pub pinned: bool,
    pub last_access: Timestamp,
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

fn bit_get(bm: &[u8], i: u32) -> bool {
    bm.get((i / 8) as usize)
        .is_some_and(|b| b & (0x80 >> (i % 8)) != 0)
}

fn bit_set(bm: &mut [u8], i: u32, on: bool) {
    if let Some(b) = bm.get_mut((i / 8) as usize) {
        if on {
            *b |= 0x80 >> (i % 8);
        } else {
            *b &= !(0x80 >> (i % 8));
        }
    }
}

/// 블롭 파일 디렉터리. 파일 이름은 블롭 ID(매니페스트 해시) hex입니다.
#[derive(Debug)]
pub struct BlobStore {
    root: PathBuf,
}

impl BlobStore {
    pub fn new(root: PathBuf) -> std::io::Result<Self> {
        std::fs::create_dir_all(&root)?;
        Ok(BlobStore { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn path_for(&self, id: &BlobId) -> PathBuf {
        let h = hex(id.as_bytes());
        self.root.join(&h[..2]).join(&h[2..])
    }

    /// 파일의 `offset` 위치에 씁니다(파일이 없으면 만듦, 중간은 비워 둠).
    pub fn write_at(&self, id: &BlobId, offset: u64, data: &[u8]) -> std::io::Result<()> {
        let path = self.path_for(id);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        f.seek(SeekFrom::Start(offset))?;
        f.write_all(data)?;
        Ok(())
    }

    /// `offset`부터 `len`바이트. 파일이 없거나 짧으면 `None`.
    pub fn read_at(
        &self,
        id: &BlobId,
        offset: u64,
        len: usize,
    ) -> std::io::Result<Option<Vec<u8>>> {
        let mut f = match File::open(self.path_for(id)) {
            Ok(f) => f,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        f.seek(SeekFrom::Start(offset))?;
        let mut buf = vec![0u8; len];
        match f.read_exact(&mut buf) {
            Ok(()) => Ok(Some(buf)),
            Err(e) if e.kind() == ErrorKind::UnexpectedEof => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// 파일을 지웁니다. 있었으면 true.
    pub fn remove(&self, id: &BlobId) -> std::io::Result<bool> {
        let path = self.path_for(id);
        match std::fs::remove_file(&path) {
            Ok(()) => {
                // 빈 하위 디렉터리는 정리(다른 파일이 있으면 실패하므로 무시).
                if let Some(dir) = path.parent() {
                    let _ = std::fs::remove_dir(dir);
                }
                Ok(true)
            }
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }

    pub fn file_size(&self, id: &BlobId) -> std::io::Result<u64> {
        match std::fs::metadata(self.path_for(id)) {
            Ok(m) => Ok(m.len()),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(0),
            Err(e) => Err(e),
        }
    }
}

/// 최근 쓴 매니페스트 캐시(조각 수가 많으면 매니페스트가 수 MB라 조각마다 다시 풀지 않게).
pub(crate) struct ManifestCache {
    cap: usize,
    tick: u64,
    map: HashMap<BlobId, (Arc<BlobManifest>, u64)>,
}

impl ManifestCache {
    pub(crate) fn new(cap: usize) -> Self {
        ManifestCache {
            cap: cap.max(1),
            tick: 0,
            map: HashMap::new(),
        }
    }

    fn get(&mut self, id: &BlobId) -> Option<Arc<BlobManifest>> {
        self.tick += 1;
        let tick = self.tick;
        self.map.get_mut(id).map(|(m, t)| {
            *t = tick;
            m.clone()
        })
    }

    fn insert(&mut self, id: BlobId, m: Arc<BlobManifest>) {
        self.tick += 1;
        if self.map.len() >= self.cap
            && !self.map.contains_key(&id)
            && let Some(oldest) = self
                .map
                .iter()
                .min_by_key(|(_, (_, t))| *t)
                .map(|(k, _)| *k)
        {
            self.map.remove(&oldest);
        }
        self.map.insert(id, (m, self.tick));
    }

    pub(crate) fn remove(&mut self, id: &BlobId) {
        self.map.remove(id);
    }
}

fn read_status(r: &rusqlite::Row<'_>) -> rusqlite::Result<BlobStatus> {
    Ok(BlobStatus {
        id: BlobId(r.get(0)?),
        size: from_sql_u64(r.get(1)?),
        chunk_size: u32::try_from(r.get::<_, i64>(2)?).unwrap_or(0),
        chunk_count: u32::try_from(r.get::<_, i64>(3)?).unwrap_or(0),
        have_count: u32::try_from(r.get::<_, i64>(4)?).unwrap_or(0),
        stored_bytes: from_sql_u64(r.get(5)?),
        complete: r.get(6)?,
        pinned: r.get(7)?,
        last_access: ts_from_sql(r.get(8)?),
    })
}

const STATUS_SELECT: &str = "SELECT blob_id, size, chunk_size, chunk_count, have_count, stored_bytes, complete, \
                             pinned, last_access FROM blobs";

impl Store {
    /// 블롭 파일 디렉터리.
    pub fn blob_files(&self) -> &BlobStore {
        &self.blob_files
    }

    /// 블롭을 등록하고 공간에 묶습니다(닫힌 스웜: 그 공간 멤버에게만 제공). 새 블롭이면 true.
    pub fn blob_register(&self, space: &SpaceId, blob: &PersistentBlob) -> Result<bool> {
        let m = &blob.manifest;
        let manifest = postcard::to_stdvec(m)?;
        let count = m.chunks.len();
        let have = vec![0u8; count.div_ceil(8)];
        let now = ts_to_sql(Timestamp::now());
        let created = self.write(|tx| {
            let n = tx
                .prepare_cached(
                    "INSERT OR IGNORE INTO blobs (blob_id, manifest, size, chunk_size, chunk_count, have, have_count, \
                     stored_bytes, complete, pinned, created_at, last_access) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, 0, ?7, 0, ?8, ?8)",
                )?
                .execute(params![
                    blob.id.as_bytes(),
                    manifest,
                    to_sql_u64(m.size),
                    m.chunk_size,
                    count as i64,
                    have,
                    count == 0,
                    now
                ])?;
            tx.prepare_cached("INSERT OR IGNORE INTO blob_spaces (blob_id, space) VALUES (?1, ?2)")?
                .execute(params![blob.id.as_bytes(), space.as_bytes()])?;
            Ok(n > 0)
        })?;
        self.manifests.lock().insert(blob.id, Arc::new(m.clone()));
        Ok(created)
    }

    pub fn blob_manifest(&self, id: &BlobId) -> Result<Option<Arc<BlobManifest>>> {
        if let Some(m) = self.manifests.lock().get(id) {
            return Ok(Some(m));
        }
        let bytes: Option<Vec<u8>> = self.read(|c| {
            Ok(
                c.prepare_cached("SELECT manifest FROM blobs WHERE blob_id = ?1")?
                    .query_row([id.as_bytes()], |r| r.get(0))
                    .optional()?,
            )
        })?;
        let Some(bytes) = bytes else { return Ok(None) };
        let m: BlobManifest =
            postcard::from_bytes(&bytes).map_err(|_| StoreError::Corrupt("블롭 매니페스트"))?;
        if m.id() != *id {
            return Err(StoreError::Corrupt("블롭 매니페스트"));
        }
        let m = Arc::new(m);
        self.manifests.lock().insert(*id, m.clone());
        Ok(Some(m))
    }

    fn blob_bitmap(&self, id: &BlobId) -> Result<Option<Vec<u8>>> {
        self.read(|c| {
            Ok(
                c.prepare_cached("SELECT have FROM blobs WHERE blob_id = ?1")?
                    .query_row([id.as_bytes()], |r| r.get(0))
                    .optional()?,
            )
        })
    }

    /// 받은 조각을 검증해 저장합니다. 매니페스트 해시와 맞지 않으면 [`StoreError::InvalidInput`]
    /// (보낸 시더 점수를 깎을 근거).
    pub fn blob_write_chunk(&self, id: &BlobId, index: u32, data: &[u8]) -> Result<ChunkWrite> {
        let m = self.blob_manifest(id)?.ok_or(StoreError::NotFound)?;
        if !m.verify_chunk(index, data) {
            return Err(StoreError::InvalidInput("조각 해시 불일치"));
        }
        if self.blob_bitmap(id)?.is_some_and(|bm| bit_get(&bm, index)) {
            return Ok(ChunkWrite::AlreadyHave);
        }
        let offset = u64::from(index) * u64::from(m.chunk_size);
        self.blob_files.write_at(id, offset, data)?;
        let now = ts_to_sql(Timestamp::now());
        self.write(|tx| {
            let row: Option<(Vec<u8>, i64, i64)> = tx
                .prepare_cached("SELECT have, have_count, chunk_count FROM blobs WHERE blob_id = ?1")?
                .query_row([id.as_bytes()], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .optional()?;
            // 쓰는 사이 블롭이 지워졌으면 파일 조각도 정리합니다.
            let Some((mut bm, have_count, chunk_count)) = row else {
                self.blob_files.remove(id)?;
                return Err(StoreError::NotFound);
            };
            if bit_get(&bm, index) {
                return Ok(ChunkWrite::AlreadyHave);
            }
            bit_set(&mut bm, index, true);
            let have_count = have_count + 1;
            let complete = have_count >= chunk_count;
            tx.prepare_cached(
                "UPDATE blobs SET have = ?2, have_count = ?3, stored_bytes = stored_bytes + ?4, complete = ?5, \
                 last_access = ?6 WHERE blob_id = ?1",
            )?
            .execute(params![id.as_bytes(), bm, have_count, data.len() as i64, complete, now])?;
            Ok(ChunkWrite::Stored { complete })
        })
    }

    /// 가진 조각을 읽습니다. 파일이 손상돼 해시가 맞지 않으면 비트를 지우고 `None`(다시 받게).
    pub fn blob_read_chunk(&self, id: &BlobId, index: u32) -> Result<Option<Vec<u8>>> {
        let Some(m) = self.blob_manifest(id)? else {
            return Ok(None);
        };
        if index as usize >= m.chunks.len()
            || !self.blob_bitmap(id)?.is_some_and(|bm| bit_get(&bm, index))
        {
            return Ok(None);
        }
        let offset = u64::from(index) * u64::from(m.chunk_size);
        let len =
            usize::try_from(m.chunk_len(index)).map_err(|_| StoreError::Corrupt("조각 길이"))?;
        let data = self.blob_files.read_at(id, offset, len)?;
        match data {
            Some(d) if m.verify_chunk(index, &d) => {
                let now = ts_to_sql(Timestamp::now());
                self.write(|tx| {
                    tx.prepare_cached(
                        "UPDATE blobs SET last_access = ?2 WHERE blob_id = ?1 AND last_access < ?3",
                    )?
                    .execute(params![
                        id.as_bytes(),
                        now,
                        now - ACCESS_TOUCH_MS
                    ])?;
                    Ok(())
                })?;
                Ok(Some(d))
            }
            _ => {
                tracing::warn!(blob = %id.short(), index, "블롭 조각 손상: 다시 받도록 표시");
                self.write(|tx| {
                    let row: Option<Vec<u8>> = tx
                        .prepare_cached("SELECT have FROM blobs WHERE blob_id = ?1")?
                        .query_row([id.as_bytes()], |r| r.get(0))
                        .optional()?;
                    if let Some(mut bm) = row
                        && bit_get(&bm, index)
                    {
                        bit_set(&mut bm, index, false);
                        tx.prepare_cached(
                            "UPDATE blobs SET have = ?2, have_count = have_count - 1, \
                             stored_bytes = MAX(stored_bytes - ?3, 0), complete = 0 WHERE blob_id = ?1",
                        )?
                        .execute(params![id.as_bytes(), bm, len as i64])?;
                    }
                    Ok(())
                })?;
                Ok(None)
            }
        }
    }

    pub fn blob_status(&self, id: &BlobId) -> Result<Option<BlobStatus>> {
        self.read(|c| {
            Ok(
                c.prepare_cached(&format!("{STATUS_SELECT} WHERE blob_id = ?1"))?
                    .query_row([id.as_bytes()], read_status)
                    .optional()?,
            )
        })
    }

    /// 가진 조각 비트맵과 전체 조각 수(`BlobHave` 응답).
    pub fn blob_have(&self, id: &BlobId) -> Result<Option<(Vec<u8>, u32)>> {
        self.read(|c| {
            Ok(
                c.prepare_cached("SELECT have, chunk_count FROM blobs WHERE blob_id = ?1")?
                    .query_row([id.as_bytes()], |r| {
                        Ok((
                            r.get::<_, Vec<u8>>(0)?,
                            u32::try_from(r.get::<_, i64>(1)?).unwrap_or(0),
                        ))
                    })
                    .optional()?,
            )
        })
    }

    /// 블롭이 그 공간에 속하는지(닫힌 스웜 검사).
    pub fn blob_in_space(&self, id: &BlobId, space: &SpaceId) -> Result<bool> {
        self.read(|c| {
            Ok(c.prepare_cached(
                "SELECT EXISTS(SELECT 1 FROM blob_spaces WHERE blob_id = ?1 AND space = ?2)",
            )?
            .query_row(params![id.as_bytes(), space.as_bytes()], |r| r.get(0))?)
        })
    }

    pub fn blobs_in_space(&self, space: &SpaceId) -> Result<Vec<BlobId>> {
        self.read(|c| {
            let mut stmt = c.prepare_cached(
                "SELECT blob_id FROM blob_spaces WHERE space = ?1 ORDER BY blob_id",
            )?;
            let rows = stmt.query_map([space.as_bytes()], |r| Ok(BlobId(r.get(0)?)))?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })
    }

    pub fn blob_set_pinned(&self, id: &BlobId, pinned: bool) -> Result<bool> {
        self.write(|tx| {
            Ok(tx
                .prepare_cached("UPDATE blobs SET pinned = ?2 WHERE blob_id = ?1")?
                .execute(params![id.as_bytes(), pinned])?
                > 0)
        })
    }

    /// 블롭(기록과 파일)을 지웁니다. 있었으면 true.
    pub fn blob_delete(&self, id: &BlobId) -> Result<bool> {
        let had = self.write(|tx| {
            Ok(tx
                .prepare_cached("DELETE FROM blobs WHERE blob_id = ?1")?
                .execute([id.as_bytes()])?
                > 0)
        })?;
        self.manifests.lock().remove(id);
        let had_file = self.blob_files.remove(id)?;
        Ok(had || had_file)
    }

    /// 블롭 조각 데이터만 지웁니다(파일 삭제, 비트맵 초기화). 등록·매니페스트·공간 소속은 남아서 나중에
    /// 다시 받을 수 있습니다. 지운 바이트 수를 돌려줍니다(없거나 이미 비었으면 0).
    pub fn blob_drop_data(&self, id: &BlobId) -> Result<u64> {
        let freed = self.write(|tx| {
            let row: Option<(i64, i64)> = tx
                .prepare_cached("SELECT stored_bytes, chunk_count FROM blobs WHERE blob_id = ?1")?
                .query_row([id.as_bytes()], |r| Ok((r.get(0)?, r.get(1)?)))
                .optional()?;
            let Some((stored, chunk_count)) = row else {
                return Ok(0);
            };
            let count = usize::try_from(chunk_count).map_err(|_| StoreError::Corrupt("조각 수"))?;
            tx.prepare_cached(
                "UPDATE blobs SET have = ?2, have_count = 0, stored_bytes = 0, complete = ?3 WHERE blob_id = ?1",
            )?
            .execute(params![id.as_bytes(), vec![0u8; count.div_ceil(8)], count == 0])?;
            // 쓰기 잠금 안에서 지워, 지운 뒤 비트만 다시 켜지는 일이 없게 합니다.
            self.blob_files.remove(id)?;
            Ok(from_sql_u64(stored))
        })?;
        Ok(freed)
    }

    /// 저장된 블롭 조각 바이트 합.
    pub fn blob_total_size(&self) -> Result<u64> {
        self.read(|c| {
            let n: i64 = c
                .prepare_cached("SELECT COALESCE(SUM(stored_bytes), 0) FROM blobs")?
                .query_row([], |r| r.get(0))?;
            Ok(from_sql_u64(n))
        })
    }

    /// 전체 크기가 `max_bytes` 이하가 될 때까지 오래 안 쓴(고정되지 않은) 블롭의 조각 데이터부터 지웁니다
    /// ([`Store::blob_drop_data`]: 등록·공간 소속은 남아 다시 받을 수 있음). 데이터가 없는 블롭은 건드리지 않고,
    /// `protect`에 있는 블롭(지금 보는 화면 등)은 건너뜁니다. 비운 블롭을 돌려줍니다.
    pub fn blob_evict_lru(&self, max_bytes: u64, protect: &[BlobId]) -> Result<Vec<BlobId>> {
        let mut total = self.blob_total_size()?;
        if total <= max_bytes {
            return Ok(Vec::new());
        }
        let candidates: Vec<BlobId> = self.read(|c| {
            let mut stmt = c.prepare_cached(
                "SELECT blob_id FROM blobs WHERE pinned = 0 AND stored_bytes > 0 ORDER BY last_access, blob_id",
            )?;
            let rows = stmt.query_map([], |r| Ok(BlobId(r.get(0)?)))?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })?;
        let mut evicted = Vec::new();
        for id in candidates {
            if total <= max_bytes {
                break;
            }
            if protect.contains(&id) {
                continue;
            }
            let freed = self.blob_drop_data(&id)?;
            if freed > 0 {
                total = total.saturating_sub(freed);
                evicted.push(id);
            }
        }
        Ok(evicted)
    }

    /// 조각 데이터를 가진 블롭 전부, 오래 안 쓴 것부터(같으면 블롭 ID 순). 데몬의 저장 공간 정리가 고정·쓰임을
    /// 따로 보고 고릅니다.
    pub fn blob_cache_entries(&self) -> Result<Vec<BlobStatus>> {
        self.read(|c| {
            let mut stmt = c.prepare_cached(&format!(
                "{STATUS_SELECT} WHERE stored_bytes > 0 ORDER BY last_access, blob_id"
            ))?;
            let rows = stmt.query_map([], read_status)?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })
    }

    /// 등록된 블롭 ID 전부(조각이 없어도). 미리 받기 보충이 "한 번도 받으려 한 적 없는" 블롭을 한 번에 가릴 때 씁니다.
    pub fn blob_ids(&self) -> Result<Vec<BlobId>> {
        self.read(|c| {
            let mut stmt = c.prepare_cached("SELECT blob_id FROM blobs")?;
            let rows = stmt.query_map([], |r| Ok(BlobId(r.get(0)?)))?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })
    }

    /// 마지막 접근 시각을 정합니다(시험에서 정리 순서를 정할 때). 있었으면 true.
    pub fn blob_set_last_access(&self, id: &BlobId, at: Timestamp) -> Result<bool> {
        self.write(|tx| {
            Ok(tx
                .prepare_cached("UPDATE blobs SET last_access = ?2 WHERE blob_id = ?1")?
                .execute(params![id.as_bytes(), ts_to_sql(at)])?
                > 0)
        })
    }
}
