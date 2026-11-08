//! 화면공유 인코더(ffmpeg). 통화의 화면공유를 웹뷰 WebRTC 대신 고른 인코더(AOM AV1, SVT-AV1, NVENC AV1·H.264·HEVC)로
//! 만들 때 씁니다. 캡처·인코딩은 실행 파일 옆 `ffmpeg` 폴더의 `ffmpeg.exe`(LGPL 빌드, x264 없음)를 따로 띄워 하고, 그
//! 표준 출력(FLV)에서 프레임을 꺼내 웹뷰에 넘깁니다(웹뷰가 프레임 암호화한 뒤 데이터 채널로 나름, voice/native-screen.ts).
//!
//! 따로 띄우는 까닭: ffmpeg가 죽어도 통화 키를 가진 앱 프로세스는 그대로이고, 링크하지 않으므로 LGPL 조건이 단순합니다.
//! 앱이 비정상으로 끝나도 캡처가 남지 않게 잡 객체(닫히면 자식 종료)에 넣습니다. 띄우기 전에 폴더의 파일이 배포본
//! (scripts/ffmpeg-files.txt의 sha256)과 같고 목록에 없는 파일이 없는지 봅니다(포터블 폴더는 사용자가 쓸 수 있는 곳이고,
//! ffmpeg는 NVENC 같은 DLL을 자기 폴더에서 먼저 찾으므로 끼워 넣은 DLL도 막아야 함). 확인한 파일은 쓰기·지우기를 막고
//! 연 채 ffmpeg가 끝날 때까지 쥐고 있어, 확인과 실행 사이에 바뀌지 않습니다.
//!
//! 웹뷰 명령: `screen_status`, `screen_sources`, `screen_start`(채널로 바이트 메시지), `screen_stop`. 화면 소리는 [`audio`].
//! 채널 메시지: `[1][코덱 u8: 1 AV1, 2 H.264, 3 HEVC][가로 u16 LE][세로 u16 LE][설정 레코드]`,
//! `[2][플래그 u8: 1 키][시각 ms u32 LE][프레임]`, `[3][끝난 까닭 UTF-8(정상 종료면 비어 있음)]`.

pub mod args;
pub mod audio;
pub mod flv;
pub mod sources;
#[cfg(windows)]
mod thumbs;

use std::collections::VecDeque;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::Serialize;
use tauri::State;
use tauri::ipc::{Channel, InvokeResponseBody};

use crate::error::CmdError;
use args::{EncodeSettings, Encoder, Target};
use flv::{Event, FlvReader};

/// ffmpeg 폴더에 넣을 파일과 sha256. 빌드 스크립트와 같은 파일을 씁니다.
const PINNED: &str = include_str!("../../../../../scripts/ffmpeg-files.txt");

const EXE: &str = "ffmpeg.exe";
/// 오류 메시지에 붙일 ffmpeg 표준 오류의 끝부분.
const STDERR_TAIL: usize = 2048;
const PROBE_TIMEOUT: Duration = Duration::from_secs(20);

/// ffmpeg 폴더: `YANGPA_FFMPEG_DIR` → 실행 파일 옆 `ffmpeg`(설치본·포터블) → 데이터 경로 옆 `ffmpeg`.
pub fn ffmpeg_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("YANGPA_FFMPEG_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(d);
    }
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|p| p.join("ffmpeg")))
        && dir.is_dir()
    {
        return dir;
    }
    match yangpa_rpc::endpoint::data_dir() {
        Ok(data) => data
            .parent()
            .map(|p| p.join("ffmpeg"))
            .unwrap_or_else(|| data.join("ffmpeg")),
        Err(_) => PathBuf::from("ffmpeg"),
    }
}

/// 빠졌거나 배포본과 다르거나 목록에 없는 파일(폴더 기준, 까닭은 뒤에 표시). 상태 표시용이라 해시는 캐시를 씁니다.
/// 실제로 띄울 때는 [`hold`]가 붙잡은 파일을 다시 해시합니다.
pub fn missing_files(dir: &Path) -> Vec<String> {
    let mut out: Vec<String> = crate::pinned::paths(PINNED)
        .filter_map(|rel| {
            let path = dir.join(rel);
            if !path.is_file() {
                return Some(rel.to_string());
            }
            let want = crate::pinned::sha256_of(PINNED, rel)?;
            match crate::pinned::sha256_cached(&path) {
                Ok(h) if h == want => None,
                _ => Some(format!("{rel}(배포본과 다름)")),
            }
        })
        .collect();
    out.extend(unlisted(dir));
    out
}

