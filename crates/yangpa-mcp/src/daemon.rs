//! 데몬 연결. 처음 도구를 부를 때 연결하고, 끊겼으면 다음 호출에서 다시 연결합니다.
//!
//! 토큰은 `YANGPA_TOKEN`(자동화 토큰)에서만 읽습니다. 소유자 토큰 파일로 넘어가지 않는 이유는,
//! 소유자 토큰은 자동화 토큰이 아니라서 파괴적 작업이 사람 확인 없이 실행되기 때문입니다.

use std::ffi::OsString;
use std::time::Duration;

use tokio::sync::Mutex;
use yangpa_rpc::api::serde_util::SecretString;
use yangpa_rpc::api::{HelloResult, Method};
use yangpa_rpc::endpoint::{ENV_TOKEN, default_endpoint_with};
use yangpa_rpc::{ClientError, Endpoint, RpcClient};

/// 자동화가 아닌 토큰을 일부러 허용할 때 켜는 환경 변수(`1`).
pub const ENV_ALLOW_NON_AUTOMATION: &str = "YANGPA_MCP_ALLOW_NON_AUTOMATION";
/// 요청 하나의 시간 상한.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// 시작 설정.
#[derive(Debug)]
pub struct Config {
    pub endpoint: Endpoint,
    pub token: SecretString,
    pub allow_non_automation: bool,
    pub timeout: Duration,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Config> {
        Config::from_env_with(yangpa_rpc::endpoint::process_env)
    }

    pub fn from_env_with(env: impl Fn(&str) -> Option<OsString>) -> anyhow::Result<Config> {
        let token = env(ENV_TOKEN)
            .map(|t| SecretString::new(t.to_string_lossy().trim()))
            .filter(|t| !t.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "{ENV_TOKEN}가 없습니다. 앱이나 `yangpa token create --name ai --scopes read,write --automation`으로 \
                     AI용 자동화 토큰을 발급해 {ENV_TOKEN}에 넣으세요"
                )
            })?;
        let endpoint = default_endpoint_with(&env)
            .map_err(|e| anyhow::anyhow!("데몬 엔드포인트를 정할 수 없습니다: {e}"))?;
        let allow_non_automation = env(ENV_ALLOW_NON_AUTOMATION)
            .is_some_and(|v| matches!(v.to_string_lossy().trim(), "1" | "true" | "yes"));
        Ok(Config {
            endpoint,
            token,
            allow_non_automation,
            timeout: REQUEST_TIMEOUT,
        })
    }
}

/// 도구 실행 실패.
#[derive(Debug)]
pub enum ToolError {
    /// 데몬이 돌려준 오류 또는 연결 오류.
    Client(ClientError),
    /// 도구 인자가 잘못됨(이름을 못 찾음 등).
    Input(String),
    /// 자동화가 아닌 토큰(사람 확인 장치를 우회하게 되므로 거부).
    NotAutomation,
}

impl From<ClientError> for ToolError {
    fn from(e: ClientError) -> Self {
        ToolError::Client(e)
    }
}

impl std::error::Error for ToolError {}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ToolError::Client(e) => write!(f, "{e}"),
            ToolError::Input(s) => f.write_str(s),
            ToolError::NotAutomation => f.write_str(
                "YANGPA_TOKEN이 자동화(AI) 토큰이 아닙니다. 사람 확인 안전장치가 동작하도록 `--automation`으로 발급한 토큰을 쓰세요",
            ),
        }
    }
}

/// 데몬 연결 하나를 여러 도구 호출이 나눠 씁니다(요청은 ID로 다중화됨).
pub struct Daemon {
    cfg: Config,
    client: Mutex<Option<RpcClient>>,
}

impl Daemon {
    pub fn new(cfg: Config) -> Daemon {
        Daemon {
            cfg,
            client: Mutex::new(None),
        }
    }

    /// 테스트용: 이미 인증된 클라이언트로 만듭니다.
    pub fn with_client(cfg: Config, client: RpcClient) -> Daemon {
        Daemon {
            cfg,
            client: Mutex::new(Some(client)),
        }
    }

    /// 연결된(끊겼으면 다시 연결한) 클라이언트.
    pub async fn client(&self) -> Result<RpcClient, ToolError> {
        let mut slot = self.client.lock().await;
        if let Some(c) = slot.as_ref()
            && !c.is_closed()
        {
            return Ok(c.clone());
        }
        *slot = None;
        let (client, _) = self.connect().await?;
        *slot = Some(client.clone());
        Ok(client)
    }

    async fn connect(&self) -> Result<(RpcClient, HelloResult), ToolError> {
        let client = tokio::time::timeout(self.cfg.timeout, RpcClient::connect(&self.cfg.endpoint))
            .await
            .map_err(|_| ClientError::Timeout)??
            .with_timeout(self.cfg.timeout);
        let hello = client.hello(self.cfg.token.expose()).await?;
        if !hello.automation && !self.cfg.allow_non_automation {
            return Err(ToolError::NotAutomation);
        }
        Ok((client, hello))
    }

    /// 시작할 때 한 번 연결해 봅니다. 데몬이 아직 없으면 괜찮고(나중에 연결), 토큰이 자동화가 아니면 실패합니다.
    pub async fn preflight(&self) -> Result<Option<HelloResult>, ToolError> {
        match self.connect().await {
            Ok((client, hello)) => {
                *self.client.lock().await = Some(client);
                Ok(Some(hello))
            }
            Err(ToolError::Client(ClientError::Connect(_) | ClientError::Timeout)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// 메서드 호출. 보낸 뒤 실패한 요청은 다시 보내지 않습니다(메시지 중복 전송 방지).
    pub async fn request<M: Method>(&self, params: M::Params) -> Result<M::Result, ToolError> {
        let client = self.client().await?;
        Ok(client.request::<M>(params).await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> + use<> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| {
            pairs
                .iter()
                .find(|(pk, _)| pk == k)
                .map(|(_, v)| OsString::from(v))
        }
    }

    #[test]
    fn token_is_required_and_owner_file_is_not_used() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().to_str().unwrap().to_string();
        // 소유자 토큰 파일이 있어도 읽지 않습니다.
        std::fs::write(dir.path().join("owner.token"), "owner-secret\n").unwrap();
        let e = Config::from_env_with(env_of(&[("YANGPA_DATA_DIR", &data)])).unwrap_err();
        assert!(e.to_string().contains("YANGPA_TOKEN"));

        let cfg = Config::from_env_with(env_of(&[
            ("YANGPA_DATA_DIR", &data),
            ("YANGPA_TOKEN", " ai-token \n"),
            ("YANGPA_ENDPOINT", "/tmp/y.sock"),
        ]))
        .unwrap();
        assert_eq!(cfg.token.expose(), "ai-token");
        assert_eq!(cfg.endpoint.path(), std::path::Path::new("/tmp/y.sock"));
        assert!(!cfg.allow_non_automation);
        assert!(!format!("{cfg:?}").contains("ai-token"));

        let cfg = Config::from_env_with(env_of(&[
            ("YANGPA_DATA_DIR", &data),
            ("YANGPA_TOKEN", "t"),
            (ENV_ALLOW_NON_AUTOMATION, "1"),
        ]))
        .unwrap();
        assert!(cfg.allow_non_automation);
    }
}
