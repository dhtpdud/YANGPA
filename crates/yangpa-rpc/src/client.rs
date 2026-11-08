//! 로컬 API 클라이언트. 요청 ID로 응답을 맞춰 여러 요청을 한 연결에서 동시에 보냅니다.
//!
//! ```no_run
//! # async fn f() -> Result<(), yangpa_rpc::client::ClientError> {
//! use yangpa_rpc::{api, client::RpcClient, endpoint};
//! let ep = endpoint::default_endpoint().map_err(yangpa_rpc::client::ClientError::Connect)?;
//! let client = RpcClient::connect(&ep).await?;
//! client.hello("토큰").await?;
//! let spaces = client.request::<api::m::SpaceList>(api::Empty).await?;
//! # let _ = spaces; Ok(()) }
//! ```

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use base64::Engine;
use bytes::Bytes;
use futures::{SinkExt, Stream, StreamExt};
use interprocess::local_socket::tokio::Stream as LocalStream;
use interprocess::local_socket::traits::tokio::Stream as _;
use parking_lot::Mutex;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_util::codec::{FramedRead, FramedWrite};
use tracing::debug;

use crate::api::events::EVENTS_METHOD;
use crate::api::{
    self, BlobReadParams, BlobReadResult, HelloParams, HelloResult, Method, Notification,
    SubscribeParams, SubscribeResult, UnsubscribeParams, topic_matches,
};
use crate::endpoint::Endpoint;
use crate::framing::{Frame, FrameCodec, MAX_FRAME};
use crate::jsonrpc::{Id, Message, Request, RpcError};

/// 구독 하나가 쌓아 둘 수 있는 묶음 수. 넘으면 버리고 `events.dropped`로 알립니다.
const SUBSCRIPTION_QUEUE: usize = 256;

#[derive(Debug, Error)]
pub enum ClientError {
    /// 데몬이 돌려준 JSON-RPC 오류.
    #[error("{}: {}", crate::jsonrpc::describe_code(.0.code), .0.message)]
    Rpc(RpcError),
    #[error("데몬에 연결할 수 없습니다: {0}")]
    Connect(std::io::Error),
    #[error("데몬 연결이 끊겼습니다")]
    Closed,
    #[error("응답 시간이 초과됐습니다")]
    Timeout,
    #[error("요청을 직렬화할 수 없습니다: {0}")]
    Encode(serde_json::Error),
    #[error("응답을 해석할 수 없습니다: {0}")]
    Decode(serde_json::Error),
    #[error("요청이 프레임 상한(16MiB)보다 큽니다")]
    TooLarge,
    #[error("예상하지 못한 응답: {0}")]
    Unexpected(&'static str),
}

impl ClientError {
    /// JSON-RPC 오류면 그 객체.
    pub fn rpc(&self) -> Option<&RpcError> {
        match self {
            ClientError::Rpc(e) => Some(e),
            _ => None,
        }
    }

    /// JSON-RPC 오류 코드.
    pub fn code(&self) -> Option<i64> {
        self.rpc().map(|e| e.code)
    }

    /// `1006`이면 사람 확인 ID.
    pub fn confirmation_id(&self) -> Option<&str> {
        self.rpc().and_then(RpcError::confirmation_id)
    }
}

impl From<RpcError> for ClientError {
    fn from(e: RpcError) -> Self {
        ClientError::Rpc(e)
    }
}

enum Reply {
    Json(Result<Value, RpcError>),
    Binary(Bytes),
}

struct SubSlot {
    topics: Vec<String>,
    tx: mpsc::Sender<Vec<Notification>>,
    dropped: u64,
}

#[derive(Default)]
struct Shared {
    pending: Mutex<HashMap<u64, oneshot::Sender<Reply>>>,
    subs: Mutex<HashMap<u64, SubSlot>>,
    next_sub: AtomicU64,
    closed: AtomicBool,
}

/// 마지막 클라이언트 복제본이 사라지면 읽기 작업을 멈춰 연결을 닫습니다.
struct ReaderGuard(JoinHandle<()>);

impl Drop for ReaderGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// 로컬 API 클라이언트. 복제해서 여러 작업에서 같이 써도 됩니다.
#[derive(Clone)]
pub struct RpcClient {
    out: mpsc::Sender<Frame>,
    shared: Arc<Shared>,
    next_id: Arc<AtomicU64>,
    timeout: Option<Duration>,
    _reader: Arc<ReaderGuard>,
}

impl std::fmt::Debug for RpcClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RpcClient")
            .field("closed", &self.is_closed())
            .finish()
    }
}

