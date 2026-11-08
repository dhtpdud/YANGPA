//! 양파 메신저(가칭) 명령줄 도구 `yangpa`.
//!
//! 앱의 모든 기능을 같은 구조로 노출합니다(`yangpa crew create "게임 모임"`, `yangpa channel create --crew …`).
//! 데몬(yangpad)의 로컬 API 클라이언트일 뿐이라 권한·확인 규칙은 데몬이 정합니다.
//!
//! 종료 코드: 0 성공, 1 오류, 2 사용법 오류, 3 사람 확인 대기(`1006`), 4 데몬에 연결할 수 없음.

pub mod args;
pub mod commands;
pub mod daemon;
pub mod output;
pub mod parse;
pub mod resolve;

use clap::Parser;
use yangpa_rpc::ClientError;
use yangpa_rpc::jsonrpc::{
    ERR_AUTH_REQUIRED, ERR_CONFIRMATION_REQUIRED, ERR_NO_IDENTITY, ERR_OUT_OF_SCOPE,
    ERR_VOLATILE_READ_DENIED,
};

use crate::args::{Cli, Command};

pub const EXIT_ERROR: i32 = 1;
pub const EXIT_USAGE: i32 = 2;
pub const EXIT_CONFIRMATION: i32 = 3;
pub const EXIT_NO_DAEMON: i32 = 4;

/// `main`에서 부릅니다. 종료 코드를 돌려줍니다.
pub fn main_entry() -> i32 {
    let cli = match Cli::try_parse() {
        Ok(c) => c,
        Err(e) => {
            let code = if e.use_stderr() { EXIT_USAGE } else { 0 };
            let _ = e.print();
            return code;
        }
    };
    let json = cli.json;
    if let Command::Daemon(d) = &cli.command {
        return match daemon::run(d, cli.endpoint.as_deref()) {
            Ok(code) => code,
            Err(e) => report(&e, json, None),
        };
    }
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("오류: 비동기 런타임을 만들 수 없습니다: {e}");
            return EXIT_ERROR;
        }
    };
    rt.block_on(run(cli))
}

/// 명령 하나를 실행하고 종료 코드를 돌려줍니다.
pub async fn run(cli: Cli) -> i32 {
    let Cli {
        endpoint,
        token,
        json,
        timeout,
        command,
    } = cli;
    let endpoint = match yangpa_rpc::endpoint::resolve_endpoint(endpoint.as_deref()) {
        Ok(e) => e,
        Err(e) => {
            return report(
                &anyhow::Error::new(e).context("데몬 엔드포인트를 정할 수 없습니다"),
                json,
                None,
            );
        }
    };
    let ep = endpoint.to_string();
    let result = async {
        if matches!(command, Command::Version) {
            return commands::version(&endpoint, json, timeout).await;
        }
        let client = commands::connect(&endpoint, token.as_deref(), timeout).await?;
        commands::Ctx::new(client, json).run(command).await
    }
    .await;
    match result {
        Ok(()) => 0,
        Err(e) => report(&e, json, Some(&ep)),
    }
}

/// 오류를 표준 오류로 알리고 종료 코드를 정합니다. 데몬 오류에는 다음에 할 일을 덧붙입니다.
pub fn report(e: &anyhow::Error, json: bool, endpoint: Option<&str>) -> i32 {
    let client = e.chain().find_map(|c| c.downcast_ref::<ClientError>());
    let code = exit_code(client);
    if json {
        let body = match client.and_then(ClientError::rpc) {
            Some(rpc) => serde_json::json!({ "error": rpc }),
            None => serde_json::json!({ "error": { "message": format!("{e:#}") } }),
        };
        eprintln!("{body}");
        return code;
    }
    eprintln!("{}", human_error(e));
    if let Some(hint) = hint(client, endpoint) {
        eprintln!("{}", output::sanitize_text(&hint));
    }
    code
}

/// 사람이 읽는 오류 한 줄. 오류 문구에는 다른 멤버가 정한 이름(이름 찾기 실패 시 후보 목록)이나 데몬이 되돌려준
/// 입력이 들어갈 수 있으므로 터미널 제어 문자를 지웁니다.
pub fn human_error(e: &anyhow::Error) -> String {
    output::sanitize_text(&format!("오류: {e:#}"))
}

fn exit_code(e: Option<&ClientError>) -> i32 {
    match e {
        Some(ClientError::Connect(_)) => EXIT_NO_DAEMON,
        Some(e) if e.code() == Some(ERR_CONFIRMATION_REQUIRED) => EXIT_CONFIRMATION,
        _ => EXIT_ERROR,
    }
}

