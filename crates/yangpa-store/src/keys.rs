//! 상태 키·채널 뿌리, 비밀값(신원·기기·네트워크 키, 키 그룹 상태).
//!
//! 휘발성 세션 키는 이 모듈에 넣을 수 없습니다([`KeyKind`]에 휘발성 종류가 없음). [`crate::VolatileStore`]만 다룹니다.

use rusqlite::{OptionalExtension, params};
use yangpa_core::body::Secret32;
use yangpa_core::ids::SpaceId;
use yangpa_core::time::Timestamp;
use zeroize::Zeroizing;

use crate::db::{Store, from_sql_u64, to_sql_u64, ts_from_sql, ts_to_sql};
use crate::error::{Result, StoreError};

const MAX_KEY_ID_LEN: usize = 128;

const MAX_SECRET_LEN: usize = 16 * 1024 * 1024;

/// 키 하나의 ID·범위 길이 검사(트랜잭션 전에).
pub(crate) fn check_key_entry(e: &KeyEntry) -> Result<()> {
    if e.id.is_empty() || e.id.len() > MAX_KEY_ID_LEN || e.scope.len() > MAX_KEY_ID_LEN {
        return Err(StoreError::InvalidInput("키 ID·범위 길이"));
    }
    Ok(())
}

/// 트랜잭션 안에서 키를 넣습니다(같은 (종류, ID)가 있으면 그대로). 새로 넣었으면 true.
pub(crate) fn put_key_entry_tx(tx: &rusqlite::Transaction<'_>, e: &KeyEntry) -> Result<bool> {
    Ok(tx
        .prepare_cached(
            "INSERT OR IGNORE INTO keys (kind, id, scope, space, epoch, not_before, key, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )?
        .execute(params![
            e.kind.code(),
            e.id,
            e.scope,
            e.space.as_ref().map(|s| s.as_bytes()),
            e.epoch.map(to_sql_u64),
            e.not_before.map(ts_to_sql),
            &e.key.0,
            ts_to_sql(e.created_at)
        ])?
        > 0)
}

/// 디스크에 두는 키 종류.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum KeyKind {
    /// 공간 상태 키(id = `StateKeyId`).
    State,
    /// 일반 채널 기록 키의 채널 뿌리(id = `ChannelRootId`, 본문은 그 뿌리에서 파생한 기록 키로 엶).
    History,
    // 3은 옛 MLS 에포크 키라 다시 쓰지 않습니다(스키마 6판이 그 행을 지움).
}

impl KeyKind {
    fn code(self) -> i64 {
        match self {
            KeyKind::State => 1,
            KeyKind::History => 2,
        }
    }

    fn from_code(c: i64) -> Result<Self> {
        match c {
            1 => Ok(KeyKind::State),
            2 => Ok(KeyKind::History),
            _ => Err(StoreError::Corrupt("키 종류")),
        }
    }
}

/// 키 하나와 메타데이터.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyEntry {
    pub kind: KeyKind,
    pub id: Vec<u8>,
    /// 키를 고를 때 묶는 범위(예: 기록 키는 채널 ID, 상태 키는 공간 ID).
    pub scope: Vec<u8>,
    /// 소속 공간(공간을 떠날 때 함께 지움).
    pub space: Option<SpaceId>,
    pub epoch: Option<u64>,
    /// 기록 키의 사용 시작 시각.
    pub not_before: Option<Timestamp>,
    pub key: Secret32,
    pub created_at: Timestamp,
}

/// 비밀값 이름.
pub mod secret_names {
    /// 신원 키 시드.
    pub const IDENTITY: &str = "identity";
    /// 기기 키(서명 + 상자) 직렬화.
    pub const DEVICE: &str = "device";
    /// 직접 연결 주 스웜(`--direct`) 네트워크 키. Tor로 도는 기기는 시작할 때 지웁니다(예전 빌드의 일반 모드 키).
    pub const NET_KEY: &str = "net_key";
    /// Tor 스웜 네트워크 키(앱 사용자 기기의 주 스웜, 직접 연결 키와 분리).
    pub const NET_KEY_TOR: &str = "net_key_tor";
}

type KeyRow = (
    i64,
    Vec<u8>,
    Vec<u8>,
    Option<[u8; 16]>,
    Option<i64>,
    Option<i64>,
    [u8; 32],
    i64,
);

fn read_key_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<KeyRow> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
        r.get(6)?,
        r.get(7)?,
    ))
}

fn decode_key_row(row: KeyRow) -> Result<KeyEntry> {
    let (kind, id, scope, space, epoch, not_before, key, created_at) = row;
    Ok(KeyEntry {
        kind: KeyKind::from_code(kind)?,
        id,
        scope,
        space: space.map(SpaceId),
        epoch: epoch.map(from_sql_u64),
        not_before: not_before.map(ts_from_sql),
        key: Secret32(key),
        created_at: ts_from_sql(created_at),
    })
}

const KEY_SELECT: &str =
    "SELECT kind, id, scope, space, epoch, not_before, key, created_at FROM keys";

impl Store {
    /// 키를 넣습니다. 같은 (종류, ID)가 이미 있으면 바꾸지 않습니다(먼저 받은 키가 이김: 같은 ID로 다른 키를 보내
    /// 예전 메시지를 못 읽게 만드는 공격 방지). 새로 넣었으면 true.
    pub fn put_key(&self, kind: KeyKind, id: &[u8], scope: &[u8], key: &Secret32) -> Result<bool> {
        self.put_key_entry(&KeyEntry {
            kind,
            id: id.to_vec(),
            scope: scope.to_vec(),
            space: None,
            epoch: None,
            not_before: None,
            key: key.clone(),
            created_at: Timestamp::now(),
        })
    }

