//! 로컬 API의 모든 메서드 요청·응답 타입과 메서드 표(범위·파괴적 여부).
//!
//! - [`types`]: 보기 타입(`MessageView`, `SpaceView` …)과 설정 DTO.
//! - [`params`]: 메서드별 인자·결과 구조체.
//! - [`events`]: `events` 알림 묶음에 실리는 [`Notification`].
//! - [`m`]: 메서드 표지 타입(`m::CrewCreate` …). [`Method`]로 이름·인자·결과 타입을 묶어
//!   클라이언트가 `client.request::<m::CrewCreate>(…)`처럼 타입 맞춰 부를 수 있습니다.
//!
//! 필드 이름은 docs/api.md 그대로(snake_case)입니다. 식별자는 base58 문자열, 이모지 ID는 10진수 문자열,
//! 시각은 유닉스 밀리초 정수입니다.

pub mod events;
mod methods;
pub mod params;
pub mod serde_util;
pub mod types;

use std::fmt;
use std::str::FromStr;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::jsonrpc::RpcError;

pub use events::{
    Notification, NotifySender, NotifyView, default_notification_filter, topic_matches,
};
pub use methods::{METHODS, m};
pub use params::*;
pub use types::*;

/// 메서드 하나: 이름과 인자·결과 타입.
pub trait Method {
    const NAME: &'static str;
    type Params: Serialize + DeserializeOwned + Send + Sync + 'static;
    type Result: Serialize + DeserializeOwned + Send + Sync + 'static;
}

/// 토큰 범위.
///
/// `owner`는 모든 범위를 포함합니다. 나머지는 서로 포함하지 않으므로(최소 권한), 필요한 범위를 모두 적어 발급합니다.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Read,
    Write,
    /// 역할·추방·초대·채널 관리.
    Admin,
    /// 시더·피어·Tor·토큰 설정.
    Settings,
    /// 전부.
    Owner,
}

impl Scope {
    pub const ALL: [Scope; 5] = [
        Scope::Read,
        Scope::Write,
        Scope::Admin,
        Scope::Settings,
        Scope::Owner,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Scope::Read => "read",
            Scope::Write => "write",
            Scope::Admin => "admin",
            Scope::Settings => "settings",
            Scope::Owner => "owner",
        }
    }

    /// 부여된 범위 목록이 이 범위를 만족하는지.
    pub fn satisfied_by(self, granted: &[Scope]) -> bool {
        granted.iter().any(|g| *g == self || *g == Scope::Owner)
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for Scope {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Scope::ALL
            .into_iter()
            .find(|x| x.name() == s.trim())
            .ok_or_else(|| format!("알 수 없는 범위: {s}"))
    }
}

/// 메서드가 공간 제한 토큰(`spaces`가 비어 있지 않은 토큰)과 어떻게 맞물리는지.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpaceRule {
    /// `params.space`가 반드시 있고, 제한 토큰이면 허용 목록에 있어야 합니다.
    Required,
    /// `space?` 선택. 있으면 검사, 없으면 데몬이 결과를 허용 공간으로 걸러 주므로 제한 토큰도 허용.
    OptionalFiltered,
    /// `space?` 선택. 있으면 검사, 없으면 전역 작업이라 제한 토큰은 거부.
    OptionalGlobal,
    /// 공간 인자 없음. 데몬이 결과를 허용 공간으로 걸러 주므로 제한 토큰도 허용.
    Filtered,
    /// 공간 인자 없는 전역 작업. 제한 토큰은 거부.
    Global,
}

/// 자동화(AI) 토큰이 부르면 사람 확인 대기열로 가는지.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Destructive {
    No,
    /// 항상 사람 확인.
    Always,
    /// 대상이 남의 것일 때만(예: 남의 메시지 삭제). 판단은 데몬이 합니다.
    WhenOthers,
}

