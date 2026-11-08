//! 양파 메신저(가칭) 데스크톱 앱의 Tauri 쪽.
//!
//! - 데몬(`yangpad`)에 붙거나 없으면 띄우고, 소유자 토큰으로 인증합니다([`daemon`]).
//! - 웹뷰 명령: `rpc_call`, `daemon_status`, `daemon_retry`, `set_content_protection`, `set_capture_watch`
//!   (휘발성 채널에서 캡처·녹화 감지, [`capture`]), `open_external`,
//!   `ui_prefs_get`, `ui_prefs_set`, `notify_set_viewing`(지금 연 채널, [`notify`]),
//!   `local_preview_open`·`local_preview_close`(보내기 전 동영상 미리보기, [`preview`]),
//!   `neural_*`(통화의 신경망 음성 코덱 Mimi·DAC, 모델 폴더의 ONNX 모델, [`neural`]),
//!   `screen_*`(화면공유 인코더: exe 옆 ffmpeg 폴더의 ffmpeg로 캡처·인코딩, [`screen`]),
//!   `secret_copy`(비밀값 복사, [`secret`]),
//!   `deep_link_take`(웹 초대 페이지가 연 `yangpa://` 링크, [`deeplink`]).
//!   알림 묶음은 `yangpa-events`, 연결 상태는 `yangpa-daemon`, 캡처 감지는 `yangpa-capture`,
//!   OS 알림을 누른 채널은 `yangpa-open-channel`, 받은 링크는 `yangpa-open-link` 이벤트로 보냅니다.
//! - 앱은 하나만 뜹니다. 두 번째 실행은 받은 링크만 넘기고 끝납니다(`YANGPA_DATA_DIR`로 프로필을 나눈 실행은 예외).
//! - 블롭은 `yangpa-blob` 프로토콜로 메모리에서 바로(no-store) 넘깁니다([`blob`]).
//! - 창은 incognito(디스크 캐시·저장소 없음)로 만들고, 앱 밖으로의 이동과 새 창을 막습니다.
//! - 트레이 아이콘(열기·끝내기, 리눅스 제외). UI 설정 `closeToTray`(기본 켬)면 창을 닫아도 트레이로 내리고 앱은 계속 돕니다.
//! - 윈도는 UI 설정 `autoStart`(기본 켬)대로 로그인하면 앱을 켭니다(현재 사용자 `Run` 키, `platform::set_auto_start`).
//!   `autoStartToTray`(기본 끔)면 그 실행(`--autostart`)은 창 없이 트레이로만 켭니다.
//! - Android는 데몬을 이 프로세스 안에서 돌리고([`embedded`]), 트레이 대신 포그라운드 서비스로 살아 있습니다([`android`]).

#[cfg(target_os = "android")]
pub mod android;
pub mod avatar;
pub mod blob;
pub mod capture;
pub mod daemon;
pub mod datadir;
pub mod deeplink;
#[cfg(target_os = "android")]
mod embedded;
pub mod error;
pub mod i18n;
pub mod idle;
pub mod neural;
pub mod notify;
pub mod pinned;
pub mod platform;
pub mod prefs;
pub mod preview;
pub mod screen;
pub mod secret;
#[cfg(test)]
pub(crate) mod testutil;
pub mod update;

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::{Map, Value};
use tauri::webview::{DownloadEvent, NewWindowResponse, PermissionKind, PermissionResponse};
use tauri::{AppHandle, Emitter, Manager, RunEvent, State, WebviewWindow, WebviewWindowBuilder};
use tauri_plugin_deep_link::DeepLinkExt;
use tauri_plugin_notification::NotificationExt;
use tracing::{error, warn};
use yangpa_core::ids::{ChannelId, SpaceId};

use crate::blob::BlobServer;
use crate::daemon::{Daemon, DaemonState, MAIN_WINDOW};
use crate::error::CmdError;

/// 웹뷰가 받으려다 취소한 다운로드의 주소와 웹뷰가 정한 저장 경로(Windows만)를 프런트엔드에 알리는 이벤트(`watchSaveRequests`).
const SAVE_REQUEST_EVENT: &str = "yangpa-save-request";

