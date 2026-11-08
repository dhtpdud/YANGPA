//! Android 전용 앱 기능. Kotlin 쪽은 `gen/android/.../YangpaPlugin.kt`입니다.
//!
//! - 데이터 경로: 앱 전용 폴더(`use_private_data_dir`). `allowBackup`을 꺼서 백업으로도 나가지 않습니다.
//! - 포그라운드 서비스(`keep_alive`, `call_active`): 서버 푸시가 없어 프로세스가 살아 있어야 메시지를 받습니다.
//! - `file_import`·`file_export`: 파일 고르기·저장 창이 주는 `content://`를 데몬이 쓰는 경로와 잇습니다.
//! - `set_secure`: 휘발성 채널의 FLAG_SECURE(캡처·최근 앱 미리보기 막기).
//! - `notify`: 메시지 알림. 최근 앱에서 밀어 화면이 닫힌 뒤에도 돌므로 `call`(화면이 없으면 패닉)을 쓰지 않습니다.

use std::sync::atomic::{AtomicI32, Ordering};

use jni::JavaVM;
use jni::objects::{JClass, JObject, JValue};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tauri::plugin::{Builder, PluginHandle, TauriPlugin};
use tauri::{AppHandle, Manager, Runtime};
use tracing::warn;

use crate::error::CmdError;
use crate::platform;

const PLUGIN: &str = "yangpa-android";
const PLUGIN_PACKAGE: &str = "app.yangpa.android";
const PLUGIN_CLASS: &str = "YangpaPlugin";
/// 앱 데이터 폴더 안 데몬 데이터 폴더.
const DATA_SUBDIR: &str = "daemon";
/// 고른 파일의 복사본을 두는 앱 캐시 안 폴더(YangpaPlugin.kt).
const IMPORTS: &str = "imports";
/// Android는 앱 데이터 폴더의 파일을 실행할 수 없어 lyrebird를 네이티브 라이브러리로 넣습니다.
const LYREBIRD: &str = "liblyrebird.so";

struct Bridge<R: Runtime>(PluginHandle<R>);

pub fn plugin<R: Runtime>() -> TauriPlugin<R> {
    Builder::new(PLUGIN)
        .setup(|app, api| {
            let handle = api.register_android_plugin(PLUGIN_PACKAGE, PLUGIN_CLASS)?;
            app.manage(Bridge(handle));
            Ok(())
        })
        .build()
}

fn call<R: Runtime, T: serde::de::DeserializeOwned>(
    app: &AppHandle<R>,
    method: &str,
    args: serde_json::Value,
) -> Result<T, CmdError> {
    app.state::<Bridge<R>>()
        .0
        .run_mobile_plugin(method, args)
        .map_err(|e| CmdError::internal(format!("Android 기능을 부르지 못했습니다({method}): {e}")))
}

#[derive(Deserialize)]
struct PathReply {
    path: String,
}

/// 데몬이 읽는 데이터 경로와 lyrebird 경로를 정합니다. 데몬이 뜨기 전(setup)에 불러야 합니다.
pub fn use_private_data_dir<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<()> {
    let dir = app.path().app_data_dir()?.join(DATA_SUBDIR);
    let libs = call::<R, PathReply>(app, "nativeLibDir", json!({}));
    // SAFETY: 이 값을 읽는 데몬 감독 작업은 setup이 끝난 뒤에 뜹니다.
    unsafe {
        std::env::set_var(yangpa_rpc::endpoint::ENV_DATA_DIR, dir);
        match libs {
            Ok(l) => std::env::set_var(
                yangpa_daemon::config::ENV_LYREBIRD,
                std::path::Path::new(&l.path).join(LYREBIRD),
            ),
            Err(e) => warn!("{e}"),
        }
    }
    Ok(())
}

/// 포그라운드 서비스를 켜고 끕니다(UI 설정 `closeToTray`). 알림 글은 화면 언어를 따릅니다.
pub fn keep_alive<R: Runtime>(app: &AppHandle<R>, on: bool) {
    let text = crate::i18n::text("keepAlive", "메시지를 받기 위해 실행 중입니다", &[]);
    if let Err(e) =
        call::<R, serde_json::Value>(app, "keepAlive", json!({ "on": on, "text": text }))
    {
        warn!("{e}");
    }
}

/// FLAG_SECURE를 켜고 끕니다.
pub fn set_secure<R: Runtime>(app: &AppHandle<R>, on: bool) -> Result<(), CmdError> {
    call::<R, serde_json::Value>(app, "setSecure", json!({ "on": on })).map(|_| ())
}

/// 프로세스를 새로 띄웁니다. 화면이 없으면 끝내기만 하고 다음에 앱을 열 때 뜹니다.
pub fn restart<R: Runtime>(app: &AppHandle<R>) {
    if app.get_webview_window(crate::daemon::MAIN_WINDOW).is_none() {
        std::process::exit(0);
    }
    if let Err(e) = call::<R, serde_json::Value>(app, "restart", json!({})) {
        warn!("{e}");
    }
}

/// 알림 ID. 1번은 포그라운드 서비스의 상시 알림입니다.
static NEXT_NOTICE: AtomicI32 = AtomicI32::new(2);

/// OS 알림을 띄웁니다. `link`는 누르면 열 주소입니다.
pub fn notify(title: &str, body: Option<&str>, link: Option<&str>) {
    if let Err(e) = show_notice(title, body, link) {
        warn!("알림을 띄우지 못했습니다: {e}");
    }
}