/// 메서드 표의 한 줄.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MethodSpec {
    pub name: &'static str,
    /// 필요한 범위. `None`이면 인증 없이 부를 수 있음(`auth.hello`, `system.version`).
    pub scope: Option<Scope>,
    pub space: SpaceRule,
    pub destructive: Destructive,
    /// 사람 토큰만(자동화 토큰은 범위가 있어도 거부).
    pub human_only: bool,
}

impl MethodSpec {
    pub const fn new(name: &'static str, scope: Option<Scope>, space: SpaceRule) -> Self {
        MethodSpec {
            name,
            scope,
            space,
            destructive: Destructive::No,
            human_only: false,
        }
    }

    pub const fn destructive(mut self) -> Self {
        self.destructive = Destructive::Always;
        self
    }

    pub const fn destructive_others(mut self) -> Self {
        self.destructive = Destructive::WhenOthers;
        self
    }

    pub const fn human_only(mut self) -> Self {
        self.human_only = true;
        self
    }

    pub const fn is_public(&self) -> bool {
        self.scope.is_none()
    }
}

/// 메서드 표에서 찾기.
pub fn method_spec(method: &str) -> Option<&'static MethodSpec> {
    METHODS.iter().find(|s| s.name == method)
}

/// 메서드에 필요한 토큰 범위. 인증 없이 부를 수 있는 메서드와 없는 메서드는 `None`입니다
/// (구분이 필요하면 [`method_spec`]).
pub fn required_scope(method: &str) -> Option<Scope> {
    method_spec(method).and_then(|s| s.scope)
}

/// 인증 없이 부를 수 있는 메서드인지.
pub fn is_public(method: &str) -> bool {
    method_spec(method).is_some_and(MethodSpec::is_public)
}

/// 자동화 토큰이면 사람 확인이 필요한 메서드인지(대상에 따라 다른 경우 포함).
pub fn is_destructive(method: &str) -> bool {
    method_spec(method).is_some_and(|s| s.destructive != Destructive::No)
}

/// 요청 인자를 해석합니다. 인자가 없으면(`null`) 빈 객체로 봅니다. 모양이 틀리면 `-32602`.
///
/// 위치 인자(배열)는 받지 않습니다. serde는 구조체를 배열로도 읽기 때문에, 배열을 받으면 서버의
/// `params.space` 공간 제한 검사를 건너뛴 채 `space`가 채워질 수 있습니다.
pub fn parse_params<P: DeserializeOwned>(params: Value) -> Result<P, RpcError> {
    let params = match params {
        Value::Null => Value::Object(Default::default()),
        Value::Object(_) => params,
        _ => {
            return Err(RpcError::invalid_params(
                "params는 이름 있는 인자(객체)여야 합니다",
            ));
        }
    };
    serde_json::from_value(params)
        .map_err(|e| RpcError::invalid_params(format!("잘못된 인자: {e}")))
}

/// 결과를 JSON으로 바꿉니다.
pub fn to_result<R: Serialize>(result: &R) -> Result<Value, RpcError> {
    serde_json::to_value(result).map_err(|e| RpcError::internal(format!("결과 직렬화 실패: {e}")))
}

/// 인자 없는 메서드의 인자, 그리고 `{}`를 돌려주는 메서드의 결과.
///
/// 어떤 값이 와도 받아들입니다(`null`, `{}`, 모르는 필드).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Empty;

impl Serialize for Empty {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        s.serialize_map(Some(0))?.end()
    }
}

impl<'de> Deserialize<'de> for Empty {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        serde::de::IgnoredAny::deserialize(d)?;
        Ok(Empty)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn method_table_is_unique_and_documented() {
        let mut seen = HashSet::new();
        for s in METHODS {
            assert!(seen.insert(s.name), "중복 메서드: {}", s.name);
            assert!(s.name.contains('.'), "이름 형식: {}", s.name);
        }
        // docs/api.md의 메서드가 모두 있는지 몇 개 확인.
        for name in [
            "auth.hello",
            "system.version",
            "friend.link.create",
            "message.list",
            "blob.read",
            "seed.coverage",
            "voice.signal",
            "confirm.approve",
            "events.subscribe",
        ] {
            assert!(method_spec(name).is_some(), "{name} 없음");
        }
    }

