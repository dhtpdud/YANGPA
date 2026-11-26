// SPDX-License-Identifier: LicenseRef-YANGPA-Supporter
//! 크루 구독자 역할(decisions.md "크루 구독자 역할"): 크루 관리자가 자기 GitHub Sponsors·Patreon·SellApp 계정을 이 기기에
//! 연결하고 역할을 고르면, 그 정기 구독자인 멤버에게 이 기기(확인 기기)가 역할을 주고 구독이 끝나면 뺍니다.
//!
//! - 크리에이터 계정: GitHub 기기 로그인(`read:user`), Patreon PKCE(크루 역할 전용 클라이언트, `127.0.0.1:47901`,
//!   `identity identity.memberships campaigns campaigns.members`), SellApp 상점 이름 + API 키. 토큰·키는 이 기기의 암호화 DB
//!   설정에만 둡니다.
//! - 멤버 증명(`YpRequest::SubRole`): 그 크루·그 사용자에게만 맞아 받은 쪽이 다른 곳에 다시 쓸 수 없습니다. GitHub는 한 번 쓰는
//!   로그인 코드(PKCE `code_verifier`를 크루·사용자·nonce로 만듦, 확인 기기가 코드를 바꿔 계정 ID만 읽음, 멤버 기기가 자기 몫의
//!   로그인을 먼저 해 넓은 토큰이 나오는 계정이면 보내지 않음). Patreon은 멤버 기기가 자기 토큰으로 읽은 후원 멤버 번호·크루·
//!   사용자로 만든 값(토큰은 멤버 기기를 떠나지 않아 크루용 클라이언트 주인 계정도 안전, 확인 기기가 캠페인 멤버 목록에서 찾음).
//!   SellApp은 라이선스 키·크루·사용자로 만든 값(확인 기기가 자기 상점 키 목록에서 맞는 키를 찾음). 한 계정·구독은 크루마다 한
//!   사용자에게만 묶입니다(나중에 증명한 쪽).
//! - 정기 구독만 셉니다: GitHub 1회 후원 제외, Patreon 유료 `active_patron`(체험·선물 제외), SellApp `ACTIVE`이고 멈추지 않음.
//! - 증명을 받을 때, 설정을 바꿀 때, 시작할 때와 하루에 한 번 묶인 멤버를 다시 보고 역할을 맞춥니다. 이 기기가 준 역할만
//!   빼고(손으로 준 역할은 그대로), 서비스에 닿지 못한 경로는 판정하지 않습니다.
//! - GitHub는 Tor로 도는 기기면 Arti로, Patreon·SellApp은 직접 연결로 보냅니다(Patreon은 Tor 출구를 막고, SellApp은
//!   상점 주인 자신의 계정).
//!
//! 로그에는 토큰·키·계정 ID를 남기지 않습니다.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use parking_lot::Mutex;
use rand::RngCore as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use tokio::net::TcpListener;
use tokio::sync::Notify;
use tracing::debug;
use yangpa_core::body::EventBody;
use yangpa_core::ids::{DeviceId, RoleId, SpaceId, UserId};
use yangpa_core::model::{Permissions, SubscriberRole, SubscriptionProvider};
use yangpa_core::time::Timestamp;
use yangpa_core::wire::{SubRoleAsk, SubRoleProof, WireErrorCode, YpRequest, YpResponse};
use yangpa_net::{PeerId, TorIsolation};
use yangpa_rpc::RpcError;
use yangpa_rpc::api::{
    CreatorLink, CreatorStatus, Notification, SubroleBegin, SubscriberRoleInput, SupporterAuthUrl,
    SupporterDeviceCode,
};
use zeroize::{Zeroize, Zeroizing};

use super::{
    Authorize, CheckError, DAY, EXPIRED, FAILED, GITHUB_CLIENT_ID, LOGIN_WAIT, NETWORK, authorize,
    call, callback_listener, github_device_start, github_poll, login_callback, loopback,
    pkce_begin, pkce_exchange, post_form, str_field,
};
use crate::api::rpc;
use crate::author::{self, KeyChoice};
use crate::blobs::preview::Route;
use crate::node::Node;

/// 멤버 확인 전용 GitHub OAuth 앱(웹 로그인, 돌아올 주소 `http://127.0.0.1/callback`). GitHub는 설치형 앱도 비밀키를 내게 해
/// 소스에 둡니다(숨길 수 없는 값. 안전은 PKCE `code_verifier`를 크루·사용자에 묶는 데서 옴). 후원자 확인 앱과 나눠, 거기서
/// 승인한 `read:user`가 멤버 확인에 붙지 않게 합니다. 둘 중 하나라도 비어 있으면 GitHub 경로를 쓰지 않습니다.
const MEMBER_GITHUB_CLIENT_ID: &str = "Ov23li425lYAITrT0JVa";
const MEMBER_GITHUB_CLIENT_SECRET: &str = "554a38b3adc54c4837d48f9a43416c2c60d37f72";
const GITHUB_AUTHORIZE: &str = "https://github.com/login/oauth/authorize";
const GITHUB_TOKEN: &str = "https://github.com/login/oauth/access_token";
/// 크루 역할 전용 Patreon 클라이언트(공개 값, PKCE라 비밀키 없음). 후원자 확인 클라이언트와 나눠, 사용자·클라이언트마다
/// 하나뿐인 Patreon 토큰이 서로를 끊지 않게 합니다. 아무도 쓰지 않는 계정이 등록해, 넓은 토큰을 받는 주인 계정이 실제
/// 사용자 가운데 없습니다. 비어 있으면 Patreon 경로를 쓰지 않습니다.
const CREW_PATREON_CLIENT_ID: &str =
    "3Rhu2TjqXu8h59wJbnGuybeKwDe_eX_k2pkV1GEE8hlmI-4PbLBGhaF4G2C-kMAn";
/// 그 클라이언트에 등록한 돌아올 주소의 포트(후원자 확인의 47900과 겹치지 않게).
const CREW_PATREON_PORT: u16 = 47901;
/// `identity.memberships`: 같은 기기에서 다른 크루에 후원자로 증명할 때 이 토큰을 써 새 로그인(이 토큰을 끊음)을 피합니다.
const CREATOR_PATREON_SCOPE: &str = "identity identity.memberships campaigns campaigns.members";
const MEMBER_PATREON_SCOPE: &str = "identity identity.memberships";
/// 한 증명에 실을 Patreon 후원 번호 수 한도. 멤버 기기는 늘 이만큼 채워 보냅니다.
const MAX_PATREON_TAGS: usize = 200;
const PATREON_API: &str = "https://www.patreon.com/api/oauth2/v2";
const SELLAPP_API: &str = "https://sell.app/api/v2";

const CREATOR_PREFIX: &str = "subrole.creator.";
const BIND_PREFIX: &str = "subrole.bind.";
/// 실패하면 다시 해 볼 간격.
const RETRY: Duration = Duration::from_secs(3600);
/// 시작한 뒤 공간 상태가 올라올 때까지 기다리는 시간.
const START_DELAY: Duration = Duration::from_secs(60);
/// 증명을 받은 확인 기기가 답을 미루는 한도(일반 모드 망 요청 상한 30초 안). 넘으면 `Busy`로 답하고 이어서 확인합니다.
const ANSWER_WAIT: Duration = Duration::from_secs(20);
/// 명단 페이지 수 상한(GitHub 100, Patreon 1000명씩).
const MAX_PAGES: usize = 200;
/// SellApp 키 목록에서 한 증명을 찾을 때 보는 페이지 수(100개씩, 앞 8자가 같은 키만이라 보통 한 쪽).
const SELLAPP_KEY_PAGES: usize = 10;
/// SellApp은 분당 60회라 구독마다 물을 때 간격을 둡니다.
const SELLAPP_GAP: Duration = Duration::from_millis(1100);
/// 사용자마다 한 시간에 받는 증명 수(SellApp 한도·크리에이터 계정 보호).
const PROOFS_PER_HOUR: u32 = 10;
const MAX_SECRET: usize = 512;
const MAX_STORE: usize = 64;

/// 묶은 기록과 이 기기가 준 역할의 읽고 고치고 쓰기가 겹치지 않게.
static LOCK: Mutex<()> = Mutex::new(());
/// (공간, 사용자)별 증명 횟수: (창 시작, 횟수).
type ProofSlots = HashMap<(SpaceId, UserId), (Instant, u32)>;
static PROOFS: LazyLock<Mutex<ProofSlots>> = LazyLock::new(Default::default);

/// 이 기기에 연결한 크리에이터 계정(DB 설정, 경로마다 하나).
#[derive(Clone, Serialize, Deserialize)]
struct Creator {
    token: String,
    #[serde(default)]
    refresh: Option<String>,
    #[serde(default)]
    token_expires: Option<Timestamp>,
    /// 화면 이름(GitHub 로그인, Patreon 캠페인, SellApp 상점).
    name: String,
    /// Patreon 캠페인 ID, SellApp 상점 이름(`X-STORE`).
    #[serde(default)]
    target: String,
    #[serde(default)]
    needs_login: bool,
    /// 받은 권한(Patreon). 예전 연결은 비어 있습니다.
    #[serde(default)]
    scope: String,
}

impl Creator {
    fn new(token: String, name: String, target: String) -> Self {
        Creator {
            token,
            refresh: None,
            token_expires: None,
            name,
            target,
            needs_login: false,
            scope: String::new(),
        }
    }
}

impl Drop for Creator {
    fn drop(&mut self) {
        self.token.zeroize();
        if let Some(r) = &mut self.refresh {
            r.zeroize();
        }
    }
}

