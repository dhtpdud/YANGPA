//! 복호화한 메시지 보기(목록 조회용). 내용은 그 메시지의 저장 키로 봉인해 두어, 저장 키를 폐기하면
//! 이벤트 본문과 보기가 함께 복구 불가능해집니다.

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use yangpa_core::body::{LinkEmbed, MessageBody, ReactionEmoji, Secret32};
use yangpa_core::event::{EventHeader, EventKind, Lane};
use yangpa_core::ids::{ChannelId, DeviceId, EventId, SpaceId, UserId};
use yangpa_core::time::Timestamp;

use crate::db::{Store, from_sql_u64, lane_code, to_sql_u64, ts_from_sql, ts_to_sql};
use crate::error::{Result, StoreError};
use crate::events::{BODY_ERASED, erase_event_body, is_tombstoned};

/// 한 번에 돌려주는 최대 메시지 수.
pub const MAX_PAGE: u32 = 500;
/// 반응 이모지 표기 최대 길이.
const MAX_EMOJI_LEN: usize = 128;

const VIEW_AD: &[u8] = b"yangpa/v1/store-view\0";
const PENDING_EDIT_AD: &[u8] = b"yangpa/v1/store-pending-edit\0";

fn view_ad(id: &EventId) -> Vec<u8> {
    let mut ad = VIEW_AD.to_vec();
    ad.extend_from_slice(id.as_bytes());
    ad
}

fn pending_edit_ad(id: &EventId) -> Vec<u8> {
    let mut ad = PENDING_EDIT_AD.to_vec();
    ad.extend_from_slice(id.as_bytes());
    ad
}

/// 보기 행 종류.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    Message,
    ForumPost,
}

impl MessageKind {
    fn code(self) -> i64 {
        match self {
            MessageKind::Message => 1,
            MessageKind::ForumPost => 2,
        }
    }

    fn from_code(c: i64) -> Result<Self> {
        match c {
            1 => Ok(MessageKind::Message),
            2 => Ok(MessageKind::ForumPost),
            _ => Err(StoreError::Corrupt("메시지 종류")),
        }
    }

    /// 이 보기를 만들 수 있는 이벤트 종류.
    fn event_kind(self) -> EventKind {
        match self {
            MessageKind::Message => EventKind::Message,
            MessageKind::ForumPost => EventKind::ForumPost,
        }
    }
}

/// 포럼 게시글의 제목·태그(봉인되는 내용). `Debug`는 제목을 찍지 않습니다(로그에 내용 금지).
#[derive(Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ForumMeta {
    pub title: String,
    #[serde(default)]
    pub tags: Vec<u32>,
}

impl std::fmt::Debug for ForumMeta {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ForumMeta")
            .field("title_len", &self.title.len())
            .field("tags", &self.tags)
            .finish()
    }
}

/// 봉인해 저장하는 복호화된 내용. `Debug`는 본문을 찍지 않습니다(로그에 내용 금지).
#[derive(Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct MessageContent {
    pub body: MessageBody,
    #[serde(default)]
    pub forum: Option<ForumMeta>,
}

impl std::fmt::Debug for MessageContent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MessageContent")
            .field("text_len", &self.body.text.len())
            .field("attachments", &self.body.attachments.len())
            .field("embeds", &self.body.embeds.len())
            .field("reply_to", &self.body.reply_to)
            .field("thread", &self.body.thread)
            .field("forum", &self.forum)
            .finish()
    }
}

/// 반응 집계 한 줄.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReactionSummary {
    pub emoji: ReactionEmoji,
    pub count: u32,
    /// 조회한 사용자(`viewer`)가 누른 반응인지.
    pub me: bool,
}

/// 메시지 보기 행.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MessageRecord {
    pub id: EventId,
    pub space: SpaceId,
    pub channel: ChannelId,
    /// 포럼 게시글 안 댓글이면 게시글 ID. 채널 목록에는 `None`인 것만 나옵니다.
    pub thread: Option<EventId>,
    pub lamport: u64,
    pub author: UserId,
    pub author_device: DeviceId,
    pub created_at: Timestamp,
    pub kind: MessageKind,
    /// 삭제된 메시지는 `None`.
    pub content: Option<MessageContent>,
    pub edited_at: Option<Timestamp>,
    pub deleted: bool,
    /// 이 기기 사용자를 언급했는지(안 읽은 언급 수 계산용).
    pub mentions_me: bool,
    /// 포럼 게시글 고정·잠금.
    pub pinned: bool,
    pub locked: bool,
    /// 포럼 게시글의 마지막 활동(댓글 포함). 일반 메시지는 작성 시각.
    pub last_activity: Timestamp,
    /// 포럼 게시글의 댓글 수(삭제 제외).
    pub reply_count: u32,
    /// 익명 채널에서 쓴 글(헤더 종류 `AnonMessage`·`AnonForumPost`). 저장된 이벤트 헤더가 기준이라 따로 저장하지 않습니다.
    pub anonymous: bool,
    /// 읽을 때만 채워집니다(쓸 때는 무시).
    pub reactions: Vec<ReactionSummary>,
}

impl MessageRecord {
    /// 이벤트 헤더와 복호화한 내용으로 새 보기 행을 만듭니다.
    pub fn from_header(
        id: EventId,
        header: &EventHeader,
        kind: MessageKind,
        content: MessageContent,
    ) -> Result<Self> {
        let channel = header
            .channel
            .ok_or(StoreError::InvalidInput("채널 없는 메시지"))?;
        Ok(MessageRecord {
            id,
            space: header.space,
            channel,
            thread: content.body.thread,
            lamport: header.lamport,
            author: header.user,
            author_device: header.author,
            created_at: header.created_at,
            kind,
            content: Some(content),
            edited_at: None,
            deleted: false,
            mentions_me: false,
            pinned: false,
            locked: false,
            last_activity: header.created_at,
            reply_count: 0,
            anonymous: header.kind.is_anonymous(),
            reactions: Vec::new(),
        })
    }
}

/// 메시지 목록 조회 조건.
#[derive(Clone, Debug)]
pub struct MessageQuery {
    pub space: SpaceId,
    pub channel: ChannelId,
    /// `Some`이면 그 게시글의 댓글, `None`이면 채널 본 목록.
    pub thread: Option<EventId>,
    /// 이 위치(논리 시계, ID)보다 오래된 것부터(키셋 페이지).
    pub before: Option<(u64, EventId)>,
    pub limit: u32,
    /// 반응의 `me` 계산용.
    pub viewer: Option<UserId>,
}

