//! 데몬 연결 관리: 떠 있는 `yangpad`에 붙고, 없으면 한 번 띄운 뒤 붙습니다.
//! 연결되면 소유자 토큰으로 인증하고 모든 이벤트를 구독해 웹뷰로 넘깁니다. 끊기면 다시 붙습니다.

use std::collections::HashMap;
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use futures::StreamExt;
use parking_lot::{Mutex, RwLock};
use serde::Serialize;
use serde_json::{Value, json};
use tauri::{AppHandle, Emitter, Runtime};
use tokio::sync::Notify;
use tracing::{debug, info, warn};
use yangpa_core::UserId;
use yangpa_core::ids::SpaceId;
use yangpa_core::model::SpaceKind;
use yangpa_rpc::api::{IdentityView, Notification, SpaceSummary};
use yangpa_rpc::{ClientError, RpcClient, Subscription, endpoint};

use crate::error::CmdError;
use crate::notify::Notifier;

/// 웹뷰로 알림 묶음을 보내는 이벤트 이름.
pub const EVENTS: &str = "yangpa-events";
/// 연결 상태 이벤트 이름.
pub const STATE_EVENT: &str = "yangpa-daemon";
/// 메인 창 라벨.
pub const MAIN_WINDOW: &str = "main";

/// 데몬 실행 파일 경로를 바꾸는 환경 변수.
pub const ENV_DAEMON_BIN: &str = "YANGPA_DAEMON_BIN";
/// `1`이면 데몬을 띄우지 않고 붙기만 합니다(개발·테스트용).
pub const ENV_NO_SPAWN: &str = "YANGPA_NO_SPAWN";
/// 데몬 실행 파일 이름(확장자 제외).
const DAEMON_BIN: &str = "yangpad";
/// 이 앱의 타깃 트리플(tauri-build가 넣음). 번들 전 사이드카 이름(`yangpad-<트리플>`)에 씁니다.
const TARGET_TRIPLE: &str = env!("TAURI_ENV_TARGET_TRIPLE");

/// 앱이 끝날 때 데몬의 종료 응답을 이만큼만 기다립니다(종료를 오래 붙잡지 않게).
pub const STOP_TIMEOUT: Duration = Duration::from_secs(3);
/// 띄운 데몬이 지켜볼 앱 PID(`yangpad --parent-pid`).
#[cfg(not(target_os = "android"))]
const PARENT_PID_ENV: &str = "YANGPA_PARENT_PID";
/// 요청 하나의 응답 상한. Tor 부트스트랩·복원처럼 오래 걸리는 호출도 있어 넉넉히 둡니다.
const CALL_TIMEOUT: Duration = Duration::from_secs(180);
/// 띄운 데몬이 소켓을 열 때까지 기다리는 시간.
const SPAWN_WAIT: Duration = Duration::from_secs(20);
const BACKOFF_MIN: Duration = Duration::from_millis(500);
const BACKOFF_MAX: Duration = Duration::from_secs(10);
/// 이만큼 유지된 연결이 끊기면 곧바로 다시 붙습니다(데몬 재시작). 더 짧으면 대기 간격을 늘려 가며 붙습니다.
const STABLE_CONNECTION: Duration = Duration::from_secs(5);
/// 메서드 이름 최대 길이.
const METHOD_MAX: usize = 64;
/// 웹뷰가 직접 부르면 안 되는 메서드(인증·구독은 이 프로세스가 관리).
const BLOCKED_METHODS: &[&str] = &["auth.hello", "events.subscribe", "events.unsubscribe"];

/// 웹뷰에 보이는 연결 상태(프런트엔드 `DaemonState`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum DaemonState {
    Connecting,
    Spawning,
    Connected {
        #[serde(skip_serializing_if = "Option::is_none")]
        version: Option<String>,
    },
    Error {
        message: String,
    },
}

