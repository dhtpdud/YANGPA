//! 화면공유 소리: 윈도의 WASAPI 프로세스 루프백(윈도 10 2004 이상). 창을 공유하면 그 창의 프로세스(와 자식) 소리만, 모니터나
//! 웹뷰가 고른 화면을 공유하면 이 앱 프로세스 트리(웹뷰가 내는 통화 소리 포함)를 뺀 모든 소리를 잡습니다: 시청자가 자기
//! 목소리를 되돌려 듣지 않게. 소리는 메모리로만 지나 웹뷰로 갑니다(웹뷰가 트랙으로 만들어 프레임 암호화해 보냄,
//! voice/screen-audio.ts).
//!
//! 웹뷰 명령: `screen_audio_start`(채널로 바이트 메시지, 공유 번호), `screen_audio_stop`.
//! 채널 메시지: `[1][48kHz 스테레오 f32 LE 인터리브 PCM]`(10ms 남짓씩), `[3][끝난 까닭 UTF-8]`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::SyncSender;
use std::time::Duration;

use parking_lot::Mutex;
use tauri::State;
use tauri::ipc::{Channel, InvokeResponseBody};

use crate::error::CmdError;

pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: u16 = 2;
/// 한 메시지에 모으는 최소 프레임(10ms). 너무 잘게 보내면 채널 메시지가 초당 수백 개가 됩니다.
#[cfg(windows)]
const CHUNK_FRAMES: usize = SAMPLE_RATE as usize / 100;
/// 여는 데 걸리는 시간 상한(활성화는 비동기로 끝남).
const OPEN_TIMEOUT: Duration = Duration::from_secs(5);

/// 지금 잡는 소리(공유 번호, 멈춤 표시). 한 번에 하나만.
#[derive(Default)]
pub struct ScreenAudio {
    current: Mutex<Option<(u32, Arc<AtomicBool>)>>,
    next_id: Mutex<u32>,
}

impl ScreenAudio {
    /// `id`가 None이면 무엇이든, 아니면 그 번호일 때만 멈춥니다(늦게 온 멈춤이 새 공유를 끊지 않게).
    fn stop(&self, id: Option<u32>) {
        let mut cur = self.current.lock();
        if cur
            .as_ref()
            .is_some_and(|(i, _)| id.is_none_or(|id| id == *i))
            && let Some((_, stop)) = cur.take()
        {
            stop.store(true, Ordering::Relaxed);
        }
    }
}

#[cfg(windows)]
fn data_message(pcm: &[u8]) -> Vec<u8> {
    let mut m = Vec::with_capacity(1 + pcm.len());
    m.push(1);
    m.extend_from_slice(pcm);
    m
}

fn end_message(reason: &str) -> Vec<u8> {
    let mut m = vec![3];
    m.extend_from_slice(reason.as_bytes());
    m
}

/// 화면 소리를 잡기 시작합니다. `window`는 공유하는 창의 핸들(없으면 이 앱을 뺀 시스템 소리).
#[tauri::command]
pub async fn screen_audio_start(
    audio: State<'_, Arc<ScreenAudio>>,
    window: Option<String>,
    on_data: Channel<InvokeResponseBody>,
) -> std::result::Result<u32, CmdError> {
    let audio = audio.inner().clone();
    tauri::async_runtime::spawn_blocking(move || start(&audio, window.as_deref(), on_data))
        .await
        .map_err(|_| CmdError::internal("화면 소리를 열지 못했습니다"))?
}

fn start(
    audio: &Arc<ScreenAudio>,
    window: Option<&str>,
    on_data: Channel<InvokeResponseBody>,
) -> std::result::Result<u32, CmdError> {
    // 창 핸들은 지금 목록에 있는 창만 받습니다(웹뷰가 보낸 숫자를 그대로 믿지 않음).
    let pid = match window {
        Some(id) => Some(
            super::sources::window_process(id)
                .ok_or_else(|| CmdError::invalid_params("고른 창이 없어졌습니다"))?,
        ),
        None => None,
    };
    audio.stop(None);
    let stop = Arc::new(AtomicBool::new(false));
    let id = {
        let mut n = audio.next_id.lock();
        *n = n.wrapping_add(1).max(1);
        *n
    };
    *audio.current.lock() = Some((id, stop.clone()));
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    let audio2 = audio.clone();
    std::thread::spawn(move || {
        let r = capture(pid, &stop, &on_data, &ready_tx);
        if let Err(e) = r
            && !stop.load(Ordering::Relaxed)
        {
            let _ = on_data.send(InvokeResponseBody::Raw(end_message(&e)));
        }
        audio2.stop(Some(id));
    });
    match ready_rx.recv_timeout(OPEN_TIMEOUT) {
        Ok(Ok(())) => Ok(id),
        Ok(Err(e)) => Err(CmdError::internal(e)),
        Err(_) => {
            audio.stop(Some(id));
            Err(CmdError::internal("화면 소리를 열지 못했습니다(시간 초과)"))
        }
    }
}

