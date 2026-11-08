//! 누출 검사(`diag.leakcheck`): 휘발성 채널에 보낸 표식 문자열이 디스크 어딘가에 남았는지 찾습니다.
//!
//! 복호화한 DB의 모든 문자열·바이트 열, 저장 키로 봉인된 열(풀어서), 데이터 디렉터리 아래 모든 파일
//! (DB·WAL·블롭·로그 등, 제외 없음)을 훑습니다. 표식은 원문 바이트, 소문자 hex, UTF-16LE로 찾습니다.
//! 단 SQLite WAL 색인(`*-shm`)의 잠금 바이트 구간([`SHM_LOCK_BYTES`])만은 읽지 않습니다.

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::ops::Range;
use std::path::Path;
use std::time::Duration;

use rusqlite::Connection;
use rusqlite::types::ValueRef;
use yangpa_core::body::Secret32;

use crate::db::Store;
use crate::error::{Result, StoreError};

const FILE_CHUNK: usize = 1024 * 1024;
const MIN_MARKER_LEN: usize = 6;
const MAX_MARKER_LEN: usize = 1024;
/// SQLite WAL 색인(`-shm`)에서 연결들이 바이트 범위 잠금으로 쓰는 구간(`WalCkptInfo.aLock` 120..128, DMS 128)과
/// 그 뒤 계수기. Windows의 바이트 범위 잠금은 강제라서, 다른 연결이 쓰기·체크포인트 중 이 바이트를 배타적으로 잠근
/// 동안 이 구간을 읽으면 `ERROR_LOCK_VIOLATION`이 납니다. 잠금 자리·계수기뿐이라(행 내용 없음) 건너뜁니다.
const SHM_LOCK_BYTES: Range<u64> = 120..136;
/// 바이트 범위 잠금에 걸린 읽기를 다시 해 보는 횟수와 간격(다른 파일·짧은 잠금용).
const LOCK_RETRIES: u32 = 50;
const LOCK_RETRY_WAIT: Duration = Duration::from_millis(20);

fn needles(marker: &[u8]) -> Vec<Vec<u8>> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = vec![marker.to_vec()];
    out.push(
        marker
            .iter()
            .flat_map(|b| [HEX[(b >> 4) as usize], HEX[(b & 0x0f) as usize]])
            .collect(),
    );
    if let Ok(s) = std::str::from_utf8(marker) {
        out.push(s.encode_utf16().flat_map(u16::to_le_bytes).collect());
    }
    out
}

fn find(hay: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || hay.len() < needle.len() {
        return false;
    }
    let first = needle[0];
    let last_start = hay.len() - needle.len();
    let mut i = 0;
    while i <= last_start {
        match hay[i..=last_start].iter().position(|&b| b == first) {
            Some(p) => {
                let s = i + p;
                if &hay[s..s + needle.len()] == needle {
                    return true;
                }
                i = s + 1;
            }
            None => return false,
        }
    }
    false
}

fn contains_any(hay: &[u8], needles: &[Vec<u8>]) -> bool {
    needles.iter().any(|n| find(hay, n))
}

fn scan_columns(
    c: &Connection,
    needles: &[Vec<u8>],
    findings: &mut BTreeSet<String>,
) -> Result<()> {
    let tables: Vec<String> = {
        let mut stmt =
            c.prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    for t in tables {
        let mut stmt = c.prepare(&format!("SELECT * FROM \"{}\"", t.replace('"', "\"\"")))?;
        let names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            for (i, name) in names.iter().enumerate() {
                let hit = match row.get_ref(i)? {
                    ValueRef::Text(b) | ValueRef::Blob(b) => contains_any(b, needles),
                    _ => false,
                };
                if hit {
                    findings.insert(format!("db:{t}.{name}"));
                }
            }
        }
    }
    Ok(())
}

