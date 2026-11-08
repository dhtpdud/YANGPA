//! 사용자 문서(README·안내서·헤드리스 안내)에 적힌 `yangpa …` 예시가 실제 명령줄 구조로 해석되는지 봅니다.
//!
//! 문서가 없는 하위 명령이나 인자를 가리키면(예: `profile nick` 대신 `profile nickname`) 따라 한 사용자는
//! 사용법 오류만 받습니다. 데몬 없이 해석만 하므로 빠르고 결정적입니다.

use std::path::{Path, PathBuf};

use clap::Parser;
use clap::error::ErrorKind;
use yangpa_cli::args::Cli;

/// 검사할 문서(저장소 루트 기준). 더하면 `.github/workflows/ci.yml`의 `paths`에도 더해야 그 문서만 바뀐 PR에서 CI가 돕니다.
const DOCS: &[&str] = &[
    "README.md",
    "CONTRIBUTING.md",
    "SECURITY.md",
    "docs/user-guide.md",
    "docs/headless.md",
];

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// 문서에서 뽑은 명령 한 줄.
struct Example {
    doc: &'static str,
    line: usize,
    text: String,
    /// 본문 속 짧은 언급(`` `yangpa seed set --all-channels` ``)은 필수 인자를 줄여 적을 수 있어 그 오류는 봐 줍니다.
    inline: bool,
}

/// 코드 블록의 줄(역슬래시 줄 잇기 반영)과 본문의 인라인 코드를 모읍니다.
fn collect(doc: &'static str, src: &str) -> Vec<Example> {
    let mut out = Vec::new();
    let mut in_block = false;
    let mut block_shell = false;
    let mut pending: Option<(usize, String)> = None;
    for (i, raw) in src.lines().enumerate() {
        let line_no = i + 1;
        let trimmed = raw.trim_start();
        if trimmed.starts_with("```") {
            if in_block {
                in_block = false;
                pending = None;
            } else {
                in_block = true;
                let lang = trimmed.trim_start_matches('`').trim();
                block_shell = matches!(lang, "" | "bash" | "sh" | "shell" | "console");
            }
            continue;
        }
        if in_block {
            if !block_shell {
                continue;
            }
            let (start, mut acc) = pending.take().unwrap_or((line_no, String::new()));
            let part = trimmed.trim_end();
            if let Some(cont) = part.strip_suffix('\\') {
                acc.push_str(cont);
                acc.push(' ');
                pending = Some((start, acc));
                continue;
            }
            acc.push_str(part);
            for seg in shell_segments(&acc) {
                out.push(Example {
                    doc,
                    line: start,
                    text: seg,
                    inline: false,
                });
            }
            continue;
        }
        // 본문: 백틱 하나로 감싼 코드만 봅니다.
        let mut rest = raw;
        while let Some(a) = rest.find('`') {
            let after = &rest[a + 1..];
            let Some(b) = after.find('`') else { break };
            let code = &after[..b];
            for seg in shell_segments(code) {
                out.push(Example {
                    doc,
                    line: line_no,
                    text: seg,
                    inline: true,
                });
            }
            rest = &after[b + 1..];
        }
    }
    out
}

/// 한 줄을 셸 연산자(`&&`, `||`, `;`, `|`, `&`)로 나눠 `yangpa`로 시작하는 조각만 돌려줍니다.
/// 주석(`#` 뒤)과 리디렉션(`>` 뒤)은 버립니다.
fn shell_segments(line: &str) -> Vec<String> {
    let mut segs = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut chars = line.chars().peekable();
    let mut prev_space = true;
    let mut cut = false;
    // `<요청 ID>` 같은 자리표시 안의 `>`는 리디렉션이 아닙니다.
    let mut angle = false;
    while let Some(c) = chars.next() {
        if let Some(q) = quote {
            cur.push(c);
            if c == q {
                quote = None;
            }
            prev_space = false;
            continue;
        }
        match c {
            '\'' | '"' => {
                quote = Some(c);
                cur.push(c);
            }
            '<' if !cut => {
                angle = true;
                cur.push(c);
            }
            '>' if angle => {
                angle = false;
                cur.push(c);
            }
            '#' if prev_space => break,
            '&' | '|' | ';' => {
                // `&&`, `||`도 한 번에 넘깁니다.
                if chars.peek() == Some(&c) {
                    chars.next();
                }
                if !cut {
                    segs.push(std::mem::take(&mut cur));
                }
                cur.clear();
                cut = false;
            }
            '>' => cut = true,
            _ if !cut => cur.push(c),
            _ => {}
        }
        prev_space = c.is_whitespace();
    }
    segs.push(cur);
    segs.into_iter()
        .map(|s| s.trim().to_owned())
        .filter(|s| s == "yangpa" || s.starts_with("yangpa "))
        .collect()
}

