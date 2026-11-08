//! API 안전장치(B4): 자동화 토큰의 파괴적 작업 사람 확인 대기열, 감사 로그, 토큰 폐기 반영.
//!
//! 코어가 부르는 고리:
//! - [`before_call`]: 모든 API 호출 전(서버·프로세스 안 공통). 폐기된 토큰을 막고, 자동화 토큰이 항상 파괴적인
//!   메서드를 부르면 실행하지 않고 확인 대기열에 넣어 `1006`을 돌려줍니다.
//! - [`after_call`]: 모든 API 호출 뒤. 감사 로그에 메서드·토큰 이름·결과 코드·시각만 남깁니다(인자·결과 없음).
//! - [`confirm_when_others`]: 대상에 따라 파괴적인 메서드(`message.delete`의 남의 메시지)를 함수가 판단한 뒤.
//!   인자의 `space`·`channel`이 휘발성 채널이면 확인 요청을 RAM에만 둡니다(휘발성 메시지 ID가 디스크에 닿지 않게).
//!
//! 승인(`confirm.approve`)은 대기열에서 요청을 꺼내(한 번만 꺼내짐) 원래 토큰의 범위로 `api::dispatch`를 부릅니다.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::debug;
use yangpa_core::ids::{ChannelId, RoleId, SpaceId, UserId};
use yangpa_core::time::Timestamp;
use yangpa_rpc::api::{Destructive, MethodSpec, Notification, Scope, method_spec};
use yangpa_rpc::{AuthInfo, RpcError, RpcOutput};
use yangpa_store::{ApiTokenRecord, AuditEntry, PendingConfirmation};

use crate::api::{self, CallCtx, OWNER_TOKEN_ID};
use crate::node::Node;

/// 사람 확인을 기다리는 기본 시간.
pub(crate) const DEFAULT_CONFIRM_TTL: Duration = Duration::from_secs(60 * 60);
/// 대기열에 둘 수 있는 확인 요청 수(자동화가 요청을 쌓아 사람을 지치게 하지 못하게).
pub(crate) const MAX_PENDING: usize = 100;
/// 만료된 확인 요청을 지우는 주기.
const PRUNE_INTERVAL: Duration = Duration::from_secs(60);

/// 관리 영역의 노드 수준 상태(`Node::admin`).
pub struct AdminState {
    /// 폐기한 토큰 ID. 서버 연결은 인증 정보를 연결할 때 받아 두므로, 이미 붙은 연결도 다음 호출부터 막습니다.
    revoked: Mutex<HashSet<String>>,
    confirm_ttl: Mutex<Duration>,
    /// 휘발성 채널을 대상으로 한 확인 요청(인자에 휘발성 메시지 ID가 들어가므로 디스크에 쓰지 않음, 재시작하면 사라짐).
    volatile_pending: Mutex<HashMap<String, PendingConfirmation>>,
}

impl Default for AdminState {
    fn default() -> Self {
        AdminState {
            revoked: Mutex::new(HashSet::new()),
            confirm_ttl: Mutex::new(DEFAULT_CONFIRM_TTL),
            volatile_pending: Mutex::new(HashMap::new()),
        }
    }
}

impl AdminState {
    /// 확인 요청 유효 시간을 바꿉니다(시험용).
    pub(crate) fn set_confirm_ttl(&self, ttl: Duration) {
        *self.confirm_ttl.lock() = ttl;
    }

    fn confirm_ttl(&self) -> Duration {
        *self.confirm_ttl.lock()
    }

    pub(crate) fn mark_revoked(&self, token_id: &str) {
        self.revoked.lock().insert(token_id.to_owned());
    }

    pub(crate) fn is_revoked(&self, token_id: &str) -> bool {
        self.revoked.lock().contains(token_id)
    }

    /// 만료된 RAM 확인 요청을 지웁니다.
    fn prune_volatile(&self, now: Timestamp) {
        self.volatile_pending
            .lock()
            .retain(|_, p| p.expires_at.is_none_or(|e| e > now));
    }
}

/// 이 연결의 토큰이 폐기됐는지(소유자 토큰은 폐기되지 않음).
pub(crate) fn is_revoked(node: &Node, auth: &AuthInfo) -> bool {
    auth.token_id != OWNER_TOKEN_ID && node.admin.is_revoked(&auth.token_id)
}

