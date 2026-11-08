//! `yangpad`: 양파 메신저 코어 데몬. 노드를 띄우고 로컬 API(JSON-RPC)를 엽니다.
//!
//! 데이터 경로는 `--data-dir` → `YANGPA_DATA_DIR` → OS 기본 경로 순, 엔드포인트는 `--endpoint` →
//! `YANGPA_ENDPOINT` → 기본값 순으로 정합니다. 처음 실행하면 데이터 경로에 소유자 토큰(`owner.token`)을 만듭니다.
//! 로그에는 메시지 내용·키·토큰을 남기지 않습니다. 한 데이터 경로에는 데몬 하나만 뜹니다(`yangpad.lock`).

mod parent;

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::Context;
use clap::builder::BoolishValueParser;
use clap::{Parser, ValueEnum};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;
use yangpa_daemon::host::Host;
use yangpa_daemon::{KeyStorage, NodeConfig, SfuHostMode};
use yangpa_net::Multiaddr;

/// 런타임을 내릴 때 남은 블로킹 작업을 기다리는 시간. tokio 기본은 끝날 때까지 기다려 프로세스가 남을 수 있습니다.
const RUNTIME_SHUTDOWN: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, ValueEnum)]
enum KeyStorageArg {
    /// OS 키 저장소, 안 되면 0600 파일.
    Auto,
    /// OS 키 저장소만.
    OsKeyring,
    /// 데이터 경로의 0600 파일(헤드리스 서버).
    File,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum SfuHostArg {
    /// 통화 SFU를 맡지 않음.
    Never,
    /// 참가 중인 통화에서 연결이 가장 좋으면 맡음.
    Auto,
    /// 먼저 맡겠다고 알림(볼 수 있는 통화의 호스트 후보, 헤드리스 시더용).
    Prefer,
}

/// 예전 `--default-mode` 값(서버 패키지의 `YANGPA_DEFAULT_MODE=normal`을 계속 읽으려고 둠).
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum ModeArg {
    /// Tor로(기본과 같음).
    Tor,
    /// `--direct`와 같음.
    Normal,
}

impl From<SfuHostArg> for SfuHostMode {
    fn from(k: SfuHostArg) -> Self {
        match k {
            SfuHostArg::Never => SfuHostMode::Never,
            SfuHostArg::Auto => SfuHostMode::Auto,
            SfuHostArg::Prefer => SfuHostMode::Prefer,
        }
    }
}

impl From<KeyStorageArg> for KeyStorage {
    fn from(k: KeyStorageArg) -> Self {
        match k {
            KeyStorageArg::Auto => KeyStorage::Auto,
            KeyStorageArg::OsKeyring => KeyStorage::OsKeyring,
            KeyStorageArg::File => KeyStorage::File,
        }
    }
}

/// YANGPA 코어 데몬.
#[derive(Debug, Parser)]
#[command(name = "yangpad", version, about)]
struct Args {
    /// 데이터 경로(DB, 블롭, 소유자 토큰).
    #[arg(long, env = "YANGPA_DATA_DIR")]
    data_dir: Option<PathBuf>,
    /// 로컬 API 엔드포인트(유닉스 소켓 경로 또는 Windows named pipe).
    #[arg(long, env = "YANGPA_ENDPOINT")]
    endpoint: Option<String>,
    /// 수신 주소(여러 번). 비우면 모든 인터페이스의 QUIC·TCP.
    #[arg(long = "listen", value_name = "MULTIADDR")]
    listen: Vec<Multiaddr>,
    /// 부트스트랩·릴레이 노드 주소(여러 번, `/p2p/<id>` 포함). 상시 노드 설정은 사용자 설정에서 빠졌고, 시험·운영용으로만
    /// 남긴 옵션이라 도움말에 보이지 않습니다.
    #[arg(long = "bootstrap", value_name = "MULTIADDR", hide = true)]
    bootstrap: Vec<Multiaddr>,
    /// P2P 쪽 직접 스웜의 수신 주소(여러 번). 주면 STUN·UPnP로 바깥에 묻지 않습니다(시험용, 도움말에 숨김).
    #[arg(long = "p2p-listen", value_name = "MULTIADDR", hide = true)]
    p2p_listen: Vec<Multiaddr>,
    /// 다른 노드의 릴레이 역할도 합니다(공인 주소가 있는 서버용, `--direct`를 켬).
    #[arg(long)]
    relay_server: bool,
    /// Tor를 거치지 않고 직접 연결로 시작합니다(공인 주소로 받는 상시 시더·중계 서버, 시험용). 연결되는 상대에게 IP가
    /// 보입니다. 끄면(기본) Tor로 돌고, Tor를 준비할 수 없으면 시작하지 않습니다.
    // 환경 변수는 1/0·true/false·on/off를 모두 받습니다(기본 bool 파서는 true/false만 받아 `=1`로 시작하지 못함).
    #[arg(long, env = "YANGPA_DIRECT", value_parser = BoolishValueParser::new())]
    direct: bool,
    /// 예전 옵션: `normal`이면 `--direct`와 같습니다.
    #[arg(long, env = "YANGPA_DEFAULT_MODE", value_enum, hide = true)]
    default_mode: Option<ModeArg>,
    /// 예전 옵션: 이제 기본이 Tor라 `YANGPA_DEFAULT_MODE=normal`을 덮어 Tor로 띄울 때만 뜻이 있습니다.
    #[arg(long, hide = true)]
    tor: bool,
    /// 같은 네트워크 노드 찾기(mDNS)를 끕니다.
    #[arg(long)]
    no_mdns: bool,
    /// DB 키를 둘 곳.
    #[arg(long, value_enum, default_value = "auto")]
    key_storage: KeyStorageArg,
    /// 주기적 따라잡기 간격(예: 5m).
    #[arg(long, value_parser = humantime::parse_duration)]
    sync_interval: Option<Duration>,
    /// 이만큼 열지 않은 크루를 휴면(gossip 구독·주기 따라잡기 중단)으로 둡니다(예: 14d). 0이면 휴면을 쓰지 않습니다
    /// (NAS·VPS의 상시 시더).
    #[arg(long, env = "YANGPA_DORMANT_AFTER", value_parser = humantime::parse_duration)]
    dormant_after: Option<Duration>,
    /// 남에게서 받은 첨부에 쓸 디스크 한도의 기본값(예: 50GB, 기본 2GB). `blob.cache_set_limit`로 정한 값이 우선합니다.
    #[arg(long, env = "YANGPA_CACHE_LIMIT", value_parser = parse_bytes, value_name = "크기")]
    cache_limit: Option<u64>,
    /// 데이터 경로 디스크에 남겨 둘 최소 여유 공간의 기본값(예: 5GB, 기본 1GB).
    #[arg(long, env = "YANGPA_CACHE_MIN_FREE", value_parser = parse_bytes, value_name = "크기")]
    cache_min_free: Option<u64>,
    /// 보관 전용 노드로 띄웁니다: 관리자가 지정한 크루의 암호문(키 없이)을 받아 두고 멤버에게 내줍니다.
    #[arg(long, env = "YANGPA_ARCHIVE", value_parser = BoolishValueParser::new())]
    archive: bool,
    /// 보관 전용 노드가 공간마다 받아 둘 용량 상한(예: 50GB, 모든 이벤트와 첨부, 기본 20GiB). 단위가 없으면 바이트.
    #[arg(long, env = "YANGPA_ARCHIVE_QUOTA", value_parser = parse_bytes, value_name = "크기")]
    archive_quota: Option<u64>,
    /// 이 기기 이름(기기 목록 표시용).
    #[arg(long)]
    device_name: Option<String>,
    /// 휘발성 채널 첨부 하나의 최대 크기(MiB, 기본은 RAM 예산까지). 첨부는 RAM에만 둡니다.
    #[arg(long, env = "YANGPA_VOLATILE_ATTACHMENT_MIB", value_name = "MIB")]
    volatile_attachment_mib: Option<u64>,
    /// 휘발성 채널 첨부에 쓰는 RAM 전체 상한(MiB, 기본 1024). 넘으면 새 첨부를 보내거나 받지 않고 오류로 알립니다.
    #[arg(long, env = "YANGPA_VOLATILE_RAM_MIB", value_name = "MIB")]
    volatile_ram_mib: Option<u64>,
    /// 통화 SFU 호스트를 맡을지(Tor 모드에서는 맡지 않음).
    #[arg(long, env = "YANGPA_SFU_HOST", value_enum, default_value = "auto")]
    sfu_host: SfuHostArg,
    /// SFU 미디어 UDP 포트(0이면 아무 포트).
    #[arg(long, env = "YANGPA_SFU_PORT", default_value_t = 0)]
    sfu_port: u16,
    /// SFU가 바깥에 알릴 IP(공인 IP·포트 포워딩, 쉼표로 여러 개).
    #[arg(
        long,
        env = "YANGPA_SFU_ANNOUNCE",
        value_delimiter = ',',
        value_name = "IP"
    )]
    sfu_announce: Vec<std::net::IpAddr>,
    /// SFU 미디어 소켓을 열 IP(쉼표로 여러 개, 비우면 노드 수신 주소의 IP).
    #[arg(
        long,
        env = "YANGPA_SFU_BIND",
        value_delimiter = ',',
        value_name = "IP"
    )]
    sfu_bind: Vec<std::net::IpAddr>,
    /// SFU가 동시에 받는 참가자 수.
    #[arg(long, env = "YANGPA_SFU_CAPACITY")]
    sfu_capacity: Option<u16>,
    /// 로그 수준(tracing 필터 문법).
    #[arg(long, env = "YANGPA_LOG", default_value = "info")]
    log: String,
    /// 로그를 표준 오류 대신 이 파일에 덧붙여 씁니다(`diag.leakcheck`도 이 파일을 훑음).
    #[arg(long, env = "YANGPA_LOG_FILE")]
    log_file: Option<PathBuf>,
    /// 이 프로세스가 끝나면 데몬도 끝납니다. 데스크톱 앱이 데몬을 띄울 때 자기 PID를 넘겨, 앱이 강제로 꺼져도 데몬이
    /// 남지 않게 합니다.
    #[arg(
        long,
        env = "YANGPA_PARENT_PID",
        value_name = "PID",
        value_parser = clap::value_parser!(u32).range(1..)
    )]
    parent_pid: Option<u32>,
}

