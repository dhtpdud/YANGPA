//! `yangpa-mcp` 실행 파일: AI 클라이언트(MCP)가 표준 입출력으로 실행합니다.
//!
//! 필요한 환경 변수: `YANGPA_TOKEN`(자동화 토큰). 선택: `YANGPA_ENDPOINT`, `YANGPA_MCP_LOG`(로그 수준, 기본 warn).

use std::process::ExitCode;

use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> ExitCode {
    // 표준 출력은 MCP 메시지 전용이므로 로그는 표준 오류로 보냅니다. 메시지 내용·토큰은 로그에 남기지 않습니다.
    let filter =
        EnvFilter::try_from_env("YANGPA_MCP_LOG").unwrap_or_else(|_| EnvFilter::new("warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();
    match yangpa_mcp::run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("yangpa-mcp: {e:#}");
            ExitCode::FAILURE
        }
    }
}