pub(crate) fn start(node: &Arc<Node>) {
    let _ = node.store.prune_confirmations(Timestamp::now());
    let weak = Arc::downgrade(node);
    node.tasks.spawn(async move {
        let mut tick = tokio::time::interval(PRUNE_INTERVAL);
        tick.tick().await;
        loop {
            tick.tick().await;
            let Some(node) = weak.upgrade() else { break };
            if node.is_shutting_down() {
                break;
            }
            node.admin.prune_volatile(Timestamp::now());
            if let Err(e) = node.store.prune_confirmations(Timestamp::now()) {
                debug!(error = %e, "만료된 확인 요청 정리 실패");
            }
        }
    });
}

// ───────────── 호출 전후 고리 ─────────────

pub(crate) async fn before_call(
    node: &Arc<Node>,
    ctx: &CallCtx,
    spec: &MethodSpec,
    params: &Value,
) -> Result<(), RpcError> {
    if let Some(auth) = ctx.auth()
        && is_revoked(node, auth)
    {
        return Err(RpcError::new(
            yangpa_rpc::jsonrpc::ERR_AUTH_REQUIRED,
            "폐기된 토큰입니다",
        ));
    }
    // 데몬은 받은 경로의 아무 파일이나 읽고 써서(`owner.token` 포함) 소유자가 아닌 토큰이 소유자가 되는 길이 됩니다.
    if !ctx.has_scope(Scope::Owner) && names_local_file(params) {
        return Err(RpcError::out_of_scope(
            "파일 경로는 소유자 토큰만 넘길 수 있습니다(첨부는 blob.paste로 올리세요)",
        ));
    }
    if ctx.is_automation() && api::always_destructive(spec) {
        return Err(enqueue(node, ctx, spec.name, params)?);
    }
    Ok(())
}

/// 인자가 데몬이 열 로컬 파일 경로를 담는지: 맨 위나 `attachments` 항목의 `path`·`*_path` 문자열(빈 값은 이미지
/// 지우기라 뺌). 메서드 이름이 아니라 필드 이름으로 보므로 경로를 받는 새 메서드도 따로 적지 않아도 걸립니다.
fn names_local_file(params: &Value) -> bool {
    fn has_path(v: &Value) -> bool {
        v.as_object().is_some_and(|o| {
            o.iter().any(|(k, v)| {
                (k == "path" || k.ends_with("_path")) && v.as_str().is_some_and(|s| !s.is_empty())
            })
        })
    }
    has_path(params)
        || params
            .get("attachments")
            .and_then(Value::as_array)
            .is_some_and(|a| a.iter().any(has_path))
}

/// 감사 로그 한 줄. 인자·결과 내용은 남기지 않습니다. 토큰 없이 부른 공개 메서드(`system.version`)는 남기지 않습니다
/// (아무 로컬 프로세스나 불러 기록을 밀어낼 수 있으므로).
pub(crate) fn after_call(
    node: &Arc<Node>,
    ctx: &CallCtx,
    spec: &MethodSpec,
    result: &Result<RpcOutput, RpcError>,
) {
    let Some(auth) = ctx.auth() else { return };
    let code = result.as_ref().err().map(|e| e.code);
    if skip_audit(spec.name, code) {
        return;
    }
    audit(
        node,
        &auth.token_name,
        spec.name,
        auth.automation,
        None,
        code,
    );
}

/// 감사 로그에 남기지 않는 호출. Tor 통화의 소리 패킷(`voice.media`)은 초당 수십 번 불려, 성공한 호출까지 적으면
/// 디스크를 계속 쓰고 감사 로그를 밀어내며 말한 때가 디스크에 남습니다(실패만 적음).
pub(crate) fn skip_audit(method: &str, error: Option<i64>) -> bool {
    method == "voice.media" && error.is_none()
}

/// 서버가 직접 답한 호출(`auth.hello`, 구독, 인증·범위 검사 거절)의 감사 로그. 인증하지 않은 연결은 `auth.hello`
/// 실패만 남깁니다(무차별 대입 흔적).
pub(crate) fn on_server_call(
    node: &Node,
    auth: Option<&AuthInfo>,
    method: &str,
    error: Option<i64>,
) {
    if skip_audit(method, error) {
        return;
    }
    match auth {
        Some(a) => audit(node, &a.token_name, method, a.automation, None, error),
        None if method == "auth.hello" => audit(node, "", method, false, None, error),
        None => {}
    }
}

