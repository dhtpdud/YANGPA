// SPDX-License-Identifier: LicenseRef-YANGPA-Supporter
//! 후원자 확인: 앱의 라이브러리 탭·콘텐츠 뷰어를 GitHub Sponsors나 Patreon의 **정기** 후원자와 GitHub 3달러 이상
//! 1회 후원자(30일)에게만 엽니다(decisions.md "후원자 확인").
//!
//! 서버 없이 이 기기가 직접 확인하고, 요청은 링크 미리보기와 같은 HTTP로 보냅니다. GitHub는 Tor로 도는 기기면 Arti로,
//! Patreon은 Tor 출구를 막아 늘 직접 연결로 보냅니다(로그인 때 브라우저가 이미 IP를 보임).
//! - GitHub: 기기 로그인(device flow, 비밀키 없음) → GraphQL `sponsorshipForViewerAsSponsor`. 1회 후원은 3달러
//!   이상이면 결제 뒤 30일까지 열고, 후원받는 계정 본인은 엽니다.
//! - Patreon: PKCE 로그인(비밀키 없음, 등록한 `127.0.0.1:47900`으로 돌아옴) → `identity?include=memberships.campaign`.
//!   우리 캠페인의 `active_patron`이고 실제로 내는 금액이 있어야 합니다(무료 멤버·체험·선물 제외). 요청보다 넓은 권한이
//!   오면(크리에이터 본인 계정) 토큰을 버립니다. Patreon은 사용자·클라이언트마다 토큰이 하나뿐이라, 내 기기 중 한
//!   대만 로그인하고 나머지는 아래처럼 결과를 받습니다.
//! - 내 기기끼리 나누기: 계정을 연결해 직접 확인한 기기가 확인할 때마다, 그리고 내 다른 기기와 인증된 연결이 생길
//!   때마다 결과를 보냅니다(`YpRequest::SupporterShare`). 받는 쪽은 인증된 연결로 내 기기인지 확인하고, 보낸 기기마다
//!   하나씩 `valid_until`까지만 믿으며 다시 나누지 않습니다. 연결을 해제하거나 구독이 끝나면 끝났다는 결과를 보냅니다.
//! - 시작할 때와 하루에 한 번 확인합니다. 닿지 못하면 마지막 결과를 GitHub는 확인 뒤 3일, Patreon은 다음 결제일
//!   하루 뒤까지만 믿습니다. 토큰이 거절되면 바로 잠급니다.
//! - 월 후원 금액(USD 센트)도 남겨 후원자 표시(카드 배너·후광, `crate::flair`)의 단계를 정합니다.
//! - 이 판을 처음 켤 때 신원이 이미 있으면(기존 사용자) 30일 동안 그대로 엽니다. 확인 경로가 하나도 없는 빌드는
//!   잠그지 않습니다.
//!
//! 토큰은 암호화된 DB 설정에만 두고, 로그에는 토큰·계정 이름을 남기지 않습니다.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use parking_lot::Mutex;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Notify;
use tracing::debug;
use url::Url;
use yangpa_core::ids::{DeviceId, UserId};
use yangpa_core::time::Timestamp;
use yangpa_core::wire::{SupporterShare, WireErrorCode, YpRequest, YpResponse};
use yangpa_net::{PeerId, TorIsolation};
use yangpa_rpc::RpcError;
use yangpa_rpc::api::{
    Notification, SupporterAuthUrl, SupporterDeviceCode, SupporterLink, SupporterProvider,
    SupporterShared, SupporterStatus,
};

use crate::blobs::preview::{Route, http_request};
use crate::error::Result;
use crate::node::Node;

/// GitHub OAuth 앱(기기 로그인을 켠 공개 클라이언트 ID). 비어 있으면 GitHub 확인을 쓰지 않습니다.
const GITHUB_CLIENT_ID: &str = "Ov23liOPx0EHYQr1593h";
/// 후원받는 GitHub 계정.
const GITHUB_SPONSORABLE: &str = "dhtpdud";
/// Patreon 클라이언트 ID(공개 값, PKCE라 비밀키 없음).
const PATREON_CLIENT_ID: &str = "bMF6Y0ucoBE-HyGRS8raUODGWx_pS9NUSRpTMZhw_1_JeG7A_mJG9La2Yp6aZtNx";
/// 후원받는 Patreon 캠페인 ID. 비어 있으면 Patreon 확인을 쓰지 않습니다.
const PATREON_CAMPAIGN_ID: &str = "2922371";
/// Patreon 클라이언트에 등록한 돌아올 주소의 포트(이 PC에서만 받음).
const PATREON_PORT: u16 = 47900;
const PATREON_SCOPES: [&str; 2] = ["identity", "identity.memberships"];

const GITHUB_KEY: &str = "supporter.github";
const PATREON_KEY: &str = "supporter.patreon";
const GRACE_KEY: &str = "supporter.grace_until";
/// 내 다른 기기가 보내 준 결과(`supporter.shared.<기기>`).
const SHARED_PREFIX: &str = "supporter.shared.";
const MAX_LOGIN_CHARS: usize = 256;

