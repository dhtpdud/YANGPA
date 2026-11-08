//! 로컬 API: 토큰 인증, 메서드 표 → 영역별 함수 연결, 프로세스 안 호출([`crate::Node::call`]).
//!
//! 메서드마다 영역 파일(`api/<영역>.rs`)에 함수가 하나씩 있고, 아래 [`routes!`] 표가 이름으로 연결합니다.
//! 함수 모양은 모두 같습니다:
//!
//! ```ignore
//! pub(crate) async fn send(node: &Arc<Node>, ctx: &CallCtx, p: P<m::MessageSend>) -> R<m::MessageSend>
//! ```
//!
//! 인자·결과 타입은 `yangpa_rpc::api::m`의 메서드 표지가 정하므로 표와 함수가 어긋나면 컴파일되지 않습니다.
//! 서버가 인증·범위·사람 전용·공간 제한(`params.space`)을 검사한 뒤 여기로 오며, 공간 권한(`1003`),
//! 대상에 따라 다른 파괴적 작업 판단, 휘발성 읽기(`1007`)는 각 함수가 맡습니다.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use yangpa_core::ids::SpaceId;
use yangpa_core::time::Timestamp;
use yangpa_rpc::api::{
    Destructive, HelloParams, HelloResult, Method, MethodSpec, Notification, Scope, SpaceRule, m,
    method_spec, parse_params,
};
use yangpa_rpc::{AuthInfo, ConnCtx, RpcError, RpcHandler, RpcOutput};

use crate::admin;
use crate::error::Result;
use crate::node::Node;

// 코어
pub(crate) mod friend;
pub(crate) mod identity;
pub(crate) mod message;
pub(crate) mod space;
pub(crate) mod system;
// B1(크루)
pub(crate) mod channel;
pub(crate) mod crew;
pub(crate) mod emoji;
pub(crate) mod invite;
pub(crate) mod member;
pub(crate) mod role;
// B2(휘발성)
pub(crate) mod volatile;
// B3(블롭·정책)
pub(crate) mod blob;
pub(crate) mod seeder;
// B4(기타)
pub(crate) mod audit;
pub(crate) mod confirm;
pub(crate) mod diag;
pub(crate) mod note;
pub(crate) mod presence;
pub(crate) mod profile;
pub(crate) mod settings;
// 후원 기능은 별도 라이선스라 `supporter/` 폴더에 모읍니다(supporter/LICENSE.md).
#[path = "../supporter/api.rs"]
pub(crate) mod supporter;
pub(crate) mod token;
pub(crate) mod trust;
pub(crate) mod voice;
// W2(모더레이션)
pub(crate) mod moderation;
// X4(저장 공간)
pub(crate) mod library;
pub(crate) mod storage;
pub(crate) mod transfer;
// X3(보관 전용 노드)
pub(crate) mod archive;

/// 메서드 `M`의 인자 타입.
pub(crate) type P<M> = <M as Method>::Params;
/// 메서드 `M`의 결과.
pub(crate) type R<M> = std::result::Result<<M as Method>::Result, RpcError>;

/// 소유자 토큰의 토큰 ID(감사 로그·확인 대기열 표시용).
pub const OWNER_TOKEN_ID: &str = "owner";

/// 호출한 쪽. 서버 연결의 인증 정보를 담거나, 프로세스 안 호출이면 직접 만듭니다.
#[derive(Clone, Debug, Default)]
pub struct CallCtx {
    auth: Option<AuthInfo>,
}

impl CallCtx {
    /// 소유자(모든 범위, 사람).
    pub fn owner() -> Self {
        CallCtx {
            auth: Some(AuthInfo {
                token_id: OWNER_TOKEN_ID.into(),
                token_name: OWNER_TOKEN_ID.into(),
                scopes: vec![Scope::Owner],
                spaces: Vec::new(),
                automation: false,
            }),
        }
    }

    /// 인증하지 않은 호출(공개 메서드만).
    pub fn anonymous() -> Self {
        CallCtx { auth: None }
    }

    pub fn from_auth(auth: AuthInfo) -> Self {
        CallCtx { auth: Some(auth) }
    }

    pub fn auth(&self) -> Option<&AuthInfo> {
        self.auth.as_ref()
    }