/// 감사 로그의 `detail`(JSON). 형식을 고정해 `audit.list`가 읽습니다.
#[derive(Serialize, Deserialize, Default)]
struct AuditDetail {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    code: Option<i64>,
    #[serde(default)]
    automation: bool,
    /// 사람이 승인해 실행한 호출이면 승인한 토큰 이름.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    approved_by: Option<String>,
}

/// 보안상 중요한 줄인지: 실패, 사람이 승인한 실행, 파괴적 메서드, 관리·소유자 범위, 토큰·확인 메서드. 자동화 토큰의
/// 평범한 성공 호출은 중요하지 않은 줄로 두어, 그런 호출을 잔뜩 해서 중요한 기록을 밀어내지 못하게 합니다.
fn is_important(method: &str, failed: bool, approved: bool) -> bool {
    if failed || approved || method.starts_with("token.") || method.starts_with("confirm.") {
        return true;
    }
    method_spec(method).is_some_and(|s| {
        s.destructive != Destructive::No
            || s.human_only
            || matches!(s.scope, Some(Scope::Owner | Scope::Admin))
    })
}

fn audit(
    node: &Node,
    token: &str,
    method: &str,
    automation: bool,
    approved_by: Option<&str>,
    code: Option<i64>,
) {
    let detail = AuditDetail {
        code,
        automation,
        approved_by: approved_by.map(str::to_owned),
    };
    let entry = AuditEntry {
        seq: 0,
        at: Timestamp::now(),
        token: if token.is_empty() { "-" } else { token }.to_owned(),
        method: method.to_owned(),
        outcome: if code.is_none() { "ok" } else { "error" }.to_owned(),
        detail: serde_json::to_string(&detail).unwrap_or_default(),
        // 토큰 없는 줄(인증 실패)은 누구나 만들 수 있으므로 중요한 줄을 밀어내지 못하게 평범한 줄로 둡니다.
        important: !token.is_empty() && is_important(method, code.is_some(), approved_by.is_some()),
    };
    if let Err(e) = node.store.append_audit(&entry) {
        debug!(error = %e, "감사 로그 쓰기 실패");
    }
}

/// 저장된 감사 로그 한 줄을 API 모양으로.
pub(crate) fn audit_view(e: AuditEntry) -> yangpa_rpc::api::AuditEntry {
    let d: AuditDetail = serde_json::from_str(&e.detail).unwrap_or_default();
    yangpa_rpc::api::AuditEntry {
        id: u64::try_from(e.seq).unwrap_or(0),
        at: e.at,
        method: e.method,
        token_name: e.token,
        ok: e.outcome == "ok",
        code: d.code,
        automation: d.automation,
    }
}

/// 대상이 남의 것일 때만 파괴적인 작업. 자동화 토큰이면 실행하지 않고 확인 대기열로 보냅니다.
pub(crate) async fn confirm_when_others(
    node: &Arc<Node>,
    ctx: &CallCtx,
    method: &str,
    params: &Value,
) -> Result<(), RpcError> {
    if ctx.is_automation() {
        return Err(enqueue(node, ctx, method, params)?);
    }
    Ok(())
}

// ───────────── 확인 대기열 ─────────────

/// 확인 요청을 만들어 저장하고 알린 뒤, 돌려줄 `1006` 오류를 만듭니다.
fn enqueue(node: &Node, ctx: &CallCtx, method: &str, params: &Value) -> Result<RpcError, RpcError> {
    let auth = ctx.auth().ok_or_else(RpcError::auth_required)?;
    let now = Timestamp::now();
    let pending = pending(node)?;
    if pending.len() >= MAX_PENDING {
        return Err(RpcError::conflict(
            "사람 확인 대기열이 가득 찼습니다(confirm.list에서 처리하세요)",
        ));
    }
    let ttl = u64::try_from(node.admin.confirm_ttl().as_millis()).unwrap_or(u64::MAX);
    let id = bs58::encode(rand::random::<[u8; 16]>()).into_string();
    let rec = PendingConfirmation {
        id: id.clone(),
        created_at: now,
        expires_at: Some(Timestamp(now.0.saturating_add(ttl))),
        token: auth.token_id.clone(),
        method: method.to_owned(),
        params: params.clone(),
        summary: summarize(node, method, params),
    };
    // 휘발성 채널 대상이면 인자(휘발성 메시지 ID 등)가 디스크에 닿지 않게 RAM에만 둡니다.
    if targets_volatile(node, params) {
        node.admin
            .volatile_pending
            .lock()
            .insert(rec.id.clone(), rec.clone());
    } else {
        node.store.put_confirmation(&rec).map_err(store_err)?;
    }
    node.notify
        .publish(Notification::ConfirmPending(view(node, &rec)));
    debug!(method, "사람 확인 대기");
    Ok(RpcError::confirmation_required(id))
}