const DAY: Duration = Duration::from_secs(24 * 3600);
/// 확인 주기와 그 확인을 돌리는 간격(실패하면 다음 간격에 다시).
const CHECK_EVERY: Duration = DAY;
const TICK: Duration = Duration::from_secs(3600);
const GITHUB_TRUST: Duration = Duration::from_secs(3 * 24 * 3600);
/// GitHub 1회 후원은 3달러 이상이면 결제한 때부터 30일 동안 엽니다(Seyoung, 2026-11-02).
const ONE_TIME_MIN_CENTS: u64 = 300;
const ONE_TIME_PERIOD: Duration = Duration::from_secs(30 * 24 * 3600);
const GRACE: Duration = Duration::from_secs(30 * 24 * 3600);
const LOGIN_WAIT: Duration = Duration::from_secs(10 * 60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_BODY: usize = 256 * 1024;
const USER_AGENT: &str = "YANGPA-supporter/1";

/// 진행 중인 로그인(경로마다 하나, 새로 시작하면 이전 것을 멈춤).
#[derive(Default)]
pub(crate) struct SupporterState {
    logins: Mutex<HashMap<SupporterProvider, Arc<Notify>>>,
    /// 확인을 한 줄로(주기 작업과 `supporter.refresh`가 같은 토큰을 겹쳐 갱신하지 않게).
    checking: tokio::sync::Mutex<()>,
    /// 내 표시 내기를 한 줄로(`flair::spread`).
    pub(crate) spreading: tokio::sync::Mutex<()>,
}

/// 연결한 계정(DB 설정, 경로마다 하나).
#[derive(Clone, Serialize, Deserialize)]
struct Link {
    token: String,
    #[serde(default)]
    refresh: Option<String>,
    #[serde(default)]
    token_expires: Option<Timestamp>,
    login: String,
    active: bool,
    checked_at: Timestamp,
    valid_until: Timestamp,
    #[serde(default)]
    needs_login: bool,
    /// 월 후원 금액(USD 센트, 후원받는 계정 본인은 최대). 이 값이 없던 판의 연결은 바로 다시 확인합니다.
    #[serde(default)]
    monthly_cents: Option<u32>,
    /// 그 금액을 낸 기간의 끝(1회 후원). 정기 후원은 없음.
    #[serde(default)]
    paid_until: Option<Timestamp>,
    /// 확인에서 본 한 번 결제액(센트, 후원 뱃지용). 끝난 후원도 셈.
    #[serde(default)]
    cents: u64,
}

/// 내 다른 기기가 보내 준 결과(DB 설정).
#[derive(Clone, Serialize, Deserialize)]
struct Shared {
    device: DeviceId,
    provider: SupporterProvider,
    login: String,
    active: bool,
    valid_until: Timestamp,
}

/// 확인 한 번의 결과.
#[derive(Debug, PartialEq, Eq)]
struct Verdict {
    login: String,
    active: bool,
    valid_until: Timestamp,
    monthly_cents: u32,
    paid_until: Option<Timestamp>,
    /// 한 번 결제액(센트, 뱃지용). GitHub는 끝난 후원도 마지막 것의 단계 금액, Patreon은 지금 내는 금액.
    cents: u64,
}

#[derive(Debug)]
enum CheckError {
    /// 토큰이 거절됨(다시 로그인해야 함).
    Unauthorized,
    /// 닿지 못했거나 응답이 이상함(마지막 결과를 유지).
    Failed(String),
}

pub(crate) fn start(node: &Arc<Node>) {
    init_grace(node);
    let weak = Arc::downgrade(node);
    node.tasks.spawn(async move {
        loop {
            {
                let Some(node) = weak.upgrade() else { break };
                if node.is_shutting_down() {
                    break;
                }
                check_due(&node, false).await;
            }
            tokio::time::sleep(TICK).await;
        }
    });
}

fn providers() -> Vec<SupporterProvider> {
    let mut v = Vec::new();
    if !GITHUB_CLIENT_ID.is_empty() {
        v.push(SupporterProvider::Github);
    }
    if !PATREON_CLIENT_ID.is_empty() && !PATREON_CAMPAIGN_ID.is_empty() {
        v.push(SupporterProvider::Patreon);
    }
    v
}

fn key(p: SupporterProvider) -> &'static str {
    match p {
        SupporterProvider::Github => GITHUB_KEY,
        SupporterProvider::Patreon => PATREON_KEY,
    }
}

fn load(node: &Node, p: SupporterProvider) -> Option<Link> {
    node.store.get_setting(key(p)).ok().flatten()
}

fn save(node: &Node, p: SupporterProvider, link: &Link) -> Result<()> {
    node.store.put_setting(key(p), link)?;
    crate::badges::on_support_seen(node, link.cents);
    Ok(())
}

/// 확인 경로가 있는 판을 처음 켤 때 한 번: 신원이 이미 있으면 기존 사용자 유예. 경로가 없는 판은 아무것도 막지 않으므로
/// 시작하지 않습니다(그 판에서 유예가 흘러가 버리지 않게).
fn init_grace(node: &Node) {
    if providers().is_empty() {
        return;
    }
    if node
        .store
        .get_setting::<Timestamp>(GRACE_KEY)
        .ok()
        .flatten()
        .is_some()
    {
        return;
    }
    let until = if node.me().is_ok() {
        Timestamp::now().saturating_add(GRACE)
    } else {
        Timestamp(0)
    };
    if let Err(e) = node.store.put_setting(GRACE_KEY, &until) {
        debug!(error = %e, "후원 유예 기록 실패");
    }
}

pub(crate) fn status(node: &Node) -> SupporterStatus {
    let now = Timestamp::now();
    let providers = providers();
    let links: Vec<SupporterLink> = providers
        .iter()
        .filter_map(|&p| load(node, p).map(|l| view(p, &l, now)))
        .collect();
    let grace_until = node
        .store
        .get_setting::<Timestamp>(GRACE_KEY)
        .ok()
        .flatten()
        .filter(|g| now < *g);
    let shared: Vec<SupporterShared> = node
        .store
        .settings_with_prefix(SHARED_PREFIX)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(_, v)| serde_json::from_value::<Shared>(v).ok())
        .map(|s| SupporterShared {
            device: s.device,
            provider: s.provider,
            login: s.login,
            active: s.active && now < s.valid_until,
            valid_until: s.valid_until,
        })
        .collect();
    SupporterStatus {
        active: providers.is_empty()
            || grace_until.is_some()
            || links.iter().any(|l| l.active)
            || shared.iter().any(|s| s.active),
        grace_until,
        providers,
        links,
        shared,
    }
}

fn view(provider: SupporterProvider, l: &Link, now: Timestamp) -> SupporterLink {
    SupporterLink {
        provider,
        login: l.login.clone(),
        active: l.active && !l.needs_login && now < l.valid_until,
        checked_at: l.checked_at,
        valid_until: l.valid_until,
        needs_login: l.needs_login,
    }
}