#[tauri::command]
pub fn screen_audio_stop(audio: State<'_, Arc<ScreenAudio>>, id: Option<u32>) {
    audio.stop(id);
}

/// 루프백을 열어 멈출 때까지 PCM을 채널로 보냅니다. 열기 결과는 `ready`로 먼저 알립니다.
#[cfg(windows)]
fn capture(
    pid: Option<u32>,
    stop: &AtomicBool,
    out: &Channel<InvokeResponseBody>,
    ready: &SyncSender<std::result::Result<(), String>>,
) -> std::result::Result<(), String> {
    use windows::Win32::Media::Audio::AUDCLNT_BUFFERFLAGS_SILENT;
    use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};

    // 이 스레드만의 COM(멀티스레드 아파트). 활성화 완료 알림이 다른 스레드에서 오므로.
    let init = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    let opened = win::open(pid);
    let _ = ready.send(opened.as_ref().map(|_| ()).map_err(Clone::clone));
    let r = (|| {
        let (client, capture) = opened?;
        unsafe { client.Start() }.map_err(|e| format!("화면 소리를 시작하지 못했습니다: {e}"))?;
        let frame_bytes = usize::from(CHANNELS) * 4;
        let mut buf: Vec<u8> = Vec::with_capacity(CHUNK_FRAMES * frame_bytes * 2);
        while !stop.load(Ordering::Relaxed) {
            // 이벤트 대신 짧게 쉬며 꺼냅니다(루프백 버퍼는 200ms라 넉넉함).
            std::thread::sleep(Duration::from_millis(5));
            loop {
                let n = unsafe { capture.GetNextPacketSize() }
                    .map_err(|e| format!("화면 소리가 끊겼습니다: {e}"))?;
                if n == 0 {
                    break;
                }
                let mut data: *mut u8 = std::ptr::null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                unsafe { capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None) }
                    .map_err(|e| format!("화면 소리가 끊겼습니다: {e}"))?;
                let len = frames as usize * frame_bytes;
                if flags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0 || data.is_null() {
                    buf.resize(buf.len() + len, 0);
                } else {
                    // GetBuffer가 준 구간은 ReleaseBuffer 전까지 유효합니다.
                    buf.extend_from_slice(unsafe { std::slice::from_raw_parts(data, len) });
                }
                unsafe { capture.ReleaseBuffer(frames) }
                    .map_err(|e| format!("화면 소리가 끊겼습니다: {e}"))?;
            }
            if buf.len() >= CHUNK_FRAMES * frame_bytes {
                if out
                    .send(InvokeResponseBody::Raw(data_message(&buf)))
                    .is_err()
                {
                    return Err("화면이 소리를 받지 않습니다".into());
                }
                buf.clear();
            }
        }
        let _ = unsafe { client.Stop() };
        Ok(())
    })();
    if init.is_ok() {
        unsafe { CoUninitialize() };
    }
    r
}

#[cfg(not(windows))]
fn capture(
    _pid: Option<u32>,
    _stop: &AtomicBool,
    _out: &Channel<InvokeResponseBody>,
    ready: &SyncSender<std::result::Result<(), String>>,
) -> std::result::Result<(), String> {
    let e = "화면 소리는 윈도에서만 보낼 수 있습니다".to_string();
    let _ = ready.send(Err(e.clone()));
    Err(e)
}

#[cfg(windows)]
mod win {
    use std::sync::mpsc::{SyncSender, sync_channel};

    use windows::Win32::Media::Audio::{
        AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_LOOPBACK,
        AUDIOCLIENT_ACTIVATION_PARAMS, AUDIOCLIENT_ACTIVATION_PARAMS_0,
        AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS,
        ActivateAudioInterfaceAsync, IActivateAudioInterfaceAsyncOperation,
        IActivateAudioInterfaceCompletionHandler, IActivateAudioInterfaceCompletionHandler_Impl,
        IAudioCaptureClient, IAudioClient, PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
        PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE, VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
        WAVEFORMATEX,
    };
    use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
    use windows::Win32::System::Com::{IAgileObject, IAgileObject_Impl};
    use windows::Win32::System::Variant::VT_BLOB;
    use windows::core::{HRESULT, IUnknown, Interface, Ref, implement};