    #[test]
    fn scopes_and_flags() {
        assert!(is_public("auth.hello"));
        assert!(is_public("system.version"));
        assert!(!is_public("system.status"));
        assert_eq!(required_scope("auth.hello"), None);
        assert_eq!(required_scope("nope.nope"), None);
        assert_eq!(required_scope("message.list"), Some(Scope::Read));
        assert_eq!(required_scope("message.send"), Some(Scope::Write));
        assert_eq!(required_scope("member.kick"), Some(Scope::Admin));
        assert_eq!(required_scope("seed.set"), Some(Scope::Settings));
        assert_eq!(required_scope("token.create"), Some(Scope::Owner));
        assert_eq!(required_scope("system.shutdown"), Some(Scope::Owner));
        for name in [
            "member.kick",
            "message.delete",
            "channel.delete",
            "role.update",
            "role.delete",
            "member.set_roles",
            "invite.create",
            "crew.transfer_ownership",
            "crew.convert_request",
            "crew.convert_accept",
            "crew.leave",
            "crew.disband",
            "token.create",
            "member.ban",
            "member.unban",
            "report.resolve",
        ] {
            assert!(is_destructive(name), "{name}은 파괴적 작업");
        }
        assert_eq!(
            method_spec("report.resolve").unwrap().destructive,
            Destructive::WhenOthers
        );
        for (name, scope) in [
            ("member.ban", Scope::Admin),
            ("member.unban", Scope::Admin),
            ("member.bans", Scope::Admin),
            ("report.list", Scope::Admin),
            ("report.resolve", Scope::Admin),
            ("message.report", Scope::Write),
            ("user.block", Scope::Write),
            ("user.unblock", Scope::Write),
            ("user.blocked", Scope::Read),
        ] {
            assert_eq!(required_scope(name), Some(scope), "{name}");
        }
        for name in ["member.bans", "report.list", "message.report", "user.block"] {
            assert!(!is_destructive(name), "{name}은 파괴적 작업이 아님");
        }
        assert_eq!(method_spec("user.block").unwrap().space, SpaceRule::Global);
        assert_eq!(
            method_spec("message.delete").unwrap().destructive,
            Destructive::WhenOthers
        );
        assert!(!is_destructive("message.send"));
        assert!(!is_destructive("role.create"));
        assert!(method_spec("confirm.approve").unwrap().human_only);
        assert!(method_spec("channel.allow_api_read").unwrap().human_only);
        for name in ["channel.p2p_join", "channel.p2p_leave", "trust.set_space"] {
            let spec = method_spec(name).unwrap();
            assert!(spec.human_only, "{name}");
            assert_eq!(spec.scope, Some(Scope::Write), "{name}");
            assert_eq!(spec.space, SpaceRule::Required, "{name}");
            assert!(!is_destructive(name), "{name}");
        }
        // 사람 신뢰는 공간에 묶이지 않지만 IP 노출 동의라 사람만.
        let trust = method_spec("trust.set_user").unwrap();
        assert!(trust.human_only && trust.space == SpaceRule::Global);
        // 켜면 이 기기의 onion 서비스가 회로를 더 자주 만들어 가드 찾기에 더 드러나므로 사람만.
        let fc = method_spec("net.set_file_circuits").unwrap();
        assert!(fc.human_only && fc.scope == Some(Scope::Settings));
        assert!(!method_spec("net.file_circuits").unwrap().human_only);
        assert_eq!(
            method_spec("channel.allow_api_read").unwrap().scope,
            Some(Scope::Owner)
        );
    }

