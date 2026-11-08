//! 로컬 API 서버.
//!
//! 데몬은 [`RpcHandler`]를 구현하고 [`serve`]로 엔드포인트를 엽니다. 서버가 공통 규칙을 맡습니다.
//!
//! - 연결 후 첫 호출은 `auth.hello`. 인증 전에는 `auth.hello`, `system.version`만 허용(`1001`).
//! - 메서드 표([`crate::api::METHODS`])에 없는 메서드는 핸들러에 넘기지 않고 `-32601`.
//! - 토큰 범위(`1002`), 사람 전용 메서드, 공간 제한 토큰의 `params.space` 검사를 핸들러 호출 전에 합니다.
//! - 연결마다 동시 요청 수를 제한합니다. 다 차면 그 연결의 다음 요청 읽기를 미룹니다(역압).
//! - `events.subscribe`/`events.unsubscribe`는 서버가 직접 처리합니다. 데몬은 [`EventHub::publish`]로
//!   알림을 내보내고, 서버가 연결마다 화면 프레임(약 16ms) 단위로 묶어 `events` 알림 하나로 보냅니다.
//!   연결마다 큐가 정해져 있어, 느린 클라이언트는 알림을 잃을 뿐 데몬을 막지 않습니다(`events.dropped`로 알림).
//! - 로그에는 메서드 이름·오류 코드만 남기고 인자·결과는 남기지 않습니다.

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use interprocess::local_socket::ListenerOptions;
use interprocess::local_socket::tokio::Listener;
use interprocess::local_socket::traits::tokio::Listener as _;
use parking_lot::{Mutex, RwLock};
use serde::Serialize;
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::codec::{FramedRead, FramedWrite};
use tracing::{debug, warn};
use yangpa_core::ids::SpaceId;

use crate::api::events::{EVENTS_METHOD, valid_topic};
use crate::api::{
    Destructive, HelloParams, HelloResult, MethodSpec, Notification, Scope, SpaceRule,
    SubscribeParams, SubscribeResult, UnsubscribeParams, default_notification_filter, method_spec,
    parse_params, topic_matches,
};
use crate::endpoint::Endpoint;
use crate::framing::{Frame, FrameCodec, MAX_BINARY_PAYLOAD, MAX_JSON_PAYLOAD};
use crate::jsonrpc::{Id, Message, Request, Response, RpcError};

/// 토큰 문자열 최대 길이(인증 전 입력이라 짧게 제한).
const MAX_TOKEN_LEN: usize = 512;
/// 구독 하나의 주제 수 상한.
const MAX_TOPICS: usize = 32;

/// 인증된 토큰 정보. 데몬의 [`RpcHandler::authenticate`]가 돌려줍니다.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthInfo {
    pub token_id: String,
    /// 감사 로그에 남기는 이름.
    pub token_name: String,
    pub scopes: Vec<Scope>,
    /// 허용 공간. 비면 전체.
    pub spaces: Vec<SpaceId>,
    /// AI용 토큰. 파괴적 작업은 사람 확인 대기열로 갑니다.
    pub automation: bool,
}

impl AuthInfo {
    pub fn has_scope(&self, scope: Scope) -> bool {
        scope.satisfied_by(&self.scopes)
    }

    pub fn allows_space(&self, space: &SpaceId) -> bool {
        self.spaces.is_empty() || self.spaces.contains(space)
    }

    pub fn is_space_restricted(&self) -> bool {
        !self.spaces.is_empty()
    }
}

/// 핸들러 결과.
#[derive(Clone, Debug, PartialEq)]
pub enum RpcOutput {
    Json(Value),
    /// 바이너리 프레임으로 보낼 결과(`blob.read`의 `binary: true`). 요청 ID가 음이 아닌 정수여야 합니다.
    Binary(Bytes),
}

impl RpcOutput {
    pub fn json<T: Serialize>(value: &T) -> Result<RpcOutput, RpcError> {
        crate::api::to_result(value).map(RpcOutput::Json)
    }

    /// `{}`
    pub fn empty() -> RpcOutput {
        RpcOutput::Json(Value::Object(Default::default()))
    }
}

/// 데몬이 구현하는 요청 처리기.
#[async_trait]
pub trait RpcHandler: Send + Sync + 'static {
    /// 토큰을 확인합니다. 틀리면 `None`. 토큰 원문을 로그에 남기지 마세요.
    async fn authenticate(&self, token: &str) -> Option<AuthInfo>;

    /// 요청 하나를 처리합니다. 서버가 이미 인증·범위·공간 제한을 검사했으므로, 데몬은 공간 권한(`1003`),
    /// 자동화 토큰의 파괴적 작업(`1006`, [`ConnCtx::needs_confirmation`]), 휘발성 읽기(`1007`), 감사 로그를 맡습니다.
    async fn handle(
        &self,
        ctx: &ConnCtx,
        method: &str,
        params: Value,
    ) -> Result<RpcOutput, RpcError>;

    /// 알림을 이 연결에 보낼지(그리고 무엇으로 바꿔 보낼지) 정합니다. 기본은
    /// [`default_notification_filter`](공간 제한, 자동화 토큰의 휘발성 내용 지움).
    fn filter_notification(&self, auth: &AuthInfo, n: &Notification) -> Option<Notification> {
        default_notification_filter(auth, n)
    }

    /// 서버가 처리기를 거치지 않고 직접 답한 호출(`auth.hello`, `events.subscribe`·`events.unsubscribe`, 인증·범위
    /// 검사에서 거절한 호출)이 끝난 뒤 불립니다(감사 로그용). `auth`는 호출 뒤의 인증 정보(실패한 `auth.hello`면 `None`), `error`는 실패 코드.
    fn on_server_call(&self, _auth: Option<&AuthInfo>, _method: &str, _error: Option<i64>) {}

    /// 연결이 끊긴 뒤 한 번 불립니다.
    async fn on_disconnect(&self, _ctx: &ConnCtx) {}
}

