//! 큰 크루 상태의 저장·색인 비용(`yangpa-bench scale --store 30000:45000`).
//!
//! 간추린 상태(기기 기록에 키 대신 키 약속), 바뀐 행만 쓰기([`StateDelta`]), 상태 색인, 기기 키 표를 잽니다. 상태는
//! 해소기를 거치지 않고 바로 짓습니다: 3만 명 가입을 해소하면 서명 확인만 10초 남짓이고, 여기서 재는 것은 저장·색인이라 서명이
//! 필요 없습니다. 기기 ID는 실제처럼 서명 공개키에서, 키 약속은 상자 키·서명 키에서 셉니다.
//!
//! 처음 읽기(콜드 로드)와 그때의 상주 메모리는 새 프로세스(`yangpa-bench scale-load`)에서 잽니다. 같은 프로세스에서
//! 재면 상태를 지으며 쓴 메모리가 RSS에 남기 때문입니다.
//!
//! 바꾸기마다 그 바꾸기를 실은 이벤트도 씁니다: 받기 쪽은 상태 이벤트와 본문에 실린 기기 키를 한 트랜잭션으로 넣고
//! (`Store::insert_events_with_device_keys`), 해소한 뒤 바뀐 상태 행을 따로 씁니다. 이벤트 본문은 무작위 바이트로
//! 크기만 맞춥니다(가입은 잰 가입 기록 크기, 기기 목록 갱신은 목록 + 서명 둘).
//!
//! 데몬은 시작할 때 저장된 공간 스트림 이벤트를 모두 다시 확인·해소합니다(`spaces::load_all`). 그 가운데 가입마다
//! 드는 서명 확인(이벤트 하나 + 기기 목록 하나)을 따로 재어 3만 명이면 얼마인지 어림합니다([`ReplayReport`]).

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use yangpa_core::body::Secret32;
use yangpa_core::device::{
    BoxPublicKey, DeviceEntry, DeviceKeys, DeviceList, DeviceSignKeys, MemberDevices,
    SignedDeviceList,
};
use yangpa_core::event::{EVENT_VERSION, EventHeader, EventKind, KeyRef, SignedEvent};
use yangpa_core::ids::{ChannelId, DeviceId, RoleId, SpaceId, StateKeyId, UserId};
use yangpa_core::model::{
    BanInfo, ChannelSettings, Member, OverwriteTarget, PermissionOverwrite, Permissions, Role,
    SecurityLevel, SharedMap, SpaceKind, SpaceSettings, SpaceState,
};
use yangpa_core::sign::{Sig, SigningKeypair};
use yangpa_core::time::Timestamp;
use yangpa_daemon::testkit::IndexProbe;
use yangpa_store::{PersistentEvent, StateDelta, Store};

use super::state::{peak_rss_mb, rss_mb};

const T0: u64 = 1_800_000_000_000;

/// 채널 수(하나는 일부만 보는 채널).
const CHANNELS: usize = 100;

/// 역할 수(@everyone 제외).
const ROLES: usize = 20;

/// 바꾸기 종류마다 잴 횟수.
const SAMPLES: usize = 15;

/// 기기 키 표 LRU 크기(데몬 `devkeys::LRU_CAP`과 같게).
const LRU_ENTRIES: usize = 4096;

/// 가입 이벤트 본문 크기(기기 항목에 서명 공개키를 실은 뒤 잰 가입 기록 13,148B, performance.md).
const JOIN_BODY: usize = 13_148;

/// 기기 목록이 없는 상태 이벤트 본문 크기(역할·밴·채널 권한·설정, 넉넉히).
const SMALL_BODY: usize = 1_024;

/// 다시 세우기 어림에서 서명 확인을 잴 횟수.
const VERIFY_SAMPLES: u32 = 64;

