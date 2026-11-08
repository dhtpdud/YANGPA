//! SQLCipher 연결 열기, 키 설정, 이전 적용, 읽기 연결 풀.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use parking_lot::Mutex;
use rusqlite::{
    Connection, ErrorCode, OpenFlags, OptionalExtension, Transaction, TransactionBehavior,
};
use yangpa_core::body::Secret32;
use yangpa_core::event::{Lane, StreamId};
use yangpa_core::time::Timestamp;
use zeroize::Zeroizing;

use crate::blobs::{BlobStore, ManifestCache};
use crate::error::{Result, StoreError};
use crate::schema::{DEVICE_SIGN_KEYS_VERSION, MIGRATIONS, VOLATILE_CHANNELS_VERSION};
use crate::search::SearchState;

/// 데이터 디렉터리 안 DB 파일 이름.
pub const DB_FILE_NAME: &str = "yangpa.db";
/// 데이터 디렉터리 안 블롭 디렉터리 이름.
pub const BLOB_DIR_NAME: &str = "blobs";
/// 지우기에서 블롭 파일이 남았다는 표시(설정 키). 다음에 열 때 다시 지웁니다.
const BLOB_WIPE_PENDING: &str = "store.blob_wipe_pending";

/// 저장소 설정.
#[derive(Clone, Debug)]
pub struct StoreOptions {
    /// 읽기 전용 연결 수(WAL이라 쓰기와 동시에 읽을 수 있음). 0이면 쓰기 연결 하나로 읽습니다.
    pub readers: usize,
    /// 감사 로그의 평범한 줄 최대 행 수.
    pub audit_cap: u32,
    /// 감사 로그의 중요한 줄(실패·파괴적 작업·관리 작업 등) 최대 행 수. 평범한 줄과 따로 셉니다.
    pub audit_important_cap: u32,
    /// SQLCipher가 해제하는 메모리를 0으로 덮어쓰고 잠급니다(`cipher_memory_security`). 프로세스 전역 설정입니다.
    pub memory_security: bool,
    /// 잠금 대기 시간.
    pub busy_timeout: Duration,
    /// 메모리에 둘 블롭 매니페스트 수(조각 쓰기마다 큰 매니페스트를 다시 풀지 않게).
    pub manifest_cache: usize,
}

impl Default for StoreOptions {
    fn default() -> Self {
        StoreOptions {
            readers: 3,
            audit_cap: 10_000,
            audit_important_cap: 50_000,
            memory_security: true,
            busy_timeout: Duration::from_secs(5),
            manifest_cache: 64,
        }
    }
}

/// 로컬 저장소: SQLCipher DB(WAL, `secure_delete`) + 블롭 파일 디렉터리.
///
/// 디스크 등급 데이터만 받습니다. 쓰기는 연결 하나를 잠가 직렬화하고, 읽기는 작은 연결 풀에서 동시에 합니다.
pub struct Store {
    pub(crate) dir: PathBuf,
    pub(crate) writer: Mutex<Connection>,
    readers: Vec<Mutex<Connection>>,
    next_reader: AtomicUsize,
    pub(crate) blob_files: BlobStore,
    pub(crate) manifests: Mutex<ManifestCache>,
    pub(crate) opts: StoreOptions,
    /// RAM 검색 색인과 그 변경 기록(`search.rs`).
    pub(crate) search: SearchState,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store")
            .field("dir", &self.dir)
            .field("readers", &self.readers.len())
            .finish()
    }
}

impl Store {
    /// 데이터 디렉터리의 DB를 엽니다(없으면 만듦). 키가 틀리면 [`StoreError::WrongKey`].
    pub fn open(dir: &Path, db_key: &Secret32) -> Result<Store> {
        Self::open_with(dir, db_key, StoreOptions::default())
    }

