//! JSON-RPC 2.0 메시지 타입과 오류 코드.
//!
//! 일괄 요청(배열)은 쓰지 않습니다. 이벤트는 서버가 `events` 알림 하나에 묶어 보내므로 필요가 없습니다.

use std::fmt;

use serde::de::{self, Deserializer};
use serde::ser::{SerializeMap, Serializer};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 프로토콜 버전 문자열.
pub const VERSION: &str = "2.0";

// JSON-RPC 표준 오류.
pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;

// 양파 API 오류(docs/api.md).
/// 인증 필요(`auth.hello` 전이거나 토큰이 틀림).
pub const ERR_AUTH_REQUIRED: i64 = 1001;
/// 토큰 범위 밖(범위·공간 제한·사람 전용 메서드).
pub const ERR_OUT_OF_SCOPE: i64 = 1002;
/// 공간 권한 없음.
pub const ERR_FORBIDDEN: i64 = 1003;
/// 대상 없음.
pub const ERR_NOT_FOUND: i64 = 1004;
/// 잘못된 입력.
pub const ERR_INVALID_INPUT: i64 = 1005;
/// 사람 확인 대기(`data.confirmation_id`).
pub const ERR_CONFIRMATION_REQUIRED: i64 = 1006;
/// 휘발성 채널 API 읽기 거부.
pub const ERR_VOLATILE_READ_DENIED: i64 = 1007;
/// 아직 신원 없음.
pub const ERR_NO_IDENTITY: i64 = 1008;
/// 네트워크 오류.
pub const ERR_NETWORK: i64 = 1009;
/// 충돌·대기(커밋 확정 노드 오프라인 등).
pub const ERR_CONFLICT: i64 = 1010;

/// 오류 코드의 짧은 설명(CLI·MCP 표시용).
pub fn describe_code(code: i64) -> &'static str {
    match code {
        PARSE_ERROR => "JSON 해석 실패",
        INVALID_REQUEST => "잘못된 요청",
        METHOD_NOT_FOUND => "없는 메서드",
        INVALID_PARAMS => "잘못된 인자",
        INTERNAL_ERROR => "내부 오류",
        ERR_AUTH_REQUIRED => "인증 필요",
        ERR_OUT_OF_SCOPE => "토큰 범위 밖",
        ERR_FORBIDDEN => "권한 없음",
        ERR_NOT_FOUND => "없음",
        ERR_INVALID_INPUT => "잘못된 입력",
        ERR_CONFIRMATION_REQUIRED => "사람 확인 대기",
        ERR_VOLATILE_READ_DENIED => "휘발성 채널 읽기 거부",
        ERR_NO_IDENTITY => "신원 없음",
        ERR_NETWORK => "네트워크 오류",
        ERR_CONFLICT => "충돌·대기",
        _ => "오류",
    }
}

/// 요청 ID. 숫자(정수) 또는 문자열.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Id {
    Num(i64),
    Str(String),
}

impl Id {
    /// 바이너리 프레임에 쓸 수 있는 ID(음이 아닌 정수)인지.
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Id::Num(n) => u64::try_from(*n).ok(),
            Id::Str(_) => None,
        }
    }
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Id::Num(n) => write!(f, "{n}"),
            Id::Str(s) => write!(f, "{s:?}"),
        }
    }
}

impl From<u32> for Id {
    fn from(v: u32) -> Self {
        Id::Num(v as i64)
    }
}

impl From<i64> for Id {
    fn from(v: i64) -> Self {
        Id::Num(v)
    }
}

impl From<&str> for Id {
    fn from(v: &str) -> Self {
        Id::Str(v.to_owned())
    }
}

/// `"jsonrpc": "2.0"` 표식.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TwoPointZero;

impl Serialize for TwoPointZero {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(VERSION)
    }
}

impl<'de> Deserialize<'de> for TwoPointZero {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = String::deserialize(d)?;
        if v == VERSION {
            Ok(TwoPointZero)
        } else {
            Err(de::Error::custom("jsonrpc 버전은 \"2.0\"이어야 함"))
        }
    }
}

