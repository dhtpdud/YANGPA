//! 양파 메신저(가칭) 공용 타입.
//!
//! 모든 크레이트가 공유하는 계약입니다: 식별자, 서명, 이벤트·본문, 공간·채널·권한 모델,
//! 링크 형식, 블롭 매니페스트, 노드 간 프로토콜 메시지.

/// 태그 붙은 enum을 형식에 따라 다르게 직렬화합니다. JSON처럼 사람이 읽는 형식은 원래 타입의 태그 표현
/// (`#[serde(remote = "Self", tag = ...)]`로 만든 고유 함수), postcard·CBOR 같은 이진 형식은 기본(바깥 태그)
/// 표현(`$bin` 거울 타입)을 씁니다. postcard는 자기 기술 형식이 아니라 내부·인접 태그 enum을 풀지 못합니다.
macro_rules! tagged_only_for_humans {
    ($ty:ident, $bin:ident) => {
        impl serde::Serialize for $ty {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                if s.is_human_readable() {
                    $ty::serialize(self, s)
                } else {
                    $bin::serialize(self, s)
                }
            }
        }

        impl<'de> serde::Deserialize<'de> for $ty {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                if d.is_human_readable() {
                    $ty::deserialize(d)
                } else {
                    $bin::deserialize(d)
                }
            }
        }
    };
}

pub mod archive;
pub mod badge;
pub mod blob;
pub mod body;
pub mod checkpoint;
pub mod device;
pub mod emoji;
pub mod error;
pub mod event;
pub mod ids;
pub mod keyupdate;
pub mod links;
pub mod model;
pub mod sign;
pub mod time;
pub mod wire;

pub use error::CoreError;
pub use ids::*;
pub use time::Timestamp;
