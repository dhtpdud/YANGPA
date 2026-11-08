//! 시험용 가짜 데몬: 메모리 스트림 위에서 실제 `yangpa-rpc` 서버로 돕니다.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use serde_json::{Value, json};
use yangpa_core::ids::{BlobId, ChannelId, EventId, SpaceId};
use yangpa_rpc::api::{BLOB_READ_MAX_LEN, BlobReadParams, Scope};
use yangpa_rpc::server::serve_connection;
use yangpa_rpc::{
    AuthInfo, ConnCtx, EventHub, RpcClient, RpcError, RpcHandler, RpcOutput, ServerConfig,
};

pub fn space() -> SpaceId {
    SpaceId::from_bytes([1; 16])
}

pub fn channel() -> ChannelId {
    ChannelId::from_bytes([4; 16])
}

pub fn blob() -> BlobId {
    BlobId::from_bytes([2; 32])
}

pub fn missing_blob() -> BlobId {
    BlobId::from_bytes([0xee; 32])
}

pub fn key_from() -> EventId {
    EventId::from_bytes([3; 32])
}

pub struct FakeDaemon {
    pub data: Vec<u8>,
    /// `blob.read` 호출 수.
    pub reads: AtomicUsize,
    /// `blob.read` 응답 지연(밀리초).
    pub delay_ms: AtomicU64,
    in_flight: AtomicUsize,
    /// 동시에 처리 중이던 `blob.read` 최대 수.
    pub max_in_flight: AtomicUsize,
    /// 받은 `system.shutdown` 수.
    pub shutdowns: AtomicUsize,
    /// `blob.status`가 알려 줄 크기(0이면 그 메서드가 없는 데몬처럼 답함).
    pub status_size: AtomicU64,
}

impl FakeDaemon {
    pub fn new(data: Vec<u8>) -> Arc<FakeDaemon> {
        Arc::new(FakeDaemon {
            data,
            reads: AtomicUsize::new(0),
            delay_ms: AtomicU64::new(0),
            in_flight: AtomicUsize::new(0),
            max_in_flight: AtomicUsize::new(0),
            shutdowns: AtomicUsize::new(0),
            status_size: AtomicU64::new(0),
        })
    }

    pub fn reads(&self) -> usize {
        self.reads.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl RpcHandler for FakeDaemon {
    async fn authenticate(&self, _token: &str) -> Option<AuthInfo> {
        Some(AuthInfo {
            token_id: "t".into(),
            token_name: "owner".into(),
            scopes: vec![Scope::Owner],
            spaces: Vec::new(),
            automation: false,
        })
    }

    async fn handle(
        &self,
        _ctx: &ConnCtx,
        method: &str,
        params: Value,
    ) -> Result<RpcOutput, RpcError> {
        match method {
            "system.version" => Ok(RpcOutput::Json(json!({"version": "9.9.9", "protocol": 1}))),
            "system.shutdown" => {
                self.shutdowns.fetch_add(1, Ordering::SeqCst);
                Ok(RpcOutput::Json(json!({})))
            }
            "channel.delete" => {
                let mut e = RpcError::new(1006, "사람 확인이 필요합니다");
                e.data = Some(json!({"confirmation_id": "c-1"}));
                Err(e)
            }
            "blob.status" if self.status_size.load(Ordering::SeqCst) > 0 => Ok(RpcOutput::Json(
                json!({"have_chunks": 1, "total_chunks": 1, "seeders_online": 0, "size": self.status_size.load(Ordering::SeqCst)}),
            )),
            "blob.read" => {
                let p: BlobReadParams = serde_json::from_value(params)
                    .map_err(|e| RpcError::invalid_params(e.to_string()))?;
                if p.blob == missing_blob() {
                    return Err(RpcError::not_found("블롭이 없습니다"));
                }
                self.reads.fetch_add(1, Ordering::SeqCst);
                let delay = self.delay_ms.load(Ordering::SeqCst);
                if delay > 0 {
                    let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                    self.max_in_flight.fetch_max(now, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                    self.in_flight.fetch_sub(1, Ordering::SeqCst);
                }
                let off = p.offset.unwrap_or(0) as usize;
                let len = p.len.unwrap_or(BLOB_READ_MAX_LEN).min(BLOB_READ_MAX_LEN) as usize;
                // 끝 이후 offset은 엄격한 데몬처럼 1005로 답합니다.
                if off > 0 && off >= self.data.len() {
                    return Err(RpcError::new(1005, "범위 밖"));
                }
                let end = (off + len).min(self.data.len());
                Ok(RpcOutput::Binary(Bytes::copy_from_slice(
                    &self.data[off..end],
                )))
            }
            _ => Err(RpcError::new(-32601, "없는 메서드")),
        }
    }
}

/// 가짜 데몬에 붙은 인증된 클라이언트.
pub async fn connect(h: Arc<FakeDaemon>) -> RpcClient {
    let (a, b) = tokio::io::duplex(1 << 20);
    tokio::spawn(serve_connection(
        b,
        h,
        EventHub::new(),
        ServerConfig::default(),
    ));
    let c = RpcClient::from_stream(a);
    c.hello("owner").await.expect("hello");
    c
}