    pub fn put_key_entry(&self, e: &KeyEntry) -> Result<bool> {
        check_key_entry(e)?;
        self.write(|tx| put_key_entry_tx(tx, e))
    }

    pub fn get_key(&self, kind: KeyKind, id: &[u8]) -> Result<Option<Secret32>> {
        Ok(self.get_key_entry(kind, id)?.map(|e| e.key))
    }

    pub fn get_key_entry(&self, kind: KeyKind, id: &[u8]) -> Result<Option<KeyEntry>> {
        let row = self.read(|c| {
            Ok(
                c.prepare_cached(&format!("{KEY_SELECT} WHERE kind = ?1 AND id = ?2"))?
                    .query_row(params![kind.code(), id], read_key_row)
                    .optional()?,
            )
        })?;
        row.map(decode_key_row).transpose()
    }

    /// 범위 안 키들(최신 우선: `epoch` 열(순번), 사용 시작, 생성 시각 내림차순).
    pub fn keys_in_scope(&self, kind: KeyKind, scope: &[u8]) -> Result<Vec<KeyEntry>> {
        let rows = self.read(|c| {
            let mut stmt = c.prepare_cached(&format!(
                "{KEY_SELECT} WHERE kind = ?1 AND scope = ?2 \
                 ORDER BY epoch DESC, not_before DESC, created_at DESC, id"
            ))?;
            let rows = stmt.query_map(params![kind.code(), scope], read_key_row)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })?;
        rows.into_iter().map(decode_key_row).collect()
    }

    /// 범위 안 최신 키 `limit`개(정렬은 [`Self::keys_in_scope`]와 같음). 오래된 키가 많아도 이만큼만 풉니다.
    pub fn recent_keys_in_scope(
        &self,
        kind: KeyKind,
        scope: &[u8],
        limit: usize,
    ) -> Result<Vec<KeyEntry>> {
        let rows = self.read(|c| {
            let mut stmt = c.prepare_cached(&format!(
                "{KEY_SELECT} WHERE kind = ?1 AND scope = ?2 \
                 ORDER BY epoch DESC, not_before DESC, created_at DESC, id LIMIT ?3"
            ))?;
            let rows = stmt.query_map(
                params![kind.code(), scope, i64::try_from(limit).unwrap_or(i64::MAX)],
                read_key_row,
            )?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })?;
        rows.into_iter().map(decode_key_row).collect()
    }

    /// 순번(`epoch`)이 없는 키에 순번을 적습니다(다른 길로 먼저 들어와 순번 없이 저장된 키). 이미 있으면 그대로.
    pub fn set_key_epoch(&self, kind: KeyKind, id: &[u8], epoch: u64) -> Result<()> {
        self.write(|tx| {
            tx.prepare_cached(
                "UPDATE keys SET epoch = ?3 WHERE kind = ?1 AND id = ?2 AND epoch IS NULL",
            )?
            .execute(params![kind.code(), id, to_sql_u64(epoch)])?;
            Ok(())
        })
    }

    /// 범위에서 가장 최신 키.
    pub fn latest_key(&self, kind: KeyKind, scope: &[u8]) -> Result<Option<KeyEntry>> {
        Ok(self.keys_in_scope(kind, scope)?.into_iter().next())
    }

    pub fn delete_key(&self, kind: KeyKind, id: &[u8]) -> Result<bool> {
        self.write(|tx| {
            Ok(tx
                .prepare_cached("DELETE FROM keys WHERE kind = ?1 AND id = ?2")?
                .execute(params![kind.code(), id])?
                > 0)
        })
    }

    pub fn delete_keys_in_scope(&self, kind: KeyKind, scope: &[u8]) -> Result<usize> {
        self.write(|tx| {
            Ok(tx
                .prepare_cached("DELETE FROM keys WHERE kind = ?1 AND scope = ?2")?
                .execute(params![kind.code(), scope])?)
        })
    }

    // ---- 비밀값 ----

    pub fn put_secret(&self, name: &str, value: &[u8]) -> Result<()> {
        if name.is_empty() || name.len() > 128 || value.len() > MAX_SECRET_LEN {
            return Err(StoreError::InvalidInput("비밀값 이름·크기"));
        }
        self.write(|tx| {
            tx.prepare_cached(
                "INSERT OR REPLACE INTO secrets (name, value, updated_at) VALUES (?1, ?2, ?3)",
            )?
            .execute(params![name, value, ts_to_sql(Timestamp::now())])?;
            Ok(())
        })
    }

    /// 비밀값. 돌려준 버퍼는 해제 시 0으로 덮어씁니다.
    pub fn get_secret(&self, name: &str) -> Result<Option<Zeroizing<Vec<u8>>>> {
        self.read(|c| {
            let v: Option<Vec<u8>> = c
                .prepare_cached("SELECT value FROM secrets WHERE name = ?1")?
                .query_row([name], |r| r.get(0))
                .optional()?;
            Ok(v.map(Zeroizing::new))
        })
    }

    pub fn delete_secret(&self, name: &str) -> Result<bool> {
        self.write(|tx| {
            Ok(tx
                .prepare_cached("DELETE FROM secrets WHERE name = ?1")?
                .execute([name])?
                > 0)
        })
    }
}
