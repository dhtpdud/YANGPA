//! 데몬 설정.

use std::path::PathBuf;
use std::time::Duration;

use yangpa_net::{KadMode, Multiaddr};

/// lyrebird 실행 파일 경로를 정하는 환경 변수(Android: 실행 파일 옆이 없어 앱이 네이티브 라이브러리 폴더로 정합니다).
pub const ENV_LYREBIRD: &str = "YANGPA_LYREBIRD";

/// DB 키(SQLCipher)를 어디에 둘지.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum KeyStorage {
    /// OS 키 저장소를 먼저 쓰고, 쓸 수 없으면 데이터 경로의 0600 파일.
    #[default]
    Auto,
    /// OS 키 저장소만(쓸 수 없으면 시작 실패).
    OsKeyring,
    /// 데이터 경로의 0600 파일(`db.key`). 테스트·헤드리스 서버용.
    File,
}

/// 네트워크 설정.
#[derive(Clone, Debug)]
pub struct NetOptions {
    /// 수신 주소. 비우면 네트워크 계층 기본값(모든 인터페이스, QUIC + TCP).
    pub listen: Vec<Multiaddr>,
    /// 부트스트랩·릴레이 노드.
    pub bootstrap: Vec<Multiaddr>,
    pub mdns: bool,
    pub relay_server: bool,
    /// 확인 절차 없이 알릴 외부 주소(주소가 고정된 상시 노드·릴레이 시험용). 릴레이는 외부 주소가 있어야 예약을 받습니다.
    pub external_addrs: Vec<Multiaddr>,
    pub kad_mode: KadMode,
    /// Tor를 거치지 않는 직접 연결 스웜으로 띄웁니다(상시 시더·중계 서버·시험용, 연결되는 상대에게 IP가 보임). 끄면(기본)
    /// Tor로 돌고, 준비할 수 없으면 시작이 실패합니다(`tor::net_config` 담당).
    pub direct: bool,
    pub request_timeout: Duration,
    /// P2P 전용 채널용 P2P 스웜의 수신 주소. 비우면 네트워크 계층 기본값(모든 인터페이스, QUIC + TCP).
    pub p2p_listen: Vec<Multiaddr>,
}

impl Default for NetOptions {
    fn default() -> Self {
        NetOptions {
            listen: Vec::new(),
            bootstrap: Vec::new(),
            mdns: true,
            relay_server: false,
            external_addrs: Vec::new(),
            kad_mode: KadMode::Auto,
            direct: false,
            request_timeout: Duration::from_secs(30),
            p2p_listen: Vec::new(),
        }
    }
}

/// 링크 미리보기(보내는 쪽 조회) 한도. 받는 쪽은 외부 사이트에 접속하지 않습니다.
#[derive(Clone, Debug)]
pub struct LinkPreviewOptions {
    /// 꺼도 채널 설정과 무관하게 미리보기를 만들지 않습니다.
    pub enabled: bool,
    /// 사설·루프백·링크 로컬 주소 조회 허용(SSRF 주소 검사 끄기). 시험용이며 기본 꺼짐.
    pub allow_private_addrs: bool,
    /// 80·443·8080·8443 밖의 포트 조회 허용(주소 검사는 그대로). 시험용이며 기본 꺼짐.
    pub allow_any_port: bool,
    /// 주소 하나(넘겨주기 포함)를 조회하는 전체 시간.
    pub timeout: Duration,
    /// 읽을 HTML 최대 바이트(넘으면 거기까지만 봄).
    pub max_page_bytes: usize,
    /// 넘겨주기(3xx) 최대 횟수.
    pub max_redirects: u8,
    /// 메시지 하나에서 조회할 최대 링크 수.
    pub max_links: usize,
}

impl Default for LinkPreviewOptions {
    fn default() -> Self {
        LinkPreviewOptions {
            enabled: true,
            allow_private_addrs: false,
            allow_any_port: false,
            timeout: Duration::from_secs(5),
            max_page_bytes: 512 * 1024,
            max_redirects: 3,
            max_links: 3,
        }
    }
}

