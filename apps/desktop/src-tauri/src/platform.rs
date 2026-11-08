//! 플랫폼별 처리: 외부 링크 열기, 화면 캡처 제외 지원 여부, 웹뷰 이동 제한, 리눅스 웹뷰 설정, 윈도 알림 앱 ID 등록.

use std::process::{Command, Stdio};

use url::Url;

use crate::error::CmdError;

/// 외부로 여는 주소 최대 길이.
pub const URL_MAX: usize = 4096;

/// http(s)만, 사용자 정보 없이, 길이 제한. 정규화한 문자열을 돌려줍니다.
pub fn validate_external_url(raw: &str) -> Result<String, CmdError> {
    if raw.len() > URL_MAX {
        return Err(CmdError::invalid_params("주소가 너무 깁니다"));
    }
    let url =
        Url::parse(raw.trim()).map_err(|_| CmdError::invalid_params("주소가 잘못됐습니다"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(CmdError::invalid_params("http(s) 주소만 열 수 있습니다"));
    }
    if url.host_str().is_none_or(str::is_empty) {
        return Err(CmdError::invalid_params("주소에 호스트가 없습니다"));
    }
    // `https://은행@피싱` 같은 속임수를 막습니다.
    if !url.username().is_empty() || url.password().is_some() {
        return Err(CmdError::invalid_params(
            "사용자 정보가 든 주소는 열지 않습니다",
        ));
    }
    Ok(url.into())
}

/// 기본 브라우저로 엽니다. 셸을 거치지 않고 인자로만 넘깁니다(정규화된 URL이라 공백·따옴표 없음).
pub fn open_in_browser(url: &str) -> Result<(), CmdError> {
    let url = validate_external_url(url)?;
    let mut cmd = if cfg!(target_os = "macos") {
        let mut c = Command::new("open");
        c.arg(&url);
        c
    } else if cfg!(windows) {
        let mut c = Command::new("rundll32");
        c.arg("url.dll,FileProtocolHandler").arg(&url);
        c
    } else {
        let mut c = Command::new("xdg-open");
        c.arg(&url);
        c
    };
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = cmd
        .spawn()
        .map_err(|e| CmdError::internal(format!("브라우저를 열지 못했습니다: {e}")))?;
    // 좀비가 남지 않게 따로 기다립니다.
    std::thread::Builder::new()
        .name("open-url".into())
        .spawn(move || {
            let _ = child.wait();
        })
        .ok();
    Ok(())
}

/// 창 캡처 제외를 OS가 지원하는지(윈도: WDA_EXCLUDEFROMCAPTURE, macOS: NSWindowSharingNone, Android: FLAG_SECURE,
/// 리눅스: 없음).
pub const CONTENT_PROTECTION_SUPPORTED: bool = cfg!(any(
    target_os = "windows",
    target_os = "macos",
    target_os = "android"
));

/// 앱 화면 자체의 주소인지(마이크·카메라·화면 공유 권한은 여기서만 줍니다).
/// `about:blank`나 iframe 플레이어(https) 같은, 이동은 허용되지만 앱이 아닌 주소는 빠집니다.
pub fn is_app_origin(url: &Url) -> bool {
    match url.scheme() {
        "tauri" => url.host_str() == Some("localhost"),
        "http" | "https" => {
            let host = url.host_str().unwrap_or("");
            host == "tauri.localhost"
                || (cfg!(debug_assertions)
                    && url.scheme() == "http"
                    && host == "localhost"
                    && url.port() == Some(1420))
        }
        _ => false,
    }
}

/// iframe 이동도 이동 처리기를 거치는지(wry 0.57 소스 기준).
/// 윈도 WebView2는 `NavigationStarting`(최상위 프레임만)에 붙어 iframe은 여기로 오지 않고,
/// macOS WKWebView(`decidePolicyForNavigationAction`)·리눅스 WebKitGTK(`decide-policy`)는 모든 프레임이 옵니다.
const SUBFRAME_NAVIGATION_HITS_HANDLER: bool = !cfg!(any(windows, target_os = "android"));

/// 웹뷰가 이동해도 되는 주소. 앱 화면과, 누른 뒤 불러오는 https 플레이어(iframe)만.
/// macOS·리눅스는 iframe 이동도 같은 처리기를 거쳐 https를 막으면 플레이어가 깨지므로 https를 허용합니다
/// (대신 IPC 권한은 앱 출처에만 있고, 링크는 기본 브라우저로 엽니다). 윈도는 최상위 창의 https 이동을 막습니다.
pub fn navigation_allowed(url: &Url) -> bool {
    navigation_allowed_for(url, SUBFRAME_NAVIGATION_HITS_HANDLER)
}

fn navigation_allowed_for(url: &Url, subframes_hit_handler: bool) -> bool {
    is_app_origin(url)
        || (url.scheme() == "about" && matches!(url.path(), "blank" | "srcdoc"))
        || (subframes_hit_handler && url.scheme() == "https")
}

/// 리눅스 WebKitGTK: WebRTC·미디어 스트림을 켜고, 콘솔 메시지가 표준 출력(로그)으로 가지 않게 합니다.
#[cfg(target_os = "linux")]
pub fn configure_linux_webview<R: tauri::Runtime>(w: &tauri::WebviewWindow<R>) {
    use webkit2gtk::{SettingsExt, WebViewExt};
    let r = w.with_webview(|pw| {
        let wv = pw.inner();
        if let Some(s) = WebViewExt::settings(&wv) {
            s.set_enable_media_stream(true);
            s.set_enable_mediasource(true);
            s.set_enable_webrtc(true);
            s.set_enable_write_console_messages_to_stdout(false);
            s.set_enable_offline_web_application_cache(false);
        }
    });
    if let Err(e) = r {
        tracing::warn!("웹뷰 설정을 바꾸지 못했습니다: {e}");
    }
}

/// 윈도: 창을 최소화했다고 웹뷰에 알립니다(보던 화면공유를 PiP로 띄움, components/ScreenPip.tsx). 웹뷰의 PiP는 사용자
/// 동작 뒤에만 열리는데 제목 표시줄의 최소화는 웹뷰 밖의 동작이라, DevTools 프로토콜의 `userGesture`로 이벤트를 보냅니다.
#[cfg(windows)]
pub fn notify_minimized<R: tauri::Runtime>(w: &tauri::WebviewWindow<R>) {
    use webview2_com::CallDevToolsProtocolMethodCompletedHandler;
    use windows_core::HSTRING;
    const PARAMS: &str =
        r#"{"expression":"dispatchEvent(new Event('yangpa-minimized'))","userGesture":true}"#;
    let r = w.with_webview(|pw| {
        // SAFETY: 메인 스레드에서 살아 있는 컨트롤러로 부르고, 문자열은 호출 동안 살아 있습니다.
        let r = unsafe {
            pw.controller().CoreWebView2().and_then(|core| {
                core.CallDevToolsProtocolMethod(
                    &HSTRING::from("Runtime.evaluate"),
                    &HSTRING::from(PARAMS),
                    &CallDevToolsProtocolMethodCompletedHandler::create(Box::new(|_, _| Ok(()))),
                )
            })
        };
        if let Err(e) = r {
            tracing::debug!("최소화를 웹뷰에 알리지 못했습니다: {e}");
        }
    });
    if let Err(e) = r {
        tracing::debug!("최소화를 웹뷰에 알리지 못했습니다: {e}");
    }
}

/// 윈도: 설치하지 않고 실행한 앱(직접 빌드한 exe, 압축본)의 OS 알림이 보이도록 알림 앱 ID(AUMID)를 현재 사용자
/// 레지스트리(`HKCU\Software\Classes\AppUserModelId\<ID>`)에 등록합니다. 알림 플러그인은 `target\debug`·
/// `target\release` 밖에서 실행하면 번들 식별자를 앱 ID로 쓰는데, 윈도는 등록되지 않은 앱 ID의 알림을 조용히 버립니다.
/// 설치본은 시작 메뉴 바로 가기가 같은 ID를 이미 갖고 있어 덮어써도 그대로입니다.
#[cfg(windows)]
pub fn register_toast_app_id(aumid: &str, display_name: &str) {
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey,
        RegCreateKeyExW, RegSetValueExW,
    };

    // 플러그인과 같은 조건: 개발 빌드 폴더에서는 앱 ID를 쓰지 않으므로 등록도 하지 않습니다.
    if dev_build() || aumid.is_empty() {
        return;
    }
    let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
    let subkey = wide(&format!("Software\\Classes\\AppUserModelId\\{aumid}"));
    let name = wide("DisplayName");
    let value = wide(display_name);
    let Ok(bytes) = u32::try_from(value.len() * 2) else {
        return;
    };
    let mut key: HKEY = std::ptr::null_mut();
    // SAFETY: 문자열은 모두 NUL로 끝나는 UTF-16이고 호출 동안 살아 있습니다. key는 만들기에 성공했을 때만 쓰고 닫습니다.
    unsafe {
        let r = RegCreateKeyExW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            0,
            std::ptr::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            std::ptr::null(),
            &mut key,
            std::ptr::null_mut(),
        );
        if r != ERROR_SUCCESS {
            tracing::warn!(
                "알림 앱 ID를 등록하지 못했습니다(오류 {r}). OS 알림이 보이지 않으면 설치본을 쓰세요"
            );
            return;
        }
        let r = RegSetValueExW(key, name.as_ptr(), 0, REG_SZ, value.as_ptr().cast(), bytes);
        RegCloseKey(key);
        if r != ERROR_SUCCESS {
            tracing::warn!("알림 앱 이름을 등록하지 못했습니다(오류 {r})");
        }
    }
}