/// 한 크루에서 묶은 계정과 이 기기가 준 역할(DB 설정 `subrole.bind.<공간>`).
#[derive(Default, Serialize, Deserialize)]
struct Binds {
    #[serde(default)]
    binds: Vec<Bind>,
    /// 이 기기가 준 역할. 이것만 뺍니다(손으로 준 역할은 그대로).
    #[serde(default)]
    granted: Vec<(UserId, RoleId)>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Bind {
    user: UserId,
    provider: SubscriptionProvider,
    /// GitHub 사용자 번호, Patreon 사용자 ID, SellApp 구독 ID.
    account: String,
}

pub(crate) fn providers() -> Vec<SubscriptionProvider> {
    let mut v = Vec::new();
    if ![
        GITHUB_CLIENT_ID,
        MEMBER_GITHUB_CLIENT_ID,
        MEMBER_GITHUB_CLIENT_SECRET,
    ]
    .contains(&"")
    {
        v.push(SubscriptionProvider::Github);
    }
    if !CREW_PATREON_CLIENT_ID.is_empty() {
        v.push(SubscriptionProvider::Patreon);
    }
    v.push(SubscriptionProvider::Sellapp);
    v
}

fn creator_key(p: SubscriptionProvider) -> String {
    let name = match p {
        SubscriptionProvider::Github => "github",
        SubscriptionProvider::Patreon => "patreon",
        SubscriptionProvider::Sellapp => "sellapp",
    };
    format!("{CREATOR_PREFIX}{name}")
}

fn load_creator(node: &Node, p: SubscriptionProvider) -> Option<Creator> {
    node.store.get_setting(&creator_key(p)).ok().flatten()
}

fn save_creator(node: &Node, p: SubscriptionProvider, c: &Creator) -> crate::error::Result<()> {
    Ok(node.store.put_setting(&creator_key(p), c)?)
}

fn load_binds(node: &Node, space: &SpaceId) -> Binds {
    node.store
        .get_setting(&format!("{BIND_PREFIX}{space}"))
        .ok()
        .flatten()
        .unwrap_or_default()
}

fn save_binds(node: &Node, space: &SpaceId, b: &Binds) {
    if let Err(e) = node.store.put_setting(&format!("{BIND_PREFIX}{space}"), b) {
        debug!(error = %e, "구독자 역할 기록 저장 실패");
    }
}

fn notify(node: &Node, space: Option<SpaceId>, result: &str) {
    node.notify.publish(Notification::SubroleUpdated {
        space,
        result: result.into(),
    });
}

/// GitHub는 Tor로 도는 기기면 Arti로, 나머지는 직접 연결로.
fn route(node: &Node, p: SubscriptionProvider) -> Route {
    if p == SubscriptionProvider::Github && crate::tor::tor_active(node) {
        Route::Tor {
            net: node.net.clone(),
            isolation: TorIsolation::new(),
        }
    } else {
        Route::Direct
    }
}

/// 같은 경로의 이전 로그인(크리에이터·멤버)을 멈추고 새 멈춤 신호를 돌려줍니다. Patreon은 둘이 한 포트를 씁니다.
fn replace_login(node: &Node, p: SubscriptionProvider) -> Arc<Notify> {
    let cancel = Arc::new(Notify::new());
    if let Some(old) = node.supporter.crew_logins.lock().insert(p, cancel.clone()) {
        old.notify_one();
    }
    cancel
}

fn need_provider(p: SubscriptionProvider) -> Result<(), RpcError> {
    if providers().contains(&p) {
        Ok(())
    } else {
        Err(RpcError::invalid_input(
            "이 빌드는 이 경로의 구독 확인을 쓰지 않습니다",
        ))
    }
}

pub(crate) fn start(node: &Arc<Node>) {
    let weak = Arc::downgrade(node);
    node.tasks.spawn(async move {
        tokio::time::sleep(START_DELAY).await;
        loop {
            let ok = {
                let Some(node) = weak.upgrade() else { break };
                if node.is_shutting_down() {
                    break;
                }
                sync_all(&node).await
            };
            tokio::time::sleep(if ok { DAY } else { RETRY }).await;
        }
    });
}

// ───────────── 크리에이터 계정 ─────────────

pub(crate) fn creator_status(node: &Node) -> CreatorStatus {
    let links = providers()
        .into_iter()
        .filter_map(|p| {
            load_creator(node, p).map(|c| CreatorLink {
                provider: p,
                name: c.name.clone(),
                needs_login: c.needs_login,
            })
        })
        .collect();
    CreatorStatus {
        providers: providers(),
        links,
    }
}

pub(crate) async fn creator_github_begin(
    node: &Arc<Node>,
) -> Result<SupporterDeviceCode, RpcError> {
    need_provider(SubscriptionProvider::Github)?;
    let route = route(node, SubscriptionProvider::Github);
    let login = github_device_start(&route, "read:user").await?;
    let cancel = replace_login(node, SubscriptionProvider::Github);
    let (device_code, interval, deadline) =
        (login.device_code.clone(), login.interval, login.deadline);
    let weak = Arc::downgrade(node);
    node.tasks.spawn(async move {
        let r = tokio::select! {
            r = github_poll(&route, &device_code, interval, deadline) => r,
            _ = cancel.notified() => return,
        };
        let r = match r {
            Ok(token) => match github_viewer(&route, &token).await {
                Ok((login, _)) => Ok(Creator::new(token, login, String::new())),
                Err(_) => Err(NETWORK),
            },
            Err(e) => Err(e),
        };
        if let Some(node) = weak.upgrade() {
            finish_creator(&node, SubscriptionProvider::Github, r);
        }
    });
    Ok(login.code)
}

pub(crate) async fn creator_patreon_begin(node: &Arc<Node>) -> Result<SupporterAuthUrl, RpcError> {
    need_provider(SubscriptionProvider::Patreon)?;
    let cancel = replace_login(node, SubscriptionProvider::Patreon);
    let pkce = pkce_begin(
        CREW_PATREON_CLIENT_ID,
        CREW_PATREON_PORT,
        CREATOR_PATREON_SCOPE,
    )
    .await?;
    let url = pkce.url.clone();
    let weak = Arc::downgrade(node);
    node.tasks.spawn(async move {
        let route = Route::Direct;
        let r = tokio::select! {
            r = pkce.code() => match r {
                Ok(code) => patreon_creator(&route, &code, &pkce.verifier, &pkce.redirect).await,
                Err(e) => Err(e),
            },
            _ = cancel.notified() => return,
        };
        if let Some(node) = weak.upgrade() {
            finish_creator(&node, SubscriptionProvider::Patreon, r);
        }
    });
    Ok(SupporterAuthUrl { url })
}

/// 코드를 크리에이터 토큰으로 바꾸고 첫 캠페인을 찾습니다.
async fn patreon_creator(
    route: &Route,
    code: &str,
    verifier: &str,
    redirect: &str,
) -> Result<Creator, &'static str> {
    let (status, v) = pkce_exchange(route, CREW_PATREON_CLIENT_ID, code, verifier, redirect)
        .await
        .map_err(|_| NETWORK)?;
    let mut c = patreon_tokens(status, &v).ok_or(FAILED)?;
    let auth = format!("Bearer {}", c.token);
    let url = format!("{PATREON_API}/campaigns?fields%5Bcampaign%5D=vanity,creation_name");
    let (status, v) = call(route, "GET", &url, &[("Authorization", &auth)], &[])
        .await
        .map_err(|_| NETWORK)?;
    if status != 200 {
        return Err(FAILED);
    }
    let first = v
        .get("data")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .ok_or(NO_CAMPAIGN)?;
    c.target = str_field(first, "id").ok_or(FAILED)?.to_owned();
    let attrs = first.get("attributes");
    c.name = ["vanity", "creation_name"]
        .iter()
        .find_map(|k| {
            attrs
                .and_then(|a| str_field(a, k))
                .filter(|s| !s.is_empty())
        })
        .unwrap_or(&c.target)
        .to_owned();
    Ok(c)
}

const NO_CAMPAIGN: &str = "no_campaign";
const LINKED: &str = "linked";

/// Patreon 토큰 응답(처음 바꿀 때와 갱신할 때).
fn patreon_tokens(status: u16, v: &Value) -> Option<Creator> {
    let token = str_field(v, "access_token").filter(|_| status == 200)?;
    let mut c = Creator::new(token.to_owned(), String::new(), String::new());
    c.refresh = str_field(v, "refresh_token").map(str::to_owned);
    c.scope = str_field(v, "scope").unwrap_or_default().to_owned();
    c.token_expires = v
        .get("expires_in")
        .and_then(Value::as_u64)
        .map(|s| Timestamp::now().saturating_add(Duration::from_secs(s)));
    Some(c)
}

fn finish_creator(node: &Arc<Node>, p: SubscriptionProvider, r: Result<Creator, &'static str>) {
    let result = match r {
        Ok(c) => match save_creator(node, p, &c) {
            Ok(()) => LINKED,
            Err(_) => FAILED,
        },
        Err(e) => e,
    };
    notify(node, None, result);
}

/// SellApp 상점 이름과 API 키를 확인하고 저장합니다.
pub(crate) async fn creator_sellapp(
    node: &Arc<Node>,
    store: &str,
    key: &str,
) -> Result<(), RpcError> {
    let store = store.trim().to_ascii_lowercase();
    let key = key.trim();
    let bad = || RpcError::invalid_input("SellApp 상점 이름과 API 키를 확인하세요");
    let store_ok = !store.is_empty()
        && store.len() <= MAX_STORE
        && store
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if !store_ok || key.is_empty() || key.len() > MAX_SECRET || key.chars().any(char::is_control) {
        return Err(bad());
    }
    let c = Creator::new(key.to_owned(), store.clone(), store);
    // 멤버 확인에 쓰는 두 목록을 이 키로 읽을 수 있는지.
    for path in ["/subscriptions?limit=1", "/license-keys?limit=1"] {
        match sellapp(&c, path).await {
            Ok((200, _)) => {}
            Ok((401 | 403 | 404, _)) => return Err(bad()),
            _ => return Err(RpcError::network("SellApp에 닿지 못했습니다")),
        }
    }
    rpc(save_creator(node, SubscriptionProvider::Sellapp, &c))?;
    notify(node, None, LINKED);
    Ok(())
}

pub(crate) fn creator_unlink(node: &Node, p: SubscriptionProvider) -> crate::error::Result<()> {
    node.store.delete_setting(&creator_key(p))?;
    notify(node, None, LINKED);
    Ok(())
}

async fn sellapp(c: &Creator, path: &str) -> Result<(u16, Value), CheckError> {
    let auth = format!("Bearer {}", c.token);
    // 브라우저 같은 User-Agent는 Cloudflare가 막으므로 `call`의 기본값을 씁니다.
    call(
        &Route::Direct,
        "GET",
        &format!("{SELLAPP_API}{path}"),
        &[("Authorization", &auth), ("X-STORE", &c.target)],
        &[],
    )
    .await
}

/// 토큰이 3일 안에 끝나면 갱신합니다(비밀키 없이). 거절되면 다시 연결해야 합니다.
async fn patreon_refresh_if_needed(c: &mut Creator) -> Result<(), CheckError> {
    let soon = Timestamp::now().saturating_add(Duration::from_secs(3 * 24 * 3600));
    if c.token_expires.is_none_or(|e| e > soon) {
        return Ok(());
    }
    let Some(refresh) = c.refresh.clone() else {
        return Err(CheckError::Unauthorized);
    };
    let (status, v) = post_form(
        &Route::Direct,
        "https://www.patreon.com/api/oauth2/token",
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", &refresh),
            ("client_id", CREW_PATREON_CLIENT_ID),
        ],
    )
    .await?;
    if (400..500).contains(&status) {
        return Err(CheckError::Unauthorized);
    }
    let fresh = patreon_tokens(status, &v).ok_or(CheckError::Failed("Patreon 갱신".into()))?;
    c.token.clone_from(&fresh.token);
    if fresh.refresh.is_some() {
        c.refresh.clone_from(&fresh.refresh);
    }
    if !fresh.scope.is_empty() {
        c.scope.clone_from(&fresh.scope);
    }
    c.token_expires = fresh.token_expires;
    Ok(())
}

// ───────────── 구독자 명단 ─────────────

/// 한 번 맞추는 동안 묶어 쓰는 판정. 닿지 못한 경로는 `None`(판정하지 않음).
#[derive(Default)]
struct Lookup {
    sets: HashMap<SubscriptionProvider, Option<HashSet<String>>>,
    sellapp: HashMap<String, Option<bool>>,
    sellapp_last: Option<Instant>,
}

impl Lookup {
    async fn active(
        &mut self,
        node: &Node,
        p: SubscriptionProvider,
        account: &str,
    ) -> Option<bool> {
        if let Some(v) = node
            .supporter
            .test_active
            .lock()
            .get(&(p, account.to_owned()))
        {
            return Some(*v);
        }
        if p == SubscriptionProvider::Sellapp {
            if let Some(v) = self.sellapp.get(account) {
                return *v;
            }
            if let Some(t) = self.sellapp_last {
                tokio::time::sleep(SELLAPP_GAP.saturating_sub(t.elapsed())).await;
            }
            self.sellapp_last = Some(Instant::now());
            let v = sellapp_active(node, account).await;
            self.sellapp.insert(account.to_owned(), v);
            return v;
        }
        if let std::collections::hash_map::Entry::Vacant(slot) = self.sets.entry(p) {
            slot.insert(match fetch_set(node, p).await {
                Ok(s) => Some(s),
                Err(e) => {
                    debug!(provider = ?p, error = ?e, "구독자 명단을 받지 못함");
                    None
                }
            });
        }
        self.sets[&p].as_ref().map(|s| s.contains(account))
    }