    pub fn is_automation(&self) -> bool {
        self.auth.as_ref().is_some_and(|a| a.automation)
    }

    /// 이 공간을 다룰 수 있는지(공간 제한 토큰이면 허용 목록에 있어야 함).
    pub fn allows_space(&self, space: &SpaceId) -> bool {
        self.auth.as_ref().is_some_and(|a| a.allows_space(space))
    }

    pub fn has_scope(&self, scope: Scope) -> bool {
        self.auth.as_ref().is_some_and(|a| a.has_scope(scope))
    }

    pub fn token_name(&self) -> &str {
        self.auth.as_ref().map_or("", |a| a.token_name.as_str())
    }
}

// ───────────── 메서드 표 ─────────────

macro_rules! routes {
    ($( $m:ident => $f:path ),* $(,)?) => {
        /// 연결된 메서드 이름(테스트가 `METHODS`와 비교).
        #[cfg(test)]
        pub(crate) const ROUTED: &[&str] = &[
            <m::BlobRead as Method>::NAME,
            $( <m::$m as Method>::NAME ),*
        ];

        async fn route(
            node: &Arc<Node>,
            ctx: &CallCtx,
            method: &str,
            params: Value,
        ) -> std::result::Result<RpcOutput, RpcError> {
            // 결과가 바이너리 프레임일 수 있는 메서드.
            if method == <m::BlobRead as Method>::NAME {
                return blob::read(node, ctx, parse_params(params)?).await;
            }
            $(
                if method == <m::$m as Method>::NAME {
                    let p: P<m::$m> = parse_params(params)?;
                    let r: <m::$m as Method>::Result = $f(node, ctx, p).await?;
                    return RpcOutput::json(&r);
                }
            )*
            Err(RpcError::method_not_found(method))
        }
    };
}