/// 폴더 바로 아래의 목록에 없는 파일. 하위 폴더는 DLL을 찾는 곳이 아니라 보지 않고, 안내문(`.txt`, 배포 README·라이선스
/// 고지)은 읽어 들일 코드가 아니라 둡니다.
fn unlisted(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|e| !e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.to_ascii_lowercase().ends_with(".txt") {
                return None;
            }
            // 윈도 파일 이름은 대소문자를 가리지 않습니다.
            let listed = crate::pinned::paths(PINNED).any(|p| p.eq_ignore_ascii_case(&name));
            (!listed).then(|| format!("{name}(배포본에 없는 파일)"))
        })
        .collect()
}

/// 확인한 ffmpeg 파일을 쓰기·지우기를 막은 채 연 핸들. 쥐고 있는 동안 파일을 바꾸거나 바꿔치기할 수 없습니다.
pub struct Held(#[allow(dead_code)] Vec<std::fs::File>);

fn open_locked(path: &Path) -> std::io::Result<std::fs::File> {
    let mut o = std::fs::OpenOptions::new();
    o.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // 읽기만 함께 허용: ffmpeg와 DLL을 읽어 들이는 것은 되고, 쓰기·이름 바꾸기·지우기는 실패합니다.
        const FILE_SHARE_READ: u32 = 1;
        o.share_mode(FILE_SHARE_READ);
    }
    o.open(path)
}

/// 목록의 파일을 잠가 열고 캐시 없이 해시해 봅니다. 하나라도 빠졌거나 다르거나 목록에 없는 파일이 있으면 그 목록.
fn hold(dir: &Path) -> std::result::Result<Held, Vec<String>> {
    let mut bad = Vec::new();
    let mut files = Vec::new();
    for rel in crate::pinned::paths(PINNED) {
        let Ok(mut f) = open_locked(&dir.join(rel)) else {
            bad.push(rel.to_string());
            continue;
        };
        let want = crate::pinned::sha256_of(PINNED, rel);
        match crate::pinned::sha256_reader(&mut f) {
            Ok(h) if Some(h.as_str()) == want => files.push(f),
            _ => bad.push(format!("{rel}(배포본과 다름)")),
        }
    }
    // 잠근 뒤에 봐야 확인과 실행 사이에 끼워 넣을 틈이 줄어듭니다.
    bad.extend(unlisted(dir));
    if bad.is_empty() {
        Ok(Held(files))
    } else {
        Err(bad)
    }
}

fn ready_dir() -> Result<(PathBuf, Held), CmdError> {
    if !cfg!(windows) {
        return Err(CmdError::invalid_params(
            "화면공유 인코더는 윈도에서만 쓸 수 있습니다",
        ));
    }
    let dir = ffmpeg_dir();
    match hold(&dir) {
        Ok(held) => Ok((dir, held)),
        Err(bad) => Err(CmdError::invalid_params(format!(
            "ffmpeg 폴더에 빠졌거나 바뀌었거나 배포본에 없는 파일이 있습니다: {}",
            bad.join(", ")
        ))),
    }
}

fn command(dir: &Path) -> Command {
    let mut cmd = Command::new(dir.join(EXE));
    // DLL은 실행 파일 옆에서 찾습니다(현재 폴더도 같은 곳으로).
    // SVT-AV1은 ffmpeg 로그 수준과 따로 정보 줄을 표준 오류에 써서, 오류만 남깁니다(끝난 까닭을 가리지 않게).
    cmd.current_dir(dir)
        .env("SVT_LOG", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// 자식 프로세스를 잡 객체에 넣어, 앱이 어떻게 끝나든(이 핸들이 닫히면) 함께 끝나게 합니다.
#[cfg(windows)]
struct Job(windows_sys::Win32::Foundation::HANDLE);

// SAFETY: 잡 핸들은 어느 스레드에서 닫아도 됩니다.
#[cfg(windows)]
unsafe impl Send for Job {}

#[cfg(windows)]
impl Job {
    fn for_child(child: &Child) -> Option<Job> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject,
        };
        // SAFETY: 이름 없는 잡을 만들고, 크기를 맞춘 구조체로 설정한 뒤 살아 있는 자식 핸들을 넣습니다.
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return None;
            }
            let job = Job(job);
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const _,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            ) == 0
            {
                return None;
            }
            if AssignProcessToJobObject(job.0, child.as_raw_handle() as _) == 0 {
                return None;
            }
            Some(job)
        }
    }
}

