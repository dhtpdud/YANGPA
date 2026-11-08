//! 로컬 API 엔드포인트와 데이터 경로, 소유자 토큰 파일.
//!
//! - Unix: `$XDG_RUNTIME_DIR/yangpa/yangpad.sock`, 없으면 `<데이터 경로>/yangpad.sock`(권한 0600).
//! - Windows: named pipe `\\.\pipe\yangpad-<사용자 이름>`.
//! - `YANGPA_ENDPOINT`로 바꿀 수 있고, 명시 인자(`--endpoint`)가 가장 우선입니다.
//! - 데이터 경로: `YANGPA_DATA_DIR`, 없으면 윈도 데스크톱 앱 묶음(설치판·포터블: 실행 파일 옆에 `yangpa-desktop.exe`)이면
//!   [`desktop_data_dir_in`](설정 파일 `data-dir.json`이 정한 폴더, 없으면 실행 파일 옆 `Data`, 예전 판 데이터가 OS 기본
//!   경로에 있으면 그곳), 아니면 OS 기본(`directories::ProjectDirs("", "", "yangpa")`의 data_dir). 앱·데몬·CLI가 같은
//!   폴더에 있어 셋 다 같은 판단을 합니다. 그 엔드포인트는 데이터 폴더에 딸린 것([`Endpoint::in_dir`])이라 같은 PC의 다른
//!   설치판·포터블 데몬과 겹치지 않습니다(OS 기본 경로를 쓰면 예전처럼 사용자 이름의 파이프).
//!
//! 환경 변수를 읽는 함수마다 `_with(env)` 판을 두어, 프로세스 환경을 바꾸지 않고 시험할 수 있게 했습니다.

use std::ffi::OsString;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use interprocess::local_socket::{GenericFilePath, Name, ToFsName};

use crate::api::serde_util::SecretString;

/// 엔드포인트를 바꾸는 환경 변수.
pub const ENV_ENDPOINT: &str = "YANGPA_ENDPOINT";
/// 데이터 경로를 바꾸는 환경 변수.
pub const ENV_DATA_DIR: &str = "YANGPA_DATA_DIR";
/// 클라이언트(CLI·MCP)가 쓸 API 토큰 환경 변수.
pub const ENV_TOKEN: &str = "YANGPA_TOKEN";
/// 소켓 파일 이름.
pub const SOCKET_FILE: &str = "yangpad.sock";
/// 소유자 토큰 파일 이름.
pub const OWNER_TOKEN_FILE: &str = "owner.token";
/// 데이터 폴더 설정 파일(앱 설정에서 폴더를 바꾸면 씀). 설치 파일·zip에는 넣지 않아 업데이트해도 그대로입니다.
pub const DATA_DIR_FILE: &str = "data-dir.json";
/// 데스크톱 앱의 기본 데이터 폴더(실행 파일 옆).
pub const DEFAULT_DATA_DIR: &str = "Data";
/// 데스크톱 앱 실행 파일. 이것과 같은 폴더의 데몬·CLI도 그 폴더 기준의 데이터를 씁니다.
const DESKTOP_EXE: &str = "yangpa-desktop.exe";
/// 토큰 파일 최대 크기(잘못된 파일을 통째로 읽지 않게).
const MAX_TOKEN_FILE: u64 = 4096;

/// Unix 소켓 경로 길이 상한(`sun_path`; macOS 104, Linux 108, 끝 NUL 포함).
#[cfg(target_os = "linux")]
const MAX_SOCKET_PATH: usize = 107;
#[cfg(not(target_os = "linux"))]
const MAX_SOCKET_PATH: usize = 103;

/// 환경 변수 조회 함수.
pub fn process_env(key: &str) -> Option<OsString> {
    std::env::var_os(key).filter(|v| !v.is_empty())
}

/// 로컬 API 엔드포인트(Unix 소켓 경로 또는 Windows named pipe 경로).
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Endpoint(PathBuf);

