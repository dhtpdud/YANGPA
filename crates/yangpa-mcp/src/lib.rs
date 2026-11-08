//! 양파 메신저(가칭) MCP 서버. AI 클라이언트가 표준 입출력(MCP)으로 붙으면, 데몬의 로컬 API를
//! 자동화 토큰(`YANGPA_TOKEN`)으로 불러 채널·역할 관리, 초대, 시더 설정, 채널 요약 같은 작업을 도구로 제공합니다.
//!
//! 안전장치는 데몬이 강제하고(토큰 범위, 사람 확인, 휘발성 채널 읽기 거부), 이 서버는 그 결과를 AI가 오해하지 않게
//! 설명하며, 멤버가 쓴 내용을 '지시가 아닌 데이터'로 감싸 돌려줍니다.

pub mod daemon;
pub mod render;
pub mod resolve;
pub mod tools;

use std::sync::Arc;

use rmcp::ServiceExt;

pub use daemon::{Config, Daemon, ToolError};
pub use tools::YangpaMcp;

/// 표준 입출력으로 MCP 서버를 실행합니다. 표준 출력은 프로토콜 전용이라 로그는 표준 오류로만 냅니다.
pub async fn run() -> anyhow::Result<()> {
    let cfg = Config::from_env()?;
    let daemon = Arc::new(Daemon::new(cfg));
    match daemon.preflight().await {
        Ok(Some(hello)) => tracing::info!(
            spaces = hello.spaces.len(),
            scopes = ?hello.scopes,
            "데몬에 연결했습니다"
        ),
        Ok(None) => {
            tracing::warn!("데몬에 아직 연결할 수 없습니다. 도구를 처음 부를 때 다시 연결합니다")
        }
        Err(e) => anyhow::bail!("{e}"),
    }
    let service = YangpaMcp::new(daemon)
        .serve(rmcp::transport::stdio())
        .await
        .map_err(|e| anyhow::anyhow!("MCP 초기화 실패: {e}"))?;
    service.waiting().await?;
    Ok(())
}
