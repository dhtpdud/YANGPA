//! 이벤트 저장: 중복 제거, 본문 재봉인, 스트림 머리·받지 않은 기록 관리, 구간 지문.

use std::collections::HashMap;

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use yangpa_core::body::Secret32;
use yangpa_core::event::{EventHeader, EventKind, KeyRef, Lane, SignedEvent, StreamId};
use yangpa_core::ids::{ChannelId, EventId, SpaceId};
use yangpa_core::model::SecurityLevel;
use yangpa_core::sign::Sig;
use yangpa_core::time::Timestamp;
use yangpa_core::wire::{LamportRange, RangeFingerprint};

use crate::db::{Store, from_sql_u64, lane_code, placeholders, stream_key, to_sql_u64, ts_to_sql};
use crate::error::{Result, StoreError};
use crate::messages::{self, MessageRecord};

/// 본문 상태(events.body_state).
pub(crate) const BODY_ABSENT: i64 = 0;
pub(crate) const BODY_PRESENT: i64 = 1;
pub(crate) const BODY_ERASED: i64 = 2;

/// 한 번에 조회할 수 있는 ID 수(SQLite 인자 상한 아래로).
const MAX_IDS_PER_QUERY: usize = 512;
/// `ids_in_range`가 한 번에 돌려주는 최대 개수. `limit`은 네트워크 요청에서 오므로 메모리를 묶어 둡니다.
pub const MAX_RANGE_IDS: u32 = 65_536;
/// `stream_events`가 한 번에 돌려주는 최대 이벤트 수(본문 포함이라 더 작게).
pub const MAX_RANGE_EVENTS: u32 = 4_096;
/// `expire_channel_messages` 한 번에 지우는 최대 이벤트 수(쓰기 잠금을 오래 잡지 않게).
pub const MAX_EXPIRE_BATCH: u32 = 10_000;

/// 디스크에 저장해도 되는 이벤트. 디스크 저장 API는 이 타입만 받습니다.
///
/// 휘발성 채널의 이벤트(또는 휘발성 세션 키로 암호화된 이벤트)로는 만들 수 없어서, 휘발성 내용이
/// 실수로 디스크 경로에 들어가는 코드는 실행 전에 막힙니다.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PersistentEvent {
    event: SignedEvent,
    id: EventId,
}

impl PersistentEvent {
    /// `channel_security`는 이벤트가 속한 채널의 보안 등급입니다(공간 수준 이벤트는 `Normal`).
    ///
    /// 서명 검증은 호출자(동기화 계층) 몫이고, 여기서는 저장에 필요한 구조·본문 해시만 확인합니다.
    pub fn new(event: SignedEvent, channel_security: SecurityLevel) -> Result<Self> {
        if channel_security == SecurityLevel::Volatile
            || matches!(event.header.key, KeyRef::Volatile { .. })
        {
            return Err(StoreError::VolatileRejected);
        }
        event
            .header
            .check_shape()
            .map_err(|_| StoreError::InvalidInput("이벤트 헤더 형식"))?;
        // SQLite 정수(i64)에 순서를 지키며 담기 위해. 이 범위를 넘는 논리 시계는 정상 동작으로 생기지 않습니다.
        if event.header.lamport >= i64::MAX as u64 {
            return Err(StoreError::InvalidInput("논리 시계 범위"));
        }
        if let Some(body) = &event.body
            && (body.len() != event.header.body_len as usize
                || *blake3::hash(body).as_bytes() != event.header.body_hash)
        {
            return Err(StoreError::InvalidInput("본문 해시 불일치"));
        }
        let id = event.id();
        Ok(PersistentEvent { event, id })
    }

    /// 공간 수준 스트림(상태·키 합의 레인 등) 이벤트. 채널 스트림 이벤트면 거부합니다.
    pub fn space_level(event: SignedEvent) -> Result<Self> {
        if event.header.stream() != StreamId::Space {
            return Err(StoreError::InvalidInput("공간 수준 이벤트가 아님"));
        }
        Self::new(event, SecurityLevel::Normal)
    }

    pub fn id(&self) -> EventId {
        self.id
    }

    pub fn event(&self) -> &SignedEvent {
        &self.event
    }

    pub fn into_inner(self) -> SignedEvent {
        self.event
    }
}

/// 이벤트 하나를 넣은 결과.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InsertOutcome {
    /// 새로 저장함.
    Inserted,
    /// 이미 있음(바뀐 것 없음).
    Duplicate,
    /// 헤더만 있던 이벤트에 본문을 채움.
    BodyAdded,
}

/// 받지 않은 부모 하나('받지 않은 기록' 구간의 경계).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MissingParent {
    pub id: EventId,
    /// 이 부모를 가리킨 자식 중 논리 시계가 가장 작은 것.
    pub child: EventId,
    pub child_lamport: u64,
    pub since: Timestamp,
}

