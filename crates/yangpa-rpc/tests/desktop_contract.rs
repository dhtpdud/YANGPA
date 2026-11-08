//! 데스크톱 앱(apps/desktop)과 로컬 API 계약이 어긋나지 않게 지키는 시험.
//!
//! 메서드 표(이름·범위·공간 규칙·파괴적 여부·사람 전용), 알림 종류, 권한 이름·비트를
//! `apps/desktop/src/api/methods.generated.json`으로 내보냅니다. 커밋된 파일이 지금 표와 다르면 파일을 새로 쓰고
//! 실패합니다(새로 쓴 파일을 커밋하면 다음 실행부터 통과). 앱 쪽 vitest(`contract.test.ts`)는 이 파일과
//! `types.ts`의 메서드·알림·권한 목록이 정확히 같은지 검사합니다.

use std::path::PathBuf;

use serde_json::{Value, json};
use yangpa_core::model::Permissions;
use yangpa_rpc::api::{Destructive, METHODS, Notification, SpaceRule};

fn generated_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../apps/desktop/src/api/methods.generated.json")
}

fn space_rule(r: SpaceRule) -> &'static str {
    match r {
        SpaceRule::Required => "required",
        SpaceRule::OptionalFiltered => "optional_filtered",
        SpaceRule::OptionalGlobal => "optional_global",
        SpaceRule::Filtered => "filtered",
        SpaceRule::Global => "global",
    }
}

fn destructive(d: Destructive) -> &'static str {
    match d {
        Destructive::No => "no",
        Destructive::Always => "always",
        Destructive::WhenOthers => "when_others",
    }
}

/// 지금 메서드 표의 JSON 설명. 순서는 표 순서 그대로(사람이 읽기 쉽게).
fn contract() -> Value {
    let methods: Vec<Value> = METHODS
        .iter()
        .map(|m| {
            json!({
                "name": m.name,
                "scope": m.scope.map_or("public", |s| s.name()),
                "space": space_rule(m.space),
                "destructive": destructive(m.destructive),
                "human_only": m.human_only,
            })
        })
        .collect();
    let permissions: Vec<Value> = Permissions::NAMES
        .iter()
        .map(|(name, bits)| {
            assert_eq!(bits.0.count_ones(), 1, "권한 {name}은 비트 하나여야 합니다");
            json!({"name": name, "bit": bits.0.trailing_zeros()})
        })
        .collect();
    json!({
        "_comment": "cargo test -p yangpa-rpc --test desktop_contract 가 만듭니다. 손으로 고치지 마세요.",
        "methods": methods,
        "notifications": Notification::TYPES,
        "permissions": permissions,
    })
}

#[test]
fn desktop_generated_contract_is_fresh() {
    let path = generated_path();
    let want = serde_json::to_string_pretty(&contract()).expect("JSON") + "\n";
    // 윈도 체크아웃의 CRLF 변환은 내용 차이가 아닙니다.
    let have = std::fs::read_to_string(&path)
        .unwrap_or_default()
        .replace("\r\n", "\n");
    if have != want {
        std::fs::write(&path, &want).expect("methods.generated.json 쓰기");
        panic!(
            "{}가 메서드 표와 달라 새로 썼습니다. 이 파일과 apps/desktop/src/api/types.ts·methods.ts를 맞춰 커밋하세요.",
            path.display()
        );
    }
}

#[test]
fn contract_lists_every_method_once() {
    let c = contract();
    let names: Vec<&str> = c["methods"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["name"].as_str().unwrap())
        .collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), names.len(), "중복 메서드");
    assert!(names.contains(&"message.send"));
    assert_eq!(c["methods"][0]["scope"], "public");
}
