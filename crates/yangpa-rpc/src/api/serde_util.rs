//! API 직렬화 도우미.

use std::fmt;
use std::str::FromStr;

use serde::de::{self, Deserializer, Visitor};
use serde::{Deserialize, Serialize, Serializer};
use yangpa_core::ids::EmojiId;
use zeroize::Zeroize;

/// 비밀 문자열(토큰·기기 연결 링크). `Debug`에 내용이 나오지 않고, 해제 시 0으로 덮어씁니다.
#[derive(Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SecretString(String);

impl SecretString {
    pub fn new(s: impl Into<String>) -> Self {
        SecretString(s.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl From<String> for SecretString {
    fn from(s: String) -> Self {
        SecretString(s)
    }
}

impl From<&str> for SecretString {
    fn from(s: &str) -> Self {
        SecretString(s.to_owned())
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretString(..)")
    }
}

impl Drop for SecretString {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

/// 이모지 ID를 10진수 문자열로 직렬화합니다(64비트 정수는 JS에서 정밀도를 잃음). 읽을 때는 숫자도 받습니다.
pub mod emoji_id_str {
    use super::*;

    pub fn serialize<S: Serializer>(id: &EmojiId, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(id)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<EmojiId, D::Error> {
        struct V;
        impl Visitor<'_> for V {
            type Value = EmojiId;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("10진수 문자열 이모지 ID")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<EmojiId, E> {
                EmojiId::from_str(v.trim()).map_err(|_| E::custom("잘못된 이모지 ID"))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<EmojiId, E> {
                Ok(EmojiId(v))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<EmojiId, E> {
                u64::try_from(v)
                    .map(EmojiId)
                    .map_err(|_| E::custom("잘못된 이모지 ID"))
            }
        }
        d.deserialize_any(V)
    }
}

/// `Option<Option<T>>` 필드: 빠짐 = 바꾸지 않음, `null` = 지움(기본값으로), 값 = 그 값.
///
/// `#[serde(default, skip_serializing_if = "Option::is_none", with = "double_option")]`와 함께 씁니다.
pub mod double_option {
    use super::*;

    pub fn serialize<T: Serialize, S: Serializer>(
        v: &Option<Option<T>>,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        match v {
            Some(inner) => inner.serialize(s),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, T: Deserialize<'de>, D: Deserializer<'de>>(
        d: D,
    ) -> Result<Option<Option<T>>, D::Error> {
        Option::<T>::deserialize(d).map(Some)
    }
}

/// 초대 토큰 해시(32바이트). JSON에서는 hex 문자열.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct TokenHash(pub [u8; 32]);

impl fmt::Display for TokenHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&hex::encode(self.0))
    }
}

impl fmt::Debug for TokenHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TokenHash({})", hex::encode(&self.0[..4]))
    }
}

impl FromStr for TokenHash {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut out = [0u8; 32];
        hex::decode_to_slice(s.trim(), &mut out)
            .map_err(|_| "토큰 해시는 64자리 hex여야 함".to_string())?;
        Ok(TokenHash(out))
    }
}

impl Serialize for TokenHash {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for TokenHash {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct E {
        #[serde(with = "emoji_id_str")]
        id: EmojiId,
    }

    #[derive(Serialize, Deserialize, PartialEq, Debug, Default)]
    struct D {
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            with = "double_option"
        )]
        v: Option<Option<u64>>,
    }

    #[test]
    fn emoji_id_as_string() {
        let e = E {
            id: EmojiId(u64::MAX),
        };
        let s = serde_json::to_string(&e).unwrap();
        assert_eq!(s, r#"{"id":"18446744073709551615"}"#);
        assert_eq!(serde_json::from_str::<E>(&s).unwrap(), e);
        assert_eq!(
            serde_json::from_str::<E>(r#"{"id":42}"#).unwrap().id,
            EmojiId(42)
        );
        assert!(serde_json::from_str::<E>(r#"{"id":"x"}"#).is_err());
    }

    #[test]
    fn double_option_three_states() {
        assert_eq!(serde_json::from_str::<D>("{}").unwrap().v, None);
        assert_eq!(
            serde_json::from_str::<D>(r#"{"v":null}"#).unwrap().v,
            Some(None)
        );
        assert_eq!(
            serde_json::from_str::<D>(r#"{"v":5}"#).unwrap().v,
            Some(Some(5))
        );
        assert_eq!(serde_json::to_string(&D { v: None }).unwrap(), "{}");
        assert_eq!(
            serde_json::to_string(&D { v: Some(None) }).unwrap(),
            r#"{"v":null}"#
        );
    }

    #[test]
    fn secret_and_hash() {
        let s = SecretString::new("abc");
        assert_eq!(format!("{s:?}"), "SecretString(..)");
        assert_eq!(serde_json::to_string(&s).unwrap(), "\"abc\"");
        let h = TokenHash([0xab; 32]);
        let j = serde_json::to_string(&h).unwrap();
        assert_eq!(j.len(), 66);
        assert_eq!(serde_json::from_str::<TokenHash>(&j).unwrap(), h);
        assert!("zz".parse::<TokenHash>().is_err());
    }
}
