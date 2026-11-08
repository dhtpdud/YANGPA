//! 데몬 통합 시험을 실행 파일 하나로 묶습니다. 파일마다 따로 링크하면(실행 파일 하나에 수백 MB) 코드를 조금만
//! 바꿔도 같은 링크를 열몇 번 되풀이해, 시험 빌드 시간 대부분이 거기에 들었습니다. 시험 내용은 각 파일 그대로이고,
//! 한 파일만 돌릴 때는 `cargo test -p yangpa-daemon --test daemon <파일 이름>::`.
//!
//! Cargo.toml의 `autotests = false`라 tests/에 새 파일을 만들면 여기에 넣어야 돕니다. 빠뜨리면
//! `every_test_file_is_registered`가 실패합니다. perf.rs는 bench.yml이 `--test perf`로 따로 빌드합니다.

#[path = "common/mod.rs"]
mod common;

#[path = "admin.rs"]
mod admin;
#[path = "archive.rs"]
mod archive;
#[path = "blobs.rs"]
mod blobs;
#[path = "checkpoint.rs"]
mod checkpoint;
#[path = "core.rs"]
mod core;
#[path = "crews.rs"]
mod crews;
#[path = "integration.rs"]
mod integration;
#[path = "keygroups.rs"]
mod keygroups;
#[path = "moderation.rs"]
mod moderation;
#[path = "net_features.rs"]
mod net_features;
#[path = "presence.rs"]
mod presence;
#[path = "robustness.rs"]
mod robustness;
#[path = "sfu.rs"]
mod sfu;
#[path = "storage.rs"]
mod storage;
#[path = "tor_leak.rs"]
mod tor_leak;
#[path = "volatile.rs"]
mod volatile;
#[path = "volatile_blobs.rs"]
mod volatile_blobs;

/// tests/의 `.rs` 파일이 모두 이 묶음이나 따로 둔 시험 대상(Cargo.toml `[[test]]`)에 들어 있는지 봅니다.
#[test]
fn every_test_file_is_registered() {
    let here = include_str!("daemon.rs");
    let manifest = include_str!("../Cargo.toml");
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    for entry in std::fs::read_dir(&dir).expect("tests 폴더") {
        let path = entry.expect("tests 항목").path();
        if path.extension().is_none_or(|e| e != "rs") {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .expect("파일 이름");
        let in_bundle = here.contains(&format!("#[path = \"{name}\"]"));
        let own_target = manifest.contains(&format!("path = \"tests/{name}\""));
        assert!(
            in_bundle || own_target,
            "tests/{name}이 어떤 시험 대상에도 없습니다. tests/daemon.rs에 모듈로 넣으세요"
        );
    }
}