impl Endpoint {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Endpoint(path.into())
    }

    /// 디렉터리 하나에 딸린 엔드포인트. Unix는 `<dir>/yangpad.sock`, Windows는 named pipe가 파일 경로를 쓸 수 없어
    /// 디렉터리 경로에서 만든 고유 이름(`\\.\pipe\yangpad-<해시>`)을 씁니다. 한 사용자가 데이터 경로를 달리해
    /// 데몬을 여러 개 띄울 때(테스트 포함) 서로 겹치지 않게 합니다.
    pub fn in_dir(dir: &Path) -> Self {
        #[cfg(windows)]
        {
            Endpoint(PathBuf::from(format!(
                r"\\.\pipe\yangpad-{:016x}",
                fnv1a64(dir.as_os_str().as_encoded_bytes())
            )))
        }
        #[cfg(not(windows))]
        {
            Endpoint(dir.join("yangpad.sock"))
        }
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    /// Windows named pipe(`\\.\pipe\…`)인지.
    pub fn is_named_pipe(&self) -> bool {
        self.0.to_string_lossy().starts_with(r"\\.\pipe\")
    }

    /// interprocess 이름으로 바꿉니다. Unix 소켓 경로가 너무 길면 알기 쉬운 오류를 냅니다.
    pub(crate) fn to_name(&self) -> io::Result<Name<'_>> {
        if !self.is_named_pipe() && self.0.as_os_str().len() > MAX_SOCKET_PATH {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "소켓 경로가 너무 깁니다({}바이트, 최대 {MAX_SOCKET_PATH}): {}",
                    self.0.as_os_str().len(),
                    self
                ),
            ));
        }
        self.0.as_os_str().to_fs_name::<GenericFilePath>()
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0.display())
    }
}

impl fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Endpoint({})", self.0.display())
    }
}

impl FromStr for Endpoint {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        if s.is_empty() {
            return Err("엔드포인트가 비었습니다".into());
        }
        Ok(Endpoint(PathBuf::from(s)))
    }
}

fn not_found(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, msg.to_owned())
}

/// 이 실행 파일이 있는 폴더.
pub fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(Path::to_path_buf))
}

/// 윈도 데스크톱 앱 묶음(설치판·포터블: 실행 파일 옆에 `yangpa-desktop.exe`가 있음)이면 그 폴더.
pub fn desktop_dir() -> Option<PathBuf> {
    let dir = exe_dir()?;
    (cfg!(windows) && dir.join(DESKTOP_EXE).is_file()).then_some(dir)
}

/// 앱 폴더에 파일을 만들 수 있는지(Program Files에 깐 MSI는 못 함). 프로세스마다 한 번만 봅니다.
fn writable(dir: &Path) -> bool {
    static WRITABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *WRITABLE.get_or_init(|| {
        let probe = dir.join(format!(".yangpa-write-test-{}", std::process::id()));
        let ok = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&probe)
            .is_ok();
        let _ = std::fs::remove_file(&probe);
        ok
    })
}

/// 데이터 폴더 설정 파일 위치: 앱 폴더에 쓸 수 있으면 그 안, 아니면 OS 기본 데이터 경로 옆.
pub fn data_dir_file_in(app: &Path, writable: bool, system: Option<&Path>) -> Option<PathBuf> {
    if writable {
        Some(app.join(DATA_DIR_FILE))
    } else {
        Some(system?.parent()?.join(DATA_DIR_FILE))
    }
}

/// 앱의 기본 데이터 폴더: 앱 폴더 옆 `Data`, 앱 폴더에 쓸 수 없으면 OS 기본 경로.
pub fn default_data_dir_in(app: &Path, writable: bool, system: Option<&Path>) -> Option<PathBuf> {
    if writable {
        Some(app.join(DEFAULT_DATA_DIR))
    } else {
        system.map(Path::to_path_buf)
    }
}

/// 데스크톱 앱의 데이터 폴더. 설정 파일의 `data_dir`(상대 경로는 앱 폴더 기준)가 먼저이고, 없으면 기본 폴더입니다.
/// 기본 `Data`에 양파 데이터가 없고 `system`(예전 판이 쓰던 OS 기본 경로)에 있으면 옮기지 않고 그곳을 그대로 씁니다.
pub fn desktop_data_dir_in(app: &Path, writable: bool, system: Option<&Path>) -> Option<PathBuf> {
    let set = data_dir_file_in(app, writable, system)
        .and_then(|f| std::fs::read(f).ok())
        .filter(|b| b.len() <= MAX_TOKEN_FILE as usize)
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .and_then(|v| {
            v.get("data_dir")?
                .as_str()
                .map(str::trim)
                .map(str::to_owned)
        })
        .filter(|d| !d.is_empty());
    if let Some(d) = set {
        return Some(app.join(d));
    }
    let data = default_data_dir_in(app, writable, system)?;
    match system {
        Some(old)
            if !data.join(OWNER_TOKEN_FILE).exists() && old.join(OWNER_TOKEN_FILE).is_file() =>
        {
            Some(old.to_path_buf())
        }
        _ => Some(data),
    }
}