/// 오류 종류별 안내 문구.
pub fn hint(e: Option<&ClientError>, endpoint: Option<&str>) -> Option<String> {
    let e = e?;
    if let ClientError::Connect(_) = e {
        return Some(format!(
            "데몬이 실행 중인지 확인하세요(엔드포인트: {}). GUI 없이 실행하려면 `yangpa daemon`.",
            endpoint.unwrap_or("기본값")
        ));
    }
    let rpc = e.rpc()?;
    match rpc.code {
        ERR_CONFIRMATION_REQUIRED => {
            let id = rpc.confirmation_id().unwrap_or("?");
            Some(format!(
                "자동화 토큰의 파괴적 작업이라 사람 확인을 기다립니다. 앱에서 승인하거나 `yangpa confirm approve {id}`를 실행하세요."
            ))
        }
        ERR_AUTH_REQUIRED => Some(
            "토큰이 없거나 잘못됐습니다. --token, YANGPA_TOKEN 또는 소유자 토큰 파일을 확인하세요.".into(),
        ),
        ERR_OUT_OF_SCOPE => {
            let need = rpc
                .data
                .as_ref()
                .and_then(|d| d.get("required"))
                .and_then(|v| v.as_str())
                .map(|s| format!("(필요한 범위: {s})"))
                .unwrap_or_default();
            Some(format!("이 토큰으로는 할 수 없는 작업입니다{need}. 범위가 맞는 토큰을 쓰세요."))
        }
        ERR_NO_IDENTITY => {
            Some("아직 신원이 없습니다. `yangpa identity create --name <이름>`으로 만드세요.".into())
        }
        ERR_VOLATILE_READ_DENIED => Some(
            "휘발성 채널은 API 읽기가 기본 거부입니다. 사람이 `yangpa channel allow-api-read --crew … <채널> on`으로 허용해야 합니다."
                .into(),
        ),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use yangpa_rpc::RpcError;

    #[test]
    fn exit_codes_and_hints() {
        let conn = ClientError::Connect(std::io::Error::from(std::io::ErrorKind::NotFound));
        assert_eq!(exit_code(Some(&conn)), EXIT_NO_DAEMON);
        assert!(
            hint(Some(&conn), Some("/run/x.sock"))
                .unwrap()
                .contains("/run/x.sock")
        );

        let pending = ClientError::Rpc(RpcError::confirmation_required("c-42"));
        assert_eq!(exit_code(Some(&pending)), EXIT_CONFIRMATION);
        assert!(
            hint(Some(&pending), None)
                .unwrap()
                .contains("yangpa confirm approve c-42")
        );

        let scope = ClientError::Rpc(
            RpcError::out_of_scope("범위 밖").with_data(serde_json::json!({"required": "admin"})),
        );
        assert_eq!(exit_code(Some(&scope)), EXIT_ERROR);
        assert!(hint(Some(&scope), None).unwrap().contains("admin"));
        assert!(
            hint(
                Some(&ClientError::Rpc(RpcError::volatile_read_denied())),
                None
            )
            .is_some()
        );
        assert_eq!(exit_code(None), EXIT_ERROR);
        assert!(hint(None, None).is_none());
    }

    #[test]
    fn error_text_cannot_control_the_terminal() {
        // 이름 찾기 실패 메시지에는 멤버가 정한 이름이 그대로 들어갑니다.
        let e = anyhow::anyhow!(
            "멤버 'x'을(를) 찾을 수 없습니다. 있는 멤버: 철수\x1b]0;pwned\x07, 영희\u{202E}"
        );
        let s = human_error(&e);
        assert!(!s.contains('\x1b') && !s.contains('\x07') && !s.contains('\u{202E}'));
        assert!(s.starts_with("오류: ") && s.contains("철수"));
    }

    #[test]
    fn report_finds_client_error_through_context() {
        let e = anyhow::Error::new(ClientError::Rpc(RpcError::confirmation_required("c-1")))
            .context("겉 문맥");
        assert_eq!(report(&e, true, None), EXIT_CONFIRMATION);
        let e = anyhow::anyhow!("그냥 오류");
        assert_eq!(report(&e, false, None), EXIT_ERROR);
    }

    #[tokio::test]
    async fn unreachable_daemon_exits_with_4() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("없음.sock");
        let cli = Cli::try_parse_from([
            "yangpa",
            "--endpoint",
            sock.to_str().unwrap(),
            "--token",
            "t",
            "space",
            "list",
        ])
        .unwrap();
        assert_eq!(run(cli).await, EXIT_NO_DAEMON);
    }
}