/// 파이프 인스턴스가 모두 쓰이는 중일 때 다시 시도하는 최대 시간.
#[cfg(windows)]
const PIPE_BUSY_WAIT: Duration = Duration::from_secs(5);

/// Windows named pipe에 붙습니다. 인스턴스가 모두 쓰이는 중(`ERROR_PIPE_BUSY`)이면 잠깐씩 쉬며
/// [`PIPE_BUSY_WAIT`]까지만 다시 시도합니다. interprocess의 tokio 연결은 이때 블로킹 스레드에서
/// 끝없이 기다려서, 수락을 멈춘 서버에 붙으면 바깥 시간 제한이 지나도 런타임 종료가 막힙니다.
#[cfg(windows)]
async fn connect_pipe(
    path: &std::path::Path,
) -> std::io::Result<tokio::net::windows::named_pipe::NamedPipeClient> {
    use tokio::net::windows::named_pipe::ClientOptions;
    // winerror.h
    const ERROR_PIPE_BUSY: i32 = 231;
    let deadline = tokio::time::Instant::now() + PIPE_BUSY_WAIT;
    loop {
        match ClientOptions::new().open(path) {
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                if tokio::time::Instant::now() >= deadline {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        format!(
                            "데몬이 새 연결을 받지 않습니다(파이프가 모두 사용 중): {}",
                            path.display()
                        ),
                    ));
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            other => return other,
        }
    }
}

/// 요청 하나가 끝나기 전에 취소돼도 대기 목록에서 지웁니다.
struct PendingGuard<'a> {
    shared: &'a Shared,
    id: u64,
}

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        self.shared.pending.lock().remove(&self.id);
    }
}

/// Unix 소켓 서버 프로세스의 유효 사용자가 나인지. 확인할 수 없으면 거부합니다.
#[cfg(unix)]
fn verify_socket_server(stream: &LocalStream) -> std::io::Result<()> {
    use interprocess::local_socket::traits::StreamCommon;
    // SAFETY: geteuid는 인자가 없고 실패하지 않습니다.
    let me = unsafe { libc::geteuid() };
    same_user(stream.peer_creds()?.euid(), me)
}

#[cfg(any(unix, test))]
fn same_user(server: Option<u32>, me: u32) -> std::io::Result<()> {
    use std::io::{Error, ErrorKind};
    match server {
        Some(uid) if uid == me => Ok(()),
        Some(uid) => Err(Error::new(
            ErrorKind::PermissionDenied,
            format!(
                "데몬 소켓 서버(uid {uid})가 다른 OS 사용자의 프로세스라 연결하지 않습니다. \
                 다른 사용자가 같은 이름의 소켓을 먼저 열었을 수 있습니다"
            ),
        )),
        None => Err(Error::new(
            ErrorKind::PermissionDenied,
            "데몬 소켓 서버의 사용자를 확인할 수 없어 연결하지 않습니다",
        )),
    }
}

impl RpcClient {
    /// 엔드포인트에 연결합니다(인증 전). 이어서 [`hello`](Self::hello)를 부르세요.
    pub async fn connect(endpoint: &Endpoint) -> Result<RpcClient, ClientError> {
        #[cfg(windows)]
        if endpoint.is_named_pipe() {
            let pipe = connect_pipe(endpoint.path())
                .await
                .map_err(ClientError::Connect)?;
            // pipe 이름은 남이 먼저 차지할 수 있어, 토큰을 보내기 전에 서버가 같은 OS 사용자인지 봅니다.
            crate::winpipe::verify_pipe_server(&pipe).map_err(ClientError::Connect)?;
            return Ok(RpcClient::from_stream(pipe));
        }
        let name = endpoint.to_name().map_err(ClientError::Connect)?;
        let stream = LocalStream::connect(name)
            .await
            .map_err(ClientError::Connect)?;
        // Windows pipe 이름과 공용 폴더(`--endpoint /tmp/…`)의 소켓 이름은 남이 먼저 차지할 수 있어, 토큰을 보내기 전에
        // 서버가 같은 OS 사용자인지 봅니다.
        #[cfg(windows)]
        crate::winpipe::verify_server(&stream).map_err(ClientError::Connect)?;
        #[cfg(unix)]
        verify_socket_server(&stream).map_err(ClientError::Connect)?;
        Ok(RpcClient::from_stream(stream))
    }

