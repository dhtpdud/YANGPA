//! OS 알림.
//!
//! - 1대1 공간: 남이 보낸 새 메시지마다(글타래 답글 포함). 크루: 설정대로(기본은 모든 메시지, 글타래 답글은 나를
//!   멘션했을 때만). 크루·채널마다 수준을 바꾸거나 잠시 끌 수 있고(`notifyRules`), 끄는 동안은 멘션만 알립니다.
//!   본인 공간: 없음. 내 ID를 아직 모르면(온보딩 중) 내 글과 구분할 수 없어 메시지는 알리지 않습니다.
//! - 멘션: 나를 부른 `<@ID>`, 또는 데몬이 나를 멘션했다고 표시한 글(권한 있는 사람의 `@everyone`·`@here`).
//! - 창이 포커스를 가진 동안에도 다른 채널의 메시지는 띄웁니다. 지금 보고 있는 채널은 띄우지 않습니다.
//!   친구 요청·AI 작업 확인은 창 안에서도 알리므로 창이 뒤에 있을 때만.
//! - 오프라인 동안 쓰인 메시지(작성 시각이 다시 온라인이 된 시각보다 앞섬)는 멘션이어도 띄우지 않습니다. 앱의 안 읽음
//!   표시만 남습니다. 그 시각은 데몬에 새로 붙을 때(앱을 켬, 오프라인 모드에서 돌아옴), PC가 잠들었다 깰 때, 망이 끊긴
//!   동안(연결이 하나도 없음) 잡습니다. 폰은 CPU가 잠든 것을 오프라인으로 치지 않고 나갈 길이 없을 때만 칩니다.
//! - 휘발성 메시지는 본문 없이 제목만(데몬도 본문을 비우지만 여기서 한 번 더 막습니다). 미리보기를 끄면 메시지 알림이
//!   본문 없이 나갑니다(윈도 알림 센터는 알림 내용을 디스크에 남깁니다). 친구 요청·AI 작업 확인은 이름만 담습니다.
//! - 한 묶음에 넷 이상이면, 또는 알림을 띄운 뒤 1.5초 안에 둘 이상 더 오면 "새 알림 N개" 하나로 묶습니다(본문 없음).
//! - 누르면 창을 앞으로 가져오고 그 채널을 엽니다(`yangpa-open-channel` 이벤트). 창이 뒤에 있으면 작업 표시줄도
//!   깜빡입니다(윈도가 알림을 버려도 보이게).
//! - 창이 앞에 있는지는 윈도에서 전경 창을 직접 보고(WebView2 포커스 이벤트는 시작할 때 첫 포커스를 놓칠 수
//!   있음), 다른 운영체제는 `WindowEvent::Focused`로 따라갑니다.
//!
//! 보고 있는 채널과 알림에 붙인 채널 ID는 이 프로세스 메모리에만 둡니다(휘발성 채널일 수 있어 로그·파일에 남기지 않음).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde_json::{Map, Value, json};
use socket2::{Domain, Socket, Type};
#[cfg(desktop)]
use tauri::UserAttentionType;
use tauri::{AppHandle, Emitter, Manager, Runtime};
use tauri_plugin_notification::ActionPerformed;
#[cfg(not(target_os = "android"))]
use tauri_plugin_notification::NotificationExt;
use tracing::debug;
use yangpa_core::UserId;
use yangpa_core::ids::{ChannelId, SpaceId};
use yangpa_core::model::SpaceKind;
use yangpa_core::time::Timestamp;
use yangpa_rpc::api::Notification;

use crate::avatar;
use crate::daemon::{Daemon, MAIN_WINDOW};

/// 알림을 눌렀을 때 웹뷰로 보내는 이벤트 이름(`{space, channel}`).
pub const OPEN_CHANNEL: &str = "yangpa-open-channel";

/// 알림 사이 최소 간격.
const GAP: Duration = Duration::from_millis(1500);
/// 한 묶음에서 이보다 많으면 요약 하나로.
const SUMMARY_OVER: usize = 3;
const TITLE_MAX: usize = 64;
const BODY_MAX: usize = 160;
/// PC가 잠들었는지, 망이 끊겼는지 보는 간격.
const OFFLINE_TICK: Duration = Duration::from_secs(5);
/// 틱 사이가 이보다 벌어지면 PC가 잠들었던 것으로 봅니다.
const SLEPT_MS: u64 = 30_000;
/// 연결이 없어도 나갈 길이 있으면 이만큼 전 글까지는 실시간으로 칩니다. 망이 끊긴 것이 아니라 끊겨 있던 친구가 글을
/// 보내며 다시 거는 중일 수 있습니다(Tor는 수십 초 걸림).
const DIAL_GRACE_MS: u64 = 60_000;
/// 폰은 화면이 꺼지면 CPU가 수시로 잠들고 그동안 온 글을 알리려고 서비스를 띄워 둡니다. 틱이 밀리거나 연결이 끊긴
/// 것만으로는 오프라인이 아닙니다.
const PHONE: bool = cfg!(target_os = "android");

/// 알림이 가리키는 채널.
pub type Target = (SpaceId, ChannelId);

/// 크루 메시지를 얼마나 알릴지.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CrewLevel {
    #[default]
    All,
    Mentions,
    Off,
}

impl CrewLevel {
    fn parse(v: Option<&Value>) -> Option<Self> {
        match v.and_then(Value::as_str) {
            Some("all") => Some(CrewLevel::All),
            Some("mentions") => Some(CrewLevel::Mentions),
            Some("off") => Some(CrewLevel::Off),
            _ => None,
        }
    }
}

/// 알림 설정(UI 설정 파일의 `notify`·`notifyCrew`·`notifyPreview`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NotifyPrefs {
    pub enabled: bool,
    pub crew: CrewLevel,
    pub preview: bool,
}

impl Default for NotifyPrefs {
    fn default() -> Self {
        NotifyPrefs {
            enabled: true,
            crew: CrewLevel::All,
            preview: true,
        }
    }
}