/// 첨부·블롭·휴면 설정(B3).
#[derive(Clone, Debug)]
pub struct SharingOptions {
    /// 이 기기가 보내는 첨부 하나의 최대 크기(평문 바이트). 기본 무제한이고, 채널에 한도가 있으면 더 작은 쪽을
    /// 씁니다(크루 관리자가 채널마다 정함, decisions.md).
    pub max_attachment_bytes: u64,
    /// 새 블롭의 조각 크기(암호문 기준, 16KiB~4MiB로 맞춤).
    pub chunk_size: u32,
    /// 동시에 내려받는 블롭 수.
    pub max_active_downloads: usize,
    /// 남에게서 받은 첨부(고정·내 첨부·이모지·프로필 이미지 빼고)에 쓸 디스크 한도. 넘으면 오래 안 본 것부터
    /// 데이터를 비웁니다(`blob.cache_set_limit`로 바꾼 값이 우선, decisions.md).
    pub cache_limit_bytes: u64,
    /// 데이터 경로 디스크에 남겨 둘 여유 공간. 이보다 적으면 한도 아래여도 비웁니다.
    pub cache_min_free_bytes: u64,
    /// 이만큼 활동(열기·보내기·받은 전파)이 없는 공간은 휴면으로 둡니다.
    pub dormant_after: Duration,
    pub link_preview: LinkPreviewOptions,
}

impl Default for SharingOptions {
    fn default() -> Self {
        SharingOptions {
            max_attachment_bytes: u64::MAX,
            chunk_size: yangpa_core::blob::DEFAULT_CHUNK_SIZE,
            max_active_downloads: 3,
            cache_limit_bytes: 2 * 1024 * 1024 * 1024,
            cache_min_free_bytes: 1024 * 1024 * 1024,
            dormant_after: Duration::from_secs(14 * 24 * 60 * 60),
            link_preview: LinkPreviewOptions::default(),
        }
    }
}

/// 휘발성 채널 첨부(RAM 전용) 한도. 첨부 암호문은 디스크에 가지 않고 RAM에만 있으므로 RAM 사용을 묶습니다.
#[derive(Clone, Debug)]
pub struct VolatileOptions {
    /// 휘발성 첨부 하나의 최대 크기(평문 바이트). 기본 무제한이지만 채널 한도와 RAM 예산(`max_attachment_ram_bytes`)을
    /// 넘지 못합니다.
    pub max_attachment_bytes: u64,
    /// 이 기기가 휘발성 첨부 암호문에 쓰는 RAM 전체 상한(바이트). 보내는 것·받는 것을 합쳐 셉니다. 받을 첨부는
    /// 받기 시작할 때 크기만큼 미리 잡으므로, 넘으면 받지 않고 오류로 알립니다. 기본 1GiB.
    pub max_attachment_ram_bytes: u64,
}

impl Default for VolatileOptions {
    fn default() -> Self {
        VolatileOptions {
            max_attachment_bytes: u64::MAX,
            max_attachment_ram_bytes: 1024 * 1024 * 1024,
        }
    }
}

/// 보관 전용 노드(protocol.md "보관 전용 노드"). 켜면 관리자의 지정을 받아 크루의 암호문을 받아 두고 멤버에게 내줍니다.
#[derive(Clone, Debug)]
pub struct ArchiveOptions {
    /// 지정 제안(`archive.offer`)·지정 수락을 합니다. 꺼도 이미 받아 둔 공간은 그대로 제공합니다.
    pub enabled: bool,
    /// 공간마다 받아 둘 용량 상한(바이트, 모든 이벤트의 헤더·본문과 첨부). 메시지 본문·첨부는 7/8까지만 받고 넘으면
    /// 메시지는 헤더만, 첨부는 받지 않습니다. 나머지는 상태·키 이벤트 몫입니다.
    pub quota_bytes: u64,
}

impl Default for ArchiveOptions {
    fn default() -> Self {
        ArchiveOptions {
            enabled: false,
            quota_bytes: 20 * 1024 * 1024 * 1024,
        }
    }
}