routes! {
    // 시스템·인증(코어)
    AuthHello => system::auth_hello,
    SystemVersion => system::version,
    SystemStatus => system::status,
    SystemShutdown => system::shutdown,
    UpdateCheck => system::update_check,
    UpdateDownload => system::update_download,
    NetStatusGet => system::net_status,
    NetPeers => system::net_peers,
    EventsSubscribe => system::events_subscribe,
    EventsUnsubscribe => system::events_unsubscribe,
    // 신원·기기·프로필(코어)
    IdentityCreate => identity::create,
    IdentityGet => identity::get,
    IdentityDelete => identity::delete,
    DeviceList => identity::device_list,
    DeviceRename => identity::device_rename,
    DeviceRevoke => identity::device_revoke,
    DeviceLinkCreate => identity::device_link_create,
    DeviceLinkPending => identity::device_link_pending,
    DeviceLinkDecide => identity::device_link_decide,
    DeviceLinkCancel => identity::device_link_cancel,
    DeviceLinkJoin => identity::device_link_join,
    DeviceLinkAbort => identity::device_link_abort,
    SupporterStatusGet => supporter::status,
    SupporterGithubBegin => supporter::github_begin,
    SupporterPatreonBegin => supporter::patreon_begin,
    SupporterRefresh => supporter::refresh,
    SupporterUnlink => supporter::unlink,
    FlairList => supporter::flair_list,
    FlairSet => supporter::flair_set,
    PresenceGet => presence::get,
    PresenceSet => presence::set,
    ProfileGet => identity::profile_get,
    ProfileSet => identity::profile_set,
    // 친구(코어)
    FriendLinkCreate => friend::link_create,
    FriendLinkList => friend::link_list,
    FriendLinkRevoke => friend::link_revoke,
    FriendAdd => friend::add,
    FriendRequests => friend::requests,
    FriendAccept => friend::accept,
    FriendReject => friend::reject,
    FriendList => friend::list,
    FriendPropose => friend::propose,
    FriendOpen => friend::open,
    FriendRemove => friend::remove,
    FriendSafetyNumber => friend::safety_number,
    FriendVerify => friend::verify,
    // 신뢰 스위치
    TrustGet => trust::get,
    TrustSetUser => trust::set_user,
    TrustSetSpace => trust::set_space,
    // 공간·채널 보기(코어)
    SpaceList => space::list,
    SpaceGet => space::get,
    ChannelList => space::channel_list,
    ChannelMarkRead => space::mark_read,
    ChannelTyping => channel::typing,
    // 메시지(코어)
    MessageList => message::list,
    MessageSend => message::send,
    MessageEdit => message::edit,
    MessageDelete => message::delete,
    MessageReact => message::react,
    MessageUnreact => message::unreact,
    MessageSearch => message::search,
    ForumCreatePost => message::forum_create_post,
    ForumListPosts => message::forum_list_posts,
    ForumGetPost => message::forum_get_post,
    ForumSetFeatured => message::forum_set_featured,
    ForumUpdatePost => message::forum_update_post,
    // B1: 크루·채널 관리·멤버·역할·초대·이모지
    CrewCreate => crew::create,
    CrewUpdate => crew::update,
    CrewSetServers => crew::set_servers,
    CrewUseServers => crew::use_servers,
    CrewConvertRequest => crew::convert_request,
    CrewConvertAccept => crew::convert_accept,
    CrewLeave => crew::leave,
    CrewTransferOwnership => crew::transfer_ownership,
    CrewDisband => crew::disband,
    CrewSetCommitter => crew::set_committer,
    ChannelCreate => channel::create,
    ChannelUpdate => channel::update,
    ChannelDelete => channel::delete,
    ChannelSetP2p => channel::set_p2p,
    ChannelP2pJoin => channel::p2p_join,
    ChannelP2pLeave => channel::p2p_leave,
    MemberList => member::list,
    MemberSearch => member::search,
    MemberKick => member::kick,
    MemberSetRoles => member::set_roles,
    RoleList => role::list,
    RoleCreate => role::create,
    RoleUpdate => role::update,
    RoleDelete => role::delete,
    InviteCreate => invite::create,
    InviteList => invite::list,
    InviteRevoke => invite::revoke,
    InviteJoin => invite::join,
    EmojiList => emoji::list,
    EmojiCreate => emoji::create,
    EmojiDelete => emoji::delete,
    // B2: 휘발성 채널
    ChannelJoin => volatile::join,
    ChannelLeave => volatile::leave,
    ChannelAllowApiRead => volatile::allow_api_read,
    // B3: 블롭·시더·피어 정책
    BlobStatus => blob::status,
    BlobFetch => blob::fetch,
    BlobSave => blob::save,
    SeedGet => seeder::seed_get,
    SeedSet => seeder::seed_set,
    SeedExcludeMessage => seeder::exclude_message,
    SeedCoverage => seeder::coverage,
    PeerGet => seeder::peer_get,
    PeerSet => seeder::peer_set,
    // B4: 설정·진단·토큰·감사·확인·음성·메모·크루 프로필
    NetStunServers => settings::stun_servers,
    NetSetStunServers => settings::set_stun_servers,
    NetFileCircuits => settings::file_circuits,
    NetSetFileCircuits => settings::set_file_circuits,
    NetChannelTopics => settings::channel_topics,
    NetSetChannelTopics => settings::set_channel_topics,
    NetBridges => settings::bridges,
    NetSetBridges => settings::set_bridges,
    DiagMetricsGet => diag::metrics,
    DiagLeakcheck => diag::leakcheck,
    TokenCreate => token::create,
    TokenList => token::list,
    TokenRevoke => token::revoke,
    AuditList => audit::list,
    ConfirmList => confirm::list,
    ConfirmApprove => confirm::approve,
    ConfirmDeny => confirm::deny,
    VoiceJoin => voice::join,
    VoiceLeave => voice::leave,
    VoiceSignal => voice::signal,
    VoiceMedia => voice::media,
    VoiceState => voice::state,
    VoiceWatch => voice::watch,
    NoteGet => note::get,
    NoteSet => note::set,
    ProfileSetCrew => profile::set_crew,
    BadgeGet => profile::badge_get,
    BadgeSetShown => profile::badge_set_shown,
    BadgeGrantFounder => profile::badge_grant_founder,
    // W2: 모더레이션(밴·차단·신고)
    MemberBan => moderation::ban,
    MemberUnban => moderation::unban,
    MemberBans => moderation::bans,
    UserBlock => moderation::block,
    UserUnblock => moderation::unblock,
    UserBlocked => moderation::blocked,
    UserDevices => moderation::devices,
    UserBlockDevice => moderation::block_device,
    UserUnblockDevice => moderation::unblock_device,
    UserBlockedDevices => moderation::blocked_devices,
    MessageReport => moderation::report,
    ReportList => moderation::report_list,
    ReportResolve => moderation::report_resolve,
    // X4: 저장 공간 정리·고정, 확정 기기 후보
    BlobCacheStatus => storage::cache_status,
    BlobCacheSetLimit => storage::cache_set_limit,
    BlobCacheClear => storage::cache_clear,
    BlobPin => storage::pin,
    BlobUnpin => storage::unpin,
    BlobPins => storage::pins,
    BlobEvict => storage::evict,
    LibraryList => library::list,
    // 6단계: 다시 잇기, 전송 속도, 범위별 속도 제한
    BlobRelink => blob::relink,
    BlobPaste => blob::paste,
    BlobPasteDiscard => blob::paste_discard,
    TransferRatesGet => transfer::rates,
    RateLimits => transfer::limits,
    RateSetLimit => transfer::set_limit,
    CrewCommitterCandidates => crew::committer_candidates,
    // X3: 보관 전용 노드
    ArchiveOffer => archive::offer,
    ArchiveHosted => archive::hosted,
    ArchiveDrop => archive::drop,
    ArchiveDesignate => archive::designate,
    ArchiveRevoke => archive::revoke,
    ArchiveList => archive::list,
}