    pub fn open_with(dir: &Path, db_key: &Secret32, opts: StoreOptions) -> Result<Store> {
        create_private_dir(dir)?;
        let blob_files = BlobStore::new(dir.join(BLOB_DIR_NAME))?;
        let path = dir.join(DB_FILE_NAME);
        let writer = open_connection(&path, db_key, &opts, false)?;
        let mode: String = writer.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
        if !mode.eq_ignore_ascii_case("wal") {
            return Err(StoreError::Corrupt("WAL 모드로 바꿀 수 없음"));
        }
        migrate(&writer)?;
        let search = SearchState::default();
        search.install(&writer)?;
        // 오래 열어 두는 연결은 처음에 한 번 통계를 맞춰 둡니다(SQLite 권장).
        writer.execute_batch("PRAGMA optimize = 0x10002;")?;
        let mut readers = Vec::with_capacity(opts.readers);
        for _ in 0..opts.readers {
            readers.push(Mutex::new(open_connection(&path, db_key, &opts, true)?));
        }
        let manifests = Mutex::new(ManifestCache::new(opts.manifest_cache));
        let store = Store {
            dir: dir.to_path_buf(),
            writer: Mutex::new(writer),
            readers,
            next_reader: AtomicUsize::new(0),
            blob_files,
            manifests,
            opts,
            search,
        };
        store.finish_pending_wipe()?;
        Ok(store)
    }

    /// 테스트용: 임시 디렉터리에 무작위 키로 엽니다. 디렉터리는 돌려준 `TempDir`가 해제될 때 지워집니다.
    pub fn open_temp() -> Result<(Store, tempfile::TempDir)> {
        let dir = tempfile::Builder::new().prefix("yangpa-store-").tempdir()?;
        let store = Store::open(dir.path(), &Secret32::random())?;
        Ok((store, dir))
    }

    /// 데이터 디렉터리.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// 적용된 스키마 버전.
    pub fn schema_version(&self) -> Result<i64> {
        self.read(|c| Ok(c.query_row("PRAGMA user_version", [], |r| r.get(0))?))
    }

    /// WAL 내용을 DB 파일로 옮기고 WAL을 비웁니다. 삭제 직후 예전 페이지가 WAL에 남는 시간을 줄입니다.
    /// 읽는 중인 연결이 있으면 잠금 대기 시간만큼 기다리고, 그래도 바쁘면 일부만 옮겨집니다(오류 아님).
    pub fn checkpoint(&self) -> Result<()> {
        let c = self.writer.lock();
        c.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
        Ok(())
    }

