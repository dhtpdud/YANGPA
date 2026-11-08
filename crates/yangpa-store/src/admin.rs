//! 설정, API 토큰, 감사 로그, 사람 확인 대기열, 노드 주소 캐시, 초대 사용 기록.

use rusqlite::{OptionalExtension, params};
use serde::Serialize;
use serde::de::DeserializeOwned;
use yangpa_core::ids::{DeviceId, SpaceId, UserId};
use yangpa_core::time::Timestamp;

use crate::db::{Store, ts_from_sql, ts_to_sql};
use crate::error::{Result, StoreError};

const MAX_SETTING_LEN: usize = 4 * 1024 * 1024;
const MAX_AUDIT_FIELD: usize = 4096;
const MAX_PEER_ADDRS: usize = 32;

/// API 토큰(원문 대신 해시만 저장).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiTokenRecord {
    pub id: String,
    pub name: String,
    pub token_hash: [u8; 32],
    /// `read`, `write`, `admin`, `settings`, `owner`.
    pub scopes: Vec<String>,
    /// 비면 모든 공간.
    pub spaces: Vec<SpaceId>,
    /// AI용 토큰(파괴적 작업은 사람 확인 대기).
    pub automation: bool,
    pub created_at: Timestamp,
    pub revoked: bool,
    pub last_used_at: Option<Timestamp>,
}

/// 감사 로그 한 줄. 메시지 내용은 넣지 않습니다.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditEntry {
    /// 저장 순번(쓸 때는 무시).
    pub seq: i64,
    pub at: Timestamp,
    /// 토큰 이름.
    pub token: String,
    pub method: String,
    pub outcome: String,
    pub detail: String,
    /// 보안상 중요한 줄(오류·자동화·파괴적 작업·토큰·확인 등). 평범한 줄과 따로 상한을 두어, 평범한 호출을 잔뜩 해서
    /// 이전의 중요한 기록을 밀어내지 못하게 합니다.
    pub important: bool,
}

/// AI가 요청한 파괴적 작업(사람 확인 대기).
#[derive(Clone, Debug, PartialEq)]
pub struct PendingConfirmation {
    pub id: String,
    pub created_at: Timestamp,
    pub expires_at: Option<Timestamp>,
    pub token: String,
    pub method: String,
    pub params: serde_json::Value,
    /// 사람에게 보여줄 요약.
    pub summary: String,
}

/// 연결해 본 노드.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerRecord {
    /// libp2p PeerId 바이트.
    pub peer_id: Vec<u8>,
    pub addrs: Vec<String>,
    pub user: Option<UserId>,
    pub device: Option<DeviceId>,
    pub last_success: Option<Timestamp>,
    pub last_attempt: Option<Timestamp>,
    pub failures: u32,
}

fn read_token(r: &rusqlite::Row<'_>) -> rusqlite::Result<(ApiTokenRecord, String, String)> {
    Ok((
        ApiTokenRecord {
            id: r.get(0)?,
            name: r.get(1)?,
            token_hash: r.get(2)?,
            scopes: Vec::new(),
            spaces: Vec::new(),
            automation: r.get(5)?,
            created_at: ts_from_sql(r.get(6)?),
            revoked: r.get(7)?,
            last_used_at: r.get::<_, Option<i64>>(8)?.map(ts_from_sql),
        },
        r.get(3)?,
        r.get(4)?,
    ))
}

fn decode_token(row: (ApiTokenRecord, String, String)) -> Result<ApiTokenRecord> {
    let (mut t, scopes, spaces) = row;
    t.scopes = serde_json::from_str(&scopes).map_err(|_| StoreError::Corrupt("토큰 범위"))?;
    t.spaces = serde_json::from_str(&spaces).map_err(|_| StoreError::Corrupt("토큰 공간"))?;
    Ok(t)
}

const TOKEN_SELECT: &str = "SELECT id, name, token_hash, scopes, spaces, automation, created_at, revoked, last_used_at FROM api_tokens";

