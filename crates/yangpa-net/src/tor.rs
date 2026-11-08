//! Arti(Tor) 위에 직접 만든 libp2p 전송.
//!
//! libp2p-community-tor(libp2p 0.53·Arti 0.24 전용)의 방식을 libp2p 0.57·Arti 0.47로 옮기고 수신을 더했습니다.
//! - 나가는 연결: `/onion3/<주소>:<포트>`는 onion 서비스로, `/dns*/tcp`·`/ip*/tcp`는 출구 노드로.
//!   이름은 출구 노드가 풀므로 로컬 DNS 조회가 없습니다.
//! - 들어오는 연결: onion 서비스(`launch_onion_service`)의 스트림 요청을 받아 전송 계층의 수신 연결로 넘기고,
//!   주소를 스웜에 알립니다.
//! - Arti 클라이언트는 처음 필요할 때 만듭니다.
//!
//! 보안·다중화(TLS + yamux)는 다른 전송과 똑같이 위에서 덧씌웁니다.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use arti_client::config::pt::TransportConfigBuilder;
use arti_client::config::{
    BridgeConfigBuilder, CfgPath, Reconfigure, TorClientConfig, TorClientConfigBuilder,
};
use arti_client::{DataStream, StreamPrefs, TorAddr, TorClient};
use futures::future::{BoxFuture, Ready};
use futures::{FutureExt, StreamExt};
use libp2p::core::Endpoint;
use libp2p::core::transport::{DialOpts, ListenerId, Transport, TransportError, TransportEvent};
use libp2p::multiaddr::Multiaddr;
use safelog::DisplayRedacted;
use tokio::sync::{OnceCell, Semaphore, mpsc};
use tor_cell::relaycell::msg::{Connected, End, EndReason};
use tor_hsservice::config::OnionServiceConfigBuilder;
use tor_hsservice::{HsId, HsNickname, OnionServiceConfig, StreamRequest, handle_rend_requests};
use tor_proto::stream::IncomingStreamRequest;
use tor_rtcompat::PreferredRuntime;
use tracing::{debug, info, warn};

use crate::addr::{self, TorTarget};
use crate::config::TorBridges;
use crate::tor_stream::{TorActivity, TorIsolation};

/// onion 서비스 키·상태를 찾는 이름(Arti 상태 디렉터리 안).
const ONION_NICKNAME: &str = "yangpa";
/// 예전 빌드가 일반 모드에서 따로 쓰던 친구 전용 onion 서비스 이름. 이제 onion은 하나라 키를 지울 때만 씁니다.
const OLD_FRIEND_ONION_NICKNAME: &str = "yangpa-friend";
/// 동시에 수락 처리 중인 onion 스트림 상한.
const MAX_PENDING_ACCEPTS: usize = 32;
/// onion 서비스를 다시 띄우는 간격(처음·최대).
const ONION_RETRY_MIN: Duration = Duration::from_secs(2);
const ONION_RETRY_MAX: Duration = Duration::from_secs(60);
/// 부트스트랩이 준비되지 않았을 때 다시 시도하는 간격(처음·최대).
const BOOTSTRAP_RETRY_MIN: Duration = Duration::from_secs(10);
const BOOTSTRAP_RETRY_MAX: Duration = Duration::from_secs(120);
/// 준비된 뒤 디렉터리가 만료돼 있으면 Arti의 합의문 재시도 대기를 깨우는 간격.
const DIR_WAKE: Duration = Duration::from_secs(600);
/// lyrebird 하나가 맡는 브리지 전송(Tor Browser `pt_config.json`과 같음).
const LYREBIRD_TRANSPORTS: [&str; 4] = ["obfs4", "snowflake", "webtunnel", "meek_lite"];

