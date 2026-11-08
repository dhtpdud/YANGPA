//! 공간 목록과 해소된 상태 저장(행 단위는 [`crate::space_state`]), 공간 데이터 일괄 삭제.

use std::collections::BTreeMap;

use rusqlite::{Connection, OptionalExtension, params};
use yangpa_core::ids::{BlobId, ChannelId, SpaceId};
use yangpa_core::model::{ChannelSettings, SpaceState};
use yangpa_core::time::Timestamp;

use crate::db::{Store, ts_from_sql, ts_to_sql};
use crate::error::Result;
use crate::space_state::{StateDelta, write_state_delta};

/// 공간 하나의 로컬 기록. 해소된 상태는 [`Store::load_space_state`]로 따로 읽습니다(3만 명 상태를 목록마다 읽지 않게).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpaceRecord {
    pub space: SpaceId,
    /// 휴면: 실시간 전파를 끊고 가끔만 따라잡는 공간.
    pub dormant: bool,
    pub joined_at: Timestamp,
    pub updated_at: Timestamp,
    /// 마지막으로 연 시각(휴면 판단용).
    pub last_read_at: Option<Timestamp>,
}

type SpaceRow = ([u8; 16], bool, i64, i64, Option<i64>);

fn read_space(r: &rusqlite::Row<'_>) -> rusqlite::Result<SpaceRow> {
    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
}

fn decode_space(row: SpaceRow) -> SpaceRecord {
    let (space, dormant, joined_at, updated_at, last_read_at) = row;
    SpaceRecord {
        space: SpaceId(space),
        dormant,
        joined_at: ts_from_sql(joined_at),
        updated_at: ts_from_sql(updated_at),
        last_read_at: last_read_at.map(ts_from_sql),
    }
}

/// 옛 상태 캐시(JSON)에서 채널 표만 읽는 모양. 상태의 다른 필드 형식이 바뀌어도(기기 기록 등) 채널은 읽힙니다.
#[derive(serde::Deserialize)]
struct ChannelsOnly {
    #[serde(default)]
    channels: BTreeMap<ChannelId, ChannelSettings>,
}

