//! 데스크톱 앱의 데이터 폴더. 윈도 설치판·포터블(실행 파일 옆에 `yangpa-desktop.exe`)은 데몬·CLI·앱 설정·알림 프사
//! 캐시·웹뷰 저장소를 모두 한 폴더에 둡니다. 기본은 실행 파일 옆 `Data`이고(앱 폴더에 쓸 수 없는 MSI는 OS 기본 경로),
//! 설정에서 바꾸면 `data-dir.json`에 적힙니다(경로 해석은 `yangpa_rpc::endpoint::desktop_data_dir_in`). 예전 판은 데이터를
//! OS 기본 경로에 두었으므로, 폴더를 정한 적 없고 `Data`가 비었으면 옮기지 않고 그 경로를 그대로 씁니다(설정에도 보임).
//!
//! 폴더 바꾸기: 웹뷰가 새 폴더를 고르면 [`data_dir_plan`]이 지금 데이터의 크기와 예상 시간을 알려 주고, 사용자가
//! "옮기기"나 "옮기지 않기"를 고르면 [`data_dir_change`]가 데몬을 끄고(데이터 경로 잠금이 풀릴 때까지 기다린 뒤 그 잠금을
//! 쥐고) 옮긴 다음 `data-dir.json`을 쓰고 앱을 다시 시작합니다. 같은 드라이브면 이름만 바꾸고, 다르면 복사한 뒤 원본을
//! 지웁니다. 도중에 실패하면 옮긴 것을 되돌리고 설정은 그대로 둡니다. DB 키의 OS 키 저장소 계정 이름은 데이터 폴더의
//! `db.keyring`에 적혀 함께 옮겨집니다(yangpa-daemon `identity.rs`).
//!
//! 웹뷰 명령: `data_dir_info`, `data_dir_plan`, `data_dir_change`, `app_restart`.
//! 옮기는 동안 `yangpa-data-move` 이벤트로 진행(`{done, total}` 바이트)을 보냅니다.

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, State};
use tracing::{info, warn};
use yangpa_rpc::endpoint;

use crate::daemon::{Daemon, MAIN_WINDOW};
use crate::error::CmdError;

/// 앱 설정·캐시를 두는 데이터 폴더 안 하위 폴더.
pub const APP_SUBDIR: &str = "app";
/// 웹뷰(WebView2) 저장소. 앱이 도는 동안 웹뷰가 쥐고 있어 옮기지 않습니다(다시 시작하면 새 폴더에 새로 생김).
pub const WEBVIEW_SUBDIR: &str = "webview";
/// 데몬의 데이터 경로 잠금 파일(yangpa-daemon과 같은 이름).
const LOCK_FILE: &str = "yangpad.lock";
/// 진행 이벤트 이름.
pub const MOVE_EVENT: &str = "yangpa-data-move";
/// 데몬이 끝나 잠금이 풀리기를 기다리는 시간.
const LOCK_WAIT: Duration = Duration::from_secs(20);
/// 다른 드라이브로 복사할 때 예상에 쓰는 속도(보수적으로 USB 메모리·HDD 쪽).
const COPY_BYTES_PER_SEC: f64 = 60.0 * 1024.0 * 1024.0;
/// 파일 하나마다 드는 시간(만들기·닫기).
const PER_FILE_SECS: f64 = 0.002;
/// 같은 드라이브에서 이름만 바꿀 때 항목 하나마다 드는 시간.
const PER_RENAME_SECS: f64 = 0.001;

/// 데스크톱 앱 묶음이면 데이터 폴더. `YANGPA_DATA_DIR`로 정했으면 그것이 우선이라 이 폴더를 쓰지 않습니다.
pub fn desktop() -> Option<PathBuf> {
    if endpoint::process_env(endpoint::ENV_DATA_DIR).is_some() {
        return None;
    }
    endpoint::desktop_data_dir()
}