#[derive(Debug, thiserror::Error)]
pub enum TorTransportError {
    #[error("Arti 오류: {0}")]
    Arti(#[from] arti_client::Error),
    #[error("Tor 주소 오류: {0}")]
    Addr(#[from] arti_client::TorAddrError),
    #[error("Tor 설정 오류: {0}")]
    Config(String),
    #[error("입출력 오류: {0}")]
    Io(#[from] std::io::Error),
    #[error("onion 서비스 오류: {0}")]
    Service(String),
}

/// 여러 전송·작업이 함께 쓰는 Arti 상태. 클라이언트는 처음 필요할 때 만듭니다.
///
/// 마지막 참조가 사라지면(스웜 작업 종료) 백그라운드 작업을 멈춰 Arti 클라이언트를 놓습니다. 다만 Arti 0.47은
/// 클라이언트를 놓아도 내부 작업이 상태·디렉터리 캐시 잠금을 계속 쥐므로, 같은 프로세스에서 네트워크를 다시 띄울
/// 때는 새로 만들지 않고 이 상태를 새 스웜에 넘깁니다([`crate::transport`]의 `build`).
pub struct ArtiShared {
    state_dir: PathBuf,
    cache_dir: PathBuf,
    client: OnceCell<Arc<TorClient<PreferredRuntime>>>,
    /// 브리지(검열 우회). 클라이언트가 떠 있으면 바꿀 때 바로 다시 설정합니다([`ArtiShared::set_bridges`]).
    bridges: Mutex<Option<TorBridges>>,
    /// 브리지를 바꾸면 부트스트랩 재시도 대기를 끊고 바로 다시 해 봅니다.
    kick: Arc<tokio::sync::Notify>,
    tasks: Mutex<Vec<tokio::task::AbortHandle>>,
    /// 전송이 받아들인 다이얼 수(누출 시험에서 'Arti 경로를 탔다'를 확인).
    dials: AtomicU64,
    /// 앱 스트림 시도 수([`ArtiShared::connect_stream`]).
    streams: AtomicU64,
    /// 서비스 이름별로 읽어 둔 onion 주소(`xxxx.onion`).
    hosts: Mutex<HashMap<&'static str, String>>,
    /// 시험용: 실제 Tor 망 대신 닿지 않는 루프백 디렉터리만 알게 합니다([`ArtiShared::offline_for_test`]).
    #[cfg(test)]
    offline: bool,
}

impl Drop for ArtiShared {
    fn drop(&mut self) {
        let tasks = match self.tasks.get_mut() {
            Ok(t) => t,
            Err(e) => e.into_inner(),
        };
        for t in tasks.drain(..) {
            t.abort();
        }
    }
}

impl ArtiShared {
    pub fn new(state_dir: PathBuf, cache_dir: PathBuf) -> Arc<Self> {
        Arc::new(ArtiShared {
            state_dir,
            cache_dir,
            client: OnceCell::new(),
            bridges: Mutex::new(None),
            kick: Arc::new(tokio::sync::Notify::new()),
            tasks: Mutex::new(Vec::new()),
            dials: AtomicU64::new(0),
            streams: AtomicU64::new(0),
            hosts: Mutex::new(HashMap::new()),
            #[cfg(test)]
            offline: false,
        })
    }

    /// 시험용: 부트스트랩이 실제 Tor 망에서 디렉터리를 받지 않게 합니다. 시험이 망 상태에 따라 달라지지 않게 하려는 것으로,
    /// Windows에서 멈추던 것은 합의문 해석 중의 saturating-time 무한 반복이었습니다(`system_time_limits_finish`).
    #[cfg(test)]
    pub(crate) fn offline_for_test(state_dir: PathBuf, cache_dir: PathBuf) -> Arc<Self> {
        let mut s = Arc::into_inner(Self::new(state_dir, cache_dir)).expect("방금 만든 Arc");
        s.offline = true;
        Arc::new(s)
    }

    /// 이 상태가 그 데이터 경로를 쓰는지(다시 띄울 때 이어 쓸 수 있는지).
    pub(crate) fn uses_dirs(
        &self,
        state_dir: &std::path::Path,
        cache_dir: &std::path::Path,
    ) -> bool {
        self.state_dir == state_dir && self.cache_dir == cache_dir
    }

    /// 지금까지의 Arti 다이얼·앱 스트림 시도 수.
    pub fn activity(&self) -> TorActivity {
        TorActivity {
            dials: self.dials.load(Ordering::Relaxed),
            streams: self.streams.load(Ordering::Relaxed),
        }
    }

    /// libp2p 밖의 앱 스트림(링크 미리보기 등)을 Arti로 엽니다. `host`는 이름·IP 문자열·`xxx.onion`이고 이름은
    /// 출구 노드가 풉니다. 루프백·사설 주소와 `localhost`는 Arti 설정(`allow_local_addrs` 꺼짐)으로 거절됩니다.
    pub async fn connect_stream(
        &self,
        host: &str,
        port: u16,
        isolation: TorIsolation,
    ) -> Result<DataStream, TorTransportError> {
        self.streams.fetch_add(1, Ordering::Relaxed);
        let client = self.client().await?;
        let mut prefs = StreamPrefs::new();
        prefs.set_isolation(isolation.token);
        Ok(client.connect_with_prefs((host, port), &prefs).await?)
    }

    /// 부트스트랩 진행률(0~100, 100은 회로를 만들 수 있을 때만). Arti를 아직 시작하지 않았으면 `None`.
    ///
    /// 이벤트로 받아 두지 않고 읽을 때 계산합니다: 합의문이 시간이 지나 만료되는 것은 이벤트가 오지 않습니다.
    pub fn progress(&self) -> Option<u8> {
        self.client.get().map(|c| {
            let s = c.bootstrap_status();
            bootstrap_pct(s.as_frac(), s.ready_for_traffic())
        })
    }

    /// 브리지를 바꿉니다. 같으면 아무것도 하지 않고, 클라이언트가 떠 있으면 바로 다시 설정합니다(Arti가 회로를 새로
    /// 만듦). 브리지 줄이 깨졌으면 바꾸지 않고 오류입니다.
    pub fn set_bridges(&self, bridges: Option<TorBridges>) -> Result<(), TorTransportError> {
        if let Some(b) = &bridges {
            self.config_with(Some(b))?;
        }
        {
            let mut cur = lock(&self.bridges);
            if *cur == bridges {
                return Ok(());
            }
            *cur = bridges;
        }
        if let Some(client) = self.client.get() {
            client.reconfigure(&self.client_config()?, Reconfigure::WarnOnFailures)?;
            self.kick.notify_one();
            wake_dir_refresh(client);
            info!("Tor 브리지 설정을 바꿈");
        }
        Ok(())
    }

    /// Arti를 시작했는지.
    #[cfg(test)]
    pub fn is_started(&self) -> bool {
        self.client.initialized()
    }

    /// Arti 클라이언트를 돌려줍니다. 처음 호출이면 만들고 백그라운드에서 부트스트랩합니다.
    pub async fn client(&self) -> Result<Arc<TorClient<PreferredRuntime>>, TorTransportError> {
        self.client.get_or_try_init(|| self.create()).await.cloned()
    }

    async fn create(&self) -> Result<Arc<TorClient<PreferredRuntime>>, TorTransportError> {
        // TorClient::builder()는 tokio 런타임 밖에서 패닉하므로 미리 확인합니다.
        tokio::runtime::Handle::try_current().map_err(|_| {
            TorTransportError::Config("tokio 런타임 밖에서는 Arti를 시작할 수 없음".into())
        })?;
        for dir in [&self.state_dir, &self.cache_dir] {
            create_private_dir(dir)?;
        }
        drop_unreadable_state(&self.state_dir);
        let config = self.client_config()?;
        let client = TorClient::builder()
            .config(config)
            .create_unbootstrapped_async()
            .await?;
        info!("Arti 시작");

        // 준비될 때까지 이 작업이 부트스트랩을 맡고, 그 뒤에는 디렉터리 갱신을 지켜봅니다. 네트워크를 다시 띄워 스웜이
        // 바뀌어도 이어집니다.
        let c = client.clone();
        let kick = self.kick.clone();
        let bootstrap = tokio::spawn(async move {
            let mut delay = BOOTSTRAP_RETRY_MIN;
            loop {
                if let Err(e) = c.bootstrap().await {
                    warn!(error = %e, retry_secs = delay.as_secs(), "Tor 부트스트랩 실패, 다시 시도");
                }
                tokio::select! {
                    _ = wait_ready(&c) => break,
                    _ = tokio::time::sleep(delay) => delay = (delay * 2).min(BOOTSTRAP_RETRY_MAX),
                    // 브리지를 바꿨으면 바로 다시(물러난 대기를 처음부터).
                    _ = kick.notified() => delay = BOOTSTRAP_RETRY_MIN,
                }
            }
            // Arti는 합의문 받기를 거듭 실패하면 재시도 대기를 끝없이 늘립니다(무작위로 최대 3배씩). 디렉터리가 만료돼
            // 있는 동안 깨워 Tor 망이 회복되면 곧 새 합의문을 받게 합니다. 신선할 때 깨우면 정한 갱신이 당겨지므로 그때는 두기.
            loop {
                tokio::time::sleep(DIR_WAKE).await;
                if c.bootstrap_status().as_frac() < 0.99 {
                    wake_dir_refresh(&c);
                }
            }
        });
        let mut tasks = match self.tasks.lock() {
            Ok(t) => t,
            Err(e) => e.into_inner(),
        };
        tasks.push(bootstrap.abort_handle());
        Ok(client)
    }

    fn client_config(&self) -> Result<TorClientConfig, TorTransportError> {
        let bridges = lock(&self.bridges).clone();
        match self.config_with(bridges.as_ref()) {
            // 브리지 설정이 깨져도 onion 주소·Tor 시작은 막지 않습니다. 오류 내용은 브리지 줄을 담을 수 있어 남기지 않음.
            Err(_) if bridges.is_some() => {
                warn!("브리지 설정을 쓰지 못해 브리지 없이 Tor를 설정함");
                self.config_with(None)
            }
            r => r,
        }
    }

    fn config_with(
        &self,
        bridges: Option<&TorBridges>,
    ) -> Result<TorClientConfig, TorTransportError> {
        let mut b = TorClientConfigBuilder::from_directories(&self.state_dir, &self.cache_dir);
        // 기본값(128번)이면 Tor 망에 새 합의문이 오래 없을 때 갱신을 포기해, 다시 켤 때까지 옛 목록을 쓰다 끊깁니다.
        b.download_schedule().retry_bootstrap().attempts(u32::MAX);
        // /data/user/0을 system이 가져 Arti의 권한 검사를 통과하지 못합니다. 앱 폴더는 OS 샌드박스가 지킵니다.
        #[cfg(target_os = "android")]
        b.storage().permissions().dangerously_trust_everyone();
        if let Some(br) = bridges {
            for line in &br.lines {
                let bridge: BridgeConfigBuilder = line.parse().map_err(|e| {
                    TorTransportError::Config(format!("브리지 줄을 읽지 못함: {e}"))
                })?;
                b.bridges().bridges().push(bridge);
            }
            let mut t = TransportConfigBuilder::default();
            t.protocols(
                LYREBIRD_TRANSPORTS
                    .iter()
                    .map(|p| p.parse().expect("고정 전송 이름"))
                    .collect(),
            )
            .path(CfgPath::new_literal(br.lyrebird.clone()))
            .run_on_startup(false);
            b.bridges().transports().push(t);
        }
        #[cfg(test)]
        if self.offline {
            // 연결이 거부되는 루프백 주소 하나만 디렉터리 캐시로 둡니다(키는 아무 값).
            let mut fallback = arti_client::config::dir::FallbackDir::builder();
            fallback
                .rsa_identity([23; 20].into())
                .ed_identity([99; 32].into())
                .orports()
                .push("127.0.0.1:1".parse().expect("고정 주소"));
            b.tor_network().set_fallback_caches(vec![fallback]);
        }
        b.build()
            .map_err(|e| TorTransportError::Config(e.to_string()))
    }

    /// onion 서비스의 신원 키(주소)를 읽거나 처음이면 만듭니다. 서비스를 띄우지 않으므로 상태 잠금을 잡지 않습니다.
    fn onion_identity(&self, svc: OnionServiceConfig) -> Result<HsId, TorTransportError> {
        TorClient::<PreferredRuntime>::create_onion_service(&self.client_config()?, svc)?
            .generate_identity_key(Default::default())
            .map_err(|e| TorTransportError::Service(e.to_string()))
    }

    /// 서비스 이름의 onion 주소(`xxxx.onion`). 망 없이 키에서 정해지고, 키가 없으면 만듭니다.
    fn onion_host(&self, nickname: &'static str) -> Result<String, TorTransportError> {
        if let Some(h) = lock(&self.hosts).get(nickname) {
            return Ok(h.clone());
        }
        for dir in [&self.state_dir, &self.cache_dir] {
            create_private_dir(dir)?;
        }
        let host = self
            .onion_identity(service_config(nickname)?)?
            .display_unredacted()
            .to_string();
        lock(&self.hosts).insert(nickname, host.clone());
        Ok(host)
    }

    /// 이 기기의 onion 주소(`xxxx.onion`). 연락처 링크·친구 카드에 넣습니다.
    pub fn device_onion_host(&self) -> Result<String, TorTransportError> {
        self.onion_host(ONION_NICKNAME)
    }

    /// onion 서비스의 키와 상태를 지웁니다(폐기된 기기의 데이터를 지울 때, 예전 친구 전용 키도). 다음에 띄우면 새 주소가
    /// 되어 지운 신원과 이어지지 않습니다. 떠 있는 서비스는 키를 잃어 새 연결을 받지 못하므로 다시 띄워야 합니다.
    pub fn forget_onion_keys(&self) {
        lock(&self.hosts).clear();
        for nick in [ONION_NICKNAME, OLD_FRIEND_ONION_NICKNAME] {
            for dir in [
                self.state_dir.join("keystore").join("hss").join(nick),
                self.state_dir.join("hss").join(nick),
            ] {
                match std::fs::remove_dir_all(&dir) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => warn!(error = %e, "onion 서비스 키를 지우지 못함"),
                }
            }
        }
    }
}

/// 브리지 줄 하나를 검사합니다: Tor 브리지 줄 형식이고, 전송은 없거나(일반 브리지) lyrebird가 맡는 것이어야 합니다.
pub fn check_bridge_line(line: &str) -> Result<(), String> {
    let b: BridgeConfigBuilder = line.parse().map_err(|e| format!("{e}"))?;
    if let Some(t) = b.get_transport().filter(|t| !t.is_empty())
        && !LYREBIRD_TRANSPORTS.contains(&t)
    {
        return Err(format!("지원하지 않는 전송: {t}"));
    }
    b.build().map(|_| ()).map_err(|e| e.to_string())
}

/// Arti 부트스트랩 비율을 0~100으로. Arti 비율은 준비 여부와 맞지 않아(조금 만료된 합의문은 회로를 만들 수 있어도
/// 약 92%) 100은 준비됐을 때만, 준비 전에는 99까지로 둡니다.
fn bootstrap_pct(frac: f32, ready: bool) -> u8 {
    if ready {
        100
    } else {
        ((frac.clamp(0.0, 1.0) * 100.0).round() as u8).min(99)
    }
}

/// Arti 디렉터리 작업의 재시도 대기를 깨웁니다. 첫 `bootstrap()` 전에는 작업이 없어 아무것도 하지 않습니다.
fn wake_dir_refresh(client: &TorClient<PreferredRuntime>) {
    if let Some(h) = client.dirmgr().ok().and_then(|d| d.download_task_handle()) {
        h.fire();
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match m.lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    }
}

fn service_config(nickname: &str) -> Result<OnionServiceConfig, TorTransportError> {
    let nickname = HsNickname::new(nickname.to_string())
        .map_err(|e| TorTransportError::Config(e.to_string()))?;
    OnionServiceConfigBuilder::default()
        .nickname(nickname)
        .build()
        .map_err(|e| TorTransportError::Config(e.to_string()))
}

/// Arti가 회로를 만들 수 있을 때(디렉터리·가드 연결)까지 기다립니다.
///
/// `TorClient::bootstrap()`으로 기다리지 않는 까닭: 다른 곳(끊긴 다이얼·내려간 스웜의 작업)에서 진행 중이던
/// 부트스트랩이 취소되면, Arti 0.47은 그 뒤의 `bootstrap()`을 준비 전에도 바로 `Ok`로 돌려줍니다.
async fn wait_ready(client: &TorClient<PreferredRuntime>) {
    // 상태를 읽기 전에 구독해 그 사이의 변화를 놓치지 않습니다.
    let mut events = client.bootstrap_events();
    if client.bootstrap_status().ready_for_traffic() {
        return;
    }
    while let Some(s) = events.next().await {
        if s.ready_for_traffic() {
            return;
        }
    }
    // 이벤트가 끝났으면 클라이언트가 내려간 것이라 준비될 일이 없습니다(부르는 작업이 함께 멈춤).
    futures::future::pending::<()>().await;
}

fn create_private_dir(dir: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    // Arti는 다른 사용자가 쓸 수 있는 디렉터리를 거부하므로 소유자 전용으로 맞춥니다.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// 읽을 수 없는 Arti 상태 파일을 지웁니다. 전원이 끊기면 디스크에 내려가지 못한 파일이 NUL만 남는데, 그대로 두면 onion
/// 서비스가 시작할 때마다 실패해 아무도 이 기기로 들어오지 못합니다. 이 파일들은 없으면 Arti가 새로 시작하고, onion 주소를
/// 정하는 키는 `keystore`에 따로 있어 주소는 그대로입니다.
fn drop_unreadable_state(state_dir: &std::path::Path) {
    let entries = |dir: &std::path::Path| -> Vec<PathBuf> {
        let list = std::fs::read_dir(dir).into_iter().flatten().flatten();
        list.map(|e| e.path()).collect()
    };
    let services = entries(&state_dir.join("hss"));
    let replay_logs = services.iter().flat_map(|s| entries(&s.join("iptreplay")));
    let service_files = services.iter().flat_map(|s| entries(s));
    let files = service_files
        .chain(replay_logs)
        .chain(entries(&state_dir.join("state")));
    for path in files {
        let Ok(raw) = std::fs::read(&path) else {
            continue;
        };
        let readable = match path.extension().and_then(|e| e.to_str()) {
            Some("json") => serde_json::from_slice::<serde_json::Value>(&raw).is_ok(),
            // 재전송 기록은 정해진 머리로 시작하고, 머리가 틀리면 Arti가 서비스를 띄우지 않습니다.
            Some("bin") => raw.first() != Some(&0),
            _ => true,
        };
        if readable {
            continue;
        }
        let file = path.file_name();
        match std::fs::remove_file(&path) {
            Ok(()) => warn!(?file, "읽을 수 없는 Tor 상태 파일을 지움"),
            Err(e) => warn!(?file, error = %e, "읽을 수 없는 Tor 상태 파일을 지우지 못함"),
        }
    }
}

/// 전송이 여는 onion 서비스.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OnionListen {
    /// 수신하지 않음.
    Off,
    /// 주소를 스웜에 알립니다(상태·연락처에 실림, Tor 스웜은 identify가 꺼져 있음).
    Public,
}

impl OnionListen {
    fn nickname(self) -> Option<&'static str> {
        match self {
            OnionListen::Off => None,
            OnionListen::Public => Some(ONION_NICKNAME),
        }
    }
}

enum ListenerMsg {
    Address(Multiaddr),
    /// onion 서비스가 멈춤(다시 띄우는 중). 그동안은 이 주소로 연결받을 수 없습니다.
    Expired(Multiaddr),
    Incoming(Box<DataStream>),
}

struct OnionListener {
    id: ListenerId,
    rx: mpsc::Receiver<ListenerMsg>,
    task: tokio::task::JoinHandle<()>,
    addr: Option<Multiaddr>,
}

/// Arti 전송.
pub struct ArtiTransport {
    shared: Arc<ArtiShared>,
    listen: OnionListen,
    /// 켜면 연결마다 새 격리 토큰을 줘 회로를 따로 만듭니다([`NetConfig::tor_isolated`](crate::NetConfig)).
    isolate_each: bool,
    listeners: Vec<OnionListener>,
    pending:
        VecDeque<TransportEvent<Ready<Result<DataStream, TorTransportError>>, TorTransportError>>,
}

impl ArtiTransport {
    pub fn new(shared: Arc<ArtiShared>, listen: OnionListen, isolate_each: bool) -> Self {
        ArtiTransport {
            shared,
            listen,
            isolate_each,
            listeners: Vec::new(),
            pending: VecDeque::new(),
        }
    }