#[cfg(desktop)]
const TRAY_ID: &str = "main";
/// 메인 프레임이 앱 화면인지(Android: 권한 콜백이 `url()`을 부를 수 없어 페이지가 열릴 때 적어 둡니다).
#[cfg(target_os = "android")]
static ON_APP_PAGE: AtomicBool = AtomicBool::new(false);
/// 창을 닫으면 트레이로 내릴지(UI 설정 `closeToTray`).
static CLOSE_TO_TRAY: AtomicBool = AtomicBool::new(true);

/// 화면 언어가 정해지면 글을 바꿀 트레이 메뉴 항목([`shell_texts`]).
#[cfg(all(desktop, not(target_os = "linux")))]
struct TrayItems {
    open: tauri::menu::MenuItem<tauri::Wry>,
    quit: tauri::menu::MenuItem<tauri::Wry>,
}

/// UI 설정 중 Tauri 쪽이 읽는 것(알림·트레이·윈도 시작 시 켜기)을 반영합니다.
fn apply_prefs(app: &AppHandle, daemon: &Daemon, m: &Map<String, Value>) {
    daemon.notifier.load(m);
    let on = m
        .get("closeToTray")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    CLOSE_TO_TRAY.store(on, Ordering::Relaxed);
    #[cfg(windows)]
    platform::set_auto_start(m.get("autoStart").and_then(Value::as_bool).unwrap_or(true));
    #[cfg(target_os = "android")]
    android::keep_alive(app, on);
    #[cfg(not(target_os = "android"))]
    let _ = app;
}

/// 데몬 API 호출. 인증·구독 메서드는 막습니다.
#[tauri::command]
async fn rpc_call(
    daemon: State<'_, Arc<Daemon>>,
    method: String,
    params: Option<Value>,
) -> Result<Value, CmdError> {
    daemon.call(&method, params).await
}

#[tauri::command]
fn daemon_status(daemon: State<'_, Arc<Daemon>>) -> DaemonState {
    daemon.state()
}

/// 다시 붙기 대기를 건너뜁니다(오류 배너의 "다시 시도").
#[tauri::command]
fn daemon_retry(daemon: State<'_, Arc<Daemon>>) {
    daemon.wake();
}

/// 창 캡처 제외. 지원하는 OS면 true.
#[tauri::command]
fn set_content_protection(window: WebviewWindow, enabled: bool) -> Result<bool, CmdError> {
    if !platform::CONTENT_PROTECTION_SUPPORTED {
        return Ok(false);
    }
    #[cfg(target_os = "android")]
    android::set_secure(window.app_handle(), enabled)?;
    #[cfg(not(target_os = "android"))]
    window
        .set_content_protected(enabled)
        .map_err(|e| CmdError::internal(format!("캡처 제외를 바꾸지 못했습니다: {e}")))?;
    Ok(true)
}

/// 휘발성 채널을 보는 동안 캡처·녹화 감지를 켜고 끕니다. 지원하는 OS면 true.
#[tauri::command]
fn set_capture_watch(
    app: AppHandle,
    watch: State<'_, capture::CaptureWatch>,
    enabled: bool,
) -> bool {
    watch.set(&app, enabled)
}

#[tauri::command]
fn open_external(app: AppHandle, url: String) -> Result<(), CmdError> {
    #[cfg(target_os = "android")]
    return android::open_url(&app, &url);
    #[cfg(not(target_os = "android"))]
    {
        let _ = app;
        platform::open_in_browser(&url)
    }
}

/// UI 설정 폴더: 윈도 설치판·포터블이면 데이터 폴더 안(`<데이터>/app`), 아니면 OS 앱 설정 경로.
fn prefs_dir(app: &AppHandle) -> Result<std::path::PathBuf, CmdError> {
    if let Some(d) = datadir::app_dir() {
        return Ok(d);
    }
    app.path()
        .app_config_dir()
        .map_err(|e| CmdError::internal(format!("설정 경로를 찾지 못했습니다: {e}")))
}

/// 알림 프사 등 캐시 폴더: 윈도 설치판·포터블이면 `<데이터>/app/cache`, 아니면 OS 앱 캐시 경로.
pub(crate) fn cache_dir<R: tauri::Runtime>(app: &AppHandle<R>) -> Option<std::path::PathBuf> {
    datadir::app_dir()
        .map(|d| d.join("cache"))
        .or_else(|| app.path().app_cache_dir().ok())
}