/// 수정 적용 요청. 수정 이벤트는 먼저 저장돼 있어야 하고, 작성자가 원본 작성자와 같아야 적용됩니다.
#[derive(Clone, PartialEq, Eq)]
pub struct EditUpdate {
    /// 수정 이벤트 ID.
    pub event: EventId,
    pub lamport: u64,
    pub at: Timestamp,
    pub text: String,
    pub embeds: Vec<LinkEmbed>,
}

impl std::fmt::Debug for EditUpdate {
    // 수정 내용은 찍지 않습니다.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EditUpdate")
            .field("event", &self.event)
            .field("lamport", &self.lamport)
            .field("at", &self.at)
            .field("text_len", &self.text.len())
            .field("embeds", &self.embeds.len())
            .finish()
    }
}

/// 반응 추가·제거. 반응 이벤트(`event`)는 먼저 저장돼 있어야 합니다(디스크 등급 확인).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReactionUpdate {
    pub space: SpaceId,
    pub target: EventId,
    pub user: UserId,
    pub emoji: ReactionEmoji,
    /// true = 추가, false = 제거.
    pub present: bool,
    pub event: EventId,
    pub lamport: u64,
}

/// 포럼 게시글 변경.
#[derive(Clone, PartialEq, Eq)]
pub struct ForumUpdate {
    pub event: EventId,
    pub lamport: u64,
    pub title: Option<String>,
    pub tags: Option<Vec<u32>>,
    pub pinned: Option<bool>,
    pub locked: Option<bool>,
    /// 제목·태그를 바꾼 시각(게시글의 `edited_at`을 이보다 앞이면 올림). 고정·잠금만 바꾸면 `None`.
    pub edited_at: Option<Timestamp>,
}

impl std::fmt::Debug for ForumUpdate {
    // 제목은 찍지 않습니다.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ForumUpdate")
            .field("event", &self.event)
            .field("lamport", &self.lamport)
            .field("title", &self.title.as_ref().map(|t| t.len()))
            .field("tags", &self.tags)
            .field("pinned", &self.pinned)
            .field("locked", &self.locked)
            .field("edited_at", &self.edited_at)
            .finish()
    }
}

/// 포럼 게시글 정렬.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ForumSort {
    /// 마지막 활동 순.
    Activity,
    /// 작성 순.
    Created,
}

/// 채널별 안 읽은 메시지·언급 수.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChannelUnread {
    pub channel: ChannelId,
    pub unread: u32,
    pub mentions: u32,
}

fn emoji_key(e: &ReactionEmoji) -> Result<Vec<u8>> {
    match e {
        ReactionEmoji::Unicode { value } => {
            if value.is_empty() || value.len() > MAX_EMOJI_LEN {
                return Err(StoreError::InvalidInput("반응 이모지 길이"));
            }
            let mut k = vec![b'u'];
            k.extend_from_slice(value.as_bytes());
            Ok(k)
        }
        ReactionEmoji::Custom { id, name, .. } => {
            if name.len() > MAX_EMOJI_LEN {
                return Err(StoreError::InvalidInput("반응 이모지 길이"));
            }
            let mut k = vec![b'c'];
            k.extend_from_slice(&id.0.to_be_bytes());
            Ok(k)
        }
    }
}

fn msg_key(c: &Connection, id: &EventId) -> Result<Option<Secret32>> {
    let k: Option<[u8; 32]> = c
        .prepare_cached("SELECT key FROM msg_keys WHERE event_id = ?1")?
        .query_row([id.as_bytes()], |r| r.get(0))
        .optional()?;
    Ok(k.map(Secret32))
}

fn seal_content(key: &Secret32, id: &EventId, content: &MessageContent) -> Result<Vec<u8>> {
    let plain = zeroize::Zeroizing::new(postcard::to_stdvec(content)?);
    Ok(yangpa_crypto::seal(key, &view_ad(id), &plain))
}

pub(crate) fn open_content(key: &Secret32, id: &EventId, sealed: &[u8]) -> Result<MessageContent> {
    let plain = yangpa_crypto::open(key, &view_ad(id), sealed)
        .map_err(|_| StoreError::Corrupt("메시지 보기"))?;
    postcard::from_bytes(&plain).map_err(|_| StoreError::Corrupt("메시지 보기"))
}

/// 게시글의 댓글 수·마지막 활동을 다시 셉니다(댓글이 게시글보다 먼저 와도 맞게).
fn refresh_thread_stats(tx: &Transaction<'_>, post: &EventId) -> Result<()> {
    tx.prepare_cached(
        "UPDATE messages SET \
           reply_count = (SELECT COUNT(*) FROM messages r WHERE r.space = messages.space \
               AND r.channel = messages.channel AND r.thread = messages.event_id AND r.deleted = 0), \
           last_activity = MAX(created_at, COALESCE((SELECT MAX(r.created_at) FROM messages r \
               WHERE r.space = messages.space AND r.channel = messages.channel \
               AND r.thread = messages.event_id AND r.deleted = 0), 0)) \
         WHERE event_id = ?1",
    )?
    .execute([post.as_bytes()])?;
    Ok(())
}