fn main() -> ExitCode {
    // libp2p TLS(rustls KeyLogFile)는 이 변수가 있으면 연결 세션 키를 그 파일에 씁니다. 키가 디스크에 남지 않게 지웁니다.
    // SAFETY: 런타임을 만들기 전이라 환경을 읽거나 쓰는 다른 스레드가 없습니다.
    unsafe { std::env::remove_var("SSLKEYLOGFILE") };
    no_power_throttling();
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("yangpad: 비동기 런타임을 만들 수 없습니다: {e}");
            return ExitCode::FAILURE;
        }
    };
    let code = rt.block_on(daemon_main());
    rt.shutdown_timeout(RUNTIME_SHUTDOWN);
    code
}

/// 윈도가 창 없는 이 데몬을 전원 조절(EcoQoS, 효율 모드)로 느리게 돌리지 않게 빠집니다. 앱 창을 내리거나 가려 두면
/// Tor·전송이 느려지는 것처럼 보이는 것을 막습니다. 실패해도(옛 윈도) 그대로 돕니다.
#[cfg(windows)]
fn no_power_throttling() {
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        PROCESS_POWER_THROTTLING_EXECUTION_SPEED, PROCESS_POWER_THROTTLING_STATE,
        ProcessPowerThrottling, SetProcessInformation,
    };
    let state = PROCESS_POWER_THROTTLING_STATE {
        Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
        // 조절하는 항목(ControlMask)을 끔(StateMask 0) = 늘 제 속도.
        StateMask: 0,
    };
    // SAFETY: 현재 프로세스 의사 핸들과 크기를 맞춘 구조체 포인터만 넘깁니다.
    unsafe {
        SetProcessInformation(
            GetCurrentProcess(),
            ProcessPowerThrottling,
            (&raw const state).cast(),
            std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        );
    }
}