/// 데스크톱 앱 묶음이면 앱 설정·캐시 폴더(`<데이터>/app`).
pub fn app_dir() -> Option<PathBuf> {
    desktop().map(|d| d.join(APP_SUBDIR))
}

/// 데스크톱 앱 묶음이면 웹뷰 저장소 폴더(`<데이터>/webview`).
pub fn webview_dir() -> Option<PathBuf> {
    desktop().map(|d| d.join(WEBVIEW_SUBDIR))
}

/// 데몬 데이터인지 볼 때 빼는 것(앱이 먼저 만드는 하위 폴더와 잠금 파일).
fn is_app_entry(name: &std::ffi::OsStr) -> bool {
    name == APP_SUBDIR || name == WEBVIEW_SUBDIR || name == LOCK_FILE
}

/// 폴더 상태.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DirState {
    /// 없음.
    Missing,
    /// 비었거나 앱 하위 폴더만 있음.
    Empty,
    /// 양파 데이터가 있음(소유자 토큰 파일).
    Yangpa,
    /// 다른 파일이 있음.
    Other,
}

pub fn dir_state(dir: &Path) -> DirState {
    let Ok(rd) = fs::read_dir(dir) else {
        return if dir.exists() {
            DirState::Other
        } else {
            DirState::Missing
        };
    };
    if dir.join(endpoint::OWNER_TOKEN_FILE).is_file() {
        return DirState::Yangpa;
    }
    if rd.flatten().any(|e| !is_app_entry(&e.file_name())) {
        DirState::Other
    } else {
        DirState::Empty
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Usage {
    pub bytes: u64,
    pub files: u64,
}

/// 옮길 데이터의 크기(웹뷰 저장소·잠금 파일은 빼고, 링크는 따라가지 않음).
pub fn usage(dir: &Path) -> Usage {
    let mut u = Usage::default();
    for (src, _) in entries(dir) {
        walk(&src, &mut |m| {
            u.bytes += m.len();
            u.files += 1;
        });
    }
    u
}

fn walk(path: &Path, f: &mut impl FnMut(&fs::Metadata)) {
    let Ok(m) = fs::symlink_metadata(path) else {
        return;
    };
    if m.is_dir() {
        if let Ok(rd) = fs::read_dir(path) {
            for e in rd.flatten() {
                walk(&e.path(), f);
            }
        }
    } else if m.is_file() {
        f(&m);
    }
}

/// 옮길 맨 위 항목(원본 경로, 이름).
fn entries(dir: &Path) -> Vec<(PathBuf, std::ffi::OsString)> {
    let Ok(rd) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut v: Vec<_> = rd
        .flatten()
        .filter(|e| e.file_name() != WEBVIEW_SUBDIR && e.file_name() != LOCK_FILE)
        .map(|e| (e.path(), e.file_name()))
        .collect();
    v.sort();
    v
}

/// 비교용 경로: `.`·`..`를 풀고, 윈도는 대소문자를 무시합니다(파일 시스템을 건드리지 않음, 없는 폴더도 됨).
fn norm(p: &Path) -> String {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            c => out.push(c.as_os_str()),
        }
    }
    let s = out.to_string_lossy().replace('\\', "/");
    let s = s.trim_end_matches('/').to_string();
    if cfg!(windows) { s.to_lowercase() } else { s }
}

fn inside(child: &str, parent: &str) -> bool {
    child == parent || child.starts_with(&format!("{parent}/"))
}

/// 같은 드라이브(윈도: 드라이브 문자·UNC 공유, 그 밖: 같은 장치)인지. 모르면 거짓(복사로 계산).
fn same_volume(a: &Path, b: &Path) -> bool {
    #[cfg(windows)]
    {
        let root = |p: &Path| match p.components().next() {
            Some(Component::Prefix(x)) => Some(x.as_os_str().to_string_lossy().to_lowercase()),
            _ => None,
        };
        root(a).is_some() && root(a) == root(b)
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let dev = |p: &Path| {
            p.ancestors()
                .find_map(|x| fs::metadata(x).ok())
                .map(|m| m.dev())
        };
        dev(a).is_some() && dev(a) == dev(b)
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = (a, b);
        false
    }
}

