//! 휘발성 채널을 보는 동안 화면 캡처·녹화 감지(윈도).
//!
//! 윈도에는 "지금 화면을 찍거나 녹화하는 중"을 알려 주는 공개 API가 없어 두 가지로 짐작합니다.
//! - 클립보드에 그림이 새로 들어왔는데 넣은 쪽이 시스템(PrtScn)이거나 알려진 캡처 도구일 때: 스크린샷
//! - 알려진 캡처·녹화 프로그램이 새로 뜨거나 켜져 있을 때: 캡처 도구는 뜰 때 한 번, 녹화 프로그램은 켜져 있는 동안
//!
//! 감지는 화면을 가리고 알리는 용도입니다. 찍힌 화면에서 창 내용을 빼는 일은 캡처 제외(`set_content_protection`)가 합니다.
//! 프로세스 이름·클립보드 내용은 로그로 남기지 않습니다.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use parking_lot::Mutex;
use serde::Serialize;
use tauri::{AppHandle, Emitter};

use crate::daemon::MAIN_WINDOW;

/// 웹뷰로 보내는 이벤트 이름.
pub const EVENT: &str = "yangpa-capture";

/// 이 OS에서 감지를 지원하는지.
pub const SUPPORTED: bool = cfg!(windows);

/// 클립보드를 보는 간격. 프로세스 목록은 [`SCAN_EVERY`]번에 한 번 봅니다.
const TICK: Duration = Duration::from_millis(250);
const SCAN_EVERY: u32 = 2;

#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CaptureEvent {
    /// 스크린샷 한 번. `tool`은 알아낸 경우 프로그램 이름.
    Screenshot { tool: Option<&'static str> },
    /// 켜져 있는 녹화 프로그램 목록이 바뀜. 빈 목록이면 모두 꺼짐.
    Recording { tools: Vec<&'static str> },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    /// 트레이에 늘 떠 있을 수 있는 캡처 도구: 새로 뜰 때와 클립보드에 그림을 넣을 때만 알립니다.
    Screenshot,
    /// 녹화 프로그램: 켜져 있는 동안 계속 가립니다.
    Recording,
}

struct Tool {
    exe: &'static str,
    name: &'static str,
    kind: Kind,
}

const fn tool(exe: &'static str, name: &'static str, kind: Kind) -> Tool {
    Tool { exe, name, kind }
}

/// 알려진 캡처·녹화 프로그램(실행 파일 이름, 대소문자 무시).
const TOOLS: &[Tool] = &[
    // Win+Shift+S·PrtScn(윈도 11)이 띄우는 영역 캡처 화면과 캡처 도구 앱.
    tool("screenclippinghost.exe", "캡처 도구", Kind::Screenshot),
    tool("snippingtool.exe", "캡처 도구", Kind::Screenshot),
    tool("sharex.exe", "ShareX", Kind::Screenshot),
    tool("greenshot.exe", "Greenshot", Kind::Screenshot),
    tool("lightshot.exe", "Lightshot", Kind::Screenshot),
    tool("picpick.exe", "PicPick", Kind::Screenshot),
    tool("alcapture.exe", "알캡처", Kind::Screenshot),
    tool("obs64.exe", "OBS Studio", Kind::Recording),
    tool("obs32.exe", "OBS Studio", Kind::Recording),
    tool("obs.exe", "OBS Studio", Kind::Recording),
    tool("bdcam.exe", "반디캠", Kind::Recording),
    tool("ocam.exe", "오캠", Kind::Recording),
    tool("gomcam.exe", "곰캠", Kind::Recording),
    tool("camrecorder.exe", "Camtasia", Kind::Recording),
    tool("xsplit.core.exe", "XSplit", Kind::Recording),
    // Xbox Game Bar 녹화(Win+Alt+R) 동안만 뜨는 녹화 프로세스.
    tool("bcastdvr.exe", "Xbox Game Bar 녹화", Kind::Recording),
];

fn lookup(exe: &str) -> Option<&'static Tool> {
    TOOLS.iter().find(|t| t.exe.eq_ignore_ascii_case(exe))
}