/// 보기 행을 넣습니다. 이미 있으면 바꾸지 않습니다(수정·삭제·반응은 전용 메서드).
///
/// 작성자·기기·논리 시계·작성 시각·종류는 서명된 헤더가 기준이라, 저장된 이벤트와 다르면 거부합니다
/// (화면에 다른 사람이 쓴 것처럼 보이는 보기 행이 생기지 않게).
pub(crate) fn put_view_tx(tx: &Transaction<'_>, v: &MessageRecord) -> Result<bool> {
    type Row = (
        i64,
        [u8; 16],
        Option<[u8; 16]>,
        [u8; 32],
        [u8; 32],
        i64,
        i64,
        i64,
    );
    let row: Option<Row> = tx
        .prepare_cached(
            "SELECT body_state, space, channel, author_user, author_device, lamport, created_at, kind \
             FROM events WHERE id = ?1",
        )?
        .query_row([v.id.as_bytes()], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?))
        })
        .optional()?;
    let Some((state, space, channel, user, device, lamport, created_at, kind)) = row else {
        return Err(StoreError::NotFound);
    };
    if space != v.space.0 || channel != Some(v.channel.0) {
        return Err(StoreError::InvalidInput(
            "보기 행의 공간·채널이 이벤트와 다름",
        ));
    }
    if user != v.author.0
        || device != v.author_device.0
        || lamport != to_sql_u64(v.lamport)
        || created_at != ts_to_sql(v.created_at)
    {
        return Err(StoreError::InvalidInput(
            "보기 행의 작성자·논리 시계·시각이 이벤트 헤더와 다름",
        ));
    }
    let event_kind = u16::try_from(kind)
        .map(EventKind::from_code)
        .map_err(|_| StoreError::Corrupt("이벤트 종류"))?;
    if event_kind.base() != v.kind.event_kind() || event_kind.is_anonymous() != v.anonymous {
        return Err(StoreError::InvalidInput(
            "보기 행 종류가 이벤트 종류와 다름",
        ));
    }
    let tombstoned = is_tombstoned(tx, &v.id)?;
    if state == BODY_ERASED && !tombstoned {
        // 수명 만료 등으로 이미 폐기한 메시지는 되살리지 않습니다.
        return Ok(false);
    }
    let exists: bool = tx
        .prepare_cached("SELECT EXISTS(SELECT 1 FROM messages WHERE event_id = ?1)")?
        .query_row([v.id.as_bytes()], |r| r.get(0))?;
    if exists {
        return Ok(false);
    }
    let deleted = v.deleted || tombstoned;
    let sealed = if deleted {
        None
    } else {
        let content = v
            .content
            .as_ref()
            .ok_or(StoreError::InvalidInput("메시지 내용 없음"))?;
        let key = msg_key(tx, &v.id)?.ok_or(StoreError::InvalidInput("본문을 받지 않은 이벤트"))?;
        Some(seal_content(&key, &v.id, content)?)
    };
    tx.prepare_cached(
        "INSERT INTO messages (event_id, space, channel, thread, lamport, author_user, author_device, created_at, \
         kind, content_sealed, edited_at, deleted, mention, pinned, locked, last_activity, reply_count, anonymous) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, 0, ?17)",
    )?
    .execute(params![
        v.id.as_bytes(),
        v.space.as_bytes(),
        v.channel.as_bytes(),
        v.thread.as_ref().map(|t| t.as_bytes()),
        to_sql_u64(v.lamport),
        v.author.as_bytes(),
        v.author_device.as_bytes(),
        ts_to_sql(v.created_at),
        v.kind.code(),
        sealed,
        v.edited_at.map(ts_to_sql),
        deleted,
        v.mentions_me && !deleted,
        v.pinned,
        v.locked,
        ts_to_sql(v.created_at.max(v.last_activity)),
        v.anonymous,
    ])?;
    if !deleted {
        apply_pending_edits(tx, &v.id)?;
    }
    if let Some(post) = &v.thread {
        refresh_thread_stats(tx, post)?;
    }
    if v.kind == MessageKind::ForumPost {
        refresh_thread_stats(tx, &v.id)?;
    }
    Ok(true)
}