/// 상태 체크포인트(protocol.md "체크포인트"). 소유자 기기(또는 위임받은 기기)가 조건이 되면 냅니다.
#[derive(Clone, Debug)]
pub struct CheckpointOptions {
    /// 끄면 내지 않습니다(받고 따르는 것은 그대로).
    pub enabled: bool,
    /// 첫 체크포인트: 활성 기기가 이 수를 넘거나
    pub start_devices: usize,
    /// 처리한 공간 스트림 이벤트가 이 수를 넘을 때.
    pub start_events: usize,
    /// 그 뒤: 바탕 뒤 이벤트가 이만큼 쌓이거나
    pub every_events: usize,
    /// 바탕이 이만큼 오래되면(바탕 뒤 이벤트가 하나라도 있을 때).
    pub every: Duration,
    /// 머리가 하나로 이만큼 그대로일 때만 냅니다(늦은 이벤트가 덜 생기게).
    pub settle: Duration,
    /// 낼 때가 됐는데 이만큼 머리가 가라앉지 않으면 지금 머리 전부를 부모로 둔 체크포인트로 합칩니다.
    pub merge_after: Duration,
    /// 조건을 보는 간격.
    pub check_every: Duration,
    /// 체크포인트로 들어온 뒤 받아 준 노드가 아닌 출처가 같은 머리를 확인해 주기를 기다리는 시간. 그동안 그 공간에
    /// 쓰지 않고, 넘으면 체크포인트 상태를 버리고 처음부터 따라잡습니다.
    pub second_source_wait: Duration,
}

impl Default for CheckpointOptions {
    fn default() -> Self {
        CheckpointOptions {
            enabled: true,
            start_devices: 256,
            start_events: 5_000,
            every_events: 1_000,
            every: Duration::from_secs(24 * 60 * 60),
            settle: Duration::from_secs(5 * 60),
            merge_after: Duration::from_secs(48 * 60 * 60),
            check_every: Duration::from_secs(60),
            second_source_wait: Duration::from_secs(60),
        }
    }
}

/// 이 노드가 통화 SFU 호스트를 맡을지(protocol.md "음성·화면공유", decisions.md).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SfuHostMode {
    /// 맡지 않음.
    Never,
    /// 참가 중인 통화에서 연결이 가장 좋으면 맡음(데스크톱 기본).
    #[default]
    Auto,
    /// 먼저 맡겠다고 알림. 참가하지 않은 통화도 볼 수 있으면 호스트 후보가 됩니다(헤드리스 시더 기본).
    Prefer,
}

/// 통화 SFU 설정. Tor로 도는 노드는 설정과 상관없이 맡지 않습니다(실제 IP가 드러나므로). 그래서 실제로 맡는 것은
/// 직접 연결 주 스웜(`--direct`)의 상시 노드뿐입니다.
#[derive(Clone, Debug)]
pub struct SfuOptions {
    pub host: SfuHostMode,
    /// 미디어 UDP 포트(0이면 아무 포트). 방화벽·포트 포워딩을 쓰는 서버는 고정합니다.
    pub port: u16,
    /// 바깥에 알릴 주소(공인 IP, NAT 포트 포워딩). 비우면 STUN 서버 목록(`net.stun_servers`)으로 알아낸 주소만 씁니다.
    pub announce: Vec<std::net::IpAddr>,
    /// 미디어 소켓을 열 주소. 비우면 노드 수신 주소의 IP(다른 IP가 있으면 루프백은 뺌).
    pub bind: Vec<std::net::IpAddr>,
    /// 동시에 받는 참가자 수.
    pub capacity: u16,
    /// 이 수를 넘으면 메시에서 SFU로 바꿈(기획서 MESH_MAX).
    pub mesh_max: usize,
    /// SFU에서 이 수 이하로 줄면 메시로 돌아감(오르내림 반복 방지).
    pub mesh_min: usize,
}

impl Default for SfuOptions {
    fn default() -> Self {
        SfuOptions {
            host: SfuHostMode::Auto,
            port: 0,
            announce: Vec::new(),
            bind: Vec::new(),
            capacity: 25,
            mesh_max: 6,
            mesh_min: 4,
        }
    }
}