/// 예상 시간(초).
pub fn estimate_secs(u: Usage, same_volume: bool, entries: usize) -> f64 {
    if same_volume {
        entries as f64 * PER_RENAME_SECS
    } else {
        u.bytes as f64 / COPY_BYTES_PER_SEC + u.files as f64 * PER_FILE_SECS
    }
}

#[derive(Serialize)]
pub struct DataDirInfo {
    /// 윈도 데스크톱 앱(설치판·포터블)이면 참(폴더를 바꿀 수 있음).
    changeable: bool,
    dir: String,
    /// 기본 폴더(실행 파일 옆 Data, 앱 폴더에 쓸 수 없으면 OS 기본 경로).
    default_dir: Option<String>,
    usage: Usage,
}

#[derive(Clone, Serialize)]
pub struct Plan {
    from: String,
    to: String,
    usage: Usage,
    same_volume: bool,
    estimate_secs: f64,
    /// 새 폴더 상태: missing·empty면 옮길 수 있고, yangpa면 옮기지 않고 그 데이터를 씀.
    target: DirState,
}

/// 폴더를 바꿀 수 있으면 (지금 데이터 폴더, 앱 폴더, 설정 파일 위치).
fn changeable() -> Result<(PathBuf, PathBuf, PathBuf), CmdError> {
    let only = || CmdError::invalid_params("Windows 앱에서만 데이터 폴더를 바꿀 수 있습니다");
    let from = desktop().ok_or_else(only)?;
    let app = endpoint::desktop_dir().ok_or_else(only)?;
    let (_, file) = endpoint::desktop_defaults().ok_or_else(only)?;
    Ok((from, app, file))
}

pub fn make_plan(from: &Path, to: &Path) -> Result<Plan, CmdError> {
    if !to.is_absolute() {
        return Err(CmdError::invalid_params("폴더 경로가 잘못됐습니다"));
    }
    let (f, t) = (norm(from), norm(to));
    if f == t {
        return Err(CmdError::invalid_params("지금 쓰는 폴더입니다"));
    }
    if inside(&t, &f) || inside(&f, &t) {
        return Err(CmdError::invalid_params(
            "지금 데이터 폴더의 안쪽이나 바깥쪽 폴더는 고를 수 없습니다",
        ));
    }
    let usage = usage(from);
    let same = same_volume(from, to);
    Ok(Plan {
        from: from.to_string_lossy().into_owned(),
        to: to.to_string_lossy().into_owned(),
        usage,
        same_volume: same,
        estimate_secs: estimate_secs(usage, same, entries(from).len()),
        target: dir_state(to),
    })
}

/// 데스크톱 앱인데 UI 설정이 아직 데이터 폴더에 없으면 예전 판이 쓰던 OS 앱 설정 경로의 것을 한 번 가져옵니다
/// (예전 판 데이터를 이어 쓰는 사람의 테마·음성 설정 등이 업데이트 뒤에도 그대로이게).
pub fn import_old_prefs(old_dir: Option<PathBuf>) {
    let (Some(new), Some(old)) = (app_dir(), old_dir) else {
        return;
    };
    let (from, to) = (crate::prefs::path_in(&old), crate::prefs::path_in(&new));
    if to.exists() || !from.is_file() {
        return;
    }
    if let Err(e) = fs::create_dir_all(&new).and_then(|_| fs::copy(&from, &to)) {
        warn!("예전 UI 설정을 가져오지 못했습니다: {e}");
    }
}