/// 인자의 (`space`, `channel`)이 휘발성 채널인지. 채널을 모르면(상태가 없음) 조심해서 휘발성으로 봅니다.
fn targets_volatile(node: &Node, params: &Value) -> bool {
    let (Some(space), Some(channel)) = (space_of(params), param::<ChannelId>(params, "channel"))
    else {
        return false;
    };
    node.space_state(&space)
        .is_none_or(|st| st.channels.get(&channel).is_none_or(|c| c.is_volatile()))
}

/// 요청 하나를 꺼냅니다(RAM 대기열 먼저). 두 번 꺼내지지 않습니다.
fn take(node: &Node, id: &str) -> Result<Option<PendingConfirmation>, RpcError> {
    if let Some(p) = node.admin.volatile_pending.lock().remove(id) {
        return Ok(Some(p));
    }
    node.store.take_confirmation(id).map_err(store_err)
}

fn store_err(e: yangpa_store::StoreError) -> RpcError {
    crate::error::Error::from(e).into()
}

fn param<T: std::str::FromStr>(params: &Value, key: &str) -> Option<T> {
    params.get(key)?.as_str()?.parse().ok()
}

fn space_of(params: &Value) -> Option<SpaceId> {
    param(params, "space")
}

/// 사람이 읽는 요약. 이름(공간·채널·역할·사용자)만 쓰고 메시지 내용은 넣지 않습니다.
fn summarize(node: &Node, method: &str, params: &Value) -> String {
    let space = space_of(params);
    let state = space.and_then(|s| node.space_state(&s));
    let space_name = match (&space, &state) {
        (Some(s), Some(st)) => Some(crate::spaces::display_name(node, s, st)),
        (Some(s), None) => Some(s.short()),
        _ => None,
    };
    let user = param::<UserId>(params, "user").map(|u| crate::identity::display_name(node, &u));
    let channel = param::<ChannelId>(params, "channel").map(|c| {
        state
            .as_ref()
            .and_then(|st| st.channels.get(&c))
            .map_or_else(|| c.short(), |ch| ch.name.clone())
    });
    let role = param::<RoleId>(params, "role").map(|r| {
        state
            .as_ref()
            .and_then(|st| st.roles.get(&r))
            .map_or_else(|| r.to_string(), |x| x.name.clone())
    });
    let mut s = match method {
        "crew.transfer_ownership" => "크루 소유권 넘기기".to_owned(),
        "member.kick" => "멤버 추방".to_owned(),
        "member.set_roles" => "멤버 역할 바꾸기".to_owned(),
        "channel.delete" => "채널 삭제".to_owned(),
        "role.update" => "역할 바꾸기".to_owned(),
        "role.delete" => "역할 삭제".to_owned(),
        "invite.create" => "초대 링크 만들기".to_owned(),
        "token.create" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            format!(
                "API 토큰 만들기: {}",
                name.chars().take(64).collect::<String>()
            )
        }
        "message.delete" => "남의 메시지 삭제".to_owned(),
        "member.ban" => "멤버 밴(내보내고 다시 못 들어오게)".to_owned(),
        "member.unban" => "멤버 밴 해제".to_owned(),
        "report.resolve" => {
            let action = params.get("action").and_then(Value::as_str).unwrap_or("");
            format!("신고 처리: {}", action.chars().take(32).collect::<String>())
        }
        "blob.cache_clear" => "받은 첨부 데이터 모두 비우기".to_owned(),
        "blob.cache_set_limit" => {
            // 1GiB 아래는 MiB로 보여 줍니다(작은 값이 "0.0GiB"로 보여 무엇을 승인하는지 모르게 되지 않게).
            let gib = |key: &str| {
                params.get(key).and_then(Value::as_u64).map(|b| {
                    let b = b as f64;
                    if b >= f64::from(1u32 << 30) {
                        format!("{:.1}GiB", b / f64::from(1u32 << 30))
                    } else {
                        format!("{:.0}MiB", b / f64::from(1u32 << 20))
                    }
                })
            };
            let mut s = "저장 공간 줄이기(넘는 받은 첨부를 비움)".to_owned();
            if let Some(v) = gib("limit_bytes") {
                s.push_str(&format!(" · 한도 {v}"));
            }
            if let Some(v) = gib("min_free_bytes") {
                s.push_str(&format!(" · 최소 여유 {v}"));
            }
            if params.get("reset").and_then(Value::as_bool) == Some(true) {
                s.push_str(" · 기본값으로");
            }
            s
        }
        other => other.to_owned(),
    };
    for (label, v) in [
        ("대상", user),
        ("채널", channel),
        ("역할", role),
        ("공간", space_name),
    ] {
        if let Some(v) = v {
            s.push_str(&format!(" · {label} {v}"));
        }
    }
    s
}