/// 검사를 마친 호출 하나를 실행합니다. 확인 승인(`confirm.approve`)처럼 다른 호출 안에서 다시 부를 수 있게
/// 박스한 퓨처를 돌려줍니다.
pub(crate) fn dispatch<'a>(
    node: &'a Arc<Node>,
    ctx: &'a CallCtx,
    method: &'a str,
    params: Value,
) -> Pin<Box<dyn Future<Output = std::result::Result<RpcOutput, RpcError>> + Send + 'a>> {
    Box::pin(route(node, ctx, method, params))
}

/// 감사·확인 고리를 거쳐 실행합니다(서버 경로와 프로세스 안 경로 공통).
async fn execute(
    node: &Arc<Node>,
    ctx: &CallCtx,
    spec: &'static MethodSpec,
    params: Value,
) -> std::result::Result<RpcOutput, RpcError> {
    if node.is_shutting_down() && spec.name != "system.shutdown" {
        return Err(RpcError::conflict("데몬이 종료 중입니다"));
    }
    crate::presence::check_blocked(node, spec.name)?;
    // 확인 대기(`1006`)·폐기된 토큰 거절도 감사 로그에 남깁니다.
    let r = match admin::before_call(node, ctx, spec, &params).await {
        Ok(()) => dispatch(node, ctx, spec.name, params).await,
        Err(e) => Err(e),
    };
    admin::after_call(node, ctx, spec, &r);
    r
}

/// 프로세스 안 호출([`Node::call`]). 서버가 하는 검사(`check_access`)를 똑같이 합니다.
pub(crate) async fn call_in_process(
    node: &Arc<Node>,
    ctx: &CallCtx,
    method: &str,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let Some(spec) = method_spec(method) else {
        return Err(RpcError::method_not_found(method));
    };
    if let Err(e) = check_access(spec, ctx.auth(), &params) {
        admin::on_server_call(node, ctx.auth(), spec.name, Some(e.code));
        return Err(e);
    }
    match execute(node, ctx, spec, params).await? {
        RpcOutput::Json(v) => Ok(v),
        RpcOutput::Binary(_) => Err(RpcError::invalid_request(
            "바이너리 결과는 프로세스 안 호출로 받을 수 없습니다(binary: false로 부르세요)",
        )),
    }
}