#[tauri::command]
pub async fn data_dir_info() -> Result<DataDirInfo, CmdError> {
    tauri::async_runtime::spawn_blocking(|| {
        let dir = endpoint::data_dir()
            .map_err(|e| CmdError::internal(format!("데이터 경로를 정할 수 없습니다: {e}")))?;
        let default_dir = changeable()
            .ok()
            .and_then(|_| endpoint::desktop_defaults())
            .map(|(d, _)| d.to_string_lossy().into_owned());
        Ok(DataDirInfo {
            changeable: default_dir.is_some(),
            usage: usage(&dir),
            dir: dir.to_string_lossy().into_owned(),
            default_dir,
        })
    })
    .await
    .map_err(|_| CmdError::internal("데이터 폴더를 읽지 못했습니다"))?
}

#[tauri::command]
pub async fn data_dir_plan(target: String) -> Result<Plan, CmdError> {
    tauri::async_runtime::spawn_blocking(move || {
        let (from, _, _) = changeable()?;
        make_plan(&from, Path::new(target.trim()))
    })
    .await
    .map_err(|_| CmdError::internal("데이터 폴더를 읽지 못했습니다"))?
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeMode {
    /// 지금 데이터를 새 폴더로 옮김.
    Move,
    /// 옮기지 않고 새 폴더를 씀(비었으면 새로 시작, 양파 데이터가 있으면 그것).
    Switch,
}

/// 데이터 경로 잠금. 데몬이 끝나 잠금이 풀릴 때까지 기다렸다가 쥐어, 옮기는 동안 다른 데몬이 뜨지 못하게 합니다.
fn lock_dir(dir: &Path) -> Result<Option<fs::File>, CmdError> {
    if !dir.is_dir() {
        return Ok(None);
    }
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join(LOCK_FILE))
        .map_err(|e| CmdError::internal(format!("데이터 폴더를 잠그지 못했습니다: {e}")))?;
    let deadline = Instant::now() + LOCK_WAIT;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(Some(file)),
            Err(fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(fs::TryLockError::WouldBlock) => {
                return Err(CmdError::internal(
                    "데몬(yangpad)이 아직 이 데이터 폴더를 쓰고 있습니다. 같은 데이터를 쓰는 다른 양파 앱(설치판·포터블)을 끄고 다시 해 주세요",
                ));
            }
            // 잠금을 지원하지 않는 파일 시스템: 데몬이 끝나기를 잠깐 기다리고 그냥 갑니다.
            Err(fs::TryLockError::Error(_)) => {
                std::thread::sleep(Duration::from_secs(2));
                return Ok(None);
            }
        }
    }
}

/// 옮긴 것(되돌리기용).
enum Done {
    Renamed(PathBuf, PathBuf),
    Copied(PathBuf),
}

struct Progress<'a> {
    total: u64,
    done: u64,
    last: Instant,
    emit: &'a dyn Fn(u64, u64),
}

impl Progress<'_> {
    fn add(&mut self, n: u64) {
        self.done += n;
        if self.last.elapsed() >= Duration::from_millis(100) {
            self.last = Instant::now();
            (self.emit)(self.done, self.total);
        }
    }
}

fn copy_tree(src: &Path, dst: &Path, p: &mut Progress<'_>) -> io::Result<()> {
    let m = fs::symlink_metadata(src)?;
    if m.is_dir() {
        fs::create_dir_all(dst)?;
        for e in fs::read_dir(src)? {
            let e = e?;
            copy_tree(&e.path(), &dst.join(e.file_name()), p)?;
        }
    } else if m.is_file() {
        let n = fs::copy(src, dst)?;
        if n != m.len() {
            return Err(io::Error::other(format!(
                "{}을(를) 다 복사하지 못했습니다",
                src.display()
            )));
        }
        p.add(n);
    }
    Ok(())
}