/// [`crate::Node::start`]에 넘기는 설정.
#[derive(Clone, Debug)]
pub struct NodeConfig {
    /// 데이터 경로(DB, 블롭, 소유자 토큰).
    pub data_dir: PathBuf,
    /// 로그 파일(데몬이 로그를 여기에 씀). `diag.leakcheck`가 데이터 경로와 함께 훑습니다.
    pub log_file: Option<PathBuf>,
    pub key_storage: KeyStorage,
    pub net: NetOptions,
    /// 휴면이 아닌 공간의 주기적 따라잡기 간격.
    pub sync_interval: Duration,
    /// 끊긴 친구·내 기기에 다시 걸어 보는 기본 간격(기기마다 두 배씩 늘림, Tor 스웜이면 두 배에서 시작).
    pub redial_interval: Duration,
    /// 소유자 토큰 파일(`<데이터 경로>/owner.token`)을 처음 실행 때 만들지.
    pub owner_token_file: bool,
    /// SQLCipher 메모리 보호(프로세스 전역). 테스트에서 끌 수 있습니다.
    pub store_memory_security: bool,
    /// 이 기기 이름(기기 목록에 표시).
    pub device_name: String,
    /// 공간의 활성 기기가 이 수 이하면 직접 보내고(`PushEvents`), 넘으면 gossip(protocol.md: 8).
    /// 시험에서 gossip 경로를 볼 때 낮춥니다.
    pub push_max_devices: usize,
    /// 비공개 키 그룹 채널을 볼 수 있는 기기(이 기기 포함)가 이 수를 넘으면 그 그룹의 gossip 토픽으로 보내고, 이하면 볼
    /// 수 있는 기기 모두에게 직접 보냅니다(대형 크루 설계: 32). 시험에서 그룹 토픽 경로를 볼 때 낮춥니다.
    pub group_topic_min: usize,
    /// 활성 기기가 이 수를 넘는 공간에서만, 곧바로 받은(gossip·`PushEvents`·그 빈 부모 채우기) 상태·키 합의 이벤트를 작성
    /// 기기별로 묶습니다(버스트 30, 분당 10). 작은 크루(256대 이하)는 예전 그대로입니다. 시험에서 제한을 볼 때 0으로
    /// 낮춥니다.
    pub event_limit_devices: usize,
    /// 감사 로그 보관 행 수(평범한 줄, 중요한 줄). 따로 세므로 평범한 호출을 잔뜩 해도 중요한 줄은 밀려나지 않습니다.
    pub audit_caps: (u32, u32),
    /// 첨부·블롭·링크 미리보기·휴면(B3).
    pub sharing: SharingOptions,
    /// 휘발성 채널 첨부의 RAM 한도.
    pub volatile: VolatileOptions,
    /// 확정 기기가 키 그룹을 맞추기 전에 기다리는 시간. 그 사이 온 가입·기기 추가·제거를 `KeyUpdate` 하나로 묶습니다.
    pub commit_batch_window: Duration,
    /// 제거가 계속 들어오면 묶음 창을 이만큼까지 늘립니다(대량 추방을 갱신 하나로). 소유자·관리자의 즉시 요청은 바로.
    pub commit_batch_max: Duration,
    /// 보내는 쪽 규칙: 제거를 반영한 키 갱신을 이만큼 기다린 뒤에는 옛 키로 보내고 `key_rotation_pending`을 켭니다
    /// (`SpaceSettings::strict_rotation`이면 끝없이 기다림).
    pub rotation_wait: Duration,
    /// 새 확정 기기가 옛 확정 기기의 키 그룹 표를 기다리는 시간. 지나면 공개 끝으로 이어받아 전체 재키합니다.
    pub key_handover_wait: Duration,
    /// 보관 전용 노드(X3).
    pub archive: ArchiveOptions,
    /// 통화 SFU 호스트.
    pub sfu: SfuOptions,
    /// 사용자가 STUN 서버 목록(`net.stun_servers`)을 한 번도 저장하지 않았을 때 쓰는 목록(`host:port`). 빈 목록을
    /// 저장하면 이것도 쓰지 않습니다.
    pub default_stun_servers: Vec<String>,
    /// 상태 체크포인트.
    pub checkpoint: CheckpointOptions,
    /// 원년멤버 증서를 서명하는 개발자 계정(기본 [`yangpa_core::badge::DEVELOPER`], 시험에서 바꿈).
    pub developer: Option<yangpa_core::ids::UserId>,
}

