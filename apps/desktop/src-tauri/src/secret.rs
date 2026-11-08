//! 기기 연결 링크의 복사.
//!
//! 윈도는 클립보드 기록(Win+V)·클라우드 클립보드·클립보드 감시 프로그램에서 빠지게 표시해서 넣고, [`CLEAR_AFTER`] 뒤
//! 클립보드가 그대로면 비웁니다. 다른 OS는 false를 돌려주고 웹뷰가 직접 복사합니다.
//!
//! 비밀값은 로그에 남기지 않습니다.

use std::time::Duration;

use crate::error::CmdError;

/// 복사한 값을 클립보드에 두는 시간.
pub const CLEAR_AFTER: Duration = Duration::from_secs(60);

/// 클립보드에 넣습니다. 이 OS에서 직접 넣지 못하면 false(웹뷰가 넣음).
#[tauri::command]
pub fn secret_copy(text: String) -> Result<bool, CmdError> {
    imp::copy(&text, CLEAR_AFTER)
}

#[cfg(windows)]
mod imp {
    use std::time::Duration;

    use windows_sys::Win32::Foundation::{GlobalFree, HGLOBAL};
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, GetClipboardSequenceNumber, OpenClipboard,
        RegisterClipboardFormatW, SetClipboardData,
    };
    use windows_sys::Win32::System::Memory::{
        GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock,
    };
    use windows_sys::Win32::System::Ole::CF_UNICODETEXT;

    use crate::error::CmdError;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// 바이트를 옮길 수 있는 전역 메모리에 담습니다.
    unsafe fn global(bytes: &[u8]) -> Option<HGLOBAL> {
        unsafe {
            let h = GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1));
            if h.is_null() {
                return None;
            }
            let p = GlobalLock(h);
            if p.is_null() {
                GlobalFree(h);
                return None;
            }
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), p.cast::<u8>(), bytes.len());
            GlobalUnlock(h);
            Some(h)
        }
    }

    /// 성공하면 클립보드가 메모리를 가져갑니다. 실패하면 직접 풉니다.
    unsafe fn set(format: u32, bytes: &[u8]) -> bool {
        unsafe {
            let Some(h) = global(bytes) else { return false };
            if SetClipboardData(format, h).is_null() {
                GlobalFree(h);
                return false;
            }
            true
        }
    }

    /// 클립보드 기록·클라우드 동기화·감시 프로그램에서 빼 달라는 표시.
    /// <https://learn.microsoft.com/windows/win32/dataxchg/clipboard-formats#cloud-clipboard-and-clipboard-history-formats>
    unsafe fn mark_private() {
        unsafe {
            let zero = 0u32.to_le_bytes();
            for (name, data) in [
                ("ExcludeClipboardContentFromMonitorProcessing", &zero[..]),
                ("CanIncludeInClipboardHistory", &zero[..]),
                ("CanUploadToCloudClipboard", &zero[..]),
            ] {
                let f = RegisterClipboardFormatW(wide(name).as_ptr());
                if f != 0 {
                    set(f, data);
                }
            }
        }
    }

    pub fn copy(text: &str, clear_after: Duration) -> Result<bool, CmdError> {
        let fail = || CmdError::internal("클립보드에 넣지 못했습니다");
        let utf16: Vec<u8> = wide(text).iter().flat_map(|c| c.to_le_bytes()).collect();
        // SAFETY: 클립보드를 열고 닫는 사이에만 쓰고, 넘긴 메모리는 성공하면 클립보드 것이 됩니다.
        let seq = unsafe {
            if OpenClipboard(std::ptr::null_mut()) == 0 {
                return Err(fail());
            }
            let ok = EmptyClipboard() != 0 && {
                mark_private();
                set(u32::from(CF_UNICODETEXT), &utf16)
            };
            CloseClipboard();
            if !ok {
                return Err(fail());
            }
            GetClipboardSequenceNumber()
        };
        std::thread::spawn(move || {
            std::thread::sleep(clear_after);
            // SAFETY: 위와 같음. 그 사이 다른 것을 복사했으면(순번이 바뀜) 건드리지 않습니다.
            unsafe {
                if GetClipboardSequenceNumber() == seq && OpenClipboard(std::ptr::null_mut()) != 0 {
                    if GetClipboardSequenceNumber() == seq {
                        EmptyClipboard();
                    }
                    CloseClipboard();
                }
            }
        });
        Ok(true)
    }
}

#[cfg(not(windows))]
mod imp {
    use std::time::Duration;

    use crate::error::CmdError;

    pub fn copy(_text: &str, _clear_after: Duration) -> Result<bool, CmdError> {
        Ok(false)
    }
}