/// 연결한 경로를 확인합니다(`force`가 아니면 하루가 지난 것만). 실패는 다음 주기에 다시.
pub(crate) async fn check_due(node: &Arc<Node>, force: bool) {
    // 오프라인 모드에서는 직접 연결(Patreon)로도 나가지 않습니다. 마지막 결과의 유효 기간은 그대로 흐릅니다.
    if crate::presence::offline(node) {
        return;
    }
    let _one = node.supporter.checking.lock().await;
    let mut checked = false;
    for p in providers() {
        let Some(mut link) = load(node, p) else {
            continue;
        };
        let due = Timestamp::now()
            .as_millis()
            .saturating_sub(link.checked_at.as_millis())
            >= CHECK_EVERY.as_millis() as u64;
        if link.needs_login || !(force || due || link.monthly_cents.is_none()) {
            continue;
        }
        let route = route(node, p);
        let r = match p {
            SupporterProvider::Github => github_check(&route, &link.token).await,
            SupporterProvider::Patreon => {
                match patreon_refresh_if_needed(&route, &mut link).await {
                    Ok(()) => {
                        // 갱신했을 수 있는 토큰을 확인 결과와 상관없이 먼저 저장합니다. Patreon은 갱신 토큰도 바꿔, 잃으면
                        // 다시 로그인해야 합니다.
                        if let Err(e) = save(node, p, &link) {
                            debug!(error = %e, "후원 토큰 저장 실패");
                        }
                        patreon_check(&route, &link.token).await
                    }
                    Err(e) => Err(e),
                }
            }
        };
        match r {
            Ok(v) => {
                link.login = v.login;
                link.active = v.active;
                link.valid_until = v.valid_until;
                link.monthly_cents = Some(v.monthly_cents);
                link.paid_until = v.paid_until;
                link.cents = v.cents;
                link.checked_at = Timestamp::now();
            }
            Err(CheckError::Unauthorized) => {
                link.needs_login = true;
                link.active = false;
            }
            Err(CheckError::Failed(e)) => {
                debug!(provider = ?p, error = %e, "후원 확인 실패(다음 주기에 다시)");
                continue;
            }
        }
        if let Err(e) = save(node, p, &link) {
            debug!(error = %e, "후원 확인 결과 저장 실패");
        }
        checked = true;
        node.notify
            .publish(Notification::SupporterUpdated { error: None });
    }
    if checked {
        share_all(node);
    }
    // 확인하지 않은 때에도: 낸 표시의 기한이 다가오면 다시 냅니다.
    crate::flair::refresh(node);
    // 시작할 때 이미 있던 공간과 앞서 잠들었거나 쓰지 못한 공간에도 내 표시가 닿게.
    crate::flair::spread(node, node.space_ids()).await;
}

/// 이 기기가 직접 확인한 연결 중 유효한 것의 가장 큰 월 후원 금액과 그 금액을 낸 기간의 끝(1회 후원, 같은 금액이면
/// 정기 후원이나 더 늦은 끝을 고름). 연결이 없으면 `None`(받기만 한 기기는 후원 단계를 내지 않게), 있지만 유효한 것이
/// 없으면 0.
pub(crate) fn own_support(node: &Node) -> Option<(u32, Option<Timestamp>)> {
    let now = Timestamp::now();
    let links: Vec<(SupporterProvider, Link)> = providers()
        .into_iter()
        .filter_map(|p| load(node, p).map(|l| (p, l)))
        .collect();
    if links.is_empty() {
        return None;
    }
    Some(
        links
            .iter()
            .filter(|(p, l)| view(*p, l, now).active)
            .map(|(_, l)| (l.monthly_cents.unwrap_or(0), l.paid_until))
            .max_by_key(|&(c, end)| (c, end.unwrap_or(Timestamp(u64::MAX))))
            .unwrap_or((0, None)),
    )
}

pub(crate) fn unlink(node: &Arc<Node>, p: SupporterProvider) -> Result<()> {
    let login = load(node, p).map(|l| l.login).unwrap_or_default();
    node.store.delete_setting(key(p))?;
    node.notify
        .publish(Notification::SupporterUpdated { error: None });
    // 남은 연결이 있으면 그 결과를, 없으면 끝났다는 결과를 내 다른 기기에 보냅니다.
    let share = own_share(node).unwrap_or(SupporterShare {
        provider: provider_name(p).into(),
        login,
        active: false,
        valid_until: Timestamp::now(),
    });
    broadcast(node, share);
    crate::flair::refresh(node);
    Ok(())
}

/// 시험용(`testkit::set_supporter_link`): 로그인 없이 연결을 넣고 내 다른 기기에 나눕니다.
pub(crate) fn put_link_for_test(
    node: &Arc<Node>,
    p: SupporterProvider,
    login: &str,
    valid_until: Timestamp,
    monthly_cents: u32,
) {
    let link = new_link(
        String::new(),
        Verdict {
            login: login.into(),
            active: true,
            valid_until,
            monthly_cents,
            paid_until: None,
            cents: 0,
        },
    );
    if save(node, p, &link).is_ok() {
        share_all(node);
        crate::flair::refresh(node);
    }
}

// ───────────── 내 기기끼리 나누기 ─────────────

fn provider_name(p: SupporterProvider) -> &'static str {
    match p {
        SupporterProvider::Github => "github",
        SupporterProvider::Patreon => "patreon",
    }
}

/// 이 기기가 직접 확인한 결과: 유효한 연결 중 가장 오래 가는 것, 모두 끝났으면 끝났다는 결과. 연결이 없으면 없음
/// (받기만 한 기기가 남의 결과를 지우지 않게).
fn own_share(node: &Node) -> Option<SupporterShare> {
    let now = Timestamp::now();
    let links: Vec<(SupporterProvider, Link)> = providers()
        .into_iter()
        .filter_map(|p| load(node, p).map(|l| (p, l)))
        .collect();
    let (p, l) = links
        .iter()
        .filter(|(p, l)| view(*p, l, now).active)
        .max_by_key(|(_, l)| l.valid_until)
        .or(links.first())?;
    let active = view(*p, l, now).active;
    Some(SupporterShare {
        provider: provider_name(*p).into(),
        login: l.login.clone(),
        active,
        valid_until: if active { l.valid_until } else { now },
    })
}

/// 인증된 연결이 생긴 내 다른 기기에 이 기기가 확인한 결과를 보냅니다(연결한 계정이 있을 때만).
pub(crate) fn share_to(node: &Arc<Node>, peer: PeerId, user: UserId) {
    let Ok(me) = node.me() else { return };
    if user != me.user() {
        return;
    }
    if let Some(share) = own_share(node) {
        send(node, peer, share);
    }
}

fn share_all(node: &Arc<Node>) {
    if let Some(share) = own_share(node) {
        broadcast(node, share);
    }
}

fn broadcast(node: &Arc<Node>, share: SupporterShare) {
    let Ok(me) = node.me() else { return };
    for (peer, user, device) in node.peers.authed_peers() {
        if user == me.user() && device != me.device_id() {
            send(node, peer, share.clone());
        }
    }
}