/// 토큰 ID의 이름(지운 토큰이면 ID).
fn token_name(node: &Node, token_id: &str) -> String {
    node.store
        .list_api_tokens()
        .ok()
        .and_then(|v| v.into_iter().find(|t| t.id == token_id))
        .map_or_else(|| token_id.to_owned(), |t| t.name)
}

pub(crate) fn view(node: &Node, p: &PendingConfirmation) -> yangpa_rpc::api::PendingConfirmation {
    yangpa_rpc::api::PendingConfirmation {
        id: p.id.clone(),
        method: p.method.clone(),
        params: p.params.clone(),
        token_name: token_name(node, &p.token),
        space: space_of(&p.params),
        requested_at: p.created_at,
        expires_at: p.expires_at,
        summary: p.summary.clone(),
    }
}

/// 지금 기다리는 확인 요청(만료된 것은 지움). 디스크 대기열과 RAM 대기열(휘발성 채널 대상)을 합쳐 만든 순서로.
pub(crate) fn pending(node: &Node) -> Result<Vec<PendingConfirmation>, RpcError> {
    let now = Timestamp::now();
    let _ = node.store.prune_confirmations(now);
    node.admin.prune_volatile(now);
    let mut all = node.store.list_confirmations().map_err(store_err)?;
    all.extend(node.admin.volatile_pending.lock().values().cloned());
    all.sort_by(|a, b| (a.created_at, &a.id).cmp(&(b.created_at, &b.id)));
    Ok(all)
}

/// 승인하는 쪽이 이 요청을 다룰 수 있는지: 원래 메서드의 범위·공간 제한을 승인자 토큰에도 똑같이 적용합니다.
fn check_approver(ctx: &CallCtx, p: &PendingConfirmation) -> Result<&'static MethodSpec, RpcError> {
    let spec = method_spec(&p.method).ok_or_else(|| RpcError::method_not_found(&p.method))?;
    let auth = ctx.auth().ok_or_else(RpcError::auth_required)?;
    if auth.automation {
        return Err(RpcError::out_of_scope("사람 토큰만 확인할 수 있습니다"));
    }
    if let Some(required) = spec.scope
        && !auth.has_scope(required)
    {
        return Err(RpcError::out_of_scope(format!(
            "이 요청을 승인하려면 {required} 범위가 필요합니다"
        )));
    }
    match space_of(&p.params) {
        Some(s) if !auth.allows_space(&s) => Err(RpcError::out_of_scope(
            "이 토큰은 그 공간의 요청을 다룰 수 없습니다",
        )),
        None if auth.is_space_restricted() => Err(RpcError::out_of_scope(
            "공간 제한 토큰은 전역 작업 요청을 다룰 수 없습니다",
        )),
        _ => Ok(spec),
    }
}

/// 승인자가 볼 수 있는 요청인지(`confirm.list` 걸러내기).
pub(crate) fn visible_to(ctx: &CallCtx, p: &PendingConfirmation) -> bool {
    match space_of(&p.params) {
        Some(s) => ctx.allows_space(&s),
        None => ctx.auth().is_some_and(|a| !a.is_space_restricted()),
    }
}

