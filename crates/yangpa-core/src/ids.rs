//! 고정 길이 식별자 타입.
//!
//! 사람이 읽는 형식(JSON API, 링크)에서는 base58 문자열로, 이진 형식(postcard, CBOR)에서는
//! 바이트 그대로 직렬화합니다.

use std::fmt;
use std::str::FromStr;

use serde::de::{self, Deserializer, Visitor};
use serde::{Deserialize, Serialize, Serializer};

use crate::error::CoreError;

macro_rules! fixed_id {
    ($(#[$meta:meta])* $name:ident, $len:expr) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
        pub struct $name(pub [u8; $len]);

        impl $name {
            pub const LEN: usize = $len;

            pub const fn from_bytes(bytes: [u8; $len]) -> Self {
                Self(bytes)
            }

            pub fn from_slice(bytes: &[u8]) -> Result<Self, CoreError> {
                let arr: [u8; $len] = bytes
                    .try_into()
                    .map_err(|_| CoreError::InvalidLength { expected: $len, got: bytes.len() })?;
                Ok(Self(arr))
            }

            pub const fn as_bytes(&self) -> &[u8; $len] {
                &self.0
            }

            /// 무작위 식별자를 만듭니다.
            pub fn random() -> Self {
                let mut b = [0u8; $len];
                rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut b);
                Self(b)
            }

            /// 로그용 짧은 표기(앞 4바이트 hex).
            pub fn short(&self) -> String {
                hex::encode(&self.0[..4])
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&bs58::encode(&self.0).into_string())
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({})", stringify!($name), self.short())
            }
        }

        impl FromStr for $name {
            type Err = CoreError;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                let v = bs58::decode(s).into_vec().map_err(|_| CoreError::InvalidEncoding)?;
                Self::from_slice(&v)
            }
        }

        impl AsRef<[u8]> for $name {
            fn as_ref(&self) -> &[u8] {
                &self.0
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                if s.is_human_readable() {
                    s.serialize_str(&self.to_string())
                } else {
                    s.serialize_bytes(&self.0)
                }
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                struct V;
                impl<'de> Visitor<'de> for V {
                    type Value = $name;
                    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                        write!(f, "{} bytes or base58 string", $len)
                    }
                    fn visit_str<E: de::Error>(self, v: &str) -> Result<$name, E> {
                        v.parse().map_err(E::custom)
                    }
                    fn visit_bytes<E: de::Error>(self, v: &[u8]) -> Result<$name, E> {
                        $name::from_slice(v).map_err(E::custom)
                    }
                    fn visit_byte_buf<E: de::Error>(self, v: Vec<u8>) -> Result<$name, E> {
                        $name::from_slice(&v).map_err(E::custom)
                    }
                    fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<$name, A::Error> {
                        let mut out = [0u8; $len];
                        for (i, slot) in out.iter_mut().enumerate() {
                            *slot = seq
                                .next_element()?
                                .ok_or_else(|| de::Error::invalid_length(i, &self))?;
                        }
                        Ok($name(out))
                    }
                }
                if d.is_human_readable() {
                    d.deserialize_str(V)
                } else {
                    d.deserialize_bytes(V)
                }
            }
        }
    };
}

