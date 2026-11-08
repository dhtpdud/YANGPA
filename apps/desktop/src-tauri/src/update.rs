//! 앱 업데이트 설치. 확인·내려받기는 데몬이 Tor로 합니다(`update.check`·`update.download`, yangpa-daemon `update.rs`).
//! 여기서는 이 앱이 어떤 묶음인지 보고 받을 파일을 정하고([`update_target`]), 데몬이 받은 파일의 서명·SHA-256을 다시
//! 확인한 뒤 설치합니다([`update_install`]). 웹뷰는 판 번호만 넘기고 경로는 넘기지 못합니다(웹뷰가 아무 파일이나
//! 실행시키지 못하게). 더 새 판이 아니면 설치하지 않습니다(서명된 옛 판으로 되돌리기 막기).
//!
//! 묶음별 설치:
//! - Windows NSIS: `setup.exe /P /UPDATE /R`(진행 창만, 지우지 않고 덮어씀, 끝나면 앱을 다시 띄움) 뒤 앱 종료.
//!   설치 파일의 훅이 데몬을 끕니다. exe 옆 `Data`는 건드리지 않습니다.
//! - Windows MSI(쓸 수 없는 Program Files): `msiexec /i … /passive AUTOLAUNCHAPP=True` 뒤 앱 종료(관리자 확인 창).
//! - Windows 포터블(exe 옆 [`PORTABLE_MARKER`]): zip을 exe 옆 [`STAGING`]에 풀고, 앱이 끝나기를 기다렸다가 덮어쓰고
//!   다시 띄우는 배치 파일([`APPLY_CMD`])을 띄운 뒤 앱 종료. `Data`·`data-dir.json`은 덮어쓰지 않습니다.
//!   포터블 exe는 설치 파일 묶음에서 복사돼 Tauri의 묶음 표시가 NSIS·MSI로 남으므로 표시 파일을 먼저 봅니다.
//! - Linux AppImage: 받은 파일로 `$APPIMAGE`를 바꾸고 다시 시작.
//! - deb·macOS: 받은 설치 파일을 시스템으로 엽니다(설치는 사용자가).
//! - Android·개발 빌드: 설치하지 않음(웹뷰가 릴리스 페이지를 엽니다).

use std::path::{Path, PathBuf};

use serde::Serialize;
use sha2::{Digest, Sha256};
use tauri::AppHandle;
use tracing::{info, warn};
use yangpa_core::release::{self, SUMS, SUMS_SIG};
use yangpa_rpc::endpoint;

use crate::error::CmdError;

/// 포터블 zip에만 있는 표시 파일(scripts/build-windows-release.sh가 넣음).
pub const PORTABLE_MARKER: &str = ".yangpa-portable";
/// 포터블 업데이트를 푸는 폴더(exe 옆). 다음 시작 때 지웁니다.
pub const STAGING: &str = ".yangpa-update";
/// 데몬이 받은 파일을 두는 데이터 폴더 안 폴더(yangpa-daemon `update.rs`와 같음).
const DOWNLOADS: &str = "updates";

/// 포터블 덮어쓰기: `%1` 앱 PID가 끝나면 `%2`(새 판 폴더)를 `%3`(exe 폴더)에 복사하고 앱을 다시 띄웁니다.
/// 데이터(`Data`)와 데이터 폴더 설정(`data-dir.json`)은 빼고, 데몬이 아직 쥔 파일은 robocopy가 1초마다 60번 다시 시도합니다.
/// 경로는 인자로만 받아(배치 파일 자체는 ASCII) 한글 경로도 그대로 갑니다. `timeout`은 콘솔이 없으면 실패해 ping으로 쉽니다.
pub const APPLY_CMD: &str = "@echo off\r\n\
:wait\r\n\
tasklist /fi \"PID eq %~1\" /nh | find \"%~1\" >nul && (ping -n 2 127.0.0.1 >nul & goto wait)\r\n\
robocopy \"%~2\" \"%~3\" /e /xd Data /xf data-dir.json /r:60 /w:1 /np /njh /njs /nfl /ndl >nul\r\n\
start \"\" \"%~3\\yangpa-desktop.exe\"\r\n";

/// 이 설치본의 업데이트 방식.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Nsis,
    Msi,
    Portable,
    AppImage,
    /// 받은 설치 파일을 시스템으로 엽니다(deb·dmg).
    Open,
}

/// 웹뷰에 주는 업데이트 방식과 릴리스 파일 이름의 판 뒤 부분(`YANGPA_<판>_<suffix>`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Target {
    pub kind: Kind,
    pub suffix: &'static str,
}