/// 서버 설정.
#[derive(Clone, Debug)]
pub struct ServerConfig {
    /// 동시 연결 수. 넘으면 새 연결을 바로 닫습니다.
    pub max_connections: usize,
    /// 연결 하나의 동시 요청 수.
    pub max_concurrent_requests: usize,
    /// 연결 하나의 알림 큐 길이. 다 차면 알림을 버립니다.
    pub notification_queue: usize,
    /// 알림 묶음 간격(화면 프레임 하나).
    pub batch_interval: Duration,
    pub batch_max_items: usize,
    pub batch_max_bytes: usize,
    /// 이 횟수만큼 `auth.hello`가 실패하면 연결을 닫습니다.
    pub max_failed_auth: u32,
    /// 프레임 하나를 쓰는 시간 상한. 넘으면 클라이언트가 읽지 않는 것으로 보고 연결을 닫습니다.
    pub write_timeout: Duration,
    /// 연결 하나의 구독 수.
    pub max_subscriptions: usize,
    /// 이 시간 안에 `auth.hello`에 성공하지 않으면 연결을 닫습니다(인증 없는 연결이 자리를 차지하지 못하게).
    pub auth_timeout: Duration,
    /// 인증 전 프레임 상한. 인증 전에 부를 수 있는 메서드(`auth.hello`, `system.version`)는 작으므로
    /// 인증하지 않은 상대가 큰 프레임으로 메모리를 잡지 못하게 작게 둡니다.
    pub pre_auth_max_frame: usize,
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            max_connections: 64,
            max_concurrent_requests: 32,
            notification_queue: 1024,
            batch_interval: Duration::from_millis(16),
            batch_max_items: 512,
            batch_max_bytes: 1024 * 1024,
            max_failed_auth: 5,
            write_timeout: Duration::from_secs(30),
            max_subscriptions: 16,
            auth_timeout: Duration::from_secs(15),
            pre_auth_max_frame: 64 * 1024,
        }
    }
}

/// 연결 하나의 알림 보내기 핸들. 복제해 백그라운드 작업에서 써도 됩니다. 큐가 차면 막지 않고 버립니다.
#[derive(Clone, Debug)]
pub struct Notifier {
    tx: mpsc::Sender<Arc<Notification>>,
    dropped: Arc<AtomicU64>,
}

impl Notifier {
    /// 큐에 넣습니다. 큐가 찼거나 연결이 끊겼으면 `false`.
    pub fn send(&self, n: Arc<Notification>) -> bool {
        match self.tx.try_send(n) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
    }

    pub fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }
}

type Subs = Mutex<BTreeMap<u64, Vec<String>>>;

struct HubConn {
    subs: Arc<Subs>,
    notifier: Notifier,
}

#[derive(Default)]
struct HubInner {
    conns: RwLock<HashMap<u64, HubConn>>,
}

/// 데몬이 알림을 내보내는 곳. 구독한 연결마다 주제가 맞으면 그 연결의 큐에 넣습니다.
#[derive(Clone, Default)]
pub struct EventHub {
    inner: Arc<HubInner>,
}

impl EventHub {
    pub fn new() -> Self {
        Self::default()
    }

    /// 알림을 구독자에게 보냅니다. 막히지 않습니다. 받은 연결 수를 돌려줍니다.
    pub fn publish(&self, n: Notification) -> usize {
        let kind = n.kind();
        let n = Arc::new(n);
        let conns = self.inner.conns.read();
        let mut delivered = 0;
        for c in conns.values() {
            let wants = c
                .subs
                .lock()
                .values()
                .any(|topics| topic_matches(topics, kind));
            if wants && c.notifier.send(n.clone()) {
                delivered += 1;
            }
        }
        delivered
    }

    /// 이 종류의 알림을 받을 연결이 있는지(알림을 만드는 비용을 아낄 때).
    pub fn has_subscribers(&self, kind: &str) -> bool {
        self.inner
            .conns
            .read()
            .values()
            .any(|c| c.subs.lock().values().any(|t| topic_matches(t, kind)))
    }

    /// 구독이 하나라도 있는 연결 수.
    pub fn subscriber_count(&self) -> usize {
        self.inner
            .conns
            .read()
            .values()
            .filter(|c| !c.subs.lock().is_empty())
            .count()
    }

    fn register(&self, id: u64, subs: Arc<Subs>, notifier: Notifier) {
        self.inner
            .conns
            .write()
            .insert(id, HubConn { subs, notifier });
    }

    fn unregister(&self, id: u64) {
        self.inner.conns.write().remove(&id);
    }
}

/// 연결 하나의 문맥. 핸들러가 받습니다.
pub struct ConnCtx {
    id: u64,
    auth: OnceLock<AuthInfo>,
    notifier: Notifier,
    subs: Arc<Subs>,
    next_sub: AtomicU64,
    max_subscriptions: usize,
    hub: EventHub,
}

impl std::fmt::Debug for ConnCtx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnCtx")
            .field("id", &self.id)
            .field("token", &self.auth.get().map(|a| &a.token_name))
            .finish()
    }
}

impl ConnCtx {
    fn new(id: u64, notifier: Notifier, hub: EventHub, max_subscriptions: usize) -> Self {
        ConnCtx {
            id,
            auth: OnceLock::new(),
            notifier,
            subs: Arc::new(Mutex::new(BTreeMap::new())),
            next_sub: AtomicU64::new(1),
            max_subscriptions,
            hub,
        }
    }

    /// 연결 번호(프로세스 안에서 유일).
    pub fn id(&self) -> u64 {
        self.id
    }

    /// 인증 정보. `auth.hello` 전이면 `None`(인증 없이 부를 수 있는 메서드에서만 그렇습니다).
    pub fn auth(&self) -> Option<&AuthInfo> {
        self.auth.get()
    }

    pub fn token_name(&self) -> Option<&str> {
        self.auth.get().map(|a| a.token_name.as_str())
    }

    pub fn is_automation(&self) -> bool {
        self.auth.get().is_some_and(|a| a.automation)
    }

    pub fn scopes(&self) -> &[Scope] {
        self.auth.get().map(|a| a.scopes.as_slice()).unwrap_or(&[])
    }

    pub fn has_scope(&self, scope: Scope) -> bool {
        self.auth.get().is_some_and(|a| a.has_scope(scope))
    }

    /// 이 토큰이 그 공간을 다룰 수 있는지. 결과를 공간별로 거르는 메서드(`space.list` 등)에서 씁니다.
    pub fn allows_space(&self, space: &SpaceId) -> bool {
        self.auth.get().is_some_and(|a| a.allows_space(space))
    }

