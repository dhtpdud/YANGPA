//! 친구·친구 링크·친구 요청·프로필·기기 목록·친구 메모.

use rusqlite::{OptionalExtension, params};
use yangpa_core::device::{SignedDeviceList, SignedProfile};
use yangpa_core::ids::{SpaceId, UserId};
use yangpa_core::time::Timestamp;
pub use yangpa_core::wire::ContactKind;

use crate::db::{Store, from_sql_u64, to_sql_u64, ts_from_sql, ts_to_sql};
use crate::error::{Result, StoreError};

const MAX_NOTE_LEN: usize = 64 * 1024;
const MAX_PAYLOAD_LEN: usize = 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FriendRecord {
    pub user: UserId,
    /// 두 사람의 1대1 공간(수락 후 생김).
    pub space: Option<SpaceId>,
    pub added_at: Timestamp,
    /// 안전 번호 확인 여부.
    pub verified: bool,
    pub kind: ContactKind,
    /// 임시 연락처가 만료되는 시각(쓴 연락처 링크의 만료, 없으면 만료 없음).
    pub expires_at: Option<Timestamp>,
    /// 임시 연락처가 쓴(링크 주인 쪽이면 받은) 연락처 링크 토큰의 해시. 링크를 폐기하면 함께 만료합니다.
    pub link_hash: Option<[u8; 32]>,
}

impl FriendRecord {
    /// 친구 기록(연락처 링크로 생긴 것이 아님).
    pub fn friend(user: UserId, space: Option<SpaceId>) -> Self {
        FriendRecord {
            user,
            space,
            added_at: Timestamp::now(),
            verified: false,
            kind: ContactKind::Friend,
            expires_at: None,
            link_hash: None,
        }
    }

    pub fn is_friend(&self) -> bool {
        self.kind == ContactKind::Friend
    }

    /// 만료되지 않은 연락처(친구·임시 연락처).
    pub fn is_active(&self) -> bool {
        self.kind != ContactKind::Expired
    }
}

/// 내가 만든 친구 링크(토큰 원문 대신 해시만 저장).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FriendLinkRecord {
    pub token_hash: [u8; 32],
    pub created_at: Timestamp,
    pub expires_at: Option<Timestamp>,
    pub single_use: bool,
    pub uses: u32,
    pub revoked: bool,
}

/// 링크 사용 시도 결과.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RedeemOutcome {
    /// 사용 가능(사용 횟수를 올림).
    Accepted,
    Unknown,
    Expired,
    Revoked,
    /// 1회용 링크를 이미 씀.
    AlreadyUsed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RequestDirection {
    Incoming,
    Outgoing,
}

impl RequestDirection {
    fn code(self) -> i64 {
        match self {
            RequestDirection::Incoming => 0,
            RequestDirection::Outgoing => 1,
        }
    }

    fn from_code(c: i64) -> Result<Self> {
        match c {
            0 => Ok(RequestDirection::Incoming),
            1 => Ok(RequestDirection::Outgoing),
            _ => Err(StoreError::Corrupt("친구 요청 방향")),
        }
    }
}

/// 친구 요청. 받은 요청은 상대가 보낸 `FriendRedeem`을, 보낸 요청은 수락 확인용 정보를 `payload`에 둡니다
/// (형식은 데몬이 정함). 같은 방향·같은 사용자 요청은 하나만 남습니다(새 것이 덮어씀).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FriendRequestRecord {
    pub request_id: [u8; 16],
    pub direction: RequestDirection,
    pub user: UserId,
    pub profile: Option<SignedProfile>,
    pub token_hash: [u8; 32],
    pub payload: Vec<u8>,
    pub received_at: Timestamp,
}

type RequestRow = (
    [u8; 16],
    i64,
    [u8; 32],
    Option<Vec<u8>>,
    [u8; 32],
    Vec<u8>,
    i64,
);

fn decode_request(row: RequestRow) -> Result<FriendRequestRecord> {
    let (request_id, direction, user, profile, token_hash, payload, received_at) = row;
    let profile = profile
        .map(|p| {
            postcard::from_bytes::<SignedProfile>(&p)
                .map_err(|_| StoreError::Corrupt("친구 요청 프로필"))
        })
        .transpose()?;
    Ok(FriendRequestRecord {
        request_id,
        direction: RequestDirection::from_code(direction)?,
        user: UserId(user),
        profile,
        token_hash,
        payload,
        received_at: ts_from_sql(received_at),
    })
}