#[cfg(windows)]
impl Drop for Job {
    fn drop(&mut self) {
        // SAFETY: CreateJobObjectW가 준 핸들을 한 번만 닫습니다.
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0) };
    }
}

/// 띄운 ffmpeg와, 그것이 끝날 때까지 쥐고 있을 것들(잡 객체, 잠근 파일).
struct Spawned {
    child: Child,
    #[cfg(windows)]
    _job: Job,
}

/// ffmpeg를 띄우고 잡 객체에 넣습니다. 넣지 못하면 앱이 죽었을 때 캡처가 남을 수 있으니 바로 끝내고 실패로 돌립니다.
fn spawn(cmd: &mut Command) -> std::result::Result<Spawned, String> {
    #[cfg_attr(not(windows), allow(unused_mut))]
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("ffmpeg를 띄우지 못했습니다: {e}"))?;
    #[cfg(windows)]
    let Some(job) = Job::for_child(&child) else {
        let _ = child.kill();
        let _ = child.wait();
        return Err("ffmpeg를 앱과 함께 끝나게 묶지 못했습니다".into());
    };
    Ok(Spawned {
        child,
        #[cfg(windows)]
        _job: job,
    })
}

struct Running {
    id: u32,
    child: Arc<Mutex<Child>>,
    #[cfg(windows)]
    _job: Job,
    _held: Held,
}

impl Running {
    fn kill(&self) {
        let mut c = self.child.lock();
        let _ = c.kill();
        let _ = c.wait();
    }
}

#[derive(Clone, Serialize)]
pub struct EncoderStatus {
    id: &'static str,
    available: bool,
    /// 쓸 수 없는 까닭(드라이버·GPU 없음 등).
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Serialize)]
pub struct ScreenStatus {
    dir: String,
    /// 빠졌거나 배포본과 다른 파일. 비었으면 쓸 수 있음.
    missing: Vec<String>,
    encoders: Vec<EncoderStatus>,
}

#[derive(Default)]
pub struct Screen {
    current: Mutex<Option<Running>>,
    next_id: Mutex<u32>,
    /// 인코더 확인 결과(앱을 켜 둔 동안 GPU가 바뀌는 일은 드물어 한 번만).
    probes: Mutex<Option<Vec<EncoderStatus>>>,
}

impl Screen {
    /// 웹뷰를 다시 읽을 때: 받을 채널이 사라졌으니 캡처를 멈춥니다.
    pub fn stop_all(&self) {
        self.stop(None);
    }

    fn stop(&self, id: Option<u32>) {
        let mut g = self.current.lock();
        if id.is_some_and(|id| g.as_ref().is_some_and(|r| r.id != id)) {
            return;
        }
        if let Some(r) = g.take() {
            r.kill();
        }
    }

    fn probe(&self, dir: &Path) -> Vec<EncoderStatus> {
        if let Some(p) = self.probes.lock().as_ref() {
            return p.clone();
        }
        // 확인용 ffmpeg도 같은 확인을 거친 파일로만 띄웁니다(끝날 때까지 잠가 둠).
        let _held = match hold(dir) {
            Ok(h) => h,
            Err(bad) => {
                let msg = format!("ffmpeg 폴더가 바뀌었습니다: {}", bad.join(", "));
                return Encoder::ALL
                    .iter()
                    .map(|e| EncoderStatus {
                        id: e.id(),
                        available: false,
                        error: Some(msg.clone()),
                    })
                    .collect();
            }
        };
        let handles: Vec<_> = Encoder::ALL
            .iter()
            .map(|&e| {
                let dir = dir.to_path_buf();
                std::thread::spawn(move || probe_one(&dir, e))
            })
            .collect();
        let out: Vec<EncoderStatus> = handles
            .into_iter()
            .zip(Encoder::ALL)
            .map(|(h, e)| {
                h.join().unwrap_or_else(|_| EncoderStatus {
                    id: e.id(),
                    available: false,
                    error: Some("확인이 멈췄습니다".into()),
                })
            })
            .collect();
        *self.probes.lock() = Some(out.clone());
        out
    }
}

