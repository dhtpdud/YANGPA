//! `yangpa daemon [인자…]`: 데몬(yangpad)을 GUI 없이 실행합니다(NAS·VPS 상시 시더용 헤드리스 모드).
//!
//! 찾는 순서: `YANGPA_DAEMON` 환경 변수 → 이 실행 파일과 같은 디렉터리 → `PATH`.
//! Unix에서는 `exec`로 프로세스를 바꿔 신호(SIGTERM 등)가 데몬에 바로 가게 하고,
//! Windows에서는 자식으로 띄워 끝날 때까지 기다린 뒤 종료 코드를 그대로 돌려줍니다.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow};

use crate::args::DaemonArgs;

/// 데몬 실행 파일 이름.
pub const DAEMON_BIN: &str = "yangpad";
/// 데몬 경로를 직접 지정하는 환경 변수.
pub const ENV_DAEMON: &str = "YANGPA_DAEMON";

fn bin_name() -> String {
    format!("{DAEMON_BIN}{}", std::env::consts::EXE_SUFFIX)
}

/// 데몬 실행 파일을 찾습니다.
pub fn find_daemon() -> Option<PathBuf> {
    find_daemon_with(
        std::env::var_os(ENV_DAEMON),
        std::env::current_exe().ok(),
        std::env::var_os("PATH"),
    )
}

/// 테스트할 수 있게 환경을 인자로 받는 판.
pub fn find_daemon_with(
    explicit: Option<OsString>,
    current_exe: Option<PathBuf>,
    path_var: Option<OsString>,
) -> Option<PathBuf> {
    if let Some(p) = explicit.filter(|p| !p.is_empty()) {
        let p = PathBuf::from(p);
        return is_executable(&p).then_some(p);
    }
    let name = bin_name();
    if let Some(dir) = current_exe.as_deref().and_then(Path::parent) {
        let p = dir.join(&name);
        if is_executable(&p) {
            return Some(p);
        }
    }
    let path_var = path_var?;
    std::env::split_paths(&path_var)
        .filter(|d| !d.as_os_str().is_empty())
        .map(|d| d.join(&name))
        .find(|p| is_executable(p))
}

fn is_executable(p: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(p) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// 데몬을 실행합니다. Unix에서는 성공하면 돌아오지 않습니다. 돌려주는 값은 종료 코드입니다.
pub fn run(args: &DaemonArgs, endpoint: Option<&str>) -> Result<i32> {
    let bin = find_daemon().ok_or_else(|| {
        anyhow!(
            "데몬 실행 파일({})을 찾을 수 없습니다. yangpa와 같은 디렉터리나 PATH에 두거나 {ENV_DAEMON}로 경로를 지정하세요",
            bin_name()
        )
    })?;
    let mut cmd = Command::new(&bin);
    cmd.args(&args.args);
    // `yangpa daemon --endpoint …`처럼 전역 옵션으로 받은 엔드포인트도 데몬에 전합니다.
    if let Some(e) = endpoint.filter(|e| !e.trim().is_empty()) {
        cmd.env(yangpa_rpc::endpoint::ENV_ENDPOINT, OsStr::new(e));
    }
    exec(cmd, &bin)
}

#[cfg(unix)]
fn exec(mut cmd: Command, bin: &Path) -> Result<i32> {
    use std::os::unix::process::CommandExt;
    let err = cmd.exec();
    Err(err).with_context(|| format!("데몬을 실행할 수 없습니다: {}", bin.display()))
}

#[cfg(not(unix))]
fn exec(mut cmd: Command, bin: &Path) -> Result<i32> {
    let status = cmd
        .status()
        .with_context(|| format!("데몬을 실행할 수 없습니다: {}", bin.display()))?;
    Ok(status.code().unwrap_or(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_exe(dir: &Path) -> PathBuf {
        let p = dir.join(bin_name());
        std::fs::write(&p, b"#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        p
    }

    #[test]
    fn lookup_order() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let in_a = make_exe(a.path());
        let in_b = make_exe(b.path());
        let fake_cli = a.path().join("yangpa");
        let path_var = std::env::join_paths([b.path()]).unwrap();

        // 실행 파일 옆이 PATH보다 먼저.
        assert_eq!(
            find_daemon_with(None, Some(fake_cli.clone()), Some(path_var.clone())),
            Some(in_a.clone())
        );
        // 옆에 없으면 PATH.
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(
            find_daemon_with(
                None,
                Some(empty.path().join("yangpa")),
                Some(path_var.clone())
            ),
            Some(in_b.clone())
        );
        // 환경 변수가 가장 먼저이고, 잘못된 경로면 다른 곳을 찾지 않습니다.
        assert_eq!(
            find_daemon_with(Some(in_b.clone().into()), Some(fake_cli.clone()), None),
            Some(in_b)
        );
        assert_eq!(
            find_daemon_with(
                Some("/없는/경로/yangpad".into()),
                Some(fake_cli),
                Some(path_var)
            ),
            None
        );
        assert_eq!(find_daemon_with(None, None, None), None);
    }

    #[cfg(unix)]
    #[test]
    fn non_executable_is_skipped() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let p = make_exe(d.path());
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        let path_var = std::env::join_paths([d.path()]).unwrap();
        assert_eq!(find_daemon_with(None, None, Some(path_var)), None);
    }
}