/// 서버의 `check_access`와 같은 규칙(인증·범위·사람 전용·공간 제한). 확인 승인(`admin::approve`)도 원래 토큰으로
/// 다시 검사할 때 씁니다.
pub(crate) fn check_access(
    spec: &MethodSpec,
    auth: Option<&AuthInfo>,
    params: &Value,
) -> std::result::Result<(), RpcError> {
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
        Some(Value::String(s)) => Some(
            s.parse::<SpaceId>()
                .map_err(|_| RpcError::invalid_params("space가 올바른 공간 ID가 아닙니다"))?,
        ),
        Some(_) => return Err(RpcError::invalid_params("space는 문자열이어야 합니다")),
    };
    match (spec.space, space) {
        (_, Some(s)) if !auth.allows_space(&s) => Err(RpcError::out_of_scope(
            "이 토큰은 그 공간을 다룰 수 없습니다",
        )),
        (SpaceRule::Required, None) => Err(RpcError::invalid_params("space가 필요합니다")),
        (SpaceRule::OptionalGlobal | SpaceRule::Global, None) => Err(RpcError::out_of_scope(
            "공간 제한 토큰은 전역 작업을 할 수 없습니다",
        )),
        _ => Ok(()),
    }
}

/// 이 메서드가 자동화 토큰에게 항상 사람 확인을 요구하는지.
pub(crate) fn always_destructive(spec: &MethodSpec) -> bool {
    spec.destructive == Destructive::Always
}

// ───────────── 토큰 ─────────────

/// API 토큰 해시(저장·비교용). 원문은 저장하지 않습니다.
pub(crate) fn hash_token(token: &str) -> [u8; 32] {
    blake3::derive_key("yangpa/v1/api-token", token.as_bytes())
}

/// 토큰을 확인합니다. 소유자 토큰이면 모든 범위, 아니면 `api_tokens`에서 찾습니다.
pub(crate) fn authenticate_token(node: &Node, token: &str) -> Option<AuthInfo> {
    let hash = hash_token(token);
    let owner = *node.owner_token_hash.lock();
    if owner.is_some_and(|h| constant_eq(&h, &hash)) {
        return CallCtx::owner().auth;
    }
    let rec = node.store.find_api_token(&hash).ok().flatten()?;
    let scopes: Vec<Scope> = rec.scopes.iter().filter_map(|s| s.parse().ok()).collect();
    if scopes.is_empty() {
        return None;
    }
    let _ = node.store.touch_api_token(&rec.id, Timestamp::now());
    Some(AuthInfo {
        token_id: rec.id,
        token_name: rec.name,
        scopes,
        spaces: rec.spaces,
        automation: rec.automation,
    })
}

fn constant_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// 소유자 토큰 파일(`<데이터 경로>/owner.token`)을 읽거나 처음이면 만들고, 해시를 기억합니다.
pub(crate) fn init_owner_token(node: &Arc<Node>) -> Result<()> {
    let path = node
        .cfg
        .data_dir
        .join(yangpa_rpc::endpoint::OWNER_TOKEN_FILE);
    let token = match yangpa_rpc::endpoint::read_token_file(&path) {
        Ok(t) => t,
        Err(_) => {
            let raw: [u8; 32] = rand::random();
            let t = zeroize::Zeroizing::new(bs58::encode(raw).into_string());
            yangpa_rpc::endpoint::write_token_file(&path, &t)?;
            yangpa_rpc::api::serde_util::SecretString::new(t.as_str())
        }
    };
    *node.owner_token_hash.lock() = Some(hash_token(token.expose()));
    Ok(())
}

/// `auth.hello`를 프로세스 안에서 부르면 토큰만 확인합니다(연결 상태가 없으므로).
pub(crate) fn hello(node: &Node, p: HelloParams) -> std::result::Result<HelloResult, RpcError> {
    let auth = authenticate_token(node, p.token.expose()).ok_or_else(|| {
        RpcError::new(
            yangpa_rpc::jsonrpc::ERR_AUTH_REQUIRED,
            "토큰이 올바르지 않습니다",
        )
    })?;
    Ok(HelloResult {
        scopes: auth.scopes,
        automation: auth.automation,
        spaces: auth.spaces,
    })
}

// ───────────── 서버 처리기 ─────────────

/// `yangpa_rpc::serve`에 넘기는 처리기.
pub struct ApiHandler {
    node: Arc<Node>,
}

impl ApiHandler {
    pub fn new(node: Arc<Node>) -> Self {
        ApiHandler { node }
    }
}