#[cfg(all(windows, target_arch = "x86_64"))]
fn target() -> Option<(Kind, &'static str)> {
    use tauri::utils::config::BundleType;
    if endpoint::exe_dir()?.join(PORTABLE_MARKER).is_file() {
        return Some((Kind::Portable, "x64_portable.zip"));
    }
    match tauri::utils::platform::bundle_type()? {
        BundleType::Nsis => Some((Kind::Nsis, "x64-setup.exe")),
        BundleType::Msi => Some((Kind::Msi, "x64_en-US.msi")),
        _ => None,
    }
}

#[cfg(target_os = "linux")]
fn target() -> Option<(Kind, &'static str)> {
    use tauri::utils::config::BundleType;
    let arm = cfg!(target_arch = "aarch64");
    match tauri::utils::platform::bundle_type()? {
        BundleType::AppImage if std::env::var_os("APPIMAGE").is_some() => Some((
            Kind::AppImage,
            if arm {
                "aarch64.AppImage"
            } else {
                "amd64.AppImage"
            },
        )),
        BundleType::Deb => Some((Kind::Open, if arm { "arm64.deb" } else { "amd64.deb" })),
        _ => None,
    }
}

#[cfg(target_os = "macos")]
fn target() -> Option<(Kind, &'static str)> {
    Some((Kind::Open, "universal.dmg"))
}

#[cfg(not(any(
    all(windows, target_arch = "x86_64"),
    target_os = "linux",
    target_os = "macos"
)))]
fn target() -> Option<(Kind, &'static str)> {
    None
}

/// 이 설치본을 앱 안에서 업데이트할 수 있으면 그 방식. 없으면(Android·개발 빌드) 웹뷰가 릴리스 페이지를 엽니다.
#[tauri::command]
pub fn update_target() -> Option<Target> {
    target().map(|(kind, suffix)| Target { kind, suffix })
}

/// 데몬이 받은 그 판의 파일을 다시 확인하고 설치합니다. NSIS·MSI·포터블은 설치를 띄운 뒤 앱을 끝내고, AppImage는
/// 다시 시작하며, 여는 방식(deb·dmg)은 설치 파일만 엽니다.
#[tauri::command]
pub async fn update_install(app: AppHandle, version: String) -> Result<Kind, CmdError> {
    let t = update_target()
        .ok_or_else(|| CmdError::invalid_params("이 설치본은 앱 안에서 업데이트할 수 없습니다"))?;
    let asset = format!("YANGPA_{version}_{}", t.suffix);
    let verified = endpoint::data_dir().ok().and_then(|data| {
        let dir = data.join(DOWNLOADS);
        (release::is_newer(&version, env!("CARGO_PKG_VERSION"))
            && release::valid_asset(&asset, &version))
        .then(|| verify(&dir, &asset))
        .flatten()
    });
    let file = tauri::async_runtime::spawn_blocking(move || verified)
        .await
        .ok()
        .flatten()
        .ok_or_else(|| CmdError::invalid_params("업데이트 파일을 확인하지 못했습니다"))?;
    info!(kind = ?t.kind, "업데이트를 설치합니다");
    let started = match t.kind {
        Kind::Nsis => spawn(std::process::Command::new(&file).args(["/P", "/UPDATE", "/R"])),
        Kind::Msi => spawn(
            std::process::Command::new("msiexec")
                .arg("/i")
                .arg(&file)
                .args(["/passive", "AUTOLAUNCHAPP=True"]),
        ),
        Kind::Portable => {
            let v = version.clone();
            tauri::async_runtime::spawn_blocking(move || portable(&file, &v))
                .await
                .unwrap_or_else(|e| Err(e.to_string()))
        }
        Kind::AppImage => appimage(&file),
        Kind::Open => spawn(std::process::Command::new(opener()).arg(&file)),
    };
    if let Err(e) = started {
        warn!(error = %e, "업데이트를 시작하지 못했습니다");
        return Err(CmdError::internal("업데이트를 시작하지 못했습니다"));
    }
    match t.kind {
        // RunEvent::Exit를 거쳐 데몬도 끕니다(설치 파일·덮어쓰기가 그 파일을 바꾸게).
        Kind::Nsis | Kind::Msi | Kind::Portable => app.exit(0),
        Kind::AppImage => app.request_restart(),
        Kind::Open => {}
    }
    Ok(t.kind)
}