/// 이 실행 파일이 데스크톱 앱 묶음이면 그 데이터 폴더.
pub fn desktop_data_dir() -> Option<PathBuf> {
    let app = desktop_dir()?;
    desktop_data_dir_in(&app, writable(&app), system_data_dir().ok().as_deref())
}

/// 이 실행 파일이 데스크톱 앱 묶음이면 (기본 데이터 폴더, 설정 파일 위치).
pub fn desktop_defaults() -> Option<(PathBuf, PathBuf)> {
    let app = desktop_dir()?;
    let (w, system) = (writable(&app), system_data_dir().ok());
    Some((
        default_data_dir_in(&app, w, system.as_deref())?,
        data_dir_file_in(&app, w, system.as_deref())?,
    ))
}

/// 데이터 경로.
pub fn data_dir() -> io::Result<PathBuf> {
    data_dir_with(process_env)
}

pub fn data_dir_with(env: impl Fn(&str) -> Option<OsString>) -> io::Result<PathBuf> {
    if let Some(d) = env(ENV_DATA_DIR) {
        return Ok(PathBuf::from(d));
    }
    if let Some(d) = desktop_data_dir() {
        return Ok(d);
    }
    system_data_dir()
}

/// OS 기본 데이터 경로(윈도 `%APPDATA%\yangpa\data`). 데스크톱 앱 묶음이 아닐 때와 예전 판이 씀.
pub fn system_data_dir() -> io::Result<PathBuf> {
    directories::ProjectDirs::from("", "", "yangpa")
        .map(|p| p.data_dir().to_path_buf())
        .ok_or_else(|| {
            not_found(
                "홈 디렉터리를 찾을 수 없어 데이터 경로를 정할 수 없습니다(YANGPA_DATA_DIR 지정)",
            )
        })
}

/// 기본 엔드포인트(`YANGPA_ENDPOINT` 반영).
pub fn default_endpoint() -> io::Result<Endpoint> {
    default_endpoint_with(process_env)
}

pub fn default_endpoint_with(env: impl Fn(&str) -> Option<OsString>) -> io::Result<Endpoint> {
    if let Some(e) = env(ENV_ENDPOINT) {
        return Ok(Endpoint(PathBuf::from(e)));
    }
    // 앱이 OS 기본 경로를 쓰면(예전 판 데이터를 이어 씀) 예전처럼 다른 앱·CLI와 같은 엔드포인트로.
    if env(ENV_DATA_DIR).is_none()
        && let Some(d) = desktop_data_dir()
        && system_data_dir().ok().as_ref() != Some(&d)
    {
        return Ok(data_dir_endpoint(&d));
    }
    platform_endpoint(&env)
}

/// 데스크톱 앱 데이터 폴더의 엔드포인트. 윈도는 같은 폴더를 대소문자를 달리 적어도 같은 이름이 되게 소문자로 맞춥니다
/// (앱이 띄운 데몬과 사용자가 셸에서 띄운 CLI가 같은 파이프를 찾게).
pub fn data_dir_endpoint(data_dir: &Path) -> Endpoint {
    if cfg!(windows) {
        Endpoint::in_dir(Path::new(&data_dir.to_string_lossy().to_lowercase()))
    } else {
        Endpoint::in_dir(data_dir)
    }
}

#[cfg(unix)]
fn platform_endpoint(env: &impl Fn(&str) -> Option<OsString>) -> io::Result<Endpoint> {
    if let Some(rt) = env("XDG_RUNTIME_DIR") {
        let rt = PathBuf::from(rt);
        // 상대 경로는 XDG 규약 위반이라 무시합니다.
        if rt.is_absolute() {
            return Ok(Endpoint(rt.join("yangpa").join(SOCKET_FILE)));
        }
    }
    Ok(Endpoint(data_dir_with(env)?.join(SOCKET_FILE)))
}

#[cfg(windows)]
fn platform_endpoint(env: &impl Fn(&str) -> Option<OsString>) -> io::Result<Endpoint> {
    let user = env("USERNAME")
        .map(|u| u.to_string_lossy().into_owned())
        .unwrap_or_default();
    Ok(Endpoint(PathBuf::from(format!(
        r"\\.\pipe\yangpad-{}",
        sanitize_pipe_user(&user)
    ))))
}