/// 짧게 인코딩해 보고 결과를 봅니다(NVENC는 NVIDIA GPU·드라이버가 없거나 세대가 맞지 않으면 실패, 예: AV1은 RTX 40 이상).
fn probe_one(dir: &Path, e: Encoder) -> EncoderStatus {
    let fail = |msg: String| EncoderStatus {
        id: e.id(),
        available: false,
        error: Some(msg),
    };
    let mut cmd = command(dir);
    cmd.args(args::probe_args(e)).stderr(Stdio::piped());
    let Spawned {
        mut child,
        #[cfg(windows)]
        _job,
    } = match spawn(&mut cmd) {
        Ok(s) => s,
        Err(err) => return fail(err),
    };
    let stderr = child.stderr.take();
    let tail = std::thread::spawn(move || read_tail(stderr));
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) if start.elapsed() < PROBE_TIMEOUT => {
                std::thread::sleep(Duration::from_millis(50))
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let tail = tail.join().unwrap_or_default();
    match status {
        Some(s) if s.success() => EncoderStatus {
            id: e.id(),
            available: true,
            error: None,
        },
        Some(_) => fail(first_line(&tail).unwrap_or_else(|| "이 기기에서 쓸 수 없습니다".into())),
        None => fail("확인 시간이 지났습니다".into()),
    }
}

fn read_tail(stderr: Option<std::process::ChildStderr>) -> String {
    let Some(mut s) = stderr else {
        return String::new();
    };
    let mut tail: VecDeque<u8> = VecDeque::with_capacity(STDERR_TAIL);
    let mut buf = [0u8; 1024];
    while let Ok(n) = s.read(&mut buf) {
        if n == 0 {
            break;
        }
        for &b in &buf[..n] {
            if tail.len() == STDERR_TAIL {
                tail.pop_front();
            }
            tail.push_back(b);
        }
    }
    String::from_utf8_lossy(&tail.into_iter().collect::<Vec<u8>>()).into_owned()
}

/// ffmpeg 오류 출력에서 보여 줄 줄(마지막 비지 않은 줄).
fn first_line(s: &str) -> Option<String> {
    s.lines()
        .map(str::trim)
        .rfind(|l| !l.is_empty())
        .map(|l| l.chars().take(300).collect())
}

#[tauri::command]
pub async fn screen_status(
    screen: State<'_, Arc<Screen>>,
) -> std::result::Result<ScreenStatus, CmdError> {
    let screen = screen.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let dir = ffmpeg_dir();
        let unavailable = |msg: &str| {
            Encoder::ALL
                .iter()
                .map(|e| EncoderStatus {
                    id: e.id(),
                    available: false,
                    error: Some(msg.to_string()),
                })
                .collect()
        };
        if !cfg!(windows) {
            return ScreenStatus {
                dir: dir.to_string_lossy().into_owned(),
                missing: Vec::new(),
                encoders: unavailable("윈도에서만 쓸 수 있습니다"),
            };
        }
        let missing = missing_files(&dir);
        let encoders = if missing.is_empty() {
            screen.probe(&dir)
        } else {
            unavailable("ffmpeg 폴더가 없거나 바뀌었습니다")
        };
        ScreenStatus {
            dir: dir.to_string_lossy().into_owned(),
            missing,
            encoders,
        }
    })
    .await
    .map_err(|_| CmdError::internal("ffmpeg 확인이 멈췄습니다"))
}

#[tauri::command]
pub async fn screen_sources() -> std::result::Result<Vec<sources::Source>, CmdError> {
    tauri::async_runtime::spawn_blocking(|| {
        #[allow(unused_mut)]
        let mut list = sources::list();
        #[cfg(windows)]
        list.iter_mut().for_each(thumbs::fill);
        list
    })
    .await
    .map_err(|_| CmdError::internal("화면 목록을 읽지 못했습니다"))
}