    use super::{CHANNELS, OPEN_TIMEOUT, SAMPLE_RATE};

    /// 활성화가 끝났다는 알림. 루프백 활성화는 아무 스레드에서나 부를 수 있어야 해(IAgileObject) 채널로만 알립니다.
    #[implement(IActivateAudioInterfaceCompletionHandler, IAgileObject)]
    struct Done(SyncSender<()>);

    impl IActivateAudioInterfaceCompletionHandler_Impl for Done_Impl {
        fn ActivateCompleted(
            &self,
            _op: Ref<IActivateAudioInterfaceAsyncOperation>,
        ) -> windows::core::Result<()> {
            let _ = self.0.try_send(());
            Ok(())
        }
    }

    impl IAgileObject_Impl for Done_Impl {}

    pub(super) fn open(pid: Option<u32>) -> Result<(IAudioClient, IAudioCaptureClient), String> {
        let fail = |e: windows::core::Error| {
            format!("화면 소리를 열지 못했습니다(윈도 10 2004 이상이 필요합니다): {e}")
        };
        let (target, mode) = match pid {
            Some(p) => (p, PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE),
            None => (
                std::process::id(),
                PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
            ),
        };
        let params = AUDIOCLIENT_ACTIVATION_PARAMS {
            ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
            Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
                ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                    TargetProcessId: target,
                    ProcessLoopbackMode: mode,
                },
            },
        };
        // VT_BLOB은 스택의 params를 가리키기만 합니다. PROPVARIANT의 Drop(PropVariantClear)은 이 포인터를 힙에서 풀려다
        // 힙을 깨뜨리므로 버리지 않습니다(가진 자원 없음).
        let mut pv = std::mem::ManuallyDrop::new(PROPVARIANT::default());
        unsafe {
            let v = &mut *pv.Anonymous.Anonymous;
            v.vt = VT_BLOB;
            v.Anonymous.blob.cbSize = size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32;
            v.Anonymous.blob.pBlobData = &params as *const _ as *mut u8;
        }
        let (tx, rx) = sync_channel(1);
        let handler: IActivateAudioInterfaceCompletionHandler = Done(tx).into();
        let op = unsafe {
            ActivateAudioInterfaceAsync(
                VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
                &IAudioClient::IID,
                Some(&*pv),
                &handler,
            )
        }
        .map_err(fail)?;
        rx.recv_timeout(OPEN_TIMEOUT)
            .map_err(|_| "화면 소리를 여는 데 시간이 너무 걸립니다".to_string())?;
        let mut hr = HRESULT(0);
        let mut unk: Option<IUnknown> = None;
        unsafe { op.GetActivateResult(&mut hr, &mut unk) }.map_err(fail)?;
        hr.ok().map_err(fail)?;
        let client: IAudioClient = unk
            .ok_or_else(|| "화면 소리 장치를 받지 못했습니다".to_string())?
            .cast()
            .map_err(fail)?;
        // 프로세스 루프백은 형식을 묻지 못해(GetMixFormat 없음) 정해서 엽니다. 변환은 윈도가 함.
        let block = CHANNELS * 4;
        let format = WAVEFORMATEX {
            wFormatTag: 3, // WAVE_FORMAT_IEEE_FLOAT
            nChannels: CHANNELS,
            nSamplesPerSec: SAMPLE_RATE,
            nAvgBytesPerSec: SAMPLE_RATE * u32::from(block),
            nBlockAlign: block,
            wBitsPerSample: 32,
            cbSize: 0,
        };
        unsafe {
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
                2_000_000, // 200ms(100ns 단위)
                0,
                &format,
                None,
            )
        }
        .map_err(fail)?;
        let capture: IAudioCaptureClient = unsafe { client.GetService() }.map_err(fail)?;
        Ok((client, capture))
    }
}

#[cfg(all(test, windows))]
mod tests {
    use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};

    /// 실제 오디오 서비스가 있어야 해서 손으로 돌립니다: `cargo test -p yangpa-desktop loopback_opens -- --ignored`.
    #[test]
    #[ignore]
    fn loopback_opens_and_reads() {
        let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        let (client, capture) = super::win::open(None).expect("열기");
        unsafe { client.Start() }.expect("시작");
        std::thread::sleep(std::time::Duration::from_millis(300));
        // 아무 소리가 없으면 패킷이 없을 수 있어 크기 조회가 되는지만 봅니다.
        let n = unsafe { capture.GetNextPacketSize() }.expect("패킷 크기");
        eprintln!("packet frames: {n}");
        unsafe { client.Stop() }.expect("멈춤");
    }
}