/// 저장 키로 봉인된 열을 풀어 훑습니다. (열 이름, SQL, AD 접두어)
fn scan_sealed(c: &Connection, needles: &[Vec<u8>], findings: &mut BTreeSet<String>) -> Result<()> {
    const SEALED: &[(&str, &str, &[u8])] = &[
        (
            "events.body_sealed",
            "SELECT e.id, e.body_sealed, k.key FROM events e JOIN msg_keys k ON k.event_id = e.id \
             WHERE e.body_sealed IS NOT NULL",
            b"",
        ),
        (
            "messages.content_sealed",
            "SELECT m.event_id, m.content_sealed, k.key FROM messages m JOIN msg_keys k ON k.event_id = m.event_id \
             WHERE m.content_sealed IS NOT NULL",
            b"yangpa/v1/store-view\0",
        ),
        (
            "pending_edits.sealed",
            "SELECT p.edit_id, p.sealed, k.key FROM pending_edits p JOIN msg_keys k ON k.event_id = p.edit_id",
            b"yangpa/v1/store-pending-edit\0",
        ),
    ];
    for (label, sql, ad_prefix) in SEALED {
        let mut stmt = c.prepare(sql)?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let id: Vec<u8> = row.get(0)?;
            let sealed: Vec<u8> = row.get(1)?;
            let key: [u8; 32] = row.get(2)?;
            let mut ad = ad_prefix.to_vec();
            ad.extend_from_slice(&id);
            match yangpa_crypto::open(&Secret32(key), &ad, &sealed) {
                Ok(plain) => {
                    if contains_any(&plain, needles) {
                        findings.insert(format!("db:{label}(복호화)"));
                    }
                }
                Err(_) => {
                    findings.insert(format!("db:{label}(복호화 실패: 검사 불가)"));
                }
            }
        }
    }
    Ok(())
}

/// Windows에서 다른 핸들이 배타적으로 잠근 바이트 범위를 읽으려 할 때(`ERROR_LOCK_VIOLATION`).
fn is_lock_violation(e: &std::io::Error) -> bool {
    cfg!(windows) && e.raw_os_error() == Some(33)
}

/// 바이트 범위 잠금에 걸리면 잠깐 기다렸다 다시 읽습니다(SQLite 잠금은 짧게 쥠).
fn read_retrying(f: &mut impl Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut tries = 0;
    loop {
        match f.read(buf) {
            Err(e) if is_lock_violation(&e) && tries < LOCK_RETRIES => {
                tries += 1;
                std::thread::sleep(LOCK_RETRY_WAIT);
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            r => return r,
        }
    }
}

/// SQLite WAL 색인 파일(`<DB>-shm`).
fn is_wal_index(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.ends_with("-shm"))
}

fn scan_file(path: &Path, needles: &[Vec<u8>]) -> std::io::Result<bool> {
    let skip = is_wal_index(path).then_some(SHM_LOCK_BYTES);
    scan_reader(File::open(path)?, needles, skip)
}

/// `skip` 구간은 읽지 않고(그 앞뒤를 이어 붙여 찾지도 않음) 나머지에서 표식을 찾습니다.
fn scan_reader(
    mut f: impl Read + Seek,
    needles: &[Vec<u8>],
    skip: Option<Range<u64>>,
) -> std::io::Result<bool> {
    let overlap = needles
        .iter()
        .map(Vec::len)
        .max()
        .unwrap_or(1)
        .saturating_sub(1);
    let mut buf = vec![0u8; FILE_CHUNK + overlap];
    let mut carry = 0usize;
    let mut pos = 0u64;
    loop {
        let mut want = buf.len() - carry;
        if let Some(r) = &skip {
            if r.contains(&pos) {
                f.seek(SeekFrom::Start(r.end))?;
                pos = r.end;
                carry = 0;
                continue;
            }
            if pos < r.start {
                want = want.min(usize::try_from(r.start - pos).unwrap_or(usize::MAX));
            }
        }
        let n = read_retrying(&mut f, &mut buf[carry..carry + want])?;
        if n == 0 {
            return Ok(false);
        }
        pos += n as u64;
        let filled = carry + n;
        if contains_any(&buf[..filled], needles) {
            return Ok(true);
        }
        // 조각 경계에 걸친 표식을 놓치지 않게 끝부분을 남깁니다.
        let keep = overlap.min(filled);
        buf.copy_within(filled - keep..filled, 0);
        carry = keep;
    }
}