    /// 자동화 토큰이 항상 사람 확인이 필요한 메서드를 불렀는지. (`message.delete`처럼 대상에 따라 다른
    /// 메서드는 데몬이 대상을 보고 판단합니다.)
    pub fn needs_confirmation(&self, method: &str) -> bool {
        self.is_automation()
            && method_spec(method).is_some_and(|s| s.destructive == Destructive::Always)
    }

    /// 이 연결에만 알림을 보냅니다(구독과 무관, 걸러내기는 적용). 큐가 차면 `false`.
    pub fn notify(&self, n: Notification) -> bool {
        self.notifier.send(Arc::new(n))
    }

    /// 백그라운드 작업에서 쓸 알림 핸들.
    pub fn notifier(&self) -> Notifier {
        self.notifier.clone()
    }

    /// 이벤트 구독을 시작합니다(`events.subscribe`와 같음). 주제가 비면 전부.
    pub fn subscribe(&self, topics: Vec<String>) -> Result<u64, RpcError> {
        if topics.len() > MAX_TOPICS {
            return Err(RpcError::invalid_input(format!(
                "주제는 최대 {MAX_TOPICS}개"
            )));
        }
        if let Some(bad) = topics.iter().find(|t| !valid_topic(t)) {
            let shown: String = bad.chars().take(64).collect();
            return Err(RpcError::invalid_input(format!("알 수 없는 주제: {shown}")));
        }
        let mut subs = self.subs.lock();
        if subs.len() >= self.max_subscriptions {
            return Err(RpcError::invalid_input(format!(
                "구독은 연결마다 최대 {}개",
                self.max_subscriptions
            )));
        }
        let id = self.next_sub.fetch_add(1, Ordering::Relaxed);
        subs.insert(id, topics);
        Ok(id)
    }

    /// 구독을 끝냅니다. 없던 구독이면 `false`.
    pub fn unsubscribe(&self, subscription: u64) -> bool {
        self.subs.lock().remove(&subscription).is_some()
    }

    /// 이 연결의 구독 수.
    pub fn subscription_count(&self) -> usize {
        self.subs.lock().len()
    }

    /// 알림을 내보낼 허브(핸들러가 다른 연결에 알릴 때).
    pub fn hub(&self) -> &EventHub {
        &self.hub
    }
}

/// 알림을 화면 프레임 단위 `events` 묶음으로 모읍니다.
///
/// 첫 알림이 들어온 뒤 `interval`이 지나거나, 개수·크기 상한에 닿으면 프레임 하나로 내보냅니다.
#[derive(Debug)]
pub struct NotificationBatcher {
    buf: Vec<u8>,
    count: usize,
    deadline: Option<Instant>,
    interval: Duration,
    max_items: usize,
    max_bytes: usize,
}

const BATCH_PREFIX: &[u8] = br#"{"jsonrpc":"2.0","method":"events","params":{"batch":["#;
const BATCH_SUFFIX: &[u8] = b"]}}";

impl NotificationBatcher {
    pub fn new(interval: Duration, max_items: usize, max_bytes: usize) -> Self {
        debug_assert_eq!(EVENTS_METHOD, "events");
        NotificationBatcher {
            buf: Vec::new(),
            count: 0,
            deadline: None,
            interval,
            max_items: max_items.max(1),
            max_bytes: max_bytes.clamp(
                BATCH_PREFIX.len() + BATCH_SUFFIX.len() + 2,
                MAX_JSON_PAYLOAD,
            ),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn len(&self) -> usize {
        self.count
    }

    /// 지금 묶음을 내보낼 시각.
    pub fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    /// 알림을 넣습니다. 지금 내보내야 할 프레임(이전 묶음이 찼거나 이번에 찼을 때)을 돌려줍니다.
    /// 알림 하나가 프레임 상한보다 크면 버리고 `Err`.
    pub fn push(&mut self, n: &Notification) -> Result<Vec<Bytes>, serde_json::Error> {
        let item = serde_json::to_vec(n)?;
        Ok(self.push_raw(&item))
    }

    /// 이미 직렬화된 알림 JSON을 넣습니다. 너무 크면 버립니다.
    pub fn push_raw(&mut self, item: &[u8]) -> Vec<Bytes> {
        let overhead = BATCH_PREFIX.len() + BATCH_SUFFIX.len();
        if item.len() + overhead > MAX_JSON_PAYLOAD {
            warn!(bytes = item.len(), "알림이 너무 커서 버림");
            return Vec::new();
        }
        let mut out = Vec::new();
        if self.count > 0 && overhead + self.buf.len() + 1 + item.len() > self.max_bytes {
            out.extend(self.flush());
        }
        if self.count == 0 {
            self.deadline = Some(Instant::now() + self.interval);
        } else {
            self.buf.push(b',');
        }
        self.buf.extend_from_slice(item);
        self.count += 1;
        if self.count >= self.max_items || overhead + self.buf.len() >= self.max_bytes {
            out.extend(self.flush());
        }
        out
    }

    /// 모은 알림을 `events` 프레임 본문 하나로 내보냅니다.
    pub fn flush(&mut self) -> Option<Bytes> {
        if self.count == 0 {
            return None;
        }
        let mut frame =
            Vec::with_capacity(BATCH_PREFIX.len() + self.buf.len() + BATCH_SUFFIX.len());
        frame.extend_from_slice(BATCH_PREFIX);
        frame.append(&mut self.buf);
        frame.extend_from_slice(BATCH_SUFFIX);
        self.count = 0;
        self.deadline = None;
        Some(Bytes::from(frame))
    }
}

/// 실행 중인 서버. 버려도 서버는 멈추지 않습니다(멈추려면 [`ServerHandle::shutdown`]).
pub struct ServerHandle {
    endpoint: Endpoint,
    shutdown: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl ServerHandle {
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// 새 연결을 그만 받고 열린 연결을 닫습니다.
    pub fn shutdown(&self) {
        let _ = self.shutdown.send(true);
    }

    /// 수락 작업이 끝날 때까지 기다립니다(소켓 파일은 이때 지워집니다).
    pub async fn wait(self) {
        let _ = self.task.await;
    }
}

/// 기본 설정으로 서버를 엽니다.
pub async fn serve(
    endpoint: Endpoint,
    handler: Arc<dyn RpcHandler>,
    hub: EventHub,
) -> io::Result<ServerHandle> {
    serve_with(endpoint, handler, hub, ServerConfig::default()).await
}

/// 엔드포인트를 열고 수락 작업을 띄웁니다.
pub async fn serve_with(
    endpoint: Endpoint,
    handler: Arc<dyn RpcHandler>,
    hub: EventHub,
    config: ServerConfig,
) -> io::Result<ServerHandle> {
    let listener = bind(&endpoint)?;
    let (shutdown, shutdown_rx) = watch::channel(false);
    let config = Arc::new(config);
    let task = tokio::spawn(accept_loop(listener, handler, hub, config, shutdown_rx));
    Ok(ServerHandle {
        endpoint,
        shutdown,
        task,
    })
}

fn bind(endpoint: &Endpoint) -> io::Result<Listener> {
    #[cfg(unix)]
    {
        if !endpoint.is_named_pipe() {
            prepare_socket_path(endpoint.path())?;
            return bind_unix(endpoint);
        }
    }
    #[cfg(windows)]
    let opts = {
        use interprocess::os::windows::local_socket::ListenerOptionsExt;
        // 내 사용자와 SYSTEM만(기본값은 Everyone에게 읽기를 줘 다른 계정이 연결 자리를 차지할 수 있음, winpipe.rs).
        ListenerOptions::new().security_descriptor(crate::winpipe::private_pipe_descriptor()?)
    };
    #[cfg(not(windows))]
    let opts = ListenerOptions::new();
    opts.name(endpoint.to_name()?).create_tokio()
}

#[cfg(unix)]
fn bind_unix(endpoint: &Endpoint) -> io::Result<Listener> {
    use interprocess::os::unix::local_socket::ListenerOptionsExt;
    // fchmod 후 bind라 umask 경쟁이 없습니다. 지원하지 않는 OS는 bind 뒤 chmod로 대신합니다
    // (상위 디렉터리가 0700이라 그 사이에 다른 사용자가 끼어들 수 없음).
    match ListenerOptions::new()
        .name(endpoint.to_name()?)
        .mode(0o600)
        .create_tokio()
    {
        Err(e) if e.kind() == io::ErrorKind::Unsupported => {
            use std::os::unix::fs::PermissionsExt;
            let l = ListenerOptions::new()
                .name(endpoint.to_name()?)
                .create_tokio()?;
            std::fs::set_permissions(endpoint.path(), std::fs::Permissions::from_mode(0o600))?;
            Ok(l)
        }
        other => other,
    }
}

/// 소켓 상위 디렉터리를 만들고(0700), 남은 소켓 파일을 정리합니다.
///
/// 소켓이 아닌 파일은 지우지 않습니다. 소켓에 연결이 되면 다른 데몬이 실행 중이므로 `AddrInUse`.
#[cfg(unix)]
pub fn prepare_socket_path(path: &std::path::Path) -> io::Result<()> {
    use std::os::unix::fs::FileTypeExt;
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        crate::endpoint::create_private_dir(parent)?;
    }
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    if !meta.file_type().is_socket() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "엔드포인트 경로에 소켓이 아닌 파일이 있습니다: {}",
                path.display()
            ),
        ));
    }
    match std::os::unix::net::UnixStream::connect(path) {
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!("다른 데몬이 이미 실행 중입니다: {}", path.display()),
        )),
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
            ) =>
        {
            debug!("남은 소켓 파일 정리");
            match std::fs::remove_file(path) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            }
        }
        Err(e) => Err(e),
    }
}