fn read_peer(r: &rusqlite::Row<'_>) -> rusqlite::Result<(PeerRecord, String)> {
    Ok((
        PeerRecord {
            peer_id: r.get(0)?,
            addrs: Vec::new(),
            user: r.get::<_, Option<[u8; 32]>>(2)?.map(UserId),
            device: r.get::<_, Option<[u8; 32]>>(3)?.map(DeviceId),
            last_success: r.get::<_, Option<i64>>(4)?.map(ts_from_sql),
            last_attempt: r.get::<_, Option<i64>>(5)?.map(ts_from_sql),
            failures: u32::try_from(r.get::<_, i64>(6)?).unwrap_or(u32::MAX),
        },
        r.get(1)?,
    ))
}

fn decode_peer(row: (PeerRecord, String)) -> Result<PeerRecord> {
    let (mut p, addrs) = row;
    p.addrs = serde_json::from_str(&addrs).map_err(|_| StoreError::Corrupt("노드 주소"))?;
    Ok(p)
}

const PEER_SELECT: &str =
    "SELECT peer_id, addrs, user, device, last_success, last_attempt, failures FROM peer_cache";

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

impl Store {
    // ---- 설정(JSON) ----

    pub fn put_setting<T: Serialize + ?Sized>(&self, key: &str, value: &T) -> Result<()> {
        let json = serde_json::to_string(value)?;
        if key.is_empty() || key.len() > 256 || json.len() > MAX_SETTING_LEN {
            return Err(StoreError::InvalidInput("설정 키·값 크기"));
        }
        self.write(|tx| {
            tx.prepare_cached("INSERT OR REPLACE INTO settings (key, value) VALUES (?1, ?2)")?
                .execute(params![key, json])?;
            Ok(())
        })
    }