fn walk(
    root: &Path,
    dir: &Path,
    needles: &[Vec<u8>],
    findings: &mut BTreeSet<String>,
) -> std::io::Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        // 심볼릭 링크는 따라가지 않습니다(데이터 디렉터리 밖을 훑거나 순환하지 않게).
        let meta = std::fs::symlink_metadata(&path)?;
        let rel = || {
            path.strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/")
        };
        if meta.is_dir() {
            walk(root, &path, needles, findings)?;
        } else if meta.is_file() && meta.len() > 0 {
            // 빈 파일에는 표식이 없습니다. 데몬의 잠금 파일(`yangpad.lock`)은 비어 있고, 윈도에서는 잠긴 동안
            // 읽기도 막혀(os error 33) 열어 보면 검사 전체가 실패합니다.
            match scan_file(&path, needles) {
                Ok(true) => {
                    findings.insert(format!("file:{}", rel()));
                }
                Ok(false) => {}
                // 검사 중 지워진 파일(WAL 정리 등)은 건너뜁니다.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
    }
    Ok(())
}

/// 데이터 디렉터리 밖의 파일(예: 따로 둔 로그 파일)에 표식이 있는지. 찾는 방식은 [`Store::scan_for_marker`]와 같습니다.
pub fn file_contains_marker(path: &Path, marker: &[u8]) -> std::io::Result<bool> {
    if marker.len() < MIN_MARKER_LEN || marker.len() > MAX_MARKER_LEN {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "표식 길이(6~1024바이트)",
        ));
    }
    scan_file(path, &needles(marker))
}

/// 조각으로 나눠 들어오는 평문에서 표식을 찾습니다(조각 경계에 걸친 표식 포함). 남겨 둔 끝부분은 평문이라 해제할
/// 때 0으로 덮어씁니다.
struct MarkerScanner<'a> {
    needles: &'a [Vec<u8>],
    carry: zeroize::Zeroizing<Vec<u8>>,
}

impl<'a> MarkerScanner<'a> {
    fn new(needles: &'a [Vec<u8>]) -> Self {
        MarkerScanner {
            needles,
            carry: zeroize::Zeroizing::new(Vec::new()),
        }
    }

    fn feed(&mut self, bytes: &[u8]) -> bool {
        let overlap = self
            .needles
            .iter()
            .map(Vec::len)
            .max()
            .unwrap_or(1)
            .saturating_sub(1);
        let mut buf = zeroize::Zeroizing::new(Vec::with_capacity(self.carry.len() + bytes.len()));
        buf.extend_from_slice(&self.carry);
        buf.extend_from_slice(bytes);
        if contains_any(&buf, self.needles) {
            return true;
        }
        let keep = overlap.min(buf.len());
        self.carry.clear();
        self.carry.extend_from_slice(&buf[buf.len() - keep..]);
        false
    }

    /// 이어지지 않는 조각(빠진 조각 뒤)은 경계를 잇지 않습니다.
    fn reset(&mut self) {
        self.carry.clear();
    }
}

impl Store {
    /// 키를 아는 디스크 블롭을 풀어 평문에서 표식을 찾습니다(암호화돼 저장된 첨부는 파일 검사로 보이지 않으므로).
    /// 발견한 블롭은 `blob:<ID 앞부분>`, 풀리지 않는 조각이 있으면 `blob:<ID 앞부분>(복호화 실패: 검사 불가)`.
    pub fn scan_blobs_for_marker(
        &self,
        marker: &[u8],
        keys: &[(yangpa_core::ids::BlobId, Secret32)],
    ) -> Result<Vec<String>> {
        if marker.len() < MIN_MARKER_LEN || marker.len() > MAX_MARKER_LEN {
            return Err(StoreError::InvalidInput("표식 길이(6~1024바이트)"));
        }
        let needles = needles(marker);
        let mut findings = BTreeSet::new();
        for (blob, key) in keys {
            let Some(manifest) = self.blob_manifest(blob)? else {
                continue;
            };
            // 압축한 블롭(`yangpa_crypto::blobpack`)은 평문 조각에 표식이 그대로 보이지 않으므로 풀어서 봅니다.
            if let Some(found) = self.scan_compressed_blob(blob, key, &manifest, &needles)? {
                if let Some(f) = found {
                    findings.insert(f);
                }
                continue;
            }
            let mut scan = MarkerScanner::new(&needles);
            for i in 0..manifest.chunks.len() as u32 {
                let Some(ct) = self.blob_read_chunk(blob, i)? else {
                    scan.reset();
                    continue;
                };
                match yangpa_crypto::decrypt_blob_chunk(key, &manifest, i, &ct) {
                    Ok(pt) => {
                        let pt = zeroize::Zeroizing::new(pt);
                        if scan.feed(&pt) {
                            findings.insert(format!("blob:{}", blob.short()));
                            break;
                        }
                    }
                    Err(_) => {
                        findings.insert(format!("blob:{}(복호화 실패: 검사 불가)", blob.short()));
                        break;
                    }
                }
            }
        }
        Ok(findings.into_iter().collect())
    }