    /// 삭제 직후 쓰는 짧은 체크포인트. 긴 읽기(검색·누출 검사)가 있어도 쓰기를 오래 막지 않게 잠깐만 기다립니다.
    pub(crate) fn checkpoint_soon(&self) {
        // 지운 글이 RAM 검색 색인에도 남지 않게 먼저 맞춥니다.
        self.scrub_search_index();
        let c = self.writer.lock();
        let r = c
            .busy_timeout(Duration::from_millis(200))
            .and_then(|_| c.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(())))
            .and_then(|_| c.busy_timeout(self.opts.busy_timeout));
        if let Err(e) = r {
            tracing::debug!(error = %e, "삭제 후 체크포인트 실패");
            let _ = c.busy_timeout(self.opts.busy_timeout);
        }
    }

    /// 주기적 정리: 질의 계획 통계 갱신(`PRAGMA optimize`)과 체크포인트. 데몬이 한가할 때 부릅니다.
    pub fn maintenance(&self) -> Result<()> {
        {
            let c = self.writer.lock();
            c.execute_batch("PRAGMA optimize;")?;
        }
        self.checkpoint()
    }

    /// 이 기기의 데이터를 모두 지웁니다(다른 기기가 이 기기를 폐기했을 때). 모든 표의 행과 블롭 파일을 지우고 WAL을
    /// 비웁니다(`secure_delete`라 지운 페이지는 0으로 덮임). `keep_secrets`에 적은 비밀과 `keep_settings`의 접두어로
    /// 시작하는 설정만 남기며, 표 지우기와 남기기는 한 트랜잭션입니다(중간에 멈춰도 남길 설정을 잃지 않게).
    ///
    /// 블롭 디렉터리를 지우지 못하면(Windows에서 아직 열린 파일 등) 지울 수 있는 파일만 지우고 표시를 남겨 다음에 열 때
    /// 다시 지웁니다. 이것은 오류로 보지 않습니다(행은 이미 지워졌고 블롭 파일은 열 키가 DB에 없음).
    pub fn wipe_all(&self, keep_secrets: &[&str], keep_settings: &[&str]) -> Result<()> {
        self.write(|tx| {
            let tables: Vec<String> = tx
                .prepare(
                    "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
                )?
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            for t in tables {
                match t.as_str() {
                    "secrets" => {
                        let mut stmt = tx.prepare("SELECT name FROM secrets")?;
                        let names: Vec<String> = stmt
                            .query_map([], |r| r.get(0))?
                            .collect::<rusqlite::Result<_>>()?;
                        for n in names.iter().filter(|n| !keep_secrets.contains(&n.as_str())) {
                            tx.execute("DELETE FROM secrets WHERE name = ?1", [n])?;
                        }
                    }
                    "settings" => {
                        let mut stmt = tx.prepare("SELECT key FROM settings")?;
                        let keys: Vec<String> = stmt
                            .query_map([], |r| r.get(0))?
                            .collect::<rusqlite::Result<_>>()?;
                        for k in keys
                            .iter()
                            .filter(|k| !keep_settings.iter().any(|p| k.starts_with(p)))
                        {
                            tx.execute("DELETE FROM settings WHERE key = ?1", [k])?;
                        }
                    }
                    _ => {
                        tx.execute(&format!("DELETE FROM \"{}\"", t.replace('"', "\"\"")), [])?;
                    }
                }
            }
            Ok(())
        })?;
        *self.manifests.lock() = ManifestCache::new(self.opts.manifest_cache);
        // 표를 통째로 지우면(DELETE 최적화) 변경 훅이 행마다 불리지 않으므로 색인을 버립니다.
        self.reset_search_index();
        if !self.clear_blob_dir() {
            tracing::warn!("블롭 디렉터리를 다 지우지 못해 다음에 열 때 다시 지움");
            let _ = self.write(|tx| {
                tx.execute(
                    "INSERT OR REPLACE INTO settings (key, value) VALUES (?1, 'true')",
                    [BLOB_WIPE_PENDING],
                )?;
                Ok(())
            });
        }
        self.checkpoint()
    }

    /// 블롭 디렉터리를 비웁니다(지울 수 있는 것은 모두). 다 지웠으면 true.
    fn clear_blob_dir(&self) -> bool {
        let root = self.blob_files.root().to_path_buf();
        let all = match std::fs::remove_dir_all(&root) {
            Ok(()) => true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
            Err(_) => {
                // 하나가 막혀도 나머지는 지웁니다.
                remove_tree_best_effort(&root);
                std::fs::read_dir(&root).map_or(true, |mut d| d.next().is_none())
            }
        };
        create_private_dir(&root).is_ok() && all
    }

    /// 지난번 지우기에서 남은 블롭 파일을 지웁니다(열 때).
    fn finish_pending_wipe(&self) -> Result<()> {
        let pending: Option<String> = self.read(|c| {
            Ok(c.query_row(
                "SELECT value FROM settings WHERE key = ?1",
                [BLOB_WIPE_PENDING],
                |r| r.get(0),
            )
            .optional()?)
        })?;
        if pending.is_some() && self.clear_blob_dir() {
            self.write(|tx| {
                tx.execute("DELETE FROM settings WHERE key = ?1", [BLOB_WIPE_PENDING])?;
                Ok(())
            })?;
        }
        Ok(())
    }

    /// 읽기 연결로 실행합니다.
    pub(crate) fn read<R>(&self, f: impl FnOnce(&Connection) -> Result<R>) -> Result<R> {
        let n = self.readers.len();
        if n == 0 {
            return f(&self.writer.lock());
        }
        let start = self.next_reader.fetch_add(1, Ordering::Relaxed) % n;
        for i in 0..n {
            if let Some(c) = self.readers[(start + i) % n].try_lock() {
                return f(&c);
            }
        }
        let c = self.readers[start].lock();
        f(&c)
    }

    /// 쓰기 트랜잭션 하나로 실행합니다. `f`가 오류를 내면 되돌립니다.
    pub(crate) fn write<R>(&self, f: impl FnOnce(&Transaction<'_>) -> Result<R>) -> Result<R> {
        let mut c = self.writer.lock();
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let r = f(&tx)?;
        tx.commit()?;
        Ok(r)
    }
}

fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// `PRAGMA key = "x'…'"`(원시 키 형식: 키 파생 없이 32바이트를 그대로 씀). 문자열은 해제 시 0으로 덮어씁니다.
fn key_pragma(key: &Secret32) -> Zeroizing<String> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    // 재할당으로 키 사본이 흩어지지 않게 처음부터 넉넉히 잡습니다.
    let mut s = Zeroizing::new(String::with_capacity(96));
    s.push_str("PRAGMA key = \"x'");
    for b in key.0 {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s.push_str("'\";");
    s
}

/// 연결을 열고 키를 겁니다. 여러 노드가 한 프로세스에서 동시에 열 때 SQLCipher의 `PRAGMA key`가 드물게 실패하는
/// 일이 있어(부하 시험에서 수백 번에 한 번, 같은 연결을 닫고 다시 열면 됨) 몇 번까지 다시 시도합니다. 틀린 키는 여기서가
/// 아니라 첫 페이지를 읽을 때 드러나므로 다시 시도해도 키 검사를 건너뛰지 않습니다.
fn keyed_connection(
    path: &Path,
    flags: OpenFlags,
    key: &Secret32,
    opts: &StoreOptions,
) -> Result<Connection> {
    const ATTEMPTS: u32 = 3;
    let mut attempt = 1;
    loop {
        let conn = Connection::open_with_flags(path, flags)?;
        // SQLCipher 기본 로그(stderr)를 끕니다. 오류는 StoreError로 올라옵니다.
        conn.execute_batch("PRAGMA cipher_log_level = NONE;")?;
        if opts.memory_security {
            conn.execute_batch("PRAGMA cipher_memory_security = ON;")?;
        }
        match conn.execute_batch(&key_pragma(key)) {
            Ok(()) => return Ok(conn),
            Err(e) if attempt < ATTEMPTS => {
                tracing::warn!(attempt, error = %e, "DB 키 설정 실패, 다시 엶");
                drop(conn);
                std::thread::sleep(Duration::from_millis(20 * u64::from(attempt)));
                attempt += 1;
            }
            Err(e) => return Err(e.into()),
        }
    }
}

fn open_connection(
    path: &Path,
    key: &Secret32,
    opts: &StoreOptions,
    reader: bool,
) -> Result<Connection> {
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_CREATE
        | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = keyed_connection(path, flags, key, opts)?;
    // SQLCipher가 아니면 PRAGMA key가 조용히 무시되고 평문 DB가 만들어지므로 반드시 확인합니다.
    let cipher: Option<String> = conn
        .query_row("PRAGMA cipher_version", [], |r| r.get(0))
        .optional()?;
    if cipher.is_none() {
        return Err(StoreError::NotEncrypted);
    }
    // 키는 첫 페이지를 읽을 때 검증됩니다.
    match conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| {
        r.get::<_, i64>(0)
    }) {
        Ok(_) => {}
        Err(rusqlite::Error::SqliteFailure(e, _)) if e.code == ErrorCode::NotADatabase => {
            return Err(StoreError::WrongKey);
        }
        Err(e) => return Err(e.into()),
    }
    conn.busy_timeout(opts.busy_timeout)?;
    conn.set_prepared_statement_cache_capacity(128);
    conn.execute_batch(
        "PRAGMA foreign_keys = ON;
         PRAGMA temp_store = MEMORY;
         PRAGMA synchronous = NORMAL;
         PRAGMA secure_delete = ON;",
    )?;
    if reader {
        conn.execute_batch("PRAGMA query_only = ON;")?;
    } else {
        // 체크포인트 뒤 WAL 파일을 줄여, 지운 데이터의 예전 페이지가 오래 남지 않게 합니다.
        conn.execute_batch("PRAGMA journal_size_limit = 4194304; PRAGMA cache_size = -16000;")?;
    }
    Ok(conn)
}