#[tauri::command]
fn ui_prefs_get(app: AppHandle) -> Result<Map<String, Value>, CmdError> {
    Ok(prefs::load(&prefs_dir(&app)?))
}

/// 웹뷰가 화면 언어로 번역한 셸 문구([`i18n`]). 트레이 메뉴 글도 바로 바꿉니다.
#[tauri::command]
fn shell_texts(app: AppHandle, texts: HashMap<String, String>) -> Result<(), CmdError> {
    i18n::set(texts)?;
    #[cfg(target_os = "android")]
    android::keep_alive(&app, CLOSE_TO_TRAY.load(Ordering::Relaxed));
    #[cfg(all(desktop, not(target_os = "linux")))]
    if let Some(items) = app.try_state::<TrayItems>() {
        let _ = items.open.set_text(i18n::text("open", "열기", &[]));
        let _ = items.quit.set_text(i18n::text("quit", "끝내기", &[]));
    }
    #[cfg(target_os = "linux")]
    let _ = app;
    Ok(())
}

#[tauri::command]
fn ui_prefs_set(
    app: AppHandle,
    daemon: State<'_, Arc<Daemon>>,
    prefs: Value,
) -> Result<(), CmdError> {
    prefs::save(&prefs_dir(&app)?, &prefs)?;
    if let Value::Object(m) = &prefs {
        apply_prefs(&app, &daemon, m);
    }
    Ok(())
}

/// 웹뷰가 지금 연 채널(그 채널의 새 메시지는 창이 앞에 있는 동안 OS 알림을 띄우지 않음). 홈이면 둘 다 비웁니다.
/// 메모리에만 둡니다.
#[tauri::command]
fn notify_set_viewing(
    daemon: State<'_, Arc<Daemon>>,
    space: Option<String>,
    channel: Option<String>,
) -> Result<(), CmdError> {
    let bad = |_| CmdError::invalid_params("공간·채널 ID가 잘못됐습니다");
    let viewing = match (space, channel) {
        (Some(s), Some(c)) => Some((
            SpaceId::from_str(&s).map_err(bad)?,
            ChannelId::from_str(&c).map_err(bad)?,
        )),
        _ => None,
    };
    daemon.notifier.set_viewing(viewing);
    Ok(())
}

/// WebDriver 창 시험(`webdriver` 기능으로 빌드하고 tauri-driver가 띄운 경우)에서만 incognito를 끕니다.
/// WebKitGTK는 incognito 웹뷰를 자동화하지 못하기 때문입니다(wry가 자동화를 허용한 컨텍스트 대신 새 임시
/// 컨텍스트를 씀). 웹뷰 저장소는 시험용 데이터 경로(`YANGPA_DATA_DIR/webview`) 안에 두고, 그 값이 없으면 그대로
/// 둡니다. 배포 빌드에는 이 기능이 없어 늘 incognito입니다.
fn webdriver_window(config: &mut tauri::utils::config::WindowConfig) {
    if !cfg!(feature = "webdriver")
        || std::env::var("TAURI_WEBVIEW_AUTOMATION").as_deref() != Ok("true")
    {
        return;
    }
    match std::env::var_os("YANGPA_DATA_DIR").filter(|d| !d.is_empty()) {
        Some(dir) => {
            warn!("WebDriver 시험: incognito를 끄고 웹뷰 저장소를 시험 데이터 경로에 둡니다");
            config.incognito = false;
            config.data_directory = Some(std::path::PathBuf::from(dir).join("webview"));
        }
        None => warn!(
            "WebDriver 시험: YANGPA_DATA_DIR가 없어 incognito를 그대로 둡니다(창을 자동화할 수 없음)"
        ),
    }
}