/// 지난번 이후 클립보드가 바뀐 모습.
#[derive(Debug, Clone, Default)]
pub struct ClipboardChange {
    /// 그림 형식이 들어 있는지.
    pub image: bool,
    /// 넣은 창의 프로그램 실행 파일 이름. 넣은 창이 없으면(PrtScn) None.
    pub owner_exe: Option<String>,
    /// 넣은 쪽이 시스템(창 없음)인지.
    pub system_owner: bool,
}

/// 관찰을 이벤트로 바꾸는 상태 기계(OS 호출과 떨어뜨려 시험합니다).
#[derive(Default)]
pub struct Detector {
    primed: bool,
    shot_tools: BTreeSet<&'static str>,
    recording: Vec<&'static str>,
}

impl Detector {
    /// 프로세스 목록이 바뀌었을 때. 처음 한 번은 이미 떠 있는 캡처 도구를 알리지 않고 녹화 프로그램만 알립니다.
    pub fn on_processes<S: AsRef<str>>(&mut self, exes: &[S]) -> Vec<CaptureEvent> {
        let mut shots = BTreeSet::new();
        let mut rec = BTreeSet::new();
        for t in exes.iter().filter_map(|e| lookup(e.as_ref())) {
            match t.kind {
                Kind::Screenshot => shots.insert(t.name),
                Kind::Recording => rec.insert(t.name),
            };
        }
        let mut out = Vec::new();
        if self.primed
            && let Some(new) = shots.difference(&self.shot_tools).next()
        {
            out.push(CaptureEvent::Screenshot { tool: Some(*new) });
        }
        let rec: Vec<_> = rec.into_iter().collect();
        if rec != self.recording && (self.primed || !rec.is_empty()) {
            out.push(CaptureEvent::Recording { tools: rec.clone() });
        }
        self.recording = rec;
        self.shot_tools = shots;
        self.primed = true;
        out
    }

    /// 클립보드가 바뀌었을 때. 다른 앱에서 그림을 복사한 것은 무시하고, 시스템(PrtScn)이나 캡처 도구가 넣은 그림만 셉니다.
    pub fn on_clipboard(&mut self, c: &ClipboardChange) -> Option<CaptureEvent> {
        if !c.image {
            return None;
        }
        if c.system_owner {
            return Some(CaptureEvent::Screenshot { tool: None });
        }
        let t = lookup(c.owner_exe.as_deref()?)?;
        Some(CaptureEvent::Screenshot { tool: Some(t.name) })
    }
}

/// 감지 스레드 하나를 켜고 끕니다(휘발성 채널을 보는 동안만).
#[derive(Default)]
pub struct CaptureWatch {
    stop: Mutex<Option<Arc<AtomicBool>>>,
}

impl CaptureWatch {
    /// 켜거나 끕니다. 지원하는 OS면 true.
    pub fn set(&self, app: &AppHandle, enabled: bool) -> bool {
        let mut cur = self.stop.lock();
        if let Some(flag) = cur.take() {
            flag.store(true, Ordering::Relaxed);
        }
        if !SUPPORTED || !enabled {
            return SUPPORTED;
        }
        let flag = Arc::new(AtomicBool::new(false));
        let app = app.clone();
        let stop = flag.clone();
        let spawned = std::thread::Builder::new()
            .name("capture-watch".into())
            .spawn(move || run(&app, &stop));
        match spawned {
            Ok(_) => {
                *cur = Some(flag);
                true
            }
            Err(e) => {
                tracing::warn!("캡처 감지를 시작하지 못했습니다: {e}");
                false
            }
        }
    }
}