    /// Arti로 걸 수 있는 주소인지(onion·이름·IP의 TCP만).
    pub fn accepts(addr: &Multiaddr) -> Option<TorTarget> {
        addr::tor_target(addr)
    }
}

impl Drop for ArtiTransport {
    fn drop(&mut self) {
        for l in &self.listeners {
            l.task.abort();
        }
    }
}

impl Transport for ArtiTransport {
    type Output = DataStream;
    type Error = TorTransportError;
    type ListenerUpgrade = Ready<Result<DataStream, TorTransportError>>;
    type Dial = BoxFuture<'static, Result<DataStream, TorTransportError>>;

    fn listen_on(
        &mut self,
        id: ListenerId,
        addr: Multiaddr,
    ) -> Result<(), TransportError<Self::Error>> {
        let (Some(port), Some(nickname)) = (addr::onion_marker_port(&addr), self.listen.nickname())
        else {
            return Err(TransportError::MultiaddrNotSupported(addr));
        };
        let (tx, rx) = mpsc::channel(32);
        let shared = self.shared.clone();
        let task = tokio::spawn(run_onion_listener(shared, nickname, port, tx));
        self.listeners.push(OnionListener {
            id,
            rx,
            task,
            addr: None,
        });
        Ok(())
    }

    fn remove_listener(&mut self, id: ListenerId) -> bool {
        let Some(pos) = self.listeners.iter().position(|l| l.id == id) else {
            return false;
        };
        let l = self.listeners.remove(pos);
        l.task.abort();
        self.pending.push_back(TransportEvent::ListenerClosed {
            listener_id: id,
            reason: Ok(()),
        });
        true
    }