const REQUEST_SELECT: &str = "SELECT request_id, direction, user, profile, token_hash, payload, received_at FROM friend_requests";

fn read_request(r: &rusqlite::Row<'_>) -> rusqlite::Result<RequestRow> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
        r.get(6)?,
    ))
}

fn read_link(r: &rusqlite::Row<'_>) -> rusqlite::Result<FriendLinkRecord> {
    Ok(FriendLinkRecord {
        token_hash: r.get(0)?,
        created_at: ts_from_sql(r.get(1)?),
        expires_at: r.get::<_, Option<i64>>(2)?.map(ts_from_sql),
        single_use: r.get(3)?,
        uses: u32::try_from(r.get::<_, i64>(4)?).unwrap_or(u32::MAX),
        revoked: r.get(5)?,
    })
}

const FRIEND_COLS: &str = "user, space, added_at, verified, kind, expires_at, link_hash";

fn read_friend(r: &rusqlite::Row<'_>) -> rusqlite::Result<FriendRecord> {
    Ok(FriendRecord {
        user: UserId(r.get(0)?),
        space: r.get::<_, Option<[u8; 16]>>(1)?.map(SpaceId),
        added_at: ts_from_sql(r.get(2)?),
        verified: r.get(3)?,
        kind: ContactKind::from_code(r.get(4)?),
        expires_at: r.get::<_, Option<i64>>(5)?.map(ts_from_sql),
        link_hash: r.get(6)?,
    })
}

impl Store {
    // ---- 친구 ----

    /// 친구·연락처 기록을 넣거나 바꿉니다(처음 넣은 시각은 그대로).
    pub fn put_friend(&self, f: &FriendRecord) -> Result<()> {
        self.write(|tx| {
            tx.prepare_cached(
                "INSERT INTO friends (user, space, added_at, verified, kind, expires_at, link_hash) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
                 ON CONFLICT (user) DO UPDATE SET space = excluded.space, verified = excluded.verified, \
                 kind = excluded.kind, expires_at = excluded.expires_at, link_hash = excluded.link_hash",
            )?
            .execute(params![
                f.user.as_bytes(),
                f.space.as_ref().map(|s| s.as_bytes()),
                ts_to_sql(f.added_at),
                f.verified,
                f.kind.code(),
                f.expires_at.map(ts_to_sql),
                f.link_hash.as_ref().map(|h| &h[..]),
            ])?;
            Ok(())
        })
    }

    pub fn get_friend(&self, user: &UserId) -> Result<Option<FriendRecord>> {
        self.read(|c| {
            Ok(c.prepare_cached(&format!(
                "SELECT {FRIEND_COLS} FROM friends WHERE user = ?1"
            ))?
            .query_row([user.as_bytes()], read_friend)
            .optional()?)
        })
    }

    pub fn friend_by_space(&self, space: &SpaceId) -> Result<Option<FriendRecord>> {
        self.read(|c| {
            Ok(c.prepare_cached(&format!(
                "SELECT {FRIEND_COLS} FROM friends WHERE space = ?1"
            ))?
            .query_row([space.as_bytes()], read_friend)
            .optional()?)
        })
    }

    pub fn list_friends(&self) -> Result<Vec<FriendRecord>> {
        self.read(|c| {
            let mut stmt = c.prepare_cached(&format!(
                "SELECT {FRIEND_COLS} FROM friends ORDER BY added_at, user"
            ))?;
            let rows = stmt.query_map([], read_friend)?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })
    }

    /// 연락처 종류·만료를 바꿉니다. 친구가 되면 링크 흔적(`expires_at`·`link_hash`)을 지웁니다. 기록이 있었으면 true.
    pub fn set_contact_kind(
        &self,
        user: &UserId,
        kind: ContactKind,
        expires_at: Option<Timestamp>,
        link_hash: Option<[u8; 32]>,
    ) -> Result<bool> {
        self.write(|tx| {
            Ok(tx
                .prepare_cached(
                    "UPDATE friends SET kind = ?2, expires_at = ?3, link_hash = ?4 WHERE user = ?1",
                )?
                .execute(params![
                    user.as_bytes(),
                    kind.code(),
                    expires_at.map(ts_to_sql),
                    link_hash.as_ref().map(|h| &h[..]),
                ])?
                > 0)
        })
    }

    /// 만료 시각이 지난 임시 연락처를 만료된 연락처로 바꾸고 그 사용자들을 돌려줍니다.
    pub fn expire_due_contacts(&self, now: Timestamp) -> Result<Vec<UserId>> {
        self.expire_contacts_where(
            "expires_at IS NOT NULL AND expires_at <= ?1",
            ts_to_sql(now),
        )
    }