/// 연결과 알림 판단에 필요한 작은 캐시(내용 없음: 내 ID, 공간 종류).
pub struct Daemon {
    client: RwLock<Option<RpcClient>>,
    state: Mutex<DaemonState>,
    wake: Notify,
    me: Mutex<Option<UserId>>,
    kinds: Mutex<HashMap<SpaceId, SpaceKind>>,
    /// 앱이 끝나는 중이면 데몬을 다시 띄우지 않습니다(방금 끈 데몬을 감독 작업이 되살리지 않게).
    exiting: AtomicBool,
    pub(crate) notifier: Notifier,
}

impl Default for Daemon {
    fn default() -> Self {
        Daemon {
            client: RwLock::new(None),
            state: Mutex::new(DaemonState::Connecting),
            wake: Notify::new(),
            me: Mutex::new(None),
            kinds: Mutex::new(HashMap::new()),
            exiting: AtomicBool::new(false),
            notifier: Notifier::default(),
        }
    }
}

impl Daemon {
    /// 지금 연결된 클라이언트.
    pub fn client(&self) -> Option<RpcClient> {
        self.client
            .read()
            .as_ref()
            .filter(|c| !c.is_closed())
            .cloned()
    }

    /// 시험용: 클라이언트를 직접 넣습니다.
    #[cfg(test)]
    pub(crate) fn set_client(&self, c: RpcClient) {
        *self.client.write() = Some(c);
    }

    pub fn state(&self) -> DaemonState {
        self.state.lock().clone()
    }

    /// 다시 붙기 대기를 건너뜁니다.
    pub fn wake(&self) {
        self.wake.notify_one();
    }

    pub fn me(&self) -> Option<UserId> {
        *self.me.lock()
    }

    pub fn space_kind(&self, space: &SpaceId) -> Option<SpaceKind> {
        self.kinds.lock().get(space).copied()
    }

    fn set_state<R: Runtime>(&self, app: &AppHandle<R>, s: DaemonState) {
        {
            let mut cur = self.state.lock();
            if *cur == s {
                return;
            }
            *cur = s.clone();
        }
        if let Err(e) = app.emit_to(MAIN_WINDOW, STATE_EVENT, &s) {
            debug!("상태 이벤트를 보내지 못했습니다: {e}");
        }
    }

    /// 웹뷰의 `rpc_call`.
    pub async fn call(&self, method: &str, params: Option<Value>) -> Result<Value, CmdError> {
        validate_method(method)?;
        let params = normalize_params(params)?;
        let client = self.client().ok_or_else(CmdError::unavailable)?;
        match client.call_value(method, params).await {
            Ok(v) => Ok(v),
            Err(e) => {
                if matches!(e, ClientError::Closed) {
                    self.wake();
                }
                Err(e.into())
            }
        }
    }

    /// 앱이 끝날 때 붙어 있던 데몬도 끕니다(앱을 닫으면 `yangpad`도 끝남). 앱이 데몬을 띄우지 않게 한
    /// 설정(`YANGPA_NO_SPAWN=1`)이면 데몬을 따로 관리하는 것이므로 그대로 둡니다. 끄라고 했으면 true.
    pub async fn stop_on_exit(&self, spawn_allowed: bool) -> bool {
        self.exiting.store(true, Ordering::SeqCst);
        if !spawn_allowed {
            return false;
        }
        let Some(client) = self.client() else {
            return false;
        };
        let call = client.call_value("system.shutdown", json!({}));
        match tokio::time::timeout(STOP_TIMEOUT, call).await {
            Ok(Ok(_)) => {
                info!("앱을 끝내며 데몬을 껐습니다");
                true
            }
            Ok(Err(e)) => {
                warn!("앱을 끝내며 데몬을 끄지 못했습니다: {e}");
                false
            }
            Err(_) => {
                warn!("데몬 종료 응답이 늦어 기다리지 않고 끝냅니다");
                false
            }
        }
    }

    /// 내 ID와 공간 종류를 다시 읽습니다(알림 판단용). 실패해도 다음 기회에 다시 합니다.
    pub async fn refresh_cache(&self) {
        let Some(client) = self.client() else { return };
        if let Ok(id) = client
            .call::<_, IdentityView>("identity.get", json!({}))
            .await
        {
            *self.me.lock() = Some(id.user);
        }
        if let Ok(list) = client
            .call::<_, Vec<SpaceSummary>>("space.list", json!({}))
            .await
        {
            let mut kinds = self.kinds.lock();
            kinds.clear();
            kinds.extend(list.into_iter().map(|s| (s.space, s.kind)));
        }
    }
}