    /// 연결하고 토큰으로 인증합니다.
    pub async fn connect_with_token(
        endpoint: &Endpoint,
        token: &str,
    ) -> Result<(RpcClient, HelloResult), ClientError> {
        let client = RpcClient::connect(endpoint).await?;
        let hello = client.hello(token).await?;
        Ok((client, hello))
    }

    /// 이미 열린 스트림 위에 클라이언트를 만듭니다(테스트·내장용).
    pub fn from_stream<S>(io: S) -> RpcClient
    where
        S: AsyncRead + AsyncWrite + Send + 'static,
    {
        let (rd, wr) = tokio::io::split(io);
        let (out_tx, out_rx) = mpsc::channel::<Frame>(64);
        let shared = Arc::new(Shared::default());
        tokio::spawn(write_loop(FramedWrite::new(wr, FrameCodec::new()), out_rx));
        let reader = tokio::spawn(read_loop(
            FramedRead::new(rd, FrameCodec::new()),
            shared.clone(),
        ));
        RpcClient {
            out: out_tx,
            shared,
            next_id: Arc::new(AtomicU64::new(1)),
            timeout: None,
            _reader: Arc::new(ReaderGuard(reader)),
        }
    }

    /// 요청마다 응답 시간 상한을 둡니다(기본 없음).
    pub fn with_timeout(mut self, timeout: Duration) -> RpcClient {
        self.timeout = Some(timeout);
        self
    }

    /// 연결이 끊겼는지.
    pub fn is_closed(&self) -> bool {
        self.shared.closed.load(Ordering::Acquire) || self.out.is_closed()
    }

    /// `auth.hello`.
    pub async fn hello(&self, token: &str) -> Result<HelloResult, ClientError> {
        self.request::<api::m::AuthHello>(HelloParams {
            token: token.into(),
        })
        .await
    }

    /// 타입이 정해진 메서드 호출.
    pub async fn request<M: Method>(&self, params: M::Params) -> Result<M::Result, ClientError> {
        self.call(M::NAME, params).await
    }

    /// 이름으로 부르는 호출.
    pub async fn call<P: Serialize, R: DeserializeOwned>(
        &self,
        method: &str,
        params: P,
    ) -> Result<R, ClientError> {
        let v = self
            .call_value(
                method,
                serde_json::to_value(params).map_err(ClientError::Encode)?,
            )
            .await?;
        serde_json::from_value(v).map_err(ClientError::Decode)
    }

    /// 결과를 JSON 그대로 받는 호출.
    pub async fn call_value(&self, method: &str, params: Value) -> Result<Value, ClientError> {
        match self.call_raw(method, params).await? {
            Reply::Json(r) => r.map_err(ClientError::Rpc),
            Reply::Binary(_) => Err(ClientError::Unexpected("JSON 대신 바이너리 응답")),
        }
    }

    /// 결과가 바이너리 프레임으로 오는 호출. 서버가 `data_base64` JSON으로 답하면 풀어서 돌려줍니다.
    pub async fn call_binary<P: Serialize>(
        &self,
        method: &str,
        params: P,
    ) -> Result<Bytes, ClientError> {
        let params = serde_json::to_value(params).map_err(ClientError::Encode)?;
        match self.call_raw(method, params).await? {
            Reply::Binary(b) => Ok(b),
            Reply::Json(Err(e)) => Err(ClientError::Rpc(e)),
            Reply::Json(Ok(v)) => {
                let r: BlobReadResult = serde_json::from_value(v).map_err(ClientError::Decode)?;
                let data = r
                    .data_base64
                    .ok_or(ClientError::Unexpected("바이너리 결과 없음"))?;
                base64::engine::general_purpose::STANDARD
                    .decode(data)
                    .map(Bytes::from)
                    .map_err(|_| ClientError::Unexpected("base64 결과가 잘못됨"))
            }
        }
    }