static NEXT_CONN_ID: AtomicU64 = AtomicU64::new(1);

async fn accept_loop(
    listener: Listener,
    handler: Arc<dyn RpcHandler>,
    hub: EventHub,
    config: Arc<ServerConfig>,
    mut shutdown: watch::Receiver<bool>,
) {
    let slots = Arc::new(Semaphore::new(config.max_connections.max(1)));
    loop {
        let stream = tokio::select! {
            r = listener.accept() => match r {
                Ok(s) => s,
                Err(e) => {
                    warn!(error = %e, "연결 수락 실패");
                    // 파일 기술자 고갈 같은 오류가 바로 반복되지 않게 잠깐 쉽니다.
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
            },
            _ = wait_shutdown(&mut shutdown) => break,
        };
        let Ok(permit) = slots.clone().try_acquire_owned() else {
            warn!("동시 연결 수 초과로 연결을 닫음");
            drop(stream);
            continue;
        };
        let handler = handler.clone();
        let hub = hub.clone();
        let config = config.clone();
        let shutdown = shutdown.clone();
        tokio::spawn(serve_connection_inner(
            stream,
            handler,
            hub,
            config,
            shutdown,
            Some(Arc::new(permit)),
        ));
    }
    // listener가 여기서 해제되며 소켓 파일을 지웁니다.
}

/// 이미 열린 스트림 하나를 처리합니다(테스트·내장용). 연결이 끝나면 돌아옵니다.
pub async fn serve_connection<S>(
    io: S,
    handler: Arc<dyn RpcHandler>,
    hub: EventHub,
    config: ServerConfig,
) where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    let (_tx, rx) = watch::channel(false);
    serve_connection_inner(io, handler, hub, Arc::new(config), rx, None).await;
}

/// 응답 프레임과 그 요청의 동시 요청 자리. 자리는 프레임을 소켓에 쓴 뒤에 돌려줘, 읽지 않는 클라이언트의 연결에 쌓이는
/// 응답이 `max_concurrent_requests`개를 넘지 않게 합니다(대기열·쓰기 묶음 크기와 상관없이).
type OutFrame = (Frame, Option<OwnedSemaphorePermit>);

