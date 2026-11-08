//! 메시지 안 커스텀 이모지 표기: 정지 `<:이름:ID>`, 애니메이션 `<a:이름:ID>` (디스코드 양식).

use crate::ids::EmojiId;

/// 지원 확장자.
pub const ALLOWED_MIME: &[&str] = &["image/png", "image/gif", "image/webp"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EmojiToken<'a> {
    pub animated: bool,
    pub name: &'a str,
    pub id: EmojiId,
    /// 원문에서의 바이트 범위.
    pub range: std::ops::Range<usize>,
}

pub fn format_emoji(name: &str, id: EmojiId, animated: bool) -> String {
    if animated {
        format!("<a:{name}:{id}>")
    } else {
        format!("<:{name}:{id}>")
    }
}

/// 이름 규칙: 2~32자, 영숫자·밑줄·한글.
pub fn valid_name(name: &str) -> bool {
    let n = name.chars().count();
    (2..=32).contains(&n) && name.chars().all(|c| c.is_alphanumeric() || c == '_')
}

/// 텍스트에서 커스텀 이모지 표기를 모두 찾습니다.
pub fn parse_emoji_tokens(text: &str) -> Vec<EmojiToken<'_>> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while let Some(off) = text[i..].find('<') {
        let start = i + off;
        let Some(end_off) = text[start..].find('>') else {
            break;
        };
        let end = start + end_off;
        let inner = &text[start + 1..end];
        let (animated, rest) = if let Some(r) = inner.strip_prefix("a:") {
            (true, r)
        } else if let Some(r) = inner.strip_prefix(':') {
            (false, r)
        } else {
            i = start + 1;
            continue;
        };
        if let Some((name, id)) = rest.split_once(':')
            && valid_name(name)
            && !id.is_empty()
            && id.bytes().all(|b| b.is_ascii_digit())
            && let Ok(id) = id.parse::<u64>()
        {
            out.push(EmojiToken {
                animated,
                name,
                id: EmojiId(id),
                range: start..end + 1,
            });
            i = end + 1;
            continue;
        }
        i = start + 1;
        if i >= bytes.len() {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_tokens() {
        let id = EmojiId::from_image(b"img");
        let text = format!(
            "안녕 {} 그리고 {} <notemoji>",
            format_emoji("양파", id, false),
            format_emoji("dance", EmojiId(42), true)
        );
        let toks = parse_emoji_tokens(&text);
        assert_eq!(toks.len(), 2);
        assert_eq!(toks[0].name, "양파");
        assert!(!toks[0].animated);
        assert_eq!(toks[0].id, id);
        assert!(toks[1].animated);
        assert_eq!(&text[toks[1].range.clone()], "<a:dance:42>");
    }

    #[test]
    fn names() {
        assert!(valid_name("yangpa_1"));
        assert!(!valid_name("a"));
        assert!(!valid_name("bad name"));
    }
}