    fn failed(&self) -> bool {
        self.sets.values().any(Option::is_none) || self.sellapp.values().any(Option::is_none)
    }
}

/// 크리에이터 토큰이 거절되면 다시 연결하라고 표시합니다.
fn mark_needs_login(node: &Node, p: SubscriptionProvider) {
    if let Some(mut c) = load_creator(node, p)
        && !c.needs_login
    {
        c.needs_login = true;
        let _ = save_creator(node, p, &c);
        notify(node, None, LINKED);
    }
}

/// GitHub·Patreon의 지금 정기 구독자 계정 전부.
async fn fetch_set(node: &Node, p: SubscriptionProvider) -> Result<HashSet<String>, CheckError> {
    let Some(mut c) = load_creator(node, p).filter(|c| !c.needs_login) else {
        return Err(CheckError::Failed("크리에이터 계정 없음".into()));
    };
    let r = match p {
        SubscriptionProvider::Github => github_sponsors(&route(node, p), &c.token).await,
        SubscriptionProvider::Patreon => match patreon_refresh_if_needed(&mut c).await {
            Ok(()) => {
                let _ = save_creator(node, p, &c);
                patreon_members(&c).await.map(|rows| {
                    rows.into_iter()
                        .filter(|r| r.paying)
                        .map(|r| r.user)
                        .collect()
                })
            }
            Err(e) => Err(e),
        },
        SubscriptionProvider::Sellapp => Err(CheckError::Failed("SellApp은 구독마다 확인".into())),
    };
    if matches!(r, Err(CheckError::Unauthorized)) {
        mark_needs_login(node, p);
    }
    r
}

async fn github_sponsors(route: &Route, token: &str) -> Result<HashSet<String>, CheckError> {
    let query = "query($after: String) { viewer { sponsorshipsAsMaintainer(first: 100, after: $after, \
                 includePrivate: true, activeOnly: true) { pageInfo { hasNextPage endCursor } \
                 nodes { isOneTimePayment sponsorEntity { ... on User { databaseId } \
                 ... on Organization { databaseId } } } } } }";
    let auth = format!("bearer {token}");
    let mut out = HashSet::new();
    let mut after: Option<String> = None;
    for _ in 0..MAX_PAGES {
        let body = json!({ "query": query, "variables": { "after": after } }).to_string();
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
        let (ids, next) = (status == 200)
            .then(|| github_sponsors_page(&v))
            .flatten()
            .ok_or_else(|| CheckError::Failed(format!("GitHub 응답 {status}")))?;
        out.extend(ids);
        match next {
            Some(n) => after = Some(n),
            None => return Ok(out),
        }
    }
    Err(CheckError::Failed("GitHub 후원자가 너무 많음".into()))
}

/// GitHub 후원자 한 페이지: 정기 후원자의 사용자 번호와 다음 페이지 커서.
fn github_sponsors_page(v: &Value) -> Option<(Vec<String>, Option<String>)> {
    let s = v.pointer("/data/viewer/sponsorshipsAsMaintainer")?;
    let ids = s
        .get("nodes")?
        .as_array()?
        .iter()
        .filter(|n| n.get("isOneTimePayment").and_then(Value::as_bool) == Some(false))
        .filter_map(|n| {
            n.pointer("/sponsorEntity/databaseId")
                .and_then(Value::as_u64)
        })
        .map(|id| id.to_string())
        .collect();
    let next = (s.pointer("/pageInfo/hasNextPage").and_then(Value::as_bool) == Some(true))
        .then(|| s.pointer("/pageInfo/endCursor").and_then(Value::as_str))
        .flatten()
        .map(str::to_owned);
    Some((ids, next))
}

/// 이 캠페인의 후원 하나: 후원 번호(UUID, 소문자), 사용자 ID, 유료 정기 후원인지(체험·선물 제외).
#[derive(Debug, PartialEq, Eq)]
struct PatronRow {
    member: String,
    user: String,
    paying: bool,
}

async fn patreon_members(c: &Creator) -> Result<Vec<PatronRow>, CheckError> {
    let auth = format!("Bearer {}", c.token);
    let base = format!(
        "{PATREON_API}/campaigns/{}/members?include=user\
         &fields%5Bmember%5D=patron_status,currently_entitled_amount_cents,is_free_trial,is_gifted\
         &page%5Bcount%5D=1000",
        url::form_urlencoded::byte_serialize(c.target.as_bytes()).collect::<String>()
    );
    let mut out = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..MAX_PAGES {
        let url = match &cursor {
            Some(cur) => format!(
                "{base}&page%5Bcursor%5D={}",
                url::form_urlencoded::byte_serialize(cur.as_bytes()).collect::<String>()
            ),
            None => base.clone(),
        };
        let (status, v) = call(
            &Route::Direct,
            "GET",
            &url,
            &[("Authorization", &auth)],
            &[],
        )
        .await?;
        if status == 401 {
            return Err(CheckError::Unauthorized);
        }
        let (ids, next) = (status == 200)
            .then(|| patreon_members_page(&v))
            .flatten()
            .ok_or_else(|| CheckError::Failed(format!("Patreon 응답 {status}")))?;
        out.extend(ids);
        match next {
            Some(n) => cursor = Some(n),
            None => return Ok(out),
        }
    }
    Err(CheckError::Failed("Patreon 후원자가 너무 많음".into()))
}

/// Patreon 멤버 한 페이지와 다음 커서.
fn patreon_members_page(v: &Value) -> Option<(Vec<PatronRow>, Option<String>)> {
    let rows = v
        .get("data")?
        .as_array()?
        .iter()
        .filter_map(|m| {
            let paying = m.get("attributes").is_some_and(|a| {
                str_field(a, "patron_status") == Some("active_patron")
                    && a.get("currently_entitled_amount_cents")
                        .and_then(Value::as_u64)
                        .unwrap_or(0)
                        > 0
                    && a.get("is_free_trial").and_then(Value::as_bool) != Some(true)
                    && a.get("is_gifted").and_then(Value::as_bool) != Some(true)
            });
            Some(PatronRow {
                member: str_field(m, "id").and_then(patreon_member_id)?,
                user: m
                    .pointer("/relationships/user/data/id")?
                    .as_str()?
                    .to_owned(),
                paying,
            })
        })
        .collect();
    let next = v
        .pointer("/meta/pagination/cursors/next")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    Some((rows, next))
}

/// SellApp 구독 하나가 지금 유효한지. 없는 구독은 아님, 닿지 못하면 `None`.
async fn sellapp_active(node: &Node, sub: &str) -> Option<bool> {
    let c = load_creator(node, SubscriptionProvider::Sellapp).filter(|c| !c.needs_login)?;
    if !sub.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
        return Some(false);
    }
    match sellapp(&c, &format!("/subscriptions/{sub}")).await {
        Ok((200, v)) => Some(sellapp_sub_active(&v)),
        Ok((404, _)) => Some(false),
        Ok((401, _)) => {
            mark_needs_login(node, SubscriptionProvider::Sellapp);
            None
        }
        _ => None,
    }
}

/// SellApp 응답은 `data`로 감싸였을 수도 있습니다.
fn sellapp_data(v: &Value) -> &Value {
    v.get("data").filter(|d| d.is_object()).unwrap_or(v)
}

/// `ACTIVE`이고 멈추지 않은 구독(해지 예약은 기간 끝까지 `ACTIVE`). 체험·연체·미납·해지는 아님.
fn sellapp_sub_active(v: &Value) -> bool {
    let d = sellapp_data(v);
    str_field(d, "status") == Some("ACTIVE") && d.get("paused_at").is_none_or(Value::is_null)
}

/// 라이선스 키 목록 한 줄의 구독 ID(1회 구매 키는 없음).
fn sellapp_key_subscription(row: &Value) -> Option<String> {
    match row.get("subscription_id")? {
        Value::Number(n) => Some(n.to_string()),
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

fn is_uuid(s: &str) -> bool {
    s.len() == 36
        && s.bytes().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => b == b'-',
            _ => b.is_ascii_hexdigit(),
        })
}

/// 라이선스 키의 UUID 부분. 고객이 보는 키에는 상점 접두어가 붙을 수 있어 멤버·확인 기기 모두 이 부분만 씁니다.
/// shortcut: UUID로 끝나는 키만 받습니다(SellApp 문서의 형식). 무작위가 짧은 키는 증명 값에서 거꾸로 찾을 수 있어 받지 않습니다.
fn sellapp_key(s: &str) -> Option<Zeroizing<String>> {
    let s = s.trim();
    let tail = s.get(s.len().checked_sub(36)?..)?;
    is_uuid(tail).then(|| Zeroizing::new(tail.to_ascii_lowercase()))
}

/// Patreon 후원 번호(멤버 ID). 무작위 UUID라야 증명 값에서 거꾸로 찾을 수 없어 그 형식만 받습니다.
fn patreon_member_id(s: &str) -> Option<String> {
    is_uuid(s).then(|| s.to_ascii_lowercase())
}

// ───────────── 증명 묶기(멤버·확인 기기 공통) ─────────────

/// 그 크루에 그 사용자가 낼 때만 맞는 값. 확인 기기는 인증된 연결의 사용자로 다시 계산하므로, 받은 증명을 다른 크루나 다른
/// 사용자 이름으로 내면 값이 달라집니다.
fn bound(space: &SpaceId, user: &UserId, secret: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(b"yangpa-subrole-v1");
    h.update(space.as_bytes());
    h.update(user.as_bytes());
    h.update(secret);
    h.finalize().into()
}

/// GitHub·Patreon 로그인의 PKCE `code_verifier`.
fn verifier(space: &SpaceId, user: &UserId, nonce: &[u8; 32]) -> Zeroizing<String> {
    Zeroizing::new(URL_SAFE_NO_PAD.encode(bound(space, user, nonce)))
}

fn sellapp_proof(space: &SpaceId, user: &UserId, key: &str) -> SubRoleProof {
    SubRoleProof::Sellapp {
        tag: bound(space, user, key.as_bytes()),
        hint: key[..8].to_owned(),
    }
}

// ───────────── 멤버 증명(확인 기기 쪽) ─────────────

async fn github_viewer(route: &Route, token: &str) -> Result<(String, u64), CheckError> {
    let body = json!({ "query": "query { viewer { login databaseId } }" }).to_string();
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
    let viewer = v.pointer("/data/viewer").filter(|_| status == 200);
    match viewer.and_then(|x| Some((str_field(x, "login")?, x.get("databaseId")?.as_u64()?))) {
        Some((login, id)) => Ok((login.to_owned(), id)),
        None => Err(CheckError::Failed(format!("GitHub 응답 {status}"))),
    }
}

/// GitHub 로그인 코드를 토큰으로(멤버 확인 앱).
async fn github_exchange(
    route: &Route,
    code: &str,
    verifier: &str,
    redirect: &str,
) -> Result<(u16, Value), CheckError> {
    let pairs = [
        ("client_id", MEMBER_GITHUB_CLIENT_ID),
        ("client_secret", MEMBER_GITHUB_CLIENT_SECRET),
        ("code", code),
        ("redirect_uri", redirect),
        ("code_verifier", verifier),
    ];
    post_form(route, GITHUB_TOKEN, &pairs).await
}