    fn dial(
        &mut self,
        addr: Multiaddr,
        opts: DialOpts,
    ) -> Result<Self::Dial, TransportError<Self::Error>> {
        // 홀펀칭(상대가 다이얼러 역할)은 Tor 위에서 의미가 없습니다.
        if opts.role != Endpoint::Dialer {
            return Err(TransportError::MultiaddrNotSupported(addr));
        }
        let Some(target) = Self::accepts(&addr) else {
            return Err(TransportError::MultiaddrNotSupported(addr));
        };
        self.shared.dials.fetch_add(1, Ordering::Relaxed);
        let shared = self.shared.clone();
        let isolation = self.isolate_each.then(TorIsolation::new);
        Ok(async move {
            let client = shared.client().await?;
            let tor_addr = match target {
                TorTarget::Onion { host, port } | TorTarget::Dns { host, port } => {
                    TorAddr::from((host, port))?
                }
                TorTarget::Ip(sa) => TorAddr::dangerously_from(sa)?,
            };
            let stream = match isolation {
                Some(i) => {
                    let mut prefs = StreamPrefs::new();
                    prefs.set_isolation(i.token);
                    client.connect_with_prefs(tor_addr, &prefs).await?
                }
                None => client.connect(tor_addr).await?,
            };
            debug!("Tor 경유 연결 성립");
            Ok(stream)
        }
        .boxed())
    }