/// 바꾸기 한 종류의 측정.
#[derive(Debug, Clone, Serialize)]
pub struct ChangeReport {
    pub kind: &'static str,
    pub samples: usize,
    /// 행 변화 계산 + 쓰기(기기 키 행 포함)의 스레드 CPU 시간(중앙값·최대, 마이크로초).
    pub cpu_median_us: f64,
    pub cpu_max_us: f64,
    /// 같은 일의 벽시계 시간(최대).
    pub wall_max_us: f64,
    /// 쓴 행 수와 값 바이트(최대).
    pub rows: usize,
    pub value_bytes: usize,
    /// WAL에 쓴 바이트(체크포인트로 비운 뒤 한 번 적용한 WAL 크기). 페이지(4KiB)와 프레임 머리 포함. 받을 때 쓰는
    /// 이벤트 + 새 기기 키 행(한 트랜잭션, 중앙값·최대)과 해소한 뒤 쓰는 상태 행(중앙값·최대), 둘을 더한 것(최대)을
    /// 나눠 봅니다.
    pub wal_event_median: u64,
    pub wal_event_max: u64,
    pub wal_state_median: u64,
    pub wal_state_max: u64,
    pub wal_total_max: u64,
    /// 흉내 낸 이벤트 본문 크기.
    pub event_body: usize,
}

/// 시작 때 다시 세우기에서 가입마다 드는 서명 확인.
#[derive(Debug, Clone, Serialize)]
pub struct ReplayReport {
    /// 가입 크기 이벤트 하나(`SignedEvent::verify`: 혼합 서명 + 본문 해시).
    pub event_verify: Duration,
    /// 기기 하나짜리 서명된 목록 하나(`SignedDeviceList::verify`: 신원 서명 + 공동 서명).
    pub list_verify: Duration,
    /// 사용자 수만큼 가입을 다시 볼 때 위 둘의 합(초대 증명·프로필 확인, 복호화, 해소는 빠짐).
    pub joins_estimate: Duration,
}

/// 전파가 묻는 상태 색인 비용. 부분은 [기기 수·목록, 채널 두 개(모두 보는 채널, 일부만 보는 채널), 기기 → 사용자].
#[derive(Debug, Clone, Serialize)]
pub struct IndexReport {
    /// 새 사본에서 처음(기기 목록·기기 → 사용자 표·채널 두 개 계산).
    pub first: [Duration; 3],
    /// 같은 사본에서 다시(전파 한 번마다 드는 몫, 평균, 합).
    pub again: Duration,
    /// 멤버 하나가 들어온 다음 사본에서 처음(기기 → 사용자 표는 앞 사본에서 고침).
    pub next: [Duration; 3],
}

/// 새 프로세스에서 처음 읽기.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColdReport {
    pub open: Duration,
    /// `Store::load_space_state`(행에서 상태 다시 짓기).
    pub load: Duration,
    /// 읽은 상태로 상태 색인 처음 계산(전파 한 번 몫).
    pub index: Duration,
    /// 기기 키 표 LRU만큼(4,096개) 키를 DB에서 읽기.
    pub keys: Duration,
    pub members: usize,
    pub devices: usize,
    /// 위를 다 들고 있을 때의 상주 메모리와 최대 상주 메모리(MB).
    pub rss_mb: Option<f64>,
    pub peak_rss_mb: Option<f64>,
    /// 읽기 전 상주 메모리(프로그램·SQLCipher만).
    pub base_rss_mb: Option<f64>,
}

/// `--store` 측정 결과.
#[derive(Debug, Clone, Serialize)]
pub struct StoreReport {
    pub users: usize,
    pub devices: usize,
    /// 상태 짓기(키 약속·목록 해시 계산 포함).
    pub build: Duration,
    /// 처음 통째로 쓰기(상태 행 + 기기 키 행).
    pub full_write: Duration,
    /// 상태 행 값 바이트 합(처음 쓰기).
    pub state_bytes: usize,
    /// DB 파일 크기(체크포인트 뒤, MB). 기기 키 표가 대부분.
    pub db_mb: f64,
    pub changes: Vec<ChangeReport>,
    pub index: IndexReport,
    pub cold: ColdReport,
    pub replay: ReplayReport,
}