fn run(app: &AppHandle, stop: &AtomicBool) {
    let mut det = Detector::default();
    let mut seq = os::clipboard_seq();
    let mut procs = os::processes();
    let emit = |ev: CaptureEvent| {
        if let Err(e) = app.emit_to(MAIN_WINDOW, EVENT, &ev) {
            tracing::warn!("캡처 감지 이벤트를 보내지 못했습니다: {e}");
        }
    };
    for ev in det.on_processes(&names(&procs)) {
        emit(ev);
    }
    let mut tick = 0u32;
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(TICK);
        if stop.load(Ordering::Relaxed) {
            break;
        }
        tick = tick.wrapping_add(1);
        if tick.is_multiple_of(SCAN_EVERY) {
            procs = os::processes();
            for ev in det.on_processes(&names(&procs)) {
                emit(ev);
            }
        }
        let now = os::clipboard_seq();
        if now != seq {
            seq = now;
            if let Some(ev) = det.on_clipboard(&os::clipboard_change(&mut procs)) {
                emit(ev);
            }
        }
    }
}

fn names(procs: &[(u32, String)]) -> Vec<&str> {
    procs.iter().map(|(_, n)| n.as_str()).collect()
}

#[cfg(windows)]
mod os {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::DataExchange::{
        GetClipboardOwner, GetClipboardSequenceNumber, IsClipboardFormatAvailable,
    };
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::Ole::{CF_BITMAP, CF_DIB, CF_DIBV5};
    use windows_sys::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;

    use super::ClipboardChange;

    pub fn clipboard_seq() -> u32 {
        // SAFETY: 인자 없는 조회 함수입니다.
        unsafe { GetClipboardSequenceNumber() }
    }

    /// 실행 중인 프로세스(pid, 실행 파일 이름).
    pub fn processes() -> Vec<(u32, String)> {
        let mut out = Vec::new();
        // SAFETY: 스냅숏 핸들은 아래에서 닫고, 항목 구조체는 dwSize를 채워 넘깁니다.
        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            if snap == INVALID_HANDLE_VALUE || snap.is_null() {
                return out;
            }
            let mut e: PROCESSENTRY32W = std::mem::zeroed();
            e.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
            let mut ok = Process32FirstW(snap, &mut e);
            while ok != 0 {
                let len = e
                    .szExeFile
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(e.szExeFile.len());
                out.push((
                    e.th32ProcessID,
                    String::from_utf16_lossy(&e.szExeFile[..len]),
                ));
                ok = Process32NextW(snap, &mut e);
            }
            CloseHandle(snap);
        }
        out
    }

    /// 클립보드를 열지 않고 형식과 넣은 창만 봅니다(내용은 읽지 않음).
    pub fn clipboard_change(procs: &mut Vec<(u32, String)>) -> ClipboardChange {
        // SAFETY: 형식 확인과 소유 창 조회는 클립보드를 열지 않는 읽기 전용 호출입니다.
        unsafe {
            let image = [CF_DIB, CF_DIBV5, CF_BITMAP]
                .iter()
                .any(|&f| IsClipboardFormatAvailable(u32::from(f)) != 0);
            if !image {
                return ClipboardChange::default();
            }
            let owner = GetClipboardOwner();
            if owner.is_null() {
                return ClipboardChange {
                    image,
                    owner_exe: None,
                    system_owner: true,
                };
            }
            let mut pid = 0u32;
            GetWindowThreadProcessId(owner, &mut pid);
            // 방금 뜬 캡처 도구일 수 있어, 목록에 없으면 한 번 새로 봅니다.
            if !procs.iter().any(|(p, _)| *p == pid) {
                *procs = processes();
            }
            let owner_exe = procs
                .iter()
                .find(|(p, _)| *p == pid)
                .map(|(_, n)| n.clone());
            ClipboardChange {
                image,
                owner_exe,
                system_owner: false,
            }
        }
    }
}

#[cfg(not(windows))]
mod os {
    use super::ClipboardChange;