fn show_notice(title: &str, body: Option<&str>, link: Option<&str>) -> jni::errors::Result<()> {
    let ctx = ndk_context::android_context();
    // SAFETY: tao가 첫 화면에서 정한 프로세스 전역 값입니다(VM과 애플리케이션 컨텍스트의 전역 참조).
    let (vm, context) = unsafe {
        (
            JavaVM::from_raw(ctx.vm().cast())?,
            JObject::from_raw(ctx.context().cast()),
        )
    };
    let mut env = vm.attach_current_thread()?;
    // 이 스레드는 시스템 클래스 로더라 앱 클래스를 찾지 못합니다.
    let loader = env
        .call_method(&context, "getClassLoader", "()Ljava/lang/ClassLoader;", &[])?
        .l()?;
    let name = env.new_string("app.yangpa.android.YangpaNotifier")?;
    let class = env
        .call_method(
            &loader,
            "loadClass",
            "(Ljava/lang/String;)Ljava/lang/Class;",
            &[JValue::Object(&name)],
        )?
        .l()?;
    let channel = env.new_string(crate::i18n::text("newMessage", "새 메시지", &[]))?;
    let title = env.new_string(title)?;
    let body: JObject = match body {
        Some(b) => env.new_string(b)?.into(),
        None => JObject::null(),
    };
    let link: JObject = match link {
        Some(l) => env.new_string(l)?.into(),
        None => JObject::null(),
    };
    env.call_static_method(
        JClass::from(class),
        "show",
        "(Landroid/content/Context;ILjava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)V",
        &[
            JValue::Object(&context),
            JValue::Int(NEXT_NOTICE.fetch_add(1, Ordering::Relaxed)),
            JValue::Object(&channel),
            JValue::Object(&title),
            JValue::Object(&body),
            JValue::Object(&link),
        ],
    )?;
    Ok(())
}

/// 기본 브라우저로 엽니다.
pub fn open_url<R: Runtime>(app: &AppHandle<R>, url: &str) -> Result<(), CmdError> {
    let url = platform::validate_external_url(url)?;
    call::<R, serde_json::Value>(app, "openUrl", json!({ "url": url })).map(|_| ())
}

/// 고른 `content://` 파일을 앱 폴더로 복사하고 그 경로를 돌려줍니다. 파일 이름은 그대로라 첨부 이름이 유지됩니다.
#[tauri::command]
pub async fn file_import(app: AppHandle, uri: String) -> Result<String, CmdError> {
    tauri::async_runtime::spawn_blocking(move || {
        call::<_, PathReply>(&app, "fileImport", json!({ "uri": uri })).map(|i| i.path)
    })
    .await
    .map_err(|_| CmdError::internal("파일 복사가 멈췄습니다"))?
}

/// 보낸 파일의 앱 캐시 복사본을 지웁니다. 데몬은 암호화한 사본을 따로 두므로 평문 복사본을 남기지 않습니다.
#[tauri::command]
pub fn file_release(app: AppHandle, path: String) -> Result<(), CmdError> {
    let root = app
        .path()
        .app_cache_dir()
        .map_err(|e| CmdError::internal(format!("캐시 경로를 찾지 못했습니다: {e}")))?
        .join(IMPORTS);
    let dir = std::path::Path::new(&path).parent();
    match dir {
        // `file_name`이 없으면 `..`입니다.
        Some(d) if d.parent() == Some(root.as_path()) && d.file_name().is_some() => {
            std::fs::remove_dir_all(d)
                .map_err(|e| CmdError::internal(format!("복사본을 지우지 못했습니다: {e}")))
        }
        _ => Err(CmdError::invalid_params("앱이 복사한 파일이 아닙니다")),
    }
}

/// 임시 파일을 저장 창이 준 `content://`에 쓰고 임시 파일을 지웁니다.
#[tauri::command]
pub async fn file_export(app: AppHandle, path: String, uri: String) -> Result<(), CmdError> {
    tauri::async_runtime::spawn_blocking(move || {
        call::<_, serde_json::Value>(&app, "fileExport", json!({ "path": path, "uri": uri }))
            .map(|_| ())
    })
    .await
    .map_err(|_| CmdError::internal("파일 저장이 멈췄습니다"))?
}

/// 통화 중이면 서비스에 마이크 종류를 더합니다(앱을 내려도 통화가 끊기지 않게).
#[tauri::command]
pub fn call_active(app: AppHandle, on: bool) -> Result<(), CmdError> {
    call::<_, serde_json::Value>(&app, "callActive", json!({ "on": on })).map(|_| ())
}

/// 앱을 뒤로 내립니다(첫 화면에서 뒤로 가기).
#[tauri::command]
pub fn app_minimize(app: AppHandle) -> Result<(), CmdError> {
    call::<_, serde_json::Value>(&app, "minimize", json!({})).map(|_| ())
}

#[derive(Debug, Deserialize, Serialize)]
pub struct AppInfo {
    /// 임시 파일을 둘 폴더(저장 창에 쓰기 전).
    pub temp_dir: String,
}

#[tauri::command]
pub fn app_info(app: AppHandle) -> Result<AppInfo, CmdError> {
    let dir = app
        .path()
        .app_cache_dir()
        .map_err(|e| CmdError::internal(format!("캐시 경로를 찾지 못했습니다: {e}")))?;
    Ok(AppInfo {
        temp_dir: dir.to_string_lossy().into_owned(),
    })
}
