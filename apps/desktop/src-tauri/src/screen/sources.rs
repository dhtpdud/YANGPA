//! 공유할 수 있는 모니터·창 목록(윈도). 웹뷰 기본 선택 창 대신 앱이 직접(미리보기·아이콘과 함께) 보여 주고, 고른 핸들을
//! gfxcapture에 넘깁니다.
//! 창 제목은 화면에만 쓰고 로그에 남기지 않습니다.

use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceKind {
    Monitor,
    Window,
}

#[derive(Clone, Debug, Serialize)]
pub struct Source {
    pub kind: SourceKind,
    /// 핸들(HMONITOR·HWND)을 10진 문자열로(자바스크립트 숫자 정밀도를 넘을 수 있음).
    pub id: String,
    pub title: String,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
    /// 미리보기(JPEG)·창 아이콘(PNG) data URL. 선택 창에 줄 목록에만 채웁니다(`thumbs`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thumbnail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
}

#[cfg(windows)]
pub fn list() -> Vec<Source> {
    let mut out = monitors();
    out.extend(windows());
    out
}

#[cfg(not(windows))]
pub fn list() -> Vec<Source> {
    Vec::new()
}

/// 핸들이 지금도 목록에 있는 것인지 보고 그 크기를 돌려줍니다(웹뷰가 보낸 숫자를 그대로 믿지 않음).
pub fn find(kind: &str, id: &str) -> Option<Source> {
    list().into_iter().find(|s| {
        s.id == id
            && matches!(
                (kind, &s.kind),
                ("monitor", SourceKind::Monitor) | ("window", SourceKind::Window)
            )
    })
}

/// 목록에 있는 창의 프로세스 ID(화면 소리를 그 프로그램 것만 잡을 때).
#[cfg(windows)]
pub fn window_process(id: &str) -> Option<u32> {
    use windows_sys::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;
    let w = find("window", id)?;
    let handle: usize = w.id.parse().ok()?;
    let mut pid = 0u32;
    // SAFETY: 방금 목록에서 찾은 창 핸들이고, 프로세스 ID만 읽습니다.
    unsafe { GetWindowThreadProcessId(handle as _, &mut pid) };
    (pid != 0).then_some(pid)
}

#[cfg(not(windows))]
pub fn window_process(_id: &str) -> Option<u32> {
    None
}

#[cfg(windows)]
fn monitors() -> Vec<Source> {
    use windows_sys::Win32::Foundation::{LPARAM, RECT};
    use windows_sys::Win32::Graphics::Gdi::{
        EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFOEXW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::MONITORINFOF_PRIMARY;
    use windows_sys::core::BOOL;

    unsafe extern "system" fn each(m: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
        // SAFETY: data는 아래에서 넘긴 Vec의 포인터이고 EnumDisplayMonitors가 끝날 때까지 살아 있습니다.
        let out = unsafe { &mut *(data as *mut Vec<Source>) };
        // SAFETY: 크기 칸을 채운 MONITORINFOEXW를 넘깁니다.
        let mut info: MONITORINFOEXW = unsafe { std::mem::zeroed() };
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if unsafe { GetMonitorInfoW(m, &mut info.monitorInfo) } == 0 {
            return 1;
        }
        let r = info.monitorInfo.rcMonitor;
        let n = out.len() + 1;
        let primary = info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0;
        out.push(Source {
            kind: SourceKind::Monitor,
            id: (m as usize as u64).to_string(),
            title: crate::i18n::text(
                if primary { "screenPrimary" } else { "screen" },
                if primary {
                    "화면 {n}(주 모니터)"
                } else {
                    "화면 {n}"
                },
                &[("n", &n.to_string())],
            ),
            width: (r.right - r.left).max(0) as u32,
            height: (r.bottom - r.top).max(0) as u32,
            primary,
            thumbnail: None,
            icon: None,
        });
        1
    }

    let mut out: Vec<Source> = Vec::new();
    // SAFETY: 콜백은 위의 each, data는 out을 가리킵니다.
    unsafe {
        EnumDisplayMonitors(
            std::ptr::null_mut(),
            std::ptr::null(),
            Some(each),
            &mut out as *mut Vec<Source> as LPARAM,
        );
    }
    out
}

#[cfg(windows)]
fn windows() -> Vec<Source> {
    use windows_sys::Win32::Foundation::{HWND, LPARAM, RECT};
    use windows_sys::Win32::Graphics::Dwm::{
        DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GWL_EXSTYLE, GetWindowLongPtrW, GetWindowTextLengthW, GetWindowTextW,
        GetWindowThreadProcessId, IsIconic, IsWindowVisible, WS_EX_TOOLWINDOW,
    };
    use windows_sys::core::BOOL;

    unsafe extern "system" fn each(w: HWND, data: LPARAM) -> BOOL {
        // SAFETY: data는 아래에서 넘긴 Vec의 포인터입니다. 아래 호출은 모두 창 핸들만 읽습니다.
        let out = unsafe { &mut *(data as *mut Vec<Source>) };
        unsafe {
            if IsWindowVisible(w) == 0 || IsIconic(w) != 0 {
                return 1;
            }
            if GetWindowLongPtrW(w, GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW != 0 {
                return 1;
            }
            let mut pid = 0u32;
            GetWindowThreadProcessId(w, &mut pid);
            // 이 앱의 창은 빼고(자기 화면을 공유하면 무한 거울), 가상 데스크톱 등으로 가려진(cloaked) 창도 뺍니다.
            if pid == std::process::id() {
                return 1;
            }
            let mut cloaked = 0u32;
            if DwmGetWindowAttribute(
                w,
                DWMWA_CLOAKED as u32,
                &mut cloaked as *mut u32 as *mut _,
                4,
            ) == 0
                && cloaked != 0
            {
                return 1;
            }
            let len = GetWindowTextLengthW(w);
            if len <= 0 {
                return 1;
            }
            let mut buf = vec![0u16; len as usize + 1];
            let n = GetWindowTextW(w, buf.as_mut_ptr(), buf.len() as i32);
            let title = String::from_utf16_lossy(&buf[..n.max(0) as usize]);
            if title.trim().is_empty() {
                return 1;
            }
            let mut r: RECT = std::mem::zeroed();
            if DwmGetWindowAttribute(
                w,
                DWMWA_EXTENDED_FRAME_BOUNDS as u32,
                &mut r as *mut RECT as *mut _,
                std::mem::size_of::<RECT>() as u32,
            ) != 0
            {
                return 1;
            }
            let (width, height) = (
                (r.right - r.left).max(0) as u32,
                (r.bottom - r.top).max(0) as u32,
            );
            if width < 64 || height < 64 {
                return 1;
            }
            out.push(Source {
                kind: SourceKind::Window,
                id: (w as usize as u64).to_string(),
                title,
                width,
                height,
                primary: false,
                thumbnail: None,
                icon: None,
            });
        }
        1
    }

    let mut out: Vec<Source> = Vec::new();
    // SAFETY: 콜백은 위의 each, data는 out을 가리킵니다.
    unsafe {
        EnumWindows(Some(each), &mut out as *mut Vec<Source> as LPARAM);
    }
    out
}
