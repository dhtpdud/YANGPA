//! 공간 상태를 행으로 나눠 저장하기(저장 형식 5판)와 기기 키 표.
//!
//! 3만 명 크루의 상태를 통째로 다시 쓰면 상태가 한 번 바뀔 때마다 수 MB를 씁니다. 그래서 멤버 수만큼 커지는 표
//! (멤버·기기 기록·밴)와 채널은 열쇠마다 한 행으로, 나머지(머리: 설정·역할·초대 등)는 한 행으로 두고 바뀐 행만
//! 씁니다([`StateDelta`]). 바뀐 열쇠는 지속 맵의 차이(`imbl` `diff`)로 찾아, 나눠 쓰는 마디는 건너뜁니다.
//!
//! 기기 키(상자 키·서명 키)는 상태에 두지 않고 키 약속(`key_commit`)으로 찾는 [`DeviceKeys`] 표에 둡니다.

use std::collections::{HashMap, HashSet};

use rusqlite::{OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use yangpa_core::device::{BoxPublicKey, DeviceKeys, DeviceSignKeys, MemberDevices};
use yangpa_core::ids::{ChannelId, DeviceId, SpaceId, UserId};
use yangpa_core::model::{BanInfo, ChannelSettings, DiffItem, Member, SharedMap, SpaceState};
use yangpa_core::time::Timestamp;

use crate::db::{Store, placeholders, ts_to_sql};
use crate::error::{Result, StoreError};
use crate::events::{InsertOutcome, PersistentEvent};

/// 한 사용자의 행: 멤버(떠났으면 없음)와 기기 기록(다시 들어올 때 옛 판을 거르려고 떠난 뒤에도 남을 수 있음).
type MemberRow = (UserId, Option<Vec<u8>>, Option<Vec<u8>>);

/// 상태 두 판 사이에 바뀐 행. 값은 postcard 바이트(로컬 API JSON과 무관한 저장 형식).
#[derive(Debug, Default)]
pub struct StateDelta {
    /// 큰 표를 뺀 나머지(바뀌었을 때만).
    head: Option<Vec<u8>>,
    /// 멤버·기기 기록 행. 둘 다 `None`이면 행을 지웁니다.
    members: Vec<MemberRow>,
    bans: Vec<(UserId, Option<Vec<u8>>)>,
    channels: Vec<(ChannelId, Option<Vec<u8>>)>,
    /// 새로 휘발성으로 보인 채널(`volatile_channels`에 넣음, 지우지 않음).
    volatile: Vec<ChannelId>,
}

/// 지속 맵 두 판에서 바뀐(더함·고침·뺌) 열쇠. 나눠 쓰는 마디는 건너뛰므로 바뀐 만큼만 돕니다.
fn changed_keys<K: Ord + Copy, V: PartialEq>(
    old: Option<&SharedMap<K, V>>,
    new: &SharedMap<K, V>,
) -> Vec<K> {
    match old {
        None => new.keys().copied().collect(),
        Some(o) => o
            .diff(new)
            .map(|d| match d {
                DiffItem::Add(k, _) | DiffItem::Remove(k, _) => *k,
                DiffItem::Update { new: (k, _), .. } => *k,
            })
            .collect(),
    }
}

fn encode_rows<K: Ord + Copy, V: PartialEq + Serialize>(
    old: Option<&SharedMap<K, V>>,
    new: &SharedMap<K, V>,
) -> Result<Vec<(K, Option<Vec<u8>>)>> {
    changed_keys(old, new)
        .into_iter()
        .map(|k| Ok((k, new.get(&k).map(postcard::to_stdvec).transpose()?)))
        .collect()
}

/// 큰 표를 비운 상태(머리). 새 필드는 자동으로 머리에 들어가므로 필드를 더해도 저장에서 빠지지 않습니다.
fn head_of(s: &SpaceState) -> SpaceState {
    SpaceState {
        members: SharedMap::new(),
        devices: SharedMap::new(),
        bans: SharedMap::new(),
        channels: SharedMap::new(),
        ..s.clone()
    }
}

impl StateDelta {
    /// `old`(마지막으로 저장한 판, 없으면 처음)에서 `new`로 가는 행 변화.
    pub fn between(old: Option<&SpaceState>, new: &SpaceState) -> Result<StateDelta> {
        let head = head_of(new);
        let head = match old {
            Some(o) if head_of(o) == head => None,
            _ => Some(postcard::to_stdvec(&head)?),
        };
        let mut users = changed_keys(old.map(|o| &o.members), &new.members);
        users.extend(changed_keys(old.map(|o| &o.devices), &new.devices));
        users.sort_unstable();
        users.dedup();
        let members = users
            .into_iter()
            .map(|u| {
                let m = new.members.get(&u).map(|m| postcard::to_stdvec(&**m));
                let d = new.devices.get(&u).map(|d| postcard::to_stdvec(&**d));
                Ok((u, m.transpose()?, d.transpose()?))
            })
            .collect::<Result<Vec<_>>>()?;
        let bans = encode_rows(old.map(|o| &o.bans), &new.bans)?;
        let channels = encode_rows(old.map(|o| &o.channels), &new.channels)?;
        let volatile = channels
            .iter()
            .filter(|(c, _)| {
                new.channels
                    .get(c)
                    .is_some_and(ChannelSettings::is_volatile)
            })
            .map(|(c, _)| *c)
            .collect();
        Ok(StateDelta {
            head,
            members,
            bans,
            channels,
            volatile,
        })
    }

    /// 쓸 행이 없는지.
    pub fn is_empty(&self) -> bool {
        self.head.is_none()
            && self.members.is_empty()
            && self.bans.is_empty()
            && self.channels.is_empty()
    }

    /// 쓰거나 지울 행 수(휘발성 표시 포함).
    pub fn rows(&self) -> usize {
        usize::from(self.head.is_some())
            + self.members.len()
            + self.bans.len()
            + self.channels.len()
            + self.volatile.len()
    }

    /// 쓰는 값 바이트(열쇠 제외). 측정용.
    pub fn value_bytes(&self) -> usize {
        let opt = |v: &Option<Vec<u8>>| v.as_ref().map_or(0, Vec::len);
        opt(&self.head)
            + self
                .members
                .iter()
                .map(|(_, m, d)| opt(m) + opt(d))
                .sum::<usize>()
            + self.bans.iter().map(|(_, v)| opt(v)).sum::<usize>()
            + self.channels.iter().map(|(_, v)| opt(v)).sum::<usize>()
    }
}

/// 트랜잭션 안에서 행 변화를 씁니다(공간이 없으면 만듦).
pub(crate) fn write_state_delta(
    tx: &Transaction<'_>,
    space: &SpaceId,
    delta: &StateDelta,
) -> Result<()> {
    let sp = space.as_bytes();
    let now = ts_to_sql(Timestamp::now());
    // 멤버·채널 행만 바뀌면 `spaces` 행은 건드리지 않습니다. 머리(1KB 남짓)가 WITHOUT ROWID 행의 넘침 페이지까지
    // 차지해, `updated_at` 하나만 고쳐도 페이지 두어 개를 WAL에 다시 씁니다.
    tx.prepare_cached(
        "INSERT OR IGNORE INTO spaces (space, joined_at, updated_at) VALUES (?1, ?2, ?2)",
    )?
    .execute(params![sp, now])?;
    if let Some(h) = &delta.head {
        tx.prepare_cached("UPDATE spaces SET head = ?2, updated_at = ?3 WHERE space = ?1")?
            .execute(params![sp, h, now])?;
    }
    {
        let mut put = tx.prepare_cached(
            "INSERT OR REPLACE INTO space_members (space, user, member, devices) VALUES (?1, ?2, ?3, ?4)",
        )?;
        let mut del =
            tx.prepare_cached("DELETE FROM space_members WHERE space = ?1 AND user = ?2")?;
        for (u, m, d) in &delta.members {
            if m.is_none() && d.is_none() {
                del.execute(params![sp, u.as_bytes()])?;
            } else {
                put.execute(params![sp, u.as_bytes(), m, d])?;
            }
        }
    }
    {
        let mut put = tx.prepare_cached(
            "INSERT OR REPLACE INTO space_bans (space, user, ban) VALUES (?1, ?2, ?3)",
        )?;
        let mut del = tx.prepare_cached("DELETE FROM space_bans WHERE space = ?1 AND user = ?2")?;
        for (u, v) in &delta.bans {
            match v {
                Some(v) => put.execute(params![sp, u.as_bytes(), v])?,
                None => del.execute(params![sp, u.as_bytes()])?,
            };
        }
    }
    {
        let mut put = tx.prepare_cached(
            "INSERT OR REPLACE INTO space_channels (space, channel, settings) VALUES (?1, ?2, ?3)",
        )?;
        let mut del =
            tx.prepare_cached("DELETE FROM space_channels WHERE space = ?1 AND channel = ?2")?;
        for (c, v) in &delta.channels {
            match v {
                Some(v) => put.execute(params![sp, c.as_bytes(), v])?,
                None => del.execute(params![sp, c.as_bytes()])?,
            };
        }
    }
    if !delta.volatile.is_empty() {
        let mut stmt = tx.prepare_cached(
            "INSERT OR IGNORE INTO volatile_channels (space, channel) VALUES (?1, ?2)",
        )?;
        for c in &delta.volatile {
            stmt.execute(params![sp, c.as_bytes()])?;
        }
    }
    Ok(())
}

fn decode<T: for<'de> Deserialize<'de>>(b: &[u8]) -> Result<T> {
    postcard::from_bytes(b).map_err(|_| StoreError::Corrupt("공간 상태 행"))
}

/// `device_keys` 한 행(키 약속, 기기, 상자 키, Ed25519 공개키, FN-DSA 공개키).
type KeyRow = (Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>);

fn put_device_keys_tx(tx: &Transaction<'_>, space: &SpaceId, keys: &[DeviceKeys]) -> Result<usize> {
    let mut stmt = tx.prepare_cached(
        "INSERT OR IGNORE INTO device_keys (key_commit, space, device, box_key, ed_pk, pq_pk) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )?;
    let mut n = 0;
    for k in keys.iter().filter(|k| k.keys_match()) {
        n += stmt.execute(params![
            k.key_commit(),
            space.as_bytes(),
            k.device.as_bytes(),
            k.box_key.as_bytes(),
            k.sign_keys.ed_pk,
            &*k.sign_keys.pq_pk,
        ])?;
    }
    Ok(n)
}

/// 한 번에 찾는 키 약속 수(SQLite 자리표시자 한도 아래).
const KEY_LOOKUP_CHUNK: usize = 500;

impl Store {
    /// 상태 행 변화를 한 트랜잭션으로 씁니다.
    pub fn apply_state_delta(&self, space: &SpaceId, delta: &StateDelta) -> Result<()> {
        self.write(|tx| write_state_delta(tx, space, delta))
    }

    /// 저장한 상태를 그대로 다시 짓습니다(아직 해소한 적 없으면 `None`).
    pub fn load_space_state(&self, space: &SpaceId) -> Result<Option<SpaceState>> {
        self.read(|c| {
            let sp = space.as_bytes();
            let head: Option<Option<Vec<u8>>> = c
                .prepare_cached("SELECT head FROM spaces WHERE space = ?1")?
                .query_row([sp], |r| r.get(0))
                .optional()?;
            let Some(Some(head)) = head else {
                return Ok(None);
            };
            let mut st: SpaceState = decode(&head)?;
            let mut members = SharedMap::new();
            let mut devices = SharedMap::new();
            {
                let mut stmt = c.prepare_cached(
                    "SELECT user, member, devices FROM space_members WHERE space = ?1 ORDER BY user",
                )?;
                let mut rows = stmt.query([sp])?;
                while let Some(r) = rows.next()? {
                    let u = UserId(r.get(0)?);
                    if let Some(m) = r.get::<_, Option<Vec<u8>>>(1)? {
                        members.insert(u, std::sync::Arc::new(decode::<Member>(&m)?));
                    }
                    if let Some(d) = r.get::<_, Option<Vec<u8>>>(2)? {
                        devices.insert(u, std::sync::Arc::new(decode::<MemberDevices>(&d)?));
                    }
                }
            }
            let mut bans = SharedMap::new();
            {
                let mut stmt =
                    c.prepare_cached("SELECT user, ban FROM space_bans WHERE space = ?1")?;
                let mut rows = stmt.query([sp])?;
                while let Some(r) = rows.next()? {
                    let b: Vec<u8> = r.get(1)?;
                    bans.insert(UserId(r.get(0)?), decode::<BanInfo>(&b)?);
                }
            }
            let mut channels = SharedMap::new();
            {
                let mut stmt = c.prepare_cached(
                    "SELECT channel, settings FROM space_channels WHERE space = ?1",
                )?;
                let mut rows = stmt.query([sp])?;
                while let Some(r) = rows.next()? {
                    let s: Vec<u8> = r.get(1)?;
                    channels.insert(ChannelId(r.get(0)?), decode::<ChannelSettings>(&s)?);
                }
            }
            st.members = members;
            st.devices = devices;
            st.bans = bans;
            st.channels = channels;
            Ok(Some(st))
        })
    }

    /// 기기 키를 넣습니다(같은 약속·공간 행이 있으면 그대로). 약속은 여기서 다시 계산하고, 기기 ID가 서명 키와 맞지
    /// 않는 키는 넣지 않습니다. 공간을 지우면 그 공간이 넣은 행도 지웁니다. 넣은 행 수를 돌려줍니다.
    pub fn put_device_keys(&self, space: &SpaceId, keys: &[DeviceKeys]) -> Result<usize> {
        if keys.is_empty() {
            return Ok(0);
        }
        self.write(|tx| put_device_keys_tx(tx, space, keys))
    }

    /// 이벤트와 그 본문에 실린 기기 키를 한 트랜잭션으로 넣습니다(받기 경로). 키 행은 공간마다 따로 두므로, 같은 기기가
    /// 있는 다른 공간을 지워도 이 공간의 행은 남습니다.
    pub fn insert_events_with_device_keys(
        &self,
        evs: &[PersistentEvent],
        space: &SpaceId,
        keys: &[DeviceKeys],
    ) -> Result<Vec<InsertOutcome>> {
        if keys.is_empty() {
            return self.insert_events(evs);
        }
        self.insert_events_then(evs, |tx| put_device_keys_tx(tx, space, keys).map(drop))
    }

    /// 이 공간이 넣은 키 행의 약속(시작 때 다시 적용하며 이미 있는 행을 건너뛰려고). 기본 키 색인만 읽습니다.
    pub fn device_key_commits(&self, space: &SpaceId) -> Result<HashSet<[u8; 32]>> {
        self.read(|c| {
            let mut stmt =
                c.prepare_cached("SELECT key_commit FROM device_keys WHERE space = ?1")?;
            let rows = stmt.query_map([space.as_bytes()], |r| r.get::<_, [u8; 32]>(0))?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })
    }

    /// 키 약속으로 기기 키를 찾습니다(없는 약속은 빠짐). 행에서 약속을 다시 계산해 맞는 것만 돌려줍니다(손상·바꿔치기
    /// 된 행으로 엉뚱한 키에 봉인하지 않게).
    pub fn device_keys(&self, commits: &[[u8; 32]]) -> Result<HashMap<[u8; 32], DeviceKeys>> {
        let mut out = HashMap::with_capacity(commits.len());
        for chunk in commits.chunks(KEY_LOOKUP_CHUNK) {
            let rows: Vec<KeyRow> = self.read(|c| {
                let sql = format!(
                    "SELECT key_commit, device, box_key, ed_pk, pq_pk FROM device_keys \
                     WHERE key_commit IN ({}) GROUP BY key_commit",
                    placeholders(1, chunk.len())
                );
                let mut stmt = c.prepare(&sql)?;
                let rows = stmt.query_map(
                    rusqlite::params_from_iter(chunk.iter().map(|k| k.as_slice())),
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
                )?;
                Ok(rows.collect::<rusqlite::Result<_>>()?)
            })?;
            for (commit, device, box_key, ed_pk, pq_pk) in rows {
                let (Ok(commit), Ok(device), Ok(ed_pk)) = (
                    <[u8; 32]>::try_from(commit.as_slice()),
                    <[u8; 32]>::try_from(device.as_slice()),
                    <[u8; 32]>::try_from(ed_pk.as_slice()),
                ) else {
                    return Err(StoreError::Corrupt("기기 키"));
                };
                let keys = DeviceKeys {
                    device: DeviceId(device),
                    box_key: BoxPublicKey::from_bytes(&box_key)
                        .map_err(|_| StoreError::Corrupt("기기 키"))?,
                    sign_keys: DeviceSignKeys {
                        ed_pk,
                        pq_pk: pq_pk.into(),
                    },
                };
                if keys.key_commit() == commit {
                    out.insert(commit, keys);
                }
            }
        }
        Ok(out)
    }
}