fn send(node: &Arc<Node>, peer: PeerId, share: SupporterShare) {
    node.spawn_detached(move |node| async move {
        let req = YpRequest::SupporterShare(share);
        match crate::identity::request_after_auth(&node, peer, req).await {
            Ok(YpResponse::Ok) => {}
            Ok(_) => debug!(peer = %peer, "후원 확인 결과를 받지 않음"),
            Err(e) => debug!(peer = %peer, error = %e, "후원 확인 결과 보내기 실패"),
        }
    });
}

/// 받은 결과: 인증된 내 다른 기기의 것만, 보낸 기기마다 하나씩 둡니다.
pub(crate) fn handle_share(node: &Arc<Node>, peer: PeerId, s: SupporterShare) -> YpResponse {
    let Some((user, device)) = node.peers.identity_of(&peer) else {
        return YpResponse::error(WireErrorCode::Unauthenticated, "인증되지 않음");
    };
    let Ok(me) = node.me() else {
        return YpResponse::error(WireErrorCode::Internal, "신원 없음");
    };
    if user != me.user() || device == me.device_id() {
        return YpResponse::error(WireErrorCode::Forbidden, "내 다른 기기가 아님");
    }
    let provider = match s.provider.as_str() {
        "github" => SupporterProvider::Github,
        "patreon" => SupporterProvider::Patreon,
        _ => return YpResponse::error(WireErrorCode::BadRequest, "모르는 후원 경로"),
    };
    if s.login.chars().count() > MAX_LOGIN_CHARS {
        return YpResponse::error(WireErrorCode::BadRequest, "후원 결과 형식 오류");
    }
    let shared = Shared {
        device,
        provider,
        login: s.login,
        active: s.active,
        valid_until: s.valid_until,
    };
    if let Err(e) = node
        .store
        .put_setting(&format!("{SHARED_PREFIX}{device}"), &shared)
    {
        debug!(error = %e, "받은 후원 결과 저장 실패");
        return YpResponse::error(WireErrorCode::Internal, "저장 실패");
    }
    node.notify
        .publish(Notification::SupporterUpdated { error: None });
    YpResponse::Ok
}

/// GitHub는 Tor로 도는 기기면 Arti로, Patreon은 늘 직접 연결로 보냅니다. Patreon은 Tor 출구를 403으로 막고(2026-11-02 확인),
/// 로그인 때 브라우저가 이미 IP를 보이므로 직접 연결로 더 드러나는 것은 확인하는 시각 정도입니다.
fn route(node: &Node, p: SupporterProvider) -> Route {
    if p == SupporterProvider::Github && crate::tor::tor_active(node) {
        Route::Tor {
            net: node.net.clone(),
            isolation: TorIsolation::new(),
        }
    } else {
        Route::Direct
    }
}

/// 같은 경로의 이전 로그인을 멈추고 새 멈춤 신호를 돌려줍니다.
fn replace_login(node: &Node, p: SupporterProvider) -> Arc<Notify> {
    let cancel = Arc::new(Notify::new());
    if let Some(old) = node.supporter.logins.lock().insert(p, cancel.clone()) {
        old.notify_one();
    }
    cancel
}

/// 로그인 실패 까닭(`supporter.updated`의 `error`, 앱이 번역해 보여 줌).
const DENIED: &str = "denied";
const EXPIRED: &str = "expired";
const CREATOR: &str = "creator";
const NETWORK: &str = "network";
const FAILED: &str = "failed";

/// 로그인이 끝나면(성공·실패) 연결을 저장하고 앱에 알립니다. 성공하면 내 다른 기기에도 나눕니다.
fn finish_login(
    node: &Arc<Node>,
    p: SupporterProvider,
    r: std::result::Result<Link, &'static str>,
) {
    let error = match r {
        Ok(link) => save(node, p, &link).err().map(|_| FAILED),
        Err(e) => Some(e),
    };
    if error.is_none() {
        share_all(node);
        crate::flair::refresh(node);
    }
    node.notify.publish(Notification::SupporterUpdated {
        error: error.map(str::to_owned),
    });
}

fn new_link(token: String, v: Verdict) -> Link {
    Link {
        token,
        refresh: None,
        token_expires: None,
        login: v.login,
        active: v.active,
        checked_at: Timestamp::now(),
        valid_until: v.valid_until,
        needs_login: false,
        monthly_cents: Some(v.monthly_cents),
        paid_until: v.paid_until,
        cents: v.cents,
    }
}

// ───────────── HTTP ─────────────

fn form(pairs: &[(&str, &str)]) -> String {
    url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs)
        .finish()
}

/// 요청 한 번: 상태와 JSON 본문(JSON이 아니면 `Null`).
async fn call(
    route: &Route,
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> std::result::Result<(u16, Value), CheckError> {
    let url = Url::parse(url).map_err(|e| CheckError::Failed(e.to_string()))?;
    let mut all = vec![("User-Agent", USER_AGENT), ("Accept", "application/json")];
    all.extend_from_slice(headers);
    match tokio::time::timeout(
        REQUEST_TIMEOUT,
        http_request(route, method, &url, &all, body, MAX_BODY),
    )
    .await
    {
        Ok(Some((status, body))) => {
            let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            if !(200..300).contains(&status) {
                // 원인 진단용: 상태, JSON인지(아니면 보통 Cloudflare 같은 중간 차단 페이지), OAuth 오류 종류만. 본문·토큰은 남기지 않음.
                debug!(
                    host = url.host_str().unwrap_or(""),
                    status,
                    json = !v.is_null(),
                    error = str_field(&v, "error").unwrap_or("-"),
                    "후원 확인: 실패 응답"
                );
            }
            Ok((status, v))
        }
        Ok(None) => Err(CheckError::Failed("연결 실패".into())),
        Err(_) => Err(CheckError::Failed("시간 초과".into())),
    }
}

async fn post_form(
    route: &Route,
    url: &str,
    pairs: &[(&str, &str)],
) -> std::result::Result<(u16, Value), CheckError> {
    let body = form(pairs);
    call(
        route,
        "POST",
        url,
        &[("Content-Type", "application/x-www-form-urlencoded")],
        body.as_bytes(),
    )
    .await
}

fn str_field<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str)
}

// ───────────── GitHub ─────────────

