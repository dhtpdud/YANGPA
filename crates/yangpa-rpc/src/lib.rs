//! 양파 메신저(가칭) 로컬 API: JSON-RPC 2.0 타입, 길이 접두 프레이밍, 클라이언트·서버, 모든 메서드의 타입.
//!
//! GUI(Tauri)·CLI·MCP 서버가 이 크레이트로 데몬에 붙고, 데몬은 [`server::RpcHandler`]를 구현합니다.
//! 계약은 `docs/api.md`입니다.

pub mod api;
pub mod client;
pub mod endpoint;
pub mod framing;
pub mod jsonrpc;
pub mod server;
#[cfg(windows)]
mod winpipe;

pub use client::{ClientError, RpcClient, Subscription};
pub use endpoint::Endpoint;
pub use jsonrpc::RpcError;
pub use server::{
    AuthInfo, ConnCtx, EventHub, RpcHandler, RpcOutput, ServerConfig, ServerHandle, serve,
    serve_with,
};

/// 로컬 API 프로토콜 번호(`system.version`의 `protocol`). 호환되지 않게 바뀌면 올립니다.
pub const PROTOCOL_VERSION: u32 = 1;