/// `slot`은 accept 루프의 연결 자리입니다. 처리 중인 요청도 하나씩 쥐어, 끊고 다시 붙어도 끝나지 않은 요청이 연결 수
/// 상한 밖에서 쌓이지 못합니다.
async fn serve_connection_inner<S>(
    io: S,
    handler: Arc<dyn RpcHandler>,
    hub: EventHub,
    config: Arc<ServerConfig>,
    mut shutdown: watch::Receiver<bool>,
    slot: Option<Arc<OwnedSemaphorePermit>>,
) where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    let conn_id = NEXT_CONN_ID.fetch_add(1, Ordering::Relaxed);
    let (rd, wr) = tokio::io::split(io);
    // 인증 전에는 작은 프레임만 받고, `auth.hello`에 성공하면 상한을 올립니다.
    let mut reader = FramedRead::new(rd, FrameCodec::with_max_frame(config.pre_auth_max_frame));
    let writer = FramedWrite::new(wr, FrameCodec::new());
    let auth_deadline = Instant::now() + config.auth_timeout;

    let (out_tx, out_rx) = mpsc::channel::<OutFrame>(config.max_concurrent_requests.max(1) * 2);
    let (notif_tx, notif_rx) = mpsc::channel::<Arc<Notification>>(config.notification_queue.max(1));
    let dropped = Arc::new(AtomicU64::new(0));
    let notifier = Notifier {
        tx: notif_tx,
        dropped: dropped.clone(),
    };
    let ctx = Arc::new(ConnCtx::new(
        conn_id,
        notifier,
        hub.clone(),
        config.max_subscriptions,
    ));
    let (closing_tx, closing_rx) = watch::channel(false);

    let writer_task = tokio::spawn(write_loop(
        writer,
        out_rx,
        notif_rx,
        dropped,
        ctx.clone(),
        handler.clone(),
        config.clone(),
        closing_rx,
    ));

    let permits = Arc::new(Semaphore::new(config.max_concurrent_requests.max(1)));
    let mut failed_auth = 0u32;
    debug!(conn = conn_id, "연결 시작");
    loop {
        let authed = ctx.auth().is_some();
        let frame = tokio::select! {
            f = reader.next() => f,
            _ = wait_shutdown(&mut shutdown) => break,
            // 쓰기 쪽이 끝났으면(클라이언트가 응답을 읽지 않아 시간 초과 등) 더 받은 요청을 실행하지 않습니다.
            _ = out_tx.closed() => {
                debug!(conn = conn_id, "쓰기 쪽이 닫혀 연결 종료");
                break;
            }
            _ = tokio::time::sleep_until(auth_deadline), if !authed => {
                debug!(conn = conn_id, "인증 시간 초과로 연결 종료");
                break;
            }
        };
        let bytes = match frame {
            None => break,
            Some(Err(e)) => {
                debug!(conn = conn_id, error = %e, "프레임 오류로 연결 종료");
                break;
            }
            Some(Ok(Frame::Binary { .. })) => {
                // 클라이언트는 바이너리 프레임을 보내지 않습니다.
                debug!(conn = conn_id, "클라이언트 바이너리 프레임으로 연결 종료");
                break;
            }
            Some(Ok(Frame::Json(b))) => b,
        };
        let req = match Message::parse(&bytes) {
            Ok(Message::Request(r)) => r,
            Ok(_) => continue, // 클라이언트 알림·응답은 쓰지 않습니다.
            Err(f) => {
                if send_response(&out_tx, Response::err(f.id, f.error))
                    .await
                    .is_err()
                {
                    break;
                }
                continue;
            }
        };
        let Some(spec) = method_spec(&req.method) else {
            let r = Response::err(Some(req.id), RpcError::method_not_found(&req.method));
            if send_response(&out_tx, r).await.is_err() {
                break;
            }
            continue;
        };

        // 인증은 순서가 중요하므로 읽기 루프에서 바로 처리합니다.
        if spec.name == "auth.hello" {
            let (resp, ok) = hello(&ctx, &*handler, &hub, req).await;
            handler.on_server_call(ctx.auth(), spec.name, response_error_code(&resp));
            if ctx.auth().is_some() {
                reader
                    .decoder_mut()
                    .set_max_frame(crate::framing::MAX_FRAME);
            }
            if !ok {
                failed_auth += 1;
                // 무차별 대입을 늦춥니다.
                tokio::time::sleep(Duration::from_millis(100 * u64::from(failed_auth.min(10))))
                    .await;
            }
            if send_response(&out_tx, resp).await.is_err() {
                break;
            }
            if failed_auth >= config.max_failed_auth {
                warn!(conn = conn_id, "인증 실패가 반복되어 연결을 닫음");
                break;
            }
            continue;
        }

        if let Err(e) = check_access(spec, ctx.auth(), &req.params) {
            // 범위 밖·사람 전용 거절도 감사 로그에 남깁니다(처리기까지 오지 않으므로).
            handler.on_server_call(ctx.auth(), spec.name, Some(e.code));
            if send_response(&out_tx, Response::err(Some(req.id), e))
                .await
                .is_err()
            {
                break;
            }
            continue;
        }

        if spec.name == "events.subscribe" || spec.name == "events.unsubscribe" {
            let resp = subscription_call(&ctx, spec.name, req);
            handler.on_server_call(ctx.auth(), spec.name, response_error_code(&resp));
            if send_response(&out_tx, resp).await.is_err() {
                break;
            }
            continue;
        }

        // 동시 요청 상한: 자리가 날 때까지 다음 요청을 읽지 않습니다.
        let permit = tokio::select! {
            p = permits.clone().acquire_owned() => match p { Ok(p) => p, Err(_) => break },
            _ = wait_shutdown(&mut shutdown) => break,
        };
        let handler = handler.clone();
        let ctx = ctx.clone();
        let out = out_tx.clone();
        // shortcut: 끝없이 기다리는 요청(strict_rotation의 message.send)은 끊긴 뒤에도 연결 자리를 쥡니다. 그런 메서드가
        // 늘어 다시 붙을 때마다 자리가 줄면 끊긴 연결의 요청을 취소하기.
        let slot = slot.clone();
        // 연결이 끊겨도 처리 중인 요청은 끝까지 실행합니다(중간에 끊으면 데몬 상태가 어긋날 수 있음).
        tokio::spawn(async move {
            let Request {
                id, method, params, ..
            } = req;
            let result = handler.handle(&ctx, &method, params).await;
            if let Err(e) = &result {
                debug!(conn = ctx.id, method = %method, code = e.code, "요청 실패");
            }
            let frame = output_frame(id, result);
            let _ = out.send((frame, Some(permit))).await;
            drop(slot);
        });
    }

    let _ = closing_tx.send(true);
    drop(out_tx);
    hub.unregister(conn_id);
    let _ = writer_task.await;
    handler.on_disconnect(&ctx).await;
    debug!(conn = conn_id, "연결 끝");
}

fn response_error_code(resp: &Response) -> Option<i64> {
    resp.outcome.as_ref().err().map(|e| e.code)
}