    pub fn get_setting<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        let v: Option<String> = self.read(|c| {
            Ok(
                c.prepare_cached("SELECT value FROM settings WHERE key = ?1")?
                    .query_row([key], |r| r.get(0))
                    .optional()?,
            )
        })?;
        v.map(|s| serde_json::from_str(&s).map_err(StoreError::from))
            .transpose()
    }

    pub fn delete_setting(&self, key: &str) -> Result<bool> {
        self.write(|tx| {
            Ok(tx
                .prepare_cached("DELETE FROM settings WHERE key = ?1")?
                .execute([key])?
                > 0)
        })
    }

    /// 접두어로 시작하는 설정들(키 순).
    pub fn settings_with_prefix(&self, prefix: &str) -> Result<Vec<(String, serde_json::Value)>> {
        let rows: Vec<(String, String)> = self.read(|c| {
            // LIKE의 와일드카드를 피하려고 범위 비교로 찾습니다.
            let mut stmt =
                c.prepare_cached("SELECT key, value FROM settings WHERE key >= ?1 AND substr(key, 1, length(?1)) = ?1 ORDER BY key")?;
            let rows = stmt.query_map([prefix], |r| Ok((r.get(0)?, r.get(1)?)))?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })?;
        rows.into_iter()
            .map(|(k, v)| Ok((k, serde_json::from_str(&v)?)))
            .collect()
    }

    // ---- API 토큰 ----

    pub fn create_api_token(&self, t: &ApiTokenRecord) -> Result<()> {
        let scopes = serde_json::to_string(&t.scopes)?;
        let spaces = serde_json::to_string(&t.spaces)?;
        self.write(|tx| {
            tx.prepare_cached(
                "INSERT INTO api_tokens (id, name, token_hash, scopes, spaces, automation, created_at, revoked, last_used_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            )?
            .execute(params![
                t.id,
                t.name,
                &t.token_hash,
                scopes,
                spaces,
                t.automation,
                ts_to_sql(t.created_at),
                t.revoked,
                t.last_used_at.map(ts_to_sql)
            ])?;
            Ok(())
        })
    }

    /// 해시로 유효한(폐기되지 않은) 토큰을 찾습니다.
    pub fn find_api_token(&self, token_hash: &[u8; 32]) -> Result<Option<ApiTokenRecord>> {
        let row = self.read(|c| {
            Ok(c.prepare_cached(&format!(
                "{TOKEN_SELECT} WHERE token_hash = ?1 AND revoked = 0"
            ))?
            .query_row([token_hash], read_token)
            .optional()?)
        })?;
        row.map(decode_token).transpose()
    }

    pub fn list_api_tokens(&self) -> Result<Vec<ApiTokenRecord>> {
        let rows = self.read(|c| {
            let mut stmt = c.prepare_cached(&format!("{TOKEN_SELECT} ORDER BY created_at, id"))?;
            let rows = stmt.query_map([], read_token)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })?;
        rows.into_iter().map(decode_token).collect()
    }

    pub fn revoke_api_token(&self, id: &str) -> Result<bool> {
        self.write(|tx| {
            Ok(tx
                .prepare_cached("UPDATE api_tokens SET revoked = 1 WHERE id = ?1")?
                .execute([id])?
                > 0)
        })
    }

    pub fn touch_api_token(&self, id: &str, at: Timestamp) -> Result<()> {
        self.write(|tx| {
            tx.prepare_cached("UPDATE api_tokens SET last_used_at = ?2 WHERE id = ?1")?
                .execute(params![id, ts_to_sql(at)])?;
            Ok(())
        })
    }

    // ---- 감사 로그 ----

    /// 감사 로그에 덧붙이고, 같은 종류(중요·평범)에서 상한을 넘은 오래된 줄을 지웁니다. 순번을 돌려줍니다.
    pub fn append_audit(&self, e: &AuditEntry) -> Result<i64> {
        let cap = i64::from(if e.important {
            self.opts.audit_important_cap.max(1)
        } else {
            self.opts.audit_cap.max(1)
        });
        self.write(|tx| {
            tx.prepare_cached("INSERT INTO audit_log (at, token, method, outcome, detail, important) VALUES (?1, ?2, ?3, ?4, ?5, ?6)")?
                .execute(params![
                    ts_to_sql(e.at),
                    truncate(&e.token, MAX_AUDIT_FIELD),
                    truncate(&e.method, MAX_AUDIT_FIELD),
                    truncate(&e.outcome, MAX_AUDIT_FIELD),
                    truncate(&e.detail, MAX_AUDIT_FIELD),
                    e.important
                ])?;
            let seq = tx.last_insert_rowid();
            // 같은 종류의 최근 `cap`줄만 남깁니다(다른 종류의 줄은 세지 않음).
            tx.prepare_cached(
                "DELETE FROM audit_log WHERE important = ?1 AND seq <= \
                 (SELECT seq FROM audit_log WHERE important = ?1 ORDER BY seq DESC LIMIT 1 OFFSET ?2)",
            )?
            .execute(params![e.important, cap])?;
            Ok(seq)
        })
    }

    /// 최신순 감사 로그(두 종류를 순번대로 합침). `before`는 순번 기준 페이지 위치.
    pub fn list_audit(&self, limit: u32, before: Option<i64>) -> Result<Vec<AuditEntry>> {
        let limit = limit.clamp(1, 1000);
        self.read(|c| {
            let mut stmt = c.prepare_cached(
                "SELECT seq, at, token, method, outcome, detail, important FROM audit_log WHERE seq < ?1 ORDER BY seq DESC LIMIT ?2",
            )?;
            let rows = stmt.query_map(params![before.unwrap_or(i64::MAX), limit], |r| {
                Ok(AuditEntry {
                    seq: r.get(0)?,
                    at: ts_from_sql(r.get(1)?),
                    token: r.get(2)?,
                    method: r.get(3)?,
                    outcome: r.get(4)?,
                    detail: r.get(5)?,
                    important: r.get(6)?,
                })
            })?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })
    }

    // ---- 사람 확인 대기열 ----

    pub fn put_confirmation(&self, p: &PendingConfirmation) -> Result<()> {
        let params_json = serde_json::to_string(&p.params)?;
        if params_json.len() > MAX_SETTING_LEN {
            return Err(StoreError::LimitExceeded("확인 요청 크기"));
        }
        self.write(|tx| {
            tx.prepare_cached(
                "INSERT OR REPLACE INTO confirmations (id, created_at, expires_at, token, method, params, summary) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?
            .execute(params![
                p.id,
                ts_to_sql(p.created_at),
                p.expires_at.map(ts_to_sql),
                p.token,
                p.method,
                params_json,
                p.summary
            ])?;
            Ok(())
        })
    }

    pub fn list_confirmations(&self) -> Result<Vec<PendingConfirmation>> {
        type Row = (String, i64, Option<i64>, String, String, String, String);
        let rows: Vec<Row> = self.read(|c| {
            let mut stmt = c.prepare_cached(
                "SELECT id, created_at, expires_at, token, method, params, summary FROM confirmations ORDER BY created_at, id",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?))
            })?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })?;
        rows.into_iter()
            .map(
                |(id, created_at, expires_at, token, method, params, summary)| {
                    Ok(PendingConfirmation {
                        id,
                        created_at: ts_from_sql(created_at),
                        expires_at: expires_at.map(ts_from_sql),
                        token,
                        method,
                        params: serde_json::from_str(&params)?,
                        summary,
                    })
                },
            )
            .collect()
    }

    /// 확인 요청을 꺼냅니다(승인·거절 처리 시 한 번만 꺼내지도록 지우면서 돌려줌).
    pub fn take_confirmation(&self, id: &str) -> Result<Option<PendingConfirmation>> {
        let row: Option<(i64, Option<i64>, String, String, String, String)> = self.write(|tx| {
            let row = tx
                .prepare_cached(
                    "SELECT created_at, expires_at, token, method, params, summary FROM confirmations WHERE id = ?1",
                )?
                .query_row([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))
                .optional()?;
            tx.prepare_cached("DELETE FROM confirmations WHERE id = ?1")?.execute([id])?;
            Ok(row)
        })?;
        row.map(|(created_at, expires_at, token, method, params, summary)| {
            Ok(PendingConfirmation {
                id: id.to_string(),
                created_at: ts_from_sql(created_at),
                expires_at: expires_at.map(ts_from_sql),
                token,
                method,
                params: serde_json::from_str(&params)?,
                summary,
            })
        })
        .transpose()
    }

    /// 만료된 확인 요청을 지웁니다.
    pub fn prune_confirmations(&self, now: Timestamp) -> Result<usize> {
        self.write(|tx| {
            Ok(tx
                .prepare_cached(
                    "DELETE FROM confirmations WHERE expires_at IS NOT NULL AND expires_at <= ?1",
                )?
                .execute([ts_to_sql(now)])?)
        })
    }

    // ---- 노드 주소 캐시 ----

    /// 연결 성공을 기록합니다(주소 갱신, 실패 횟수 초기화).
    pub fn record_peer_success(
        &self,
        peer_id: &[u8],
        addrs: &[String],
        at: Timestamp,
    ) -> Result<()> {
        if peer_id.is_empty() || peer_id.len() > 128 {
            return Err(StoreError::InvalidInput("PeerId 길이"));
        }
        let addrs: Vec<&str> = addrs
            .iter()
            .take(MAX_PEER_ADDRS)
            .map(|a| truncate(a, 512))
            .collect();
        let addrs = serde_json::to_string(&addrs)?;
        self.write(|tx| {
            tx.prepare_cached(
                "INSERT INTO peer_cache (peer_id, addrs, last_success, last_attempt, failures) VALUES (?1, ?2, ?3, ?3, 0) \
                 ON CONFLICT (peer_id) DO UPDATE SET addrs = excluded.addrs, last_success = excluded.last_success, \
                   last_attempt = excluded.last_attempt, failures = 0",
            )?
            .execute(params![peer_id, addrs, ts_to_sql(at)])?;
            Ok(())
        })
    }

    pub fn record_peer_failure(&self, peer_id: &[u8], at: Timestamp) -> Result<()> {
        self.write(|tx| {
            tx.prepare_cached("UPDATE peer_cache SET failures = failures + 1, last_attempt = ?2 WHERE peer_id = ?1")?
                .execute(params![peer_id, ts_to_sql(at)])?;
            Ok(())
        })
    }

    /// 인증으로 확인한 노드의 사용자·기기를 기록합니다.
    pub fn set_peer_identity(
        &self,
        peer_id: &[u8],
        user: &UserId,
        device: &DeviceId,
    ) -> Result<()> {
        if peer_id.is_empty() || peer_id.len() > 128 {
            return Err(StoreError::InvalidInput("PeerId 길이"));
        }
        self.write(|tx| {
            // 기기는 노드 하나에만 묶이므로 예전 노드 기록에서 뗍니다.
            tx.prepare_cached("UPDATE peer_cache SET device = NULL, user = NULL WHERE device = ?1 AND peer_id != ?2")?
                .execute(params![device.as_bytes(), peer_id])?;
            tx.prepare_cached(
                "INSERT INTO peer_cache (peer_id, addrs, user, device) VALUES (?1, '[]', ?2, ?3) \
                 ON CONFLICT (peer_id) DO UPDATE SET user = excluded.user, device = excluded.device",
            )?
            .execute(params![peer_id, user.as_bytes(), device.as_bytes()])?;
            Ok(())
        })
    }

    pub fn get_peer(&self, peer_id: &[u8]) -> Result<Option<PeerRecord>> {
        let row = self.read(|c| {
            Ok(
                c.prepare_cached(&format!("{PEER_SELECT} WHERE peer_id = ?1"))?
                    .query_row([peer_id], read_peer)
                    .optional()?,
            )
        })?;
        row.map(decode_peer).transpose()
    }

    pub fn peer_for_device(&self, device: &DeviceId) -> Result<Option<PeerRecord>> {
        let row = self.read(|c| {
            Ok(
                c.prepare_cached(&format!("{PEER_SELECT} WHERE device = ?1"))?
                    .query_row([device.as_bytes()], read_peer)
                    .optional()?,
            )
        })?;
        row.map(decode_peer).transpose()
    }

    /// 기기가 묶인 노드 기록(`user`·`device`가 있는 것). 공간 멤버 노드를 고를 때 기기마다 찾지 않고 이것을 한 번
    /// 훑습니다.
    pub fn identified_peers(&self) -> Result<Vec<PeerRecord>> {
        let rows = self.read(|c| {
            let mut stmt = c.prepare_cached(&format!(
                "{PEER_SELECT} WHERE device IS NOT NULL AND user IS NOT NULL"
            ))?;
            let rows = stmt.query_map([], read_peer)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })?;
        rows.into_iter().map(decode_peer).collect()
    }

    /// 최근 성공 순 노드(부트스트랩 대신 먼저 시도할 목록).
    pub fn recent_peers(&self, limit: u32) -> Result<Vec<PeerRecord>> {
        let rows = self.read(|c| {
            let mut stmt = c.prepare_cached(&format!(
                "{PEER_SELECT} WHERE last_success IS NOT NULL ORDER BY last_success DESC LIMIT ?1"
            ))?;
            let rows = stmt.query_map([limit.clamp(1, 10_000)], read_peer)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })?;
        rows.into_iter().map(decode_peer).collect()
    }

    /// 오래 성공하지 못했거나 실패가 쌓인 노드를 지웁니다.
    pub fn prune_peers(&self, older_than: Timestamp, max_failures: u32) -> Result<usize> {
        self.write(|tx| {
            Ok(tx
                .prepare_cached(
                    "DELETE FROM peer_cache WHERE device IS NULL AND \
                     (COALESCE(last_success, 0) < ?1 OR failures > ?2)",
                )?
                .execute(params![ts_to_sql(older_than), max_failures])?)
        })
    }

    // ---- 초대 사용 기록 ----

    /// 초대 사용을 기록합니다. 같은 사용자가 같은 토큰을 다시 쓰면 false.
    pub fn record_invite_redemption(
        &self,
        space: &SpaceId,
        token_hash: &[u8; 32],
        user: &UserId,
        at: Timestamp,
    ) -> Result<bool> {
        self.write(|tx| {
            Ok(tx
                .prepare_cached(
                    "INSERT OR IGNORE INTO invite_redemptions (space, token_hash, user, at) VALUES (?1, ?2, ?3, ?4)",
                )?
                .execute(params![space.as_bytes(), token_hash, user.as_bytes(), ts_to_sql(at)])?
                > 0)
        })
    }

    pub fn invite_redemption_count(&self, space: &SpaceId, token_hash: &[u8; 32]) -> Result<u32> {
        self.read(|c| {
            let n: i64 = c
                .prepare_cached(
                    "SELECT COUNT(*) FROM invite_redemptions WHERE space = ?1 AND token_hash = ?2",
                )?
                .query_row(params![space.as_bytes(), token_hash], |r| r.get(0))?;
            Ok(u32::try_from(n).unwrap_or(u32::MAX))
        })
    }
}
