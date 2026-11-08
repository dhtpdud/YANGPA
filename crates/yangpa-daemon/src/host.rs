//! 데몬 한 벌을 띄우고 내립니다: 데이터 경로 잠금, 노드, 로컬 API. `yangpad`(main.rs)와 데몬을 같은 프로세스에서 돌리는
//! Android 앱이 함께 씁니다.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use tracing::{info, warn};
use yangpa_rpc::endpoint::resolve_endpoint;
use yangpa_rpc::{EventHub, ServerHandle};

use crate::{ApiHandler, Node, NodeConfig};

/// 같은 데이터 경로로 두 데몬이 뜨지 않게 쥐는 잠금 파일. 프로세스가 끝나면(강제 종료 포함) OS가 풉니다.
const LOCK_FILE: &str = "yangpad.lock";
/// 앞선 데몬이 끝나기를 기다리는 시간. 앱을 닫자마자 다시 열면 이전 데몬이 아직 정리 중일 수 있습니다.
const LOCK_WAIT: Duration = Duration::from_secs(30);
/// 종료 신호 뒤 정리를 기다리는 시간(`LOCK_WAIT`보다 짧게). 넘기면 정리를 마치지 않고 끝냅니다.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(20);

/// 떠 있는 데몬.
pub struct Host {
    pub node: Arc<Node>,
    server: ServerHandle,
    _lock: Option<std::fs::File>,
}

impl Host {
    /// 잠금을 쥐고 노드와 로컬 API를 엽니다. `endpoint`가 없으면 기본 엔드포인트입니다.
    pub async fn start(cfg: NodeConfig, endpoint: Option<&str>) -> anyhow::Result<Host> {
        // DB·네트워크를 열기 전에 잡아, 두 데몬이 같은 기록과 키 상태를 따로 고쳐 쓰지 않게 합니다.
        let lock = lock_data_dir(&cfg.data_dir, LOCK_WAIT).await?;
        let hub = EventHub::new();
        let node = Node::start_with_hub(cfg, hub.clone())
            .await
            .context("노드를 시작할 수 없습니다")?;
        let endpoint = match resolve_endpoint(endpoint) {
            Ok(e) => e,
            Err(e) => {
                stop_node(&node).await;
                return Err(e).context("엔드포인트를 정할 수 없습니다");
            }
        };
        let server = match yangpa_rpc::serve(endpoint, Arc::new(ApiHandler::new(node.clone())), hub)
            .await
        {
            Ok(s) => s,
            Err(e) => {
                stop_node(&node).await;
                return Err(e).context("로컬 API를 열 수 없습니다(이미 다른 데몬이 떠 있나요?)");
            }
        };
        info!(endpoint = %server.endpoint().path().display(), "로컬 API 수신 시작");
        Ok(Host {
            node,
            server,
            _lock: lock,
        })
    }

    /// 로컬 API와 노드를 내립니다. 끝낸 뒤 데몬을 다시 띄워야 하면(오프라인 모드로 들어가거나 나올 때) true.
    pub async fn stop(self) -> bool {
        self.server.shutdown();
        stop_node(&self.node).await;
        if tokio::time::timeout(Duration::from_secs(2), self.server.wait())
            .await
            .is_err()
        {
            warn!("로컬 API 수락 작업이 끝나지 않아 기다리지 않고 끝냅니다");
        }
        self.node.restart_requested()
    }
}

/// 노드를 멈춥니다. 정리가 멈춰도 프로세스가 남아 데이터 경로를 붙잡지 않게 [`SHUTDOWN_GRACE`]까지만 기다립니다.
async fn stop_node(node: &Arc<Node>) {
    if tokio::time::timeout(SHUTDOWN_GRACE, node.shutdown())
        .await
        .is_err()
    {
        warn!(
            "노드 정리가 {}초 안에 끝나지 않아 기다리지 않고 끝냅니다",
            SHUTDOWN_GRACE.as_secs()
        );
    }
}

/// 데이터 경로 잠금을 쥡니다. 다른 데몬이 쥐고 있으면 `wait`까지 끝나기를 기다리고, 그래도 쥐고 있으면 오류입니다.
/// 잠금을 지원하지 않는 파일 시스템이면 경고만 남기고 그냥 뜹니다.
async fn lock_data_dir(dir: &Path, wait: Duration) -> anyhow::Result<Option<std::fs::File>> {
    yangpa_rpc::endpoint::create_private_dir(dir).context("데이터 경로를 만들 수 없습니다")?;
    let path = dir.join(LOCK_FILE);
    let mut o = std::fs::OpenOptions::new();
    o.create(true).truncate(false).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    let file = o
        .open(&path)
        .with_context(|| format!("잠금 파일을 열 수 없습니다({})", path.display()))?;
    let deadline = tokio::time::Instant::now() + wait;
    let mut waiting = false;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(Some(file)),
            Err(std::fs::TryLockError::WouldBlock) if tokio::time::Instant::now() < deadline => {
                if !waiting {
                    info!("같은 데이터 경로를 쓰는 이전 데몬이 끝나기를 기다립니다");
                    waiting = true;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            Err(std::fs::TryLockError::WouldBlock) => anyhow::bail!(
                "같은 데이터 경로({})를 쓰는 다른 데몬(yangpad)이 이미 떠 있습니다",
                dir.display()
            ),
            Err(std::fs::TryLockError::Error(e)) => {
                warn!(error = %e, "데이터 경로를 잠글 수 없어 잠금 없이 시작합니다");
                return Ok(None);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn one_daemon_per_data_dir() {
        let dir = tempfile::tempdir().unwrap();
        let first = lock_data_dir(dir.path(), Duration::ZERO).await.unwrap();
        assert!(first.is_some());
        let err = lock_data_dir(dir.path(), Duration::from_millis(300))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("다른 데몬"), "{err}");
        // 앞선 데몬이 기다리는 동안 끝나면 이어서 잡습니다.
        let waiter = tokio::spawn({
            let dir = dir.path().to_path_buf();
            async move { lock_data_dir(&dir, Duration::from_secs(10)).await }
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        drop(first);
        assert!(waiter.await.unwrap().unwrap().is_some());
    }
}