pub(crate) async fn github_begin(
    node: &Arc<Node>,
) -> std::result::Result<SupporterDeviceCode, RpcError> {
    if GITHUB_CLIENT_ID.is_empty() {
        return Err(RpcError::invalid_input(
            "이 빌드는 GitHub 후원 확인을 쓰지 않습니다",
        ));
    }
    let route = route(node, SupporterProvider::Github);
    let (_, v) = post_form(
        &route,
        "https://github.com/login/device/code",
        &[("client_id", GITHUB_CLIENT_ID), ("scope", "read:user")],
    )
    .await
    .map_err(|_| RpcError::network("GitHub에 닿지 못했습니다"))?;
    let (Some(device_code), Some(user_code), Some(uri)) = (
        str_field(&v, "device_code"),
        str_field(&v, "user_code"),
        str_field(&v, "verification_uri"),
    ) else {
        return Err(RpcError::network("GitHub 로그인 코드를 받지 못했습니다"));
    };
    let expires_in = v.get("expires_in").and_then(Value::as_u64).unwrap_or(900);
    let interval = v.get("interval").and_then(Value::as_u64).unwrap_or(5);
    let cancel = replace_login(node, SupporterProvider::Github);
    let device_code = device_code.to_owned();
    let weak = Arc::downgrade(node);
    node.tasks.spawn(async move {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(expires_in);
        let r = tokio::select! {
            r = github_poll(&route, &device_code, interval, deadline) => r,
            _ = cancel.notified() => return,
        };
        if let Some(node) = weak.upgrade() {
            finish_login(&node, SupporterProvider::Github, r);
        }
    });
    Ok(SupporterDeviceCode {
        user_code: user_code.to_owned(),
        verification_uri: uri.to_owned(),
        expires_in_secs: expires_in,
    })
}

/// 사용자가 승인할 때까지 토큰을 묻고, 받으면 바로 후원을 확인합니다.
async fn github_poll(
    route: &Route,
    device_code: &str,
    mut interval: u64,
    deadline: tokio::time::Instant,
) -> std::result::Result<Link, &'static str> {
    loop {
        tokio::time::sleep(Duration::from_secs(interval)).await;
        if tokio::time::Instant::now() >= deadline {
            return Err(EXPIRED);
        }
        let Ok((_, v)) = post_form(
            route,
            "https://github.com/login/oauth/access_token",
            &[
                ("client_id", GITHUB_CLIENT_ID),
                ("device_code", device_code),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ],
        )
        .await
        else {
            continue;
        };
        if let Some(token) = str_field(&v, "access_token") {
            let verdict = github_check(route, token).await.map_err(|_| NETWORK)?;
            return Ok(new_link(token.to_owned(), verdict));
        }
        match str_field(&v, "error") {
            Some("authorization_pending") => {}
            Some("slow_down") => interval += 5,
            Some("access_denied") => return Err(DENIED),
            Some("expired_token") => return Err(EXPIRED),
            _ => return Err(FAILED),
        }
    }
}

async fn github_check(route: &Route, token: &str) -> std::result::Result<Verdict, CheckError> {
    let query = format!(
        "query {{ viewer {{ login }} user(login: \"{GITHUB_SPONSORABLE}\") {{ \
         sponsorshipForViewerAsSponsor(activeOnly: true) {{ isOneTimePayment createdAt tierSelectedAt \
         tier {{ monthlyPriceInCents }} }} \
         last: sponsorshipForViewerAsSponsor(activeOnly: false) {{ tier {{ monthlyPriceInCents }} }} }} }}"
    );
    let body = json!({ "query": query }).to_string();
    let auth = format!("bearer {token}");
    let (status, v) = call(
        route,
        "POST",
        "https://api.github.com/graphql",
        &[
            ("Authorization", &auth),
            ("Content-Type", "application/json"),
        ],
        body.as_bytes(),
    )
    .await?;
    if status == 401 {
        return Err(CheckError::Unauthorized);
    }
    if status != 200 {
        return Err(CheckError::Failed(format!("GitHub 응답 {status}")));
    }
    github_verdict(&v, Timestamp::now())
        .ok_or_else(|| CheckError::Failed("GitHub 응답 형식".into()))
}

/// GraphQL 응답으로 판정: 정기 후원, `ONE_TIME_MIN_CENTS` 이상 1회 후원(결제 뒤 `ONE_TIME_PERIOD`까지), 또는 후원받는
/// 계정 본인. GitHub도 1회 후원을 결제 뒤 한 달 동안 활성으로 둡니다(GitHub 커뮤니티 토론 #37234, 2022).
/// 금액은 후원 단계의 월 금액이고, 1회 후원은 그 30일 동안 낸 금액을 월 금액으로 봅니다(본인은 최대).
fn github_verdict(v: &Value, now: Timestamp) -> Option<Verdict> {
    let data = v.get("data")?;
    let login = str_field(data.get("viewer")?, "login")?.to_owned();
    let trust = now.saturating_add(GITHUB_TRUST);
    let sponsorship = data
        .get("user")
        .and_then(|u| u.get("sponsorshipForViewerAsSponsor"))
        .filter(|s| !s.is_null());
    let owner = login.eq_ignore_ascii_case(GITHUB_SPONSORABLE);
    // (믿을 끝, 1회 후원이 끝나는 때)
    let (until, paid_until) = match sponsorship {
        _ if owner => (Some(trust), None),
        Some(s) => match s.get("isOneTimePayment").and_then(Value::as_bool) {
            Some(false) => (Some(trust), None),
            Some(true) => match one_time_end(s).filter(|&end| now < end) {
                Some(end) => (Some(end.min(trust)), Some(end)),
                None => (None, None),
            },
            None => (None, None),
        },
        None => (None, None),
    };
    let monthly_cents = if owner {
        u32::MAX
    } else if until.is_some() {
        sponsorship
            .and_then(|s| s.pointer("/tier/monthlyPriceInCents"))
            .and_then(Value::as_u64)
            .map_or(0, |c| c.min(u64::from(u32::MAX)) as u32)
    } else {
        0
    };
    // 뱃지용 한 번 결제액: 끝난 후원도 마지막 후원의 단계 금액(후원받는 계정 본인은 후원이 없어 0).
    let cents = data
        .pointer("/user/last/tier/monthlyPriceInCents")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    Some(Verdict {
        login,
        active: until.is_some(),
        valid_until: until.unwrap_or(now),
        monthly_cents,
        paid_until,
        cents,
    })
}

/// 1회 후원이 끝나는 시각: 금액이 모자라면 없음. 결제 시각은 `createdAt`과 `tierSelectedAt`(다시 결제해 등급을 고르면
/// 바뀜) 중 늦은 쪽입니다.
fn one_time_end(s: &Value) -> Option<Timestamp> {
    let cents = s.get("tier")?.get("monthlyPriceInCents")?.as_u64()?;
    if cents < ONE_TIME_MIN_CENTS {
        return None;
    }
    let paid = ["createdAt", "tierSelectedAt"]
        .iter()
        .filter_map(|k| str_field(s, k).and_then(parse_time))
        .max()?;
    Some(paid.saturating_add(ONE_TIME_PERIOD))
}