fn config_message(codec: flv::VideoCodec, size: (u32, u32), record: &[u8]) -> Vec<u8> {
    let mut m = Vec::with_capacity(6 + record.len());
    m.push(1);
    m.push(codec as u8);
    m.extend_from_slice(&(size.0.min(u16::MAX as u32) as u16).to_le_bytes());
    m.extend_from_slice(&(size.1.min(u16::MAX as u32) as u16).to_le_bytes());
    m.extend_from_slice(record);
    m
}

fn frame_message(key: bool, ts_ms: u32, data: &[u8]) -> Vec<u8> {
    let mut m = Vec::with_capacity(6 + data.len());
    m.push(2);
    m.push(u8::from(key));
    m.extend_from_slice(&ts_ms.to_le_bytes());
    m.extend_from_slice(data);
    m
}

fn end_message(reason: &str) -> Vec<u8> {
    let mut m = vec![3];
    m.extend_from_slice(reason.as_bytes());
    m
}

/// 화면공유를 시작합니다. 이미 있으면 그것을 멈추고 새로 띄웁니다(설정을 바꿀 때도 다시 띄움: CLI라 도중에 못 바꿈).
#[tauri::command]
pub async fn screen_start(
    screen: State<'_, Arc<Screen>>,
    source_kind: String,
    source_id: String,
    max_height: Option<u32>,
    settings: EncodeSettings,
    on_data: Channel<InvokeResponseBody>,
) -> std::result::Result<u32, CmdError> {
    let screen = screen.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        start(
            &screen,
            &source_kind,
            &source_id,
            max_height,
            &settings,
            on_data,
        )
    })
    .await
    .map_err(|_| CmdError::internal("화면공유를 시작하지 못했습니다"))?
}

fn start(
    screen: &Arc<Screen>,
    source_kind: &str,
    source_id: &str,
    max_height: Option<u32>,
    settings: &EncodeSettings,
    on_data: Channel<InvokeResponseBody>,
) -> std::result::Result<u32, CmdError> {
    let (dir, held) = ready_dir()?;
    let src = sources::find(source_kind, source_id)
        .ok_or_else(|| CmdError::invalid_params("고른 화면이나 창이 없어졌습니다"))?;
    let handle: u64 = src
        .id
        .parse()
        .map_err(|_| CmdError::invalid_params("화면 핸들이 잘못됐습니다"))?;
    let target = match src.kind {
        sources::SourceKind::Monitor => Target::Monitor(handle),
        sources::SourceKind::Window => Target::Window(handle),
    };
    let size = args::output_size(
        (src.width, src.height),
        max_height.filter(|h| *h >= 144),
        settings.encoder,
    );
    screen.stop(None);
    let mut cmd = command(&dir);
    cmd.args(args::ffmpeg_args(settings, target, size))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let Spawned {
        mut child,
        #[cfg(windows)]
            _job: job,
    } = spawn(&mut cmd).map_err(CmdError::internal)?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let child = Arc::new(Mutex::new(child));
    let id = {
        let mut n = screen.next_id.lock();
        *n = n.wrapping_add(1).max(1);
        *n
    };
    *screen.current.lock() = Some(Running {
        id,
        child: child.clone(),
        #[cfg(windows)]
        _job: job,
        _held: held,
    });
    let tail = std::thread::spawn(move || read_tail(stderr));
    let codec = settings.encoder.codec();
    let screen = screen.clone();
    std::thread::spawn(move || {
        let reason = pump(stdout, codec, size, &on_data);
        // 읽기를 그만뒀으면 ffmpeg가 가득 찬 파이프에 막혀 캡처를 쥔 채 남지 않게 먼저 끝냅니다.
        if reason.is_some() {
            let _ = child.lock().kill();
        }
        let err = tail.join().unwrap_or_default();
        let exit = child.lock().wait().ok();
        // 멈춘 것(screen_stop·새 공유)이면 조용히, 스스로 끝났으면(창을 닫음·인코더 오류) 까닭을 알립니다.
        let stopped = screen.current.lock().as_ref().is_none_or(|r| r.id != id);
        if !stopped {
            let msg = match reason {
                Some(r) => r,
                None if exit.is_some_and(|s| s.success()) => String::new(),
                None => first_line(&err).unwrap_or_else(|| "ffmpeg가 끝났습니다".into()),
            };
            let _ = on_data.send(InvokeResponseBody::Raw(end_message(&msg)));
            screen.stop(Some(id));
        }
    });
    Ok(id)
}