/// 원래 토큰의 인증 정보(폐기·삭제됐으면 없음). 실행은 사람이 승인한 것이므로 자동화 표시를 끕니다.
fn original_auth(node: &Node, token_id: &str) -> Option<AuthInfo> {
    if node.admin.is_revoked(token_id) {
        return None;
    }
    let rec: ApiTokenRecord = node
        .store
        .list_api_tokens()
        .ok()?
        .into_iter()
        .find(|t| t.id == token_id && !t.revoked)?;
    let scopes: Vec<Scope> = rec.scopes.iter().filter_map(|s| s.parse().ok()).collect();
    Some(AuthInfo {
        token_id: rec.id,
        token_name: rec.name,
        scopes,
        spaces: rec.spaces,
        automation: false,
    })
}

/// 승인: 요청을 꺼내(한 번만) 원래 토큰의 범위로 실행하고 결과를 돌려줍니다.
pub(crate) async fn approve(node: &Arc<Node>, ctx: &CallCtx, id: &str) -> Result<Value, RpcError> {
    let now = Timestamp::now();
    let found = pending(node)?
        .into_iter()
        .find(|p| p.id == id)
        .ok_or_else(|| RpcError::not_found("없거나 만료된 확인 요청"))?;
    let spec = check_approver(ctx, &found)?;
    // 같은 요청을 두 번 승인해도 실행은 한 번만(꺼내기가 원자적).
    let p = take(node, id)?.ok_or_else(|| RpcError::not_found("이미 처리한 확인 요청"))?;
    if p.expires_at.is_some_and(|e| e <= now) {
        return Err(RpcError::not_found("만료된 확인 요청"));
    }
    let auth = original_auth(node, &p.token)
        .ok_or_else(|| RpcError::out_of_scope("요청한 토큰이 폐기됐습니다"))?;
    // 원래 토큰의 범위·공간 제한을 실행 시점에 다시 봅니다(그 사이 토큰이 바뀌었을 수 있음).
    let check = crate::api::check_access(spec, Some(&auth), &p.params);
    let inner = CallCtx::from_auth(auth);
    let r = match check {
        Ok(()) => api::dispatch(node, &inner, spec.name, p.params.clone()).await,
        Err(e) => Err(e),
    };
    audit(
        node,
        inner.token_name(),
        spec.name,
        true,
        Some(ctx.token_name()),
        r.as_ref().err().map(|e| e.code),
    );
    match r? {
        RpcOutput::Json(v) => Ok(v),
        RpcOutput::Binary(_) => Err(RpcError::internal(
            "바이너리 결과는 승인으로 받을 수 없습니다",
        )),
    }
}

/// 거절: 요청을 지웁니다.
pub(crate) fn deny(node: &Node, ctx: &CallCtx, id: &str) -> Result<(), RpcError> {
    let found = pending(node)?
        .into_iter()
        .find(|p| p.id == id)
        .ok_or_else(|| RpcError::not_found("없거나 만료된 확인 요청"))?;
    if ctx.is_automation() {
        return Err(RpcError::out_of_scope("사람 토큰만 확인할 수 있습니다"));
    }
    if !visible_to(ctx, &found) {
        return Err(RpcError::out_of_scope(
            "이 토큰은 그 공간의 요청을 다룰 수 없습니다",
        ));
    }
    take(node, id)?.ok_or_else(|| RpcError::not_found("이미 처리한 확인 요청"))?;
    Ok(())
}

/// 토큰을 폐기할 때 그 토큰이 낸 확인 요청도 지웁니다.
pub(crate) fn drop_requests_of(node: &Node, token_id: &str) {
    node.admin
        .volatile_pending
        .lock()
        .retain(|_, p| p.token != token_id);
    if let Ok(all) = node.store.list_confirmations() {
        for p in all.into_iter().filter(|p| p.token == token_id) {
            let _ = node.store.take_confirmation(&p.id);
        }
    }
}

/// RAM에만 둔 확인 요청 수(시험용).
pub(crate) fn volatile_pending_count(node: &Node) -> usize {
    node.admin.volatile_pending.lock().len()
}