/// 새 프로세스에 넘기는 것(임시 DB 디렉터리의 `bench.json`).
#[derive(Serialize, Deserialize)]
struct Handoff {
    key: [u8; 32],
    space: [u8; 16],
    channels: Vec<[u8; 16]>,
    probe_user: [u8; 32],
    probe_device: [u8; 32],
}

fn tag32(tag: u8, i: usize) -> [u8; 32] {
    let mut b = [tag; 32];
    b[..8].copy_from_slice(&(i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15).to_le_bytes());
    b
}

fn tag16(tag: u8, i: usize) -> [u8; 16] {
    let mut b = [tag; 16];
    b[..8].copy_from_slice(&(i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15).to_le_bytes());
    b
}

fn user_id(i: usize) -> UserId {
    UserId(tag32(0x11, i))
}

/// 짓는 크루.
struct Crew {
    space: SpaceId,
    state: SpaceState,
    keys: Vec<DeviceKeys>,
    channels: Vec<ChannelId>,
    /// 일부만 보는 채널.
    restricted: ChannelId,
    roles: Vec<RoleId>,
    /// 상자 키 몇 개를 돌려 씀(X-Wing 키 만들기가 기기마다 들지 않게, 키 약속은 서명 키로 달라짐).
    pool: Vec<BoxPublicKey>,
    users: usize,
}

impl Crew {
    fn entry(&self, i: usize, k: usize, version: u64) -> DeviceEntry {
        entry(&self.pool, i, k, version)
    }

    /// 사용자 `i`의 기기 `n`대 목록(`version`판).
    fn list(&self, i: usize, n: usize, version: u64) -> DeviceList {
        DeviceList {
            user: user_id(i),
            version,
            devices: (0..n).map(|k| self.entry(i, k, version)).collect(),
            otp: None,
        }
    }
}

fn entry(pool: &[BoxPublicKey], i: usize, k: usize, version: u64) -> DeviceEntry {
    let mut sign_keys = DeviceSignKeys::filled(0x5a);
    sign_keys.ed_pk = tag32(0x20 + k as u8, i);
    DeviceEntry {
        device: sign_keys.device_id(),
        box_key: pool[(i + k) % pool.len()].clone(),
        sign_keys,
        name: format!("기기 {k}"),
        added_at: Timestamp(T0 + version),
        revoked_at: None,
    }
}

fn member(user: UserId, roles: Vec<RoleId>, i: usize) -> Arc<Member> {
    Arc::new(Member {
        user,
        roles,
        nickname: i.is_multiple_of(3).then(|| format!("멤버{i}")),
        automation: false,
    })
}