/// 서명한 목록의 SHA-256과 같으면 그 파일 경로.
fn verify(dir: &Path, asset: &str) -> Option<PathBuf> {
    let sums = std::fs::read(dir.join(SUMS)).ok()?;
    let sig = std::fs::read(dir.join(SUMS_SIG)).ok()?;
    let want = release::signed_sum(&sums, &sig, asset)?;
    let file = dir.join(asset);
    let mut hash = Sha256::new();
    let mut f = std::fs::File::open(&file).ok()?;
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = std::io::Read::read(&mut f, &mut buf).ok()?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
    }
    let got: String = hash.finalize().iter().map(|b| format!("{b:02x}")).collect();
    (got == want).then_some(file)
}

fn spawn(cmd: &mut std::process::Command) -> Result<(), String> {
    cmd.spawn().map(drop).map_err(|e| e.to_string())
}

fn opener() -> &'static str {
    if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    }
}

/// 포터블: exe 옆에 풀고 덮어쓰기 배치를 띄웁니다(앱이 끝나야 실행 중인 파일을 바꿀 수 있음).
#[cfg(windows)]
fn portable(zip: &Path, version: &str) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let exe_dir = endpoint::exe_dir().ok_or("실행 파일 폴더를 모릅니다")?;
    let staging = exe_dir.join(STAGING);
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).map_err(|e| e.to_string())?;
    // Windows 10 1803부터 들어 있는 bsdtar가 zip을 풉니다(`..`·절대 경로는 거절).
    let tar = PathBuf::from(std::env::var_os("SystemRoot").ok_or("SystemRoot가 없습니다")?)
        .join(r"System32\tar.exe");
    let out = std::process::Command::new(tar)
        .arg("-xf")
        .arg(zip)
        .arg("-C")
        .arg(&staging)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| e.to_string())?;
    let src = staging.join(format!("YANGPA_{version}_x64_portable"));
    if !out.status.success() || !src.join("yangpa-desktop.exe").is_file() {
        return Err(format!("zip을 풀지 못했습니다({})", out.status));
    }
    let script = staging.join("apply.cmd");
    std::fs::write(&script, APPLY_CMD).map_err(|e| e.to_string())?;
    std::process::Command::new(script)
        .arg(std::process::id().to_string())
        .arg(&src)
        .arg(&exe_dir)
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map(drop)
        .map_err(|e| e.to_string())
}

#[cfg(not(windows))]
fn portable(_: &Path, _: &str) -> Result<(), String> {
    Err("Windows 포터블만".into())
}

/// AppImage: 같은 폴더에 복사한 뒤 이름을 바꿔 한 번에 갈아 끼웁니다(돌고 있는 파일은 그대로 열려 있음).
#[cfg(unix)]
fn appimage(file: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let target = PathBuf::from(std::env::var_os("APPIMAGE").ok_or("APPIMAGE가 없습니다")?);
    let tmp = target.with_extension("update");
    std::fs::copy(file, &tmp).map_err(|e| e.to_string())?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))
        .map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &target).map_err(|e| e.to_string())
}

#[cfg(not(unix))]
fn appimage(_: &Path) -> Result<(), String> {
    Err("리눅스 AppImage만".into())
}

/// 지난 포터블 업데이트의 푼 폴더를 지웁니다(덮어쓰기 배치가 아직 끝나지 않았으면 다음에).
pub fn cleanup() {
    if let Some(dir) = endpoint::exe_dir() {
        let _ = std::fs::remove_dir_all(dir.join(STAGING));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 포터블 덮어쓰기가 데이터와 데이터 폴더 설정을 건드리지 않는지 고정합니다.
    #[test]
    fn portable_apply_keeps_data() {
        assert!(APPLY_CMD.contains("/xd Data /xf data-dir.json"));
        assert!(APPLY_CMD.is_ascii(), "배치 파일은 ASCII(경로는 인자로)");
        assert!(APPLY_CMD.contains("\"%~3\\yangpa-desktop.exe\""));
    }

    #[test]
    fn verify_needs_signature_and_hash() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("YANGPA_9.9.9_x64-setup.exe"), b"x").unwrap();
        // 서명 파일이 없으면 받은 파일이 있어도 설치하지 않음.
        assert_eq!(verify(dir.path(), "YANGPA_9.9.9_x64-setup.exe"), None);
        std::fs::write(dir.path().join(SUMS), b"00  YANGPA_9.9.9_x64-setup.exe\n").unwrap();
        std::fs::write(dir.path().join(SUMS_SIG), [0u8; 64]).unwrap();
        assert_eq!(verify(dir.path(), "YANGPA_9.9.9_x64-setup.exe"), None);
    }
}