/// 기본 STUN 서버: 가입 없이 쓰는 공개 STUN. 직접 연결 경로(P2P 스웜·SFU 호스트)가 공유기 밖 주소를 알아낼 때만
/// 묻고 Tor 연결에는 쓰지 않습니다. STUN 서버는 묻는 기기의 공인 IP를 봅니다.
pub const DEFAULT_STUN_SERVERS: [&str; 3] = [
    "stun.l.google.com:19302",
    "stun1.l.google.com:19302",
    "stun.cloudflare.com:3478",
];

impl NodeConfig {
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        NodeConfig {
            data_dir: data_dir.into(),
            log_file: None,
            key_storage: KeyStorage::Auto,
            net: NetOptions::default(),
            sync_interval: Duration::from_secs(5 * 60),
            redial_interval: Duration::from_secs(60),
            owner_token_file: true,
            store_memory_security: true,
            device_name: default_device_name(),
            push_max_devices: 8,
            group_topic_min: 32,
            event_limit_devices: 256,
            audit_caps: (10_000, 50_000),
            sharing: SharingOptions::default(),
            volatile: VolatileOptions::default(),
            commit_batch_window: Duration::from_millis(300),
            commit_batch_max: Duration::from_secs(2),
            rotation_wait: Duration::from_secs(30),
            key_handover_wait: Duration::from_secs(20),
            archive: ArchiveOptions::default(),
            sfu: SfuOptions::default(),
            default_stun_servers: DEFAULT_STUN_SERVERS.map(String::from).to_vec(),
            checkpoint: CheckpointOptions::default(),
            developer: yangpa_core::badge::DEVELOPER,
        }
    }

    /// 한 프로세스에 여러 노드를 띄우는 테스트용 설정: 127.0.0.1 직접 연결, mDNS·Tor·릴레이 끔, DB 키는 파일.
    pub fn for_test(data_dir: impl Into<PathBuf>) -> Self {
        let mut cfg = NodeConfig::new(data_dir);
        cfg.key_storage = KeyStorage::File;
        cfg.net.listen = vec!["/ip4/127.0.0.1/tcp/0".parse().expect("고정 주소")];
        cfg.net.mdns = false;
        cfg.net.kad_mode = KadMode::Server;
        cfg.net.direct = true;
        cfg.net.request_timeout = Duration::from_secs(20);
        cfg.net.p2p_listen = vec!["/ip4/127.0.0.1/tcp/0".parse().expect("고정 주소")];
        cfg.store_memory_security = false;
        cfg.device_name = "테스트 기기".into();
        cfg.commit_batch_window = Duration::from_millis(50);
        cfg.commit_batch_max = Duration::from_millis(400);
        cfg.rotation_wait = Duration::from_secs(2);
        cfg.key_handover_wait = Duration::from_secs(2);
        // 시험 기계의 디스크 여유와 무관하게 정리가 한도로만 움직이게 합니다.
        cfg.sharing.cache_min_free_bytes = 0;
        cfg.sfu.bind = vec![std::net::Ipv4Addr::LOCALHOST.into()];
        // 시험이 바깥 STUN에 묻지 않게 합니다.
        cfg.default_stun_servers = Vec::new();
        // 문턱은 그대로(보통 시험에서는 내지 않음), 체크포인트 시험이 문턱만 낮추면 바로 돌게 기다림은 짧게.
        cfg.checkpoint.settle = Duration::from_millis(200);
        cfg.checkpoint.check_every = Duration::from_millis(200);
        // 두 번째 출처를 기다리다 넘으면 처음부터 따라잡으므로, 시험이 다른 멤버를 잇는 동안은 넉넉히 기다립니다.
        cfg.checkpoint.second_source_wait = Duration::from_secs(30);
        cfg
    }
}

/// 기기 이름 기본값은 OS 이름입니다(설정 › 기기에서 바꿀 수 있음).
pub(crate) fn default_device_name() -> String {
    match std::env::consts::OS {
        "windows" => "Windows",
        "macos" => "macOS",
        "android" => "Android",
        "ios" => "iOS",
        "linux" => "Linux",
        other => other,
    }
    .into()
}