/// GitHub 토큰이 범위 없는 것인지. GitHub는 그 앱에 전에 승인한 범위를 범위 없는 로그인에도 모두 담아 주므로 확인합니다. 범위를 알
/// 수 없으면 넓은 것으로 봅니다.
fn narrow(v: &Value) -> bool {
    str_field(v, "scope") == Some("")
}

/// GitHub 멤버 로그인의 토큰 응답. 넓은 토큰은 멤버 기기가 먼저 막지만(`check_own_login`) 받아도 쓰지 않습니다.
fn github_token(status: u16, v: &Value) -> Result<Zeroizing<String>, CheckError> {
    if let Some(t) = str_field(v, "access_token").filter(|_| status == 200) {
        return if narrow(v) {
            Ok(Zeroizing::new(t.to_owned()))
        } else {
            Err(CheckError::Unauthorized)
        };
    }
    // GitHub는 틀린 코드에도 200으로 답합니다. 우리 앱 설정이 틀린 것은 멤버의 증명 탓이 아닙니다.
    match str_field(v, "error") {
        Some("incorrect_client_credentials") | None => {
            Err(CheckError::Failed(format!("GitHub 응답 {status}")))
        }
        Some(_) => Err(CheckError::Unauthorized),
    }
}

/// 증명으로 그 서비스의 계정을 알아냅니다. 틀렸거나 다른 크루·사용자의 증명은 `Unauthorized`, 증명은 맞지만 이 기기의 구독자
/// 명단에 없으면(Patreon) `None`.
async fn prove(
    node: &Node,
    space: &SpaceId,
    user: &UserId,
    proof: &SubRoleProof,
) -> Result<Option<String>, CheckError> {
    match proof {
        SubRoleProof::Github { code, nonce, port } => {
            let route = route(node, SubscriptionProvider::Github);
            let v = verifier(space, user, nonce);
            let (status, body) = github_exchange(&route, code, &v, &loopback(*port)).await?;
            let token = github_token(status, &body)?;
            Ok(Some(github_viewer(&route, &token).await?.1.to_string()))
        }
        SubRoleProof::Patreon { tags } => patreon_member_of(node, space, user, tags).await,
        SubRoleProof::Sellapp { tag, hint } => sellapp_subscription(node, space, user, tag, hint)
            .await
            .map(Some),
    }
}

/// 이 기기 캠페인의 후원 가운데 증명과 맞는 후원자의 Patreon 사용자 ID.
async fn patreon_member_of(
    node: &Node,
    space: &SpaceId,
    user: &UserId,
    tags: &[[u8; 32]],
) -> Result<Option<String>, CheckError> {
    if tags.is_empty() || tags.len() > MAX_PATREON_TAGS {
        return Err(CheckError::Unauthorized);
    }
    let p = SubscriptionProvider::Patreon;
    let mut c = load_creator(node, p)
        .filter(|c| !c.needs_login)
        .ok_or_else(|| CheckError::Failed("크리에이터 계정 없음".into()))?;
    let rows = match patreon_refresh_if_needed(&mut c).await {
        Ok(()) => {
            let _ = save_creator(node, p, &c);
            patreon_members(&c).await
        }
        Err(e) => Err(e),
    };
    match rows {
        Ok(rows) => patreon_match(&rows, space, user, tags),
        // 크리에이터 토큰 문제는 멤버의 증명 탓이 아닙니다.
        Err(CheckError::Unauthorized) => {
            mark_needs_login(node, p);
            Err(CheckError::Failed("Patreon 크리에이터 토큰 거절".into()))
        }
        Err(e) => Err(e),
    }
}

/// 받은 값 가운데 이 캠페인의 후원 번호로 만든 것이 있는지.
fn patreon_match(
    rows: &[PatronRow],
    space: &SpaceId,
    user: &UserId,
    tags: &[[u8; 32]],
) -> Result<Option<String>, CheckError> {
    let want: HashSet<&[u8; 32]> = tags.iter().collect();
    let mut found = rows
        .iter()
        .filter(|r| want.contains(&bound(space, user, r.member.as_bytes())));
    match (found.next(), found.next()) {
        (Some(r), None) => Ok(Some(r.user.clone())),
        (None, _) => Ok(None),
        // 한 캠페인의 후원은 사람마다 하나라, 여럿이 맞으면 남의 번호를 모아 낸 것입니다.
        (Some(_), Some(_)) => Err(CheckError::Unauthorized),
    }
}

/// 이 기기의 상점 키 목록에서 증명과 맞는 라이선스 키의 구독 ID. 앞 8자(`hint`)가 든 키만 봅니다.
async fn sellapp_subscription(
    node: &Node,
    space: &SpaceId,
    user: &UserId,
    tag: &[u8; 32],
    hint: &str,
) -> Result<String, CheckError> {
    let matches =
        |key: &str| sellapp_key(key).is_some_and(|k| bound(space, user, k.as_bytes()) == *tag);
    {
        let test = node.supporter.test_keys.lock();
        if !test.is_empty() {
            return test
                .iter()
                .find(|(key, _)| matches(key))
                .map(|(_, sub)| sub.clone())
                .ok_or(CheckError::Unauthorized);
        }
    }
    // 주소에 그대로 넣으므로 형식을 봅니다.
    if hint.len() != 8 || !hint.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(CheckError::Unauthorized);
    }
    let c = load_creator(node, SubscriptionProvider::Sellapp)
        .filter(|c| !c.needs_login)
        .ok_or_else(|| CheckError::Failed("SellApp 계정 없음".into()))?;
    for page in 1..=SELLAPP_KEY_PAGES {
        if page > 1 {
            tokio::time::sleep(SELLAPP_GAP).await;
        }
        let path = format!("/license-keys?key={hint}&limit=100&page={page}");
        let v = match sellapp(&c, &path).await? {
            (200, v) => v,
            (401, _) => {
                mark_needs_login(node, SubscriptionProvider::Sellapp);
                return Err(CheckError::Failed("SellApp 키 거절".into()));
            }
            (s, _) => return Err(CheckError::Failed(format!("SellApp 응답 {s}"))),
        };
        let rows = v
            .get("data")
            .and_then(Value::as_array)
            .ok_or_else(|| CheckError::Failed("SellApp 응답 형식".into()))?;
        if let Some(row) = rows
            .iter()
            .find(|r| str_field(r, "key").is_some_and(matches))
        {
            return sellapp_key_subscription(row).ok_or(CheckError::Unauthorized);
        }
        if rows.len() < 100 {
            return Err(CheckError::Unauthorized);
        }
    }
    Err(CheckError::Failed("SellApp 키가 너무 많음".into()))
}

fn provider_of(proof: &SubRoleProof) -> SubscriptionProvider {
    match proof {
        SubRoleProof::Github { .. } => SubscriptionProvider::Github,
        SubRoleProof::Patreon { .. } => SubscriptionProvider::Patreon,
        SubRoleProof::Sellapp { .. } => SubscriptionProvider::Sellapp,
    }
}

/// 사용자마다 한 시간에 `PROOFS_PER_HOUR`번까지.
fn take_proof_slot(space: SpaceId, user: UserId) -> bool {
    let mut m = PROOFS.lock();
    let now = Instant::now();
    m.retain(|_, (t, _)| now.duration_since(*t) < RETRY);
    let e = m.entry((space, user)).or_insert((now, 0));
    e.1 += 1;
    e.1 <= PROOFS_PER_HOUR
}

/// 들어온 `SubRole`: 보낸 기기가 그 크루 멤버인지, 이 기기가 그 경로를 확인하는지 본 뒤 증명을 확인합니다.
pub(crate) async fn handle(node: &Arc<Node>, peer: &PeerId, ask: SubRoleAsk) -> YpResponse {
    let Some((user, device)) = node.peers.identity_of(peer) else {
        return YpResponse::error(WireErrorCode::Unauthenticated, "인증 전");
    };
    let Some(state) = node.space_state(&ask.space) else {
        return YpResponse::error(WireErrorCode::NotFound, "모르는 공간");
    };
    if !state.is_member(&user) || !state.device_belongs(&user, &device) {
        return YpResponse::error(WireErrorCode::NotMember, "멤버가 아님");
    }
    serve(node, user, ask).await
}

async fn serve(node: &Arc<Node>, user: UserId, ask: SubRoleAsk) -> YpResponse {
    let Ok(me) = node.me() else {
        return YpResponse::error(WireErrorCode::Internal, "신원 없음");
    };
    let p = provider_of(&ask.proof);
    let held = node.space_state(&ask.space).is_some_and(|st| {
        st.subscriber_roles
            .iter()
            .any(|s| s.holder == me.device_id() && s.providers.contains(&p))
    });
    if !held {
        return YpResponse::error(WireErrorCode::NotFound, "이 기기가 확인하는 경로가 아님");
    }
    if !take_proof_slot(ask.space, user) {
        return YpResponse::Busy {
            retry_after_ms: RETRY.as_millis() as u32,
        };
    }
    // 답이 늦어지면 먼저 `Busy`로 답하고, 확인은 이어서 해 역할을 줍니다.
    let (tx, rx) = tokio::sync::oneshot::channel();
    node.spawn_detached(move |node| async move {
        let _ = tx.send(verify(&node, user, ask).await);
    });
    match tokio::time::timeout(ANSWER_WAIT, rx).await {
        Ok(Ok(r)) => r,
        Ok(Err(_)) => YpResponse::error(WireErrorCode::Internal, "확인 실패"),
        Err(_) => YpResponse::Busy {
            retry_after_ms: ANSWER_WAIT.as_millis() as u32,
        },
    }
}

async fn verify(node: &Arc<Node>, user: UserId, ask: SubRoleAsk) -> YpResponse {
    let p = provider_of(&ask.proof);
    let account = match prove(node, &ask.space, &user, &ask.proof).await {
        Ok(Some(a)) => a,
        Ok(None) => return YpResponse::error(WireErrorCode::Forbidden, "구독 중이 아님"),
        Err(CheckError::Unauthorized) => {
            return YpResponse::error(WireErrorCode::BadRequest, "증명이 맞지 않음");
        }
        Err(CheckError::Failed(e)) => {
            debug!(provider = ?p, error = %e, "구독자 증명 확인 실패");
            return YpResponse::error(WireErrorCode::Internal, "증명을 확인하지 못함");
        }
    };
    drop(ask.proof);
    bind(node, ask.space, user, p, account.clone());
    let _one = node.supporter.subrole_sync.lock().await;
    let mut lk = Lookup::default();
    let active = lk.active(node, p, &account).await;
    let applied = sync_space(node, ask.space, &mut lk, Some(user)).await;
    match (active, applied) {
        (Some(true), Ok(())) => YpResponse::Ok,
        (Some(true), Err(e)) => YpResponse::error(WireErrorCode::Internal, e.message),
        (Some(false), _) => YpResponse::error(WireErrorCode::Forbidden, "구독 중이 아님"),
        (None, _) => YpResponse::error(WireErrorCode::Internal, "구독을 확인하지 못함"),
    }
}

/// 계정을 이 사용자에게 묶습니다. 같은 계정을 묶었던 다른 사용자와, 이 사용자가 같은 경로로 묶었던 다른 계정은 풉니다.
fn bind(
    node: &Node,
    space: SpaceId,
    user: UserId,
    provider: SubscriptionProvider,
    account: String,
) {
    let _g = LOCK.lock();
    let mut b = load_binds(node, &space);
    b.binds
        .retain(|x| x.provider != provider || (x.user != user && x.account != account));
    b.binds.push(Bind {
        user,
        provider,
        account,
    });
    save_binds(node, &space, &b);
}

// ───────────── 역할 맞추기 ─────────────