#[cfg(not(windows))]
fn no_power_throttling() {}

async fn daemon_main() -> ExitCode {
    let args = Args::parse();
    let filter = EnvFilter::try_new(&args.log).unwrap_or_else(|_| EnvFilter::new("info"));
    match &args.log_file {
        Some(path) => match open_log(path) {
            Ok(f) => tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_ansi(false)
                .with_writer(std::sync::Mutex::new(f))
                .init(),
            Err(e) => {
                eprintln!(
                    "yangpad: 로그 파일을 열 수 없습니다({}): {e}",
                    path.display()
                );
                return ExitCode::FAILURE;
            }
        },
        None => tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::io::stderr)
            .init(),
    }
    match run(args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("yangpad: {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(args: Args) -> anyhow::Result<()> {
    // 잠금을 기다리기 전에 잡아 둡니다(`parent::watch`).
    let parent = args.parent_pid.and_then(parent::watch);
    let data_dir = match args.data_dir {
        Some(d) => d,
        None => yangpa_rpc::endpoint::data_dir().context("데이터 경로를 정할 수 없습니다")?,
    };
    let mut cfg = NodeConfig::new(&data_dir);
    cfg.key_storage = args.key_storage.into();
    cfg.net.listen = args.listen;
    cfg.net.bootstrap = args.bootstrap;
    cfg.net.p2p_listen = args.p2p_listen;
    cfg.net.relay_server = args.relay_server;
    // 릴레이 서버는 공인 IP로 남의 연결을 받아야 하므로 직접 연결로 띄웁니다.
    let direct = args.direct || args.relay_server;
    if args.tor && direct {
        anyhow::bail!("--tor는 --direct·--relay-server와 함께 쓸 수 없습니다");
    }
    cfg.net.direct = direct || (!args.tor && args.default_mode == Some(ModeArg::Normal));
    cfg.net.mdns = !args.no_mdns;
    if let Some(i) = args.sync_interval {
        cfg.sync_interval = i;
    }
    if let Some(d) = args.dormant_after {
        cfg.sharing.dormant_after = d;
    }
    if let Some(n) = args.device_name {
        cfg.device_name = n;
    }
    if let Some(b) = args.cache_limit {
        cfg.sharing.cache_limit_bytes = b;
    }
    if let Some(b) = args.cache_min_free {
        cfg.sharing.cache_min_free_bytes = b;
    }
    if let Some(n) = args.volatile_attachment_mib {
        cfg.volatile.max_attachment_bytes = n.saturating_mul(1024 * 1024);
    }
    if let Some(n) = args.volatile_ram_mib {
        cfg.volatile.max_attachment_ram_bytes = n.saturating_mul(1024 * 1024);
    }
    cfg.archive.enabled = args.archive;
    if let Some(q) = args.archive_quota {
        cfg.archive.quota_bytes = q;
    }
    cfg.sfu.host = args.sfu_host.into();
    cfg.sfu.port = args.sfu_port;
    cfg.sfu.announce = args.sfu_announce;
    cfg.sfu.bind = args.sfu_bind;
    if let Some(c) = args.sfu_capacity {
        cfg.sfu.capacity = c.max(1);
    }
    cfg.log_file = args.log_file;

    let host = Host::start(cfg, args.endpoint.as_deref()).await?;

    let mut stop = host.node.shutdown_signal();
    let parent_gone = async {
        match parent {
            Some(w) => w.await,
            None => std::future::pending().await,
        }
    };
    tokio::select! {
        r = tokio::signal::ctrl_c() => match r {
            Ok(()) => info!("Ctrl-C: 종료합니다"),
            Err(e) => warn!(error = %e, "Ctrl-C 신호를 받을 수 없음"),
        },
        _ = terminate() => info!("SIGTERM: 종료합니다"),
        _ = stop.wait_for(|v| *v) => info!("system.shutdown: 종료합니다"),
        () = parent_gone => info!("데몬을 띄운 앱이 끝나 종료합니다"),
    }
    if host.stop().await {
        relaunch();
    }
    Ok(())
}

/// 오프라인 모드로 들어가거나 나올 때: 같은 인자·환경(앱 PID 포함)으로 새 데몬을 띄웁니다. 새 데몬은 이 프로세스가
/// 끝나 데이터 경로 잠금을 놓을 때까지 기다리고(`lock_data_dir`), 앱은 끊긴 연결을 다시 붙습니다.
fn relaunch() {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            warn!(error = %e, "데몬을 다시 띄우지 못함(실행 파일 경로)");
            return;
        }
    };
    let mut cmd = std::process::Command::new(exe);
    cmd.args(std::env::args_os().skip(1))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // 창 없는 콘솔(앱의 daemon.rs와 같은 까닭: 브리지의 lyrebird가 창 없이 물려받게).
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    match cmd.spawn() {
        Ok(_) => info!("표시 상태가 바뀌어 데몬을 다시 띄웁니다"),
        Err(e) => warn!(error = %e, "데몬을 다시 띄우지 못함"),
    }
}