    /// 이 연락처 링크로 생긴 임시 연락처를 만료된 연락처로 바꾸고 그 사용자들을 돌려줍니다(링크 폐기).
    pub fn expire_link_contacts(&self, link_hash: &[u8; 32]) -> Result<Vec<UserId>> {
        self.expire_contacts_where("link_hash = ?1", &link_hash[..])
    }

    fn expire_contacts_where(&self, cond: &str, arg: impl rusqlite::ToSql) -> Result<Vec<UserId>> {
        let temp = ContactKind::Temporary.code();
        let expired = ContactKind::Expired.code();
        self.write(|tx| {
            let users: Vec<UserId> = {
                let mut stmt = tx.prepare_cached(&format!(
                    "SELECT user FROM friends WHERE kind = {temp} AND {cond}"
                ))?;
                let rows = stmt.query_map([&arg], |r| r.get::<_, [u8; 32]>(0))?;
                rows.map(|r| r.map(UserId))
                    .collect::<rusqlite::Result<_>>()?
            };
            for u in &users {
                tx.prepare_cached(&format!(
                    "UPDATE friends SET kind = {expired} WHERE user = ?1"
                ))?
                .execute([u.as_bytes()])?;
            }
            Ok(users)
        })
    }

    /// 임시 연락처 수(링크로 아무나 들어와 쌓이지 않게 상한을 둠).
    pub fn count_temporary_contacts(&self) -> Result<u64> {
        let temp = ContactKind::Temporary.code();
        self.read(|c| {
            let n: i64 = c.query_row(
                &format!("SELECT COUNT(*) FROM friends WHERE kind = {temp}"),
                [],
                |r| r.get(0),
            )?;
            Ok(n.max(0) as u64)
        })
    }

    pub fn set_friend_verified(&self, user: &UserId, verified: bool) -> Result<bool> {
        self.write(|tx| {
            Ok(tx
                .prepare_cached("UPDATE friends SET verified = ?2 WHERE user = ?1")?
                .execute(params![user.as_bytes(), verified])?
                > 0)
        })
    }

    pub fn remove_friend(&self, user: &UserId) -> Result<bool> {
        self.write(|tx| {
            Ok(tx
                .prepare_cached("DELETE FROM friends WHERE user = ?1")?
                .execute([user.as_bytes()])?
                > 0)
        })
    }

    // ---- 친구 링크 ----