/// 이 기기가 확인하는 모든 크루를 맞춥니다. 닿지 못한 경로가 있으면 거짓(곧 다시).
async fn sync_all(node: &Arc<Node>) -> bool {
    let Ok(me) = node.me() else { return true };
    let _one = node.supporter.subrole_sync.lock().await;
    let mut lk = Lookup::default();
    for space in node.space_ids() {
        let mine = node.space_state(&space).is_some_and(|st| {
            st.subscriber_roles
                .iter()
                .any(|s| s.holder == me.device_id())
        });
        if mine && let Err(e) = sync_space(node, space, &mut lk, None).await {
            debug!(space = %space.short(), error = %e.message, "구독자 역할 맞추기 실패");
        }
    }
    !lk.failed()
}

/// 설정을 바꾼 뒤 그 크루를 바로 맞춥니다.
pub(crate) fn sync_soon(node: &Arc<Node>, space: SpaceId) {
    node.spawn_detached(move |node| async move {
        let _one = node.supporter.subrole_sync.lock().await;
        let mut lk = Lookup::default();
        if let Err(e) = sync_space(&node, space, &mut lk, None).await {
            debug!(space = %space.short(), error = %e.message, "구독자 역할 맞추기 실패");
        }
    });
}

/// `crew.sync_subscriber_roles`: 이 기기가 확인하는 그 크루를 지금 맞춥니다. 증명한 적 없는 멤버는 계정을 몰라 여기서도
/// 찾지 못합니다.
pub(crate) async fn sync_now(node: &Arc<Node>, space: SpaceId) -> Result<(), RpcError> {
    let (state, me) = rpc(crate::crews::crew_state(node, &space))?;
    rpc(crate::crews::need(&state, &me, Permissions::MANAGE_ROLES))?;
    let device = rpc(node.me())?.device_id();
    let used: BTreeSet<SubscriptionProvider> = state
        .subscriber_roles
        .iter()
        .filter(|s| s.holder == device)
        .flat_map(|s| s.providers.iter().copied())
        .collect();
    if used.is_empty() {
        return Err(RpcError::invalid_input(
            "이 기기가 확인하는 구독자 역할이 없습니다",
        ));
    }
    let _one = node.supporter.subrole_sync.lock().await;
    let mut lk = Lookup::default();
    sync_space(node, space, &mut lk, None).await?;
    if lk.failed() {
        // 크리에이터 연결이 끊긴 것과 잠깐 닿지 못한 것은 할 일이 달라 나눠 알립니다.
        let relink = used
            .iter()
            .any(|p| load_creator(node, *p).is_none_or(|c| c.needs_login));
        return Err(if relink {
            RpcError::invalid_input("크리에이터 계정을 다시 연결하세요")
        } else {
            RpcError::network("구독 서비스에 닿지 못해 확인하지 못한 멤버가 있습니다")
        });
    }
    Ok(())
}

/// 한 멤버의 이 기기가 확인하는 역할 바꾸기: (더할 역할, 뺄 역할). `status`는 묶인 경로별 판정.
fn plan(
    mine: &[&SubscriberRole],
    has: &BTreeSet<RoleId>,
    granted: &BTreeSet<RoleId>,
    status: &HashMap<SubscriptionProvider, Option<bool>>,
) -> (Vec<RoleId>, Vec<RoleId>) {
    let (mut add, mut remove) = (Vec::new(), Vec::new());
    for s in mine {
        let st: Vec<Option<bool>> = s
            .providers
            .iter()
            .filter_map(|p| status.get(p).copied())
            .collect();
        if st.contains(&Some(true)) {
            if !has.contains(&s.role) {
                add.push(s.role);
            }
        } else if st.iter().all(|x| *x == Some(false))
            && has.contains(&s.role)
            && granted.contains(&s.role)
        {
            // 묶인 계정이 없거나 모두 구독 중이 아닐 때만(닿지 못한 경로가 있으면 그대로).
            remove.push(s.role);
        }
    }
    (add, remove)
}

/// 한 크루의 구독자 역할을 맞춥니다(`only`면 그 사용자만). 돌려주는 오류는 마지막으로 역할을 바꾸지 못한 까닭입니다.
async fn sync_space(
    node: &Arc<Node>,
    space: SpaceId,
    lk: &mut Lookup,
    only: Option<UserId>,
) -> Result<(), RpcError> {
    // 오프라인 모드에서는 직접 연결(Patreon·SellApp)로도 나가지 않습니다.
    if crate::presence::offline(node) {
        return Ok(());
    }
    let me = rpc(node.me())?;
    let Some(state) = node.space_state(&space) else {
        return Ok(());
    };
    let mine: Vec<&SubscriberRole> = state
        .subscriber_roles
        .iter()
        .filter(|s| s.holder == me.device_id())
        .filter(|s| {
            state
                .roles
                .get(&s.role)
                .is_some_and(|r| !r.permissions.contains(Permissions::ADMINISTRATOR))
        })
        .collect();
    let managed: BTreeSet<RoleId> = mine.iter().map(|s| s.role).collect();
    let (binds, granted) = {
        let _g = LOCK.lock();
        let b = load_binds(node, &space);
        (b.binds, b.granted)
    };
    let users: BTreeSet<UserId> = binds
        .iter()
        .map(|b| b.user)
        .chain(granted.iter().map(|g| g.0))
        .filter(|u| only.is_none_or(|o| o == *u))
        .collect();
    let mut last_err = Ok(());
    for user in users {
        let Some(member) = state.members.get(&user) else {
            continue;
        };
        let mut status = HashMap::new();
        for b in binds.iter().filter(|b| b.user == user) {
            let v = lk.active(node, b.provider, &b.account).await;
            status.insert(b.provider, v);
        }
        let has: BTreeSet<RoleId> = member.roles.iter().copied().collect();
        let mine_granted: BTreeSet<RoleId> = granted
            .iter()
            .filter(|g| g.0 == user && managed.contains(&g.1))
            .map(|g| g.1)
            .collect();
        let (add, remove) = plan(&mine, &has, &mine_granted, &status);
        if add.is_empty() && remove.is_empty() {
            continue;
        }
        let roles: Vec<RoleId> = has
            .iter()
            .filter(|r| !remove.contains(r))
            .chain(&add)
            .copied()
            .collect();
        match crate::api::member::apply_roles(node, space, user, &roles).await {
            Ok(()) => {
                let _g = LOCK.lock();
                let mut b = load_binds(node, &space);
                b.granted
                    .retain(|g| !(g.0 == user && remove.contains(&g.1)));
                b.granted.extend(add.iter().map(|r| (user, *r)));
                save_binds(node, &space, &b);
            }
            Err(e) => {
                debug!(space = %space.short(), error = %e.message, "구독자 역할을 바꾸지 못함");
                last_err = Err(e);
            }
        }
    }
    // 더는 확인하지 않는 역할의 기록은 버립니다(멤버의 역할은 그대로).
    {
        let _g = LOCK.lock();
        let mut b = load_binds(node, &space);
        let n = b.granted.len();
        b.granted.retain(|g| managed.contains(&g.1));
        if b.granted.len() != n {
            save_binds(node, &space, &b);
        }
    }
    last_err
}

// ───────────── 크루 설정 ─────────────

/// `crew.set_subscriber_roles`: 빈 경로의 항목은 뺍니다. 그대로인 항목은 원래 확인 기기를, 바뀐 항목은 이 기기를
/// 확인 기기로 둡니다(이 기기에 그 경로의 크리에이터 계정이 있어야 함).
pub(crate) async fn set_config(
    node: &Arc<Node>,
    space: SpaceId,
    input: &[SubscriberRoleInput],
) -> Result<(), RpcError> {
    let (state, me) = rpc(crate::crews::crew_state(node, &space))?;
    rpc(crate::crews::need(&state, &me, Permissions::MANAGE_ROLES))?;
    let device: DeviceId = rpc(node.me())?.device_id();
    let linked: Vec<SubscriptionProvider> = providers()
        .into_iter()
        .filter(|p| load_creator(node, *p).is_some_and(|c| !c.needs_login))
        .collect();
    let my_rank = state.rank_of(&me);
    let unusable = || RpcError::invalid_input("구독자 역할로 쓸 수 없는 역할입니다");
    let mut roles: Vec<SubscriberRole> = Vec::new();
    for i in input {
        let mut providers = i.providers.clone();
        providers.sort_unstable();
        providers.dedup();
        if providers.is_empty() {
            continue;
        }
        let role = state.roles.get(&i.role).ok_or_else(unusable)?;
        if i.role == RoleId::EVERYONE
            || role.permissions.contains(Permissions::ADMINISTRATOR)
            || roles.iter().any(|s| s.role == i.role)
        {
            return Err(unusable());
        }
        let same = state
            .subscriber_roles
            .iter()
            .find(|s| s.role == i.role && s.providers == providers);
        let holder = match same {
            Some(s) => s.holder,
            None => {
                if providers.iter().any(|p| !linked.contains(p)) {
                    return Err(RpcError::invalid_input(
                        "이 기기에 연결하지 않은 크리에이터 계정이 있습니다",
                    ));
                }
                device
            }
        };
        roles.push(SubscriberRole {
            role: i.role,
            providers,
            holder,
        });
    }
    roles.sort_by_key(|s| s.role);
    if roles.len() > yangpa_core::model::MAX_SUBSCRIBER_ROLES {
        return Err(unusable());
    }
    if roles == state.subscriber_roles {
        return Ok(());
    }
    let changed = state
        .subscriber_roles
        .iter()
        .filter(|s| !roles.contains(s))
        .chain(roles.iter().filter(|s| !state.subscriber_roles.contains(s)));
    let too_high = changed.into_iter().any(|s| {
        state
            .roles
            .get(&s.role)
            .is_some_and(|r| r.level() >= my_rank)
    });
    if too_high {
        return Err(RpcError::forbidden(
            "자기보다 순위가 높거나 같은 역할은 주거나 뺄 수 없습니다",
        ));
    }
    let body = EventBody::CrewSubscriberRolesSet { roles };
    rpc(author::publish(node, space, None, &body, KeyChoice::State).await)?;
    sync_soon(node, space);
    Ok(())
}

/// 시험용(`testkit::set_subrole_creator`): 로그인 없이 SellApp 크리에이터 계정을 넣습니다.
pub(crate) fn put_creator_for_test(node: &Node) {
    let c = Creator::new("test".into(), "test".into(), "test".into());
    let _ = save_creator(node, SubscriptionProvider::Sellapp, &c);
}

/// 시험용(`testkit::set_subscription`): 이 상점의 라이선스 키 `key`가 SellApp 구독 `account`의 것이고 그 구독이 `active`.
pub(crate) fn put_subscription_for_test(node: &Node, key: &str, account: &str, active: bool) {
    let s = &node.supporter;
    s.test_keys.lock().insert(key.into(), account.into());
    s.test_active
        .lock()
        .insert((SubscriptionProvider::Sellapp, account.into()), active);
}

/// 시험용(`testkit::send_sellapp_proof`): `by`가 크루 `made_for`에 내려고 만든 증명을 이 기기가 `space`에 냅니다.
pub(crate) async fn send_proof_for_test(
    node: &Arc<Node>,
    space: SpaceId,
    made_for: SpaceId,
    by: UserId,
    key: &str,
) -> &'static str {
    let key = sellapp_key(key).expect("UUID로 끝나는 키");
    let proofs = holders_of(node, &space, SubscriptionProvider::Sellapp)
        .expect("SellApp 구독자 역할")
        .into_iter()
        .map(|h| (h, sellapp_proof(&made_for, &by, &key)))
        .collect();
    ask_holders(node, space, proofs).await
}