/// 바이트 크기(`500MB`, `2GB`, `1.5TB`, 숫자만이면 바이트). 단위는 1024 배수로, CLI(`yangpa blob cache limit`)와 같습니다.
fn parse_bytes(s: &str) -> Result<u64, String> {
    let t = s.trim();
    let split = t
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(t.len());
    let (num, unit) = t.split_at(split);
    let num: f64 = num
        .parse()
        .map_err(|_| format!("크기를 읽을 수 없습니다: {s}"))?;
    let factor: f64 = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1.0,
        "k" | "kb" | "kib" => 1024.0,
        "m" | "mb" | "mib" => 1024.0 * 1024.0,
        "g" | "gb" | "gib" => 1024.0 * 1024.0 * 1024.0,
        "t" | "tb" | "tib" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        u => return Err(format!("알 수 없는 크기 단위: {u} (B, KB, MB, GB, TB)")),
    };
    let bytes = (num * factor).round();
    if !bytes.is_finite() || bytes < 0.0 || bytes > u64::MAX as f64 {
        return Err(format!("잘못된 크기: {s}"));
    }
    Ok(bytes as u64)
}

/// 로그 파일을 엽니다(덧붙이기, 유닉스에서는 주인만 읽기·쓰기).
fn open_log(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    let mut o = std::fs::OpenOptions::new();
    o.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    o.open(path)
}