/// 메서드 이름 검사: `영소문자·숫자·_` 조각을 점으로 이은 형태, 길이 제한, 금지 목록.
pub fn validate_method(method: &str) -> Result<(), CmdError> {
    let ok_shape = !method.is_empty()
        && method.len() <= METHOD_MAX
        && method.contains('.')
        && method.split('.').all(|part| {
            !part.is_empty()
                && part.starts_with(|c: char| c.is_ascii_lowercase())
                && part
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        });
    if !ok_shape {
        return Err(CmdError::new(
            crate::error::INVALID_REQUEST,
            "메서드 이름이 잘못됐습니다",
        ));
    }
    if BLOCKED_METHODS.contains(&method) {
        return Err(CmdError::new(
            crate::error::INVALID_REQUEST,
            "이 메서드는 앱이 직접 관리합니다",
        ));
    }
    Ok(())
}

/// 인자는 객체만(없거나 null이면 빈 객체).
pub fn normalize_params(params: Option<Value>) -> Result<Value, CmdError> {
    match params {
        None | Some(Value::Null) => Ok(json!({})),
        Some(v @ Value::Object(_)) => Ok(v),
        Some(_) => Err(CmdError::invalid_params("인자는 객체여야 합니다")),
    }
}

/// 연결 실패가 "데몬이 안 떠 있음"인지(띄워 볼 만한지). 남은 소켓 파일은 ConnectionRefused로 옵니다.
fn is_not_running_io(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::NotFound
            | io::ErrorKind::ConnectionRefused
            | io::ErrorKind::AddrNotAvailable
    )
}

/// 데몬 실행 파일 후보: 환경 변수 → 앱 실행 파일 옆(설치본의 사이드카, 개발 중에는 같은 target 폴더에 빌드한
/// yangpad) → 이름을 바꾸기 전 사이드카(`yangpad-<트리플>`) → (디버그 빌드만) 워크스페이스 target 폴더 → PATH.
pub fn daemon_candidates(
    env: impl Fn(&str) -> Option<OsString>,
    exe_dir: Option<&Path>,
) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(p) = env(ENV_DAEMON_BIN).filter(|v| !v.is_empty()) {
        out.push(PathBuf::from(p));
        return out;
    }
    let suffix = std::env::consts::EXE_SUFFIX;
    let name = format!("{DAEMON_BIN}{suffix}");
    if let Some(dir) = exe_dir {
        out.push(dir.join(&name));
        out.push(dir.join(format!("{DAEMON_BIN}-{TARGET_TRIPLE}{suffix}")));
    }
    // 개발 빌드만: 배포본이 빌드한 사람의 소스 경로를 뒤지지 않게 합니다.
    if cfg!(debug_assertions) {
        for dir in dev_target_dirs(&env) {
            for profile in ["debug", "release"] {
                let p = dir.join(profile).join(&name);
                if !out.contains(&p) {
                    out.push(p);
                }
            }
        }
    }
    // PATH에서 찾도록 이름만 둡니다.
    out.push(PathBuf::from(name));
    out
}

/// 개발 중 `cargo build -p yangpa-daemon`이 yangpad를 놓는 곳: CARGO_TARGET_DIR, 이 저장소의 target.
fn dev_target_dirs(env: &impl Fn(&str) -> Option<OsString>) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(t) = env("CARGO_TARGET_DIR").filter(|v| !v.is_empty()) {
        dirs.push(PathBuf::from(t));
    }
    // 이 크레이트는 <워크스페이스>/apps/desktop/src-tauri에 있습니다.
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    if let Some(ws) = manifest.ancestors().nth(3) {
        dirs.push(ws.join("target"));
    }
    dirs
}