// ───────────── 멤버 쪽 ─────────────

/// 그 크루에서 이 경로를 맡은 확인 기기들(없으면 오류).
fn holders_of(
    node: &Node,
    space: &SpaceId,
    p: SubscriptionProvider,
) -> Result<BTreeSet<DeviceId>, RpcError> {
    need_provider(p)?;
    let holders: BTreeSet<DeviceId> = node
        .space_state(space)
        .iter()
        .flat_map(|st| &st.subscriber_roles)
        .filter(|s| s.providers.contains(&p))
        .map(|s| s.holder)
        .collect();
    if holders.is_empty() {
        return Err(RpcError::invalid_input(
            "이 크루에는 이 경로의 구독자 역할이 없습니다",
        ));
    }
    Ok(holders)
}

fn random_nonce() -> [u8; 32] {
    let mut n = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut n);
    n
}

/// 이 기기 몫의 GitHub 로그인을 토큰으로 바꿔, 확인 기기가 받을 토큰도 범위 없는 것일지 봅니다.
async fn check_own_login(
    route: &Route,
    code: &str,
    verifier: &str,
    redirect: &str,
) -> Result<(), &'static str> {
    let (status, v) = github_exchange(route, code, verifier, redirect)
        .await
        .map_err(|_| NETWORK)?;
    if status != 200 || str_field(&v, "access_token").is_none() {
        Err(FAILED)
    } else if narrow(&v) {
        Ok(())
    } else {
        Err(TOO_WIDE)
    }
}

/// 이어지는 로그인. 이 기기 몫(`own`)의 코드를 `check`가 통과시켜야 브라우저를 확인 기기 몫(`steps`)으로 이어 보냅니다. 통과하지
/// 못하면 다른 기기가 쓸 코드는 만들어지지도 않습니다. `steps` 순서의 코드들을 돌려줍니다.
async fn login_chain<F: Future<Output = Result<(), &'static str>>>(
    listener: &TcpListener,
    own: &Authorize,
    steps: &[&Authorize],
    check: impl FnOnce(String) -> F,
) -> Result<Vec<String>, &'static str> {
    let back = login_callback(listener, &own.state).await?;
    let ok = check(back.code.clone()).await;
    let first = steps.first().filter(|_| ok.is_ok());
    back.answer(first.map(|a| a.url.as_str())).await;
    ok?;
    let mut codes = Vec::new();
    for (i, a) in steps.iter().enumerate() {
        match login_callback(listener, &a.state).await {
            Ok(back) => {
                let next = steps.get(i + 1).map(|a| a.url.as_str());
                codes.push(back.answer(next).await);
            }
            Err(e) if codes.is_empty() => return Err(e),
            Err(_) => break,
        }
    }
    Ok(codes)
}

/// GitHub 로그인. 코드는 한 번만 쓸 수 있어 이 기기(권한 보기)와 확인 기기마다 로그인을 하나씩 만들고, 첫 주소만 열면 나머지는
/// 돌아온 브라우저를 이어 보냅니다. 권한 없는 로그인이고, GitHub는 루프백 포트를 가리지 않아 빈 포트를 씁니다.
pub(crate) async fn member_github(
    node: &Arc<Node>,
    space: SpaceId,
) -> Result<SupporterAuthUrl, RpcError> {
    let p = SubscriptionProvider::Github;
    let holders = holders_of(node, &space, p)?;
    let user = rpc(node.me())?.user();
    let cancel = replace_login(node, p);
    let listener = callback_listener(0).await?;
    let port = listener
        .local_addr()
        .map_err(|e| RpcError::internal(e.to_string()))?
        .port();
    let redirect = loopback(port);
    let login = |v: &str| authorize(GITHUB_AUTHORIZE, MEMBER_GITHUB_CLIENT_ID, &redirect, "", v);
    let own_verifier = verifier(&space, &user, &random_nonce());
    let own = login(&own_verifier);
    let steps: Vec<(DeviceId, [u8; 32], Authorize)> = holders
        .into_iter()
        .map(|holder| {
            let nonce = random_nonce();
            (holder, nonce, login(&verifier(&space, &user, &nonce)))
        })
        .collect();
    let url = own.url.clone();
    let route = route(node, p);
    let weak = Arc::downgrade(node);
    node.tasks.spawn(async move {
        let urls: Vec<&Authorize> = steps.iter().map(|s| &s.2).collect();
        let check = |code: String| {
            let (route, redirect, v) = (&route, &redirect, &own_verifier);
            async move { check_own_login(route, &code, v, redirect).await }
        };
        let login = login_chain(&listener, &own, &urls, check);
        let r = tokio::select! {
            r = tokio::time::timeout(LOGIN_WAIT, login) => r.unwrap_or(Err(EXPIRED)),
            _ = cancel.notified() => return,
        };
        drop(listener);
        let Some(node) = weak.upgrade() else { return };
        let result = match r {
            Ok(codes) => {
                let proofs = steps
                    .iter()
                    .zip(codes)
                    .map(|((holder, nonce, _), code)| {
                        let nonce = *nonce;
                        (*holder, SubRoleProof::Github { code, nonce, port })
                    })
                    .collect();
                ask_holders(&node, space, proofs).await
            }
            Err(e) => e,
        };
        notify(&node, Some(space), result);
    });
    Ok(SupporterAuthUrl { url })
}

/// Patreon: 이 기기가 내 후원 번호를 읽어(토큰은 이 기기를 떠나지 않음) 번호마다 크루·사용자에 묶은 값만 보냅니다. 그래서
/// 클라이언트 주인 계정처럼 넓은 토큰이 나오는 계정도 괜찮습니다. 이 기기에 크리에이터 연결이 있으면 `login`(사용자가 고른
/// '다른 계정') 없이는 로그인하지 않고 그 토큰을 씁니다(같은 계정으로 같은 클라이언트에 새로 로그인하면 그 연결이 끊김).
pub(crate) async fn member_patreon(
    node: &Arc<Node>,
    space: SpaceId,
    login: bool,
) -> Result<SubroleBegin, RpcError> {
    let p = SubscriptionProvider::Patreon;
    holders_of(node, &space, p)?;
    let user = rpc(node.me())?.user();
    if !login && load_creator(node, p).is_some_and(|c| !c.needs_login) {
        // 맞추기 잠금을 기다리므로 RPC 밖에서 합니다.
        node.spawn_detached(move |node| async move {
            let r = match creator_member_token(&node).await {
                Ok(token) => send_patreon(&node, space, user, &token).await,
                Err(r) => r,
            };
            notify(&node, Some(space), r);
        });
        return Ok(SubroleBegin { url: None });
    }
    let cancel = replace_login(node, p);
    let pkce = pkce_begin(
        CREW_PATREON_CLIENT_ID,
        CREW_PATREON_PORT,
        MEMBER_PATREON_SCOPE,
    )
    .await?;
    let url = pkce.url.clone();
    let weak = Arc::downgrade(node);
    node.tasks.spawn(async move {
        let token = tokio::select! {
            r = pkce.code() => match r {
                Ok(code) => member_patreon_token(&code, &pkce.verifier, &pkce.redirect).await,
                Err(e) => Err(e),
            },
            _ = cancel.notified() => return,
        };
        drop(pkce);
        let Some(node) = weak.upgrade() else { return };
        let result = match token {
            Ok(t) => send_patreon(&node, space, user, &t).await,
            Err(e) => e,
        };
        notify(&node, Some(space), result);
    });
    Ok(SubroleBegin { url: Some(url) })
}

/// 이 기기의 Patreon 크리에이터 토큰. 내 후원 목록을 읽을 범위가 없거나(이 범위를 더하기 전의 연결) 토큰이 죽었으면 `RELINK`.
async fn creator_member_token(node: &Node) -> Result<Zeroizing<String>, &'static str> {
    let p = SubscriptionProvider::Patreon;
    let _one = node.supporter.subrole_sync.lock().await;
    let mut c = load_creator(node, p)
        .filter(|c| !c.needs_login)
        .ok_or(RELINK)?;
    if !c
        .scope
        .split_whitespace()
        .any(|s| s == "identity.memberships")
    {
        return Err(RELINK);
    }
    match patreon_refresh_if_needed(&mut c).await {
        Ok(()) => {}
        Err(CheckError::Unauthorized) => {
            mark_needs_login(node, p);
            return Err(RELINK);
        }
        Err(CheckError::Failed(_)) => return Err(NETWORK),
    }
    let _ = save_creator(node, p, &c);
    Ok(Zeroizing::new(c.token.clone()))
}

async fn member_patreon_token(
    code: &str,
    verifier: &str,
    redirect: &str,
) -> Result<Zeroizing<String>, &'static str> {
    let (status, v) = pkce_exchange(
        &Route::Direct,
        CREW_PATREON_CLIENT_ID,
        code,
        verifier,
        redirect,
    )
    .await
    .map_err(|_| NETWORK)?;
    let c = patreon_tokens(status, &v).ok_or(FAILED)?;
    Ok(Zeroizing::new(c.token.clone()))
}

/// 내 후원 번호를 읽어 그 크루의 확인 기기마다 보냅니다.
async fn send_patreon(node: &Arc<Node>, space: SpaceId, user: UserId, token: &str) -> &'static str {
    let ids = match my_patreon_members(token).await {
        Ok(ids) => ids,
        Err(CheckError::Unauthorized) => return FAILED,
        Err(_) => return NETWORK,
    };
    if ids.is_empty() {
        return NOT_SUBSCRIBED;
    }
    let tags = patreon_tags(&space, &user, &ids);
    let Ok(holders) = holders_of(node, &space, SubscriptionProvider::Patreon) else {
        return FAILED;
    };
    let proofs = holders
        .into_iter()
        .map(|h| (h, SubRoleProof::Patreon { tags: tags.clone() }))
        .collect();
    ask_holders(node, space, proofs).await
}

/// 후원 번호마다 크루·사용자에 묶은 값을 무작위 값으로 늘 `MAX_PATREON_TAGS`개까지 채워 정렬합니다. 확인 기기가 개수·순서로
/// 내가 몇 곳에 후원하는지 알지 못하게 합니다.
fn patreon_tags(space: &SpaceId, user: &UserId, ids: &[String]) -> Vec<[u8; 32]> {
    let mut tags: Vec<[u8; 32]> = ids
        .iter()
        .map(|m| bound(space, user, m.as_bytes()))
        .collect();
    tags.resize_with(MAX_PATREON_TAGS, random_nonce);
    tags.sort_unstable();
    tags
}

/// 내 Patreon 후원 번호들(모든 캠페인).
async fn my_patreon_members(token: &str) -> Result<Vec<String>, CheckError> {
    let url =
        format!("{PATREON_API}/identity?include=memberships&fields%5Bmember%5D=patron_status");
    let auth = Zeroizing::new(format!("Bearer {token}"));
    let (status, v) = call(
        &Route::Direct,
        "GET",
        &url,
        &[("Authorization", &auth)],
        &[],
    )
    .await?;
    match status {
        401 => Err(CheckError::Unauthorized),
        200 => member_ids(&v).ok_or_else(|| CheckError::Failed("Patreon 응답 형식".into())),
        s => Err(CheckError::Failed(format!("Patreon 응답 {s}"))),
    }
}

/// `identity?include=memberships`의 후원 번호들(UUID만, `MAX_PATREON_TAGS`개까지).
fn member_ids(v: &Value) -> Option<Vec<String>> {
    let list = v
        .pointer("/data/relationships/memberships/data")?
        .as_array()?;
    Some(
        list.iter()
            .filter_map(|m| str_field(m, "id").and_then(patreon_member_id))
            .take(MAX_PATREON_TAGS)
            .collect(),
    )
}

