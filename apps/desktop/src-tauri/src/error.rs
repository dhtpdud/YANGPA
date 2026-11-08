//! 명령 오류. 웹뷰에는 JSON-RPC 오류 객체 모양(`{code, message, data?}`)으로 넘겨
//! 프런트엔드가 데몬 오류와 똑같이 다루게 합니다(`toRpcError`).

use serde::Serialize;
use serde_json::Value;
use yangpa_rpc::ClientError;

/// 데스크톱 앱 전용 코드: 데몬에 연결되지 않음(docs/api.md 보충, 프런트엔드 `ErrorCode.DaemonUnavailable`).
pub const DAEMON_UNAVAILABLE: i64 = -32001;
pub const INVALID_REQUEST: i64 = -32600;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL: i64 = -32603;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CmdError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl CmdError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        CmdError {
            code,
            message: message.into(),
            data: None,
        }
    }

    pub fn unavailable() -> Self {
        CmdError::new(DAEMON_UNAVAILABLE, "데몬에 연결되지 않았습니다")
    }

    pub fn invalid_params(message: impl Into<String>) -> Self {
        CmdError::new(INVALID_PARAMS, message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        CmdError::new(INTERNAL, message)
    }
}

impl std::fmt::Display for CmdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (코드 {})", self.message, self.code)
    }
}

impl std::error::Error for CmdError {}

impl From<ClientError> for CmdError {
    fn from(e: ClientError) -> Self {
        match e {
            // 데몬 오류는 코드·데이터(예: 1006의 confirmation_id)를 그대로 넘깁니다.
            ClientError::Rpc(r) => CmdError {
                code: r.code,
                message: r.message,
                data: r.data,
            },
            ClientError::Connect(_) | ClientError::Closed => CmdError::unavailable(),
            ClientError::Timeout => CmdError::internal("데몬 응답 시간이 초과됐습니다"),
            ClientError::Encode(_) | ClientError::TooLarge => {
                CmdError::invalid_params(e.to_string())
            }
            ClientError::Decode(_) | ClientError::Unexpected(_) => {
                CmdError::internal(e.to_string())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use yangpa_rpc::RpcError;

    #[test]
    fn rpc_error_keeps_code_and_data() {
        let mut r = RpcError::new(1006, "확인 필요");
        r.data = Some(serde_json::json!({"confirmation_id": "c1"}));
        let e = CmdError::from(ClientError::Rpc(r));
        assert_eq!(e.code, 1006);
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["data"]["confirmation_id"], "c1");
        assert_eq!(v["message"], "확인 필요");
    }

    #[test]
    fn connection_errors_map_to_unavailable() {
        assert_eq!(CmdError::from(ClientError::Closed).code, DAEMON_UNAVAILABLE);
        let io = std::io::Error::from(std::io::ErrorKind::NotFound);
        assert_eq!(
            CmdError::from(ClientError::Connect(io)).code,
            DAEMON_UNAVAILABLE
        );
        let v = serde_json::to_value(CmdError::unavailable()).unwrap();
        assert!(v.get("data").is_none());
    }
}