/// 사용자 `users`명, 기기 `devices`대(앞쪽 `devices - users`명이 두 대)인 크루.
fn build(users: usize, devices: usize) -> Crew {
    let pool: Vec<BoxPublicKey> = (0..16u8).map(BoxPublicKey::filled).collect();
    let roles: Vec<RoleId> = (0..ROLES).map(|r| RoleId(tag16(0x31, r))).collect();
    let mut role_table = BTreeMap::new();
    role_table.insert(
        RoleId::EVERYONE,
        Role::everyone(Permissions::DEFAULT_EVERYONE),
    );
    for (r, id) in roles.iter().enumerate() {
        role_table.insert(
            *id,
            Role {
                id: *id,
                name: format!("역할 {r}"),
                color: 0x33_66_99,
                rank: r as u32 + 1,
                permissions: Permissions::DEFAULT_EVERYONE,
                automation: false,
            },
        );
    }
    let two = devices.saturating_sub(users);
    let mut members = SharedMap::new();
    let mut devs = SharedMap::new();
    let mut keys = Vec::with_capacity(devices);
    for i in 0..users {
        let u = user_id(i);
        let n = 1 + usize::from(i < two);
        let list = DeviceList {
            user: u,
            version: 1,
            devices: (0..n).map(|k| entry(&pool, i, k, 1)).collect(),
            otp: None,
        };
        keys.extend(list.devices.iter().map(DeviceEntry::keys));
        // 열 명 중 한 명은 역할 하나(첫 역할만 일부만 보는 채널을 봄).
        let r = if i.is_multiple_of(10) {
            vec![roles[i / 10 % ROLES]]
        } else {
            Vec::new()
        };
        members.insert(u, member(u, r, i));
        devs.insert(u, Arc::new(MemberDevices::from_list(&list)));
    }
    let mut channels = SharedMap::new();
    let ids: Vec<ChannelId> = (0..CHANNELS).map(|c| ChannelId(tag16(0x41, c))).collect();
    for (c, id) in ids.iter().enumerate() {
        channels.insert(*id, ChannelSettings::text(format!("채널 {c}")));
    }
    let restricted = ids[CHANNELS - 1];
    if let Some(ch) = channels.get_mut(&restricted) {
        ch.overwrites = vec![
            PermissionOverwrite {
                target: OverwriteTarget::Role(RoleId::EVERYONE),
                allow: Permissions::NONE,
                deny: Permissions::VIEW_CHANNEL,
            },
            PermissionOverwrite {
                target: OverwriteTarget::Role(roles[0]),
                allow: Permissions::VIEW_CHANNEL,
                deny: Permissions::NONE,
            },
        ];
    }
    let owner = user_id(0);
    let mut bans = SharedMap::new();
    for b in 0..users / 30 {
        bans.insert(
            UserId(tag32(0x61, b)),
            BanInfo {
                reason: String::new(),
                banned_by: owner,
                banned_at: Timestamp(T0),
            },
        );
    }
    let committer = devs
        .get(&owner)
        .and_then(|d: &Arc<MemberDevices>| d.devices.first().map(|r| r.device))
        .unwrap_or(DeviceId([0; 32]));
    let state = SpaceState {
        kind: SpaceKind::Crew,
        owner,
        committer,
        settings: SpaceSettings {
            name: "큰 크루".into(),
            description: String::new(),
            icon: None,
            icon_key: None,
            allow_external_emoji: false,
            strict_rotation: false,
        },
        members,
        roles: role_table,
        channels,
        devices: devs,
        invites: Default::default(),
        emojis: BTreeMap::new(),
        pending_crew_conversion: None,
        bans,
        archives: BTreeMap::new(),
        channel_transport: BTreeMap::new(),
        role_display: Default::default(),
        anonymous_channels: Default::default(),
        crew_servers: Default::default(),
        forum_featured: Default::default(),
        attach_limits: Default::default(),
        volatile_caps: Default::default(),
        edit_rules: Default::default(),
        emoji_limits: Default::default(),
        emoji_by: Default::default(),
        disbanded: false,
    };
    Crew {
        space: SpaceId(tag16(0x71, 0)),
        state,
        keys,
        channels: vec![ids[0], restricted],
        restricted,
        roles,
        pool,
        users,
    }
}

/// 바꾸기 종류.
#[derive(Clone, Copy)]
enum Change {
    Join,
    DeviceAdd,
    Role,
    Ban,
    ChannelRule,
    Settings,
}

impl Change {
    const ALL: [Change; 6] = [
        Change::Join,
        Change::DeviceAdd,
        Change::Role,
        Change::Ban,
        Change::ChannelRule,
        Change::Settings,
    ];

    fn name(self) -> &'static str {
        match self {
            Change::Join => "가입(기기 1대)",
            Change::DeviceAdd => "기기 더하기",
            Change::Role => "역할 바꾸기",
            Change::Ban => "밴(멤버 빼기 + 밴)",
            Change::ChannelRule => "채널 권한 바꾸기",
            Change::Settings => "크루 설정 바꾸기",
        }
    }
}

/// `n`번째 바꾸기가 고르는 사용자. 앞쪽 두 대짜리 사용자와 겹치지 않게 고루 고릅니다.
fn pick(crew: &Crew, n: usize) -> usize {
    1 + (n * 7919) % (crew.users - 1)
}