    fn poll(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<TransportEvent<Self::ListenerUpgrade, Self::Error>> {
        if let Some(ev) = self.pending.pop_front() {
            return Poll::Ready(ev);
        }
        let mut i = 0;
        while i < self.listeners.len() {
            let l = &mut self.listeners[i];
            match l.rx.poll_recv(cx) {
                Poll::Ready(Some(ListenerMsg::Address(a))) => {
                    l.addr = Some(a.clone());
                    return Poll::Ready(TransportEvent::NewAddress {
                        listener_id: l.id,
                        listen_addr: a,
                    });
                }
                Poll::Ready(Some(ListenerMsg::Expired(a))) => {
                    if l.addr.as_ref() == Some(&a) {
                        l.addr = None;
                    }
                    return Poll::Ready(TransportEvent::AddressExpired {
                        listener_id: l.id,
                        listen_addr: a,
                    });
                }
                Poll::Ready(Some(ListenerMsg::Incoming(stream))) => {
                    let local_addr = l.addr.clone().unwrap_or_else(Multiaddr::empty);
                    return Poll::Ready(TransportEvent::Incoming {
                        listener_id: l.id,
                        upgrade: futures::future::ready(Ok(*stream)),
                        local_addr,
                        // onion 서비스는 상대 주소를 알 수 없습니다(익명).
                        send_back_addr: Multiaddr::empty(),
                    });
                }
                Poll::Ready(None) => {
                    let id = l.id;
                    self.listeners.remove(i);
                    return Poll::Ready(TransportEvent::ListenerClosed {
                        listener_id: id,
                        reason: Ok(()),
                    });
                }
                Poll::Pending => i += 1,
            }
        }
        Poll::Pending
    }
}

/// onion 서비스 수신 작업. 서비스가 실패하거나 멈추면 간격을 늘려 가며 다시 띄웁니다.
///
/// 주소는 같은 키에서 늘 같으므로 한 번 알리면 서비스가 잠시 멈춰도 거두지 않습니다(그동안은 연결받지 못함).
/// 같은 프로세스에서 네트워크를 다시 띄우면 이전 서비스의 내부 작업이 상태 잠금을 잠시 더 쥐고 있어 첫 시도가
/// 실패할 수 있습니다. 수신을 영영 잃지 않도록 전송 계층이 이 리스너를 버릴 때(채널 닫힘)까지 재시도합니다.
async fn run_onion_listener(
    shared: Arc<ArtiShared>,
    nickname: &'static str,
    port: u16,
    tx: mpsc::Sender<ListenerMsg>,
) {
    let mut delay = ONION_RETRY_MIN;
    let mut announced = None;
    loop {
        let res = run_onion_service(&shared, nickname, port, &tx, &mut announced).await;
        if tx.is_closed() {
            return;
        }
        match res {
            Ok(()) => {
                debug!("onion 서비스가 멈춤, 다시 시작");
                delay = ONION_RETRY_MIN;
            }
            Err(e) => {
                warn!(error = %e, retry_secs = delay.as_secs(), "onion 서비스를 띄우지 못함, 다시 시도")
            }
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(ONION_RETRY_MAX);
    }
}

/// onion 주소를 알리고(`announced`에 남김) 부트스트랩이 끝나면 서비스를 띄워, 멈출 때까지 들어오는 스트림을
/// 수신 채널로 넘깁니다. 채널이 닫혔거나 띄운 서비스가 멈추면 `Ok`.
async fn run_onion_service(
    shared: &ArtiShared,
    nickname: &'static str,
    port: u16,
    tx: &mpsc::Sender<ListenerMsg>,
    announced: &mut Option<Multiaddr>,
) -> Result<(), TorTransportError> {
    let client = shared.client().await?;
    let config = service_config(nickname)?;
    // 키는 Tor 망 없이 정해지므로 주소부터 알립니다.
    let host = shared.onion_host(nickname)?;
    let addr = addr::onion_multiaddr(&host, port)
        .ok_or_else(|| TorTransportError::Service("onion 주소 형식 오류".into()))?;
    if announced.as_ref() != Some(&addr) {
        if let Some(old) = announced.take()
            && tx.send(ListenerMsg::Expired(old)).await.is_err()
        {
            return Ok(());
        }
        if tx.send(ListenerMsg::Address(addr.clone())).await.is_err() {
            return Ok(());
        }
        *announced = Some(addr);
    }
    // Arti 0.47: 디렉터리를 받기 전에 띄운 서비스는 멈춰도 설명자 게시 작업이 디렉터리를 기다리며 서비스 상태
    // 잠금을 놓지 않아, 같은 프로세스에서 다시 띄울 수 없게 됩니다(스웜 다시 띄우기·재시작). 그래서 준비된 뒤에 띄웁니다.
    // 부트스트랩은 `ArtiShared`의 작업이 맡습니다.
    wait_ready(&client).await;
    let (service, requests) = client
        .launch_onion_service(config)?
        .ok_or_else(|| TorTransportError::Service("설정에서 비활성화됨".into()))?;
    info!("onion 서비스 시작");

    let limiter = Arc::new(Semaphore::new(MAX_PENDING_ACCEPTS));
    let mut streams = Box::pin(handle_rend_requests(requests));
    while let Some(req) = streams.next().await {
        if tx.is_closed() {
            break;
        }
        let wanted = matches!(req.request(), IncomingStreamRequest::Begin(b) if b.port() == port);
        match (wanted, limiter.clone().try_acquire_owned()) {
            (true, Ok(permit)) => {
                let tx = tx.clone();
                tokio::spawn(async move {
                    accept_stream(req, tx).await;
                    drop(permit);
                });
            }
            _ => {
                // 다른 포트·형식이거나 수락 대기열이 가득 찼으면 정중히(DONE) 닫습니다.
                tokio::spawn(async move {
                    let _ = req.reject(End::new_with_reason(EndReason::DONE)).await;
                });
            }
        }
    }
    drop(service);
    Ok(())
}

async fn accept_stream(req: StreamRequest, tx: mpsc::Sender<ListenerMsg>) {
    match req.accept(Connected::new_empty()).await {
        Ok(stream) => {
            let _ = tx.send(ListenerMsg::Incoming(Box::new(stream))).await;
        }
        Err(e) => debug!(error = %e, "onion 스트림 수락 실패"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ONION: &str = "duckduckgogg42xjoc72x3sjasowoarfbgcmvfimaftt6twagswzczad";

    fn shared() -> Arc<ArtiShared> {
        ArtiShared::new("/nonexistent/state".into(), "/nonexistent/cache".into())
    }

    async fn next_event(
        t: &mut ArtiTransport,
    ) -> TransportEvent<Ready<Result<DataStream, TorTransportError>>, TorTransportError> {
        futures::future::poll_fn(|cx| Pin::new(&mut *t).poll(cx)).await
    }

    #[tokio::test]
    async fn expired_onion_address_keeps_listener() {
        let mut t = ArtiTransport::new(shared(), OnionListen::Public, false);
        let (tx, rx) = mpsc::channel(4);
        let id = ListenerId::next();
        t.listeners.push(OnionListener {
            id,
            rx,
            task: tokio::spawn(async {}),
            addr: None,
        });
        let onion: Multiaddr = format!("/onion3/{ONION}:4001").parse().unwrap();
        tx.send(ListenerMsg::Address(onion.clone())).await.unwrap();
        match next_event(&mut t).await {
            TransportEvent::NewAddress {
                listener_id,
                listen_addr,
            } => assert_eq!((listener_id, listen_addr), (id, onion.clone())),
            _ => panic!("새 주소여야 함"),
        }
        // 서비스가 멈추면 주소만 빠지고 리스너는 남아 다시 뜨기를 기다린다.
        tx.send(ListenerMsg::Expired(onion.clone())).await.unwrap();
        match next_event(&mut t).await {
            TransportEvent::AddressExpired {
                listener_id,
                listen_addr,
            } => assert_eq!((listener_id, listen_addr), (id, onion.clone())),
            _ => panic!("주소 만료여야 함"),
        }
        assert_eq!(t.listeners.len(), 1);
        assert!(t.listeners[0].addr.is_none());
        // 작업이 끝나 채널이 닫혀야 리스너가 닫힌다.
        drop(tx);
        assert!(matches!(
            next_event(&mut t).await,
            TransportEvent::ListenerClosed { reason: Ok(()), .. }
        ));
        assert!(t.listeners.is_empty());
    }

    /// Arti 클라이언트는 네트워크 없이 만들 수 있습니다. 공유 상태가 버려지면 백그라운드 부트스트랩 작업도
    /// 멈춰 클라이언트(와 상태 디렉터리 잠금)를 놓아야 합니다.
    #[tokio::test]
    async fn dropping_shared_state_releases_arti_client() {
        let dir = tempfile::tempdir().unwrap();
        let s = ArtiShared::offline_for_test(dir.path().join("state"), dir.path().join("cache"));
        let client = s.client().await.unwrap();
        assert!(s.is_started());
        assert!(s.progress().is_some());
        let weak = Arc::downgrade(&client);
        drop(client);
        assert!(weak.upgrade().is_some(), "공유 상태가 있는 동안은 유지");
        drop(s);
        for _ in 0..100 {
            if weak.upgrade().is_none() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("공유 상태를 버린 뒤에도 Arti 클라이언트가 살아 있음");
    }

    /// 정전으로 NUL만 남은 상태 파일이 있어도 onion 서비스가 뜨고 주소는 그대로입니다. 멀쩡한 파일은 건드리지 않습니다.
    #[tokio::test]
    async fn unreadable_state_files_do_not_stop_onion_service() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let s = ArtiShared::offline_for_test(state.clone(), dir.path().join("cache"));
        let host = s.device_onion_host().unwrap();
        let hs = state.join("hss").join(ONION_NICKNAME);
        create_private_dir(&hs).unwrap();
        std::fs::write(hs.join("iptpub.json"), [0u8; 1354]).unwrap();
        let kept = hs.join("note.json");
        std::fs::write(&kept, b"{}").unwrap();
        let replay = hs.join("iptreplay");
        create_private_dir(&replay).unwrap();
        let lost_log = replay.join(format!("{}.bin", "1".repeat(64)));
        std::fs::write(&lost_log, [0u8; 48]).unwrap();
        let kept_log = replay.join(format!("{}.bin", "2".repeat(64)));
        std::fs::write(&kept_log, b"<tor hss replay Kangaroo12>\n\0\0\0\0").unwrap();

        let client = s.client().await.unwrap();
        let launched = client.launch_onion_service(service_config(ONION_NICKNAME).unwrap());
        assert!(launched.is_ok_and(|l| l.is_some()), "서비스가 떠야 함");
        assert!(kept.exists(), "읽을 수 있는 파일은 남김");
        assert!(!lost_log.exists(), "머리가 NUL인 재전송 기록은 지움");
        assert!(kept_log.exists(), "머리가 맞는 재전송 기록은 남김");
        assert_eq!(s.device_onion_host().unwrap(), host);
    }

    /// 내장 브리지는 Arti 설정이 되고, 깨진 줄은 바꾸지 않고 오류입니다. 떠 있는 클라이언트도 다시 설정되며,
    /// 브리지 설정이 깨져도 onion 주소는 나옵니다.
    #[tokio::test]
    async fn bridges_validate_and_reconfigure_running_client() {
        use crate::config::{BUILTIN_OBFS4, BUILTIN_SNOWFLAKE};
        let dir = tempfile::tempdir().unwrap();
        let s = ArtiShared::offline_for_test(dir.path().join("state"), dir.path().join("cache"));
        let bridges = |lines: &[&str]| TorBridges {
            lines: lines.iter().map(|l| l.to_string()).collect(),
            lyrebird: dir.path().join("lyrebird.exe"),
        };
        for lines in [&BUILTIN_OBFS4[..], &BUILTIN_SNOWFLAKE[..]] {
            s.config_with(Some(&bridges(lines)))
                .expect("내장 브리지 설정");
            for l in lines {
                check_bridge_line(l).expect("내장 브리지 줄");
            }
        }
        // webtunnel(직접 넣는 줄)과 전송 없는 일반 브리지는 받고, lyrebird가 맡지 않는 전송은 거절.
        check_bridge_line("webtunnel [2001:db8::1]:443 0123456789ABCDEF0123456789ABCDEF01234567 url=https://example.com/abc ver=0.0.1").unwrap();
        check_bridge_line("192.0.2.9:9001 0123456789ABCDEF0123456789ABCDEF01234567").unwrap();
        assert!(
            check_bridge_line("conjure 192.0.2.1:80 0123456789ABCDEF0123456789ABCDEF01234567")
                .is_err()
        );
        assert!(check_bridge_line("아무 글").is_err());
        assert!(
            s.set_bridges(Some(bridges(&["obfs4 이건 브리지가 아님"])))
                .is_err()
        );
        assert!(lock(&s.bridges).is_none(), "깨진 줄은 저장하지 않음");

        let _client = s.client().await.unwrap();
        s.set_bridges(Some(bridges(&BUILTIN_SNOWFLAKE)))
            .expect("떠 있는 클라이언트에 브리지");
        s.set_bridges(Some(bridges(&BUILTIN_OBFS4)))
            .expect("브리지 바꾸기");
        s.set_bridges(None).expect("브리지 끄기");

        // 저장된 설정이 깨졌어도(옛 판이 남긴 값 등) 브리지 없이 설정해 onion 주소를 냄.
        *lock(&s.bridges) = Some(bridges(&["obfs4 이건 브리지가 아님"]));
        assert!(s.device_onion_host().unwrap().ends_with(".onion"));
    }

    /// 실제 Tor 망에서 내장 브리지로 부트스트랩하고, 앱이 쓰는 onion 경로(남의 onion에 걸기, 내 onion 서비스를 브리지로
    /// 알리고 브리지 없는 클라이언트가 걸어 오기)를 확인합니다. 사람이 직접 돌립니다:
    /// `YANGPA_LYREBIRD=<lyrebird 경로> [YANGPA_BRIDGE=obfs4] cargo test -p yangpa-net real_bridge -- --ignored --nocapture`
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "실제 Tor 망과 lyrebird가 필요"]
    async fn real_bridge_bootstrap() {
        use crate::config::{BUILTIN_OBFS4, BUILTIN_SNOWFLAKE};
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_test_writer()
            .try_init();
        let lyrebird = std::env::var_os("YANGPA_LYREBIRD")
            .expect("YANGPA_LYREBIRD")
            .into();
        let lines = match std::env::var("YANGPA_BRIDGE").as_deref() {
            Ok("obfs4") => &BUILTIN_OBFS4[..],
            _ => &BUILTIN_SNOWFLAKE[..],
        };
        let dir = tempfile::tempdir().unwrap();
        let s = ArtiShared::new(dir.path().join("state"), dir.path().join("cache"));
        s.set_bridges(Some(TorBridges {
            lines: lines.iter().map(|l| l.to_string()).collect(),
            lyrebird,
        }))
        .unwrap();
        let client = s.client().await.unwrap();
        let started = std::time::Instant::now();
        tokio::time::timeout(Duration::from_secs(600), wait_ready(&client))
            .await
            .expect("10분 안에 브리지로 부트스트랩");
        eprintln!("준비됨: {:?}", started.elapsed());
        let isolation = TorIsolation::new();
        s.connect_stream("check.torproject.org", 443, isolation)
            .await
            .expect("브리지로 연 회로로 스트림");

        let opts = DialOpts {
            role: Endpoint::Dialer,
            port_use: Default::default(),
        };
        let started = std::time::Instant::now();
        let mut out = ArtiTransport::new(s.clone(), OnionListen::Off, false);
        let ddg: Multiaddr = format!("/onion3/{ONION}:443").parse().unwrap();
        tokio::time::timeout(Duration::from_secs(180), out.dial(ddg, opts).unwrap())
            .await
            .expect("시간 초과")
            .expect("브리지로 남의 onion에 걸기");
        eprintln!("남의 onion에 걸림: {:?}", started.elapsed());

        // 브리지로 띄운 내 onion 서비스에 브리지 없는 클라이언트가 걸어 옵니다(서비스 기술서가 브리지로 올라갔는지).
        let started = std::time::Instant::now();
        let mut svc = ArtiTransport::new(s.clone(), OnionListen::Public, false);
        svc.listen_on(ListenerId::next(), addr::onion_listen_marker(4001))
            .unwrap();
        let mine = match next_event(&mut svc).await {
            TransportEvent::NewAddress { listen_addr, .. } => listen_addr,
            _ => panic!("새 주소여야 함"),
        };
        let other_dir = tempfile::tempdir().unwrap();
        let plain = ArtiShared::new(
            other_dir.path().join("state"),
            other_dir.path().join("cache"),
        );
        let mut dialer = ArtiTransport::new(plain, OnionListen::Off, false);
        let dial = async {
            loop {
                match tokio::time::timeout(
                    Duration::from_secs(90),
                    dialer.dial(mine.clone(), opts).unwrap(),
                )
                .await
                {
                    Ok(Ok(stream)) => break stream,
                    // 서비스 기술서가 올라가기 전이면 실패하니 다시 겁니다.
                    _ => tokio::time::sleep(Duration::from_secs(10)).await,
                }
            }
        };
        let incoming = async {
            loop {
                if let TransportEvent::Incoming { .. } = next_event(&mut svc).await {
                    break;
                }
            }
        };
        tokio::time::timeout(
            Duration::from_secs(600),
            futures::future::join(dial, incoming),
        )
        .await
        .expect("브리지로 띄운 onion 서비스에 10분 안에 닿음");
        eprintln!("내 onion 서비스에 닿음: {:?}", started.elapsed());
    }

    /// Tor 망 없이도 onion 주소는 바로 나오고(서비스는 부트스트랩 뒤에 띄움), 같은 상태 경로로 함께 띄워도
    /// 같은 주소를 알리며 리스너는 닫히지 않고 기다린다.
    #[tokio::test]
    async fn onion_listener_announces_stable_address_before_bootstrap() {
        let dir = tempfile::tempdir().unwrap();
        let first =
            ArtiShared::offline_for_test(dir.path().join("state"), dir.path().join("cache"));
        let mut t1 = ArtiTransport::new(first, OnionListen::Public, false);
        t1.listen_on(ListenerId::next(), addr::onion_listen_marker(4001))
            .unwrap();
        let addr1 = match tokio::time::timeout(Duration::from_secs(20), next_event(&mut t1))
            .await
            .expect("첫 onion 서비스")
        {
            TransportEvent::NewAddress { listen_addr, .. } => listen_addr,
            _ => panic!("새 주소여야 함"),
        };
        assert!(addr::is_onion(&addr1));

        // 같은 상태 디렉터리로 두 번째 리스너: 같은 키라 같은 주소이고, 부트스트랩을 기다리며 살아 있어야 한다.
        let second =
            ArtiShared::offline_for_test(dir.path().join("state"), dir.path().join("cache"));
        let mut t2 = ArtiTransport::new(second, OnionListen::Public, false);
        t2.listen_on(ListenerId::next(), addr::onion_listen_marker(4001))
            .unwrap();
        match tokio::time::timeout(Duration::from_secs(20), next_event(&mut t2))
            .await
            .expect("두 번째 주소")
        {
            TransportEvent::NewAddress { listen_addr, .. } => assert_eq!(listen_addr, addr1),
            _ => panic!("새 주소여야 함"),
        }
        let res = tokio::time::timeout(Duration::from_secs(3), next_event(&mut t2)).await;
        assert!(res.is_err(), "리스너가 닫히거나 주소가 바뀌면 안 됨");
        assert_eq!(t2.listeners.len(), 1);
        assert!(!t2.listeners[0].task.is_finished(), "재시도 중이어야 함");
    }

    /// 기기 데이터를 지우면 onion 키(예전 친구 전용 키도)가 지워져 다음에는 새 주소가 됩니다(지운 신원과 이어지지 않게).
    #[test]
    fn forgetting_onion_keys_changes_addresses() {
        let dir = tempfile::tempdir().unwrap();
        let (state, cache) = (dir.path().join("state"), dir.path().join("cache"));
        let s = ArtiShared::new(state.clone(), cache.clone());
        let before = s.device_onion_host().unwrap();
        let old = state
            .join("keystore")
            .join("hss")
            .join(OLD_FRIEND_ONION_NICKNAME);
        std::fs::create_dir_all(&old).unwrap();
        assert!(
            state
                .join("keystore")
                .join("hss")
                .join(ONION_NICKNAME)
                .is_dir()
        );
        s.forget_onion_keys();
        for nick in [ONION_NICKNAME, OLD_FRIEND_ONION_NICKNAME] {
            assert!(!state.join("keystore").join("hss").join(nick).exists());
        }
        let after = ArtiShared::new(state, cache).device_onion_host().unwrap();
        assert_ne!(before, after);
        assert_eq!(s.device_onion_host().unwrap(), after, "캐시도 비워야 함");
    }

    #[test]
    fn accepts_tcp_and_onion_only() {
        let onion: Multiaddr = format!("/onion3/{ONION}:4001").parse().unwrap();
        let tcp: Multiaddr = "/ip4/1.2.3.4/tcp/4001".parse().unwrap();
        let dns: Multiaddr = "/dns/example.org/tcp/4001".parse().unwrap();
        let quic: Multiaddr = "/ip4/1.2.3.4/udp/4001/quic-v1".parse().unwrap();
        assert!(ArtiTransport::accepts(&onion).is_some());
        assert!(ArtiTransport::accepts(&tcp).is_some());
        assert!(ArtiTransport::accepts(&dns).is_some());
        assert!(ArtiTransport::accepts(&quic).is_none());
    }

    #[tokio::test]
    async fn transport_is_lazy_and_rejects_unsupported() {
        let s = shared();
        let mut t = ArtiTransport::new(s.clone(), OnionListen::Off, false);
        let quic: Multiaddr = "/ip4/1.2.3.4/udp/4001/quic-v1".parse().unwrap();
        let opts = DialOpts {
            role: Endpoint::Dialer,
            port_use: Default::default(),
        };
        assert!(matches!(
            t.dial(quic, opts),
            Err(TransportError::MultiaddrNotSupported(_))
        ));
        // onion 다이얼은 미래 객체만 만들고, 기다리기 전에는 Arti를 시작하지 않습니다.
        let onion: Multiaddr = format!("/onion3/{ONION}:4001").parse().unwrap();
        let fut = t.dial(onion, opts).ok();
        assert!(fut.is_some());
        drop(fut);
        assert!(!s.is_started());
        assert_eq!(s.progress(), None);
        // 수신을 허락하지 않은 전송은 onion 서비스를 열지 않습니다.
        let marker = addr::onion_listen_marker(4001);
        assert!(t.listen_on(ListenerId::next(), marker).is_err());
        // 홀펀칭 역할 다이얼은 거부.
        let onion: Multiaddr = format!("/onion3/{ONION}:4001").parse().unwrap();
        let listener_role = DialOpts {
            role: Endpoint::Listener,
            port_use: Default::default(),
        };
        assert!(t.dial(onion, listener_role).is_err());
    }

    /// 합의문이 조금 만료돼 Arti 비율이 0.915에 머물러도 회로를 만들 수 있으면 100, 준비 전에는 100이 되지 않습니다.
    #[test]
    fn bootstrap_pct_follows_readiness() {
        assert_eq!(bootstrap_pct(0.915, true), 100);
        assert_eq!(bootstrap_pct(0.999, false), 99);
        assert_eq!(bootstrap_pct(0.4, false), 40);
        assert_eq!(bootstrap_pct(0.0, false), 0);
    }

    /// Arti(tor-netdoc)가 받은 합의문을 해석할 때 처음 계산하는 `SystemTime` 한계가 끝나는지. saturating-time 0.5.0은
    /// Windows(100ns 시계)에서 끝나지 않아 부트스트랩이 15%에서 멈췄습니다(루트 Cargo.toml의 `[patch.crates-io]`).
    #[test]
    fn system_time_limits_finish() {
        use saturating_time::SaturatingTime;
        use std::time::SystemTime;

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send((SystemTime::min_value(), SystemTime::max_value()));
        });
        let (min, max) = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("시각 한계 계산이 끝나지 않음");
        assert!(min < SystemTime::UNIX_EPOCH && SystemTime::UNIX_EPOCH < max);
        assert_eq!(min.checked_sub(Duration::from_micros(1)), None);
        assert_eq!(max.checked_add(Duration::from_micros(1)), None);
    }

    /// 실제 Tor 네트워크가 필요합니다: `cargo test -p yangpa-net -- --ignored tor_network`.
    #[tokio::test]
    #[ignore]
    async fn tor_network_bootstrap_and_onion_dial() {
        let dir = tempfile::tempdir().unwrap();
        let s = ArtiShared::new(dir.path().join("state"), dir.path().join("cache"));
        let mut t = ArtiTransport::new(s.clone(), OnionListen::Off, false);
        let onion: Multiaddr = format!("/onion3/{ONION}:443").parse().unwrap();
        let opts = DialOpts {
            role: Endpoint::Dialer,
            port_use: Default::default(),
        };
        let stream = tokio::time::timeout(
            std::time::Duration::from_secs(180),
            t.dial(onion, opts).unwrap(),
        )
        .await
        .expect("시간 초과")
        .expect("onion 연결");
        drop(stream);
        assert!(s.progress().unwrap_or(0) > 0);
    }
}
