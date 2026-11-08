//! 이름 또는 ID로 대상을 찾습니다(`--crew "게임 모임"`, `--channel 공지`, 역할·멤버 이름).
//!
//! 데몬 목록을 받아 순수 함수로 고릅니다. 순서: ID 정확히 일치 → 이름 정확히 일치 → 대소문자 무시 일치
//! → 목록에 없지만 ID 형식인 값(데몬이 판단). 같은 이름이 여럿이면 ID를 쓰라고 알려 줍니다.

use std::fmt::Display;
use std::str::FromStr;

/// 후보 하나: ID와 그 대상을 부를 수 있는 이름들(멤버는 프로필 이름과 크루 닉네임).
#[derive(Clone, Debug)]
pub struct Candidate<T> {
    pub id: T,
    pub names: Vec<String>,
}

impl<T> Candidate<T> {
    pub fn new(id: T, name: impl Into<String>) -> Self {
        Candidate {
            id,
            names: vec![name.into()],
        }
    }

    pub fn with_names(id: T, names: impl IntoIterator<Item = String>) -> Self {
        Candidate {
            id,
            names: names.into_iter().filter(|n| !n.is_empty()).collect(),
        }
    }

    fn label(&self) -> &str {
        self.names.first().map(String::as_str).unwrap_or("")
    }
}

/// 오류 메시지에 보여 줄 후보 수.
const MAX_LISTED: usize = 10;

/// `input`에 맞는 ID를 고릅니다. `what`은 오류 메시지용 대상 이름(예: "크루").
pub fn resolve<T>(input: &str, what: &str, items: &[Candidate<T>]) -> Result<T, String>
where
    T: Copy + PartialEq + FromStr + Display,
{
    let input = input.trim();
    if input.is_empty() {
        return Err(format!("{what} 이름이나 ID가 비었습니다"));
    }
    let parsed = input.parse::<T>().ok();
    if let Some(id) = parsed
        && items.iter().any(|c| c.id == id)
    {
        return Ok(id);
    }

    let exact = unique_ids(items.iter().filter(|c| c.names.iter().any(|n| n == input)));
    if let Some(r) = pick(&exact, input, what)? {
        return Ok(r);
    }
    let lower = input.to_lowercase();
    let folded = unique_ids(
        items
            .iter()
            .filter(|c| c.names.iter().any(|n| n.to_lowercase() == lower)),
    );
    if let Some(r) = pick(&folded, input, what)? {
        return Ok(r);
    }
    if let Some(id) = parsed {
        return Ok(id);
    }
    Err(not_found(input, what, items))
}

/// 목록 여러 개를 한 번에 찾습니다(하나라도 실패하면 오류).
pub fn resolve_all<T>(
    inputs: &[String],
    what: &str,
    items: &[Candidate<T>],
) -> Result<Vec<T>, String>
where
    T: Copy + PartialEq + FromStr + Display,
{
    let mut out: Vec<T> = Vec::with_capacity(inputs.len());
    for s in inputs.iter().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        let id = resolve(s, what, items)?;
        if !out.contains(&id) {
            out.push(id);
        }
    }
    Ok(out)
}

/// 채널 이름 앞의 `#`, 멤버 이름 앞의 `@`를 뗍니다(앱에서 보이는 그대로 붙여 넣어도 되게).
pub fn strip_sigil(input: &str, sigil: char) -> &str {
    let t = input.trim();
    t.strip_prefix(sigil).filter(|s| !s.is_empty()).unwrap_or(t)
}

fn unique_ids<'a, T: Copy + PartialEq + 'a>(
    it: impl Iterator<Item = &'a Candidate<T>>,
) -> Vec<&'a Candidate<T>> {
    let mut out: Vec<&Candidate<T>> = Vec::new();
    for c in it {
        if !out.iter().any(|o| o.id == c.id) {
            out.push(c);
        }
    }
    out
}

fn pick<T: Copy + Display>(
    matches: &[&Candidate<T>],
    input: &str,
    what: &str,
) -> Result<Option<T>, String> {
    match matches {
        [] => Ok(None),
        [one] => Ok(Some(one.id)),
        many => {
            let list: Vec<String> = many
                .iter()
                .take(MAX_LISTED)
                .map(|c| format!("{} ({})", c.label(), c.id))
                .collect();
            Err(format!(
                "'{input}' 이름의 {what}이(가) {}개 있습니다. ID로 지정하세요: {}",
                many.len(),
                list.join(", ")
            ))
        }
    }
}

