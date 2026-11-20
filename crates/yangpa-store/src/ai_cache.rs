//! AI 번역 결과 캐시(앱을 다시 켜도 같은 글을 다시 보내지 않게). 원문은 두지 않고 해시만 열쇠로 씁니다.
//!
//! 스키마 버전(`user_version`)을 올리지 않고 `CREATE TABLE IF NOT EXISTS`로 둡니다. 버전을 올리면 그보다 앞선 빌드가
//! 이 DB를 열지 못하는데(`SchemaTooNew`), 모르는 표는 옛 빌드가 그냥 지나칩니다. 휘발성 채널 글은 데몬이 넣지 않습니다.

use rusqlite::{Connection, OptionalExtension, params};
use yangpa_core::time::Timestamp;

use crate::db::{Store, ts_to_sql};
use crate::error::Result;

/// 캐시 행 상한. 넘치면 가장 오래 쓰지 않은 것부터 지웁니다.
pub const AI_CACHE_MAX_ROWS: i64 = 50_000;

pub(crate) fn install(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS ai_translations (
             key     BLOB PRIMARY KEY,   -- BLAKE3(형식·주소·모델·언어·원문)
             text    TEXT NOT NULL,      -- 번역문
             source  TEXT,               -- AI가 알려 준 원본 언어
             used_at INTEGER NOT NULL
         ) WITHOUT ROWID;
         CREATE INDEX IF NOT EXISTS ai_translations_used ON ai_translations(used_at);",
    )?;
    Ok(())
}

/// 캐시에 둔 번역 하나.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CachedTranslation {
    pub text: String,
    pub source: Option<String>,
}

impl Store {
    /// 열쇠마다 캐시된 번역(없으면 None). 찾은 행은 쓴 시각을 새로 적습니다.
    pub fn ai_cache_get(&self, keys: &[[u8; 32]]) -> Result<Vec<Option<CachedTranslation>>> {
        let now = ts_to_sql(Timestamp::now());
        self.write(|tx| {
            let mut get =
                tx.prepare_cached("SELECT text, source FROM ai_translations WHERE key = ?1")?;
            let mut touch =
                tx.prepare_cached("UPDATE ai_translations SET used_at = ?2 WHERE key = ?1")?;
            let mut out = Vec::with_capacity(keys.len());
            for k in keys {
                let hit = get
                    .query_row([&k[..]], |r| {
                        Ok(CachedTranslation {
                            text: r.get(0)?,
                            source: r.get(1)?,
                        })
                    })
                    .optional()?;
                if hit.is_some() {
                    touch.execute(params![&k[..], now])?;
                }
                out.push(hit);
            }
            Ok(out)
        })
    }

    /// 번역을 넣고, 상한을 넘으면 오래 쓰지 않은 행을 지웁니다.
    pub fn ai_cache_put(&self, rows: &[([u8; 32], CachedTranslation)]) -> Result<()> {
        self.ai_cache_put_max(rows, AI_CACHE_MAX_ROWS)
    }

    fn ai_cache_put_max(&self, rows: &[([u8; 32], CachedTranslation)], max: i64) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let now = ts_to_sql(Timestamp::now());
        self.write(|tx| {
            let mut put = tx.prepare_cached(
                "INSERT OR REPLACE INTO ai_translations (key, text, source, used_at) VALUES (?1, ?2, ?3, ?4)",
            )?;
            for (k, v) in rows {
                put.execute(params![&k[..], v.text, v.source, now])?;
            }
            let n: i64 = tx.query_row("SELECT count(*) FROM ai_translations", [], |r| r.get(0))?;
            if n > max {
                tx.execute(
                    "DELETE FROM ai_translations WHERE key IN
                         (SELECT key FROM ai_translations ORDER BY used_at LIMIT ?1)",
                    [n - max],
                )?;
            }
            Ok(())
        })
    }

    /// 캐시를 모두 지웁니다(지운 행 수).
    pub fn ai_cache_clear(&self) -> Result<usize> {
        self.write(|tx| Ok(tx.execute("DELETE FROM ai_translations", [])?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tr(s: &str) -> CachedTranslation {
        CachedTranslation {
            text: s.into(),
            source: Some("한국어".into()),
        }
    }

    #[test]
    fn put_get_clear() {
        let (store, _dir) = Store::open_temp().unwrap();
        let (a, b) = ([1u8; 32], [2u8; 32]);
        assert_eq!(store.ai_cache_get(&[a, b]).unwrap(), vec![None, None]);
        store.ai_cache_put(&[(a, tr("hello"))]).unwrap();
        assert_eq!(
            store.ai_cache_get(&[a, b]).unwrap(),
            vec![Some(tr("hello")), None]
        );
        store.ai_cache_put(&[(a, tr("hi"))]).unwrap();
        assert_eq!(store.ai_cache_get(&[a]).unwrap(), vec![Some(tr("hi"))]);
        assert_eq!(store.ai_cache_clear().unwrap(), 1);
        assert_eq!(store.ai_cache_get(&[a]).unwrap(), vec![None]);
    }

    #[test]
    fn install_again_keeps_rows() {
        let (store, _dir) = Store::open_temp().unwrap();
        store.ai_cache_put(&[([3u8; 32], tr("kept"))]).unwrap();
        // 다시 열 때처럼 한 번 더 만들어도(IF NOT EXISTS) 행이 남습니다.
        store.write(|tx| install(tx)).unwrap();
        assert_eq!(
            store.ai_cache_get(&[[3u8; 32]]).unwrap(),
            vec![Some(tr("kept"))]
        );
    }

    #[test]
    fn over_cap_drops_least_recently_used() {
        let (store, _dir) = Store::open_temp().unwrap();
        let (a, b, c) = ([1u8; 32], [2u8; 32], [3u8; 32]);
        store
            .write(|tx| {
                Ok(tx.execute(
                    "INSERT INTO ai_translations VALUES (?1, 'a', NULL, 1), (?2, 'b', NULL, 2)",
                    params![&a[..], &b[..]],
                )?)
            })
            .unwrap();
        store.ai_cache_put_max(&[(c, tr("c"))], 2).unwrap();
        let got = store.ai_cache_get(&[a, b, c]).unwrap();
        assert!(got[0].is_none());
        assert!(got[1].is_some() && got[2].is_some());
    }
}