// ───────────── Patreon ─────────────

pub(crate) async fn patreon_begin(
    node: &Arc<Node>,
) -> std::result::Result<SupporterAuthUrl, RpcError> {
    if !providers().contains(&SupporterProvider::Patreon) {
        return Err(RpcError::invalid_input(
            "이 빌드는 Patreon 후원 확인을 쓰지 않습니다",
        ));
    }
    let cancel = replace_login(node, SupporterProvider::Patreon);
    // 이전 로그인이 포트를 놓을 때까지 잠깐 다시 시도합니다.
    let mut listener = None;
    for _ in 0..20 {
        match TcpListener::bind(("127.0.0.1", PATREON_PORT)).await {
            Ok(l) => {
                listener = Some(l);
                break;
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
    let Some(listener) = listener else {
        // 포트는 Patreon 클라이언트에 등록한 값이라 바꿀 수 없습니다.
        return Err(RpcError::conflict(
            "로그인 결과를 받을 포트 47900을 다른 프로그램이 쓰고 있습니다",
        ));
    };
    let mut bytes = [0u8; 48];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    let verifier = URL_SAFE_NO_PAD.encode(bytes);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let mut st = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut st);
    let state = URL_SAFE_NO_PAD.encode(st);
    let redirect = format!("http://127.0.0.1:{PATREON_PORT}/callback");
    let scope = PATREON_SCOPES.join(" ");
    let url = format!(
        "https://www.patreon.com/oauth2/authorize?{}",
        form(&[
            ("response_type", "code"),
            ("client_id", PATREON_CLIENT_ID),
            ("redirect_uri", &redirect),
            ("scope", &scope),
            ("state", &state),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
        ])
    );
    let route = route(node, SupporterProvider::Patreon);
    let weak = Arc::downgrade(node);
    node.tasks.spawn(async move {
        let r = tokio::select! {
            r = tokio::time::timeout(LOGIN_WAIT, patreon_callback(&listener, &state)) => match r {
                Ok(Ok(code)) => patreon_finish(&route, &code, &verifier, &redirect).await,
                Ok(Err(e)) => Err(e),
                Err(_) => Err(EXPIRED),
            },
            _ = cancel.notified() => return,
        };
        if let Some(node) = weak.upgrade() {
            finish_login(&node, SupporterProvider::Patreon, r);
        }
    });
    Ok(SupporterAuthUrl { url })
}

/// 브라우저가 돌아올 때까지 받습니다. `state`가 맞는 `/callback`의 코드를 돌려줍니다.
async fn patreon_callback(
    listener: &TcpListener,
    state: &str,
) -> std::result::Result<String, &'static str> {
    loop {
        let Ok((mut sock, _)) = listener.accept().await else {
            continue;
        };
        let mut buf = Vec::with_capacity(2048);
        let mut chunk = [0u8; 2048];
        let target = loop {
            // 아무것도 보내지 않는 연결이 진짜 돌아오는 브라우저를 막지 않게 짧게 기다립니다.
            let Ok(Ok(n)) =
                tokio::time::timeout(Duration::from_secs(5), sock.read(&mut chunk)).await
            else {
                break None;
            };
            if n == 0 {
                break None;
            }
            buf.extend_from_slice(&chunk[..n]);
            let mut hs = [httparse::EMPTY_HEADER; 32];
            let mut req = httparse::Request::new(&mut hs);
            match req.parse(&buf) {
                Ok(httparse::Status::Complete(_)) => break req.path.map(str::to_owned),
                Ok(httparse::Status::Partial) if buf.len() < 16 * 1024 => {}
                _ => break None,
            }
        };
        let parsed = target
            .and_then(|t| Url::parse(&format!("http://127.0.0.1{t}")).ok())
            .filter(|u| u.path() == "/callback");
        let Some(u) = parsed else {
            let _ = sock
                .write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await;
            continue;
        };
        let q: HashMap<String, String> = u.query_pairs().into_owned().collect();
        if q.get("state").map(String::as_str) != Some(state) {
            // 다른 탭·오래된 요청: 이 로그인 것이 아니니 닫고 계속 기다립니다.
            let _ = sock
                .write_all(
                    b"HTTP/1.1 400 Bad Request
Content-Length: 0
Connection: close

",
                )
                .await;
            continue;
        }
        let page = "<!doctype html><meta charset=\"utf-8\"><title>YANGPA</title>\
                    <p style=\"font:16px system-ui,sans-serif;margin:40px\">YANGPA: 확인했습니다. 이 창을 닫고 앱으로 돌아가세요.</p>";
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\
             Connection: close\r\n\r\n{page}",
            page.len()
        );
        let _ = sock.write_all(resp.as_bytes()).await;
        return match (q.get("code"), q.get("error")) {
            (Some(code), _) => Ok(code.clone()),
            (None, Some(_)) => Err(DENIED),
            _ => Err(FAILED),
        };
    }
}

/// 코드를 토큰으로 바꾸고(비밀키 대신 PKCE) 후원을 확인합니다.
async fn patreon_finish(
    route: &Route,
    code: &str,
    verifier: &str,
    redirect: &str,
) -> std::result::Result<Link, &'static str> {
    let (status, v) = post_form(
        route,
        "https://www.patreon.com/api/oauth2/token",
        &[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("client_id", PATREON_CLIENT_ID),
            ("redirect_uri", redirect),
            ("code_verifier", verifier),
        ],
    )
    .await
    .map_err(|_| NETWORK)?;
    let mut link = patreon_tokens(status, &v)?;
    let verdict = patreon_check(route, &link.token)
        .await
        .map_err(|_| NETWORK)?;
    link.login = verdict.login;
    link.active = verdict.active;
    link.valid_until = verdict.valid_until;
    link.monthly_cents = Some(verdict.monthly_cents);
    link.paid_until = verdict.paid_until;
    link.cents = verdict.cents;
    Ok(link)
}

