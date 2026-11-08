//! Android: 사이드카를 띄울 수 없어 데몬을 앱 프로세스의 전용 스레드에서 돌립니다(docs/android.md). 로컬 API는 데스크톱과
//! 같은 유닉스 소켓이라 [`crate::daemon`]과 프런트엔드는 그대로입니다. 프로세스는 포그라운드 서비스가 살려 둡니다.

use std::io;
use std::path::PathBuf;
use std::time::Duration;

use tauri::{AppHandle, Runtime};
use tracing::{error, info};
use yangpa_daemon::NodeConfig;
use yangpa_daemon::host::Host;
use yangpa_rpc::endpoint;

/// 런타임을 내릴 때 남은 블로킹 작업을 기다리는 시간(yangpad와 같음).
const RUNTIME_SHUTDOWN: Duration = Duration::from_secs(2);

/// 데몬 스레드를 띄웁니다. 데몬이 소켓을 열 때까지는 [`crate::daemon`]이 붙기를 되풀이합니다.
pub fn start<R: Runtime>(app: AppHandle<R>) -> io::Result<PathBuf> {
    let data_dir = endpoint::data_dir()?;
    let cfg = NodeConfig::new(&data_dir);
    std::thread::Builder::new()
        .name("yangpad".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    error!("데몬 런타임을 만들 수 없습니다: {e}");
                    return;
                }
            };
            let restart = rt.block_on(serve(cfg));
            rt.shutdown_timeout(RUNTIME_SHUTDOWN);
            // Arti는 같은 프로세스에서 다시 만들 수 없어 오프라인 모드를 오갈 때는 프로세스를 새로 띄웁니다.
            if restart {
                crate::android::restart(&app);
            }
        })?;
    Ok(data_dir)
}

/// 끝날 때까지 돕니다. 끝난 뒤 다시 띄워야 하면 true.
async fn serve(cfg: NodeConfig) -> bool {
    let host = match Host::start(cfg, None).await {
        Ok(h) => h,
        Err(e) => {
            error!("데몬을 시작하지 못했습니다: {e:#}");
            return false;
        }
    };
    let mut stop = host.node.shutdown_signal();
    let _ = stop.wait_for(|v| *v).await;
    info!("system.shutdown: 데몬을 끝냅니다");
    host.stop().await
}