    /// 압축 블롭이면 풀어서 검사해 `Some(발견 위치 또는 None)`, 압축 블롭이 아니면 `None`(조각 단위로 검사).
    /// 일부만 받은 압축 블롭은 풀 수 없어 '검사 불가'로 적습니다.
    fn scan_compressed_blob(
        &self,
        blob: &yangpa_core::ids::BlobId,
        key: &Secret32,
        manifest: &yangpa_core::blob::BlobManifest,
        needles: &[Vec<u8>],
    ) -> Result<Option<Option<String>>> {
        let Some(ct) = self.blob_read_chunk(blob, 0)? else {
            return Ok(None);
        };
        let Ok(first) = yangpa_crypto::decrypt_blob_chunk(key, manifest, 0, &ct) else {
            return Ok(None);
        };
        let first = zeroize::Zeroizing::new(first);
        match yangpa_crypto::blobpack::parse_layout(&first, manifest) {
            Ok((yangpa_crypto::BlobLayout::Zl, _)) => {}
            _ => return Ok(None),
        }
        let fetch = |i: u32| -> std::result::Result<zeroize::Zeroizing<Vec<u8>>, StoreError> {
            let ct = self.blob_read_chunk(blob, i)?.ok_or(StoreError::NotFound)?;
            yangpa_crypto::decrypt_blob_chunk(key, manifest, i, &ct)
                .map(zeroize::Zeroizing::new)
                .map_err(|_| StoreError::NotFound)
        };
        let mut scan = MarkerScanner::new(needles);
        let mut hit = false;
        let done = yangpa_crypto::BlobReader::open(manifest, fetch).and_then(|mut r| {
            r.for_each(|piece| {
                hit = hit || scan.feed(piece);
                Ok(())
            })
        });
        Ok(Some(match done {
            Ok(()) if hit => Some(format!("blob:{}", blob.short())),
            Ok(()) => None,
            Err(_) => Some(format!(
                "blob:{}(압축본을 풀 수 없음: 검사 불가)",
                blob.short()
            )),
        }))
    }