/// 오류 객체. 서버 핸들러·클라이언트가 그대로 오류 타입으로도 씁니다([`RpcError`]).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, thiserror::Error)]
#[error("{message} (코드 {code})")]
pub struct ErrorObject {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// 핸들러·클라이언트가 쓰는 RPC 오류.
pub type RpcError = ErrorObject;

impl ErrorObject {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        ErrorObject {
            code,
            message: message.into(),
            data: None,
        }
    }

    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }

    pub fn parse_error() -> Self {
        Self::new(PARSE_ERROR, "JSON 해석 실패")
    }

    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(INVALID_REQUEST, message)
    }

    pub fn method_not_found(method: &str) -> Self {
        // 메서드 이름은 길이를 제한해 그대로 돌려줍니다(로그·응답 크기 보호).
        let shown: String = method.chars().take(64).collect();
        Self::new(METHOD_NOT_FOUND, format!("없는 메서드: {shown}"))
    }

    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(INVALID_PARAMS, message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(INTERNAL_ERROR, message)
    }

    pub fn auth_required() -> Self {
        Self::new(ERR_AUTH_REQUIRED, "인증이 필요합니다(auth.hello)")
    }

    pub fn out_of_scope(message: impl Into<String>) -> Self {
        Self::new(ERR_OUT_OF_SCOPE, message)
    }

    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(ERR_FORBIDDEN, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(ERR_NOT_FOUND, message)
    }

    pub fn invalid_input(message: impl Into<String>) -> Self {
        Self::new(ERR_INVALID_INPUT, message)
    }

    /// 사람 확인 대기. `data.confirmation_id`에 확인 ID를 넣습니다.
    pub fn confirmation_required(confirmation_id: impl Into<String>) -> Self {
        Self::new(ERR_CONFIRMATION_REQUIRED, "사람 확인이 필요합니다")
            .with_data(serde_json::json!({ "confirmation_id": confirmation_id.into() }))
    }

    pub fn volatile_read_denied() -> Self {
        Self::new(
            ERR_VOLATILE_READ_DENIED,
            "휘발성 채널은 채널별로 API 읽기를 허용하기 전까지 자동화 토큰으로 읽을 수 없습니다",
        )
    }

    pub fn no_identity() -> Self {
        Self::new(ERR_NO_IDENTITY, "아직 신원이 없습니다(identity.create)")
    }

    pub fn network(message: impl Into<String>) -> Self {
        Self::new(ERR_NETWORK, message)
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(ERR_CONFLICT, message)
    }

    /// `1006` 오류의 확인 ID.
    pub fn confirmation_id(&self) -> Option<&str> {
        if self.code != ERR_CONFIRMATION_REQUIRED {
            return None;
        }
        self.data.as_ref()?.get("confirmation_id")?.as_str()
    }
}

/// 요청(ID 있음).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub jsonrpc: TwoPointZero,
    pub id: Id,
    pub method: String,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub params: Value,
}

impl Request {
    pub fn new(id: impl Into<Id>, method: impl Into<String>, params: Value) -> Self {
        Request {
            jsonrpc: TwoPointZero,
            id: id.into(),
            method: method.into(),
            params,
        }
    }
}

/// 알림(ID 없음, 응답 없음).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Notification {
    pub jsonrpc: TwoPointZero,
    pub method: String,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub params: Value,
}

impl Notification {
    pub fn new(method: impl Into<String>, params: Value) -> Self {
        Notification {
            jsonrpc: TwoPointZero,
            method: method.into(),
            params,
        }
    }
}

/// 응답. 성공이면 `result`(null 포함), 실패면 `error`가 실립니다.
#[derive(Clone, Debug, PartialEq)]
pub struct Response {
    /// 요청 ID. 요청을 해석하지 못했으면 `None`(JSON `null`).
    pub id: Option<Id>,
    pub outcome: Result<Value, ErrorObject>,
}

impl Response {
    pub fn ok(id: Id, result: Value) -> Self {
        Response {
            id: Some(id),
            outcome: Ok(result),
        }
    }

    pub fn err(id: Option<Id>, error: ErrorObject) -> Self {
        Response {
            id,
            outcome: Err(error),
        }
    }
}