/// 프로세스·빌드가 달라도 같은 값이 나오는 64비트 FNV-1a(`DefaultHasher`는 릴리스마다 바뀔 수 있음).
#[cfg_attr(not(windows), allow(dead_code))]
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// pipe 이름에 쓸 수 있는 문자만 남깁니다.
#[cfg_attr(not(windows), allow(dead_code))]
fn sanitize_pipe_user(user: &str) -> String {
    let s: String = user
        .chars()
        .filter(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .take(64)
        .collect();
    if s.is_empty() { "default".into() } else { s }
}

/// 명시 인자 → `YANGPA_ENDPOINT` → 기본값 순으로 정합니다.
pub fn resolve_endpoint(explicit: Option<&str>) -> io::Result<Endpoint> {
    match explicit.map(str::trim).filter(|s| !s.is_empty()) {
        Some(e) => Ok(Endpoint(PathBuf::from(e))),
        None => default_endpoint(),
    }
}

/// 소유자 토큰 파일 경로(`<데이터 경로>/owner.token`).
pub fn owner_token_path() -> io::Result<PathBuf> {
    Ok(data_dir()?.join(OWNER_TOKEN_FILE))
}

pub fn owner_token_path_with(env: impl Fn(&str) -> Option<OsString>) -> io::Result<PathBuf> {
    Ok(data_dir_with(env)?.join(OWNER_TOKEN_FILE))
}

/// 토큰 파일을 읽습니다(앞뒤 공백 제거).
pub fn read_token_file(path: &Path) -> io::Result<SecretString> {
    use std::io::Read;
    let f = std::fs::File::open(path)?;
    let mut s = String::new();
    f.take(MAX_TOKEN_FILE).read_to_string(&mut s)?;
    let token = SecretString::new(s.trim());
    zeroize::Zeroize::zeroize(&mut s);
    if token.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("토큰 파일이 비었습니다: {}", path.display()),
        ));
    }
    Ok(token)
}