impl NotifyPrefs {
    /// UI 설정에서 읽습니다. 없거나 잘못된 값은 기본값.
    pub fn from_map(m: &Map<String, Value>) -> Self {
        let d = NotifyPrefs::default();
        let flag = |k: &str, def: bool| m.get(k).and_then(Value::as_bool).unwrap_or(def);
        NotifyPrefs {
            enabled: flag("notify", d.enabled),
            crew: CrewLevel::parse(m.get("notifyCrew")).unwrap_or(d.crew),
            preview: flag("notifyPreview", d.preview),
        }
    }
}

/// 읽는 크루·채널 규칙 수 상한(앱 `MAX_NOTIFY_RULES`와 같음, 설정 파일 64KiB 안에 들게).
const MAX_RULES: usize = 100;

/// 크루나 채널 하나의 알림 규칙.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rule {
    /// 없으면 위(채널 → 크루 → `notifyCrew`)를 따릅니다.
    pub level: Option<CrewLevel>,
    /// 이 시각(유닉스 ms)까지 알림 끔.
    pub mute_until: u64,
}

/// 크루·채널별 알림 규칙(UI 설정의 `notifyRules`, 키는 `공간` 또는 `공간/채널`). 휘발성 채널은 앱이 넣지 않습니다.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NotifyRules(HashMap<String, Rule>);

impl NotifyRules {
    pub fn from_map(m: &Map<String, Value>) -> Self {
        let Some(rules) = m.get("notifyRules").and_then(Value::as_object) else {
            return Self::default();
        };
        let rule = |v: &Value| {
            let v = v.as_object()?;
            Some(Rule {
                level: CrewLevel::parse(v.get("level")),
                mute_until: v.get("muteUntil").and_then(Value::as_u64).unwrap_or(0),
            })
        };
        Self(
            rules
                .iter()
                .take(MAX_RULES)
                .filter_map(|(k, v)| Some((k.clone(), rule(v)?)))
                .collect(),
        )
    }

    /// 이 채널의 크루 메시지 알림 수준. 채널 수준이 크루 수준보다 앞서고, 크루나 채널을 끈 동안은 디스코드처럼
    /// '모든 메시지'도 멘션만 알립니다(멘션까지 막으려면 수준을 '끄기'로).
    fn level(
        &self,
        space: &SpaceId,
        channel: &ChannelId,
        default: CrewLevel,
        now_ms: u64,
    ) -> CrewLevel {
        let crew = self.0.get(&space.to_string());
        let chan = self.0.get(&format!("{space}/{channel}"));
        let level = chan
            .and_then(|r| r.level)
            .or(crew.and_then(|r| r.level))
            .unwrap_or(default);
        let muted = [crew, chan]
            .into_iter()
            .flatten()
            .any(|r| r.mute_until > now_ms);
        if muted && level == CrewLevel::All {
            CrewLevel::Mentions
        } else {
            level
        }
    }
}

/// 알림을 고를 때 보는 지금 상태.
#[derive(Clone, Copy, Debug)]
pub struct Context<'a> {
    pub me: Option<&'a UserId>,
    pub prefs: &'a NotifyPrefs,
    pub rules: &'a NotifyRules,
    /// 지금 시각(유닉스 ms, 알림 끔이 끝났는지 볼 때).
    pub now_ms: u64,
    /// 다시 온라인이 된 시각(유닉스 ms). 이보다 먼저 쓰인 메시지는 띄우지 않습니다.
    pub online_since_ms: u64,
    /// 메인 창이 포커스를 가졌는지.
    pub focused: bool,
    /// 웹뷰가 열어 둔 채널.
    pub viewing: Option<Target>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notice {
    pub title: String,
    pub body: Option<String>,
    /// 누르면 열 채널. 요약·친구 요청·확인 요청은 없음.
    pub target: Option<Target>,
    /// 글타래 답글(보고 있는 채널이어도 띄움, 패널이 닫혀 있을 수 있어서).
    pub thread: bool,
    /// 보낸 사람 프사(메시지 알림만, 휘발성 채널은 없음).
    pub avatar: Option<avatar::Spec>,
}

fn summary(count: usize) -> Notice {
    Notice {
        title: crate::i18n::text("newNotices", "새 알림 {n}개", &[("n", &count.to_string())]),
        body: None,
        target: None,
        thread: false,
        avatar: None,
    }
}

/// 지금 보고 있는 채널이라 화면에 바로 보이는지(그러면 띄우지 않음).
fn on_screen(n: &Notice, focused: bool, viewing: Option<Target>) -> bool {
    focused && !n.thread && n.target.is_some() && n.target == viewing
}

