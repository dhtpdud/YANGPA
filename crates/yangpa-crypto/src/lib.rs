//! 양파 메신저 암호 계층.
//!
//! 기본 요소(AEAD·패딩·KDF·봉인 상자·잠긴 메모리)와 이벤트 본문·블롭 암호화, 신원 키,
//! 키 그룹(`KeyUpdate`)의 키 일정([`keysched`])과 키 트리([`keytree`]).

pub mod aead;
pub mod blob;
pub mod blobpack;
pub mod body;
pub mod error;
pub mod identity;
pub mod kdf;
pub mod keysched;
pub mod keytree;
pub mod locked;
pub mod pad;
pub mod sealed;

pub use aead::{open, seal};
pub use blob::{
    BlobEncryptor, decrypt_blob, decrypt_blob_chunk, encrypt_blob, encrypt_blob_stream,
};
pub use blobpack::{BlobLayout, BlobReader, ReadError, pack_blob, packed_source};
pub use body::{decrypt_body, encrypt_body, open_sealed_body, seal_body};
pub use error::{CryptoError, Result};
pub use identity::{DeviceKeys, Identity, safety_number};
pub use kdf::derive;
pub use keytree::{
    CommitCtx, CoverGroup, DeviceBundle, GroupKind, KeyTree, KeyUpdateError, KeyUpdateOutput,
    MemberCtx, MemberKeys,
};
pub use locked::LockedKeyring;
pub use pad::{pad, unpad};
pub use sealed::{BoxKeypair, open_sealed, seal_to, sealed_recipients};
pub use yangpa_core::body::Secret32;