pub(crate) async fn member_sellapp(
    node: &Arc<Node>,
    space: SpaceId,
    key: &str,
) -> Result<(), RpcError> {
    let holders = holders_of(node, &space, SubscriptionProvider::Sellapp)?;
    let user = rpc(node.me())?.user();
    let key =
        sellapp_key(key).ok_or_else(|| RpcError::invalid_input("라이선스 키를 확인하세요"))?;
    let proofs = holders
        .into_iter()
        .map(|h| (h, sellapp_proof(&space, &user, &key)))
        .collect();
    let result = ask_holders(node, space, proofs).await;
    notify(node, Some(space), result);
    Ok(())
}

const GRANTED: &str = "granted";
const NOT_SUBSCRIBED: &str = "not_subscribed";
const PENDING: &str = "pending";
const INVALID: &str = "invalid";
const UNREACHABLE: &str = "unreachable";
/// 이 계정의 로그인이 요청보다 넓은 권한을 열어 다른 기기로 보내지 않음.
const TOO_WIDE: &str = "too_wide";
/// 이 기기의 Patreon 크리에이터 연결로 내 후원 목록을 읽지 못함: 다시 연결하거나 '다른 계정'으로.
const RELINK: &str = "relink";

/// 확인 기기마다 그 기기 몫의 증명을 보내고 가장 좋은 결과를 돌려줍니다.
async fn ask_holders(
    node: &Arc<Node>,
    space: SpaceId,
    proofs: Vec<(DeviceId, SubRoleProof)>,
) -> &'static str {
    let rank = |r: &str| match r {
        GRANTED => 5,
        PENDING => 4,
        NOT_SUBSCRIBED => 3,
        INVALID => 2,
        FAILED => 1,
        _ => 0,
    };
    let Ok(me) = node.me() else {
        return FAILED;
    };
    let mut best = UNREACHABLE;
    for (holder, proof) in proofs {
        let ask = SubRoleAsk { space, proof };
        let resp = if holder == me.device_id() {
            Some(serve(node, me.user(), ask).await)
        } else {
            match crate::ai::reach_holder(node, space, holder).await {
                Some(peer) => crate::p2p::request(node, peer, YpRequest::SubRole(Box::new(ask)))
                    .await
                    .inspect_err(|e| debug!(%peer, error = %e, "구독자 증명 보내기 실패"))
                    .ok(),
                None => None,
            }
        };
        let r = match resp {
            None => UNREACHABLE,
            Some(YpResponse::Ok) => GRANTED,
            // 확인이 늦어 먼저 답한 것만 '확인 중'. 한 시간 한도에 걸린 것(긴 `retry_after_ms`)은 확인하지 않습니다.
            Some(YpResponse::Busy { retry_after_ms })
                if u128::from(retry_after_ms) <= ANSWER_WAIT.as_millis() =>
            {
                PENDING
            }
            Some(YpResponse::Busy { .. }) => FAILED,
            Some(YpResponse::Error { code, .. }) => match code {
                WireErrorCode::Forbidden => NOT_SUBSCRIBED,
                WireErrorCode::BadRequest => INVALID,
                _ => FAILED,
            },
            Some(_) => FAILED,
        };
        if rank(r) > rank(best) {
            best = r;
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_page_keeps_recurring_sponsors() {
        let v = json!({"data": {"viewer": {"sponsorshipsAsMaintainer": {
        "pageInfo": {"hasNextPage": true, "endCursor": "c1"},
        "nodes": [
            {"isOneTimePayment": false, "sponsorEntity": {"databaseId": 7}},
            {"isOneTimePayment": true, "sponsorEntity": {"databaseId": 8}},
            {"isOneTimePayment": false, "sponsorEntity": {}}
        ]}}}});
        assert_eq!(
            github_sponsors_page(&v),
            Some((vec!["7".to_owned()], Some("c1".to_owned())))
        );
        let last = json!({"data": {"viewer": {"sponsorshipsAsMaintainer": {
            "pageInfo": {"hasNextPage": false, "endCursor": "c2"}, "nodes": []}}}});
        assert_eq!(github_sponsors_page(&last), Some((vec![], None)));
        assert_eq!(github_sponsors_page(&json!({"errors": [{}]})), None);
    }

    #[test]
    fn patreon_page_keeps_paying_patrons() {
        let m = |id: &str, attrs: Value| {
            json!({"type": "member", "id": format!("0000000{id}-0000-4000-8000-000000000000"), "attributes": attrs,
                   "relationships": {"user": {"data": {"id": id, "type": "user"}}}})
        };
        let paid =
            json!({"patron_status": "active_patron", "currently_entitled_amount_cents": 500});
        let mut v = json!({"data": [
            m("1", paid.clone()),
            m("2", json!({"patron_status": "former_patron", "currently_entitled_amount_cents": 500})),
            m("3", json!({"patron_status": "active_patron", "currently_entitled_amount_cents": 0})),
            m("4", json!({"patron_status": "active_patron", "currently_entitled_amount_cents": 500, "is_free_trial": true})),
            m("5", json!({"patron_status": "active_patron", "currently_entitled_amount_cents": 500, "is_gifted": true}))
        ], "meta": {"pagination": {"cursors": {"next": "n1"}}}});
        let (rows, next) = patreon_members_page(&v).unwrap();
        let paying: Vec<&str> = rows
            .iter()
            .filter(|r| r.paying)
            .map(|r| r.user.as_str())
            .collect();
        assert_eq!((paying, rows.len()), (vec!["1"], 5));
        assert_eq!(rows[1].member, "00000002-0000-4000-8000-000000000000");
        assert_eq!(next.as_deref(), Some("n1"));
        v["meta"]["pagination"]["cursors"]["next"] = Value::Null;
        assert_eq!(patreon_members_page(&v).unwrap().1, None);
    }

    /// 로그인에 건 값과 확인 기기가 다시 계산한 값이 같은 크루·사용자에서만 맞습니다.
    #[test]
    fn proofs_fit_only_their_crew_and_user() {
        let (space, other_space) = (SpaceId::random(), SpaceId::random());
        let (user, other_user) = (UserId::from_bytes([1; 32]), UserId::from_bytes([2; 32]));
        let nonce = [7u8; 32];
        let v = verifier(&space, &user, &nonce);
        // RFC 7636: 43~128자.
        assert_eq!(v.len(), 43);
        let url = authorize(GITHUB_AUTHORIZE, "id", &loopback(5000), "", &v).url;
        let q: HashMap<String, String> = url::Url::parse(&url)
            .unwrap()
            .query_pairs()
            .into_owned()
            .collect();
        let challenge = |v: &str| URL_SAFE_NO_PAD.encode(Sha256::digest(v.as_bytes()));
        assert_eq!(
            q["code_challenge"],
            challenge(&verifier(&space, &user, &nonce))
        );
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(q["redirect_uri"], "http://127.0.0.1:5000/callback");
        for wrong in [
            verifier(&other_space, &user, &nonce),
            verifier(&space, &other_user, &nonce),
            verifier(&space, &user, &[8; 32]),
        ] {
            assert_ne!(q["code_challenge"], challenge(&wrong));
        }
    }

    /// 전에 더 넓게 승인한 GitHub 계정의 토큰은 쓰지 않습니다.
    #[test]
    fn github_token_must_be_narrow() {
        let scope = |s: &str| json!({"access_token": "t", "scope": s});
        assert_eq!(github_token(200, &scope("")).unwrap().as_str(), "t");
        for v in [
            scope("repo"),
            scope("read:user"),
            json!({"access_token": "t"}),
        ] {
            assert!(!narrow(&v), "{v}");
            assert!(
                matches!(github_token(200, &v), Err(CheckError::Unauthorized)),
                "{v}"
            );
        }
        for (status, v, bad_proof) in [
            (200, json!({"error": "bad_verification_code"}), true),
            (200, json!({"error": "redirect_uri_mismatch"}), true),
            (200, json!({"error": "incorrect_client_credentials"}), false),
            (500, scope(""), false),
        ] {
            let unauthorized = matches!(github_token(status, &v), Err(CheckError::Unauthorized));
            assert_eq!(unauthorized, bad_proof, "{status} {v}");
        }
    }

    const MEMBER_A: &str = "03ca69c3-ebea-4b9a-8fac-e4a837873254";
    const MEMBER_B: &str = "d485d5ac-6c82-42c6-9c08-c50cf01b73d7";

    fn row(member: &str, user: &str) -> PatronRow {
        PatronRow {
            member: member.into(),
            user: user.into(),
            paying: true,
        }
    }

    /// 후원 번호로 만든 값은 그 크루·그 사용자에게만 맞고, 번호를 모아 낸 증명은 거절합니다.
    #[test]
    fn patreon_tags_fit_only_their_crew_and_user() {
        let (space, user) = (SpaceId::random(), UserId::from_bytes([1; 32]));
        let rows = [row(MEMBER_A, "u-a"), row(MEMBER_B, "u-b")];
        let tags = |space: &SpaceId, user: &UserId, ids: &[&str]| -> Vec<[u8; 32]> {
            ids.iter()
                .map(|m| bound(space, user, m.as_bytes()))
                .collect()
        };
        let other = "11111111-2222-4333-8444-555555555555";
        let mine = tags(&space, &user, &[other, MEMBER_A]);
        assert_eq!(
            patreon_match(&rows, &space, &user, &mine).unwrap(),
            Some("u-a".into())
        );
        // 다른 크루·다른 사용자 이름으로 내면 아무것도 맞지 않습니다(구독 없음).
        let elsewhere = SpaceId::random();
        assert_eq!(
            patreon_match(&rows, &elsewhere, &user, &mine).unwrap(),
            None
        );
        let someone = UserId::from_bytes([2; 32]);
        assert_eq!(patreon_match(&rows, &space, &someone, &mine).unwrap(), None);
        let both = tags(&space, &user, &[MEMBER_A, MEMBER_B]);
        assert!(matches!(
            patreon_match(&rows, &space, &user, &both),
            Err(CheckError::Unauthorized)
        ));
        // 실제로 보내는 값은 늘 한도만큼 채워지고 정렬되며, 채운 값 사이에서도 내 번호가 맞습니다.
        let sent = patreon_tags(&space, &user, &[other.into(), MEMBER_A.into()]);
        assert_eq!(sent.len(), MAX_PATREON_TAGS);
        assert!(sent.is_sorted());
        assert_eq!(
            patreon_match(&rows, &space, &user, &sent).unwrap(),
            Some("u-a".into())
        );
    }

    #[test]
    fn my_patreon_member_ids_are_uuids() {
        let v = json!({"data": {"id": "1", "relationships": {"memberships": {"data": [
            {"id": MEMBER_A.to_uppercase(), "type": "member"},
            {"id": "12345", "type": "member"},
            {"id": MEMBER_B, "type": "member"}
        ]}}}});
        assert_eq!(member_ids(&v).unwrap(), [MEMBER_A, MEMBER_B]);
        assert_eq!(member_ids(&json!({"data": {"id": "1"}})), None);
        let many: Vec<Value> = (0..300)
            .map(|i| json!({"id": format!("{i:08x}-0000-4000-8000-000000000000")}))
            .collect();
        let v = json!({"data": {"relationships": {"memberships": {"data": many}}}});
        assert_eq!(member_ids(&v).unwrap().len(), MAX_PATREON_TAGS);
    }

    /// 브라우저 흉내: 루프백에 GET을 보내고 답을 돌려줍니다.
    async fn browse(port: u16, path: String) -> String {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let mut c = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        let req = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n");
        c.write_all(req.as_bytes()).await.unwrap();
        let mut out = String::new();
        c.read_to_string(&mut out).await.unwrap();
        out
    }

    fn login(port: u16) -> Authorize {
        authorize(GITHUB_AUTHORIZE, "id", &loopback(port), "", "v")
    }

    /// 이 기기 몫을 먼저 보고, 통과하면 확인 기기마다의 로그인으로 브라우저를 이어 보냅니다.
    #[tokio::test]
    async fn browser_is_sent_on_after_own_login_passes() {
        let listener = callback_listener(0).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (own, a, b) = (login(port), login(port), login(port));
        let steps = [&a, &b];
        let server = login_chain(&listener, &own, &steps, |code| async move {
            assert_eq!(code, "c0");
            Ok(())
        });
        let browser = async {
            // 다른 로그인의 `state`는 받지 않습니다(확인 기기 몫도 차례가 아니면).
            let bad = browse(port, format!("/callback?code=x&state={}", a.state)).await;
            let own = browse(port, format!("/callback?code=c0&state={}", own.state)).await;
            let one = browse(port, format!("/callback?code=c1&state={}", a.state)).await;
            let two = browse(port, format!("/callback?code=c2&state={}", b.state)).await;
            (bad, own, one, two)
        };
        let (codes, (bad, first, second, third)) = tokio::join!(server, browser);
        assert_eq!(codes.unwrap(), ["c1", "c2"]);
        assert!(bad.starts_with("HTTP/1.1 400"));
        for (answer, next) in [(first, &a), (second, &b)] {
            assert!(answer.starts_with("HTTP/1.1 302"));
            assert!(answer.contains(&format!("Location: {}\r\n", next.url)));
        }
        assert!(third.starts_with("HTTP/1.1 200"));
    }

    /// 넓은 토큰이 나오는 계정이면 브라우저를 확인 기기 몫으로 보내지 않아, 보낼 코드가 생기지 않습니다.
    #[tokio::test]
    async fn wide_account_stops_before_any_code_for_others() {
        let listener = callback_listener(0).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (own, a) = (login(port), login(port));
        let steps = [&a];
        let server = login_chain(&listener, &own, &steps, |_| async { Err(TOO_WIDE) });
        let browser = browse(port, format!("/callback?code=c0&state={}", own.state));
        let (codes, answer) = tokio::join!(server, browser);
        assert_eq!(codes, Err(TOO_WIDE));
        assert!(answer.starts_with("HTTP/1.1 200"));
        assert!(!answer.contains("Location"));
    }

    /// 실측: GitHub 경로의 안전은 GitHub가 다른 `code_verifier`를 거절하는 데 달려 있습니다. 브라우저 로그인이 두 번 필요해 손으로
    /// 돌립니다(`--ignored --nocapture`). 토큰은 이 시험 안에서만 쓰고 버립니다.
    #[tokio::test]
    #[ignore = "브라우저 로그인과 MEMBER_GITHUB_CLIENT_ID·SECRET 필요"]
    async fn live_pkce_github() {
        let listener = callback_listener(0).await.unwrap();
        let redirect = loopback(listener.local_addr().unwrap().port());
        let (space, user) = (SpaceId::random(), UserId::from_bytes([1; 32]));
        for (right, name) in [(false, "틀린"), (true, "맞는")] {
            let nonce = random_nonce();
            let v = verifier(&space, &user, &nonce);
            let a = authorize(GITHUB_AUTHORIZE, MEMBER_GITHUB_CLIENT_ID, &redirect, "", &v);
            println!("{name} verifier 시험: 이 주소를 열고 승인하세요\n{}", a.url);
            let back = tokio::time::timeout(LOGIN_WAIT, login_callback(&listener, &a.state))
                .await
                .expect("시간 초과")
                .unwrap();
            let code = back.answer(None).await;
            let used = if right {
                v
            } else {
                verifier(&SpaceId::random(), &user, &nonce)
            };
            let (status, body) = github_exchange(&Route::Direct, &code, &used, &redirect)
                .await
                .unwrap();
            let got = str_field(&body, "access_token").is_some();
            println!(
                "{name} verifier: 상태 {status}, 토큰 {got}, 오류 {:?}, 범위 {:?}",
                body.get("error"),
                body.get("scope"),
            );
            assert_eq!(got, right, "{name} verifier");
            // 전에 이 앱에 범위를 승인한 계정이면 여기서 실패합니다(앱에서는 `too_wide`).
            assert!(!right || narrow(&body), "범위 없는 토큰이 아님");
        }
    }

    /// 실측: 후원자 기기가 보는 후원 번호와 크리에이터가 보는 후원 번호가 같은지, 크리에이터 토큰으로 내 후원 목록을 읽을 수
    /// 있는지(크리에이터 기기의 후원자 증명). 크리에이터 계정으로 한 번, 그 캠페인 멤버인 다른 계정으로 한 번 로그인합니다
    /// (`--ignored --nocapture`).
    #[tokio::test]
    #[ignore = "브라우저 로그인 두 번 필요"]
    async fn live_patreon_member_ids_match() {
        let token = |scope: &'static str| async move {
            let pkce = pkce_begin(CREW_PATREON_CLIENT_ID, CREW_PATREON_PORT, scope)
                .await
                .unwrap();
            println!("{scope} 로그인: 이 주소를 열고 승인하세요\n{}", pkce.url);
            let code = pkce.code().await.unwrap();
            let (status, v) = pkce_exchange(
                &Route::Direct,
                CREW_PATREON_CLIENT_ID,
                &code,
                &pkce.verifier,
                &pkce.redirect,
            )
            .await
            .unwrap();
            patreon_tokens(status, &v).expect("토큰")
        };
        let mut creator = token(CREATOR_PATREON_SCOPE).await;
        println!("크리에이터 범위: {}", creator.scope);
        assert!(
            creator
                .scope
                .split_whitespace()
                .any(|s| s == "identity.memberships"),
            "크리에이터 토큰으로 내 후원 목록을 읽을 수 있어야 함"
        );
        my_patreon_members(&creator.token)
            .await
            .expect("크리에이터 토큰으로 내 후원 목록");
        let auth = format!("Bearer {}", creator.token);
        let (_, v) = call(
            &Route::Direct,
            "GET",
            &format!("{PATREON_API}/campaigns"),
            &[("Authorization", &auth)],
            &[],
        )
        .await
        .unwrap();
        creator.target = v
            .pointer("/data/0/id")
            .and_then(Value::as_str)
            .expect("캠페인")
            .to_owned();
        let rows = patreon_members(&creator).await.unwrap();
        let patron = token(MEMBER_PATREON_SCOPE).await;
        let mine = my_patreon_members(&patron.token).await.unwrap();
        let same: Vec<&PatronRow> = rows.iter().filter(|r| mine.contains(&r.member)).collect();
        println!(
            "크리에이터 쪽 {}개, 후원자 쪽 {}개, 겹침 {}개",
            rows.len(),
            mine.len(),
            same.len()
        );
        assert_eq!(same.len(), 1, "같은 후원 번호가 하나 보여야 함");
    }

    #[test]
    fn sellapp_key_is_the_uuid_tail() {
        let uuid = "01965f1d-f038-7116-b57f-9e7ecb4e7b8f";
        for k in [uuid, " SHOP-01965F1D-F038-7116-B57F-9E7ECB4E7B8F\n"] {
            assert_eq!(sellapp_key(k).unwrap().as_str(), uuid);
        }
        for k in [
            "",
            "KEY-B",
            &uuid[1..],
            "01965f1d-f038-7116-b57f-9e7ecb4e7bxy",
            "가나다",
        ] {
            assert!(sellapp_key(k).is_none(), "{k}");
        }
        let (space, user) = (SpaceId::random(), UserId::from_bytes([1; 32]));
        let SubRoleProof::Sellapp { tag, hint } = &sellapp_proof(&space, &user, uuid) else {
            panic!()
        };
        assert_eq!(hint, "01965f1d");
        assert_eq!(*tag, bound(&space, &user, uuid.as_bytes()));
        assert_ne!(*tag, bound(&SpaceId::random(), &user, uuid.as_bytes()));
    }

    #[test]
    fn sellapp_needs_active_unpaused_subscription() {
        let sub =
            |status: &str, paused: Value| json!({"data": {"status": status, "paused_at": paused}});
        assert!(sellapp_sub_active(&sub("ACTIVE", Value::Null)));
        assert!(sellapp_sub_active(&json!({"status": "ACTIVE"})));
        assert!(!sellapp_sub_active(&sub(
            "ACTIVE",
            json!("2026-11-01T00:00:00Z")
        )));
        for s in ["TRIALING", "PAST_DUE", "UNPAID", "CANCELED"] {
            assert!(!sellapp_sub_active(&sub(s, Value::Null)), "{s}");
        }
        let row = |sub: Value| json!({"key": "k", "subscription_id": sub});
        assert_eq!(sellapp_key_subscription(&row(json!(42))), Some("42".into()));
        assert_eq!(
            sellapp_key_subscription(&row(json!("s-1"))),
            Some("s-1".into())
        );
        // 1회 구매 상품의 키는 구독이 없습니다.
        assert_eq!(sellapp_key_subscription(&row(Value::Null)), None);
    }

    #[test]
    fn plan_adds_on_subscription_and_removes_only_own_grants() {
        use SubscriptionProvider::{Github, Patreon};
        let (r1, r2) = (RoleId([1; 16]), RoleId([2; 16]));
        let e1 = SubscriberRole {
            role: r1,
            providers: vec![Github, Patreon],
            holder: DeviceId([9; 32]),
        };
        let e2 = SubscriberRole {
            role: r2,
            providers: vec![Patreon],
            holder: DeviceId([9; 32]),
        };
        let mine = [&e1, &e2];
        let set = |r: &[RoleId]| r.iter().copied().collect::<BTreeSet<_>>();
        let st = |v: &[(SubscriptionProvider, Option<bool>)]| {
            v.iter().copied().collect::<HashMap<_, _>>()
        };
        // GitHub 구독이면 r1을 줌(Patreon만 받는 r2는 아님).
        assert_eq!(
            plan(&mine, &set(&[]), &set(&[]), &st(&[(Github, Some(true))])),
            (vec![r1], vec![])
        );
        // 끝났으면 내가 준 것만 뺌.
        let ended = st(&[(Github, Some(false))]);
        assert_eq!(
            plan(&mine, &set(&[r1]), &set(&[r1]), &ended),
            (vec![], vec![r1])
        );
        assert_eq!(
            plan(&mine, &set(&[r1]), &set(&[]), &ended),
            (vec![], vec![])
        );
        // 한 경로라도 닿지 못했으면 그대로, 다른 경로가 구독 중이면 남김.
        let mixed = st(&[(Github, Some(false)), (Patreon, None)]);
        assert_eq!(
            plan(&mine, &set(&[r1]), &set(&[r1]), &mixed),
            (vec![], vec![])
        );
        let other = st(&[(Github, Some(false)), (Patreon, Some(true))]);
        assert_eq!(
            plan(&mine, &set(&[r1]), &set(&[r1]), &other),
            (vec![r2], vec![])
        );
        // 묶인 계정이 없어졌으면(다른 사용자가 가져감) 내가 준 역할을 뺌.
        assert_eq!(
            plan(&mine, &set(&[r2]), &set(&[r2]), &st(&[])),
            (vec![], vec![r2])
        );
    }
}