/// 메인 창을 설정 파일(`app.windows[label=main]`)대로 만들되, 이동·새 창·권한 처리기를 붙입니다.
/// `hidden`이면 보이지 않게 만듭니다(트레이로만 켜기).
fn create_main_window(app: &tauri::App, hidden: bool) -> tauri::Result<WebviewWindow> {
    let mut config = app
        .config()
        .app
        .windows
        .iter()
        .find(|w| w.label == MAIN_WINDOW)
        .cloned()
        .unwrap_or_else(|| tauri::utils::config::WindowConfig {
            label: MAIN_WINDOW.into(),
            incognito: true,
            ..Default::default()
        });
    config.visible &= !hidden;
    // 윈도 설치판·포터블이면 웹뷰 저장소도 데이터 폴더 안에(incognito라도 WebView2가 폴더를 만듦).
    if let Some(dir) = datadir::webview_dir() {
        config.data_directory = Some(dir);
    }
    webdriver_window(&mut config);
    let window = WebviewWindowBuilder::from_config(app.handle(), &config)?
        .on_navigation(|url| {
            let ok = platform::navigation_allowed(url);
            if !ok {
                warn!("앱 밖으로의 이동을 막았습니다({})", url.scheme());
            }
            ok
        })
        // 링크는 프런트엔드가 open_external로 엽니다. 웹뷰 새 창은 모두 막습니다.
        .on_new_window(|_, _| NewWindowResponse::Deny)
        // 웹뷰 자체 다운로드(우클릭 "다른 이름으로 저장", 플레이어 다운로드)는 웹뷰가 쓰지 않습니다. 휘발성 첨부를 경고 없이
        // 디스크에 쓰고, 링크 저장은 Tor를 거치지 않고 그 서버에 직접 접속하기 때문입니다. 대신 주소를 프런트엔드에 알려, 그린
        // 첨부면 앱의 저장(휘발성은 경고 뒤 데몬 blob.save)으로 잇습니다. Windows 웹뷰는 이미 저장 창을 띄웠거나("다른 이름으로
        // 저장") 받기 폴더의 겹치지 않는 이름(플레이어 다운로드)을 정해 두었으므로 그 경로를 넘겨 저장 창이 두 번 뜨지 않게 합니다.
        // 다른 OS는 경로 없이 넘겨 앱이 저장 창을 띄웁니다.
        .on_download(|webview, event| {
            if let DownloadEvent::Requested { url, destination } = event {
                let path = (cfg!(windows) && destination.is_absolute())
                    .then(|| destination.to_string_lossy().into_owned());
                let req = serde_json::json!({ "url": url.as_str(), "path": path });
                if let Err(e) = webview.emit_to(MAIN_WINDOW, SAVE_REQUEST_EVENT, req) {
                    warn!("저장 요청을 앱에 알리지 못했습니다: {e}");
                }
            }
            false
        })
        .on_permission_request(|webview, kind| {
            // Android는 이 콜백이 UI 스레드에서 돌아 `url()`이 자기를 기다리다 시간이 넘고, 늦은 응답이 wry를 패닉시킵니다.
            #[cfg(target_os = "android")]
            let app_origin = {
                let _ = &webview;
                ON_APP_PAGE.load(Ordering::Relaxed)
            };
            #[cfg(not(target_os = "android"))]
            let app_origin = webview.url().is_ok_and(|u| platform::is_app_origin(&u));
            match kind {
                // 음성·영상·화면 공유는 앱 화면에서만.
                PermissionKind::Microphone
                | PermissionKind::Camera
                | PermissionKind::DisplayCapture
                    if app_origin =>
                {
                    PermissionResponse::Allow
                }
                _ => PermissionResponse::Deny,
            }
        })
        .build()?;
    #[cfg(target_os = "linux")]
    platform::configure_linux_webview(&window);
    // OS 알림이 창 포커스를 알게 합니다. 윈도는 HWND로 전경 창을 직접 보고(포커스 이벤트가 빠질 수 있음),
    // 다른 운영체제는 이벤트를 씁니다.
    let daemon = app.state::<Arc<Daemon>>().inner().clone();
    #[cfg(windows)]
    if let Ok(hwnd) = window.hwnd() {
        daemon.notifier.set_hwnd(hwnd.0 as isize);
    }
    daemon
        .notifier
        .set_focused(window.is_focused().unwrap_or(false));
    #[cfg(desktop)]
    let win = window.clone();
    window.on_window_event(move |e| match e {
        tauri::WindowEvent::Focused(f) => {
            daemon.notifier.set_focused(*f);
            // 알림으로 켠 작업 표시줄 깜빡임을 끕니다. 리눅스(X11)는 긴급 표시를 앱이 지워야 다음에 다시 깜빡임.
            #[cfg(desktop)]
            if *f {
                let _ = win.request_user_attention(None);
            }
        }
        // 트레이 아이콘이 없으면(만들지 못함) 숨긴 창을 다시 열 길이 없으니 그대로 닫습니다.
        #[cfg(desktop)]
        tauri::WindowEvent::CloseRequested { api, .. }
            if CLOSE_TO_TRAY.load(Ordering::Relaxed)
                && win.app_handle().tray_by_id(TRAY_ID).is_some() =>
        {
            api.prevent_close();
            let _ = win.hide();
            // 숨길 때 Focused(false)가 온다고 믿지 않습니다(안 오면 지금 연 채널 알림이 계속 막힘).
            daemon.notifier.set_focused(false);
        }
        #[cfg(windows)]
        tauri::WindowEvent::Resized(_) if win.is_minimized().unwrap_or(false) => {
            platform::notify_minimized(&win);
        }
        _ => {}
    });
    Ok(window)
}