async fn send_response(out: &mpsc::Sender<OutFrame>, resp: Response) -> Result<(), ()> {
    let frame = Frame::json(&resp).map_err(|_| ())?;
    out.send((frame, None)).await.map_err(|_| ())
}

fn json_frame_or_error(id: Id, value: Value) -> Frame {
    let resp = Response::ok(id.clone(), value);
    match Frame::json(&resp) {
        Ok(f) if f.wire_len() <= crate::framing::MAX_FRAME => f,
        Ok(_) => error_frame(
            id,
            RpcError::internal("응답이 프레임 상한(16MiB)보다 큽니다"),
        ),
        Err(_) => error_frame(id, RpcError::internal("응답 직렬화 실패")),
    }
}

fn error_frame(id: Id, e: RpcError) -> Frame {
    let resp = Response::err(Some(id), e);
    // 오류 객체는 작으므로 직렬화가 실패하지 않습니다. 만약 실패하면 최소 응답을 보냅니다.
    Frame::json(&resp).unwrap_or_else(|_| {
        Frame::Json(Bytes::from_static(
            br#"{"jsonrpc":"2.0","id":null,"error":{"code":-32603,"message":"internal"}}"#,
        ))
    })
}

fn output_frame(id: Id, result: Result<RpcOutput, RpcError>) -> Frame {
    match result {
        Ok(RpcOutput::Json(v)) => json_frame_or_error(id, v),
        Ok(RpcOutput::Binary(data)) => match id.as_u64() {
            Some(n) if data.len() <= MAX_BINARY_PAYLOAD => Frame::Binary { id: n, data },
            Some(_) => error_frame(
                id,
                RpcError::internal("바이너리 결과가 프레임 상한보다 큽니다"),
            ),
            None => error_frame(
                id,
                RpcError::invalid_request("바이너리 응답은 음이 아닌 정수 요청 ID가 필요합니다"),
            ),
        },
        Err(e) => error_frame(id, e),
    }
}

async fn hello(
    ctx: &ConnCtx,
    handler: &dyn RpcHandler,
    hub: &EventHub,
    req: Request,
) -> (Response, bool) {
    let id = req.id;
    if ctx.auth.get().is_some() {
        return (
            Response::err(
                Some(id),
                RpcError::invalid_request("이미 인증된 연결입니다"),
            ),
            true,
        );
    }
    let params: HelloParams = match parse_params(req.params) {
        Ok(p) => p,
        Err(e) => return (Response::err(Some(id), e), false),
    };
    let token = params.token.expose();
    if token.is_empty() || token.len() > MAX_TOKEN_LEN {
        return (
            Response::err(
                Some(id),
                RpcError::new(
                    crate::jsonrpc::ERR_AUTH_REQUIRED,
                    "토큰이 올바르지 않습니다",
                ),
            ),
            false,
        );
    }
    let Some(auth) = handler.authenticate(token).await else {
        return (
            Response::err(
                Some(id),
                RpcError::new(
                    crate::jsonrpc::ERR_AUTH_REQUIRED,
                    "토큰이 올바르지 않습니다",
                ),
            ),
            false,
        );
    };
    let result = HelloResult {
        scopes: auth.scopes.clone(),
        automation: auth.automation,
        spaces: auth.spaces.clone(),
    };
    // 읽기 루프에서만 부르므로 경쟁이 없습니다.
    let _ = ctx.auth.set(auth);
    hub.register(ctx.id, ctx.subs.clone(), ctx.notifier.clone());
    match crate::api::to_result(&result) {
        Ok(v) => (Response::ok(id, v), true),
        Err(e) => (Response::err(Some(id), e), true),
    }
}

/// 인증·범위·사람 전용·공간 제한 검사.
fn check_access(
    spec: &MethodSpec,
    auth: Option<&AuthInfo>,
    params: &Value,
) -> Result<(), RpcError> {
    // 위치 인자(배열)는 `params.space` 검사를 피해 갈 수 있으므로(serde가 구조체를 배열로도 읽음) 받지 않습니다.
    if !matches!(params, Value::Null | Value::Object(_)) {
        return Err(RpcError::invalid_params(
            "params는 이름 있는 인자(객체)여야 합니다",
        ));
    }
    let Some(required) = spec.scope else {
        return Ok(());
    };
    let Some(auth) = auth else {
        return Err(RpcError::auth_required());
    };
    if !auth.has_scope(required) {
        return Err(
            RpcError::out_of_scope(format!("토큰 범위 밖입니다(필요: {required})"))
                .with_data(serde_json::json!({ "required": required })),
        );
    }
    if spec.human_only && auth.automation {
        return Err(RpcError::out_of_scope(
            "사람 토큰만 부를 수 있는 메서드입니다",
        ));
    }
    if !auth.is_space_restricted() {
        return Ok(());
    }
    let space = match params.get("space") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => match s.parse::<SpaceId>() {
            Ok(id) => Some(id),
            Err(_) => {
                return Err(RpcError::invalid_params(
                    "space가 올바른 공간 ID가 아닙니다",
                ));
            }
        },
        Some(_) => return Err(RpcError::invalid_params("space는 문자열이어야 합니다")),
    };
    let denied = || RpcError::out_of_scope("이 토큰은 그 공간을 다룰 수 없습니다");
    match (spec.space, space) {
        (_, Some(s)) if !auth.allows_space(&s) => Err(denied()),
        (SpaceRule::Required, None) => Err(RpcError::invalid_params("space가 필요합니다")),
        (SpaceRule::OptionalGlobal | SpaceRule::Global, None) => Err(RpcError::out_of_scope(
            "공간 제한 토큰은 전역 작업을 할 수 없습니다",
        )),
        _ => Ok(()),
    }
}

fn subscription_call(ctx: &ConnCtx, method: &str, req: Request) -> Response {
    let id = req.id;
    let result = if method == "events.subscribe" {
        parse_params::<SubscribeParams>(req.params)
            .and_then(|p| ctx.subscribe(p.topics.unwrap_or_default()))
            .and_then(|subscription| crate::api::to_result(&SubscribeResult { subscription }))
    } else {
        parse_params::<UnsubscribeParams>(req.params).and_then(|p| {
            if ctx.unsubscribe(p.subscription) {
                Ok(Value::Object(Default::default()))
            } else {
                Err(RpcError::not_found("없는 구독입니다"))
            }
        })
    };
    match result {
        Ok(v) => Response::ok(id, v),
        Err(e) => Response::err(Some(id), e),
    }
}

