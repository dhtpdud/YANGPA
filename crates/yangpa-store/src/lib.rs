//! 양파 메신저 로컬 저장소.
//!
//! - [`Store`]: SQLCipher DB(원시 32바이트 키, WAL, `secure_delete`, `cipher_memory_security`) + 블롭 파일.
//!   쓰기는 연결 하나로 직렬화하고 읽기는 작은 연결 풀로 동시에 합니다. `Send + Sync`라 `Arc`로 공유합니다.
//! - [`PersistentEvent`] / [`PersistentBlob`]: 디스크 저장 API가 받는 유일한 이벤트·블롭 타입.
//!   휘발성 채널 데이터로는 만들 수 없습니다.
//! - [`VolatileStore`]: 휘발성 채널 전용 RAM 저장소(암호문만 보관, 세션 키는 잠긴 메모리).
//!
//! 받은 이벤트 본문은 메시지마다 무작위 저장 키로 다시 봉인하고(`msg_keys`), 복호화한 메시지 보기도 같은
//! 키로 봉인합니다. 삭제·수명 만료 시 저장 키를 지우면(암호학적 삭제) 기록 키가 남아 있어도 이 디스크에서
//! 내용을 되살릴 수 없습니다.

mod admin;
mod blobs;
mod db;
mod error;
mod events;
mod keys;
mod leak;
mod messages;
mod schema;
mod search;
mod social;
mod space_state;
mod spaces;
pub mod volatile;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod testutil;

pub use admin::{ApiTokenRecord, AuditEntry, PeerRecord, PendingConfirmation};
pub use blobs::{BlobStatus, BlobStore, ChunkWrite, PersistentBlob};
pub use db::{BLOB_DIR_NAME, DB_FILE_NAME, Store, StoreOptions};
pub use error::{Result, StoreError};
pub use events::{
    EventMeta, InsertOutcome, MAX_EXPIRE_BATCH, MAX_RANGE_EVENTS, MAX_RANGE_IDS, MissingParent,
    PersistentEvent,
};
pub use keys::{KeyEntry, KeyKind, secret_names};
pub use leak::file_contains_marker;
pub use messages::{
    ChannelUnread, EditUpdate, ForumMeta, ForumSort, ForumUpdate, MAX_PAGE, MessageContent,
    MessageKind, MessageQuery, MessageRecord, ReactionSummary, ReactionUpdate,
};
pub use search::{SearchFilter, SearchHit, SearchPage};
pub use social::{
    ContactKind, FriendLinkRecord, FriendRecord, FriendRequestRecord, RedeemOutcome,
    RequestDirection,
};
pub use space_state::StateDelta;
pub use spaces::SpaceRecord;
pub use volatile::{
    ExpiredEvent, RamBlobInfo, SnapshotItem, VolatileEvent, VolatileLimits, VolatileStats,
    VolatileStore,
};

// 저장소는 데몬 여러 작업이 함께 씁니다.
const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Store>();
    assert_send_sync::<VolatileStore>();
};