    /// 표식이 발견된 위치 목록(비었으면 깨끗함). 위치만 알려 주고 내용은 담지 않습니다.
    pub fn scan_for_marker(&self, marker: &[u8]) -> Result<Vec<String>> {
        if marker.len() < MIN_MARKER_LEN || marker.len() > MAX_MARKER_LEN {
            return Err(StoreError::InvalidInput("표식 길이(6~1024바이트)"));
        }
        let needles = needles(marker);
        let mut findings = BTreeSet::new();
        self.read(|c| {
            scan_columns(c, &needles, &mut findings)?;
            scan_sealed(c, &needles, &mut findings)
        })?;
        walk(&self.dir, &self.dir, &needles, &mut findings)?;
        Ok(findings.into_iter().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_works_at_edges() {
        assert!(find(b"abcdef", b"abc"));
        assert!(find(b"abcdef", b"def"));
        assert!(find(b"aaab", b"aab"));
        assert!(!find(b"abcdef", b"deg"));
        assert!(!find(b"ab", b"abc"));
        let n = needles(b"marker");
        assert!(contains_any(b"xx6d61726b6572yy", &n));
        assert!(contains_any("m\0a\0r\0k\0e\0r\0".as_bytes(), &n));
    }

    #[test]
    fn marker_scanner_joins_chunk_edges() {
        let n = needles(b"secret-marker");
        let mut s = MarkerScanner::new(&n);
        assert!(!s.feed(b"xxxxsecret-"));
        assert!(s.feed(b"markeryyyy"), "조각 경계에 걸친 표식");
        let mut s = MarkerScanner::new(&n);
        assert!(!s.feed(b"xxxxsecret-"));
        s.reset();
        assert!(!s.feed(b"markeryyyy"), "빠진 조각 너머로는 잇지 않음");
    }

    #[test]
    fn scan_file_finds_marker_across_chunk_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        let mut data = vec![b'x'; FILE_CHUNK - 3];
        data.extend_from_slice(b"secret-marker");
        data.extend_from_slice(&[b'y'; 100]);
        std::fs::write(&path, &data).unwrap();
        assert!(scan_file(&path, &needles(b"secret-marker")).unwrap());
        assert!(!scan_file(&path, &needles(b"other-marker")).unwrap());
    }

    /// 잠금 구간을 읽으면 실패하는 읽개(Windows에서 다른 연결이 그 바이트를 배타적으로 잠근 상태).
    struct Locked {
        inner: std::io::Cursor<Vec<u8>>,
        locked: Range<u64>,
    }

    impl Read for Locked {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let start = self.inner.position();
            let end = start + buf.len() as u64;
            if start < self.locked.end && self.locked.start < end {
                return Err(std::io::Error::other("잠긴 구간을 읽음"));
            }
            self.inner.read(buf)
        }
    }

    impl Seek for Locked {
        fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
            self.inner.seek(pos)
        }
    }

    #[test]
    fn wal_index_scan_skips_lock_bytes() {
        let n = needles(b"marker-ab");
        let with = |at: usize| {
            let mut data = vec![0u8; 4096];
            data[at..at + 9].copy_from_slice(b"marker-ab");
            Locked {
                inner: std::io::Cursor::new(data),
                locked: 120..129,
            }
        };
        // 잠금 구간 앞·뒤의 표식은 찾고, 잠긴 바이트는 읽지 않음.
        assert!(scan_reader(with(10), &n, Some(SHM_LOCK_BYTES)).unwrap());
        assert!(scan_reader(with(200), &n, Some(SHM_LOCK_BYTES)).unwrap());
        assert!(!scan_reader(with(1000), &needles(b"absent"), Some(SHM_LOCK_BYTES)).unwrap());
        // 건너뛴 구간에 걸친 바이트는 이어 붙이지 않음(잠금 자리라 내용이 올 수 없음).
        assert!(!scan_reader(with(115), &n, Some(SHM_LOCK_BYTES)).unwrap());
        // 건너뛰지 않으면 잠긴 구간에서 실패(고친 내용이 없을 때의 동작).
        assert!(scan_reader(with(1000), &n, None).is_err());

        // 실제 파일 이름으로도: `-shm`만 건너뜀, 짧은 파일도 괜찮음.
        let dir = tempfile::tempdir().unwrap();
        let shm = dir.path().join("yangpa.db-shm");
        let mut data = vec![b'x'; 300];
        data[125..134].copy_from_slice(b"marker-ab");
        std::fs::write(&shm, &data).unwrap();
        assert!(!scan_file(&shm, &n).unwrap());
        let other = dir.path().join("yangpa.db-wal");
        std::fs::write(&other, &data).unwrap();
        assert!(scan_file(&other, &n).unwrap(), "다른 파일은 그대로 다 훑음");
        std::fs::write(&shm, &data[..100]).unwrap();
        assert!(!scan_file(&shm, &n).unwrap());
    }

    #[cfg(windows)]
    #[test]
    fn lock_violation_is_retried() {
        struct Flaky(u32, std::io::Cursor<Vec<u8>>);
        impl Read for Flaky {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if self.0 > 0 {
                    self.0 -= 1;
                    return Err(std::io::Error::from_raw_os_error(33));
                }
                self.1.read(buf)
            }
        }
        impl Seek for Flaky {
            fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
                self.1.seek(pos)
            }
        }
        let r = Flaky(3, std::io::Cursor::new(b"..marker-ab..".to_vec()));
        assert!(scan_reader(r, &needles(b"marker-ab"), None).unwrap());
    }
}