/// 저장된 이벤트의 (공간, 작성 사용자).
fn event_owner(c: &Connection, id: &EventId) -> Result<Option<([u8; 16], [u8; 32])>> {
    Ok(
        c.prepare_cached("SELECT space, author_user FROM events WHERE id = ?1")?
            .query_row([id.as_bytes()], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?,
    )
}

/// 원본보다 먼저 온 수정 중 가장 새 것을 적용합니다. 원본이 없을 때는 작성자를 확인할 수 없었으므로
/// 여기서 원본 작성자와 같은 사람이 쓴 수정만 적용합니다(남의 메시지를 미리 고쳐 두는 것 방지).
fn apply_pending_edits(tx: &Transaction<'_>, target: &EventId) -> Result<()> {
    let pending: Vec<([u8; 32], i64, i64, Vec<u8>)> = {
        let mut stmt = tx.prepare_cached(
            "SELECT edit_id, lamport, at, sealed FROM pending_edits WHERE target = ?1 ORDER BY lamport DESC, edit_id DESC",
        )?;
        let rows = stmt.query_map([target.as_bytes()], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    tx.prepare_cached("DELETE FROM pending_edits WHERE target = ?1")?
        .execute([target.as_bytes()])?;
    let Some(target_owner) = event_owner(tx, target)? else {
        return Ok(());
    };
    for (edit_id, lamport, at, sealed) in pending {
        let edit_id = EventId(edit_id);
        if event_owner(tx, &edit_id)? != Some(target_owner) {
            // 다른 사람(또는 다른 공간)의 수정은 적용하지 않습니다.
            continue;
        }
        // 수정 이벤트 본문이 이미 폐기됐으면 그 수정도 읽을 수 없으니 다음 것을 봅니다.
        let Some(key) = msg_key(tx, &edit_id)? else {
            continue;
        };
        let Ok(plain) = yangpa_crypto::open(&key, &pending_edit_ad(&edit_id), &sealed) else {
            continue;
        };
        let Ok((text, embeds)) = postcard::from_bytes::<(String, Vec<LinkEmbed>)>(&plain) else {
            continue;
        };
        let edit = EditUpdate {
            event: edit_id,
            lamport: from_sql_u64(lamport),
            at: ts_from_sql(at),
            text,
            embeds,
        };
        apply_edit_tx(tx, target, &edit)?;
        break;
    }
    Ok(())
}

fn apply_edit_tx(tx: &Transaction<'_>, target: &EventId, e: &EditUpdate) -> Result<bool> {
    // 수정 이벤트가 디스크 등급으로 저장돼 있어야 작성자를 확인할 수 있습니다.
    let edit_owner = event_owner(tx, &e.event)?.ok_or(StoreError::NotFound)?;
    if let Some(target_owner) = event_owner(tx, target)? {
        if target_owner.0 != edit_owner.0 {
            return Err(StoreError::InvalidInput("수정과 원본의 공간이 다름"));
        }
        if target_owner.1 != edit_owner.1 {
            return Err(StoreError::InvalidInput(
                "다른 사람의 메시지는 수정할 수 없음",
            ));
        }
    }
    tx.prepare_cached("INSERT OR IGNORE INTO message_edits (target, edit_id) VALUES (?1, ?2)")?
        .execute(params![target.as_bytes(), e.event.as_bytes()])?;
    if is_tombstoned(tx, target)? {
        // 삭제된 메시지의 수정은 같은 내용을 담으므로 바로 폐기합니다.
        erase_event_body(tx, &e.event)?;
        return Ok(false);
    }
    type Row = (
        Option<Vec<u8>>,
        Option<i64>,
        Option<[u8; 32]>,
        bool,
        Option<[u8; 32]>,
    );
    let row: Option<Row> = tx
        .prepare_cached(
            "SELECT m.content_sealed, m.edit_lamport, m.edit_id, m.deleted, k.key FROM messages m \
             LEFT JOIN msg_keys k ON k.event_id = m.event_id WHERE m.event_id = ?1",
        )?
        .query_row([target.as_bytes()], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })
        .optional()?;
    let Some((sealed, edit_lamport, edit_id, deleted, key)) = row else {
        // 원본이 아직 없음: 수정 이벤트 자신의 저장 키로 봉인해 두었다가 원본이 오면 적용합니다.
        if let Some(edit_key) = msg_key(tx, &e.event)? {
            let plain = zeroize::Zeroizing::new(postcard::to_stdvec(&(&e.text, &e.embeds))?);
            let sealed = yangpa_crypto::seal(&edit_key, &pending_edit_ad(&e.event), &plain);
            tx.prepare_cached(
                "INSERT OR REPLACE INTO pending_edits (target, edit_id, lamport, at, sealed) VALUES (?1, ?2, ?3, ?4, ?5)",
            )?
            .execute(params![target.as_bytes(), e.event.as_bytes(), to_sql_u64(e.lamport), ts_to_sql(e.at), sealed])?;
        }
        return Ok(false);
    };
    if deleted {
        erase_event_body(tx, &e.event)?;
        return Ok(false);
    }
    if let (Some(l), Some(id)) = (edit_lamport, edit_id)
        && (e.lamport, e.event.0) <= (from_sql_u64(l), id)
    {
        return Ok(false);
    }
    let (Some(sealed), Some(key)) = (sealed, key) else {
        return Ok(false);
    };
    let key = Secret32(key);
    let mut content = open_content(&key, target, &sealed)?;
    content.body.text.clone_from(&e.text);
    content.body.embeds.clone_from(&e.embeds);
    let resealed = seal_content(&key, target, &content)?;
    tx.prepare_cached(
        "UPDATE messages SET content_sealed = ?2, edited_at = ?3, edit_lamport = ?4, edit_id = ?5 WHERE event_id = ?1",
    )?
    .execute(params![target.as_bytes(), resealed, ts_to_sql(e.at), to_sql_u64(e.lamport), e.event.as_bytes()])?;
    Ok(true)
}

/// 메시지(또는 임의 이벤트)의 본문을 암호학적으로 지웁니다. `keep_row`면 '삭제된 메시지' 행을 남깁니다.
pub(crate) fn erase_message_tx(tx: &Transaction<'_>, id: &EventId, keep_row: bool) -> Result<bool> {
    let had_event = erase_event_body(tx, id)?;
    let edits: Vec<[u8; 32]> = {
        let mut stmt = tx.prepare_cached("SELECT edit_id FROM message_edits WHERE target = ?1")?;
        let rows = stmt.query_map([id.as_bytes()], |r| r.get(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    for edit in edits {
        erase_event_body(tx, &EventId(edit))?;
    }
    tx.prepare_cached("DELETE FROM message_edits WHERE target = ?1")?
        .execute([id.as_bytes()])?;
    tx.prepare_cached("DELETE FROM pending_edits WHERE target = ?1")?
        .execute([id.as_bytes()])?;
    // 이 이벤트가 수정이었다면 그 저장 키로 봉인해 둔 보관 수정은 이제 읽을 수 없으니 지웁니다.
    tx.prepare_cached("DELETE FROM pending_edits WHERE edit_id = ?1")?
        .execute([id.as_bytes()])?;
    tx.prepare_cached("DELETE FROM reactions WHERE target = ?1")?
        .execute([id.as_bytes()])?;
    let thread: Option<Option<[u8; 32]>> = tx
        .prepare_cached("SELECT thread FROM messages WHERE event_id = ?1")?
        .query_row([id.as_bytes()], |r| r.get(0))
        .optional()?;
    let had_row = if keep_row {
        tx.prepare_cached(
            "UPDATE messages SET deleted = 1, content_sealed = NULL, mention = 0, reply_count = 0 WHERE event_id = ?1",
        )?
        .execute([id.as_bytes()])?
            > 0
    } else {
        tx.prepare_cached("DELETE FROM messages WHERE event_id = ?1")?
            .execute([id.as_bytes()])?
            > 0
    };
    if let Some(Some(post)) = thread {
        refresh_thread_stats(tx, &EventId(post))?;
    }
    Ok(had_event || had_row)
}

const MSG_SELECT: &str = "SELECT m.event_id, m.space, m.channel, m.thread, m.lamport, m.author_user, \
     m.author_device, m.created_at, m.kind, m.content_sealed, m.edited_at, m.deleted, m.mention, m.pinned, \
     m.locked, m.last_activity, m.reply_count, k.key, m.anonymous FROM messages m \
     LEFT JOIN msg_keys k ON k.event_id = m.event_id";

struct RawMessage {
    id: [u8; 32],
    space: [u8; 16],
    channel: [u8; 16],
    thread: Option<[u8; 32]>,
    lamport: i64,
    author: [u8; 32],
    author_device: [u8; 32],
    created_at: i64,
    kind: i64,
    sealed: Option<Vec<u8>>,
    edited_at: Option<i64>,
    deleted: bool,
    mention: bool,
    pinned: bool,
    locked: bool,
    last_activity: i64,
    reply_count: i64,
    key: Option<[u8; 32]>,
    anonymous: bool,
}

fn read_raw(r: &rusqlite::Row<'_>) -> rusqlite::Result<RawMessage> {
    Ok(RawMessage {
        id: r.get(0)?,
        space: r.get(1)?,
        channel: r.get(2)?,
        thread: r.get(3)?,
        lamport: r.get(4)?,
        author: r.get(5)?,
        author_device: r.get(6)?,
        created_at: r.get(7)?,
        kind: r.get(8)?,
        sealed: r.get(9)?,
        edited_at: r.get(10)?,
        deleted: r.get(11)?,
        mention: r.get(12)?,
        pinned: r.get(13)?,
        locked: r.get(14)?,
        last_activity: r.get(15)?,
        reply_count: r.get(16)?,
        key: r.get(17)?,
        anonymous: r.get(18)?,
    })
}

fn reactions_tx(
    c: &Connection,
    target: &EventId,
    viewer: Option<&UserId>,
) -> Result<Vec<ReactionSummary>> {
    let me: &[u8] = viewer.map(|u| u.as_bytes().as_slice()).unwrap_or(&[]);
    let mut stmt = c.prepare_cached(
        "SELECT emoji, COUNT(*), MAX(user = ?2), MIN(lamport) AS first FROM reactions \
         WHERE target = ?1 AND present = 1 GROUP BY emoji_key ORDER BY first, emoji_key",
    )?;
    let rows = stmt.query_map(params![target.as_bytes(), me], |r| {
        Ok((
            r.get::<_, Vec<u8>>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, bool>(2)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (emoji, count, mine) = row?;
        let emoji: ReactionEmoji =
            serde_json::from_slice(&emoji).map_err(|_| StoreError::Corrupt("반응 이모지"))?;
        out.push(ReactionSummary {
            emoji,
            count: u32::try_from(count).unwrap_or(u32::MAX),
            me: mine,
        });
    }
    Ok(out)
}

fn decode_message(
    c: &Connection,
    raw: RawMessage,
    viewer: Option<&UserId>,
) -> Result<MessageRecord> {
    let id = EventId(raw.id);
    let content = match (&raw.sealed, &raw.key) {
        (Some(sealed), Some(key)) if !raw.deleted => {
            match open_content(&Secret32(*key), &id, sealed) {
                Ok(c) => Some(c),
                Err(_) => {
                    // 행 하나가 손상돼도 목록 전체가 실패하지 않게 내용만 비웁니다.
                    tracing::warn!(id = %id.short(), "메시지 보기 복호화 실패");
                    None
                }
            }
        }
        _ => None,
    };
    let reactions = if raw.deleted {
        Vec::new()
    } else {
        reactions_tx(c, &id, viewer)?
    };
    Ok(MessageRecord {
        id,
        space: SpaceId(raw.space),
        channel: ChannelId(raw.channel),
        thread: raw.thread.map(EventId),
        lamport: from_sql_u64(raw.lamport),
        author: UserId(raw.author),
        author_device: DeviceId(raw.author_device),
        created_at: ts_from_sql(raw.created_at),
        kind: MessageKind::from_code(raw.kind)?,
        content,
        edited_at: raw.edited_at.map(ts_from_sql),
        deleted: raw.deleted,
        mentions_me: raw.mention,
        pinned: raw.pinned,
        locked: raw.locked,
        last_activity: ts_from_sql(raw.last_activity),
        reply_count: u32::try_from(raw.reply_count).unwrap_or(u32::MAX),
        anonymous: raw.anonymous,
        reactions,
    })
}

impl Store {
    /// 메시지 보기 행을 넣습니다. 이벤트가 먼저 저장돼 있어야 하고(같은 트랜잭션이면
    /// [`Store::insert_events_with_views`]), 이미 있거나 본문이 폐기된 메시지면 false.
    pub fn put_message_view(&self, v: &MessageRecord) -> Result<bool> {
        self.write(|tx| put_view_tx(tx, v))
    }

    pub fn get_message(
        &self,
        id: &EventId,
        viewer: Option<&UserId>,
    ) -> Result<Option<MessageRecord>> {
        self.read(|c| {
            let raw = c
                .prepare_cached(&format!("{MSG_SELECT} WHERE m.event_id = ?1"))?
                .query_row([id.as_bytes()], read_raw)
                .optional()?;
            raw.map(|r| decode_message(c, r, viewer)).transpose()
        })
    }

    /// 채널 본 목록(댓글 제외)을 최신순((논리 시계, ID) 내림차순)으로. `before`는 키셋 페이지 위치.
    pub fn list_messages(
        &self,
        space: &SpaceId,
        channel: &ChannelId,
        before: Option<(u64, EventId)>,
        limit: u32,
    ) -> Result<Vec<MessageRecord>> {
        self.list_messages_for(&MessageQuery {
            space: *space,
            channel: *channel,
            thread: None,
            before,
            limit,
            viewer: None,
        })
    }

    /// 조건을 다 지정하는 목록 조회(댓글 목록, 반응 `me` 계산 포함).
    pub fn list_messages_for(&self, q: &MessageQuery) -> Result<Vec<MessageRecord>> {
        let limit = q.limit.clamp(1, MAX_PAGE);
        let thread = q.thread.as_ref().map(|t| t.as_bytes());
        self.read(|c| {
            let raws: Vec<RawMessage> = match q.before {
                Some((lamport, id)) => {
                    let mut stmt = c.prepare_cached(&format!(
                        "{MSG_SELECT} WHERE m.space = ?1 AND m.channel = ?2 AND m.thread IS ?3 \
                         AND (m.lamport, m.event_id) < (?4, ?5) ORDER BY m.lamport DESC, m.event_id DESC LIMIT ?6"
                    ))?;
                    let rows = stmt.query_map(
                        params![q.space.as_bytes(), q.channel.as_bytes(), thread, to_sql_u64(lamport), id.as_bytes(), limit],
                        read_raw,
                    )?;
                    rows.collect::<rusqlite::Result<_>>()?
                }
                None => {
                    let mut stmt = c.prepare_cached(&format!(
                        "{MSG_SELECT} WHERE m.space = ?1 AND m.channel = ?2 AND m.thread IS ?3 \
                         ORDER BY m.lamport DESC, m.event_id DESC LIMIT ?4"
                    ))?;
                    let rows =
                        stmt.query_map(params![q.space.as_bytes(), q.channel.as_bytes(), thread, limit], read_raw)?;
                    rows.collect::<rusqlite::Result<_>>()?
                }
            };
            raws.into_iter().map(|r| decode_message(c, r, q.viewer.as_ref())).collect()
        })
    }

    /// 메시지 위치(논리 시계, ID). API의 `before: EventId`를 키셋 위치로 바꿀 때 씁니다.
    pub fn message_position(&self, id: &EventId) -> Result<Option<(u64, EventId)>> {
        self.read(|c| {
            let l: Option<i64> = c
                .prepare_cached("SELECT lamport FROM messages WHERE event_id = ?1")?
                .query_row([id.as_bytes()], |r| r.get(0))
                .optional()?;
            Ok(l.map(|l| (from_sql_u64(l), *id)))
        })
    }

    /// 수정 적용. (논리 시계, ID)가 더 큰 수정이 이깁니다. 원본이 아직 없으면 보관했다가 원본이 오면 적용합니다.
    /// 지금 보기에 반영됐으면 true. 수정 이벤트가 먼저 저장돼 있어야 하고(없으면 [`StoreError::NotFound`]),
    /// 원본과 공간·작성자가 다르면 거부합니다(보관된 수정은 원본이 왔을 때 같은 검사로 걸러냄).
    pub fn apply_edit(&self, target: &EventId, e: &EditUpdate) -> Result<bool> {
        self.write(|tx| apply_edit_tx(tx, target, e))
    }

    /// 메시지 삭제: 삭제 표시를 남기고(원본이 늦게 와도 저장하지 않음), 본문·수정본·반응을 암호학적으로 지우고,
    /// 보기 행은 '삭제된 메시지'로 남깁니다.
    pub fn mark_message_deleted(
        &self,
        space: &SpaceId,
        target: &EventId,
        at: Timestamp,
    ) -> Result<bool> {
        let r = self.write(|tx| {
            // 다른 공간의 메시지를 지우거나, 공간을 떠날 때 남는 표시가 생기지 않게 합니다.
            if let Some((target_space, _)) = event_owner(tx, target)?
                && target_space != space.0
            {
                return Err(StoreError::InvalidInput("삭제 대상이 다른 공간의 이벤트"));
            }
            // 상태·키 합의·키 이벤트의 본문을 지우면 공간을 다시 세울 수 없게 됩니다(메시지 레인만 지움).
            let lane: Option<i64> = tx
                .prepare_cached("SELECT lane FROM events WHERE id = ?1")?
                .query_row([target.as_bytes()], |r| r.get(0))
                .optional()?;
            if lane.is_some_and(|l| l != lane_code(Lane::Message)) {
                return Err(StoreError::InvalidInput("삭제 대상이 메시지가 아님"));
            }
            tx.prepare_cached("INSERT OR IGNORE INTO tombstones (event_id, space, deleted_at) VALUES (?1, ?2, ?3)")?
                .execute(params![target.as_bytes(), space.as_bytes(), ts_to_sql(at)])?;
            erase_message_tx(tx, target, true)
        })?;
        self.checkpoint_soon();
        Ok(r)
    }

    /// 공간에서 그 사용자가 `since`부터 `until`까지(작성 시각 기준, 양 끝 포함) 쓴, 아직 지우지 않은 메시지·게시글.
    /// 밴과 함께 최근 메시지를 지울 때 씁니다(끝을 밴 시각으로 묶어 밴이 풀린 뒤 쓴 메시지는 건드리지 않음).
    pub fn messages_by_author_between(
        &self,
        space: &SpaceId,
        user: &UserId,
        since: Timestamp,
        until: Timestamp,
    ) -> Result<Vec<(EventId, ChannelId)>> {
        self.read(|c| {
            let mut stmt = c.prepare_cached(
                "SELECT event_id, channel FROM messages \
                 WHERE space = ?1 AND author_user = ?2 AND created_at >= ?3 AND created_at <= ?4 \
                   AND deleted = 0 \
                 ORDER BY lamport, event_id",
            )?;
            let rows = stmt.query_map(
                params![
                    space.as_bytes(),
                    user.as_bytes(),
                    ts_to_sql(since),
                    ts_to_sql(until)
                ],
                |r| Ok((EventId(r.get(0)?), ChannelId(r.get(1)?))),
            )?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })
    }

    pub fn is_deleted(&self, id: &EventId) -> Result<bool> {
        self.read(|c| is_tombstoned(c, id))
    }

    /// 반응 추가·제거. (대상, 이모지, 사용자)마다 (논리 시계, 이벤트 ID)가 큰 쪽이 이겨서 도착 순서와 무관하게 수렴합니다.
    /// 상태가 바뀌었으면 true. 반응 이벤트(`r.event`)가 먼저 저장돼 있어야 하고(없으면 [`StoreError::NotFound`]),
    /// 그 공간·작성자가 `r.space`·`r.user`와 같아야 합니다.
    pub fn apply_reaction(&self, r: &ReactionUpdate) -> Result<bool> {
        let key = emoji_key(&r.emoji)?;
        // ReactionEmoji는 내부 태그 enum이라 자기 기술 형식(JSON)으로 저장합니다(postcard로는 못 풂).
        let emoji = serde_json::to_vec(&r.emoji)?;
        self.write(|tx| {
            // 반응 이벤트가 디스크 등급으로 저장돼 있어야 합니다(휘발성 채널의 반응이 이 표로 오지 않게).
            let (event_space, event_user) =
                event_owner(tx, &r.event)?.ok_or(StoreError::NotFound)?;
            if event_space != r.space.0 || event_user != r.user.0 {
                return Err(StoreError::InvalidInput("반응 이벤트의 공간·작성자가 다름"));
            }
            if let Some((target_space, _)) = event_owner(tx, &r.target)?
                && target_space != r.space.0
            {
                return Err(StoreError::InvalidInput("반응 대상이 다른 공간의 이벤트"));
            }
            let erased: bool = tx
                .prepare_cached(
                    "SELECT EXISTS(SELECT 1 FROM tombstones WHERE event_id = ?1) \
                     OR EXISTS(SELECT 1 FROM events WHERE id = ?1 AND body_state = ?2)",
                )?
                .query_row(params![r.target.as_bytes(), BODY_ERASED], |row| row.get(0))?;
            if erased {
                return Ok(false);
            }
            let n = tx
                .prepare_cached(
                    "INSERT INTO reactions (target, emoji_key, user, space, emoji, present, lamport, event_id) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
                     ON CONFLICT (target, emoji_key, user) DO UPDATE SET emoji = excluded.emoji, \
                       present = excluded.present, lamport = excluded.lamport, event_id = excluded.event_id \
                     WHERE (excluded.lamport, excluded.event_id) > (reactions.lamport, reactions.event_id)",
                )?
                .execute(params![
                    r.target.as_bytes(),
                    key,
                    r.user.as_bytes(),
                    r.space.as_bytes(),
                    emoji,
                    r.present,
                    to_sql_u64(r.lamport),
                    r.event.as_bytes()
                ])?;
            Ok(n > 0)
        })
    }

    pub fn reactions(
        &self,
        target: &EventId,
        viewer: Option<&UserId>,
    ) -> Result<Vec<ReactionSummary>> {
        self.read(|c| reactions_tx(c, target, viewer))
    }

    /// 그 이모지로 반응한 사용자.
    pub fn reactors(&self, target: &EventId, emoji: &ReactionEmoji) -> Result<Vec<UserId>> {
        let key = emoji_key(emoji)?;
        self.read(|c| {
            let mut stmt = c.prepare_cached(
                "SELECT user FROM reactions WHERE target = ?1 AND emoji_key = ?2 AND present = 1 ORDER BY lamport",
            )?;
            let rows = stmt.query_map(params![target.as_bytes(), key], |r| Ok(UserId(r.get(0)?)))?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })
    }

    /// 포럼 게시글 변경. 게시글마다 (논리 시계, ID)가 큰 변경이 이깁니다(필드별이 아니라 게시글 단위).
    /// 변경 이벤트(`u.event`)가 먼저 저장돼 있어야 하고(없으면 [`StoreError::NotFound`]), 게시글과 같은 공간이어야
    /// 합니다(다른 공간의 권한으로 이 공간 게시글을 고정·잠금·제목 변경하지 못하게).
    pub fn apply_forum_update(&self, post: &EventId, u: &ForumUpdate) -> Result<bool> {
        self.write(|tx| {
            let (update_space, _) = event_owner(tx, &u.event)?.ok_or(StoreError::NotFound)?;
            if let Some((post_space, _)) = event_owner(tx, post)?
                && post_space != update_space
            {
                return Err(StoreError::InvalidInput("게시글 변경과 게시글의 공간이 다름"));
            }
            type Row = (Option<Vec<u8>>, Option<i64>, Option<[u8; 32]>, bool, Option<[u8; 32]>, i64);
            let row: Option<Row> = tx
                .prepare_cached(
                    "SELECT m.content_sealed, m.meta_lamport, m.meta_id, m.deleted, k.key, m.kind FROM messages m \
                     LEFT JOIN msg_keys k ON k.event_id = m.event_id WHERE m.event_id = ?1",
                )?
                .query_row([post.as_bytes()], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))
                .optional()?;
            let Some((sealed, meta_lamport, meta_id, deleted, key, kind)) = row else { return Ok(false) };
            if deleted || MessageKind::from_code(kind)? != MessageKind::ForumPost {
                return Ok(false);
            }
            if let (Some(l), Some(id)) = (meta_lamport, meta_id)
                && (u.lamport, u.event.0) <= (from_sql_u64(l), id)
            {
                return Ok(false);
            }
            let mut resealed = None;
            if u.title.is_some() || u.tags.is_some() {
                let (Some(sealed), Some(key)) = (sealed, key) else { return Ok(false) };
                let key = Secret32(key);
                let mut content = open_content(&key, post, &sealed)?;
                let meta = content.forum.get_or_insert_with(ForumMeta::default);
                if let Some(t) = &u.title {
                    meta.title.clone_from(t);
                }
                if let Some(t) = &u.tags {
                    meta.tags.clone_from(t);
                }
                resealed = Some(seal_content(&key, post, &content)?);
            }
            tx.prepare_cached(
                "UPDATE messages SET content_sealed = COALESCE(?2, content_sealed), pinned = COALESCE(?3, pinned), \
                 locked = COALESCE(?4, locked), meta_lamport = ?5, meta_id = ?6, \
                 edited_at = CASE WHEN ?7 IS NULL THEN edited_at ELSE MAX(COALESCE(edited_at, ?7), ?7) END WHERE event_id = ?1",
            )?
            .execute(params![
                post.as_bytes(),
                resealed,
                u.pinned,
                u.locked,
                to_sql_u64(u.lamport),
                u.event.as_bytes(),
                u.edited_at.map(ts_to_sql)
            ])?;
            Ok(true)
        })
    }

    /// 포럼 게시글 목록(고정 먼저).
    pub fn list_forum_posts(
        &self,
        space: &SpaceId,
        channel: &ChannelId,
        sort: ForumSort,
        ascending: bool,
        limit: u32,
        viewer: Option<&UserId>,
    ) -> Result<Vec<MessageRecord>> {
        let limit = limit.clamp(1, MAX_PAGE);
        // 고정한 글은 방향과 상관없이 맨 위입니다.
        let order = match (sort, ascending) {
            (ForumSort::Activity, false) => "m.pinned DESC, m.last_activity DESC, m.event_id DESC",
            (ForumSort::Activity, true) => "m.pinned DESC, m.last_activity ASC, m.event_id ASC",
            (ForumSort::Created, false) => "m.pinned DESC, m.lamport DESC, m.event_id DESC",
            (ForumSort::Created, true) => "m.pinned DESC, m.lamport ASC, m.event_id ASC",
        };
        self.read(|c| {
            let mut stmt = c.prepare_cached(&format!(
                "{MSG_SELECT} WHERE m.space = ?1 AND m.channel = ?2 AND m.thread IS NULL AND m.kind = ?3 \
                 ORDER BY {order} LIMIT ?4"
            ))?;
            let rows = stmt.query_map(
                params![space.as_bytes(), channel.as_bytes(), MessageKind::ForumPost.code(), limit],
                read_raw,
            )?;
            let raws: Vec<RawMessage> = rows.collect::<rusqlite::Result<_>>()?;
            raws.into_iter().map(|r| decode_message(c, r, viewer)).collect()
        })
    }

    /// 여러 메시지를 한 연결로 읽습니다(주어진 순서대로, 없거나 지운 것은 빠짐). 검색 결과 한 쪽을 채울 때 씁니다.
    pub fn get_messages(
        &self,
        ids: &[EventId],
        viewer: Option<&UserId>,
    ) -> Result<Vec<MessageRecord>> {
        self.read(|c| {
            let mut stmt = c.prepare_cached(&format!("{MSG_SELECT} WHERE m.event_id = ?1"))?;
            let mut out = Vec::with_capacity(ids.len());
            for id in ids {
                if let Some(raw) = stmt.query_row([id.as_bytes()], read_raw).optional()?
                    && !raw.deleted
                {
                    out.push(decode_message(c, raw, viewer)?);
                }
            }
            Ok(out)
        })
    }

    /// 읽음 위치를 앞으로만 옮깁니다. 옮겼으면 true.
    pub fn set_read_marker(
        &self,
        space: &SpaceId,
        channel: &ChannelId,
        lamport: u64,
        event: &EventId,
    ) -> Result<bool> {
        self.write(|tx| {
            Ok(tx
                .prepare_cached(
                    "INSERT INTO read_markers (space, channel, lamport, event_id, updated_at) VALUES (?1, ?2, ?3, ?4, ?5) \
                     ON CONFLICT (space, channel) DO UPDATE SET lamport = excluded.lamport, \
                       event_id = excluded.event_id, updated_at = excluded.updated_at \
                     WHERE (excluded.lamport, excluded.event_id) > (read_markers.lamport, read_markers.event_id)",
                )?
                .execute(params![
                    space.as_bytes(),
                    channel.as_bytes(),
                    to_sql_u64(lamport),
                    event.as_bytes(),
                    ts_to_sql(Timestamp::now())
                ])?
                > 0)
        })
    }

    /// 채널의 가장 최근 메시지 위치(글타래 답글·포럼 글·삭제된 것 포함). 메시지가 없으면 `None`.
    pub fn latest_in_channel(
        &self,
        space: &SpaceId,
        channel: &ChannelId,
    ) -> Result<Option<(u64, EventId)>> {
        self.read(|c| {
            Ok(c.prepare_cached(
                "SELECT lamport, event_id FROM messages WHERE space = ?1 AND channel = ?2 \
                 ORDER BY lamport DESC, event_id DESC LIMIT 1",
            )?
            .query_row(params![space.as_bytes(), channel.as_bytes()], |r| {
                Ok((from_sql_u64(r.get(0)?), EventId(r.get(1)?)))
            })
            .optional()?)
        })
    }

    pub fn read_marker(
        &self,
        space: &SpaceId,
        channel: &ChannelId,
    ) -> Result<Option<(u64, EventId)>> {
        self.read(|c| {
            Ok(c.prepare_cached(
                "SELECT lamport, event_id FROM read_markers WHERE space = ?1 AND channel = ?2",
            )?
            .query_row(params![space.as_bytes(), channel.as_bytes()], |r| {
                Ok((from_sql_u64(r.get(0)?), EventId(r.get(1)?)))
            })
            .optional()?)
        })
    }

    /// 공간의 채널별 안 읽은 메시지·언급 수(내가 쓴 것과 삭제된 것 제외). 0인 채널은 빠집니다.
    pub fn unread_counts(&self, space: &SpaceId, me: &UserId) -> Result<Vec<ChannelUnread>> {
        self.read(|c| {
            let mut stmt = c.prepare_cached(
                "SELECT m.channel, COUNT(*), SUM(m.mention) FROM messages m \
                 LEFT JOIN read_markers r ON r.space = m.space AND r.channel = m.channel \
                 WHERE m.space = ?1 AND m.author_user != ?2 AND m.deleted = 0 \
                   AND (r.lamport IS NULL OR (m.lamport, m.event_id) > (r.lamport, r.event_id)) \
                 GROUP BY m.channel ORDER BY m.channel",
            )?;
            let rows = stmt.query_map(params![space.as_bytes(), me.as_bytes()], |r| {
                Ok(ChannelUnread {
                    channel: ChannelId(r.get(0)?),
                    unread: u32::try_from(r.get::<_, i64>(1)?).unwrap_or(u32::MAX),
                    mentions: u32::try_from(r.get::<_, Option<i64>>(2)?.unwrap_or(0))
                        .unwrap_or(u32::MAX),
                })
            })?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })
    }

    /// [`Store::unread_counts`]에서 `excluded` 사용자(내가 차단한 사람 등)가 쓴 메시지도 뺍니다.
    /// 제외 목록을 SQL에 넣으면 사용자 수만큼 식이 깊어지거나 변수 수 상한에 걸리므로, 채널·작성자별로 세어
    /// 여기서 빼고 더합니다(행 수는 안 읽은 메시지를 쓴 사람 수만큼).
    pub fn unread_counts_excluding(
        &self,
        space: &SpaceId,
        me: &UserId,
        excluded: &[UserId],
    ) -> Result<Vec<ChannelUnread>> {
        if excluded.is_empty() {
            return self.unread_counts(space, me);
        }
        let excluded: std::collections::HashSet<&UserId> = excluded.iter().collect();
        self.read(|c| {
            let mut stmt = c.prepare_cached(
                "SELECT m.channel, m.author_user, COUNT(*), SUM(m.mention) FROM messages m \
                 LEFT JOIN read_markers r ON r.space = m.space AND r.channel = m.channel \
                 WHERE m.space = ?1 AND m.author_user != ?2 AND m.deleted = 0 \
                   AND (r.lamport IS NULL OR (m.lamport, m.event_id) > (r.lamport, r.event_id)) \
                 GROUP BY m.channel, m.author_user ORDER BY m.channel",
            )?;
            let rows = stmt.query_map(params![space.as_bytes(), me.as_bytes()], |r| {
                Ok((
                    ChannelId(r.get(0)?),
                    UserId(r.get(1)?),
                    r.get::<_, i64>(2)?,
                    r.get::<_, Option<i64>>(3)?.unwrap_or(0),
                ))
            })?;
            let mut out: Vec<ChannelUnread> = Vec::new();
            for row in rows {
                let (channel, author, unread, mentions) = row?;
                if excluded.contains(&author) {
                    continue;
                }
                let unread = u32::try_from(unread).unwrap_or(u32::MAX);
                let mentions = u32::try_from(mentions).unwrap_or(u32::MAX);
                match out.last_mut() {
                    Some(last) if last.channel == channel => {
                        last.unread = last.unread.saturating_add(unread);
                        last.mentions = last.mentions.saturating_add(mentions);
                    }
                    _ => out.push(ChannelUnread {
                        channel,
                        unread,
                        mentions,
                    }),
                }
            }
            Ok(out)
        })
    }
}