/// 데몬을 앱과 분리된 프로세스로 띄웁니다(앱이 끝날 때는 RPC로 끔, [`Daemon::stop_on_exit`]). 입출력은 버립니다.
/// 앱이 강제로 꺼지면 그 RPC가 없으므로, 앱의 PID를 넘겨 데몬이 앱 프로세스가 끝나는 것을 보고 스스로 끝나게 합니다
/// (인자가 아닌 환경 변수라 이 설정을 모르는 옛 `yangpad`도 그대로 뜸).
#[cfg(not(target_os = "android"))]
fn spawn_daemon<R: Runtime>(_app: &AppHandle<R>) -> io::Result<PathBuf> {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf));
    let mut last = io::Error::new(
        io::ErrorKind::NotFound,
        "yangpad 실행 파일을 찾지 못했습니다",
    );
    for cand in daemon_candidates(endpoint::process_env, exe_dir.as_deref()) {
        // 경로로 준 후보는 있을 때만 시도합니다(PATH 후보는 이름 하나).
        if cand.components().count() > 1 && !cand.is_file() {
            continue;
        }
        let mut cmd = std::process::Command::new(&cand);
        cmd.env(PARENT_PID_ENV, std::process::id().to_string())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // 앱의 프로세스 그룹과 분리해 터미널 시그널이 같이 가지 않게 합니다.
            cmd.process_group(0);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // 창 없는 콘솔을 줍니다. DETACHED_PROCESS를 같이 주면 콘솔이 아예 없어, 데몬이 띄우는 콘솔 프로그램(Tor
            // 브리지의 lyrebird)마다 새 콘솔 창이 뜹니다. 창 없는 콘솔은 자식이 물려받아 창이 뜨지 않습니다.
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        match cmd.spawn() {
            Ok(mut child) => {
                // 좀비가 남지 않게 끝나기를 따로 기다립니다.
                std::thread::Builder::new()
                    .name("yangpad-reaper".into())
                    .spawn(move || {
                        let _ = child.wait();
                    })
                    .ok();
                return Ok(cand);
            }
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// Android는 데몬을 이 프로세스 안에서 돌립니다.
#[cfg(target_os = "android")]
fn spawn_daemon<R: Runtime>(app: &AppHandle<R>) -> io::Result<PathBuf> {
    crate::embedded::start(app.clone())
}

/// 연결 실패 종류. 데몬이 없을 때만 띄우고, 토큰이 아직 없으면(갓 띄운 데몬) 조금 기다립니다.
#[derive(Debug)]
enum ConnectError {
    NotRunning(io::Error),
    Token(io::Error),
    Other(ClientError),
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectError::NotRunning(e) => write!(
                f,
                "데몬(yangpad)이 실행 중이 아닙니다 ({e}). {ENV_DAEMON_BIN}로 실행 파일 경로를 지정할 수 있습니다"
            ),
            ConnectError::Token(e) => write!(f, "API 토큰을 읽지 못했습니다: {e}"),
            ConnectError::Other(e) => write!(f, "{e}"),
        }
    }
}

/// 연결 + 인증 + 구독.
async fn connect_once() -> Result<(RpcClient, Subscription), ConnectError> {
    let ep =
        endpoint::default_endpoint().map_err(|e| ConnectError::Other(ClientError::Connect(e)))?;
    let client = match RpcClient::connect(&ep).await {
        Ok(c) => c.with_timeout(CALL_TIMEOUT),
        Err(ClientError::Connect(e)) if is_not_running_io(&e) => {
            return Err(ConnectError::NotRunning(e));
        }
        Err(e) => return Err(ConnectError::Other(e)),
    };
    // 토큰은 데몬이 처음 뜰 때 만들므로 연결한 뒤에 읽습니다.
    let token = endpoint::resolve_token(None).map_err(ConnectError::Token)?;
    let hello = client
        .hello(token.expose())
        .await
        .map_err(ConnectError::Other)?;
    drop(token);
    if hello.automation {
        warn!("자동화 토큰으로 붙었습니다. 일부 작업은 사람 확인이 필요합니다");
    }
    let sub = client
        .subscribe(Vec::new())
        .await
        .map_err(ConnectError::Other)?;
    Ok((client, sub))
}