/// 트레이 아이콘: 왼쪽 클릭은 창을 앞으로, 메뉴는 열기·끝내기.
/// 리눅스는 뺍니다: appindicator 라이브러리가 없으면 오류가 아니라 panic이고, 트레이가 없는 데스크톱(GNOME)에서는
/// 아이콘이 만들어져도 보이지 않아 숨긴 창을 다시 열 수 없습니다.
#[cfg(all(desktop, not(target_os = "linux")))]
fn build_tray(app: &tauri::App) -> tauri::Result<()> {
    use tauri::menu::{Menu, MenuItem};
    use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
    let open = MenuItem::with_id(
        app,
        "open",
        i18n::text("open", "열기", &[]),
        true,
        None::<&str>,
    )?;
    let quit = MenuItem::with_id(
        app,
        "quit",
        i18n::text("quit", "끝내기", &[]),
        true,
        None::<&str>,
    )?;
    app.manage(TrayItems {
        open: open.clone(),
        quit: quit.clone(),
    });
    let mut tray = TrayIconBuilder::with_id(TRAY_ID)
        .tooltip("YANGPA")
        .menu(&Menu::with_items(app, &[&open, &quit])?)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, e| match e.id().as_ref() {
            "open" => notify::show_main(app),
            // RunEvent::Exit를 거쳐 앱이 띄운 데몬도 끕니다(on_exit).
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, e| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = e
            {
                notify::show_main(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon() {
        tray = tray.icon(icon.clone());
    }
    tray.build(app)?;
    Ok(())
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    // 기본은 경고 이상만. 메시지 내용·키는 어디에서도 로그로 남기지 않습니다.
    let filter = EnvFilter::try_from_env("YANGPA_LOG")
        .unwrap_or_else(|_| EnvFilter::new("warn,yangpa_desktop_lib=info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}

/// 앱이 끝날 때: 앱이 띄운 데몬을 끕니다.
fn on_exit(app: &AppHandle) {
    let daemon = app.state::<Arc<Daemon>>().inner().clone();
    tauri::async_runtime::block_on(async move {
        daemon.stop_on_exit(daemon::spawn_allowed()).await;
    });
}

/// 앱 진입점.
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    init_tracing();
    let daemon = Arc::new(Daemon::default());
    #[cfg_attr(mobile, allow(unused_mut))]
    let mut builder = tauri::Builder::default();
    // 첫 플러그인이어야 합니다. 데이터 경로를 따로 준 실행(한 PC에서 두 프로필 시험)은 따로 뜨게 둡니다.
    #[cfg(desktop)]
    if std::env::var_os("YANGPA_DATA_DIR").is_none_or(|d| d.is_empty()) {
        builder = builder.plugin(tauri_plugin_single_instance::init(|app, _, _| {
            notify::show_main(app)
        }));
    }
    #[cfg(target_os = "android")]
    let builder = builder.plugin(android::plugin());
    let result = builder
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(daemon.clone())
        .manage(BlobServer::default())
        .manage(preview::LocalPreviews::default())
        .manage(capture::CaptureWatch::default())
        .manage(Arc::new(neural::Neural::default()))
        .manage(Arc::new(screen::Screen::default()))
        .manage(Arc::new(screen::audio::ScreenAudio::default()))
        .manage(deeplink::PendingLink::default())
        .register_asynchronous_uri_scheme_protocol(blob::SCHEME, blob::handle)
        .on_page_load(|webview, payload| {
            // 새로 고침하면 화면공유를 받던 채널이 없어지므로, 남은 ffmpeg 캡처를 멈춥니다.
            if payload.event() == tauri::webview::PageLoadEvent::Started {
                #[cfg(target_os = "android")]
                ON_APP_PAGE.store(platform::is_app_origin(payload.url()), Ordering::Relaxed);
                webview.state::<Arc<screen::Screen>>().stop_all();
            }
        })
        .invoke_handler(tauri::generate_handler![
            rpc_call,
            daemon_status,
            daemon_retry,
            set_content_protection,
            set_capture_watch,
            open_external,
            ui_prefs_get,
            ui_prefs_set,
            shell_texts,
            notify_set_viewing,
            preview::local_preview_open,
            preview::local_preview_close,
            neural::neural_status,
            neural::neural_encode,
            neural::neural_decode,
            neural::neural_close,
            screen::screen_status,
            screen::screen_sources,
            screen::screen_start,
            screen::screen_stop,
            screen::audio::screen_audio_start,
            screen::audio::screen_audio_stop,
            secret::secret_copy,
            deeplink::deep_link_take,
            datadir::data_dir_info,
            datadir::data_dir_plan,
            datadir::data_dir_change,
            datadir::app_restart,
            update::update_target,
            update::update_install,
            #[cfg(target_os = "android")]
            android::file_import,
            #[cfg(target_os = "android")]
            android::file_export,
            #[cfg(target_os = "android")]
            android::file_release,
            #[cfg(target_os = "android")]
            android::call_active,
            #[cfg(target_os = "android")]
            android::app_minimize,
            #[cfg(target_os = "android")]
            android::app_info
        ])
        .setup(move |app| {
            #[cfg(all(desktop, not(target_os = "linux")))]
            if let Err(e) = build_tray(app) {
                warn!("트레이 아이콘을 만들지 못했습니다(창을 닫으면 앱이 끝납니다): {e}");
            }
            #[cfg(target_os = "android")]
            android::use_private_data_dir(app.handle())?;
            datadir::import_old_prefs(app.path().app_config_dir().ok());
            update::cleanup();
            let prefs = prefs_dir(app.handle())
                .map(|dir| prefs::load(&dir))
                .unwrap_or_default();
            apply_prefs(app.handle(), &daemon, &prefs);
            // 로그인할 때 켠 실행만 숨깁니다. 트레이가 없으면 숨긴 창을 열 길이 없으니 그대로 띄웁니다.
            #[cfg(desktop)]
            let hidden = std::env::args().any(|a| a == platform::AUTO_START_ARG)
                && prefs.get("autoStartToTray").and_then(Value::as_bool) == Some(true)
                && app.tray_by_id(TRAY_ID).is_some();
            #[cfg(mobile)]
            let hidden = false;
            create_main_window(app, hidden)?;
            let links = app.handle().clone();
            app.deep_link()
                .on_open_url(move |e| deeplink::open(&links, e.urls()));
            if let Ok(Some(urls)) = app.deep_link().get_current() {
                deeplink::open(app.handle(), urls);
            }
            let tap = app.handle().clone();
            app.notification()
                .on_action(move |a| notify::on_action(&tap, a))?;
            #[cfg(windows)]
            platform::register_toast_app_id(&app.config().identifier, "YANGPA");
            // 지난번에 그려 둔 알림 프사는 비웁니다(바뀐 프사·이름이 쌓이지 않게).
            if let Some(dir) = cache_dir(app.handle()) {
                avatar::clear(&dir.join(avatar::DIR));
            }
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(daemon::supervise(handle, daemon.clone()));
            tauri::async_runtime::spawn(idle::watch(daemon.clone()));
            tauri::async_runtime::spawn(notify::watch_offline(daemon.clone()));
            Ok(())
        })
        .build(tauri::generate_context!());
    match result {
        Ok(app) => app.run(|handle, event| match event {
            // 최근 앱에서 밀어 화면이 닫혀도 프로세스는 서비스로 남아 메시지를 받습니다(MainActivity.kt).
            #[cfg(target_os = "android")]
            RunEvent::ExitRequested {
                api, code: None, ..
            } if CLOSE_TO_TRAY.load(Ordering::Relaxed) => api.prevent_exit(),
            RunEvent::Exit => on_exit(handle),
            _ => {}
        }),
        Err(e) => {
            error!("앱을 시작하지 못했습니다: {e}");
            std::process::exit(1);
        }
    }
}