/// 글자 단위로 자르고 말줄임표를 붙입니다.
pub fn truncate(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// 묶음 하나에서 띄울 알림을 고르고, 많으면 하나로 묶습니다(순수 함수).
pub fn plan(
    batch: &[Notification],
    cx: Context<'_>,
    kind_of: impl Fn(&SpaceId) -> Option<SpaceKind>,
) -> Vec<Notice> {
    summarize(select(batch, cx, kind_of))
}

/// 넷 이상이면 "새 알림 N개" 하나로.
fn summarize(out: Vec<Notice>) -> Vec<Notice> {
    if out.len() > SUMMARY_OVER {
        return vec![summary(out.len())];
    }
    out
}

/// 묶음 하나에서 띄울 알림을 고릅니다(묶기 전).
fn select(
    batch: &[Notification],
    cx: Context<'_>,
    kind_of: impl Fn(&SpaceId) -> Option<SpaceKind>,
) -> Vec<Notice> {
    if !cx.prefs.enabled {
        return Vec::new();
    }
    let mut out = Vec::new();
    for n in batch {
        match n {
            Notification::MessageNew {
                space,
                channel,
                message,
                notify,
            } => {
                let Some(me) = cx.me else {
                    continue;
                };
                if message.deleted || message.author == *me {
                    continue;
                }
                // shortcut: 보낸 쪽 시계가 늦으면 온라인이 된 직후의 실시간 글도 조용합니다(안 읽음 표시는 남음), 불만이
                // 나오면 여유를 둡니다. 여유가 크면 그만큼 짧은 오프라인을 못 막습니다.
                if message.created_at.as_millis() < cx.online_since_ms {
                    continue;
                }
                let thread = message.thread.is_some();
                // 보고 있는 채널의 본문 메시지는 화면에 바로 보이므로 띄우지 않습니다(글타래 답글은 패널이 닫혀 있을 수 있음).
                if cx.focused && cx.viewing == Some((*space, *channel)) && !thread {
                    continue;
                }
                // 예전 데몬은 `mentions_me`를 보내지 않아 글의 `<@ID>`도 봅니다.
                let mention = message.mentions_me || message.text.contains(&format!("<@{me}>"));
                let wanted = match kind_of(space) {
                    Some(SpaceKind::SelfSpace) => false,
                    Some(SpaceKind::Space) => true,
                    // 크루(또는 아직 모르는 공간). 글타래 답글은 멘션일 때만(큰 크루의 글타래가 시끄럽지 않게).
                    Some(SpaceKind::Crew) | None => {
                        match cx.rules.level(space, channel, cx.prefs.crew, cx.now_ms) {
                            CrewLevel::All => mention || message.thread.is_none(),
                            CrewLevel::Mentions => mention,
                            CrewLevel::Off => false,
                        }
                    }
                };
                if !wanted {
                    continue;
                }
                let body = if message.volatile || !cx.prefs.preview {
                    None
                } else {
                    notify
                        .body
                        .as_deref()
                        .map(|b| truncate(&crate::i18n::daemon_notice(b), BODY_MAX))
                };
                // 휘발성 채널은 보낸 사람을 알리지 않습니다(데몬도 비우지만 여기서 한 번 더 막음).
                let avatar = notify
                    .sender
                    .as_ref()
                    .filter(|_| !message.volatile)
                    .map(|s| avatar::Spec {
                        space: *space,
                        user: s.user,
                        name: s.name.clone(),
                        avatar: s.avatar,
                    });
                out.push(Notice {
                    title: truncate(&crate::i18n::daemon_notice(&notify.title), TITLE_MAX),
                    body: body.filter(|b| !b.is_empty()),
                    target: Some((*space, *channel)),
                    thread,
                    avatar,
                });
            }
            // 창 안에서도 토스트로 알리므로 창이 뒤에 있을 때만.
            Notification::FriendRequest { profile, .. } if !cx.focused => out.push(Notice {
                title: crate::i18n::text("friendRequest", "친구 요청", &[]),
                body: Some(crate::i18n::text(
                    "friendRequestBody",
                    "{name}님이 친구 요청을 보냈습니다",
                    &[("name", &truncate(&profile.name, 40))],
                )),
                target: None,
                thread: false,
                avatar: None,
            }),
            Notification::ConfirmPending(p) if !cx.focused => out.push(Notice {
                title: crate::i18n::text("confirmPending", "AI 요청 확인 필요", &[]),
                body: Some(truncate(
                    &format!("{}: {}", p.token_name, p.summary),
                    BODY_MAX,
                )),
                target: None,
                thread: false,
                avatar: None,
            }),
            _ => {}
        }
    }
    out
}

/// 알림에 붙여 둔 채널을 꺼냅니다. 모양이 틀리면 None.
pub fn target_of(extra: &HashMap<String, Value>) -> Option<Target> {
    let id = |k: &str| extra.get(k).and_then(Value::as_str);
    let space = SpaceId::from_str(id("space")?).ok()?;
    let channel = ChannelId::from_str(id("channel")?).ok()?;
    Some((space, channel))
}

/// 간격 조절 판단 결과.
#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    /// 지금 띄움.
    Show(Vec<Notice>),
    /// 나중에 띄움. `Some(d)`면 d 뒤에 `flush`를 예약해야 합니다.
    Defer(Option<Duration>),
    Nothing,
}

#[derive(Default)]
struct Throttle {
    last: Option<Instant>,
    pending: Vec<Notice>,
    pending_count: usize,
    flush_scheduled: bool,
}

/// 알림 간격 조절기와 알림 판단에 쓰는 설정·지금 보는 채널(메모리에만).
#[derive(Default)]
pub struct Notifier {
    inner: Mutex<Throttle>,
    prefs: Mutex<NotifyPrefs>,
    rules: Mutex<Arc<NotifyRules>>,
    viewing: Mutex<Option<Target>>,
    focused: AtomicBool,
    /// 다시 온라인이 된 시각(유닉스 ms).
    online_since: AtomicU64,
    /// 마지막 `net.status`에서 연결이 하나도 없었는지.
    no_peers: AtomicBool,
    /// 메인 창 HWND(0이면 아직 모름).
    #[cfg(windows)]
    hwnd: std::sync::atomic::AtomicIsize,
}

impl Notifier {
    /// 메인 창이 앞에 있는지. 윈도는 전경 창을 직접 봅니다. WebView2 포커스 이벤트는 웹뷰를 만들 때의 첫 포커스를
    /// 놓쳐, 창을 한 번 내렸다 올릴 때까지 "뒤에 있음"으로 남아 보고 있는 채널도 알림이 떴습니다.
    /// 다른 운영체제나 HWND를 모를 때는 `WindowEvent::Focused`로 갱신한 값.
    pub fn focused(&self) -> bool {
        #[cfg(windows)]
        {
            use windows_sys::Win32::UI::WindowsAndMessaging::{
                GA_ROOTOWNER, GetAncestor, GetForegroundWindow,
            };
            let hwnd = self.hwnd.load(Ordering::Relaxed);
            if hwnd != 0 {
                // 이 창이 띄운 저장 창 같은 대화상자가 앞에 있어도 이 창으로 칩니다.
                // SAFETY: 창 핸들을 읽기만 하는 조회 함수이고, 빈 핸들이면 빈 핸들을 돌려줍니다.
                let front = unsafe { GetAncestor(GetForegroundWindow(), GA_ROOTOWNER) };
                return front as isize == hwnd;
            }
        }
        self.focused.load(Ordering::Relaxed)
    }

    pub fn set_focused(&self, f: bool) {
        self.focused.store(f, Ordering::Relaxed);
    }

    /// 지금을 다시 온라인이 된 시각으로 둡니다.
    pub fn mark_online(&self) {
        self.online_since
            .store(Timestamp::now().as_millis(), Ordering::Relaxed);
    }

    #[cfg(windows)]
    pub fn set_hwnd(&self, hwnd: isize) {
        self.hwnd.store(hwnd, Ordering::Relaxed);
    }