/// `from`의 데이터를 `to`로 옮깁니다. 실패하면 옮긴 것을 되돌립니다. `to`는 없거나 비어 있어야 합니다.
pub fn move_data(from: &Path, to: &Path, emit: &dyn Fn(u64, u64)) -> Result<(), CmdError> {
    let total = usage(from).bytes;
    let same = same_volume(from, to);
    endpoint::create_private_dir(to)
        .map_err(|e| CmdError::internal(format!("새 폴더를 만들지 못했습니다: {e}")))?;
    let mut p = Progress {
        total,
        done: 0,
        last: Instant::now(),
        emit,
    };
    let mut done: Vec<Done> = Vec::new();
    let result = (|| -> io::Result<()> {
        for (src, name) in entries(from) {
            let dst = to.join(&name);
            if dst.exists() {
                // 앱 하위 폴더는 새 쪽이 먼저 생겼을 수 있습니다(빈 폴더면 지우고 옮김).
                if name == APP_SUBDIR {
                    let _ = fs::remove_dir(&dst);
                }
                if dst.exists() {
                    return Err(io::Error::other(format!(
                        "새 폴더에 이미 {}이(가) 있습니다",
                        name.to_string_lossy()
                    )));
                }
            }
            if same && fs::rename(&src, &dst).is_ok() {
                let mut n = 0;
                walk(&dst, &mut |m| n += m.len());
                p.add(n);
                done.push(Done::Renamed(src, dst));
                continue;
            }
            done.push(Done::Copied(dst.clone()));
            copy_tree(&src, &dst, &mut p)?;
        }
        Ok(())
    })();
    if let Err(e) = result {
        for d in done.into_iter().rev() {
            match d {
                Done::Renamed(src, dst) => {
                    let _ = fs::rename(&dst, &src);
                }
                Done::Copied(dst) => {
                    let _ = remove_any(&dst);
                }
            }
        }
        return Err(CmdError::internal(format!(
            "데이터를 옮기지 못했습니다(옮긴 것은 되돌렸습니다): {e}"
        )));
    }
    emit(total, total);
    // 다 복사한 뒤에만 원본을 지웁니다. 지우지 못한 것은 남겨 둡니다(데이터는 이미 새 폴더에 있음).
    for d in &done {
        if let Done::Copied(dst) = d
            && let Some(name) = dst.file_name()
        {
            let src = from.join(name);
            if let Err(e) = remove_any(&src) {
                warn!("옮긴 뒤 예전 데이터를 다 지우지 못했습니다: {e}");
            }
        }
    }
    Ok(())
}