/// 저장된 이벤트의 메타데이터(본문 없이).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EventMeta {
    pub id: EventId,
    pub space: SpaceId,
    pub stream: StreamId,
    pub kind: EventKind,
    pub lamport: u64,
    /// 본문을 갖고 있는지(미수신·폐기면 false).
    pub has_body: bool,
    /// 본문이 폐기(삭제·수명 만료)되었는지.
    pub erased: bool,
    pub seed_excluded: bool,
    pub received_at: Timestamp,
}

const EVENT_SELECT: &str = "SELECT e.id, e.header, e.sig, e.body_sealed, k.key FROM events e \
                            LEFT JOIN msg_keys k ON k.event_id = e.id";

type EventRow = (
    [u8; 32],
    Vec<u8>,
    Vec<u8>,
    Option<Vec<u8>>,
    Option<[u8; 32]>,
);

fn read_event_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<EventRow> {
    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
}

/// 저장된 행을 원래 서명 이벤트로 되돌립니다. 본문은 저장 키로 풀어 전송 암호문 그대로 돌려줍니다.
fn decode_event(row: EventRow) -> Result<SignedEvent> {
    let (id, header, sig, sealed, key) = row;
    let header: EventHeader =
        postcard::from_bytes(&header).map_err(|_| StoreError::Corrupt("이벤트 헤더"))?;
    let body = match (sealed, key) {
        (Some(sealed), Some(key)) => {
            let key = Secret32(key);
            Some(
                yangpa_crypto::open(&key, &id, &sealed)
                    .map_err(|_| StoreError::Corrupt("이벤트 본문"))?
                    .to_vec(),
            )
        }
        _ => None,
    };
    let ev = SignedEvent {
        header,
        sig: Sig::from_bytes(&sig).map_err(|_| StoreError::Corrupt("이벤트 서명"))?,
        body,
    };
    // 헤더·서명이 바뀌지 않았는지(디스크 손상 탐지). BLAKE3라 싸게 확인됩니다.
    if ev.id().0 != id {
        return Err(StoreError::Corrupt("이벤트 ID 불일치"));
    }
    Ok(ev)
}

fn seal_body(id: &EventId, body: &[u8]) -> (Secret32, Vec<u8>) {
    let key = Secret32::random();
    let sealed = yangpa_crypto::seal(&key, id.as_bytes(), body);
    (key, sealed)
}

/// 공간 상태 캐시가 휘발성으로 표시한 채널인지.
pub(crate) fn is_volatile_channel(
    c: &Connection,
    space: &SpaceId,
    channel: &ChannelId,
) -> Result<bool> {
    Ok(c.prepare_cached(
        "SELECT EXISTS(SELECT 1 FROM volatile_channels WHERE space = ?1 AND channel = ?2)",
    )?
    .query_row(params![space.as_bytes(), channel.as_bytes()], |r| r.get(0))?)
}

/// 묶음 안에 상태 캐시상 휘발성 채널의 이벤트가 있으면 거부합니다. `PersistentEvent::new`는 호출자가 넘긴
/// 보안 등급만 믿으므로, 등급을 잘못 넘긴 호출(예: 휘발성 채널의 봉인 `KeyShare`)을 여기서 한 번 더 막습니다.
fn reject_volatile_channels(tx: &Transaction<'_>, evs: &[PersistentEvent]) -> Result<()> {
    let mut seen: HashMap<(SpaceId, ChannelId), bool> = HashMap::new();
    for ev in evs {
        let h = &ev.event.header;
        let StreamId::Channel(channel) = h.stream() else {
            continue;
        };
        let key = (h.space, channel);
        let volatile = match seen.get(&key) {
            Some(v) => *v,
            None => {
                let v = is_volatile_channel(tx, &h.space, &channel)?;
                seen.insert(key, v);
                v
            }
        };
        if volatile {
            return Err(StoreError::VolatileRejected);
        }
    }
    Ok(())
}

pub(crate) fn is_tombstoned(c: &Connection, id: &EventId) -> Result<bool> {
    Ok(
        c.prepare_cached("SELECT EXISTS(SELECT 1 FROM tombstones WHERE event_id = ?1)")?
            .query_row([id.as_bytes()], |r| r.get(0))?,
    )
}