impl Serialize for Response {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut m = s.serialize_map(Some(3))?;
        m.serialize_entry("jsonrpc", VERSION)?;
        m.serialize_entry("id", &self.id)?;
        match &self.outcome {
            Ok(v) => m.serialize_entry("result", v)?,
            Err(e) => m.serialize_entry("error", e)?,
        }
        m.end()
    }
}

impl<'de> Deserialize<'de> for Response {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = RawMessage::deserialize(d)?;
        match raw.classify() {
            Ok(Message::Response(r)) => Ok(r),
            Ok(_) => Err(de::Error::custom("응답이 아님")),
            Err(e) => Err(de::Error::custom(e.error.message)),
        }
    }
}

/// 받은 메시지 하나.
#[derive(Clone, Debug, PartialEq)]
pub enum Message {
    Request(Request),
    Notification(Notification),
    Response(Response),
}

/// 메시지 해석 실패. `error`를 그대로 응답으로 돌려주면 됩니다.
#[derive(Clone, Debug, PartialEq)]
pub struct ParseFailure {
    /// 알아낸 요청 ID(있으면 응답에 씀).
    pub id: Option<Id>,
    pub error: ErrorObject,
}

/// 필드가 있으면(값이 null이어도) `Some`.
fn present<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(d).map(Some)
}

#[derive(Deserialize)]
struct RawMessage {
    #[serde(default, deserialize_with = "present")]
    jsonrpc: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    id: Option<Value>,
    #[serde(default)]
    method: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    params: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    result: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    error: Option<Value>,
}

fn parse_id(v: Value) -> Option<Id> {
    match v {
        Value::Number(n) => n.as_i64().map(Id::Num),
        Value::String(s) => Some(Id::Str(s)),
        _ => None,
    }
}

impl RawMessage {
    fn classify(self) -> Result<Message, ParseFailure> {
        let fail = |id: Option<Id>, msg: &str| ParseFailure {
            id,
            error: ErrorObject::invalid_request(msg),
        };
        let id = match self.id {
            None => None,
            Some(Value::Null) => Some(None),
            Some(v) => match parse_id(v) {
                Some(id) => Some(Some(id)),
                None => return Err(fail(None, "id는 정수나 문자열이어야 함")),
            },
        };
        let known_id = id.clone().flatten();
        if self.jsonrpc.as_ref().and_then(Value::as_str) != Some(VERSION) {
            return Err(fail(known_id, "jsonrpc 버전은 \"2.0\"이어야 함"));
        }
        let params = match self.params {
            None | Some(Value::Null) => Value::Null,
            Some(p @ (Value::Object(_) | Value::Array(_))) => p,
            Some(_) => return Err(fail(known_id, "params는 객체나 배열이어야 함")),
        };
        if let Some(method) = self.method {
            let Value::String(method) = method else {
                return Err(fail(known_id, "method는 문자열이어야 함"));
            };
            return match id {
                None => Ok(Message::Notification(Notification {
                    jsonrpc: TwoPointZero,
                    method,
                    params,
                })),
                Some(None) => Err(fail(None, "요청 id는 null일 수 없음")),
                Some(Some(id)) => Ok(Message::Request(Request {
                    jsonrpc: TwoPointZero,
                    id,
                    method,
                    params,
                })),
            };
        }
        let Some(id) = id else {
            return Err(fail(None, "method도 id도 없음"));
        };
        let outcome = match (self.result, self.error) {
            (Some(r), None) => Ok(r),
            (None, Some(e)) => match serde_json::from_value::<ErrorObject>(e) {
                Ok(e) => Err(e),
                Err(_) => return Err(fail(id, "error 객체 형식이 잘못됨")),
            },
            _ => return Err(fail(id, "응답에는 result나 error 중 하나만 있어야 함")),
        };
        Ok(Message::Response(Response { id, outcome }))
    }
}

impl Message {
    /// 프레임 본문 하나를 해석합니다.
    pub fn parse(bytes: &[u8]) -> Result<Message, ParseFailure> {
        // 먼저 형식이 맞는 JSON인지 보고(-32700), 그다음 메시지 모양을 검사합니다(-32600).
        let value: Value = serde_json::from_slice(bytes).map_err(|_| ParseFailure {
            id: None,
            error: ErrorObject::parse_error(),
        })?;
        if !value.is_object() {
            return Err(ParseFailure {
                id: None,
                error: ErrorObject::invalid_request("메시지는 JSON 객체여야 함"),
            });
        }
        let raw: RawMessage = serde_json::from_value(value).map_err(|_| ParseFailure {
            id: None,
            error: ErrorObject::invalid_request("메시지 형식이 잘못됨"),
        })?;
        raw.classify()
    }