fn remove_any(p: &Path) -> io::Result<()> {
    match fs::symlink_metadata(p) {
        Ok(m) if m.is_dir() => fs::remove_dir_all(p),
        Ok(_) => fs::remove_file(p),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// 설정 파일(`path`, `data-dir.json`)에 데이터 폴더를 적습니다. 앱 폴더(`root`) 안이면 상대 경로로(포터블 폴더째
/// 옮겨도 따라가게).
pub fn write_setting(path: &Path, root: &Path, data: &Path) -> Result<(), CmdError> {
    let rel = data
        .strip_prefix(root)
        .ok()
        .filter(|r| !r.as_os_str().is_empty())
        .map(|r| r.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|| data.to_string_lossy().into_owned());
    let body = serde_json::to_vec_pretty(&serde_json::json!({ "data_dir": rel }))
        .map_err(|e| CmdError::internal(e.to_string()))?;
    let tmp = path.with_extension("json.tmp");
    path.parent()
        .map_or(Ok(()), fs::create_dir_all)
        .and_then(|_| fs::write(&tmp, body))
        .and_then(|_| fs::rename(&tmp, path))
        .map_err(|e| {
            CmdError::internal(format!(
                "설정 파일({})을 쓰지 못했습니다: {e}",
                path.display()
            ))
        })
}

fn emitter(app: &AppHandle) -> impl Fn(u64, u64) + use<> {
    let app = app.clone();
    move |done, total| {
        let _ = app.emit_to(
            MAIN_WINDOW,
            MOVE_EVENT,
            serde_json::json!({ "done": done, "total": total }),
        );
    }
}

/// 데몬을 끄고 그 데이터 경로의 잠금을 쥡니다.
async fn stop_daemon(daemon: &Daemon, dir: &Path) -> Result<Option<fs::File>, CmdError> {
    daemon.stop_on_exit(crate::daemon::spawn_allowed()).await;
    let dir = dir.to_path_buf();
    tauri::async_runtime::spawn_blocking(move || lock_dir(&dir))
        .await
        .map_err(|_| CmdError::internal("데몬이 끝나기를 기다리지 못했습니다"))?
}

/// 데이터 폴더를 바꾸고 앱을 다시 시작합니다. 실패하면 오류를 돌려주고(데몬은 꺼진 채) 웹뷰가 다시 시작을 권합니다.
#[tauri::command]
pub async fn data_dir_change(
    app: AppHandle,
    daemon: State<'_, Arc<Daemon>>,
    target: String,
    mode: ChangeMode,
) -> Result<(), CmdError> {
    let (from, root, setting) = changeable()?;
    let to = PathBuf::from(target.trim());
    let plan = make_plan(&from, &to)?;
    if mode == ChangeMode::Move && !matches!(plan.target, DirState::Missing | DirState::Empty) {
        return Err(CmdError::invalid_params(
            "데이터를 옮길 폴더는 비어 있어야 합니다",
        ));
    }
    if mode == ChangeMode::Switch && plan.target == DirState::Other {
        return Err(CmdError::invalid_params(
            "다른 파일이 있는 폴더입니다. 빈 폴더나 양파 데이터 폴더를 골라 주세요",
        ));
    }
    // 옮기기 전에 설정 파일을 쓸 수 있는지 지금 폴더를 적어 봅니다(옮긴 뒤에 못 쓰면 데이터와 설정이 어긋남).
    write_setting(&setting, &root, &from)?;
    let lock = stop_daemon(&daemon, &from).await?;
    let emit = emitter(&app);
    tauri::async_runtime::spawn_blocking(move || {
        if mode == ChangeMode::Move {
            move_data(&from, &to, &emit)?;
        }
        write_setting(&setting, &root, &to)?;
        drop(lock);
        if mode == ChangeMode::Move {
            // 남은 잠금 파일·웹뷰 저장소·빈 예전 폴더는 지울 수 있으면 지웁니다.
            let _ = fs::remove_file(from.join(LOCK_FILE));
            let _ = fs::remove_dir(&from);
        }
        info!("데이터 폴더를 바꿨습니다");
        Ok::<_, CmdError>(())
    })
    .await
    .map_err(|_| CmdError::internal("데이터 폴더를 바꾸지 못했습니다"))??;
    app.request_restart();
    Ok(())
}

/// 앱을 다시 시작합니다(폴더 바꾸기가 실패해 데몬이 꺼진 뒤 등).
#[tauri::command]
pub fn app_restart(app: AppHandle) {
    app.request_restart();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(p: &Path, bytes: &[u8]) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, bytes).unwrap();
    }

    #[test]
    fn states_and_usage() {
        let t = tempfile::tempdir().unwrap();
        let d = t.path().join("Data");
        assert_eq!(dir_state(&d), DirState::Missing);
        fs::create_dir_all(d.join(APP_SUBDIR)).unwrap();
        fs::create_dir_all(d.join(WEBVIEW_SUBDIR)).unwrap();
        assert_eq!(dir_state(&d), DirState::Empty);
        write(&d.join("x.txt"), b"x");
        assert_eq!(dir_state(&d), DirState::Other);
        write(&d.join(endpoint::OWNER_TOKEN_FILE), b"tok");
        assert_eq!(dir_state(&d), DirState::Yangpa);
        write(&d.join("blobs/a/b"), &[0u8; 1000]);
        write(&d.join(WEBVIEW_SUBDIR).join("big"), &[0u8; 5000]);
        // 웹뷰 저장소는 세지 않습니다.
        assert_eq!(
            usage(&d),
            Usage {
                bytes: 1004,
                files: 3
            }
        );
    }

    #[test]
    fn plan_rejects_nested_and_same() {
        let t = tempfile::tempdir().unwrap();
        let d = t.path().join("Data");
        fs::create_dir_all(&d).unwrap();
        assert!(make_plan(&d, &d).is_err());
        assert!(make_plan(&d, &d.join("sub")).is_err());
        assert!(make_plan(&d, t.path()).is_err());
        assert!(make_plan(&d, Path::new("relative")).is_err());
        let p = make_plan(&d, &t.path().join("Other")).unwrap();
        assert_eq!(p.target, DirState::Missing);
        assert!(p.same_volume);
    }

    #[test]
    fn moves_everything_but_webview_and_lock() {
        let t = tempfile::tempdir().unwrap();
        let from = t.path().join("Data");
        let to = t.path().join("New");
        write(&from.join(endpoint::OWNER_TOKEN_FILE), b"tok");
        write(&from.join("db/node.sqlite"), &[1u8; 300]);
        write(&from.join(APP_SUBDIR).join("ui-prefs.json"), b"{}");
        write(&from.join(WEBVIEW_SUBDIR).join("x"), b"w");
        write(&from.join(LOCK_FILE), b"");
        // 새 쪽에 앱이 먼저 만든 빈 app 폴더가 있어도 됩니다.
        fs::create_dir_all(to.join(APP_SUBDIR)).unwrap();
        let seen = std::cell::RefCell::new(Vec::new());
        move_data(&from, &to, &|d, t| seen.borrow_mut().push((d, t))).unwrap();
        assert_eq!(fs::read(to.join("db/node.sqlite")).unwrap(), vec![1u8; 300]);
        assert!(to.join(endpoint::OWNER_TOKEN_FILE).is_file());
        assert!(to.join(APP_SUBDIR).join("ui-prefs.json").is_file());
        assert!(!to.join(WEBVIEW_SUBDIR).exists());
        assert!(!from.join("db").exists());
        assert!(from.join(WEBVIEW_SUBDIR).exists());
        assert_eq!(seen.borrow().last(), Some(&(305, 305)));
    }

    #[test]
    fn copy_failure_rolls_back() {
        let t = tempfile::tempdir().unwrap();
        let from = t.path().join("Data");
        let to = t.path().join("New");
        write(&from.join("a/1"), b"one");
        write(&from.join("b"), b"two");
        // 새 폴더에 같은 이름이 있으면 실패하고, 먼저 옮긴 것은 되돌립니다.
        write(&to.join("b"), b"other");
        assert!(move_data(&from, &to, &|_, _| {}).is_err());
        assert_eq!(fs::read(from.join("a/1")).unwrap(), b"one");
        assert!(!to.join("a").exists());
        assert_eq!(fs::read(to.join("b")).unwrap(), b"other");
    }

    #[test]
    fn setting_relative_inside_app_dir() {
        let t = tempfile::tempdir().unwrap();
        let file = t.path().join(endpoint::DATA_DIR_FILE);
        write_setting(&file, t.path(), &t.path().join("Data2")).unwrap();
        assert_eq!(
            endpoint::desktop_data_dir_in(t.path(), true, None),
            Some(t.path().join("Data2"))
        );
        let other = tempfile::tempdir().unwrap();
        write_setting(&file, t.path(), other.path()).unwrap();
        let body = fs::read_to_string(&file).unwrap();
        assert!(!body.contains("Data2"));
        assert_eq!(
            endpoint::desktop_data_dir_in(t.path(), true, None).map(|p| norm(&p)),
            Some(norm(other.path()))
        );
    }

    #[test]
    fn estimates() {
        let u = Usage {
            bytes: 600 * 1024 * 1024,
            files: 1000,
        };
        assert!((estimate_secs(u, false, 5) - 12.0).abs() < 0.01);
        assert!(estimate_secs(u, true, 5) < 0.1);
    }

    #[test]
    fn norm_and_inside() {
        assert_eq!(norm(Path::new("/a/b/../c/./")), "/a/c");
        assert!(inside("/a/c/d", "/a/c"));
        assert!(!inside("/a/cd", "/a/c"));
    }
}
