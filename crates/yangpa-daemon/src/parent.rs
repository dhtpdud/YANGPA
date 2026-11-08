//! `yangpad` 실행 파일 전용: 데몬을 띄운 프로세스(데스크톱 앱)가 끝나면 데몬도 끝냅니다(`--parent-pid`).
//! 앱은 끝날 때 `system.shutdown`을 보내지만, 강제 종료·튕김에는 그 신호가 없어 데몬이 혼자 남았습니다.

use std::future::Future;

use tracing::warn;

/// Unix에서 프로세스가 끝났는지 다시 보는 간격.
#[cfg(unix)]
const POLL: std::time::Duration = std::time::Duration::from_secs(1);

/// `pid` 프로세스가 끝나면 끝나는 future. 시작하자마자 불러야 합니다: Windows는 여기서 프로세스 핸들을 열어 두어, 데이터
/// 경로 잠금을 기다리는 사이 PID가 다른 프로세스에 다시 쓰여도 헷갈리지 않습니다. 이미 끝난 프로세스면 곧바로 끝납니다.
/// 지켜볼 수 없으면(권한 등) 경고를 남기고 `None`(지켜보지 않고 계속).
#[cfg(windows)]
pub(crate) fn watch(pid: u32) -> Option<impl Future<Output = ()> + Send> {
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_INVALID_PARAMETER, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Threading::{
        INFINITE, OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };

    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    // SAFETY: 핸들을 여는 것 말고 부작용이 없는 호출입니다.
    let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if handle.is_null() {
        let e = std::io::Error::last_os_error();
        // 그런 PID의 프로세스가 없음: 앱이 이미 끝났습니다.
        if e.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) {
            let _ = tx.send(());
            return Some(gone(rx));
        }
        warn!(pid, error = %e, "앱 프로세스를 지켜볼 수 없어 앱이 끝나도 데몬이 남을 수 있습니다");
        return None;
    }
    // 원시 포인터는 Send가 아니라 정수로 넘깁니다. 이 스레드만 핸들을 쓰고 닫습니다.
    let raw = handle as usize;
    let spawned = std::thread::Builder::new()
        .name("yangpad-parent".into())
        .spawn(move || {
            let handle = raw as windows_sys::Win32::Foundation::HANDLE;
            // SAFETY: 위에서 연 유효한 핸들이고, 기다린 뒤 한 번만 닫습니다.
            let r = unsafe { WaitForSingleObject(handle, INFINITE) };
            unsafe { CloseHandle(handle) };
            if r == WAIT_OBJECT_0 {
                let _ = tx.send(());
            }
        });
    if let Err(e) = spawned {
        warn!(error = %e, "앱 프로세스를 지켜볼 스레드를 띄울 수 없습니다");
        return None;
    }
    Some(gone(rx))
}

/// Unix: 데몬을 바로 띄운 부모면 부모가 바뀌는지(끝나면 init 등으로 바뀜. 부모가 아직 거두어지지 않은 좀비여도 바로
/// 바뀌고 PID 재사용과도 상관없음), 아니면 `kill(pid, 0)`으로 그 프로세스가 아직 있는지 봅니다.
#[cfg(unix)]
pub(crate) fn watch(pid: u32) -> Option<impl Future<Output = ()> + Send> {
    let Ok(raw) = libc::pid_t::try_from(pid) else {
        warn!(pid, "앱 프로세스 ID가 잘못되어 지켜보지 않습니다");
        return None;
    };
    let is_parent = std::os::unix::process::parent_id() == pid;
    let alive = move || {
        if is_parent {
            return std::os::unix::process::parent_id() == pid;
        }
        // SAFETY: 신호 0은 보내지 않고 프로세스가 있는지만 봅니다.
        let r = unsafe { libc::kill(raw, 0) };
        r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    };
    Some(async move {
        while alive() {
            tokio::time::sleep(POLL).await;
        }
    })
}

/// 지켜보는 스레드가 알려 오면 끝납니다. 알리지 못하고 사라졌으면(앱은 그대로) 끝나지 않습니다.
#[cfg(windows)]
async fn gone(rx: tokio::sync::oneshot::Receiver<()>) {
    if rx.await.is_err() {
        std::future::pending::<()>().await;
    }
}

/// 시험용: 앱 대신 지켜볼, 혼자서는 한참 끝나지 않는 프로세스.
#[cfg(test)]
pub(crate) fn stand_in() -> std::process::Child {
    use std::process::{Command, Stdio};
    #[cfg(windows)]
    let mut cmd = {
        let mut c = Command::new("ping");
        c.args(["-n", "120", "127.0.0.1"]);
        c
    };
    #[cfg(unix)]
    let mut cmd = {
        let mut c = Command::new("sleep");
        c.arg("120");
        c
    };
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("대신 지켜볼 프로세스 실행")
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn resolves_when_process_ends() {
        let mut p = stand_in();
        let w = watch(p.id()).expect("지켜보기");
        tokio::pin!(w);
        assert!(
            tokio::time::timeout(Duration::from_millis(300), &mut w)
                .await
                .is_err(),
            "살아 있는 동안은 끝나지 않음"
        );
        p.kill().unwrap();
        p.wait().unwrap();
        tokio::time::timeout(Duration::from_secs(10), w)
            .await
            .expect("프로세스가 끝나면 끝남");
    }

    #[tokio::test]
    async fn already_gone_resolves_at_once() {
        let mut p = stand_in();
        let pid = p.id();
        p.kill().unwrap();
        p.wait().unwrap();
        let w = watch(pid).expect("지켜보기");
        tokio::time::timeout(Duration::from_secs(10), w)
            .await
            .expect("이미 끝난 프로세스는 곧바로 끝남");
    }
}