    /// `blob.read`를 바이너리 프레임으로 읽습니다.
    pub async fn blob_read(&self, mut params: BlobReadParams) -> Result<Bytes, ClientError> {
        params.binary = Some(true);
        self.call_binary(<api::m::BlobRead as Method>::NAME, params)
            .await
    }

    async fn call_raw(&self, method: &str, params: Value) -> Result<Reply, ClientError> {
        if self.is_closed() {
            return Err(ClientError::Closed);
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let req = Request::new(Id::Num(id as i64), method, params);
        let frame = Frame::json(&req).map_err(ClientError::Encode)?;
        if frame.wire_len() > MAX_FRAME {
            return Err(ClientError::TooLarge);
        }
        let (tx, rx) = oneshot::channel();
        self.shared.pending.lock().insert(id, tx);
        let _guard = PendingGuard {
            shared: &self.shared,
            id,
        };
        self.out
            .send(frame)
            .await
            .map_err(|_| ClientError::Closed)?;
        let reply = match self.timeout {
            Some(d) => tokio::time::timeout(d, rx)
                .await
                .map_err(|_| ClientError::Timeout)?,
            None => rx.await,
        };
        reply.map_err(|_| ClientError::Closed)
    }

    /// 이벤트 구독. 알림 묶음(서버의 `events` 한 프레임)이 하나씩 나오는 스트림을 돌려줍니다.
    /// 주제가 비면 전부. 스트림을 버리면 구독을 끝냅니다.
    pub async fn subscribe(&self, topics: Vec<String>) -> Result<Subscription, ClientError> {
        let local = self.shared.next_sub.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel(SUBSCRIPTION_QUEUE);
        // 응답보다 알림이 먼저 올 수 있으므로 먼저 등록합니다.
        self.shared.subs.lock().insert(
            local,
            SubSlot {
                topics: topics.clone(),
                tx,
                dropped: 0,
            },
        );
        let params = SubscribeParams {
            topics: if topics.is_empty() {
                None
            } else {
                Some(topics)
            },
        };
        match self.request::<api::m::EventsSubscribe>(params).await {
            Ok(SubscribeResult { subscription }) => Ok(Subscription {
                rx,
                shared: self.shared.clone(),
                out: self.out.downgrade(),
                next_id: self.next_id.clone(),
                local,
                server: subscription,
                _conn: self._reader.clone(),
            }),
            Err(e) => {
                self.shared.subs.lock().remove(&local);
                Err(e)
            }
        }
    }
}

/// 이벤트 구독 스트림. 항목 하나가 알림 묶음 하나입니다. 연결이 끊기면 끝납니다.
///
/// 클라이언트를 먼저 버려도 구독이 살아 있는 동안은 연결(읽기 작업)을 유지합니다. 그러지 않으면 읽기 작업이
/// 멈춘 채 스트림이 끝나지도 않아 영원히 기다리게 됩니다.
pub struct Subscription {
    rx: mpsc::Receiver<Vec<Notification>>,
    shared: Arc<Shared>,
    out: mpsc::WeakSender<Frame>,
    next_id: Arc<AtomicU64>,
    local: u64,
    server: u64,
    // Drop 순서상 마지막: 서버 쪽 구독 해제를 보낸 뒤 연결을 닫습니다.
    _conn: Arc<ReaderGuard>,
}

impl Subscription {
    /// 서버 쪽 구독 번호.
    pub fn id(&self) -> u64 {
        self.server
    }
}

impl Stream for Subscription {
    type Item = Vec<Notification>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.rx.poll_recv(cx)
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.shared.subs.lock().remove(&self.local);
        // 서버 쪽 구독도 끝냅니다(응답은 기다리지 않음).
        if let Some(out) = self.out.upgrade() {
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            let req = Request::new(
                Id::Num(id as i64),
                <api::m::EventsUnsubscribe as Method>::NAME,
                serde_json::to_value(UnsubscribeParams {
                    subscription: self.server,
                })
                .unwrap_or(Value::Null),
            );
            if let Ok(frame) = Frame::json(&req) {
                let _ = out.try_send(frame);
            }
        }
    }
}