/// `s`에 `n`번째 바꾸기를 한 새 판과 새로 배울 기기 키.
fn changed(crew: &Crew, s: &SpaceState, c: Change, n: usize) -> (SpaceState, Vec<DeviceKeys>) {
    let mut s = s.clone();
    let mut keys = Vec::new();
    let pick = pick(crew, n);
    match c {
        Change::Join => {
            let i = crew.users + 1 + n;
            let list = crew.list(i, 1, 1);
            keys.push(list.devices[0].keys());
            let u = user_id(i);
            s.members.insert(u, member(u, Vec::new(), i));
            s.devices
                .insert(u, Arc::new(MemberDevices::from_list(&list)));
        }
        Change::DeviceAdd => {
            let have = s.devices.get(&user_id(pick)).map_or(1, |d| d.devices.len());
            let list = crew.list(pick, have + 1, 2);
            keys.extend(list.devices.iter().map(DeviceEntry::keys));
            s.devices
                .insert(user_id(pick), Arc::new(MemberDevices::from_list(&list)));
        }
        Change::Role => {
            if let Some(m) = s.members.get_mut(&user_id(pick)) {
                Arc::make_mut(m).roles.push(crew.roles[n % ROLES]);
            }
        }
        Change::Ban => {
            let u = user_id(pick);
            s.members.remove(&u);
            s.bans.insert(
                u,
                BanInfo {
                    reason: "규칙 위반".into(),
                    banned_by: s.owner,
                    banned_at: Timestamp(T0 + n as u64),
                },
            );
        }
        Change::ChannelRule => {
            if let Some(ch) = s.channels.get_mut(&crew.restricted) {
                ch.overwrites.push(PermissionOverwrite {
                    target: OverwriteTarget::Member(user_id(pick)),
                    allow: Permissions::VIEW_CHANNEL,
                    deny: Permissions::NONE,
                });
            }
        }
        Change::Settings => {
            s.settings.description = format!("설명 {n}");
        }
    }
    (s, keys)
}

/// 이 스레드의 CPU 시간(ns, `/proc/thread-self/schedstat`). 못 읽으면 `None`.
fn thread_cpu_ns() -> Option<u64> {
    std::fs::read_to_string("/proc/thread-self/schedstat")
        .ok()?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

fn wal_bytes(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with("-wal"))
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}

fn db_bytes(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.metadata().ok())
        .filter(|m| m.is_file())
        .map(|m| m.len())
        .sum()
}

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(f64::total_cmp);
    v.get(v.len() / 2).copied().unwrap_or(0.0)
}

/// 이 크기 본문을 실은 상태 이벤트(본문은 무작위 바이트, 저장 크기만 맞춤).
fn state_event(
    space: SpaceId,
    by: &SigningKeypair,
    kind: EventKind,
    lamport: u64,
    body_len: usize,
) -> Result<SignedEvent> {
    let header = EventHeader {
        v: EVENT_VERSION,
        space,
        channel: None,
        kind,
        author: DeviceId(by.public()),
        user: user_id(0),
        parents: Vec::new(),
        lamport,
        created_at: Timestamp(T0 + lamport),
        key: KeyRef::State(StateKeyId([0x22; 16])),
        body_hash: [0; 32],
        body_len: 0,
        scope: None,
        state_anchor: Vec::new(),
    };
    let body = Secret32::random().0.repeat(body_len.div_ceil(32));
    Ok(SignedEvent::sign(header, body[..body_len].to_vec(), by)?)
}

/// 바꾸기 종류마다 실을 이벤트 본문 크기.
fn body_len(c: Change, keys: &[DeviceKeys], list: Option<&DeviceList>) -> Result<usize> {
    Ok(match c {
        Change::Join => JOIN_BODY,
        Change::DeviceAdd => match list {
            Some(l) => postcard::to_stdvec(l)?.len() + 2 * Sig::LEN,
            None => keys.len() * 2_300 + 2 * Sig::LEN,
        },
        _ => SMALL_BODY,
    })
}