/// 이전(버전 2): 이미 캐시된 상태들로 `volatile_channels`를 채웁니다. 읽을 수 없는 캐시는 건너뜁니다
/// (다음 상태 해소 때 다시 저장되며 채워짐).
pub(crate) fn backfill_volatile_channels(c: &Connection) -> Result<()> {
    let rows: Vec<([u8; 16], String)> = {
        let mut stmt = c.prepare("SELECT space, state FROM spaces WHERE state IS NOT NULL")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let mut stmt =
        c.prepare("INSERT OR IGNORE INTO volatile_channels (space, channel) VALUES (?1, ?2)")?;
    for (space, json) in rows {
        let Ok(state) = serde_json::from_str::<ChannelsOnly>(&json) else {
            continue;
        };
        for (channel, settings) in &state.channels {
            if settings.is_volatile() {
                stmt.execute(params![space, channel.as_bytes()])?;
            }
        }
    }
    Ok(())
}

const SPACE_SELECT: &str = "SELECT space, dormant, joined_at, updated_at, last_read_at FROM spaces";

/// 공간을 지울 때 `space` 열로 함께 지우는 표.
const SPACE_TABLES: &[&str] = &[
    "heads",
    "missing",
    "tombstones",
    "reactions",
    "read_markers",
    "invite_redemptions",
    "volatile_channels",
    "space_members",
    "space_bans",
    "space_channels",
    "device_keys",
];

impl Store {
    /// 공간을 목록에 넣습니다(이미 있으면 그대로). 새로 넣었으면 true.
    pub fn ensure_space(&self, space: &SpaceId, joined_at: Timestamp) -> Result<bool> {
        self.write(|tx| {
            Ok(tx
                .prepare_cached("INSERT OR IGNORE INTO spaces (space, joined_at, updated_at) VALUES (?1, ?2, ?2)")?
                .execute(params![space.as_bytes(), ts_to_sql(joined_at)])?
                > 0)
        })
    }

    /// 해소된 상태 전체를 다시 씁니다(공간이 없으면 만듦, 그 공간의 상태 행을 먼저 지움). 데몬은 바뀐 행만 쓰는
    /// [`Store::apply_state_delta`]를 씁니다. 상태의 휘발성 채널은 따로 기록해 두어, 호출자가 보안 등급을 잘못 넘겨도
    /// 그 채널 이벤트가 디스크 저장 API를 통과하지 못하게 합니다.
    pub fn put_space_state(&self, space: &SpaceId, state: &SpaceState) -> Result<()> {
        let delta = StateDelta::between(None, state)?;
        self.write(|tx| {
            for table in ["space_members", "space_bans", "space_channels"] {
                tx.execute(
                    &format!("DELETE FROM {table} WHERE space = ?1"),
                    [space.as_bytes()],
                )?;
            }
            write_state_delta(tx, space, &delta)
        })
    }

    pub fn get_space(&self, space: &SpaceId) -> Result<Option<SpaceRecord>> {
        let row = self.read(|c| {
            Ok(
                c.prepare_cached(&format!("{SPACE_SELECT} WHERE space = ?1"))?
                    .query_row([space.as_bytes()], read_space)
                    .optional()?,
            )
        })?;
        Ok(row.map(decode_space))
    }

    pub fn list_spaces(&self) -> Result<Vec<SpaceRecord>> {
        let rows = self.read(|c| {
            let mut stmt =
                c.prepare_cached(&format!("{SPACE_SELECT} ORDER BY joined_at, space"))?;
            let rows = stmt.query_map([], read_space)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })?;
        Ok(rows.into_iter().map(decode_space).collect())
    }

    pub fn space_ids(&self) -> Result<Vec<SpaceId>> {
        self.read(|c| {
            let mut stmt =
                c.prepare_cached("SELECT space FROM spaces ORDER BY joined_at, space")?;
            let rows = stmt.query_map([], |r| Ok(SpaceId(r.get(0)?)))?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })
    }

    pub fn set_space_dormant(&self, space: &SpaceId, dormant: bool) -> Result<bool> {
        self.write(|tx| {
            Ok(tx
                .prepare_cached("UPDATE spaces SET dormant = ?2 WHERE space = ?1")?
                .execute(params![space.as_bytes(), dormant])?
                > 0)
        })
    }

    pub fn touch_space_read(&self, space: &SpaceId, at: Timestamp) -> Result<bool> {
        self.write(|tx| {
            Ok(tx
                .prepare_cached("UPDATE spaces SET last_read_at = MAX(COALESCE(last_read_at, 0), ?2) WHERE space = ?1")?
                .execute(params![space.as_bytes(), ts_to_sql(at)])?
                > 0)
        })
    }

    /// 공간을 떠날 때: 그 공간의 이벤트·메시지·키·머리·빈 구간·반응·읽음 위치·초대 기록·상태 행·기기 키와,
    /// 다른 공간에서 쓰지 않는 블롭(파일 포함)을 모두 지웁니다. 지운 블롭 목록을 돌려줍니다.
    pub fn purge_space(&self, space: &SpaceId) -> Result<Vec<BlobId>> {
        let sp = space.as_bytes();
        let blobs = self.write(|tx| {
            // 이 공간에만 속한 블롭.
            let only_here: Vec<BlobId> = {
                let mut stmt = tx.prepare_cached(
                    "SELECT b.blob_id FROM blob_spaces b WHERE b.space = ?1 AND NOT EXISTS \
                     (SELECT 1 FROM blob_spaces o WHERE o.blob_id = b.blob_id AND o.space != ?1)",
                )?;
                let rows = stmt.query_map([sp], |r| Ok(BlobId(r.get(0)?)))?;
                rows.collect::<rusqlite::Result<_>>()?
            };
            // 수정 기록은 원본 메시지 ID로 묶여 있어 메시지를 지우기 전에 정리합니다. 원본이 끝내 오지 않은
            // 수정도 남지 않게 수정 이벤트 쪽으로도 찾습니다.
            tx.execute(
                "DELETE FROM message_edits WHERE target IN (SELECT event_id FROM messages WHERE space = ?1) \
                 OR edit_id IN (SELECT id FROM events WHERE space = ?1)",
                [sp],
            )?;
            tx.execute(
                "DELETE FROM pending_edits WHERE edit_id IN (SELECT id FROM events WHERE space = ?1) \
                 OR target IN (SELECT event_id FROM messages WHERE space = ?1)",
                [sp],
            )?;
            // msg_keys·messages는 외래 키(ON DELETE CASCADE)로 함께 지워집니다.
            tx.execute("DELETE FROM events WHERE space = ?1", [sp])?;
            for table in SPACE_TABLES {
                tx.execute(&format!("DELETE FROM {table} WHERE space = ?1"), [sp])?;
            }
            tx.execute("DELETE FROM keys WHERE space = ?1", [sp])?;
            tx.execute("DELETE FROM blob_spaces WHERE space = ?1", [sp])?;
            for b in &only_here {
                tx.execute("DELETE FROM blobs WHERE blob_id = ?1", [b.as_bytes()])?;
            }
            tx.execute("UPDATE friends SET space = NULL WHERE space = ?1", [sp])?;
            tx.execute("DELETE FROM spaces WHERE space = ?1", [sp])?;
            Ok(only_here)
        })?;
        for b in &blobs {
            self.manifests.lock().remove(b);
            self.blob_files.remove(b)?;
        }
        self.checkpoint_soon();
        Ok(blobs)
    }
}