#[async_trait]
impl RpcHandler for ApiHandler {
    async fn authenticate(&self, token: &str) -> Option<AuthInfo> {
        authenticate_token(&self.node, token)
    }

    async fn handle(
        &self,
        ctx: &ConnCtx,
        method: &str,
        params: Value,
    ) -> std::result::Result<RpcOutput, RpcError> {
        let Some(spec) = method_spec(method) else {
            return Err(RpcError::method_not_found(method));
        };
        let call = CallCtx {
            auth: ctx.auth().cloned(),
        };
        execute(&self.node, &call, spec, params).await
    }

    fn filter_notification(&self, auth: &AuthInfo, n: &Notification) -> Option<Notification> {
        // 폐기한 토큰의 열린 연결에는 더 보내지 않습니다(구독은 서버가 직접 받으므로 여기서 막음).
        if admin::is_revoked(&self.node, auth) {
            return None;
        }
        crate::volatile::filter_notification(&self.node, auth, n)
    }

    fn on_server_call(&self, auth: Option<&AuthInfo>, method: &str, error: Option<i64>) {
        admin::on_server_call(&self.node, auth, method, error);
    }
}

// ───────────── 영역 함수가 함께 쓰는 도우미 ─────────────

/// 데몬 오류를 API 오류로.
pub(crate) fn rpc<T>(r: Result<T>) -> std::result::Result<T, RpcError> {
    r.map_err(RpcError::from)
}

/// `{}` 결과.
pub(crate) fn empty() -> yangpa_rpc::api::Empty {
    yangpa_rpc::api::Empty
}

/// 내 신원(없으면 `1008`).
pub(crate) fn me(node: &Node) -> std::result::Result<Arc<crate::identity::Me>, RpcError> {
    rpc(node.me())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn every_method_is_routed() {
        let routed: HashSet<&str> = ROUTED.iter().copied().collect();
        assert_eq!(routed.len(), ROUTED.len(), "중복 연결");
        for spec in yangpa_rpc::api::METHODS {
            assert!(routed.contains(spec.name), "연결 안 됨: {}", spec.name);
        }
        for name in ROUTED {
            assert!(method_spec(name).is_some(), "표에 없는 메서드: {name}");
        }
    }

    #[test]
    fn access_rules_match_server() {
        let spec = method_spec("message.send").unwrap();
        let read_only = AuthInfo {
            token_id: "t".into(),
            token_name: "t".into(),
            scopes: vec![Scope::Read],
            spaces: vec![],
            automation: false,
        };
        let e = check_access(spec, Some(&read_only), &Value::Null).unwrap_err();
        assert_eq!(e.code, yangpa_rpc::jsonrpc::ERR_OUT_OF_SCOPE);
        assert_eq!(
            check_access(spec, None, &Value::Null).unwrap_err().code,
            yangpa_rpc::jsonrpc::ERR_AUTH_REQUIRED
        );
        let e = check_access(spec, Some(&read_only), &serde_json::json!([1])).unwrap_err();
        assert_eq!(e.code, yangpa_rpc::jsonrpc::INVALID_PARAMS);
        let restricted = AuthInfo {
            scopes: vec![Scope::Owner],
            spaces: vec![SpaceId([1; 16])],
            ..read_only.clone()
        };
        let other = SpaceId([2; 16]).to_string();
        let e = check_access(
            spec,
            Some(&restricted),
            &serde_json::json!({ "space": other }),
        )
        .unwrap_err();
        assert_eq!(e.code, yangpa_rpc::jsonrpc::ERR_OUT_OF_SCOPE);
        let global = method_spec("friend.add").unwrap();
        assert!(check_access(global, Some(&restricted), &serde_json::json!({})).is_err());
        let automation = AuthInfo {
            automation: true,
            ..restricted
        };
        let human = method_spec("friend.verify").unwrap();
        assert!(check_access(human, Some(&automation), &serde_json::json!({})).is_err());
    }

    #[test]
    fn token_hash_is_stable_and_keyed() {
        assert_eq!(hash_token("abc"), hash_token("abc"));
        assert_ne!(hash_token("abc"), hash_token("abd"));
        assert_ne!(hash_token("abc"), *blake3::hash(b"abc").as_bytes());
    }
}