/// [`ReplayReport`]: 가입 크기 이벤트와 기기 하나짜리 서명된 목록의 서명 확인.
fn replay_verify(space: SpaceId, users: usize) -> Result<ReplayReport> {
    let device = SigningKeypair::generate();
    let identity = SigningKeypair::generate();
    let ev = state_event(space, &device, EventKind::MemberJoin, 1, JOIN_BODY)?;
    let t = Instant::now();
    for _ in 0..VERIFY_SAMPLES {
        std::hint::black_box(&ev).verify()?;
    }
    let event_verify = t.elapsed() / VERIFY_SAMPLES;
    let sign_keys = DeviceSignKeys::of(&device);
    let list = DeviceList {
        user: UserId(identity.public()),
        version: 1,
        devices: vec![DeviceEntry {
            device: sign_keys.device_id(),
            box_key: BoxPublicKey::filled(1),
            sign_keys,
            name: "기기".into(),
            added_at: Timestamp(T0),
            revoked_at: None,
        }],
        otp: None,
    };
    let signed: SignedDeviceList = list.sign_with(&identity, &device)?;
    let t = Instant::now();
    for _ in 0..VERIFY_SAMPLES {
        std::hint::black_box(&signed).verify()?;
    }
    let list_verify = t.elapsed() / VERIFY_SAMPLES;
    Ok(ReplayReport {
        event_verify,
        list_verify,
        joins_estimate: (event_verify + list_verify) * users as u32,
    })
}