/// 토큰 응답을 연결로 만듭니다. 요청보다 넓은 권한(크리에이터 본인 계정)이면 버립니다.
fn patreon_tokens(status: u16, v: &Value) -> std::result::Result<Link, &'static str> {
    let token = str_field(v, "access_token").filter(|_| status == 200);
    let Some(token) = token else {
        return Err(FAILED);
    };
    if !scope_ok(str_field(v, "scope").unwrap_or("")) {
        // 크리에이터 본인 계정: 쓰기 권한까지 받으므로 저장하지 않습니다.
        return Err(CREATOR);
    }
    let expires = v
        .get("expires_in")
        .and_then(Value::as_u64)
        .map(|s| Timestamp::now().saturating_add(Duration::from_secs(s)));
    Ok(Link {
        token: token.to_owned(),
        refresh: str_field(v, "refresh_token").map(str::to_owned),
        token_expires: expires,
        login: String::new(),
        active: false,
        checked_at: Timestamp::now(),
        valid_until: Timestamp::now(),
        needs_login: false,
        monthly_cents: None,
        paid_until: None,
        cents: 0,
    })
}

/// 받은 권한이 요청한 것(`identity`, `identity.memberships`)을 넘지 않는지.
fn scope_ok(scope: &str) -> bool {
    scope
        .split_whitespace()
        .all(|s| PATREON_SCOPES.contains(&s))
}

/// 토큰이 3일 안에 끝나면 갱신합니다(비밀키 없이).
async fn patreon_refresh_if_needed(
    route: &Route,
    link: &mut Link,
) -> std::result::Result<(), CheckError> {
    let soon = Timestamp::now().saturating_add(GITHUB_TRUST);
    if link.token_expires.is_none_or(|e| e > soon) {
        return Ok(());
    }
    let Some(refresh) = link.refresh.clone() else {
        return Err(CheckError::Unauthorized);
    };
    let (status, v) = post_form(
        route,
        "https://www.patreon.com/api/oauth2/token",
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", &refresh),
            ("client_id", PATREON_CLIENT_ID),
        ],
    )
    .await?;
    if (400..500).contains(&status) {
        return Err(CheckError::Unauthorized);
    }
    let fresh = patreon_tokens(status, &v).map_err(|e| CheckError::Failed(e.into()))?;
    link.token = fresh.token;
    link.refresh = fresh.refresh.or(link.refresh.take());
    link.token_expires = fresh.token_expires;
    Ok(())
}

async fn patreon_check(route: &Route, token: &str) -> std::result::Result<Verdict, CheckError> {
    let url = "https://www.patreon.com/api/oauth2/v2/identity?include=memberships.campaign\
               &fields%5Buser%5D=full_name\
               &fields%5Bmember%5D=patron_status,currently_entitled_amount_cents,next_charge_date,is_free_trial,is_gifted";
    let auth = format!("Bearer {token}");
    let (status, v) = call(route, "GET", url, &[("Authorization", &auth)], &[]).await?;
    if status == 401 {
        return Err(CheckError::Unauthorized);
    }
    if status != 200 {
        return Err(CheckError::Failed(format!("Patreon 응답 {status}")));
    }
    patreon_verdict(&v, PATREON_CAMPAIGN_ID, Timestamp::now())
        .ok_or_else(|| CheckError::Failed("Patreon 응답 형식".into()))
}

/// 우리 캠페인의 유료 정기 후원인지. 믿는 기간은 다음 결제일 하루 뒤까지(없으면 3일).
fn patreon_verdict(v: &Value, campaign: &str, now: Timestamp) -> Option<Verdict> {
    let login = v
        .get("data")?
        .get("attributes")
        .and_then(|a| str_field(a, "full_name"))
        .unwrap_or("")
        .to_owned();
    let member = v
        .get("included")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|x| str_field(x, "type") == Some("member"))
        .find(|x| {
            x.pointer("/relationships/campaign/data/id")
                .and_then(Value::as_str)
                == Some(campaign)
        });
    let paying = member.and_then(|m| m.get("attributes")).filter(|a| {
        str_field(a, "patron_status") == Some("active_patron")
            && a.get("currently_entitled_amount_cents")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                > 0
            && a.get("is_free_trial").and_then(Value::as_bool) != Some(true)
            && a.get("is_gifted").and_then(Value::as_bool) != Some(true)
    });
    let valid_until = match paying {
        None => now,
        Some(a) => str_field(a, "next_charge_date")
            .and_then(parse_time)
            .map(|t| t.saturating_add(DAY))
            .unwrap_or_else(|| now.saturating_add(GITHUB_TRUST)),
    };
    // 캠페인 통화가 USD·월 단위라(2026-11-02 공개 캠페인 정보로 확인) 받은 금액을 그대로 월 센트로 씁니다.
    let monthly_cents = paying
        .and_then(|a| a.get("currently_entitled_amount_cents"))
        .and_then(Value::as_u64)
        .map_or(0, |c| c.min(u64::from(u32::MAX)) as u32);
    Some(Verdict {
        login,
        active: paying.is_some(),
        valid_until,
        monthly_cents,
        paid_until: None,
        cents: u64::from(monthly_cents),
    })
}