/// 표준 출력(FLV)을 읽어 채널로 넘깁니다. 읽기를 마치면 None, 스트림이 잘못됐거나 웹뷰가 받지 않으면 그 까닭.
fn pump(
    stdout: Option<std::process::ChildStdout>,
    codec: flv::VideoCodec,
    size: (u32, u32),
    out: &Channel<InvokeResponseBody>,
) -> Option<String> {
    let mut stdout = stdout?;
    let mut reader = FlvReader::default();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = match stdout.read(&mut buf) {
            Ok(0) | Err(_) => return None,
            Ok(n) => n,
        };
        let events = match reader.push(&buf[..n]) {
            Ok(e) => e,
            Err(e) => return Some(format!("ffmpeg 출력을 읽지 못했습니다: {}", e.0)),
        };
        for ev in events {
            let msg = match ev {
                Event::Config { codec: c, record } if c == codec => {
                    config_message(c, size, &record)
                }
                Event::Frame {
                    codec: c,
                    key,
                    ts_ms,
                    data,
                } if c == codec => frame_message(key, ts_ms, &data),
                _ => continue,
            };
            if out.send(InvokeResponseBody::Raw(msg)).is_err() {
                return Some("화면이 받지 않습니다".into());
            }
        }
    }
}

#[tauri::command]
pub fn screen_stop(screen: State<'_, Arc<Screen>>, id: Option<u32>) {
    screen.stop(id);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_list_has_exe_and_dlls() {
        let files: Vec<_> = crate::pinned::paths(PINNED).collect();
        assert!(files.contains(&EXE));
        assert!(files.iter().any(|f| f.starts_with("avcodec-")));
        assert!(files.contains(&"LICENSE.txt"));
        for f in files {
            assert_eq!(
                crate::pinned::sha256_of(PINNED, f).unwrap().len(),
                64,
                "{f}"
            );
        }
    }

    /// 저장소의 vendor/ffmpeg가 목록과 같은지(배포 빌드가 복사해 넣는 파일).
    #[test]
    fn vendored_build_matches_list() {
        let vendor = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../vendor/ffmpeg");
        assert!(
            missing_files(&vendor).is_empty(),
            "{:?}",
            missing_files(&vendor)
        );
    }

    #[test]
    fn missing_and_changed_files() {
        let dir = tempfile::tempdir().unwrap();
        let all = missing_files(dir.path());
        assert!(all.contains(&EXE.to_string()));
        std::fs::write(dir.path().join(EXE), b"not ffmpeg").unwrap();
        let m = missing_files(dir.path());
        assert!(m.contains(&format!("{EXE}(배포본과 다름)")), "{m:?}");
        assert!(hold(dir.path()).is_err());
    }

    #[test]
    fn rejects_planted_dlls() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("nvEncodeAPI64.dll"), b"x").unwrap();
        std::fs::write(dir.path().join("README.txt"), b"x").unwrap();
        std::fs::write(dir.path().join("FFMPEG.EXE"), b"x").unwrap();
        std::fs::create_dir(dir.path().join("licenses")).unwrap();
        let m = unlisted(dir.path());
        assert_eq!(m, vec!["nvEncodeAPI64.dll(배포본에 없는 파일)".to_string()]);
        assert!(missing_files(dir.path()).contains(&m[0]));
        let Err(bad) = hold(dir.path()) else {
            panic!("목록에 없는 DLL이 있는데 통과함");
        };
        assert!(bad.contains(&m[0]), "{bad:?}");
    }

    #[test]
    fn messages() {
        assert_eq!(
            config_message(flv::VideoCodec::Hevc, (1920, 1080), &[9]),
            vec![1, 3, 0x80, 0x07, 0x38, 0x04, 9]
        );
        assert_eq!(frame_message(true, 258, &[7]), vec![2, 1, 2, 1, 0, 0, 7]);
        assert_eq!(end_message("x"), vec![3, b'x']);
        assert_eq!(first_line("a\n\n b \n\n").as_deref(), Some("b"));
    }
}