#[cfg(unix)]
async fn terminate() {
    use tokio::signal::unix::{SignalKind, signal};
    match signal(SignalKind::terminate()) {
        Ok(mut s) => {
            s.recv().await;
        }
        Err(_) => std::future::pending::<()>().await,
    }
}

#[cfg(not(unix))]
async fn terminate() {
    std::future::pending::<()>().await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 데몬을 띄운 프로세스가 끝나면(강제 종료 포함) 데몬도 정리하고 끝납니다.
    #[tokio::test(flavor = "multi_thread")]
    async fn exits_when_parent_process_ends() {
        use std::ffi::OsString;
        use yangpa_rpc::endpoint::{OWNER_TOKEN_FILE, read_token_file};
        use yangpa_rpc::{Endpoint, RpcClient};

        let dir = tempfile::tempdir().unwrap();
        let ep = Endpoint::in_dir(dir.path());
        let mut app = parent::stand_in();
        let mut argv: Vec<OsString> = ["yangpad", "--data-dir"].map(OsString::from).into();
        argv.push(dir.path().into());
        argv.push("--endpoint".into());
        argv.push(ep.path().into());
        argv.extend(
            [
                "--key-storage",
                "file",
                "--no-mdns",
                "--direct",
                "--listen",
                "/ip4/127.0.0.1/tcp/0",
                "--parent-pid",
            ]
            .map(OsString::from),
        );
        argv.push(app.id().to_string().into());
        let daemon = tokio::spawn(run(Args::try_parse_from(argv).unwrap()));

        let token_path = dir.path().join(OWNER_TOKEN_FILE);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        loop {
            if let Ok(token) = read_token_file(&token_path)
                && RpcClient::connect_with_token(&ep, token.expose())
                    .await
                    .is_ok()
            {
                break;
            }
            if daemon.is_finished() {
                panic!("데몬이 일찍 끝남: {:?}", daemon.await);
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "데몬 API 대기 시간 초과"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        app.kill().unwrap();
        app.wait().unwrap();
        tokio::time::timeout(Duration::from_secs(30), daemon)
            .await
            .expect("앱이 끝나면 데몬도 끝남")
            .unwrap()
            .unwrap();
    }

    #[test]
    fn cache_flags() {
        assert_eq!(parse_bytes("2GB"), Ok(2 * 1024 * 1024 * 1024));
        assert_eq!(parse_bytes("1.5 kib"), Ok(1536));
        assert_eq!(parse_bytes("4096"), Ok(4096));
        assert!(parse_bytes("").is_err());
        assert!(parse_bytes("-1GB").is_err());
        assert!(parse_bytes("3 parsecs").is_err());
        let a = Args::try_parse_from(["yangpad", "--cache-limit", "50GB", "--cache-min-free", "0"])
            .unwrap();
        assert_eq!(a.cache_limit, Some(50 * 1024 * 1024 * 1024));
        assert_eq!(a.cache_min_free, Some(0));
        // 보관 용량도 같은 크기 표기를 받고, 예전처럼 숫자만 주면 바이트입니다.
        let a = Args::try_parse_from(["yangpad", "--archive", "--archive-quota", "50GB"]).unwrap();
        assert!(a.archive);
        assert_eq!(a.archive_quota, Some(50 * 1024 * 1024 * 1024));
        let a = Args::try_parse_from(["yangpad", "--archive-quota", "1048576"]).unwrap();
        assert_eq!(a.archive_quota, Some(1024 * 1024));
    }
}