fn migrate(conn: &Connection) -> Result<()> {
    let current: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    let supported = MIGRATIONS.len() as i64;
    if current > supported {
        return Err(StoreError::SchemaTooNew {
            found: current,
            supported,
        });
    }
    if current > 0 && current < DEVICE_SIGN_KEYS_VERSION as i64 {
        // 앞 단계 이전을 적용하기 전에 봅니다(하나라도 적용하면 예전 판으로 다시 열 수 없음).
        crate::schema::check_v5_data(conn)?;
    }
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(current.max(0) as usize) {
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(sql)?;
        if i + 1 == VOLATILE_CHANNELS_VERSION {
            // 이미 캐시된 공간 상태의 휘발성 채널을 채웁니다(SQL로는 base58 ID를 풀 수 없어 여기서).
            crate::spaces::backfill_volatile_channels(&tx)?;
        }
        tx.execute_batch(&format!("PRAGMA user_version = {}", i + 1))?;
        tx.commit()?;
        tracing::info!(version = i + 1, "저장소 스키마 이전 적용");
    }
    Ok(())
}

// ---- 공용 변환 ----

/// SQLite 정수는 i64라서 u64를 포화 변환합니다. 논리 시계는 [`crate::PersistentEvent`]가 범위를 미리 검사합니다.
pub(crate) fn to_sql_u64(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

pub(crate) fn from_sql_u64(v: i64) -> u64 {
    u64::try_from(v).unwrap_or(0)
}

pub(crate) fn ts_to_sql(t: Timestamp) -> i64 {
    to_sql_u64(t.0)
}

pub(crate) fn ts_from_sql(v: i64) -> Timestamp {
    Timestamp(from_sql_u64(v))
}

/// 스트림 저장 키. 공간 수준은 0 17개, 채널은 1 || 채널 ID, 키 그룹은 2 || 그룹 ID(17바이트), 받은편지함은
/// 3 || 사용자 ID(33바이트). 앞 바이트로 갈라 ID가 0이어도 섞이지 않고, 예전 키(공간·채널)는 그대로입니다.
pub(crate) fn stream_key(s: &StreamId) -> Vec<u8> {
    let (tag, id): (u8, &[u8]) = match s {
        StreamId::Space => return vec![0u8; 17],
        StreamId::Channel(c) => (1, c.as_bytes()),
        StreamId::KeyGroup(g) => (2, g.as_bytes()),
        StreamId::Inbox(u) => (3, u.as_bytes()),
    };
    let mut k = Vec::with_capacity(1 + id.len());
    k.push(tag);
    k.extend_from_slice(id);
    k
}

pub(crate) fn lane_code(l: Lane) -> i64 {
    match l {
        Lane::Message => 1,
        Lane::Key => 2,
        Lane::State => 3,
        Lane::KeyAgreement => 4,
    }
}

/// `?, ?, …` 자리표시자 n개.
pub(crate) fn placeholders(start: usize, n: usize) -> String {
    (0..n)
        .map(|i| format!("?{}", start + i))
        .collect::<Vec<_>>()
        .join(", ")
}

/// 디렉터리 아래 파일을 지울 수 있는 만큼 지웁니다(하나가 막혀도 계속).
fn remove_tree_best_effort(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let path = e.path();
        if e.file_type().is_ok_and(|t| t.is_dir()) {
            remove_tree_best_effort(&path);
            let _ = std::fs::remove_dir(&path);
        } else {
            let _ = std::fs::remove_file(&path);
        }
    }
}