    pub fn to_vec(&self) -> Result<Vec<u8>, serde_json::Error> {
        match self {
            Message::Request(r) => serde_json::to_vec(r),
            Message::Notification(n) => serde_json::to_vec(n),
            Message::Response(r) => serde_json::to_vec(r),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_roundtrip() {
        let r = Request::new(7i64, "space.list", Value::Null);
        let s = serde_json::to_string(&r).unwrap();
        assert_eq!(s, r#"{"jsonrpc":"2.0","id":7,"method":"space.list"}"#);
        assert_eq!(Message::parse(s.as_bytes()).unwrap(), Message::Request(r));

        let r = Request::new("abc", "crew.create", json!({"name": "게임 모임"}));
        let s = serde_json::to_vec(&r).unwrap();
        assert_eq!(Message::parse(&s).unwrap(), Message::Request(r));
    }

    #[test]
    fn notification_and_response() {
        let n = Notification::new("events", json!({"batch": []}));
        let s = serde_json::to_vec(&n).unwrap();
        assert_eq!(Message::parse(&s).unwrap(), Message::Notification(n));

        let ok = Response::ok(Id::Num(1), Value::Null);
        let s = serde_json::to_string(&ok).unwrap();
        assert_eq!(s, r#"{"jsonrpc":"2.0","id":1,"result":null}"#);
        assert_eq!(Message::parse(s.as_bytes()).unwrap(), Message::Response(ok));

        let err = Response::err(Some(Id::Num(2)), ErrorObject::confirmation_required("c1"));
        let s = serde_json::to_string(&err).unwrap();
        let back: Response = serde_json::from_str(&s).unwrap();
        assert_eq!(back, err);
        assert_eq!(back.outcome.unwrap_err().confirmation_id(), Some("c1"));

        let nullid = Response::err(None, ErrorObject::parse_error());
        let s = serde_json::to_string(&nullid).unwrap();
        assert_eq!(
            s,
            r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"JSON 해석 실패"}}"#
        );
        assert_eq!(serde_json::from_str::<Response>(&s).unwrap(), nullid);
    }

    #[test]
    fn invalid_messages() {
        assert_eq!(
            Message::parse(b"{not json").unwrap_err().error.code,
            PARSE_ERROR
        );
        assert_eq!(
            Message::parse(b"[1,2]").unwrap_err().error.code,
            INVALID_REQUEST
        );
        let e = Message::parse(br#"{"jsonrpc":"1.0","id":3,"method":"x"}"#).unwrap_err();
        assert_eq!(e.error.code, INVALID_REQUEST);
        assert_eq!(e.id, Some(Id::Num(3)));
        let e = Message::parse(br#"{"jsonrpc":"2.0","id":3,"method":"x","params":5}"#).unwrap_err();
        assert_eq!(e.error.code, INVALID_REQUEST);
        let e = Message::parse(br#"{"jsonrpc":"2.0","id":null,"method":"x"}"#).unwrap_err();
        assert_eq!(e.error.code, INVALID_REQUEST);
        let e = Message::parse(br#"{"jsonrpc":"2.0","id":1.5,"method":"x"}"#).unwrap_err();
        assert_eq!(e.error.code, INVALID_REQUEST);
        let e = Message::parse(
            br#"{"jsonrpc":"2.0","id":1,"result":1,"error":{"code":1,"message":"m"}}"#,
        )
        .unwrap_err();
        assert_eq!(e.error.code, INVALID_REQUEST);
        assert!(serde_json::from_str::<TwoPointZero>(r#""3.0""#).is_err());
    }

    #[test]
    fn id_u64() {
        assert_eq!(Id::Num(5).as_u64(), Some(5));
        assert_eq!(Id::Num(-1).as_u64(), None);
        assert_eq!(Id::Str("5".into()).as_u64(), None);
    }
}
