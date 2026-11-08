//! 양파 메신저 코어 데몬(`yangpad`) 라이브러리.
//!
//! [`Node::start`]가 저장소·네트워크·키 그룹·공간 상태를 묶어 띄우고, [`api::ApiHandler`]가 로컬 API(JSON-RPC)를
//! 처리합니다. 테스트는 한 프로세스에 여러 노드를 띄워 [`Node::call`]로 API를 부릅니다.
//! 모듈 지도와 작업 영역별 소유는 `docs/daemon.md`의 "모듈 소유"를 보세요.

pub mod api;
pub mod config;
pub mod error;
pub mod host;
pub mod node;
#[doc(hidden)]
pub mod testkit;

// 코어
pub(crate) mod author;
pub(crate) mod cards;
pub(crate) mod chantopics;
pub(crate) mod checkpoint;
pub(crate) mod devkeys;
pub(crate) mod devlink;
pub(crate) mod friends;
pub(crate) mod identity;
pub(crate) mod inbound;
pub(crate) mod ingest;
pub(crate) mod keygroup;
pub(crate) mod keys;
pub(crate) mod messages;
pub(crate) mod neighbors;
pub(crate) mod notify;
pub(crate) mod peers;
pub(crate) mod spaces;
pub(crate) mod state_index;
pub(crate) mod sync_service;
pub(crate) mod topics;

// 다른 작업 영역(자리표시·고리 함수). docs/daemon.md "모듈 소유" 참고.
pub(crate) mod admin;
pub(crate) mod anon;
pub(crate) mod archive;
pub(crate) mod badges;
pub(crate) mod blobs;
pub(crate) mod bridges;
pub(crate) mod crews;
pub(crate) mod diag;
pub(crate) mod filelink;
// 후원 기능은 별도 라이선스라 `supporter/` 폴더에 모읍니다(supporter/LICENSE.md).
#[path = "supporter/flair.rs"]
pub(crate) mod flair;
pub(crate) mod locator;
pub(crate) mod moderation;
pub(crate) mod notes;
pub(crate) mod p2p;
pub(crate) mod paste;
pub(crate) mod policy;
pub(crate) mod presence;
pub(crate) mod qr;
pub(crate) mod ratelimit;
pub(crate) mod scoped;
pub(crate) mod sfu;
pub(crate) mod stun;
pub(crate) mod supporter;
pub(crate) mod tor;
pub(crate) mod transfer;
pub(crate) mod trust;
mod typing;
pub(crate) mod voice;
pub(crate) mod volatile;

pub use api::{ApiHandler, CallCtx};
pub use config::{
    ArchiveOptions, CheckpointOptions, KeyStorage, NetOptions, NodeConfig, SfuHostMode, SfuOptions,
};
pub use error::{Error, Result};
pub use node::Node;

// 휘발성 채널 첨부(X1)의 RAM 한도. 다른 영역이 위 줄을 고쳐도 겹치지 않게 따로 둡니다.
pub use config::VolatileOptions;