    pub fn clipboard_seq() -> u32 {
        0
    }

    pub fn processes() -> Vec<(u32, String)> {
        Vec::new()
    }

    pub fn clipboard_change(_procs: &mut Vec<(u32, String)>) -> ClipboardChange {
        ClipboardChange::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shot(tool: Option<&'static str>) -> CaptureEvent {
        CaptureEvent::Screenshot { tool }
    }

    fn rec(tools: &[&'static str]) -> CaptureEvent {
        CaptureEvent::Recording {
            tools: tools.to_vec(),
        }
    }

    #[test]
    fn recording_tools_block_while_running_including_at_start() {
        let mut d = Detector::default();
        assert_eq!(
            d.on_processes(&["explorer.exe", "OBS64.EXE"]),
            vec![rec(&["OBS Studio"])]
        );
        assert!(d.on_processes(&["obs64.exe", "explorer.exe"]).is_empty());
        assert_eq!(
            d.on_processes(&["obs64.exe", "bdcam.exe"]),
            vec![rec(&["OBS Studio", "반디캠"])]
        );
        assert_eq!(d.on_processes(&["explorer.exe"]), vec![rec(&[])]);
    }

    #[test]
    fn nothing_running_at_start_sends_nothing() {
        let mut d = Detector::default();
        assert!(d.on_processes(&["explorer.exe"]).is_empty());
    }

    #[test]
    fn screenshot_tools_count_only_when_they_appear() {
        let mut d = Detector::default();
        // 이미 트레이에 떠 있던 도구는 알리지 않습니다.
        assert!(d.on_processes(&["ShareX.exe"]).is_empty());
        assert!(d.on_processes(&["ShareX.exe"]).is_empty());
        assert_eq!(
            d.on_processes(&["ShareX.exe", "ScreenClippingHost.exe"]),
            vec![shot(Some("캡처 도구"))]
        );
        assert!(
            d.on_processes(&["ShareX.exe", "ScreenClippingHost.exe"])
                .is_empty()
        );
        assert!(d.on_processes(&["ShareX.exe"]).is_empty());
        assert_eq!(
            d.on_processes(&["ShareX.exe", "SnippingTool.exe"]),
            vec![shot(Some("캡처 도구"))]
        );
    }

    #[test]
    fn clipboard_images_from_system_or_capture_tools_only() {
        let mut d = Detector::default();
        let prtscn = ClipboardChange {
            image: true,
            owner_exe: None,
            system_owner: true,
        };
        assert_eq!(d.on_clipboard(&prtscn), Some(shot(None)));
        let tool = ClipboardChange {
            image: true,
            owner_exe: Some("SnippingTool.exe".into()),
            system_owner: false,
        };
        assert_eq!(d.on_clipboard(&tool), Some(shot(Some("캡처 도구"))));
        // 브라우저에서 그림을 복사하거나 글자만 복사한 것은 캡처가 아닙니다.
        let browser = ClipboardChange {
            image: true,
            owner_exe: Some("chrome.exe".into()),
            system_owner: false,
        };
        assert_eq!(d.on_clipboard(&browser), None);
        let text = ClipboardChange {
            image: false,
            owner_exe: None,
            system_owner: true,
        };
        assert_eq!(d.on_clipboard(&text), None);
        let unknown = ClipboardChange {
            image: true,
            owner_exe: None,
            system_owner: false,
        };
        assert_eq!(d.on_clipboard(&unknown), None);
    }

    #[test]
    fn event_shape_for_the_webview() {
        assert_eq!(
            serde_json::to_value(shot(None)).unwrap(),
            serde_json::json!({ "kind": "screenshot", "tool": null })
        );
        assert_eq!(
            serde_json::to_value(rec(&["OBS Studio"])).unwrap(),
            serde_json::json!({ "kind": "recording", "tools": ["OBS Studio"] })
        );
    }
}