    pub fn prefs(&self) -> NotifyPrefs {
        *self.prefs.lock()
    }

    pub fn rules(&self) -> Arc<NotifyRules> {
        self.rules.lock().clone()
    }

    /// UI 설정에서 알림 설정과 크루·채널 규칙을 읽어 둡니다.
    pub fn load(&self, m: &Map<String, Value>) {
        *self.prefs.lock() = NotifyPrefs::from_map(m);
        *self.rules.lock() = Arc::new(NotifyRules::from_map(m));
    }

    pub fn viewing(&self) -> Option<Target> {
        *self.viewing.lock()
    }

    /// 웹뷰가 연 채널. 홈·채널 없는 화면이면 None.
    pub fn set_viewing(&self, v: Option<Target>) {
        *self.viewing.lock() = v;
    }

    /// 새 알림을 넣습니다.
    pub fn submit(&self, notices: Vec<Notice>, now: Instant) -> Decision {
        if notices.is_empty() {
            return Decision::Nothing;
        }
        let mut t = self.inner.lock();
        let elapsed = t.last.map(|l| now.saturating_duration_since(l));
        let free = elapsed.is_none_or(|e| e >= GAP) && t.pending_count == 0;
        if free {
            t.last = Some(now);
            return Decision::Show(notices);
        }
        t.pending_count += notices.len();
        if t.pending.is_empty() {
            t.pending.extend(notices.into_iter().take(1));
        }
        if t.flush_scheduled {
            return Decision::Defer(None);
        }
        t.flush_scheduled = true;
        let wait = GAP.saturating_sub(elapsed.unwrap_or(GAP));
        Decision::Defer(Some(wait.max(Duration::from_millis(50))))
    }

    /// 미뤄 둔 알림을 꺼냅니다(하나면 그대로, 여러 개면 요약).
    pub fn flush(&self, now: Instant) -> Option<Notice> {
        let mut t = self.inner.lock();
        t.flush_scheduled = false;
        let count = std::mem::take(&mut t.pending_count);
        let pending = std::mem::take(&mut t.pending);
        if count == 0 {
            return None;
        }
        t.last = Some(now);
        if count == 1 {
            pending.into_iter().next()
        } else {
            Some(summary(count))
        }
    }
}

/// 띄웁니다. 보낸 사람이 있으면 프사를 그려(또는 그려 둔 것을 찾아) 붙입니다(윈도만, 다른 운영체제는 앱 아이콘).
fn show<R: Runtime>(app: &AppHandle<R>, d: &std::sync::Arc<Daemon>, n: Notice) {
    #[cfg(windows)]
    if let Some(spec) = n.avatar.clone()
        && let Some(dir) = crate::cache_dir(app)
    {
        let app = app.clone();
        let client = d.client();
        tauri::async_runtime::spawn(async move {
            let dir = dir.join(avatar::DIR);
            let path = avatar::prepare(client.as_ref(), &dir, &spec).await;
            if let Some(path) = path
                && show_windows(&app, &n, &path)
            {
                return;
            }
            show_plain(&app, &n);
        });
        return;
    }
    let _ = d;
    #[cfg(target_os = "android")]
    {
        let _ = app;
        let link = n
            .target
            .as_ref()
            .map(|(space, channel)| format!("yangpa://open/{space}/{channel}"));
        crate::android::notify(&n.title, n.body.as_deref(), link.as_deref());
    }
    #[cfg(not(target_os = "android"))]
    show_plain(app, &n);
}

/// 윈도 알림을 프사(원으로 자름)와 함께 직접 띄웁니다. 알림 플러그인은 그림을 붙일 수 없어서입니다. 실패하면 false.
#[cfg(windows)]
fn show_windows<R: Runtime>(app: &AppHandle<R>, n: &Notice, path: &std::path::Path) -> bool {
    use tauri_winrt_notification::{IconCrop, Toast};
    // 플러그인과 같은 앱 ID: 개발 빌드 폴더에서는 PowerShell의 ID(등록되지 않은 ID의 알림은 윈도가 버림).
    let dev = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.display().to_string()))
        .is_some_and(|d| d.ends_with("\\target\\debug") || d.ends_with("\\target\\release"));
    let identifier = app.config().identifier.clone();
    let app_id = if dev {
        Toast::POWERSHELL_APP_ID
    } else {
        identifier.as_str()
    };
    let tap = app.clone();
    let target = n.target;
    let toast = Toast::new(app_id)
        .title(&n.title)
        .text1(n.body.as_deref().unwrap_or(""))
        .icon(path, IconCrop::Circular, "")
        .on_activated(move |_| {
            open_target(&tap, target);
            Ok(())
        });
    match toast.show() {
        Ok(()) => true,
        Err(e) => {
            debug!("프사를 붙인 알림을 띄우지 못했습니다: {e}");
            false
        }
    }
}

#[cfg(not(target_os = "android"))]
fn show_plain<R: Runtime>(app: &AppHandle<R>, n: &Notice) {
    let mut b = app.notification().builder().title(&n.title);
    if let Some(body) = &n.body {
        b = b.body(body);
    }
    // 누르면 열 채널. 이 값은 플러그인 메모리에만 있고 OS 알림에는 제목·본문만 들어갑니다.
    if let Some((space, channel)) = &n.target {
        b = b
            .extra("space", space.to_string())
            .extra("channel", channel.to_string());
    }
    if let Err(e) = b.show() {
        debug!("OS 알림을 띄우지 못했습니다: {e}");
    }
}

/// 알림을 눌렀을 때: 창을 앞으로 가져오고, 채널이 붙어 있으면 웹뷰에 열라고 알립니다.
pub fn on_action<R: Runtime>(app: &AppHandle<R>, a: &ActionPerformed) {
    if a.action_id() != "tap" {
        return;
    }
    open_target(app, a.notification().and_then(|n| target_of(n.extra())));
}