#[allow(clippy::too_many_arguments)]
async fn write_loop<W>(
    mut sink: FramedWrite<W, FrameCodec>,
    mut out_rx: mpsc::Receiver<OutFrame>,
    mut notif_rx: mpsc::Receiver<Arc<Notification>>,
    dropped: Arc<AtomicU64>,
    ctx: Arc<ConnCtx>,
    handler: Arc<dyn RpcHandler>,
    config: Arc<ServerConfig>,
    mut closing: watch::Receiver<bool>,
) where
    W: AsyncWrite + Unpin + Send + 'static,
{
    let mut batcher = NotificationBatcher::new(
        config.batch_interval,
        config.batch_max_items,
        config.batch_max_bytes,
    );
    let mut out_open = true;
    loop {
        let deadline = batcher.deadline();
        let mut frames: Vec<Frame> = Vec::new();
        // 이 묶음의 요청 자리: 다 쓴 뒤(이 반복이 끝날 때) 놓습니다.
        let mut permits: Vec<OwnedSemaphorePermit> = Vec::new();
        let mut finish = false;
        tokio::select! {
            // 응답을 알림보다 먼저 씁니다.
            biased;
            f = out_rx.recv(), if out_open => match f {
                Some((f, p)) => {
                    frames.push(f);
                    permits.extend(p);
                    // 쌓인 응답을 한 번에 씁니다.
                    while frames.len() < 64 {
                        match out_rx.try_recv() {
                            Ok((f, p)) => {
                                frames.push(f);
                                permits.extend(p);
                            }
                            Err(_) => break,
                        }
                    }
                }
                None => out_open = false,
            },
            n = notif_rx.recv() => match n {
                Some(n) => {
                    let lost = dropped.swap(0, Ordering::Relaxed);
                    if lost > 0 {
                        frames.extend(batch_push(&mut batcher, &Notification::EventsDropped { count: lost }));
                    }
                    if let Some(auth) = ctx.auth()
                        && let Some(n) = handler.filter_notification(auth, &n)
                    {
                        frames.extend(batch_push(&mut batcher, &n));
                    }
                }
                None => finish = true,
            },
            _ = sleep_until_opt(deadline), if deadline.is_some() => {
                frames.extend(batcher.flush().map(Frame::Json));
            },
            _ = wait_true(&mut closing) => finish = true,
        }
        if finish {
            // 읽기 쪽이 끝났음: 지금 쌓인 응답·알림만 쓰고 끝냅니다.
            while let Ok((f, p)) = out_rx.try_recv() {
                frames.push(f);
                permits.extend(p);
            }
            frames.extend(batcher.flush().map(Frame::Json));
            let _ = write_frames(&mut sink, frames, config.write_timeout).await;
            break;
        }
        if write_frames(&mut sink, frames, config.write_timeout)
            .await
            .is_err()
        {
            debug!(conn = ctx.id, "쓰기 실패·시간 초과로 연결 종료");
            break;
        }
    }
}

fn batch_push(batcher: &mut NotificationBatcher, n: &Notification) -> Vec<Frame> {
    match batcher.push(n) {
        Ok(v) => v.into_iter().map(Frame::Json).collect(),
        Err(e) => {
            warn!(error = %e, kind = n.kind(), "알림 직렬화 실패");
            Vec::new()
        }
    }
}

/// 감시 값이 `true`가 될 때까지 기다립니다(보내는 쪽이 사라져도 끝남). 빌린 값을 await 너머로 들고 있지 않게 감쌉니다.
async fn wait_true(rx: &mut watch::Receiver<bool>) {
    let _ = rx.wait_for(|v| *v).await;
}

/// 서버 종료 신호. [`ServerHandle`]을 버려도(보내는 쪽이 사라져도) 서버는 계속 돕니다.
async fn wait_shutdown(rx: &mut watch::Receiver<bool>) {
    if rx.wait_for(|v| *v).await.is_err() {
        std::future::pending::<()>().await;
    }
}

async fn sleep_until_opt(deadline: Option<Instant>) {
    match deadline {
        Some(d) => tokio::time::sleep_until(d).await,
        None => std::future::pending().await,
    }
}