/// 트랜잭션 안에서 이벤트 하나를 넣습니다.
fn insert_one(tx: &Transaction<'_>, ev: &PersistentEvent, now: i64) -> Result<InsertOutcome> {
    let h = &ev.event.header;
    let id = ev.id;
    let space = h.space;
    let stream = stream_key(&h.stream());
    // 삭제된 메시지의 원본·수정본이 늦게 오면 헤더만 남깁니다.
    let tombstoned = h.kind.lane() == Lane::Message && is_tombstoned(tx, &id)?;

    let existing: Option<i64> = tx
        .prepare_cached("SELECT body_state FROM events WHERE id = ?1")?
        .query_row([id.as_bytes()], |r| r.get(0))
        .optional()?;
    if let Some(state) = existing {
        if state == BODY_ABSENT
            && !tombstoned
            && let Some(body) = &ev.event.body
        {
            let (key, sealed) = seal_body(&id, body);
            tx.prepare_cached("UPDATE events SET body_sealed = ?2, body_state = ?3 WHERE id = ?1")?
                .execute(params![id.as_bytes(), sealed, BODY_PRESENT])?;
            tx.prepare_cached("INSERT OR REPLACE INTO msg_keys (event_id, key) VALUES (?1, ?2)")?
                .execute(params![id.as_bytes(), &key.0])?;
            return Ok(InsertOutcome::BodyAdded);
        }
        return Ok(InsertOutcome::Duplicate);
    }

    let (sealed, state, key) = match (&ev.event.body, tombstoned) {
        (_, true) => (None, BODY_ERASED, None),
        (Some(body), false) => {
            let (key, sealed) = seal_body(&id, body);
            (Some(sealed), BODY_PRESENT, Some(key))
        }
        (None, false) => (None, BODY_ABSENT, None),
    };
    tx.prepare_cached(
        "INSERT INTO events (id, space, stream, channel, kind, lane, author_device, author_user, lamport, \
         created_at, header, sig, body_sealed, body_state, received_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
    )?
    .execute(params![
        id.as_bytes(),
        space.as_bytes(),
        &stream,
        h.channel.as_ref().map(|c| c.as_bytes()),
        h.kind.code(),
        lane_code(h.kind.lane()),
        h.author.as_bytes(),
        h.user.as_bytes(),
        to_sql_u64(h.lamport),
        ts_to_sql(h.created_at),
        h.canonical_bytes(),
        ev.event.sig.as_bytes(),
        sealed,
        state,
        now,
    ])?;
    if let Some(key) = &key {
        tx.prepare_cached("INSERT INTO msg_keys (event_id, key) VALUES (?1, ?2)")?
            .execute(params![id.as_bytes(), &key.0])?;
    }

    // DAG: 이 이벤트를 부모로 가리킨 자식이 이미 있으면(받지 않은 기록이었으면) 머리가 아닙니다.
    let was_missing: bool = tx
        .prepare_cached("SELECT EXISTS(SELECT 1 FROM missing WHERE space = ?1 AND stream = ?2 AND event_id = ?3)")?
        .query_row(params![space.as_bytes(), &stream, id.as_bytes()], |r| r.get(0))?;
    tx.prepare_cached("DELETE FROM missing WHERE event_id = ?1")?
        .execute([id.as_bytes()])?;
    if !was_missing {
        tx.prepare_cached("INSERT OR IGNORE INTO heads (space, stream, event_id, lamport) VALUES (?1, ?2, ?3, ?4)")?
            .execute(params![space.as_bytes(), &stream, id.as_bytes(), to_sql_u64(h.lamport)])?;
    }
    for parent in &h.parents {
        let loc: Option<([u8; 16], Vec<u8>)> = tx
            .prepare_cached("SELECT space, stream FROM events WHERE id = ?1")?
            .query_row([parent.as_bytes()], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        match loc {
            Some((ps, pst)) if ps == space.0 && pst == stream => {
                tx.prepare_cached(
                    "DELETE FROM heads WHERE space = ?1 AND stream = ?2 AND event_id = ?3",
                )?
                .execute(params![space.as_bytes(), &stream, parent.as_bytes()])?;
            }
            // 다른 스트림·공간의 이벤트를 가리키는 부모는 프로토콜 위반입니다. 검증은 동기화 계층 몫이고,
            // 여기서는 다른 스트림의 머리를 건드리지도, 영원히 채워지지 않을 빈 구간을 만들지도 않습니다.
            Some(_) => {}
            None => {
                tx.prepare_cached(
                    "INSERT INTO missing (space, stream, event_id, child, child_lamport, since) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
                     ON CONFLICT (space, stream, event_id) DO UPDATE SET \
                       child = excluded.child, child_lamport = excluded.child_lamport \
                     WHERE excluded.child_lamport < missing.child_lamport",
                )?
                .execute(params![
                    space.as_bytes(),
                    &stream,
                    parent.as_bytes(),
                    id.as_bytes(),
                    to_sql_u64(h.lamport),
                    now
                ])?;
            }
        }
    }
    Ok(InsertOutcome::Inserted)
}

/// 이벤트 본문만 폐기합니다(저장 키·봉인 본문 삭제). 메시지 보기는 건드리지 않습니다.
pub(crate) fn erase_event_body(tx: &Transaction<'_>, id: &EventId) -> Result<bool> {
    let n = tx
        .prepare_cached("UPDATE events SET body_sealed = NULL, body_state = ?2 WHERE id = ?1")?
        .execute(params![id.as_bytes(), BODY_ERASED])?;
    tx.prepare_cached("DELETE FROM msg_keys WHERE event_id = ?1")?
        .execute([id.as_bytes()])?;
    Ok(n > 0)
}

fn check_ids(ids: &[EventId]) -> Result<()> {
    if ids.len() > MAX_IDS_PER_QUERY * 64 {
        return Err(StoreError::LimitExceeded("ID 수"));
    }
    Ok(())
}

impl Store {
    /// 이벤트 묶음을 한 트랜잭션으로 넣습니다. 같은 ID는 한 번만 저장되고, 본문은 메시지마다 새 무작위 저장 키로
    /// 다시 봉인합니다. 결과는 입력 순서대로입니다.
    pub fn insert_events(&self, evs: &[PersistentEvent]) -> Result<Vec<InsertOutcome>> {
        self.insert_events_with_views(evs, &[])
    }

    /// 이벤트와 그 메시지 보기 행을 한 트랜잭션으로 넣습니다(보내기·받기 경로).
    /// 보기 행은 이벤트를 모두 넣은 뒤 적용하므로 같은 묶음의 이벤트를 가리켜도 됩니다.
    pub fn insert_events_with_views(
        &self,
        evs: &[PersistentEvent],
        views: &[MessageRecord],
    ) -> Result<Vec<InsertOutcome>> {
        let now = ts_to_sql(Timestamp::now());
        self.write(|tx| {
            reject_volatile_channels(tx, evs)?;
            let mut out = Vec::with_capacity(evs.len());
            for ev in evs {
                out.push(insert_one(tx, ev, now)?);
            }
            for v in views {
                messages::put_view_tx(tx, v)?;
            }
            Ok(out)
        })
    }

    /// 이벤트를 넣고 같은 트랜잭션에서 `then`을 부릅니다.
    pub(crate) fn insert_events_then(
        &self,
        evs: &[PersistentEvent],
        then: impl FnOnce(&rusqlite::Transaction<'_>) -> Result<()>,
    ) -> Result<Vec<InsertOutcome>> {
        let now = ts_to_sql(Timestamp::now());
        self.write(|tx| {
            reject_volatile_channels(tx, evs)?;
            let mut out = Vec::with_capacity(evs.len());
            for ev in evs {
                out.push(insert_one(tx, ev, now)?);
            }
            then(tx)?;
            Ok(out)
        })
    }

    /// 저장된 서명 이벤트. 본문은 받은 그대로의 전송 암호문이며, 미수신·폐기면 `None`입니다.
    pub fn get_event(&self, id: &EventId) -> Result<Option<SignedEvent>> {
        let row = self.read(|c| {
            Ok(
                c.prepare_cached(&format!("{EVENT_SELECT} WHERE e.id = ?1"))?
                    .query_row([id.as_bytes()], read_event_row)
                    .optional()?,
            )
        })?;
        row.map(decode_event).transpose()
    }

    /// 여러 이벤트(있는 것만, 요청 순서대로).
    pub fn get_events(&self, ids: &[EventId]) -> Result<Vec<SignedEvent>> {
        check_ids(ids)?;
        let rows = self.read(|c| {
            let mut stmt = c.prepare_cached(&format!("{EVENT_SELECT} WHERE e.id = ?1"))?;
            let mut rows = Vec::with_capacity(ids.len());
            for id in ids {
                if let Some(r) = stmt.query_row([id.as_bytes()], read_event_row).optional()? {
                    rows.push(r);
                }
            }
            Ok(rows)
        })?;
        rows.into_iter().map(decode_event).collect()
    }

    pub fn has_event(&self, id: &EventId) -> Result<bool> {
        Ok(self.has_events(std::slice::from_ref(id))?[0])
    }

    pub fn has_events(&self, ids: &[EventId]) -> Result<Vec<bool>> {
        check_ids(ids)?;
        self.read(|c| {
            let mut stmt = c.prepare_cached("SELECT EXISTS(SELECT 1 FROM events WHERE id = ?1)")?;
            ids.iter()
                .map(|id| Ok(stmt.query_row([id.as_bytes()], |r| r.get(0))?))
                .collect()
        })
    }

    /// 이벤트 메타데이터(본문 없이).
    pub fn event_meta(&self, id: &EventId) -> Result<Option<EventMeta>> {
        let row = self.read(|c| {
            Ok(c.prepare_cached(
                "SELECT header, body_state, seed_excluded, received_at FROM events WHERE id = ?1",
            )?
            .query_row([id.as_bytes()], |r| {
                Ok((
                    r.get::<_, Vec<u8>>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, bool>(2)?,
                    r.get::<_, i64>(3)?,
                ))
            })
            .optional()?)
        })?;
        let Some((header, state, excluded, received)) = row else {
            return Ok(None);
        };
        let h: EventHeader =
            postcard::from_bytes(&header).map_err(|_| StoreError::Corrupt("이벤트 헤더"))?;
        Ok(Some(EventMeta {
            id: *id,
            space: h.space,
            stream: h.stream(),
            kind: h.kind,
            lamport: h.lamport,
            has_body: state == BODY_PRESENT,
            erased: state == BODY_ERASED,
            seed_excluded: excluded,
            received_at: crate::db::ts_from_sql(received),
        }))
    }

    /// 스트림의 현재 머리. 논리 시계가 큰 순(같으면 ID 순)이라 앞에서 16개를 새 이벤트의 부모로 쓰면 됩니다.
    pub fn heads(&self, space: &SpaceId, stream: StreamId) -> Result<Vec<EventId>> {
        Ok(self
            .heads_with_lamport(space, stream)?
            .into_iter()
            .map(|(_, id)| id)
            .collect())
    }

    pub fn heads_with_lamport(
        &self,
        space: &SpaceId,
        stream: StreamId,
    ) -> Result<Vec<(u64, EventId)>> {
        let sk = stream_key(&stream);
        self.read(|c| {
            let mut stmt = c.prepare_cached(
                "SELECT lamport, event_id FROM heads WHERE space = ?1 AND stream = ?2 ORDER BY lamport DESC, event_id",
            )?;
            let rows = stmt.query_map(params![space.as_bytes(), &sk], |r| {
                Ok((from_sql_u64(r.get(0)?), EventId(r.get(1)?)))
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    /// 공간 전체 논리 시계 최댓값(이벤트가 없으면 0).
    pub fn max_lamport(&self, space: &SpaceId) -> Result<u64> {
        self.read(|c| {
            let v: Option<i64> = c
                .prepare_cached("SELECT MAX(lamport) FROM events WHERE space = ?1")?
                .query_row([space.as_bytes()], |r| r.get(0))?;
            Ok(v.map(from_sql_u64).unwrap_or(0))
        })
    }

    /// 스트림의 논리 시계 최댓값(이벤트가 없으면 0).
    pub fn stream_max_lamport(&self, space: &SpaceId, stream: StreamId) -> Result<u64> {
        let sk = stream_key(&stream);
        self.read(|c| {
            let v: Option<i64> = c
                .prepare_cached("SELECT MAX(lamport) FROM events WHERE space = ?1 AND stream = ?2")?
                .query_row(params![space.as_bytes(), &sk], |r| r.get(0))?;
            Ok(v.map(from_sql_u64).unwrap_or(0))
        })
    }

    /// 구간 [start, end) 안 이벤트의 (논리 시계, ID)를 (논리 시계, ID) 오름차순으로 최대 `limit`개
    /// (`limit`은 [`MAX_RANGE_IDS`]로 제한).
    pub fn ids_in_range(
        &self,
        space: &SpaceId,
        stream: StreamId,
        range: LamportRange,
        limit: u32,
    ) -> Result<Vec<(u64, EventId)>> {
        let sk = stream_key(&stream);
        let limit = limit.min(MAX_RANGE_IDS);
        self.read(|c| {
            let mut stmt = c.prepare_cached(
                "SELECT lamport, id FROM events WHERE space = ?1 AND stream = ?2 AND lamport >= ?3 AND lamport < ?4 \
                 ORDER BY lamport, id LIMIT ?5",
            )?;
            let rows = stmt.query_map(
                params![space.as_bytes(), &sk, to_sql_u64(range.start), to_sql_u64(range.end), limit],
                |r| Ok((from_sql_u64(r.get(0)?), EventId(r.get(1)?))),
            )?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    /// 구간마다 (개수, 이벤트 ID 32바이트의 XOR). 동기화 계층의 지문 정의와 같습니다.
    pub fn fingerprints(
        &self,
        space: &SpaceId,
        stream: StreamId,
        ranges: &[LamportRange],
    ) -> Result<Vec<RangeFingerprint>> {
        if ranges.len() > 4096 {
            return Err(StoreError::LimitExceeded("구간 수"));
        }
        let sk = stream_key(&stream);
        self.read(|c| {
            let mut stmt = c.prepare_cached(
                "SELECT id FROM events WHERE space = ?1 AND stream = ?2 AND lamport >= ?3 AND lamport < ?4",
            )?;
            let mut out = Vec::with_capacity(ranges.len());
            for range in ranges {
                let mut fp = RangeFingerprint { range: *range, count: 0, xor: [0; 32] };
                if range.start < range.end {
                    let mut rows = stmt.query(params![space.as_bytes(), &sk, to_sql_u64(range.start), to_sql_u64(range.end)])?;
                    while let Some(r) = rows.next()? {
                        let id: [u8; 32] = r.get(0)?;
                        for (x, b) in fp.xor.iter_mut().zip(id) {
                            *x ^= b;
                        }
                        fp.count = fp.count.saturating_add(1);
                    }
                }
                out.push(fp);
            }
            Ok(out)
        })
    }

    /// 받지 않은 기록: 저장된 이벤트가 가리키지만 아직 없는 부모.
    pub fn missing_parents(&self, space: &SpaceId, stream: StreamId) -> Result<Vec<EventId>> {
        Ok(self
            .missing_details(space, stream)?
            .into_iter()
            .map(|m| m.id)
            .collect())
    }

    /// 받지 않은 부모와 그것을 가리킨 자식(목록 화면의 빈 구간 표시용). 자식 논리 시계 순.
    pub fn missing_details(&self, space: &SpaceId, stream: StreamId) -> Result<Vec<MissingParent>> {
        let sk = stream_key(&stream);
        self.read(|c| {
            let mut stmt = c.prepare_cached(
                "SELECT event_id, child, child_lamport, since FROM missing WHERE space = ?1 AND stream = ?2 \
                 ORDER BY child_lamport, event_id",
            )?;
            let rows = stmt.query_map(params![space.as_bytes(), &sk], |r| {
                Ok(MissingParent {
                    id: EventId(r.get(0)?),
                    child: EventId(r.get(1)?),
                    child_lamport: from_sql_u64(r.get(2)?),
                    since: crate::db::ts_from_sql(r.get(3)?),
                })
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    /// 받지 않은 기록에서 지웁니다(체크포인트 경계 아래라 앞으로도 받지 않을 부모). 지운 수.
    pub fn forget_missing(
        &self,
        space: &SpaceId,
        stream: StreamId,
        ids: &[EventId],
    ) -> Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        let sk = stream_key(&stream);
        self.write(|tx| {
            let mut stmt = tx.prepare_cached(
                "DELETE FROM missing WHERE space = ?1 AND stream = ?2 AND event_id = ?3",
            )?;
            let mut n = 0;
            for id in ids {
                n += stmt.execute(params![space.as_bytes(), &sk, id.as_bytes()])?;
            }
            Ok(n)
        })
    }

    /// [`Store::events_by_lane`]의 architecture.md 이름(레인으로 고름).
    pub fn events_by_kind(&self, space: &SpaceId, lanes: &[Lane]) -> Result<Vec<SignedEvent>> {
        self.events_by_lane(space, lanes)
    }

    /// 공간의 이벤트 중 주어진 레인에 속한 것을 (논리 시계, ID) 순으로(상태 해소 입력).
    pub fn events_by_lane(&self, space: &SpaceId, lanes: &[Lane]) -> Result<Vec<SignedEvent>> {
        if lanes.is_empty() {
            return Ok(Vec::new());
        }
        let mut codes: Vec<i64> = lanes.iter().map(|l| lane_code(*l)).collect();
        codes.sort_unstable();
        codes.dedup();
        let sql = format!(
            "{EVENT_SELECT} WHERE e.space = ?1 AND e.lane IN ({}) ORDER BY e.lamport, e.id",
            placeholders(2, codes.len())
        );
        self.query_events(&sql, space, &codes)
    }

    /// [`Store::events_by_lane`]에서 논리 시계가 `from` 이상인 것만(체크포인트 경계 위 꼬리). 차가운 시작이 경계 아래
    /// 이벤트를 읽고 풀지 않게 합니다.
    pub fn events_by_lane_from(
        &self,
        space: &SpaceId,
        lanes: &[Lane],
        from: u64,
    ) -> Result<Vec<SignedEvent>> {
        if from == 0 {
            return self.events_by_lane(space, lanes);
        }
        if lanes.is_empty() {
            return Ok(Vec::new());
        }
        let mut codes: Vec<i64> = lanes.iter().map(|l| lane_code(*l)).collect();
        codes.sort_unstable();
        codes.dedup();
        let sql = format!(
            "{EVENT_SELECT} WHERE e.space = ?1 AND e.lamport >= ?2 AND e.lane IN ({}) ORDER BY e.lamport, e.id",
            placeholders(3, codes.len())
        );
        let from = to_sql_u64(from);
        let rows = self.read(|c| {
            let mut stmt = c.prepare_cached(&sql)?;
            let mut values: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(codes.len() + 2);
            values.push(space.as_bytes());
            values.push(&from);
            for code in &codes {
                values.push(code);
            }
            let rows = stmt.query_map(values.as_slice(), read_event_row)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })?;
        rows.into_iter().map(decode_event).collect()
    }

    /// 공간의 이벤트 중 주어진 종류를 (논리 시계, ID) 순으로.
    pub fn events_by_kinds(
        &self,
        space: &SpaceId,
        kinds: &[EventKind],
    ) -> Result<Vec<SignedEvent>> {
        if kinds.is_empty() {
            return Ok(Vec::new());
        }
        if kinds.len() > 64 {
            return Err(StoreError::LimitExceeded("종류 수"));
        }
        let mut codes: Vec<i64> = kinds.iter().map(|k| i64::from(k.code())).collect();
        codes.sort_unstable();
        codes.dedup();
        let sql = format!(
            "{EVENT_SELECT} WHERE e.space = ?1 AND e.kind IN ({}) ORDER BY e.lamport, e.id",
            placeholders(2, codes.len())
        );
        self.query_events(&sql, space, &codes)
    }

    fn query_events(&self, sql: &str, space: &SpaceId, codes: &[i64]) -> Result<Vec<SignedEvent>> {
        let rows = self.read(|c| {
            let mut stmt = c.prepare_cached(sql)?;
            let mut values: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(codes.len() + 1);
            values.push(space.as_bytes());
            for code in codes {
                values.push(code);
            }
            let rows = stmt.query_map(values.as_slice(), read_event_row)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })?;
        rows.into_iter().map(decode_event).collect()
    }

    /// 스트림의 이벤트를 구간 [start, end) 안에서 (논리 시계, ID) 순으로 최대 `limit`개
    /// (`limit`은 [`MAX_RANGE_EVENTS`]로 제한).
    pub fn stream_events(
        &self,
        space: &SpaceId,
        stream: StreamId,
        range: LamportRange,
        limit: u32,
    ) -> Result<Vec<SignedEvent>> {
        let sk = stream_key(&stream);
        let limit = limit.min(MAX_RANGE_EVENTS);
        let rows = self.read(|c| {
            let mut stmt = c.prepare_cached(&format!(
                "{EVENT_SELECT} WHERE e.space = ?1 AND e.stream = ?2 AND e.lamport >= ?3 AND e.lamport < ?4 \
                 ORDER BY e.lamport, e.id LIMIT ?5"
            ))?;
            let rows = stmt.query_map(
                params![space.as_bytes(), &sk, to_sql_u64(range.start), to_sql_u64(range.end), limit],
                read_event_row,
            )?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })?;
        rows.into_iter().map(decode_event).collect()
    }

    /// 공간에 저장된 이벤트 수.
    pub fn event_count(&self, space: &SpaceId) -> Result<u64> {
        self.read(|c| {
            let n: i64 = c
                .prepare_cached("SELECT COUNT(*) FROM events WHERE space = ?1")?
                .query_row([space.as_bytes()], |r| r.get(0))?;
            Ok(from_sql_u64(n))
        })
    }

    /// 암호학적 삭제: 메시지 저장 키와 봉인 본문, 메시지 보기·반응, 같은 내용을 담은 수정 이벤트 본문까지 지웁니다.
    /// 헤더와 서명은 DAG 유지를 위해 남고, 같은 본문을 다시 받아도 저장하지 않습니다.
    /// 이벤트가 있었으면 true.
    pub fn erase_body(&self, id: &EventId) -> Result<bool> {
        Ok(self.erase_bodies(std::slice::from_ref(id))? > 0)
    }

    /// 여러 이벤트를 한 트랜잭션으로 암호학적 삭제합니다. 실제로 있던 이벤트 수를 돌려줍니다.
    pub fn erase_bodies(&self, ids: &[EventId]) -> Result<usize> {
        check_ids(ids)?;
        let n = self.write(|tx| {
            let mut n = 0;
            for id in ids {
                if messages::erase_message_tx(tx, id, false)? {
                    n += 1;
                }
            }
            Ok(n)
        })?;
        // 지운 페이지가 WAL에 남는 시간을 줄입니다(실패해도 삭제 자체는 끝남).
        self.checkpoint_soon();
        Ok(n)
    }

    /// 일반 등급 채널의 채팅 수명 만료·퇴장 시 삭제: 그 채널에서 이 기기가 `received_before` **이전에 받은**
    /// (`None`이면 전부) 메시지 레인 이벤트(메시지·수정·반응·삭제 요청 등)의 본문과 메시지 보기를 암호학적으로
    /// 지웁니다. 헤더는 DAG 유지를 위해 남습니다. 한 번에 최대 `limit`개([`MAX_EXPIRE_BATCH`] 이하)를 지우고
    /// 지운 이벤트 ID를 돌려주므로(`message.expired` 알림용) 빈 목록이 나올 때까지 부르면 됩니다.
    pub fn expire_channel_messages(
        &self,
        space: &SpaceId,
        channel: &ChannelId,
        received_before: Option<Timestamp>,
        limit: u32,
    ) -> Result<Vec<EventId>> {
        self.expire_channel_where(space, channel, "e.received_at", received_before, limit)
    }

    /// [`Self::expire_channel_messages`]와 같되 **작성 시각**(작성자가 적은 `created_at`, 받은 시각보다 늦으면 받은
    /// 시각)이 `created_before` 이전인 것을 지웁니다. 보관 전용 노드가 늦게 받은 메시지의 수명을 늘리지 않게 씁니다.
    pub fn expire_channel_messages_authored(
        &self,
        space: &SpaceId,
        channel: &ChannelId,
        created_before: Timestamp,
        limit: u32,
    ) -> Result<Vec<EventId>> {
        self.expire_channel_where(
            space,
            channel,
            "MIN(e.created_at, e.received_at)",
            Some(created_before),
            limit,
        )
    }

    fn expire_channel_where(
        &self,
        space: &SpaceId,
        channel: &ChannelId,
        time_expr: &str,
        before: Option<Timestamp>,
        limit: u32,
    ) -> Result<Vec<EventId>> {
        let limit = limit.clamp(1, MAX_EXPIRE_BATCH);
        let cutoff = before.map_or(i64::MAX, ts_to_sql);
        let sql = format!(
            "SELECT e.id FROM events e WHERE e.space = ?1 AND e.channel = ?2 AND e.lane = 1 \
             AND {time_expr} < ?3 \
             AND (e.body_state = ?4 OR EXISTS (SELECT 1 FROM messages m WHERE m.event_id = e.id)) \
             ORDER BY e.received_at, e.id LIMIT ?5"
        );
        let ids = self.write(|tx| {
            let ids: Vec<EventId> = {
                let mut stmt = tx.prepare_cached(&sql)?;
                let rows = stmt.query_map(
                    params![
                        space.as_bytes(),
                        channel.as_bytes(),
                        cutoff,
                        BODY_PRESENT,
                        limit
                    ],
                    |r| Ok(EventId(r.get(0)?)),
                )?;
                rows.collect::<rusqlite::Result<_>>()?
            };
            for id in &ids {
                messages::erase_message_tx(tx, id, false)?;
            }
            Ok(ids)
        })?;
        if !ids.is_empty() {
            self.checkpoint_soon();
        }
        Ok(ids)
    }

    /// 한 스트림의 이벤트를 헤더까지 모두 지웁니다(머리·받지 않은 기록·저장 키·메시지 보기 포함, 삭제 표시는 남김).
    /// 보관 전용 노드가 더 보관하지 않게 된 채널을 지울 때 씁니다(다시 보관하면 처음부터 다시 받음). 지운 이벤트 수.
    pub fn purge_stream(&self, space: &SpaceId, stream: StreamId) -> Result<u64> {
        let sk = stream_key(&stream);
        let n = self.write(|tx| {
            let sp = space.as_bytes();
            tx.execute(
                "DELETE FROM message_edits WHERE edit_id IN (SELECT id FROM events WHERE space = ?1 AND stream = ?2) \
                 OR target IN (SELECT id FROM events WHERE space = ?1 AND stream = ?2)",
                params![sp, &sk],
            )?;
            tx.execute(
                "DELETE FROM pending_edits WHERE edit_id IN (SELECT id FROM events WHERE space = ?1 AND stream = ?2) \
                 OR target IN (SELECT id FROM events WHERE space = ?1 AND stream = ?2)",
                params![sp, &sk],
            )?;
            tx.execute(
                "DELETE FROM reactions WHERE target IN (SELECT id FROM events WHERE space = ?1 AND stream = ?2)",
                params![sp, &sk],
            )?;
            // msg_keys·messages는 외래 키(ON DELETE CASCADE)로 함께 지워집니다.
            let n = tx.execute(
                "DELETE FROM events WHERE space = ?1 AND stream = ?2",
                params![sp, &sk],
            )?;
            for table in ["heads", "missing"] {
                tx.execute(
                    &format!("DELETE FROM {table} WHERE space = ?1 AND stream = ?2"),
                    params![sp, &sk],
                )?;
            }
            Ok(n as u64)
        })?;
        if n > 0 {
            self.checkpoint_soon();
        }
        Ok(n)
    }

    /// 공간에 저장된 이벤트가 차지하는 바이트(헤더·서명·봉인 본문 길이의 합). 보관 전용 노드의 용량 계산용.
    pub fn space_event_bytes(&self, space: &SpaceId) -> Result<u64> {
        self.read(|c| {
            let n: i64 = c
                .prepare_cached(
                    "SELECT COALESCE(SUM(LENGTH(header) + LENGTH(sig) + COALESCE(LENGTH(body_sealed), 0)), 0) \
                     FROM events WHERE space = ?1",
                )?
                .query_row([space.as_bytes()], |r| r.get(0))?;
            Ok(from_sql_u64(n))
        })
    }

    /// 시더 제공에서 이 메시지를 빼거나 다시 넣습니다(`seed.exclude_message`).
    pub fn set_seed_excluded(&self, id: &EventId, excluded: bool) -> Result<bool> {
        self.write(|tx| {
            Ok(tx
                .prepare_cached("UPDATE events SET seed_excluded = ?2 WHERE id = ?1")?
                .execute(params![id.as_bytes(), excluded])?
                > 0)
        })
    }

    /// [`Self::set_seed_excluded`]를 한 트랜잭션으로 여러 이벤트에. 바뀐 이벤트 수.
    pub fn set_seed_excluded_many(&self, ids: &[EventId], excluded: bool) -> Result<usize> {
        check_ids(ids)?;
        self.write(|tx| {
            let mut stmt =
                tx.prepare_cached("UPDATE events SET seed_excluded = ?2 WHERE id = ?1")?;
            let mut n = 0;
            for id in ids {
                n += stmt.execute(params![id.as_bytes(), excluded])?;
            }
            Ok(n)
        })
    }

    pub fn is_seed_excluded(&self, id: &EventId) -> Result<bool> {
        self.read(|c| {
            let v: Option<bool> = c
                .prepare_cached("SELECT seed_excluded FROM events WHERE id = ?1")?
                .query_row([id.as_bytes()], |r| r.get(0))
                .optional()?;
            Ok(v.unwrap_or(false))
        })
    }
}
