//! 앱 셸이 직접 띄우는 문구(트레이 메뉴·OS 알림·화면 이름)의 화면 언어 번역.
//! 카탈로그는 웹뷰(src/i18n.ts) 하나뿐이라, 웹뷰가 시작할 때 번역한 문구를 `shell_texts`로 넘깁니다.
//! 받기 전(웹뷰가 뜨기 전)에는 한국어 원문을 씁니다.

use std::collections::HashMap;
use std::sync::RwLock;

use crate::error::CmdError;

/// 받는 문구 수·길이 상한(웹뷰가 보내는 고정 표라 넉넉히).
const MAX_KEYS: usize = 32;
const MAX_LEN: usize = 1024;

static TEXTS: RwLock<Option<HashMap<String, String>>> = RwLock::new(None);

pub fn set(texts: HashMap<String, String>) -> Result<(), CmdError> {
    if texts.len() > MAX_KEYS || texts.iter().any(|(k, v)| k.len() > 64 || v.len() > MAX_LEN) {
        return Err(CmdError::invalid_params("셸 문구가 너무 많거나 깁니다"));
    }
    *TEXTS.write().unwrap_or_else(|e| e.into_inner()) = Some(texts);
    Ok(())
}

/// `key`의 번역(없으면 한국어 `ko`). `{이름}` 자리는 `vars`로 채웁니다.
pub fn text(key: &str, ko: &str, vars: &[(&str, &str)]) -> String {
    let mut s = TEXTS
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .and_then(|m| m.get(key).cloned())
        .unwrap_or_else(|| ko.to_owned());
    for (k, v) in vars {
        s = s.replace(&format!("{{{k}}}"), v);
    }
    s
}

/// 데몬이 만든 알림의 고정 문구(휘발성 제목 "새 메시지", 첨부만 보낸 본문 "첨부 파일")를 화면 언어로.
pub fn daemon_notice(s: &str) -> String {
    match s {
        "새 메시지" => text("newMessage", s, &[]),
        "첨부 파일" => text("attachment", s, &[]),
        _ => s.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn falls_back_then_uses_texts() {
        assert_eq!(text("nope", "{n}개", &[("n", "3")]), "3개");
        set(HashMap::from([("attachment".into(), "Attachment".into())])).unwrap();
        assert_eq!(daemon_notice("첨부 파일"), "Attachment");
        assert_eq!(daemon_notice("민지"), "민지");
        assert!(set(HashMap::from([("k".into(), "x".repeat(MAX_LEN + 1))])).is_err());
    }
}