    #[test]
    fn scope_satisfaction() {
        assert!(Scope::Read.satisfied_by(&[Scope::Read]));
        assert!(Scope::Admin.satisfied_by(&[Scope::Owner]));
        assert!(!Scope::Admin.satisfied_by(&[Scope::Read, Scope::Write]));
        assert!(!Scope::Read.satisfied_by(&[]));
        assert_eq!("settings".parse::<Scope>().unwrap(), Scope::Settings);
        assert!("root".parse::<Scope>().is_err());
        assert_eq!(serde_json::to_string(&Scope::Admin).unwrap(), "\"admin\"");
    }

    #[test]
    fn empty_and_params() {
        assert_eq!(serde_json::to_string(&Empty).unwrap(), "{}");
        let _: Empty = serde_json::from_str("null").unwrap();
        let _: Empty = serde_json::from_str(r#"{"x":1}"#).unwrap();
        let p: OptSpaceParams = parse_params(Value::Null).unwrap();
        assert!(p.space.is_none());
        let e = parse_params::<SpaceParams>(serde_json::json!({})).unwrap_err();
        assert_eq!(e.code, crate::jsonrpc::INVALID_PARAMS);
    }

    #[test]
    fn positional_params_are_rejected() {
        // serde는 구조체를 배열로도 읽으므로, 막지 않으면 공간 제한 검사를 피해 space를 넣을 수 있습니다.
        let space = yangpa_core::ids::SpaceId([9; 16]).to_string();
        let e = parse_params::<OptSpaceParams>(serde_json::json!([space])).unwrap_err();
        assert_eq!(e.code, crate::jsonrpc::INVALID_PARAMS);
        let e = parse_params::<MessageSearchParams>(serde_json::json!([space, null, "q", null]))
            .unwrap_err();
        assert_eq!(e.code, crate::jsonrpc::INVALID_PARAMS);
        let _: Empty = parse_params(serde_json::json!({})).unwrap();
    }

    #[test]
    fn storage_and_committer_methods() {
        for (name, scope, space) in [
            ("blob.cache_status", Scope::Read, SpaceRule::Global),
            ("blob.cache_set_limit", Scope::Settings, SpaceRule::Global),
            ("blob.cache_clear", Scope::Settings, SpaceRule::Global),
            ("blob.pin", Scope::Write, SpaceRule::Required),
            ("blob.unpin", Scope::Write, SpaceRule::Required),
            ("blob.pins", Scope::Read, SpaceRule::OptionalFiltered),
            (
                "crew.committer_candidates",
                Scope::Read,
                SpaceRule::Required,
            ),
        ] {
            let s = method_spec(name).unwrap_or_else(|| panic!("{name} 없음"));
            assert_eq!((s.scope, s.space), (Some(scope), space), "{name}");
        }
        assert!(is_destructive("blob.cache_clear"));
        // 한도 바꾸기는 줄일 때만(데몬이 값을 보고 판단).
        assert_eq!(
            method_spec("blob.cache_set_limit").unwrap().destructive,
            Destructive::WhenOthers
        );
        for name in ["blob.pin", "blob.unpin"] {
            assert!(!is_destructive(name), "{name}은 파괴적 작업이 아님");
        }
        // 미디어 힌트는 모두 선택이라 예전 앱의 첨부 인자도 그대로 읽힘.
        let a: AttachmentInput = parse_params(serde_json::json!({"path": "/a.mp4"})).unwrap();
        assert_eq!(a.thumbnail_base64, None);
        assert_eq!(
            serde_json::to_value(&a).unwrap(),
            serde_json::json!({"path": "/a.mp4"})
        );
        let l: BlobCacheLimitParams = parse_params(serde_json::json!({"limit_bytes": 5})).unwrap();
        assert_eq!(
            (l.limit_bytes, l.min_free_bytes, l.reset),
            (Some(5), None, false)
        );
    }
}