async fn write_loop<W>(mut sink: FramedWrite<W, FrameCodec>, mut rx: mpsc::Receiver<Frame>)
where
    W: AsyncWrite + Unpin + Send + 'static,
{
    while let Some(frame) = rx.recv().await {
        if let Err(e) = sink.feed(frame).await {
            debug!(error = %e, "요청 쓰기 실패");
            break;
        }
        // 쌓인 요청을 모아 한 번에 flush.
        let mut failed = false;
        while let Ok(f) = rx.try_recv() {
            if sink.feed(f).await.is_err() {
                failed = true;
                break;
            }
        }
        if failed || sink.flush().await.is_err() {
            break;
        }
    }
}

async fn read_loop<R>(mut reader: FramedRead<R, FrameCodec>, shared: Arc<Shared>)
where
    R: AsyncRead + Unpin + Send + 'static,
{
    while let Some(frame) = reader.next().await {
        match frame {
            Ok(Frame::Binary { id, data }) => {
                if let Some(tx) = shared.pending.lock().remove(&id) {
                    let _ = tx.send(Reply::Binary(data));
                }
            }
            Ok(Frame::Json(bytes)) => match Message::parse(&bytes) {
                Ok(Message::Response(r)) => {
                    let Some(id) = r.id.as_ref().and_then(Id::as_u64) else {
                        debug!("ID 없는 오류 응답");
                        continue;
                    };
                    if let Some(tx) = shared.pending.lock().remove(&id) {
                        let _ = tx.send(Reply::Json(r.outcome));
                    }
                }
                Ok(Message::Notification(n)) if n.method == EVENTS_METHOD => {
                    dispatch_events(&shared, n.params)
                }
                Ok(_) => {}
                Err(_) => debug!("해석할 수 없는 메시지"),
            },
            Err(e) => {
                debug!(error = %e, "프레임 오류로 연결 종료");
                break;
            }
        }
    }
    shared.closed.store(true, Ordering::Release);
    // 기다리던 요청은 Closed, 구독 스트림은 끝납니다.
    shared.pending.lock().clear();
    shared.subs.lock().clear();
}

/// 주제와 무관하게 모든 구독에 전하는 연결 수준 알림(버린 알림 수).
fn is_connection_notice(n: &Notification) -> bool {
    matches!(n, Notification::EventsDropped { .. })
}

/// `events` 알림 하나를 구독마다 나눠 줍니다. 모르는 종류의 알림은 건너뜁니다(새 데몬과의 호환).
fn dispatch_events(shared: &Shared, params: Value) {
    let Some(Value::Array(items)) = params.get("batch").cloned() else {
        return;
    };
    let batch: Vec<Notification> = items
        .into_iter()
        .filter_map(|v| serde_json::from_value(v).ok())
        .collect();
    if batch.is_empty() {
        return;
    }
    let mut subs = shared.subs.lock();
    for slot in subs.values_mut() {
        let mut mine: Vec<Notification> = batch
            .iter()
            .filter(|n| is_connection_notice(n) || topic_matches(&slot.topics, n.kind()))
            .cloned()
            .collect();
        if mine.is_empty() {
            continue;
        }
        let real = mine.len() as u64;
        let prev = slot.dropped;
        if prev > 0 {
            mine.insert(0, Notification::EventsDropped { count: prev });
        }
        match slot.tx.try_send(mine) {
            Ok(()) => slot.dropped = 0,
            Err(mpsc::error::TrySendError::Full(_)) => slot.dropped = prev.saturating_add(real),
            Err(mpsc::error::TrySendError::Closed(_)) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_server_must_be_the_same_user() {
        assert!(same_user(Some(1000), 1000).is_ok());
        // 루트가 띄운 소켓도 다른 사용자입니다.
        for server in [Some(0), Some(1001), None] {
            let e = same_user(server, 1000).unwrap_err();
            assert_eq!(e.kind(), std::io::ErrorKind::PermissionDenied, "{server:?}");
        }
    }
}