/// 띄운 데몬이 준비될 때까지 붙어 봅니다.
async fn connect_until(deadline: Instant) -> Result<(RpcClient, Subscription), ConnectError> {
    loop {
        match connect_once().await {
            Ok(v) => return Ok(v),
            Err(ConnectError::NotRunning(_) | ConnectError::Token(_))
                if Instant::now() < deadline =>
            {
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
            Err(e) => return Err(e),
        }
    }
}

/// 앱이 데몬을 띄워도 되는지(`YANGPA_NO_SPAWN`이 `1`이 아님). 띄울 수 있으면 끌 때도 같이 끕니다.
pub fn spawn_allowed() -> bool {
    endpoint::process_env(ENV_NO_SPAWN).is_none_or(|v| v != "1")
}

/// 연결을 유지하는 감독 작업. 앱이 끝날 때까지 돕니다.
pub async fn supervise<R: Runtime>(app: AppHandle<R>, d: Arc<Daemon>) {
    let spawn_allowed = spawn_allowed();
    let mut spawned = false;
    let mut backoff = BACKOFF_MIN;
    // 오래 붙어 있던 연결이 끊긴 직후: 데몬이 스스로 다시 뜨는 중일 수 있습니다(오프라인 모드로 들어가거나 나올 때).
    let mut relaunching = false;
    loop {
        d.set_state(&app, DaemonState::Connecting);
        let attempt = match connect_once().await {
            Err(ConnectError::NotRunning(_))
                if spawn_allowed && !spawned && !d.exiting.load(Ordering::SeqCst) =>
            {
                spawned = true;
                d.set_state(&app, DaemonState::Spawning);
                match spawn_daemon(&app) {
                    Ok(path) => {
                        info!("데몬을 띄웠습니다: {}", path.display());
                        connect_until(Instant::now() + SPAWN_WAIT).await
                    }
                    Err(spawn_err) => {
                        warn!("데몬을 띄우지 못했습니다: {spawn_err}");
                        Err(ConnectError::NotRunning(spawn_err))
                    }
                }
            }
            // 새 데몬은 옛 데몬이 데이터 경로를 놓을 때까지 기다렸다 뜨므로 그동안 오류 대신 연결 중으로 둡니다.
            Err(ConnectError::NotRunning(_)) if relaunching => {
                connect_until(Instant::now() + SPAWN_WAIT).await
            }
            other => other,
        };
        relaunching = false;
        match attempt {
            Ok((client, sub)) => {
                let connected_at = Instant::now();
                // 앱을 켰거나 오프라인 모드에서 돌아왔습니다. 곧 따라잡을 밀린 글은 OS 알림을 띄우지 않습니다.
                d.notifier.mark_online();
                let version = client
                    .call_value("system.version", json!({}))
                    .await
                    .ok()
                    .and_then(|v| v.get("version").and_then(Value::as_str).map(str::to_owned));
                *d.client.write() = Some(client);
                d.refresh_cache().await;
                d.set_state(&app, DaemonState::Connected { version });
                info!("데몬에 연결했습니다");
                forward_events(&app, &d, sub).await;
                *d.client.write() = None;
                if reconnect_immediately(connected_at.elapsed()) {
                    warn!("데몬 연결이 끊겼습니다. 다시 붙습니다");
                    // 끊긴 뒤 곧바로 다시 시도합니다(데몬 재시작이면 금방 붙음).
                    backoff = BACKOFF_MIN;
                    relaunching = true;
                    continue;
                }
                // 붙자마자 끊기는 데몬과 쉬지 않고 맴돌며 웹뷰를 다시 읽게 하지 않도록 기다립니다.
                warn!(
                    "데몬 연결이 곧바로 끊겼습니다. {}ms 뒤 다시 붙습니다",
                    backoff.as_millis()
                );
            }
            Err(e) => {
                d.set_state(
                    &app,
                    DaemonState::Error {
                        message: e.to_string(),
                    },
                );
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            _ = d.wake.notified() => {}
        }
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

/// 구독 스트림을 웹뷰 이벤트로 넘깁니다. 스트림이 끝나면(연결 끊김) 돌아옵니다.
async fn forward_events<R: Runtime>(app: &AppHandle<R>, d: &Arc<Daemon>, mut sub: Subscription) {
    while let Some(batch) = sub.next().await {
        if batch.iter().any(needs_cache_refresh) || (d.me().is_none() && has_message(&batch)) {
            let d2 = d.clone();
            tauri::async_runtime::spawn(async move { d2.refresh_cache().await });
        }
        crate::notify::handle_batch(app, d, &batch);
        if let Err(e) = app.emit_to(MAIN_WINDOW, EVENTS, &batch) {
            debug!("이벤트를 웹뷰로 보내지 못했습니다: {e}");
        }
    }
}

/// 끊긴 연결이 충분히 오래 유지됐는지(곧바로 다시 붙어도 되는지).
fn reconnect_immediately(lived: Duration) -> bool {
    lived >= STABLE_CONNECTION
}

fn needs_cache_refresh(n: &Notification) -> bool {
    matches!(
        n,
        Notification::SpaceUpdated { .. } | Notification::FriendAccepted { .. }
    )
}

fn has_message(batch: &[Notification]) -> bool {
    batch
        .iter()
        .any(|n| matches!(n, Notification::MessageNew { .. }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn method_names() {
        for ok in [
            "message.send",
            "space.list",
            "voice.signal",
            "seed.set_policy",
            "a1.b2",
        ] {
            assert!(validate_method(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "message",
            "Message.send",
            "message..send",
            ".send",
            "message.send.",
            "message.send;rm",
            "message.1send",
            "메시지.보내기",
            &"a.".repeat(40),
        ] {
            assert!(validate_method(bad).is_err(), "{bad}");
        }
        for blocked in BLOCKED_METHODS {
            let e = validate_method(blocked).unwrap_err();
            assert_eq!(e.code, crate::error::INVALID_REQUEST);
        }
    }

    #[test]
    fn short_lived_connections_back_off() {
        assert!(!reconnect_immediately(Duration::ZERO));
        assert!(!reconnect_immediately(
            STABLE_CONNECTION - Duration::from_millis(1)
        ));
        assert!(reconnect_immediately(STABLE_CONNECTION));
        assert!(reconnect_immediately(Duration::from_secs(3600)));
    }

    #[test]
    fn params_must_be_object() {
        assert_eq!(normalize_params(None).unwrap(), json!({}));
        assert_eq!(normalize_params(Some(Value::Null)).unwrap(), json!({}));
        assert_eq!(
            normalize_params(Some(json!({"a": 1}))).unwrap(),
            json!({"a": 1})
        );
        assert!(normalize_params(Some(json!([1, 2]))).is_err());
        assert!(normalize_params(Some(json!("x"))).is_err());
    }

    #[test]
    fn state_serializes_like_frontend_type() {
        assert_eq!(
            serde_json::to_value(DaemonState::Connecting).unwrap(),
            json!({"state": "connecting"})
        );
        assert_eq!(
            serde_json::to_value(DaemonState::Connected {
                version: Some("0.1.0".into())
            })
            .unwrap(),
            json!({"state": "connected", "version": "0.1.0"})
        );
        assert_eq!(
            serde_json::to_value(DaemonState::Connected { version: None }).unwrap(),
            json!({"state": "connected"})
        );
        assert_eq!(
            serde_json::to_value(DaemonState::Error {
                message: "x".into()
            })
            .unwrap(),
            json!({"state": "error", "message": "x"})
        );
    }

    #[test]
    fn candidates_order() {
        let none = |_: &str| None;
        let c = daemon_candidates(none, Some(Path::new("/opt/yangpa")));
        let suffix = std::env::consts::EXE_SUFFIX;
        let name = format!("yangpad{suffix}");
        // 실행 파일 옆(설치본) → 이름 바꾸기 전 사이드카 → … → PATH(이름만)
        assert_eq!(c[0], Path::new("/opt/yangpa").join(&name));
        assert_eq!(
            c[1],
            Path::new("/opt/yangpa").join(format!("yangpad-{TARGET_TRIPLE}{suffix}"))
        );
        assert_eq!(c.last(), Some(&PathBuf::from(&name)));
        // 시험은 디버그 빌드라 워크스페이스 target도 후보입니다.
        let ws_target = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(3)
            .unwrap()
            .join("target");
        assert!(c.contains(&ws_target.join("debug").join(&name)));
        assert!(c.contains(&ws_target.join("release").join(&name)));

        let env = |k: &str| (k == ENV_DAEMON_BIN).then(|| OsString::from("/x/yangpad-dev"));
        assert_eq!(
            daemon_candidates(env, Some(Path::new("/opt"))),
            vec![PathBuf::from("/x/yangpad-dev")]
        );
        // CARGO_TARGET_DIR은 저장소 target보다 먼저
        let env = |k: &str| (k == "CARGO_TARGET_DIR").then(|| OsString::from("/t"));
        let c = daemon_candidates(env, None);
        assert_eq!(c[0], Path::new("/t").join("debug").join(&name));
        assert_eq!(c[1], Path::new("/t").join("release").join(&name));
    }

    #[tokio::test]
    async fn exit_stops_daemon_unless_managed_elsewhere() {
        let fake = crate::testutil::FakeDaemon::new(Vec::new());
        // 연결 전이면 끌 데몬이 없습니다.
        let d = Daemon::default();
        assert!(!d.stop_on_exit(true).await);
        // YANGPA_NO_SPAWN=1(데몬을 따로 관리)이면 붙어 있어도 끄지 않습니다.
        let d = Daemon::default();
        d.set_client(crate::testutil::connect(fake.clone()).await);
        assert!(!d.stop_on_exit(false).await);
        assert_eq!(fake.shutdowns.load(Ordering::SeqCst), 0);
        // 보통은 앱을 끝내며 데몬도 끄고, 그 뒤로는 다시 띄우지 않습니다.
        let d = Daemon::default();
        d.set_client(crate::testutil::connect(fake.clone()).await);
        assert!(d.stop_on_exit(true).await);
        assert_eq!(fake.shutdowns.load(Ordering::SeqCst), 1);
        assert!(d.exiting.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn call_passes_through_results_and_errors() {
        let fake = crate::testutil::FakeDaemon::new(Vec::new());
        let d = Daemon::default();
        // 연결 전: 데몬 없음.
        let e = d.call("system.version", None).await.unwrap_err();
        assert_eq!(e.code, crate::error::DAEMON_UNAVAILABLE);
        d.set_client(crate::testutil::connect(fake).await);
        let v = d.call("system.version", None).await.unwrap();
        assert_eq!(v["version"], "9.9.9");
        // 1006은 confirmation_id까지 그대로.
        let e = d
            .call(
                "channel.delete",
                Some(json!({"space": crate::testutil::space().to_string(), "channel": crate::testutil::channel().to_string()})),
            )
            .await
            .unwrap_err();
        assert_eq!(e.code, 1006);
        assert_eq!(e.data.unwrap()["confirmation_id"], "c-1");
        // 막힌 메서드는 데몬까지 가지 않습니다.
        assert!(
            d.call("auth.hello", Some(json!({"token": "x"})))
                .await
                .is_err()
        );
        assert!(d.call("system.version", Some(json!([1]))).await.is_err());
    }

    #[test]
    fn not_running_detection() {
        assert!(is_not_running_io(&io::Error::from(
            io::ErrorKind::ConnectionRefused
        )));
        assert!(is_not_running_io(&io::Error::from(io::ErrorKind::NotFound)));
        assert!(!is_not_running_io(&io::Error::from(
            io::ErrorKind::PermissionDenied
        )));
    }

    #[test]
    fn connect_error_messages_have_no_secrets() {
        let e = ConnectError::Token(io::Error::new(io::ErrorKind::NotFound, "owner.token 없음"));
        assert!(e.to_string().contains("토큰"));
        let e = ConnectError::NotRunning(io::Error::from(io::ErrorKind::NotFound));
        assert!(e.to_string().contains(ENV_DAEMON_BIN));
    }
}