    pub fn create_friend_link(&self, l: &FriendLinkRecord) -> Result<()> {
        self.write(|tx| {
            tx.prepare_cached(
                "INSERT INTO friend_links (token_hash, created_at, expires_at, single_use, uses, revoked) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?
            .execute(params![
                &l.token_hash,
                ts_to_sql(l.created_at),
                l.expires_at.map(ts_to_sql),
                l.single_use,
                l.uses,
                l.revoked
            ])?;
            Ok(())
        })
    }

    pub fn get_friend_link(&self, token_hash: &[u8; 32]) -> Result<Option<FriendLinkRecord>> {
        self.read(|c| {
            Ok(c.prepare_cached(
                "SELECT token_hash, created_at, expires_at, single_use, uses, revoked FROM friend_links WHERE token_hash = ?1",
            )?
            .query_row([token_hash], read_link)
            .optional()?)
        })
    }

    pub fn list_friend_links(&self) -> Result<Vec<FriendLinkRecord>> {
        self.read(|c| {
            let mut stmt = c.prepare_cached(
                "SELECT token_hash, created_at, expires_at, single_use, uses, revoked FROM friend_links \
                 ORDER BY created_at DESC, token_hash",
            )?;
            let rows = stmt.query_map([], read_link)?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })
    }

    pub fn revoke_friend_link(&self, token_hash: &[u8; 32]) -> Result<bool> {
        self.write(|tx| {
            Ok(tx
                .prepare_cached("UPDATE friend_links SET revoked = 1 WHERE token_hash = ?1")?
                .execute([token_hash])?
                > 0)
        })
    }

    /// 링크 사용을 검사하고, 쓸 수 있으면 사용 횟수를 올립니다(한 트랜잭션이라 1회용 링크가 두 번 쓰이지 않음).
    pub fn redeem_friend_link(
        &self,
        token_hash: &[u8; 32],
        now: Timestamp,
    ) -> Result<RedeemOutcome> {
        self.write(|tx| {
            let link = tx
                .prepare_cached(
                    "SELECT token_hash, created_at, expires_at, single_use, uses, revoked FROM friend_links \
                     WHERE token_hash = ?1",
                )?
                .query_row([token_hash], read_link)
                .optional()?;
            let Some(link) = link else { return Ok(RedeemOutcome::Unknown) };
            if link.revoked {
                return Ok(RedeemOutcome::Revoked);
            }
            if link.expires_at.is_some_and(|e| now >= e) {
                return Ok(RedeemOutcome::Expired);
            }
            if link.single_use && link.uses > 0 {
                return Ok(RedeemOutcome::AlreadyUsed);
            }
            tx.prepare_cached("UPDATE friend_links SET uses = uses + 1 WHERE token_hash = ?1")?.execute([token_hash])?;
            Ok(RedeemOutcome::Accepted)
        })
    }

    /// 만료되었거나 폐기된 링크를 지웁니다.
    pub fn prune_friend_links(&self, now: Timestamp) -> Result<usize> {
        self.write(|tx| {
            Ok(tx
                .prepare_cached(
                    "DELETE FROM friend_links WHERE revoked = 1 OR (expires_at IS NOT NULL AND expires_at <= ?1) \
                     OR (single_use = 1 AND uses > 0)",
                )?
                .execute([ts_to_sql(now)])?)
        })
    }

    // ---- 친구 요청 ----

    pub fn put_friend_request(&self, r: &FriendRequestRecord) -> Result<()> {
        if r.payload.len() > MAX_PAYLOAD_LEN {
            return Err(StoreError::LimitExceeded("친구 요청 크기"));
        }
        let profile = r.profile.as_ref().map(postcard::to_stdvec).transpose()?;
        self.write(|tx| {
            tx.prepare_cached("DELETE FROM friend_requests WHERE direction = ?1 AND user = ?2")?
                .execute(params![r.direction.code(), r.user.as_bytes()])?;
            tx.prepare_cached(
                "INSERT OR REPLACE INTO friend_requests (request_id, direction, user, profile, token_hash, payload, received_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?
            .execute(params![
                &r.request_id,
                r.direction.code(),
                r.user.as_bytes(),
                profile,
                &r.token_hash,
                r.payload,
                ts_to_sql(r.received_at)
            ])?;
            Ok(())
        })
    }

    pub fn get_friend_request(&self, request_id: &[u8; 16]) -> Result<Option<FriendRequestRecord>> {
        let row = self.read(|c| {
            Ok(
                c.prepare_cached(&format!("{REQUEST_SELECT} WHERE request_id = ?1"))?
                    .query_row([request_id], read_request)
                    .optional()?,
            )
        })?;
        row.map(decode_request).transpose()
    }

    /// 보낸 요청 중 이 링크 토큰으로 보낸 것(수락 알림의 토큰 대조용).
    pub fn outgoing_request_by_token(
        &self,
        token_hash: &[u8; 32],
    ) -> Result<Option<FriendRequestRecord>> {
        let row = self.read(|c| {
            Ok(c.prepare_cached(&format!(
                "{REQUEST_SELECT} WHERE token_hash = ?1 AND direction = 1"
            ))?
            .query_row([token_hash], read_request)
            .optional()?)
        })?;
        row.map(decode_request).transpose()
    }

    pub fn list_friend_requests(
        &self,
        direction: RequestDirection,
    ) -> Result<Vec<FriendRequestRecord>> {
        let rows = self.read(|c| {
            let mut stmt = c.prepare_cached(&format!(
                "{REQUEST_SELECT} WHERE direction = ?1 ORDER BY received_at, request_id"
            ))?;
            let rows = stmt.query_map([direction.code()], read_request)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })?;
        rows.into_iter().map(decode_request).collect()
    }

    pub fn delete_friend_request(&self, request_id: &[u8; 16]) -> Result<bool> {
        self.write(|tx| {
            Ok(tx
                .prepare_cached("DELETE FROM friend_requests WHERE request_id = ?1")?
                .execute([request_id])?
                > 0)
        })
    }

    // ---- 프로필·기기 목록 ----

    /// 서명된 프로필. 버전이 더 높을 때만 바꿉니다(서명 검증은 호출자 몫). 바꿨으면 true.
    pub fn put_profile(&self, p: &SignedProfile) -> Result<bool> {
        let bytes = postcard::to_stdvec(p)?;
        self.write(|tx| {
            Ok(tx
                .prepare_cached(
                    "INSERT INTO profiles (user, version, profile, updated_at) VALUES (?1, ?2, ?3, ?4) \
                     ON CONFLICT (user) DO UPDATE SET version = excluded.version, profile = excluded.profile, \
                       updated_at = excluded.updated_at WHERE excluded.version > profiles.version",
                )?
                .execute(params![p.user.as_bytes(), to_sql_u64(p.profile.version), bytes, ts_to_sql(Timestamp::now())])?
                > 0)
        })
    }

    pub fn delete_profile(&self, user: &UserId) -> Result<()> {
        self.write(|tx| {
            tx.prepare_cached("DELETE FROM profiles WHERE user = ?1")?
                .execute([user.as_bytes()])?;
            Ok(())
        })
    }

    pub fn get_profile(&self, user: &UserId) -> Result<Option<SignedProfile>> {
        let bytes: Option<Vec<u8>> = self.read(|c| {
            Ok(
                c.prepare_cached("SELECT profile FROM profiles WHERE user = ?1")?
                    .query_row([user.as_bytes()], |r| r.get(0))
                    .optional()?,
            )
        })?;
        bytes
            .map(|b| postcard::from_bytes(&b).map_err(|_| StoreError::Corrupt("프로필")))
            .transpose()
    }

    /// 서명된 기기 목록. 버전이 더 높을 때만 바꿉니다. 바꿨으면 true.
    pub fn put_device_list(&self, l: &SignedDeviceList) -> Result<bool> {
        let bytes = postcard::to_stdvec(l)?;
        self.write(|tx| {
            Ok(tx
                .prepare_cached(
                    "INSERT INTO device_lists (user, version, list, updated_at) VALUES (?1, ?2, ?3, ?4) \
                     ON CONFLICT (user) DO UPDATE SET version = excluded.version, list = excluded.list, \
                       updated_at = excluded.updated_at WHERE excluded.version > device_lists.version",
                )?
                .execute(params![l.list.user.as_bytes(), to_sql_u64(l.list.version), bytes, ts_to_sql(Timestamp::now())])?
                > 0)
        })
    }

    pub fn delete_device_list(&self, user: &UserId) -> Result<()> {
        self.write(|tx| {
            tx.prepare_cached("DELETE FROM device_lists WHERE user = ?1")?
                .execute([user.as_bytes()])?;
            Ok(())
        })
    }

    pub fn get_device_list(&self, user: &UserId) -> Result<Option<SignedDeviceList>> {
        let bytes: Option<Vec<u8>> = self.read(|c| {
            Ok(
                c.prepare_cached("SELECT list FROM device_lists WHERE user = ?1")?
                    .query_row([user.as_bytes()], |r| r.get(0))
                    .optional()?,
            )
        })?;
        bytes
            .map(|b| postcard::from_bytes(&b).map_err(|_| StoreError::Corrupt("기기 목록")))
            .transpose()
    }

    pub fn device_list_version(&self, user: &UserId) -> Result<Option<u64>> {
        self.read(|c| {
            let v: Option<i64> = c
                .prepare_cached("SELECT version FROM device_lists WHERE user = ?1")?
                .query_row([user.as_bytes()], |r| r.get(0))
                .optional()?;
            Ok(v.map(from_sql_u64))
        })
    }

    // ---- 친구 메모(본인 기기에만) ----

    /// 메모를 저장합니다. 빈 문자열이면 지웁니다.
    pub fn set_note(&self, user: &UserId, text: &str) -> Result<()> {
        if text.len() > MAX_NOTE_LEN {
            return Err(StoreError::LimitExceeded("메모 길이"));
        }
        self.write(|tx| {
            if text.is_empty() {
                tx.prepare_cached("DELETE FROM notes WHERE user = ?1")?
                    .execute([user.as_bytes()])?;
            } else {
                tx.prepare_cached(
                    "INSERT OR REPLACE INTO notes (user, text, updated_at) VALUES (?1, ?2, ?3)",
                )?
                .execute(params![
                    user.as_bytes(),
                    text,
                    ts_to_sql(Timestamp::now())
                ])?;
            }
            Ok(())
        })
    }

    pub fn get_note(&self, user: &UserId) -> Result<Option<String>> {
        self.read(|c| {
            Ok(c.prepare_cached("SELECT text FROM notes WHERE user = ?1")?
                .query_row([user.as_bytes()], |r| r.get(0))
                .optional()?)
        })
    }
}