fn not_found<T>(input: &str, what: &str, items: &[Candidate<T>]) -> String {
    if items.is_empty() {
        return format!("{what} '{input}'을(를) 찾을 수 없습니다(목록이 비었습니다)");
    }
    let mut names: Vec<&str> = items.iter().map(Candidate::label).collect();
    names.dedup();
    let more = if names.len() > MAX_LISTED {
        format!(" 외 {}개", names.len() - MAX_LISTED)
    } else {
        String::new()
    };
    names.truncate(MAX_LISTED);
    format!(
        "{what} '{input}'을(를) 찾을 수 없습니다. 있는 {what}: {}{more}",
        names.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use yangpa_core::ids::{SpaceId, UserId};

    fn spaces() -> Vec<Candidate<SpaceId>> {
        vec![
            Candidate::new(SpaceId([1; 16]), "게임 모임"),
            Candidate::new(SpaceId([2; 16]), "Study"),
            Candidate::new(SpaceId([3; 16]), "중복"),
            Candidate::new(SpaceId([4; 16]), "중복"),
        ]
    }

    #[test]
    fn by_name_and_id() {
        let items = spaces();
        assert_eq!(
            resolve("게임 모임", "크루", &items).unwrap(),
            SpaceId([1; 16])
        );
        assert_eq!(
            resolve(" 게임 모임 ", "크루", &items).unwrap(),
            SpaceId([1; 16])
        );
        let id = SpaceId([2; 16]).to_string();
        assert_eq!(resolve(&id, "크루", &items).unwrap(), SpaceId([2; 16]));
        // 대소문자 무시는 정확히 일치하는 것이 없을 때만.
        assert_eq!(resolve("study", "크루", &items).unwrap(), SpaceId([2; 16]));
    }

    #[test]
    fn ambiguous_and_missing() {
        let items = spaces();
        let e = resolve("중복", "크루", &items).unwrap_err();
        assert!(
            e.contains("2개") && e.contains(&SpaceId([3; 16]).to_string()),
            "{e}"
        );
        let e = resolve("없는 크루", "크루", &items).unwrap_err();
        assert!(
            e.contains("찾을 수 없습니다") && e.contains("게임 모임"),
            "{e}"
        );
        assert!(resolve("", "크루", &items).is_err());
        // 목록에 없어도 ID 형식이면 데몬에 맡깁니다(방금 가입한 공간 등).
        let unknown = SpaceId([9; 16]);
        assert_eq!(
            resolve(&unknown.to_string(), "크루", &items).unwrap(),
            unknown
        );
        assert!(
            resolve("x", "크루", &[] as &[Candidate<SpaceId>])
                .unwrap_err()
                .contains("비었습니다")
        );
    }

    #[test]
    fn members_have_several_names() {
        let items = vec![
            Candidate::with_names(UserId([1; 32]), ["철수".to_string(), "운영자".to_string()]),
            Candidate::with_names(UserId([2; 32]), ["영희".to_string(), String::new()]),
        ];
        assert_eq!(resolve("운영자", "멤버", &items).unwrap(), UserId([1; 32]));
        assert_eq!(resolve("영희", "멤버", &items).unwrap(), UserId([2; 32]));
        assert_eq!(items[1].names.len(), 1);
    }

    #[test]
    fn lists_and_sigils() {
        let items = spaces();
        let v = resolve_all(
            &[
                "게임 모임".into(),
                "Study".into(),
                "게임 모임".into(),
                " ".into(),
            ],
            "크루",
            &items,
        )
        .unwrap();
        assert_eq!(v, vec![SpaceId([1; 16]), SpaceId([2; 16])]);
        assert!(resolve_all(&["게임 모임".into(), "없음".into()], "크루", &items).is_err());
        assert_eq!(strip_sigil("#공지", '#'), "공지");
        assert_eq!(strip_sigil("@철수", '@'), "철수");
        assert_eq!(strip_sigil("#", '#'), "#");
        assert_eq!(strip_sigil("공지", '#'), "공지");
    }
}
