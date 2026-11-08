// 앱 명령마다 `allow-<명령>` 권한을 만들어 capabilities/default.json에서 하나씩 허용합니다.
//
// 데몬 사이드카: tauri.conf.json의 `bundle.externalBin`은 `binaries/yangpad`이고, 릴리스 파이프라인이
// `binaries/yangpad-<타깃 트리플>[.exe]`를 넣습니다. 개발 빌드(`cargo build -p yangpa-desktop`)에는 그 파일이
// 없으므로, 없을 때만 TAURI_CONFIG 덧씌우기로 externalBin을 빼고 빌드합니다. 그때 앱은 실행 파일 옆·워크스페이스
// target·PATH에서 yangpad를 찾습니다(src/daemon.rs). 번들(`tauri build`)은 CLI가 설정을 따로 읽어 사이드카가
// 없으면 그대로 실패하므로, 배포본에 데몬이 빠지는 일은 없습니다.

use std::path::Path;

const SIDECAR_DIR: &str = "binaries";
const SIDECAR: &str = "yangpad";

fn sidecar_present(target: &str) -> bool {
    let exe = if target.contains("windows") {
        ".exe"
    } else {
        ""
    };
    Path::new(SIDECAR_DIR)
        .join(format!("{SIDECAR}-{target}{exe}"))
        .is_file()
}

/// 이미 있는 TAURI_CONFIG(CLI의 `--config`)에 `bundle.externalBin = null`을 더합니다.
fn without_external_bin(existing: Option<&str>) -> Result<String, serde_json::Error> {
    let mut config = match existing.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => serde_json::from_str::<serde_json::Value>(s)?,
        None => serde_json::json!({}),
    };
    if !config.is_object() {
        config = serde_json::json!({});
    }
    let bundle = config
        .as_object_mut()
        .map(|o| o.entry("bundle").or_insert_with(|| serde_json::json!({})));
    if let Some(b) = bundle {
        if !b.is_object() {
            *b = serde_json::json!({});
        }
        if let Some(o) = b.as_object_mut() {
            o.insert("externalBin".into(), serde_json::Value::Null);
        }
    }
    serde_json::to_string(&config)
}

fn main() {
    // binaries/ 안이 바뀌면(사이드카를 넣거나 뺌) 다시 판단합니다. 폴더는 README로 늘 있게 둡니다
    // (없는 경로를 지켜보면 cargo가 매번 다시 빌드합니다).
    println!("cargo:rerun-if-changed={SIDECAR_DIR}");
    let target = std::env::var("TARGET").unwrap_or_default();
    if !sidecar_present(&target) {
        let existing = std::env::var("TAURI_CONFIG").ok();
        match without_external_bin(existing.as_deref()) {
            // SAFETY: 빌드 스크립트는 아직 다른 스레드를 띄우지 않았습니다(tauri-build가 이 값을 읽기 전).
            Ok(patched) => unsafe { std::env::set_var("TAURI_CONFIG", patched) },
            Err(e) => panic!("TAURI_CONFIG를 읽지 못했습니다: {e}"),
        }
    }
    let manifest = tauri_build::AppManifest::new().commands(&[
        "rpc_call",
        "daemon_status",
        "daemon_retry",
        "set_content_protection",
        "set_capture_watch",
        "open_external",
        "ui_prefs_get",
        "ui_prefs_set",
        "shell_texts",
        "notify_set_viewing",
        "local_preview_open",
        "local_preview_close",
        "neural_status",
        "neural_encode",
        "neural_decode",
        "neural_close",
        "screen_status",
        "screen_sources",
        "screen_start",
        "screen_stop",
        "screen_audio_start",
        "screen_audio_stop",
        "secret_copy",
        "deep_link_take",
        "data_dir_info",
        "data_dir_plan",
        "data_dir_change",
        "app_restart",
        "update_target",
        "update_install",
        "file_import",
        "file_export",
        "file_release",
        "call_active",
        "app_minimize",
        "app_info",
    ]);
    if let Err(e) = tauri_build::try_build(tauri_build::Attributes::new().app_manifest(manifest)) {
        // build.rs에서는 실패를 알릴 방법이 패닉뿐입니다.
        panic!("tauri-build 실패: {e:#}");
    }
}