/// 사용자 `users`명·기기 `devices`대 크루를 짓고 저장·색인 비용을 잽니다. 처음 읽기는 `exe`(이 실행 파일)의
/// `scale-load`로 새 프로세스에서 잽니다.
pub fn measure(users: usize, devices: usize, exe: &Path) -> Result<StoreReport> {
    if users < 2 || devices < users {
        bail!("--store는 '사용자:기기'(기기 ≥ 사용자 ≥ 2)");
    }
    let t = Instant::now();
    let crew = build(users, devices);
    let build_time = t.elapsed();

    let dir = tempfile::Builder::new()
        .prefix("yangpa-bench-store-")
        .tempdir()?;
    let key = Secret32::random();
    let store = Store::open(dir.path(), &key)?;
    let t = Instant::now();
    let full = StateDelta::between(None, &crew.state)?;
    store.apply_state_delta(&crew.space, &full)?;
    store.put_device_keys(&crew.space, &crew.keys)?;
    let full_write = t.elapsed();
    store.checkpoint()?;
    let db_mb = db_bytes(dir.path()) as f64 / (1024.0 * 1024.0);

    // 바꾸기마다: 이벤트 + 키 행 넣기, 행 변화 계산 + 쓰기. 앞 판에 이어서 바꿉니다(실제처럼 마지막으로 쓴 판이
    // 기준). 이벤트 서명은 받는 쪽 일이 아니라 재는 구간 밖에서 합니다.
    let author = SigningKeypair::generate();
    let mut lamport = 0;
    let mut changes = Vec::new();
    let mut cur = crew.state.clone();
    for c in Change::ALL {
        let mut cpu = Vec::with_capacity(SAMPLES);
        let mut wall_max = Duration::ZERO;
        let (mut rows, mut value_bytes, mut wal_total_max, mut event_body) = (0, 0, 0, 0);
        let mut wal_event = Vec::with_capacity(SAMPLES);
        let mut wal_state = Vec::with_capacity(SAMPLES);
        for n in 0..SAMPLES {
            let (next, keys) = changed(&crew, &cur, c, n);
            let list = (matches!(c, Change::DeviceAdd)).then(|| {
                let have = next
                    .devices
                    .get(&user_id(pick(&crew, n)))
                    .map_or(1, |d| d.devices.len());
                crew.list(pick(&crew, n), have, 2)
            });
            let len = body_len(c, &keys, list.as_ref())?;
            event_body = event_body.max(len);
            lamport += 1;
            let kind = match c {
                Change::Join => EventKind::MemberJoin,
                Change::DeviceAdd => EventKind::DeviceListUpdate,
                _ => EventKind::RoleCreate,
            };
            let ev = state_event(crew.space, &author, kind, lamport, len)?;
            let pe = PersistentEvent::new(ev, SecurityLevel::Normal)?;
            store.checkpoint()?;
            let wal0 = wal_bytes(dir.path());
            let cpu0 = thread_cpu_ns();
            let t = Instant::now();
            store.insert_events_with_device_keys(&[pe], &crew.space, &keys)?;
            let wal1 = wal_bytes(dir.path());
            let delta = StateDelta::between(Some(&cur), &next)?;
            store.apply_state_delta(&crew.space, &delta)?;
            let wall = t.elapsed();
            let used = match (cpu0, thread_cpu_ns()) {
                (Some(a), Some(b)) => Duration::from_nanos(b.saturating_sub(a)),
                _ => wall,
            };
            let wal2 = wal_bytes(dir.path());
            cpu.push(used.as_secs_f64() * 1e6);
            wall_max = wall_max.max(wall);
            rows = rows.max(delta.rows());
            value_bytes = value_bytes.max(delta.value_bytes());
            wal_event.push(wal1.saturating_sub(wal0) as f64);
            wal_state.push(wal2.saturating_sub(wal1) as f64);
            wal_total_max = wal_total_max.max(wal2.saturating_sub(wal0));
            cur = next;
        }
        let cpu_max_us = cpu.iter().copied().fold(0.0, f64::max);
        let max = |v: &[f64]| v.iter().copied().fold(0.0, f64::max) as u64;
        let (wal_event_max, wal_state_max) = (max(&wal_event), max(&wal_state));
        changes.push(ChangeReport {
            kind: c.name(),
            samples: SAMPLES,
            cpu_median_us: median(&mut cpu),
            cpu_max_us,
            wall_max_us: wall_max.as_secs_f64() * 1e6,
            rows,
            value_bytes,
            wal_event_median: median(&mut wal_event) as u64,
            wal_event_max,
            wal_state_median: median(&mut wal_state) as u64,
            wal_state_max,
            wal_total_max,
            event_body,
        });
    }
    // 바꾼 뒤에도 행에서 똑같이 다시 지어지는지(측정이 틀린 상태를 재지 않게).
    let back = store
        .load_space_state(&crew.space)?
        .context("저장한 상태가 없음")?;
    if back != cur {
        bail!("다시 읽은 상태가 마지막으로 쓴 판과 다름");
    }

    // 상태 색인: 전파 한 번이 묻는 것.
    let probe_user = user_id(crew.users / 2);
    let probe_device = cur
        .devices
        .get(&probe_user)
        .and_then(|d| d.devices.first().map(|r| r.device))
        .context("시험 기기")?;
    let idx = IndexProbe::default();
    let s1 = Arc::new(cur.clone());
    let first = idx.touch(
        &crew.space,
        &s1,
        &crew.channels,
        (&probe_user, &probe_device),
    );
    let reps = 1000u32;
    let t = Instant::now();
    for _ in 0..reps {
        std::hint::black_box(idx.touch(
            &crew.space,
            &s1,
            &crew.channels,
            (&probe_user, &probe_device),
        ));
    }
    let again = t.elapsed() / reps;
    let (joined, _) = changed(&crew, &cur, Change::Join, SAMPLES + 1);
    let s2 = Arc::new(joined);
    let next = idx.touch(
        &crew.space,
        &s2,
        &crew.channels,
        (&probe_user, &probe_device),
    );

    let handoff = Handoff {
        key: key.0,
        space: crew.space.0,
        channels: crew.channels.iter().map(|c| c.0).collect(),
        probe_user: probe_user.0,
        probe_device: probe_device.0,
    };
    std::fs::write(dir.path().join("bench.json"), serde_json::to_vec(&handoff)?)?;
    store.checkpoint()?;
    drop(store);
    let out = std::process::Command::new(exe)
        .arg("scale-load")
        .arg(dir.path())
        .output()
        .context("scale-load 실행")?;
    if !out.status.success() {
        bail!(
            "scale-load 실패: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let cold: ColdReport =
        serde_json::from_slice(&out.stdout).context("scale-load 출력이 JSON이 아님")?;
    let replay = replay_verify(crew.space, users)?;

    Ok(StoreReport {
        users,
        devices,
        build: build_time,
        full_write,
        state_bytes: full.value_bytes(),
        db_mb,
        changes,
        index: IndexReport { first, again, next },
        cold,
        replay,
    })
}

/// 새 프로세스: 저장된 크루를 처음 읽고 색인·키 표를 데운 뒤의 시간과 상주 메모리.
pub fn cold_load(dir: &Path) -> Result<ColdReport> {
    let base_rss_mb = rss_mb();
    let h: Handoff =
        serde_json::from_slice(&std::fs::read(dir.join("bench.json")).context("bench.json 읽기")?)?;
    let space = SpaceId(h.space);
    let t = Instant::now();
    let store = Store::open(dir, &Secret32(h.key))?;
    let open = t.elapsed();
    let t = Instant::now();
    let state = store
        .load_space_state(&space)?
        .context("저장한 상태가 없음")?;
    let load = t.elapsed();
    let state = Arc::new(state);
    let channels: Vec<ChannelId> = h.channels.iter().map(|c| ChannelId(*c)).collect();
    let idx = IndexProbe::default();
    let index: Duration = idx
        .touch(
            &space,
            &state,
            &channels,
            (&UserId(h.probe_user), &DeviceId(h.probe_device)),
        )
        .iter()
        .sum();
    let commits: Vec<[u8; 32]> = state
        .active_devices()
        .take(LRU_ENTRIES)
        .map(|(_, d)| d.key_commit)
        .collect();
    let t = Instant::now();
    let held = store.device_keys(&commits)?;
    let keys = t.elapsed();
    if held.len() != commits.len() {
        bail!("기기 키 표에 없는 키 약속: {}", commits.len() - held.len());
    }
    let report = ColdReport {
        open,
        load,
        index,
        keys,
        members: state.members.len(),
        devices: state.active_devices().count(),
        rss_mb: rss_mb(),
        peak_rss_mb: peak_rss_mb(),
        base_rss_mb,
    };
    std::hint::black_box((&state, &idx, &held));
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 작은 크루로 행 쓰기가 바뀐 만큼만이고 다시 읽은 상태가 같은지(새 프로세스 없이).
    #[test]
    fn small_crew_round_trips_and_writes_little() {
        let crew = build(300, 450);
        assert_eq!(crew.state.active_devices().count(), 450);
        assert_eq!(crew.keys.len(), 450);
        let (s, _d) = Store::open_temp().unwrap();
        s.apply_state_delta(
            &crew.space,
            &StateDelta::between(None, &crew.state).unwrap(),
        )
        .unwrap();
        assert_eq!(s.put_device_keys(&crew.space, &crew.keys).unwrap(), 450);
        let mut cur = crew.state.clone();
        let author = SigningKeypair::generate();
        for (i, c) in Change::ALL.into_iter().enumerate() {
            let (next, keys) = changed(&crew, &cur, c, 3);
            let ev =
                state_event(crew.space, &author, EventKind::MemberJoin, i as u64 + 1, 64).unwrap();
            let pe = PersistentEvent::new(ev, SecurityLevel::Normal).unwrap();
            s.insert_events_with_device_keys(&[pe], &crew.space, &keys)
                .unwrap();
            let d = StateDelta::between(Some(&cur), &next).unwrap();
            assert!(d.rows() <= 3, "{}: {} 행", c.name(), d.rows());
            s.apply_state_delta(&crew.space, &d).unwrap();
            cur = next;
        }
        assert_eq!(s.load_space_state(&crew.space).unwrap().unwrap(), cur);
        let commits: Vec<[u8; 32]> = cur.active_devices().map(|(_, d)| d.key_commit).collect();
        assert_eq!(s.device_keys(&commits).unwrap().len(), commits.len());
    }
}
