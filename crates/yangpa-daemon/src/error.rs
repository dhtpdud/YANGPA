//! 데몬 내부 오류와 로컬 API 오류 코드 변환.
//!
//! 오류 문구는 로그와 API 응답으로 나가므로 메시지 내용·키를 넣지 않습니다.

use yangpa_core::CoreError;
use yangpa_crypto::CryptoError;
use yangpa_net::NetError;
use yangpa_rpc::RpcError;
use yangpa_store::StoreError;
use yangpa_sync::AuthError;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("아직 신원이 없습니다")]
    NoIdentity,
    #[error("없음: {0}")]
    NotFound(String),
    #[error("권한 없음: {0}")]
    Forbidden(String),
    #[error("잘못된 입력: {0}")]
    InvalidInput(String),
    #[error("네트워크 오류: {0}")]
    Network(String),
    /// 상대가 이 기기를 인증된 피어로 알지 않음(`Unauthenticated`). 이쪽만 상대를 기억한 경우를 가려 내 인증을 다시
    /// 보낼 때 씁니다.
    #[error("네트워크 오류: 상대가 이 기기를 인증하지 않음")]
    PeerUnauthenticated,
    #[error("충돌·대기: {0}")]
    Conflict(String),
    #[error("아직 구현되지 않음: {0}")]
    NotImplemented(&'static str),
    #[error("데몬이 종료 중입니다")]
    ShuttingDown,
    #[error("저장소 오류: {0}")]
    Store(#[from] StoreError),
    #[error("암호 오류: {0}")]
    Crypto(#[from] CryptoError),
    #[error("형식 오류: {0}")]
    Core(#[from] CoreError),
    #[error("권한 검사 실패: {0}")]
    Auth(#[from] AuthError),
    #[error("내부 오류: {0}")]
    Internal(String),
}

impl Error {
    pub fn not_found(what: impl Into<String>) -> Self {
        Error::NotFound(what.into())
    }
    pub fn forbidden(what: impl Into<String>) -> Self {
        Error::Forbidden(what.into())
    }
    pub fn invalid(what: impl Into<String>) -> Self {
        Error::InvalidInput(what.into())
    }
    pub fn conflict(what: impl Into<String>) -> Self {
        Error::Conflict(what.into())
    }
    pub fn internal(what: impl Into<String>) -> Self {
        Error::Internal(what.into())
    }
}

impl From<NetError> for Error {
    fn from(e: NetError) -> Self {
        Error::Network(e.to_string())
    }
}

impl From<postcard::Error> for Error {
    fn from(e: postcard::Error) -> Self {
        Error::Internal(format!("직렬화 실패: {e}"))
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Internal(format!("JSON 오류: {e}"))
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Internal(format!("입출력 오류: {e}"))
    }
}

impl From<Error> for RpcError {
    fn from(e: Error) -> Self {
        match e {
            Error::NoIdentity => RpcError::no_identity(),
            Error::NotFound(m) => RpcError::not_found(m),
            Error::Forbidden(m) => RpcError::forbidden(m),
            Error::InvalidInput(m) => RpcError::invalid_input(m),
            Error::Network(m) => RpcError::network(m),
            e @ Error::PeerUnauthenticated => RpcError::network(e.to_string()),
            Error::Conflict(m) => RpcError::conflict(m),
            Error::NotImplemented(what) => not_implemented(what),
            Error::ShuttingDown => RpcError::conflict("데몬이 종료 중입니다"),
            Error::Store(StoreError::NotFound) => RpcError::not_found("없음"),
            Error::Store(StoreError::InvalidInput(m)) => RpcError::invalid_input(m),
            Error::Store(StoreError::LimitExceeded(m)) => RpcError::invalid_input(m),
            Error::Auth(a) => RpcError::forbidden(a.to_string()),
            Error::Core(c) => RpcError::invalid_input(c.to_string()),
            other => RpcError::internal(other.to_string()),
        }
    }
}

/// 다른 작업 영역이 맡을 메서드의 자리표시 오류.
pub fn not_implemented(what: &str) -> RpcError {
    RpcError::internal(format!("아직 구현되지 않음: {what}"))
        .with_data(serde_json::json!({ "not_implemented": what }))
}