/// Patreon(`2026-11-30T00:00:00.000+00:00`)·GitHub(`2026-11-30T00:00:00Z`)의 UTC ISO 시각.
fn parse_time(s: &str) -> Option<Timestamp> {
    let s = s
        .strip_suffix("+00:00")
        .map_or_else(|| s.to_owned(), |b| format!("{b}Z"));
    let t = humantime::parse_rfc3339(&s).ok()?;
    let ms = t.duration_since(std::time::UNIX_EPOCH).ok()?.as_millis();
    Some(Timestamp(u64::try_from(ms).ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: Timestamp = Timestamp(1_800_000_000_000);

    #[test]
    fn github_needs_recurring_or_large_enough_one_time() {
        let v = |s: Value| json!({"data": {"viewer": {"login": "alice"}, "user": {"sponsorshipForViewerAsSponsor": s}}});
        let monthly = github_verdict(&v(json!({"isOneTimePayment": false})), NOW).unwrap();
        assert!(monthly.active);
        assert_eq!(monthly.valid_until, NOW.saturating_add(GITHUB_TRUST));
        assert_eq!(monthly.monthly_cents, 0);
        let tier = json!({"isOneTimePayment": false, "tier": {"monthlyPriceInCents": 1500}});
        assert_eq!(github_verdict(&v(tier), NOW).unwrap().monthly_cents, 1500);
        assert!(!github_verdict(&v(Value::Null), NOW).unwrap().active);

        // NOW = 2027-01-15T08:00:00Z
        let once = |cents: u64, created: &str, selected: Value| {
            github_verdict(
                &v(json!({"isOneTimePayment": true, "createdAt": created, "tierSelectedAt": selected,
                          "tier": {"monthlyPriceInCents": cents}})),
                NOW,
            )
            .unwrap()
        };
        // 3달러 이상이고 30일 안: 끝나는 날이 가까우면 그날까지만 믿음.
        let near = once(300, "2026-12-17T08:00:00Z", Value::Null);
        assert!(near.active);
        assert_eq!(near.valid_until, NOW.saturating_add(DAY));
        assert_eq!(
            once(500, "2027-01-14T08:00:00Z", Value::Null).valid_until,
            NOW.saturating_add(GITHUB_TRUST)
        );
        // 30일 동안은 낸 금액이 표시 단계(14달러 배너, 50달러 후광)를 정함.
        let big = once(5000, "2027-01-14T08:00:00Z", Value::Null);
        assert_eq!(big.monthly_cents, 5000);
        assert_eq!(big.paid_until, parse_time("2027-02-13T08:00:00Z"));
        assert_eq!(monthly.paid_until, None);
        // 3달러 미만, 30일 지남, 결제 시각 없음은 닫음.
        let small = once(299, "2027-01-14T08:00:00Z", Value::Null);
        assert!(!small.active);
        assert_eq!(small.monthly_cents, 0);
        let old = once(5000, "2026-12-16T07:59:59Z", Value::Null);
        assert!(!old.active);
        assert_eq!(old.monthly_cents, 0);
        assert!(
            !github_verdict(
                &v(json!({"isOneTimePayment": true, "tier": {"monthlyPriceInCents": 300}})),
                NOW
            )
            .unwrap()
            .active
        );
        // 다시 결제(tierSelectedAt이 늦음)하면 그때부터 30일.
        assert!(once(300, "2026-10-01T00:00:00Z", json!("2027-01-10T00:00:00Z")).active);
        // 후원받는 계정 본인은 엽니다.
        let me = json!({"data": {"viewer": {"login": "DHTPDUD"}, "user": {"sponsorshipForViewerAsSponsor": null}}});
        assert!(github_verdict(&me, NOW).unwrap().active);
        assert_eq!(github_verdict(&me, NOW).unwrap().monthly_cents, u32::MAX);
        assert!(github_verdict(&json!({"errors": [{}]}), NOW).is_none());
        // 뱃지용 금액은 끝난 후원이어도 마지막 후원의 티어 금액입니다.
        let ended = json!({"data": {"viewer": {"login": "alice"}, "user": {"sponsorshipForViewerAsSponsor": null,
                           "last": {"tier": {"monthlyPriceInCents": 1400}}}}});
        let v = github_verdict(&ended, NOW).unwrap();
        assert!(!v.active);
        assert_eq!(v.cents, 1400);
        assert_eq!(monthly.cents, 0);
    }

    fn patreon(attrs: Value, campaign: &str) -> Value {
        json!({
            "data": {"attributes": {"full_name": "Bob"}, "type": "user"},
            "included": [
                {"type": "campaign", "id": campaign},
                {"type": "member", "attributes": attrs, "relationships": {"campaign": {"data": {"id": campaign, "type": "campaign"}}}}
            ]
        })
    }

    #[test]
    fn patreon_needs_paid_membership_of_our_campaign() {
        let paid = json!({"patron_status": "active_patron", "currently_entitled_amount_cents": 500,
                          "next_charge_date": "2027-01-15T00:00:00.000+00:00"});
        let v = patreon_verdict(&patreon(paid.clone(), "42"), "42", NOW).unwrap();
        assert!(v.active);
        assert_eq!(v.login, "Bob");
        assert_eq!(v.monthly_cents, 500);
        assert_eq!(v.cents, 500);
        assert_eq!(
            v.valid_until,
            parse_time("2027-01-15T00:00:00Z")
                .unwrap()
                .saturating_add(DAY)
        );
        // 다른 캠페인, 무료 멤버, 해지, 체험, 선물은 열지 않음.
        assert!(
            !patreon_verdict(&patreon(paid.clone(), "7"), "42", NOW)
                .unwrap()
                .active
        );
        for (k, val) in [
            ("currently_entitled_amount_cents", json!(0)),
            ("patron_status", json!("former_patron")),
            ("patron_status", json!("declined_patron")),
            ("is_free_trial", json!(true)),
            ("is_gifted", json!(true)),
        ] {
            let mut a = paid.clone();
            a[k] = val;
            let v = patreon_verdict(&patreon(a, "42"), "42", NOW).unwrap();
            assert!(!v.active, "{k}");
            assert_eq!(v.valid_until, NOW);
            assert_eq!(v.monthly_cents, 0, "{k}");
            assert_eq!(v.cents, 0, "{k}");
        }
        // 다음 결제일이 없으면 3일만 믿음.
        let mut a = paid;
        a["next_charge_date"] = Value::Null;
        let v = patreon_verdict(&patreon(a, "42"), "42", NOW).unwrap();
        assert_eq!(v.valid_until, NOW.saturating_add(GITHUB_TRUST));
    }

    #[test]
    fn patreon_rejects_creator_scope() {
        assert!(scope_ok("identity identity.memberships"));
        assert!(scope_ok("identity"));
        let creator = json!({"access_token": "t", "scope": "identity campaigns w:campaigns.posts identity.memberships"});
        assert!(patreon_tokens(200, &creator).is_err());
        let ok = json!({"access_token": "t", "refresh_token": "r", "expires_in": 2678399, "scope": "identity identity.memberships"});
        let link = patreon_tokens(200, &ok).unwrap();
        assert_eq!(link.refresh.as_deref(), Some("r"));
        assert!(link.token_expires.is_some());
        assert!(patreon_tokens(400, &json!({"error": "invalid_grant"})).is_err());
    }

    #[test]
    fn link_view_expires() {
        let l = Link {
            token: String::new(),
            refresh: None,
            token_expires: None,
            login: "a".into(),
            active: true,
            checked_at: NOW,
            valid_until: NOW.saturating_add(DAY),
            needs_login: false,
            monthly_cents: Some(500),
            paid_until: None,
            cents: 0,
        };
        assert!(view(SupporterProvider::Github, &l, NOW).active);
        assert!(!view(SupporterProvider::Github, &l, NOW.saturating_add(DAY)).active);
        let relogin = Link {
            needs_login: true,
            ..l
        };
        assert!(!view(SupporterProvider::Github, &relogin, NOW).active);
    }
}
