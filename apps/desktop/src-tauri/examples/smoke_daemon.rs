//! 연기 시험용 최소 데몬: 신원이 없는 데몬처럼 답해 앱이 온보딩 화면까지 IPC로 오는지 봅니다.
//! 사용: YANGPA_ENDPOINT=/tmp/x.sock cargo run -p yangpa-desktop --example smoke_daemon

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use yangpa_rpc::api::Scope;
use yangpa_rpc::{AuthInfo, ConnCtx, EventHub, RpcError, RpcHandler, RpcOutput, endpoint, serve};

struct Smoke;

#[async_trait]
impl RpcHandler for Smoke {
    async fn authenticate(&self, token: &str) -> Option<AuthInfo> {
        (token == "smoke").then(|| AuthInfo {
            token_id: "smoke".into(),
            token_name: "smoke".into(),
            scopes: vec![Scope::Owner],
            spaces: Vec::new(),
            automation: false,
        })
    }

    async fn handle(
        &self,
        _ctx: &ConnCtx,
        method: &str,
        _params: Value,
    ) -> Result<RpcOutput, RpcError> {
        eprintln!("호출: {method}");
        match method {
            "system.version" => Ok(RpcOutput::Json(json!({"version": "smoke", "protocol": 1}))),
            "identity.get" => Err(RpcError::new(1008, "신원이 없습니다")),
            _ => Ok(RpcOutput::Json(json!([]))),
        }
    }
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let ep = endpoint::default_endpoint()?;
    let handle = serve(ep, Arc::new(Smoke), EventHub::new()).await?;
    eprintln!("준비됨");
    tokio::signal::ctrl_c().await?;
    handle.shutdown();
    Ok(())
}