async fn write_frames<W>(
    sink: &mut FramedWrite<W, FrameCodec>,
    frames: Vec<Frame>,
    timeout: Duration,
) -> Result<(), ()>
where
    W: AsyncWrite + Unpin + Send,
{
    if frames.is_empty() {
        return Ok(());
    }
    let fut = async {
        for f in frames {
            sink.feed(f).await?;
        }
        sink.flush().await
    };
    match tokio::time::timeout(timeout, fut).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => {
            debug!(error = %e, "프레임 쓰기 실패");
            Err(())
        }
        Err(_) => Err(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jsonrpc::{ERR_AUTH_REQUIRED, ERR_OUT_OF_SCOPE, INVALID_PARAMS};
    use serde_json::json;

    fn auth(scopes: Vec<Scope>, spaces: Vec<SpaceId>, automation: bool) -> AuthInfo {
        AuthInfo {
            token_id: "id".into(),
            token_name: "n".into(),
            scopes,
            spaces,
            automation,
        }
    }

    #[test]
    fn access_rules() {
        let spec = |m| method_spec(m).unwrap();
        let s1 = SpaceId([1; 16]);
        let s2 = SpaceId([2; 16]);
        assert!(check_access(spec("system.version"), None, &Value::Null).is_ok());
        assert_eq!(
            check_access(spec("space.list"), None, &Value::Null)
                .unwrap_err()
                .code,
            ERR_AUTH_REQUIRED
        );

        let reader = auth(vec![Scope::Read], vec![], false);
        assert!(check_access(spec("space.list"), Some(&reader), &Value::Null).is_ok());
        let e = check_access(spec("message.send"), Some(&reader), &json!({})).unwrap_err();
        assert_eq!(e.code, ERR_OUT_OF_SCOPE);
        assert_eq!(e.data.unwrap()["required"], "write");

        let owner_ai = auth(vec![Scope::Owner], vec![], true);
        assert_eq!(
            check_access(
                spec("confirm.approve"),
                Some(&owner_ai),
                &json!({"id": "x"})
            )
            .unwrap_err()
            .code,
            ERR_OUT_OF_SCOPE
        );
        assert!(check_access(spec("member.kick"), Some(&owner_ai), &json!({})).is_ok());
        // 자동화 토큰은 사용자를 P2P 전용 채널(IP 노출)에 넣거나 빼지 못합니다. 사람 쓰기 토큰은 됩니다.
        let writer = auth(vec![Scope::Write], vec![], false);
        let writer_ai = auth(vec![Scope::Write], vec![], true);
        let ch = json!({"space": s1.to_string(), "channel": "x"});
        for m in ["channel.p2p_join", "channel.p2p_leave"] {
            assert_eq!(
                check_access(spec(m), Some(&writer_ai), &ch)
                    .unwrap_err()
                    .code,
                ERR_OUT_OF_SCOPE,
                "{m}"
            );
            assert!(check_access(spec(m), Some(&writer), &ch).is_ok(), "{m}");
        }

        let restricted = auth(vec![Scope::Read, Scope::Settings], vec![s1], false);
        let p1 = json!({"space": s1.to_string()});
        let p2 = json!({"space": s2.to_string()});
        assert!(check_access(spec("message.list"), Some(&restricted), &p1).is_ok());
        assert_eq!(
            check_access(spec("message.list"), Some(&restricted), &p2)
                .unwrap_err()
                .code,
            ERR_OUT_OF_SCOPE
        );
        assert_eq!(
            check_access(spec("message.list"), Some(&restricted), &json!({}))
                .unwrap_err()
                .code,
            INVALID_PARAMS
        );
        assert_eq!(
            check_access(
                spec("message.list"),
                Some(&restricted),
                &json!({"space": 5})
            )
            .unwrap_err()
            .code,
            INVALID_PARAMS
        );
        // 걸러 주는 메서드는 허용, 전역 작업은 거부.
        assert!(check_access(spec("space.list"), Some(&restricted), &Value::Null).is_ok());
        assert!(check_access(spec("seed.get"), Some(&restricted), &Value::Null).is_ok());
        assert_eq!(
            check_access(spec("seed.set"), Some(&restricted), &json!({}))
                .unwrap_err()
                .code,
            ERR_OUT_OF_SCOPE
        );
        assert!(check_access(spec("seed.set"), Some(&restricted), &p1).is_ok());
        assert_eq!(
            check_access(spec("net.status"), Some(&restricted), &Value::Null)
                .unwrap_err()
                .code,
            ERR_OUT_OF_SCOPE
        );
        // 위치 인자로 다른 공간을 넣어 공간 제한을 우회할 수 없습니다.
        for m in ["seed.get", "peer.get", "message.search", "space.list"] {
            assert_eq!(
                check_access(spec(m), Some(&restricted), &json!([s2.to_string()]))
                    .unwrap_err()
                    .code,
                INVALID_PARAMS,
                "{m}"
            );
        }
        assert_eq!(
            check_access(spec("system.version"), None, &json!(["x"]))
                .unwrap_err()
                .code,
            INVALID_PARAMS
        );
    }

    #[test]
    fn batcher_groups_and_splits() {
        let mut b = NotificationBatcher::new(Duration::from_millis(16), 3, 1 << 20);
        assert!(b.deadline().is_none());
        let n = Notification::SpaceUpdated {
            space: SpaceId([1; 16]),
        };
        assert!(b.push(&n).unwrap().is_empty());
        assert!(b.deadline().is_some());
        assert!(b.push(&n).unwrap().is_empty());
        let full = b.push(&n).unwrap();
        assert_eq!(full.len(), 1);
        assert!(b.is_empty() && b.deadline().is_none());
        let v: Value = serde_json::from_slice(&full[0]).unwrap();
        assert_eq!(v["method"], "events");
        assert_eq!(v["params"]["batch"].as_array().unwrap().len(), 3);
        assert_eq!(v["params"]["batch"][0]["type"], "space.updated");

        // 크기 상한: 두 번째 알림이 들어가지 않으면 앞 묶음을 먼저 내보냅니다.
        let item = serde_json::to_vec(&n).unwrap();
        let mut b = NotificationBatcher::new(
            Duration::from_millis(16),
            100,
            BATCH_PREFIX.len() + BATCH_SUFFIX.len() + item.len() + 1,
        );
        assert!(b.push_raw(&item).len() <= 1);
        let out = b.push_raw(&item);
        assert!(!out.is_empty());
        for f in out {
            let v: Value = serde_json::from_slice(&f).unwrap();
            assert!(!v["params"]["batch"].as_array().unwrap().is_empty());
        }
        assert!(b.flush().is_none() || b.is_empty());
    }

    #[test]
    fn output_frames() {
        assert!(matches!(
            output_frame(Id::Num(3), Ok(RpcOutput::Binary(Bytes::from_static(b"x")))),
            Frame::Binary { id: 3, .. }
        ));
        let Frame::Json(b) = output_frame(Id::Str("s".into()), Ok(RpcOutput::Binary(Bytes::new())))
        else {
            panic!("JSON 오류여야 함");
        };
        let r: Response = serde_json::from_slice(&b).unwrap();
        assert!(r.outcome.is_err());
    }

    #[test]
    fn ctx_subscriptions() {
        let (tx, _rx) = mpsc::channel(4);
        let ctx = ConnCtx::new(
            1,
            Notifier {
                tx,
                dropped: Arc::new(AtomicU64::new(0)),
            },
            EventHub::new(),
            2,
        );
        let a = ctx.subscribe(vec!["message".into()]).unwrap();
        let _b = ctx.subscribe(vec![]).unwrap();
        assert!(ctx.subscribe(vec![]).is_err());
        assert!(ctx.unsubscribe(a));
        assert!(!ctx.unsubscribe(a));
        assert!(ctx.subscribe(vec!["bogus".into()]).is_err());
    }

    #[tokio::test]
    async fn notifier_drops_when_full() {
        let (tx, _rx) = mpsc::channel(1);
        let n = Notifier {
            tx,
            dropped: Arc::new(AtomicU64::new(0)),
        };
        let ev = Arc::new(Notification::EventsDropped { count: 0 });
        assert!(n.send(ev.clone()));
        assert!(!n.send(ev.clone()));
        assert!(!n.send(ev));
        assert_eq!(n.dropped.load(Ordering::Relaxed), 2);
    }
}
