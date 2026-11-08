//! 자동 자리 비움: 이 기기에 키보드·마우스 입력이 [`AWAY_AFTER`] 동안 없으면 데몬에 알립니다(`presence.set {idle}`).
//! 앱 창이 뒤에 있어도 기기 전체의 마지막 입력 시각을 봅니다. 윈도가 아니면 알 수 없어 보내지 않습니다.

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use yangpa_rpc::RpcClient;

use crate::daemon::Daemon;

const AWAY_AFTER: Duration = Duration::from_secs(10 * 60);
const EVERY: Duration = Duration::from_secs(30);

pub async fn watch(d: Arc<Daemon>) {
    // 마지막으로 알린 연결과 값. 호출마다 감사 로그에 남으므로 바뀔 때만 보내고, 데몬이 다시 뜨면(연결이 닫힘) 새
    // 데몬은 입력 없음을 모르니 다시 알립니다.
    let mut sent: Option<(RpcClient, bool)> = None;
    loop {
        tokio::time::sleep(EVERY).await;
        let Some(idle) = idle_for() else { return };
        let idle = idle >= AWAY_AFTER;
        let Some(client) = d.client() else { continue };
        if sent
            .as_ref()
            .is_some_and(|(c, v)| !c.is_closed() && *v == idle)
        {
            continue;
        }
        if client
            .call_value("presence.set", json!({ "idle": idle }))
            .await
            .is_ok()
        {
            sent = Some((client, idle));
        }
    }
}

#[cfg(windows)]
fn idle_for() -> Option<Duration> {
    use windows_sys::Win32::System::SystemInformation::GetTickCount;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
    let mut lii = LASTINPUTINFO {
        cbSize: size_of::<LASTINPUTINFO>() as u32,
        dwTime: 0,
    };
    // SAFETY: 크기를 맞춘 구조체 포인터만 넘깁니다. 두 값 모두 49.7일마다 도는 밀리초 틱이라 감산은 감싸서 합니다.
    let (ok, now) = unsafe { (GetLastInputInfo(&mut lii), GetTickCount()) };
    (ok != 0).then(|| Duration::from_millis(u64::from(now.wrapping_sub(lii.dwTime))))
}

#[cfg(not(windows))]
fn idle_for() -> Option<Duration> {
    None
}