/// 따옴표를 푸는 간단한 셸 단어 나누기. `<자리표시>`는 공백이 있어도 한 단어로 봅니다.
fn split_words(s: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut has = false;
    let mut quote: Option<char> = None;
    let mut angle = false;
    for c in s.chars() {
        match (quote, c) {
            (Some(q), _) if c == q => quote = None,
            (Some(_), _) => cur.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                has = true;
            }
            (None, '<') => {
                angle = true;
                cur.push(c);
                has = true;
            }
            (None, '>') if angle => {
                angle = false;
                cur.push(c);
            }
            (None, c) if c.is_whitespace() && !angle => {
                if has {
                    words.push(std::mem::take(&mut cur));
                    has = false;
                }
            }
            (None, c) => {
                cur.push(c);
                has = true;
            }
        }
    }
    if has {
        words.push(cur);
    }
    words
}

/// 자리표시를 그럴듯한 값으로 바꿉니다. `…` 하나뿐인 단어는 "더 있음" 표시라 빼 본 판도 함께 시도합니다.
fn candidates(words: &[String]) -> Vec<Vec<String>> {
    let fill = |w: &String| -> String {
        if w.starts_with('<') && w.ends_with('>') {
            "x".to_owned()
        } else {
            w.clone()
        }
    };
    let filled: Vec<String> = words.iter().map(fill).collect();
    let dropped: Vec<String> = words
        .iter()
        .filter(|w| w.as_str() != "…" && w.as_str() != "...")
        .map(fill)
        .collect();
    let mut out = vec![filled];
    if dropped.len() != words.len() {
        out.push(dropped);
    }
    out
}

/// 짧은 언급에서 봐 주는 오류(하위 명령·인자 이름이 맞으면 통과).
fn tolerated_inline(kind: ErrorKind) -> bool {
    matches!(
        kind,
        ErrorKind::MissingRequiredArgument
            | ErrorKind::MissingSubcommand
            | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
            | ErrorKind::InvalidValue
            | ErrorKind::ValueValidation
    )
}

fn check(ex: &Example) -> Result<(), String> {
    let words = split_words(&ex.text);
    // `yangpa <명령> --help`처럼 하위 명령 자리가 자리표시인 것은 형식 설명이라 건너뜁니다.
    if words.get(1).is_some_and(|w| w.starts_with('<')) {
        return Ok(());
    }
    let mut last = String::new();
    for cand in candidates(&words) {
        match Cli::try_parse_from(&cand) {
            Ok(_) => return Ok(()),
            // --help·--version은 해석에 성공한 것입니다.
            Err(e) if matches!(e.kind(), ErrorKind::DisplayHelp | ErrorKind::DisplayVersion) => {
                return Ok(());
            }
            Err(e) if ex.inline && tolerated_inline(e.kind()) => return Ok(()),
            Err(e) => last = format!("{:?}: {}", e.kind(), e.render().to_string().trim()),
        }
    }
    Err(last)
}

#[test]
fn documented_cli_examples_parse() {
    let root = repo_root();
    let mut checked = 0usize;
    let mut failures = Vec::new();
    for doc in DOCS {
        let path = root.join(doc);
        let src = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{} 을(를) 읽을 수 없습니다: {e}", path.display()));
        for ex in collect(doc, &src) {
            checked += 1;
            if let Err(e) = check(&ex) {
                failures.push(format!("{}:{}: `{}`\n    {e}", ex.doc, ex.line, ex.text));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "문서의 yangpa 예시 {}개가 명령줄 구조와 맞지 않습니다:\n{}",
        failures.len(),
        failures.join("\n")
    );
    // 추출이 깨져 아무것도 검사하지 않는 일이 없게.
    assert!(checked >= 40, "검사한 예시가 너무 적습니다({checked}개)");
}

#[test]
fn rejects_wrong_subcommand() {
    // 고치기 전 안내서에 있던 잘못된 예시가 실제로 걸리는지(검사기 자체의 회귀 시험).
    let ex = Example {
        doc: "test",
        line: 1,
        text: "yangpa profile nick --crew x 별명".into(),
        inline: true,
    };
    assert!(check(&ex).is_err());
    let ok = Example {
        doc: "test",
        line: 1,
        text: "yangpa profile nickname --crew <크루> <별명>".into(),
        inline: true,
    };
    assert_eq!(check(&ok), Ok(()));
}

#[test]
fn segments_split_shell_operators() {
    assert_eq!(
        shell_segments("yangpa friend requests && yangpa friend accept <요청 ID> # 주석"),
        vec!["yangpa friend requests", "yangpa friend accept <요청 ID>"]
    );
    assert_eq!(
        shell_segments("yangpa message list -n 1 > /dev/null"),
        vec!["yangpa message list -n 1"]
    );
    assert!(shell_segments("docker load < 파일").is_empty());
    assert_eq!(
        split_words("yangpa invite join 'yangpa://invite/…' --name \"게임 모임\" <요청 ID>"),
        vec![
            "yangpa",
            "invite",
            "join",
            "yangpa://invite/…",
            "--name",
            "게임 모임",
            "<요청 ID>"
        ]
    );
}