fixed_id!(
    /// 사용자 식별자 = 신원 공개키(Ed25519).
    UserId, 32
);
fixed_id!(
    /// 기기 식별자 = 기기 서명 공개키(Ed25519).
    DeviceId, 32
);
fixed_id!(
    /// 공간(스페이스·크루) 식별자. 무작위 16바이트.
    SpaceId, 16
);
fixed_id!(
    /// 채널 식별자. 무작위 16바이트.
    ChannelId, 16
);
fixed_id!(
    /// 이벤트 식별자 = BLAKE3(정규 헤더 인코딩 || 서명).
    EventId, 32
);
fixed_id!(
    /// 블롭(첨부·이모지 원본) 식별자 = 매니페스트의 BLAKE3 해시.
    BlobId, 32
);
fixed_id!(
    /// 상태 체크포인트 식별자 = 서명된 체크포인트 본문의 해시(`checkpoint::SignedCheckpoint::id`). 이벤트 ID와 달리
    /// 암호문이 아니라 평문 본문에서 나오므로, 상태 키 없이도 체크포인트 사슬을 이을 수 있습니다.
    CheckpointId, 32
);
fixed_id!(
    /// 역할 식별자. `RoleId::EVERYONE`은 모든 멤버에게 적용되는 기본 역할.
    RoleId, 16
);
fixed_id!(
    /// 채널 뿌리 키 식별자(`KeyRef::Record`): 키에서 나옵니다([`crate::body::channel_root_id`]).
    ChannelRootId, 16
);
fixed_id!(
    /// 키 그룹 뿌리(R) 식별자(`SharedKey::Root`): 키에서 나옵니다([`crate::body::group_root_id`]).
    RootKeyId, 16
);
fixed_id!(
    /// 상태 키 세대 식별자.
    StateKeyId, 16
);
fixed_id!(
    /// 친구 링크·초대 링크의 1회용 토큰.
    LinkToken, 16
);
fixed_id!(
    /// 통화(통화 채널 세션) 식별자.
    CallId, 16
);
fixed_id!(
    /// 키 그룹 식별자. 키 그룹은 같은 키를 나눠야 하는 기기 모음입니다: 크루 그룹([`KeyGroupId::crew`], 그 갱신은
    /// 공간 스트림)과 열람 규칙이 같은 채널들의 비공개 그룹([`KeyGroupId::private`], 그 키·키 합의 이벤트는 부분 전달
    /// 스트림 [`crate::event::StreamId::KeyGroup`]). 키 갱신 형식(`keyupdate`)과 스트림 이름이 같은 타입을 씁니다.
    KeyGroupId, 16
);

impl RoleId {
    /// 모든 멤버에게 적용되는 기본 역할(@everyone).
    pub const EVERYONE: RoleId = RoleId([0u8; 16]);
}

impl KeyGroupId {
    /// 크루 그룹(모든 크루 기기): 공간 ID 그대로입니다.
    pub const fn crew(space: &SpaceId) -> Self {
        KeyGroupId(space.0)
    }

    /// 비공개 키 그룹: BLAKE3-derive_key("yangpa/v3/key-group", 공간 ‖ 정규화한 열람 규칙)의 앞 16바이트. 상태만 보고
    /// 모든 노드가 같은 값을 얻어야 해서(누가 그 스트림을 받을 자격이 있는지 시더가 계산) 무작위가 아니고, 열람 규칙이
    /// 같은 채널은 같은 그룹이 됩니다. 규칙의 정규화는 [`crate::model::ChannelSettings::view_rule`]입니다.
    pub fn private(space: &SpaceId, view_rule: &[u8]) -> Self {
        let mut h = blake3::Hasher::new_derive_key("yangpa/v3/key-group");
        h.update(space.as_bytes());
        h.update(view_rule);
        let mut out = [0u8; 16];
        out.copy_from_slice(&h.finalize().as_bytes()[..16]);
        KeyGroupId(out)
    }
}

impl EventId {
    /// 바이트열의 BLAKE3 해시로 만든 식별자.
    pub fn hash_of(bytes: &[u8]) -> Self {
        EventId(*blake3::hash(bytes).as_bytes())
    }
}

/// 커스텀 이모지 ID: 이미지 BLAKE3 해시 앞 64비트(빅엔디언)를 10진수로 표기.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EmojiId(pub u64);

impl EmojiId {
    pub fn from_image(bytes: &[u8]) -> Self {
        let h = blake3::hash(bytes);
        let mut first = [0u8; 8];
        first.copy_from_slice(&h.as_bytes()[..8]);
        EmojiId(u64::from_be_bytes(first))
    }
}

impl fmt::Display for EmojiId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl fmt::Debug for EmojiId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "EmojiId({})", self.0)
    }
}

impl FromStr for EmojiId {
    type Err = CoreError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.parse::<u64>()
            .map(EmojiId)
            .map_err(|_| CoreError::InvalidEncoding)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_roundtrip_json_and_postcard() {
        let id = SpaceId::random();
        let json = serde_json::to_string(&id).unwrap();
        assert!(json.starts_with('"'));
        let back: SpaceId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, back);
        let bin = postcard::to_stdvec(&id).unwrap();
        assert_eq!(bin.len(), 1 + 16);
        let back: SpaceId = postcard::from_bytes(&bin).unwrap();
        assert_eq!(id, back);
    }

    #[test]
    fn emoji_id_is_content_derived() {
        let a = EmojiId::from_image(b"png-bytes");
        let b = EmojiId::from_image(b"png-bytes");
        assert_eq!(a, b);
        assert_eq!(a.to_string().parse::<EmojiId>().unwrap(), a);
    }
}