/// 토큰 파일을 씁니다. Unix에서는 0600, 상위 디렉터리를 새로 만들면 0700입니다.
/// 같은 디렉터리의 임시 파일에 쓴 뒤 이름을 바꿔, 읽는 쪽이 반쯤 쓰인 파일을 보지 않게 합니다.
pub fn write_token_file(path: &Path, token: &str) -> io::Result<()> {
    use std::io::Write;
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    create_private_dir(dir)?;
    let tmp = dir.join(format!(".{}.tmp-{}", OWNER_TOKEN_FILE, std::process::id()));
    // 이전 실행이 남긴 임시 파일(권한이 넓을 수 있음)이나 미리 심어 둔 심볼릭 링크를 그대로 열면
    // 0600 모드가 적용되지 않으므로, 지우고 새 파일로만(O_EXCL) 만듭니다.
    match std::fs::remove_file(&tmp) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let result = (|| {
        let mut f = opts.open(&tmp)?;
        f.write_all(token.as_bytes())?;
        f.write_all(b"\n")?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// 디렉터리를 만듭니다(Unix에서 새로 만드는 단계는 0700).
pub fn create_private_dir(dir: &Path) -> io::Result<()> {
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        b.mode(0o700);
    }
    b.create(dir)
}

/// 명시 토큰 → `YANGPA_TOKEN` → 소유자 토큰 파일 순으로 토큰을 정합니다.
pub fn resolve_token(explicit: Option<&str>) -> io::Result<SecretString> {
    resolve_token_with(explicit, process_env)
}

pub fn resolve_token_with(
    explicit: Option<&str>,
    env: impl Fn(&str) -> Option<OsString>,
) -> io::Result<SecretString> {
    if let Some(t) = explicit.map(str::trim).filter(|s| !s.is_empty()) {
        return Ok(SecretString::new(t));
    }
    if let Some(t) = env(ENV_TOKEN) {
        let t = t.to_string_lossy();
        let t = t.trim();
        if !t.is_empty() {
            return Ok(SecretString::new(t));
        }
    }
    let path = owner_token_path_with(&env)?;
    read_token_file(&path).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!(
                "API 토큰이 없습니다: --token, {ENV_TOKEN}, 또는 소유자 토큰 파일({})이 필요합니다 ({e})",
                path.display()
            ),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> + use<> {
        let map: HashMap<String, OsString> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), OsString::from(v)))
            .collect();
        move |k: &str| map.get(k).cloned()
    }

    #[test]
    fn endpoint_override_wins() {
        let e = default_endpoint_with(env_of(&[
            ("YANGPA_ENDPOINT", "/tmp/x.sock"),
            ("XDG_RUNTIME_DIR", "/run/user/1"),
        ]))
        .unwrap();
        assert_eq!(e.path(), Path::new("/tmp/x.sock"));
    }

    #[cfg(unix)]
    #[test]
    fn unix_default_endpoint() {
        let e = default_endpoint_with(env_of(&[("XDG_RUNTIME_DIR", "/run/user/1000")])).unwrap();
        assert_eq!(e.path(), Path::new("/run/user/1000/yangpa/yangpad.sock"));
        // XDG_RUNTIME_DIR가 없으면 데이터 경로.
        let e = default_endpoint_with(env_of(&[("YANGPA_DATA_DIR", "/data/yp")])).unwrap();
        assert_eq!(e.path(), Path::new("/data/yp/yangpad.sock"));
        // 상대 경로 XDG_RUNTIME_DIR는 무시.
        let e = default_endpoint_with(env_of(&[
            ("XDG_RUNTIME_DIR", "rel"),
            ("YANGPA_DATA_DIR", "/d"),
        ]))
        .unwrap();
        assert_eq!(e.path(), Path::new("/d/yangpad.sock"));
    }

    #[test]
    fn data_dir_and_token_path() {
        assert_eq!(
            data_dir_with(env_of(&[("YANGPA_DATA_DIR", "/x/y")])).unwrap(),
            PathBuf::from("/x/y")
        );
        assert_eq!(
            owner_token_path_with(env_of(&[("YANGPA_DATA_DIR", "/x/y")])).unwrap(),
            PathBuf::from("/x/y/owner.token")
        );
    }

    #[test]
    fn desktop_data_dir_from_file() {
        let app = tempfile::tempdir().unwrap();
        // 설정 파일이 없으면 기본 Data.
        assert_eq!(
            desktop_data_dir_in(app.path(), true, None),
            Some(app.path().join("Data"))
        );
        let file = app.path().join(DATA_DIR_FILE);
        std::fs::write(&file, br#"{"data_dir":"D:/yp"}"#).unwrap();
        assert_eq!(
            desktop_data_dir_in(app.path(), true, None),
            Some(app.path().join("D:/yp"))
        );
        std::fs::write(&file, br#"{"data_dir":"sub/x"}"#).unwrap();
        assert_eq!(
            desktop_data_dir_in(app.path(), true, None),
            Some(app.path().join("sub/x"))
        );
        // 망가진 파일은 기본 폴더.
        std::fs::write(&file, b"{not json").unwrap();
        assert_eq!(
            desktop_data_dir_in(app.path(), true, None),
            Some(app.path().join("Data"))
        );
    }

    #[test]
    fn desktop_keeps_legacy_data_until_set() {
        let app = tempfile::tempdir().unwrap();
        let old = tempfile::tempdir().unwrap();
        let old_data = old.path().join("data");
        std::fs::create_dir_all(&old_data).unwrap();
        // 예전 경로가 비었으면 기본 Data.
        assert_eq!(
            desktop_data_dir_in(app.path(), true, Some(&old_data)),
            Some(app.path().join("Data"))
        );
        // 예전 판 데이터가 있으면 옮기지 않고 그곳을 씀.
        std::fs::write(old_data.join(OWNER_TOKEN_FILE), b"t").unwrap();
        assert_eq!(
            desktop_data_dir_in(app.path(), true, Some(&old_data)),
            Some(old_data.clone())
        );
        // Data에 이미 데이터가 있으면 Data.
        std::fs::create_dir_all(app.path().join("Data")).unwrap();
        std::fs::write(app.path().join("Data").join(OWNER_TOKEN_FILE), b"t").unwrap();
        assert_eq!(
            desktop_data_dir_in(app.path(), true, Some(&old_data)),
            Some(app.path().join("Data"))
        );
        // 설정에서 정한 폴더가 늘 우선.
        std::fs::write(app.path().join(DATA_DIR_FILE), br#"{"data_dir":"Data"}"#).unwrap();
        std::fs::remove_file(app.path().join("Data").join(OWNER_TOKEN_FILE)).unwrap();
        assert_eq!(
            desktop_data_dir_in(app.path(), true, Some(&old_data)),
            Some(app.path().join("Data"))
        );
    }

    #[test]
    fn read_only_app_dir_uses_system_dir_and_keeps_setting_beside_it() {
        // Program Files에 깐 MSI: 기본은 OS 경로, 설정 파일은 그 옆.
        let app = tempfile::tempdir().unwrap();
        let old = tempfile::tempdir().unwrap();
        let sys = old.path().join("data");
        assert_eq!(
            desktop_data_dir_in(app.path(), false, Some(&sys)),
            Some(sys.clone())
        );
        let file = data_dir_file_in(app.path(), false, Some(&sys)).unwrap();
        assert_eq!(file, old.path().join(DATA_DIR_FILE));
        std::fs::write(&file, br#"{"data_dir":"E:/yp"}"#).unwrap();
        assert_eq!(
            desktop_data_dir_in(app.path(), false, Some(&sys)),
            Some(app.path().join("E:/yp"))
        );
        assert_eq!(desktop_data_dir_in(app.path(), false, None), None);
    }

    #[test]
    fn data_dir_endpoint_ignores_case_on_windows() {
        let a = data_dir_endpoint(Path::new("C:/Apps/Yangpa/Data"));
        let b = data_dir_endpoint(Path::new("c:/apps/yangpa/data"));
        if cfg!(windows) {
            assert_eq!(a, b);
        }
        assert_ne!(a, data_dir_endpoint(Path::new("C:/Apps/Other/Data")));
    }

    #[test]
    fn in_dir_is_stable_and_distinct_per_dir() {
        let a = Endpoint::in_dir(Path::new("/data/a"));
        assert_eq!(a, Endpoint::in_dir(Path::new("/data/a")));
        assert_ne!(a, Endpoint::in_dir(Path::new("/data/b")));
        #[cfg(windows)]
        assert!(a.is_named_pipe());
        #[cfg(not(windows))]
        assert_eq!(a.path(), Path::new("/data/a/yangpad.sock"));
        assert_ne!(fnv1a64(b"/data/a"), fnv1a64(b"/data/b"));
    }

    #[test]
    fn pipe_user_sanitized() {
        assert_eq!(sanitize_pipe_user("세영 Oh\\..x"), "세영Oh..x");
        assert_eq!(sanitize_pipe_user(""), "default");
    }

    #[test]
    fn token_file_roundtrip_and_resolution() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let path = data.join(OWNER_TOKEN_FILE);
        write_token_file(&path, "tok-123").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                std::fs::metadata(&data).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        assert_eq!(read_token_file(&path).unwrap().expose(), "tok-123");

        let data_str = data.to_str().unwrap();
        let env = env_of(&[("YANGPA_DATA_DIR", data_str)]);
        assert_eq!(resolve_token_with(None, &env).unwrap().expose(), "tok-123");
        assert_eq!(
            resolve_token_with(Some(" given "), &env).unwrap().expose(),
            "given"
        );
        let env2 = env_of(&[("YANGPA_DATA_DIR", data_str), ("YANGPA_TOKEN", "from-env")]);
        assert_eq!(resolve_token_with(None, env2).unwrap().expose(), "from-env");
        let missing = env_of(&[("YANGPA_DATA_DIR", "/nonexistent/yangpa")]);
        assert!(resolve_token_with(None, missing).is_err());

        std::fs::write(&path, "  \n").unwrap();
        assert!(read_token_file(&path).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn stale_tmp_file_does_not_leak_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(OWNER_TOKEN_FILE);
        let tmp = dir
            .path()
            .join(format!(".{}.tmp-{}", OWNER_TOKEN_FILE, std::process::id()));
        // 넓은 권한의 임시 파일과, 다른 곳을 가리키는 심볼릭 링크를 차례로 남겨 둡니다.
        std::fs::write(&tmp, "old").unwrap();
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_token_file(&path, "tok-a").unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        let target = dir.path().join("elsewhere");
        std::fs::write(&target, "").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::os::unix::fs::symlink(&target, &tmp).unwrap();
        write_token_file(&path, "tok-b").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "");
        assert_eq!(read_token_file(&path).unwrap().expose(), "tok-b");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn long_socket_path_rejected() {
        let long = format!("/tmp/{}/yangpad.sock", "a".repeat(200));
        let e = Endpoint::new(long);
        assert_eq!(e.to_name().unwrap_err().kind(), io::ErrorKind::InvalidInput);
        assert!("".parse::<Endpoint>().is_err());
    }
}