/// 개발 빌드 폴더(`target\debug`·`target\release`)에서 실행했는지.
#[cfg(windows)]
fn dev_build() -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.display().to_string()))
        .is_some_and(|d| d.ends_with("\\target\\debug") || d.ends_with("\\target\\release"))
}

/// 로그인할 때 켠 실행에 붙이는 인자. 이걸 보고 UI 설정 `autoStartToTray`면 창 없이 트레이로만 켭니다.
pub const AUTO_START_ARG: &str = "--autostart";

/// 윈도: 로그인하면 앱을 켜도록 현재 사용자의 `Run` 키에 지금 실행 파일을 등록하거나 지웁니다(UI 설정 `autoStart`).
/// 실행할 때마다 다시 써서 압축본을 옮겨도 새 경로를 따라갑니다.
#[cfg(windows)]
pub fn set_auto_start(on: bool) {
    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
    use windows_sys::Win32::System::Registry::{
        HKEY_CURRENT_USER, REG_SZ, RegDeleteKeyValueW, RegSetKeyValueW,
    };

    // 개발 빌드와 데이터 경로를 따로 준 시험 실행이 사용자의 진짜 시작 프로그램을 바꾸지 않게 합니다.
    if dev_build() || std::env::var_os("YANGPA_DATA_DIR").is_some_and(|d| !d.is_empty()) {
        return;
    }
    let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
    let subkey = wide("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
    let name = wide("YANGPA");
    let r = if on {
        let Ok(exe) = std::env::current_exe() else {
            return;
        };
        // Program Files처럼 빈칸이 있는 경로도 한 덩어리로 읽히게 따옴표로 감쌉니다.
        let value = wide(&format!("\"{}\" {AUTO_START_ARG}", exe.display()));
        let Ok(bytes) = u32::try_from(value.len() * 2) else {
            return;
        };
        // SAFETY: 문자열은 모두 NUL로 끝나는 UTF-16이고 호출 동안 살아 있습니다.
        unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                subkey.as_ptr(),
                name.as_ptr(),
                REG_SZ,
                value.as_ptr().cast(),
                bytes,
            )
        }
    } else {
        // SAFETY: 위와 같음.
        match unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, subkey.as_ptr(), name.as_ptr()) } {
            ERROR_FILE_NOT_FOUND => ERROR_SUCCESS,
            r => r,
        }
    };
    if r != ERROR_SUCCESS {
        tracing::warn!("윈도 시작 프로그램 설정을 바꾸지 못했습니다(오류 {r})");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn external_urls() {
        assert_eq!(
            validate_external_url("https://example.com/a b").unwrap(),
            "https://example.com/a%20b"
        );
        assert!(validate_external_url("http://example.com").is_ok());
        for bad in [
            "javascript:alert(1)",
            "file:///etc/passwd",
            "yangpa-blob://localhost/x",
            "https://user:pw@example.com",
            "https://bank.example@evil.example",
            "not a url",
            "mailto:a@b.c",
        ] {
            assert!(validate_external_url(bad).is_err(), "{bad}");
        }
        assert!(validate_external_url(&format!("https://e.com/{}", "a".repeat(URL_MAX))).is_err());
    }

    #[test]
    fn app_origin_only() {
        let app = |s: &str| is_app_origin(&Url::parse(s).unwrap());
        assert!(app("tauri://localhost/index.html"));
        assert!(app("http://tauri.localhost/"));
        assert!(app("https://tauri.localhost/"));
        assert!(!app("about:blank"));
        assert!(!app("https://www.youtube-nocookie.com/embed/x"));
        assert!(!app("https://tauri.localhost.evil.example/"));
        assert!(!app("tauri://evil/"));
        assert!(!app("yangpa-blob://localhost/blob/a/b/c"));
        assert_eq!(app("http://localhost:1420/"), cfg!(debug_assertions));
        assert!(!app("http://localhost:8080/"));
    }

    #[test]
    fn navigation() {
        for subframes in [true, false] {
            let ok = |s: &str| navigation_allowed_for(&Url::parse(s).unwrap(), subframes);
            assert!(ok("tauri://localhost/index.html"));
            assert!(ok("http://tauri.localhost/"));
            assert!(ok("https://tauri.localhost/"));
            assert!(ok("about:blank"));
            assert!(!ok("http://example.com/"));
            assert!(!ok("file:///etc/passwd"));
            assert!(!ok("tauri://evil/"));
            assert!(!ok("data:text/html,hi"));
            assert!(!ok("blob:tauri://localhost/x"));
            assert!(!ok("yangpa-blob://localhost/blob/a/b/c"));
        }
        // iframe 플레이어는 처리기를 거치는 플랫폼에서만 https로 허용합니다.
        let player = Url::parse("https://www.youtube-nocookie.com/embed/x").unwrap();
        assert!(navigation_allowed_for(&player, true));
        // 윈도: 처리기에 오는 것은 최상위 창 이동뿐이라 외부 https로 창을 바꾸지 못합니다.
        assert!(!navigation_allowed_for(&player, false));
        assert_eq!(navigation_allowed(&player), !cfg!(windows));
    }
}