/// 창을 앞으로 가져옵니다(알림, 트레이 아이콘, 초대 링크, 앱을 한 번 더 실행했을 때).
pub fn show_main<R: Runtime>(app: &AppHandle<R>) {
    if let Some(w) = app.get_webview_window(MAIN_WINDOW) {
        #[cfg(desktop)]
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
}

/// 창을 앞으로 가져오고, 채널이 있으면 웹뷰에 열라고 알립니다.
fn open_target<R: Runtime>(app: &AppHandle<R>, target: Option<Target>) {
    show_main(app);
    if let Some((space, channel)) = target {
        let payload = json!({ "space": space.to_string(), "channel": channel.to_string() });
        if let Err(e) = app.emit_to(MAIN_WINDOW, OPEN_CHANNEL, payload) {
            debug!("알림에서 채널을 열지 못했습니다: {e}");
        }
    }
}

/// 이번 틱까지 오프라인이었으면 그 시각. PC가 잠들었거나 나갈 길이 없으면 지금까지이고, 길은 있는데 연결만 없으면
/// 친구가 아무도 안 켠 것일 수도 있어 [`DIAL_GRACE_MS`]만큼 앞까지로 봅니다. 폰은 나갈 길이 없을 때만 칩니다([`PHONE`]).
fn offline_until(
    now: u64,
    last_tick: u64,
    no_peers: bool,
    phone: bool,
    has_route: impl FnOnce() -> bool,
) -> Option<u64> {
    if !phone && now.saturating_sub(last_tick) > SLEPT_MS {
        return Some(now);
    }
    if !no_peers {
        return None;
    }
    if !has_route() {
        return Some(now);
    }
    (!phone).then(|| now.saturating_sub(DIAL_GRACE_MS))
}

/// 밖으로 나갈 길이 있는지. UDP `connect`는 라우팅 표만 보고 아무것도 보내지 않습니다. 직접 묶지 않는 까닭은 윈도
/// 방화벽 창이 뜰 수 있어서입니다(그래서 표준 `UdpSocket`이 아닌 `socket2`).
// shortcut: 윈도 Teredo의 IPv6 기본 경로는 망이 끊겨도 남을 수 있어 IPv4만 봅니다. IPv6 전용 망은 연결이 없을 때
// `DIAL_GRACE_MS` 여유가 없어지므로, 그런 사용자가 나오면 IPv6도 보되 Teredo를 뺍니다.
fn has_route() -> bool {
    Socket::new(Domain::IPV4, Type::DGRAM, None)
        .and_then(|s| s.connect(&SocketAddr::from(([192, 0, 2, 1], 9)).into()))
        .is_ok()
}

/// 데몬에 다시 붙지 않는 오프라인(PC 잠듦, 망 끊김)을 살펴 다시 온라인이 된 시각을 끌어올립니다.
pub async fn watch_offline(d: Arc<Daemon>) {
    let mut last = Timestamp::now().as_millis();
    loop {
        tokio::time::sleep(OFFLINE_TICK).await;
        let now = Timestamp::now().as_millis();
        let no_peers = d.notifier.no_peers.load(Ordering::Relaxed);
        if let Some(until) = offline_until(now, last, no_peers, PHONE, has_route) {
            d.notifier.online_since.fetch_max(until, Ordering::Relaxed);
        }
        last = now;
    }
}

/// 이벤트 묶음 하나를 보고 필요하면 OS 알림을 띄웁니다.
pub fn handle_batch<R: Runtime>(
    app: &AppHandle<R>,
    d: &std::sync::Arc<Daemon>,
    batch: &[Notification],
) {
    for n in batch {
        if let Notification::NetStatus(s) = n {
            d.notifier.no_peers.store(s.peers == 0, Ordering::Relaxed);
        }
    }
    let me = d.me();
    let prefs = d.notifier.prefs();
    let rules = d.notifier.rules();
    let focused = d.notifier.focused();
    let cx = Context {
        me: me.as_ref(),
        prefs: &prefs,
        rules: &rules,
        now_ms: Timestamp::now().as_millis(),
        online_since_ms: d.notifier.online_since.load(Ordering::Relaxed),
        focused,
        viewing: d.notifier.viewing(),
    };
    let notices = select(batch, cx, |s| d.space_kind(s));
    // 창이 뒤에 있으면 작업 표시줄을 깜빡입니다(윈도가 알림을 조용히 버려도 보이게). 묶기 전에 봅니다(묶은 알림엔
    // 채널이 없음).
    #[cfg(desktop)]
    if !focused
        && notices.iter().any(|n| n.target.is_some())
        && let Some(w) = app.get_webview_window(MAIN_WINDOW)
    {
        let _ = w.request_user_attention(Some(UserAttentionType::Informational));
    }
    let notices = summarize(notices);
    match d.notifier.submit(notices, Instant::now()) {
        Decision::Show(list) => {
            for n in list {
                show(app, d, n);
            }
        }
        Decision::Defer(Some(wait)) => {
            let app = app.clone();
            let d = d.clone();
            tauri::async_runtime::spawn(async move {
                tokio::time::sleep(wait).await;
                let Some(n) = d.notifier.flush(Instant::now()) else {
                    return;
                };
                // 기다리는 사이 그 채널을 열었으면 띄우지 않습니다(글타래 답글은 `plan`처럼 띄움).
                if on_screen(&n, d.notifier.focused(), d.notifier.viewing()) {
                    return;
                }
                show(&app, &d, n);
            });
        }
        Decision::Defer(None) | Decision::Nothing => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use yangpa_core::ids::{DeviceId, EventId};

    fn uid(b: u8) -> UserId {
        UserId::from_bytes([b; 32])
    }

    fn space(b: u8) -> SpaceId {
        SpaceId::from_bytes([b; 16])
    }

    fn chan(b: u8) -> ChannelId {
        ChannelId::from_bytes([b; 16])
    }

    fn msg_in(
        space_b: u8,
        chan_b: u8,
        author: u8,
        text: &str,
        volatile: bool,
        thread: bool,
    ) -> Notification {
        serde_json::from_value(json!({
            "type": "message.new",
            "space": space(space_b).to_string(),
            "channel": chan(chan_b).to_string(),
            "message": {
                "id": EventId::from_bytes([author; 32]).to_string(),
                "author": uid(author).to_string(),
                "author_device": DeviceId::from_bytes([author; 32]).to_string(),
                "created_at": 1,
                "lamport": 1,
                "text": text,
                "volatile": volatile,
                "thread": thread.then(|| EventId::from_bytes([42; 32]).to_string()),
            },
            // 휘발성에도 보낸 사람을 실어 보내 앱이 한 번 더 막는지 봅니다(데몬은 비워 보냄).
            "notify": if volatile {
                json!({"title": "새 메시지", "sender": {"user": uid(author).to_string(), "name": "민지"}})
            } else {
                json!({"title": "민지", "body": text, "sender": {"user": uid(author).to_string(), "name": "민지"}})
            },
        }))
        .unwrap()
    }

    fn msg(space_b: u8, author: u8, text: &str, volatile: bool) -> Notification {
        msg_in(space_b, 9, author, text, volatile, false)
    }

    fn friend_request() -> Notification {
        serde_json::from_value(json!({
            "type": "friend.request",
            "request_id": "r1",
            "user": uid(5).to_string(),
            "profile": { "version": 1, "name": "민지" },
        }))
        .unwrap()
    }

    fn kinds(s: &SpaceId) -> Option<SpaceKind> {
        if *s == space(1) {
            Some(SpaceKind::Space)
        } else if *s == space(2) {
            Some(SpaceKind::Crew)
        } else if *s == space(3) {
            Some(SpaceKind::SelfSpace)
        } else {
            None
        }
    }

    const ME: u8 = 7;
    const ON: NotifyPrefs = NotifyPrefs {
        enabled: true,
        crew: CrewLevel::All,
        preview: true,
    };

    static NO_RULES: std::sync::LazyLock<NotifyRules> =
        std::sync::LazyLock::new(NotifyRules::default);
    const NOW: u64 = 1_000_000;

    /// 창이 뒤에 있고 기본 설정일 때.
    fn bg<'a>(me: Option<&'a UserId>, prefs: &'a NotifyPrefs) -> Context<'a> {
        Context {
            me,
            prefs,
            rules: &NO_RULES,
            now_ms: NOW,
            online_since_ms: 0,
            focused: false,
            viewing: None,
        }
    }

    #[test]
    fn dm_notifies_with_body_and_target_and_skips_own() {
        let me = uid(ME);
        let out = plan(
            &[msg(1, 5, "안녕", false), msg(1, ME, "내 글", false)],
            bg(Some(&me), &ON),
            kinds,
        );
        assert_eq!(
            out,
            vec![Notice {
                title: "민지".into(),
                body: Some("안녕".into()),
                target: Some((space(1), chan(9))),
                thread: false,
                avatar: Some(avatar::Spec {
                    space: space(1),
                    user: uid(5),
                    name: "민지".into(),
                    avatar: None,
                }),
            }]
        );
    }

    #[test]
    fn crew_levels_and_self_space_never() {
        let me = uid(ME);
        let mention = format!("<@{me}> 이거 봐");
        let batch = [
            msg(2, 5, "그냥 잡담", false),
            msg(2, 5, &mention, false),
            msg_in(2, 9, 5, "글타래 답글", false, true),
            msg_in(2, 9, 5, &mention, false, true),
            msg(3, 5, "메모", false),
        ];
        let at = |crew| {
            let prefs = NotifyPrefs { crew, ..ON };
            plan(&batch, bg(Some(&me), &prefs), kinds)
                .into_iter()
                .map(|n| n.body.unwrap_or_default())
                .collect::<Vec<_>>()
        };
        // 모든 메시지: 본문 메시지는 다, 글타래 답글은 멘션일 때만.
        assert_eq!(
            at(CrewLevel::All),
            vec!["그냥 잡담", mention.as_str(), mention.as_str()]
        );
        assert_eq!(
            at(CrewLevel::Mentions),
            vec![mention.as_str(), mention.as_str()]
        );
        assert!(at(CrewLevel::Off).is_empty());
        // 내 ID를 모르면(온보딩 중) 내 글과 구분할 수 없어 어떤 메시지도 띄우지 않습니다.
        let batch = [msg(1, 5, "안녕", false), msg(2, 5, &mention, false)];
        assert!(plan(&batch, bg(None, &ON), kinds).is_empty());
        let mentions = NotifyPrefs {
            crew: CrewLevel::Mentions,
            ..ON
        };
        // 모르는 공간은 크루처럼 다룹니다.
        assert!(plan(&[msg(4, 5, "x", false)], bg(Some(&me), &mentions), kinds).is_empty());
        assert_eq!(
            plan(&[msg(4, 5, "x", false)], bg(Some(&me), &ON), kinds).len(),
            1
        );
    }

    #[test]
    fn crew_and_channel_rules_and_mute() {
        let me = uid(ME);
        let mention = format!("<@{me}> 봐");
        let mut everyone = msg_in(2, 9, 5, "@everyone 공지", false, false);
        // 데몬이 권한을 보고 나를 멘션했다고 표시한 글(@everyone·@here).
        if let Notification::MessageNew { message, .. } = &mut everyone {
            message.mentions_me = true;
        }
        let batch = [
            msg_in(2, 9, 5, "잡담", false, false),
            msg_in(2, 9, 5, &mention, false, false),
            everyone,
            msg_in(2, 8, 5, "저쪽", false, false),
        ];
        let rules = |list: Vec<(String, Value)>| {
            let m: Map<String, Value> = list.into_iter().collect();
            NotifyRules::from_map(json!({ "notifyRules": m }).as_object().unwrap())
        };
        let run = |r: &NotifyRules, now_ms: u64| {
            let cx = Context {
                rules: r,
                now_ms,
                ..bg(Some(&me), &ON)
            };
            select(&batch, cx, kinds)
                .into_iter()
                .map(|n| n.body.unwrap_or_default())
                .collect::<Vec<_>>()
        };
        let crew = space(2).to_string();
        let ch = |c: u8| format!("{crew}/{}", chan(c));
        // 크루는 멘션만, 채널 8만 모든 메시지: 채널 규칙이 앞섭니다.
        let r = rules(vec![
            (crew.clone(), json!({"level": "mentions"})),
            (ch(8), json!({"level": "all"})),
        ]);
        assert_eq!(
            run(&r, NOW),
            vec![mention.as_str(), "@everyone 공지", "저쪽"]
        );
        // 크루를 끈 동안은 멘션만, 시각이 지나면 다시 모두.
        let r = rules(vec![(crew.clone(), json!({"muteUntil": NOW + 1}))]);
        assert_eq!(run(&r, NOW), vec![mention.as_str(), "@everyone 공지"]);
        assert_eq!(run(&r, NOW + 1).len(), 4);
        // 채널 하나만 끄고(직접 켤 때까지), 다른 채널은 그대로.
        let r = rules(vec![(
            ch(9),
            json!({"muteUntil": 9_007_199_254_740_991u64}),
        )]);
        assert_eq!(
            run(&r, NOW),
            vec![mention.as_str(), "@everyone 공지", "저쪽"]
        );
        // 수준 '끄기'는 멘션까지 막습니다.
        let r = rules(vec![(ch(9), json!({"level": "off"}))]);
        assert_eq!(run(&r, NOW), vec!["저쪽"]);
        // 1대1 공간에는 크루 규칙을 쓰지 않습니다.
        let dm = rules(vec![(space(1).to_string(), json!({"level": "off"}))]);
        let cx = Context {
            rules: &dm,
            ..bg(Some(&me), &ON)
        };
        assert_eq!(plan(&[msg(1, 5, "안녕", false)], cx, kinds).len(), 1);
        // 모양이 틀린 규칙은 건너뜁니다.
        let bad = rules(vec![
            (crew.clone(), json!({"level": "loud", "muteUntil": "곧"})),
            (ch(8), json!(3)),
        ]);
        assert_eq!(bad.0.len(), 1);
        assert_eq!(run(&bad, NOW).len(), 4);
    }

    #[test]
    fn messages_written_while_offline_stay_silent() {
        let me = uid(ME);
        let at = |ms: u64, text: &str| {
            let mut v = serde_json::to_value(msg(1, 5, text, false)).unwrap();
            v["message"]["created_at"] = json!(ms);
            serde_json::from_value::<Notification>(v).unwrap()
        };
        let cx = Context {
            online_since_ms: NOW,
            ..bg(Some(&me), &ON)
        };
        // 쌓인 글은 세지 않으므로 "새 알림 N개"로 묶이지도 않습니다.
        let batch = [
            at(NOW - 4, "가"),
            at(NOW - 3, "나"),
            at(NOW - 2, "다"),
            at(NOW - 1, "라"),
            at(NOW, "지금"),
        ];
        let out = plan(&batch, cx, kinds);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].body.as_deref(), Some("지금"));
        // 멘션도 조용합니다.
        let mention = at(NOW - 1, &format!("<@{me}> 봐"));
        assert!(plan(&[mention], cx, kinds).is_empty());
        // 친구 요청은 채팅이 아니라 그대로 알립니다.
        assert_eq!(plan(&[friend_request()], cx, kinds).len(), 1);
    }

    #[test]
    fn offline_until_tells_a_dropped_network_from_nobody_online() {
        let tick = OFFLINE_TICK.as_millis() as u64;
        let last = NOW - tick;
        // 연결이 있으면 온라인입니다(길은 볼 것도 없음).
        assert_eq!(offline_until(NOW, last, false, false, || panic!()), None);
        // 나갈 길이 없으면 지금까지 오프라인입니다.
        assert_eq!(offline_until(NOW, last, true, false, || false), Some(NOW));
        // 길이 있으면 다시 거는 중인 친구의 글이 조용해지지 않게 여유를 둡니다.
        let grace = Some(NOW - DIAL_GRACE_MS);
        assert_eq!(offline_until(NOW, last, true, false, || true), grace);
        // 틱이 크게 밀렸으면 PC가 잠들었던 것입니다. 깬 직후라 연결이 남아 있어도 같습니다.
        let slept = NOW - SLEPT_MS - 1;
        assert_eq!(
            offline_until(NOW, slept, false, false, || panic!()),
            Some(NOW)
        );
        // 폰의 CPU가 자는 사이 밀린 틱이나 끊긴 연결은 오프라인이 아닙니다. 그때 따라잡은 글을 알려야 합니다.
        assert_eq!(offline_until(NOW, slept, false, true, || panic!()), None);
        assert_eq!(offline_until(NOW, slept, true, true, || true), None);
        // 폰도 나갈 길이 없으면 오프라인입니다.
        assert_eq!(offline_until(NOW, last, true, true, || false), Some(NOW));
    }

    #[test]
    fn disabled_shows_nothing() {
        let me = uid(ME);
        let off = NotifyPrefs {
            enabled: false,
            ..ON
        };
        let batch = [msg(1, 5, "안녕", false), friend_request()];
        assert!(plan(&batch, bg(Some(&me), &off), kinds).is_empty());
    }

    #[test]
    fn focused_skips_only_the_open_channel() {
        let me = uid(ME);
        let open = Context {
            focused: true,
            viewing: Some((space(1), chan(9))),
            ..bg(Some(&me), &ON)
        };
        let here = msg_in(1, 9, 5, "여기", false, false);
        let reply_here = msg_in(1, 9, 5, "답글", false, true);
        let there = msg_in(1, 8, 5, "저기", false, false);
        let out = plan(&[here.clone(), reply_here, there], open, kinds);
        let bodies: Vec<_> = out.iter().filter_map(|n| n.body.as_deref()).collect();
        assert_eq!(bodies, vec!["답글", "저기"]);
        // 창이 뒤에 있으면 보던 채널도 띄웁니다.
        let behind = Context {
            focused: false,
            ..open
        };
        assert_eq!(plan(std::slice::from_ref(&here), behind, kinds).len(), 1);
        // 포커스는 있지만 채널을 안 보고 있으면(홈) 띄웁니다.
        let home = Context {
            viewing: None,
            ..open
        };
        assert_eq!(plan(&[here], home, kinds).len(), 1);
    }

    #[test]
    fn friend_request_only_when_window_is_behind() {
        let me = uid(ME);
        let out = plan(&[friend_request()], bg(Some(&me), &ON), kinds);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].target, None);
        let front = Context {
            focused: true,
            ..bg(Some(&me), &ON)
        };
        assert!(plan(&[friend_request()], front, kinds).is_empty());
    }

    #[test]
    fn preview_off_drops_message_bodies() {
        let me = uid(ME);
        let quiet = NotifyPrefs {
            preview: false,
            ..ON
        };
        let out = plan(&[msg(1, 5, "안녕", false)], bg(Some(&me), &quiet), kinds);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].title, "민지");
        assert_eq!(out[0].body, None);
    }

    #[test]
    fn volatile_never_has_body() {
        let me = uid(ME);
        let mut n = msg(1, 5, "비밀 내용", true);
        // 데몬이 실수로 본문을 넣어도 막습니다.
        if let Notification::MessageNew { notify, .. } = &mut n {
            notify.body = Some("비밀 내용".into());
        }
        let out = plan(&[n], bg(Some(&me), &ON), kinds);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].body, None);
        assert_eq!(out[0].avatar, None, "휘발성은 보낸 사람 프사도 없음");
    }

    #[test]
    fn many_become_summary() {
        let me = uid(ME);
        let batch: Vec<_> = (0..5).map(|i| msg(1, 10 + i, "x", false)).collect();
        let out = plan(&batch, bg(Some(&me), &ON), kinds);
        assert_eq!(out, vec![summary(5)]);
        assert_eq!(out[0].target, None);
    }

    #[test]
    fn prefs_from_map() {
        let parse = |v: Value| NotifyPrefs::from_map(v.as_object().unwrap());
        assert_eq!(parse(json!({})), NotifyPrefs::default());
        assert_eq!(NotifyPrefs::default(), ON);
        assert_eq!(
            parse(json!({"notify": false, "notifyCrew": "mentions", "notifyPreview": false})),
            NotifyPrefs {
                enabled: false,
                crew: CrewLevel::Mentions,
                preview: false
            }
        );
        assert_eq!(parse(json!({"notifyCrew": "off"})).crew, CrewLevel::Off);
        assert_eq!(
            parse(json!({"notify": "yes", "notifyCrew": "loud", "notifyPreview": 1})),
            NotifyPrefs::default()
        );
    }

    #[test]
    fn target_round_trips_through_extra() {
        let mut extra = HashMap::new();
        assert_eq!(target_of(&extra), None);
        extra.insert("space".to_owned(), json!(space(1).to_string()));
        extra.insert("channel".to_owned(), json!(chan(9).to_string()));
        assert_eq!(target_of(&extra), Some((space(1), chan(9))));
        extra.insert("channel".to_owned(), json!("쓰레기"));
        assert_eq!(target_of(&extra), None);
        extra.insert("channel".to_owned(), json!(7));
        assert_eq!(target_of(&extra), None);
    }

    #[test]
    fn truncate_counts_chars() {
        assert_eq!(truncate("가나다라마", 5), "가나다라마");
        assert_eq!(truncate("가나다라마바", 5), "가나다라…");
        assert_eq!(truncate("  공백  ", 10), "공백");
    }

    #[test]
    fn select_keeps_targets_that_summary_drops_and_threads_stay_on_screen() {
        let me = uid(ME);
        let batch: Vec<_> = (0..5).map(|i| msg(1, 5, &format!("m{i}"), false)).collect();
        // 깜빡임은 묶기 전 목록으로 정합니다(묶은 알림엔 채널이 없음).
        let picked = select(&batch, bg(Some(&me), &ON), kinds);
        assert!(picked.iter().all(|n| n.target.is_some()));
        assert_eq!(summarize(picked), vec![summary(5)]);
        let at = Some((space(1), chan(9)));
        let body = Notice {
            title: "a".into(),
            body: None,
            target: at,
            thread: false,
            avatar: None,
        };
        let reply = Notice {
            thread: true,
            ..body.clone()
        };
        assert!(on_screen(&body, true, at));
        assert!(!on_screen(&reply, true, at));
        assert!(!on_screen(&body, false, at));
        assert!(!on_screen(&summary(2), true, None));
    }

    #[test]
    fn throttle_defers_and_summarizes() {
        let n = Notifier::default();
        let t0 = Instant::now();
        let one = |s: &str| Notice {
            title: s.into(),
            body: None,
            target: Some((space(1), chan(9))),
            thread: false,
            avatar: None,
        };
        assert_eq!(n.submit(vec![one("a")], t0), Decision::Show(vec![one("a")]));
        // 간격 안: 미루고 한 번만 예약.
        let d = n.submit(vec![one("b")], t0 + Duration::from_millis(100));
        assert!(matches!(d, Decision::Defer(Some(w)) if w <= GAP));
        assert_eq!(
            n.submit(vec![one("c")], t0 + Duration::from_millis(200)),
            Decision::Defer(None)
        );
        assert_eq!(n.flush(t0 + GAP), Some(summary(2)));
        // 하나만 밀렸으면 그대로(누르면 열 채널도).
        let t1 = t0 + GAP + Duration::from_millis(10);
        assert!(matches!(
            n.submit(vec![one("d")], t1),
            Decision::Defer(Some(_))
        ));
        assert_eq!(n.flush(t1 + GAP), Some(one("d")));
        assert_eq!(n.flush(t1 + GAP), None);
        // 간격이 지나면 바로.
        let t2 = t1 + GAP * 3;
        assert_eq!(n.submit(vec![one("e")], t2), Decision::Show(vec![one("e")]));
        assert_eq!(n.submit(Vec::new(), t2), Decision::Nothing);
    }

    #[test]
    fn notifier_keeps_prefs_and_viewing() {
        let n = Notifier::default();
        assert_eq!(n.prefs(), NotifyPrefs::default());
        assert_eq!(n.viewing(), None);
        n.load(
            json!({"notifyCrew": "off", "notifyRules": {"x": {"level": "all"}}})
                .as_object()
                .unwrap(),
        );
        n.set_viewing(Some((space(2), chan(1))));
        assert_eq!(n.prefs().crew, CrewLevel::Off);
        assert_eq!(n.rules().0["x"].level, Some(CrewLevel::All));
        assert_eq!(n.viewing(), Some((space(2), chan(1))));
        n.set_viewing(None);
        assert_eq!(n.viewing(), None);
        assert!(!n.focused());
        n.set_focused(true);
        assert!(n.focused());
        // HWND를 알면 이벤트 값 대신 전경 창을 봅니다(없는 핸들은 전경일 수 없음).
        #[cfg(windows)]
        {
            n.set_hwnd(1);
            assert!(!n.focused());
        }
    }
}
