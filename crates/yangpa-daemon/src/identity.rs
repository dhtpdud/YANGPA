//! 신원·기기: 만들기, 서명된 기기 목록과 프로필, DB 키 보관(OS 키 저장소 또는 0600 파일). 새 기기 연결은 `devlink`.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use yangpa_core::body::{EventBody, Secret32};
use yangpa_core::device::{
    DeviceEntry, DeviceList, DeviceRecord, DeviceSignKeys, ListChange, Profile, SignedDeviceList,
    SignedProfile,
};
use yangpa_core::ids::{DeviceId, EventId, SpaceId, UserId};
use yangpa_core::model::{SpaceKind, SpaceState};
use yangpa_core::time::Timestamp;
use yangpa_core::wire::{
    OwnContact, PeerAuth, SocialReply, SocialRequest, WireErrorCode, YpRequest, YpResponse,
};
use yangpa_crypto::{DeviceKeys, Identity};
use yangpa_net::PeerId;
use yangpa_rpc::api::Notification;
use yangpa_store::{Store, secret_names};
use yangpa_sync::state::limits::MAX_DEVICE_ENTRIES;
use zeroize::Zeroizing;

use crate::author::{self, KeyChoice};
use crate::config::{KeyStorage, NodeConfig};
// keyring의 기본 저장소는 Android를 몰라, Android Keystore 저장소를 정한 keyring-core를 바로 씁니다.
#[cfg(target_os = "android")]
use keyring_core as keyring;

use crate::error::{Error, Result};
use crate::node::Node;
use crate::{friends, ingest, notes, peers, spaces, sync_service};

/// DB 키 파일 이름(파일 보관 방식).
pub const DB_KEY_FILE: &str = "db.key";
const KEYRING_SERVICE: &str = "yangpa";
/// 기기 이름 설정 키.
const DEVICE_NAME_SETTING: &str = "me.device_name";
/// 내 다른 기기가 휴대폰이라고 알린 기록.
const MOBILE_PREFIX: &str = "device.mobile.";
/// 이 기기가 휴대폰인지.
const MOBILE: bool = cfg!(any(target_os = "android", target_os = "ios"));
/// 예전 기본 기기 이름.
const OLD_DEFAULT_NAME: &str = "이 기기";

/// 노드 수준 신원 상태(RAM).
#[derive(Default)]
pub(crate) struct IdentityState {
    /// 사용자마다 아직 판정할 수 없어 미뤄 둔 기기 목록과 마지막으로 미룬 시각([`defer_list`]).
    deferred_lists: parking_lot::Mutex<HashMap<UserId, (Instant, Vec<SignedDeviceList>)>>,
    /// 내 기기로 연결된 뒤 첫 동기화 중([`spawn_first_sync`]). 앱을 다시 켜면 주기 동기화가 화면 없이 이어 받습니다.
    first_sync: AtomicBool,
    /// 내 다른 기기가 내 공간 목록을 알려 왔음(첫 동기화는 이것을 받은 뒤에 끝낼 수 있음).
    own_spaces_known: AtomicBool,
}

/// 미뤄 두는 기기 목록의 상한(사용자 수, 사용자마다 판 수)과 보관 시간.
const DEFERRED_USERS: usize = 256;
const DEFERRED_PER_USER: usize = 4;
const DEFERRED_TTL: Duration = Duration::from_secs(600);

/// 이 기기의 신원.
pub struct Me {
    pub identity: Identity,
    pub device: DeviceKeys,
    pub devices: SignedDeviceList,
    pub profile: SignedProfile,
}

impl Me {
    pub fn user(&self) -> UserId {
        self.identity.user_id()
    }

    pub fn device_id(&self) -> DeviceId {
        self.device.device_id()
    }
}

impl std::fmt::Debug for Me {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Me")
            .field("user", &self.user())
            .field("device", &self.device_id())
            .finish_non_exhaustive()
    }
}

// ───────────── DB 키 보관 ─────────────

/// DB 키를 읽거나 처음이면 만듭니다.
///
/// 이미 DB가 있는데 키를 읽을 수 없으면(키 저장소가 잠겼거나 늦게 뜸) 새 키를 만들지 않고 시작을 멈춥니다.
/// 새 키로 바꾸면 기존 DB를 영영 열 수 없게 되기 때문입니다(다음 시작에서 다시 시도).
pub(crate) fn load_or_create_db_key(cfg: &NodeConfig) -> Result<Secret32> {
    #[cfg(target_os = "android")]
    match android_native_keyring_store::Store::new() {
        Ok(store) => keyring_core::set_default_store(store),
        Err(e) => tracing::warn!(error = %e, "Android Keystore를 쓸 수 없습니다"),
    }
    let key = db_key(cfg)?;
    if !cfg.data_dir.join(DB_KEY_FILE).exists() {
        remember_keyring_account(&cfg.data_dir);
    }
    Ok(key)
}

fn db_key(cfg: &NodeConfig) -> Result<Secret32> {
    let file = cfg.data_dir.join(DB_KEY_FILE);
    let db_exists = cfg.data_dir.join(yangpa_store::DB_FILE_NAME).exists();
    match cfg.key_storage {
        KeyStorage::File => {
            if db_exists && !file.exists() {
                return Err(Error::internal(
                    "DB가 있지만 DB 키 파일이 없습니다(다른 키 보관 방식으로 만든 DB일 수 있음)",
                ));
            }
            file_key(&file)
        }
        KeyStorage::OsKeyring => match keyring_get(&cfg.data_dir) {
            KeyringProbe::Found(k) => Ok(k),
            KeyringProbe::Missing if !db_exists => keyring_create(&cfg.data_dir),
            KeyringProbe::Missing => Err(Error::internal(
                "DB가 있지만 OS 키 저장소에 DB 키가 없습니다",
            )),
            KeyringProbe::Unavailable => Err(Error::internal("OS 키 저장소를 쓸 수 없습니다")),
        },
        KeyStorage::Auto => {
            // 이미 파일로 만든 설치는 파일을 계속 씁니다(키 저장소가 나중에 생겨도 DB를 잃지 않게).
            let probe = if file.exists() {
                KeyringProbe::Unavailable
            } else {
                keyring_get(&cfg.data_dir)
            };
            match auto_choice(file.exists(), db_exists, probe) {
                AutoChoice::File => file_key(&file),
                AutoChoice::Keyring(k) => Ok(k),
                AutoChoice::CreateKeyring => match keyring_create(&cfg.data_dir) {
                    Ok(k) => Ok(k),
                    Err(_) => {
                        tracing::warn!("OS 키 저장소에 쓸 수 없어 DB 키를 0600 파일에 둡니다");
                        file_key(&file)
                    }
                },
                AutoChoice::CreateFile => {
                    tracing::warn!("OS 키 저장소를 쓸 수 없어 DB 키를 0600 파일에 둡니다");
                    file_key(&file)
                }
                AutoChoice::Fail(why) => Err(Error::internal(why)),
            }
        }
    }
}

/// OS 키 저장소에서 DB 키를 찾아본 결과.
enum KeyringProbe {
    Found(Secret32),
    /// 저장소는 쓸 수 있고 항목이 없음.
    Missing,
    /// 저장소를 쓸 수 없음(잠김·세션 버스 없음·시간 초과).
    Unavailable,
}

#[derive(Debug, PartialEq, Eq)]
enum AutoChoice {
    File,
    Keyring(Secret32),
    CreateKeyring,
    CreateFile,
    Fail(&'static str),
}

/// `KeyStorage::Auto`의 결정(입출력 없음).
fn auto_choice(file_exists: bool, db_exists: bool, probe: KeyringProbe) -> AutoChoice {
    if file_exists {
        return AutoChoice::File;
    }
    match probe {
        KeyringProbe::Found(k) => AutoChoice::Keyring(k),
        _ if db_exists => AutoChoice::Fail(
            "DB가 있지만 OS 키 저장소에서 DB 키를 읽을 수 없습니다(키 저장소를 풀고 다시 시작하세요)",
        ),
        KeyringProbe::Missing => AutoChoice::CreateKeyring,
        KeyringProbe::Unavailable => AutoChoice::CreateFile,
    }
}

fn file_key(path: &Path) -> Result<Secret32> {
    if let Ok(bytes) = std::fs::read(path) {
        let bytes = Zeroizing::new(bytes);
        let arr: [u8; 32] = bytes[..]
            .try_into()
            .map_err(|_| Error::internal("DB 키 파일 형식이 잘못됨"))?;
        return Ok(Secret32(arr));
    }
    let key = Secret32::random();
    write_private_file(path, &key.0)?;
    Ok(key)
}

/// 0600 파일로 씁니다(임시 파일 후 이름 바꾸기).
pub(crate) fn write_private_file(path: &Path, data: &[u8]) -> Result<()> {
    use std::io::Write;
    let dir = path.parent().unwrap_or(Path::new("."));
    yangpa_rpc::endpoint::create_private_dir(dir)?;
    let tmp = dir.join(format!(
        ".{}.tmp-{}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("key"),
        std::process::id()
    ));
    let _ = std::fs::remove_file(&tmp);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let r = (|| {
        let mut f = opts.open(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    Ok(r?)
}

/// OS 키 저장소 계정 이름 파일. 계정 이름은 처음 쓴 데이터 경로의 해시라, 데이터 폴더를 옮겨도(포터블 폴더 바꾸기)
/// 같은 키를 찾게 데이터와 함께 둡니다. 비밀이 아닌 이름만 적힙니다.
pub const KEYRING_ACCOUNT_FILE: &str = "db.keyring";

fn keyring_account(data_dir: &Path) -> String {
    let saved = std::fs::read_to_string(data_dir.join(KEYRING_ACCOUNT_FILE)).ok();
    account_from(saved.as_deref(), data_dir)
}

/// 파일에 적힌 이름이 이 형식(`db-` + 16진수 16자)이면 그것, 아니면 경로 해시.
fn account_from(saved: Option<&str>, data_dir: &Path) -> String {
    if let Some(s) = saved.map(str::trim)
        && s.len() == 19
        && s.strip_prefix("db-")
            .is_some_and(|h| h.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return s.to_owned();
    }
    format!(
        "db-{}",
        &blake3::hash(data_dir.to_string_lossy().as_bytes()).to_hex()[..16]
    )
}

/// 키 저장소에서 키를 찾았거나 만든 뒤 계정 이름을 적어 둡니다(이미 있으면 그대로). 못 적어도 이 경로에서는 계속 열립니다.
fn remember_keyring_account(data_dir: &Path) {
    let path = data_dir.join(KEYRING_ACCOUNT_FILE);
    if path.exists() {
        return;
    }
    if let Err(e) = write_private_file(&path, keyring_account(data_dir).as_bytes()) {
        tracing::warn!("키 저장소 계정 이름을 적지 못했습니다: {e}");
    }
}

/// 키 저장소 호출을 별도 스레드에서 시간 제한을 두고 돌립니다(세션 버스 없는 리눅스 등에서 오래 걸릴 수 있음).
fn with_keyring_timeout<T: Send + 'static>(
    f: impl FnOnce() -> Option<T> + Send + 'static,
) -> Option<T> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(Duration::from_secs(5)).ok().flatten()
}

/// OS 키 저장소에서 DB 키를 읽기만 합니다.
fn keyring_get(data_dir: &Path) -> KeyringProbe {
    let account = keyring_account(data_dir);
    with_keyring_timeout(move || {
        let entry = keyring::Entry::new(KEYRING_SERVICE, &account).ok()?;
        Some(match entry.get_secret() {
            Ok(bytes) => {
                let bytes = Zeroizing::new(bytes);
                match <[u8; 32]>::try_from(&bytes[..]) {
                    Ok(arr) => KeyringProbe::Found(Secret32(arr)),
                    Err(_) => KeyringProbe::Unavailable,
                }
            }
            Err(keyring::Error::NoEntry) => KeyringProbe::Missing,
            Err(_) => KeyringProbe::Unavailable,
        })
    })
    .unwrap_or(KeyringProbe::Unavailable)
}

/// OS 키 저장소에 새 DB 키를 만듭니다(DB가 아직 없을 때만 부름).
fn keyring_create(data_dir: &Path) -> Result<Secret32> {
    let account = keyring_account(data_dir);
    with_keyring_timeout(move || {
        let entry = keyring::Entry::new(KEYRING_SERVICE, &account).ok()?;
        let key = Secret32::random();
        entry.set_secret(&key.0).ok()?;
        Some(key)
    })
    .ok_or_else(|| Error::internal("OS 키 저장소에 DB 키를 쓸 수 없습니다"))
}

// ───────────── 신원 불러오기·만들기 ─────────────

pub(crate) fn load_me(store: &Store) -> Result<Option<Me>> {
    let Some(seed) = store.get_secret(secret_names::IDENTITY)? else {
        return Ok(None);
    };
    let seed: [u8; 32] = seed[..]
        .try_into()
        .map_err(|_| Error::internal("신원 키 형식이 잘못됨"))?;
    let identity = Identity::from_seed(&seed);
    let dev = store
        .get_secret(secret_names::DEVICE)?
        .ok_or_else(|| Error::internal("기기 키가 없음"))?;
    let device = DeviceKeys::from_bytes(&dev)?;
    let user = identity.user_id();
    let devices = store
        .get_device_list(&user)?
        .ok_or_else(|| Error::internal("내 기기 목록이 없음"))?;
    let profile = store
        .get_profile(&user)?
        .ok_or_else(|| Error::internal("내 프로필이 없음"))?;
    Ok(Some(Me {
        identity,
        device,
        devices,
        profile,
    }))
}

/// 새 기기의 신원을 저장합니다(만들기·기기 연결 공통).
fn install(
    node: &Arc<Node>,
    identity: Identity,
    device: DeviceKeys,
    devices: SignedDeviceList,
    profile: SignedProfile,
) -> Result<Arc<Me>> {
    // 확인부터 모든 쓰기까지 한 줄로(동시에 만들면 한 신원의 기기 키와 다른 신원의 시드가 섞여 저장될 수 있음).
    let _g = node.identity_lock.lock();
    if node.me.read().is_some() {
        return Err(Error::conflict("이미 신원이 있습니다"));
    }
    let store = &node.store;
    store.put_secret(secret_names::DEVICE, &device.to_bytes())?;
    store.put_device_list(&devices)?;
    store.put_profile(&profile)?;
    store.put_setting(DEVICE_NAME_SETTING, &node.cfg.device_name)?;
    let me = Me {
        identity,
        device,
        devices,
        profile,
    };
    // 신원 시드를 마지막에 써서, 중간에 실패하면 다음 시작 때 신원 없음으로 보입니다.
    store.put_secret(secret_names::IDENTITY, &me.identity.signing.seed()[..])?;
    let me = Arc::new(me);
    *node.me.write() = Some(me.clone());
    tracing::info!(user = %me.user().short(), device = %me.device_id().short(), "신원 설치");
    Ok(me)
}

pub(crate) fn create(node: &Arc<Node>, name: &str) -> Result<Arc<Me>> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 100 {
        return Err(Error::invalid("이름은 1~100자여야 합니다"));
    }
    if yangpa_core::badge::has_emoji(name) {
        return Err(Error::invalid(NAME_EMOJI));
    }
    let identity = Identity::generate();
    let device = DeviceKeys::generate();
    let now = Timestamp::now();
    let devices = DeviceList {
        user: identity.user_id(),
        version: now.0,
        devices: vec![device_entry(node, &device, now)],
        otp: None,
    }
    .sign_with(&identity.signing, &device.signing)?;
    let profile = Profile {
        version: now.0,
        name: name.to_owned(),
        ..Default::default()
    }
    .sign(&identity.signing)?;
    install(node, identity, device, devices, profile)
}

/// 이 기기의 기기 목록 항목.
pub(crate) fn device_entry(node: &Node, device: &DeviceKeys, now: Timestamp) -> DeviceEntry {
    DeviceEntry {
        device: device.device_id(),
        box_key: device.box_public(),
        sign_keys: DeviceSignKeys::of(&device.signing),
        name: node.cfg.device_name.clone(),
        added_at: now,
        revoked_at: None,
    }
}

/// 내 다른 기기가 승인해 준 신원을 설치합니다. 목록은 승인한 기기가 공동 서명한 판이라 닿는 사람마다 그대로 받습니다.
pub(crate) fn install_linked(
    node: &Arc<Node>,
    identity: Identity,
    device: DeviceKeys,
    devices: SignedDeviceList,
    profile: SignedProfile,
) -> Result<Arc<Me>> {
    let me = install(node, identity, device, devices, profile)?;
    // 앱이 알림을 받고 `identity.get`을 읽을 때 이미 켜져 있게 설치하자마자 켭니다.
    node.identity.first_sync.store(true, Ordering::SeqCst);
    Ok(me)
}

/// 이름에 이모지를 넣으면(뱃지와 헷갈리지 않게). 받은 프로필은 막지 않습니다(옛 이름·옛 빌드).
const NAME_EMOJI: &str = "이름에 이모지를 넣을 수 없습니다";

/// 프로필 바꾸기. 이미지(아바타·배너)는 블롭 영역이 맡아 `blobs::import_profile_image`로 넘깁니다.
pub(crate) struct ProfileChange {
    pub name: Option<String>,
    pub bio: Option<String>,
    pub theme_color: Option<Option<u32>>,
    pub status: Option<String>,
    pub avatar: Option<Option<yangpa_core::ids::BlobId>>,
    pub banner: Option<Option<yangpa_core::ids::BlobId>>,
}

pub(crate) fn set_profile(node: &Arc<Node>, change: ProfileChange) -> Result<Profile> {
    let _g = node.identity_lock.lock();
    let me = node.me()?;
    let mut p = me.profile.profile.clone();
    if let Some(n) = change.name {
        let n = n.trim().to_owned();
        if n.is_empty() || n.chars().count() > 100 {
            return Err(Error::invalid("이름은 1~100자여야 합니다"));
        }
        // 바꿀 때만 봅니다(예전에 이모지를 넣은 사람도 이름을 그대로 두고 소개는 고칠 수 있게).
        if n != p.name && yangpa_core::badge::has_emoji(&n) {
            return Err(Error::invalid(NAME_EMOJI));
        }
        p.name = n;
    }
    if let Some(b) = change.bio {
        if b.chars().count() > 1024 {
            return Err(Error::invalid("소개는 1024자 이하여야 합니다"));
        }
        p.bio = b;
    }
    if let Some(s) = change.status {
        if s.chars().count() > 128 {
            return Err(Error::invalid("상태 메시지는 128자 이하여야 합니다"));
        }
        p.status = s;
    }
    if let Some(c) = change.theme_color {
        if c.is_some_and(|c| c > 0xFF_FFFF) {
            return Err(Error::invalid("테마 색은 0xRRGGBB여야 합니다"));
        }
        p.theme_color = c;
    }
    // 이미지 키는 서명된 프로필에 함께 실어 친구·멤버가 열 수 있게 합니다(B3, 로컬 API JSON에는 나가지 않음).
    if let Some(a) = change.avatar {
        p.avatar = a;
        p.avatar_key = a.and_then(|b| crate::blobs::profile_image_key(node, &b));
    }
    if let Some(b) = change.banner {
        p.banner = b;
        p.banner_key = b.and_then(|b| crate::blobs::profile_image_key(node, &b));
    }
    p.version = p.version.saturating_add(1).max(Timestamp::now().0);
    let signed = p.clone().sign(&me.identity.signing)?;
    node.store.put_profile(&signed)?;
    let new_me = Me {
        identity: me.identity.clone(),
        device: me.device.clone(),
        devices: me.devices.clone(),
        profile: signed,
    };
    *node.me.write() = Some(Arc::new(new_me));
    Ok(p)
}

/// 사용자의 프로필. 모르면 기본값(이름 없음).
pub(crate) fn profile_of(node: &Node, user: &UserId) -> Profile {
    node.store
        .get_profile(user)
        .ok()
        .flatten()
        .map(|p| p.profile)
        .unwrap_or_default()
}

/// 표시 이름(프로필 이름, 없으면 ID 앞부분).
pub(crate) fn display_name(node: &Node, user: &UserId) -> String {
    let p = profile_of(node, user);
    if p.name.is_empty() {
        user.short()
    } else {
        p.name
    }
}

// ───────────── 기기 목록·프로필 전파(B4, W3) ─────────────
//
// 기기 목록은 늘기만 합니다(`DeviceList::extends`): 기기를 빼려면 폐기 표시를 남깁니다. 새 판은 신원 키와 함께 그 판을
// 만든 기기가 공동 서명합니다. 신원 키는 모든 기기에 있으므로, 받는 쪽은 공동 서명 기기가 자기가 아는 이전 목록에서
// 활성인지로 판을 가립니다(`SignedDeviceList::classify`).
// - 활성 기기가 만든 판은 바로 받습니다. 내 목록이 서로 모르는 판(동시에 고친 두 기기)을 만나면 결정적으로 합쳐
//   (`DeviceList::merge`) 내 공동 서명으로 같은 판에 도착합니다.
// - 폐기된 기기가 만든 판은 받지 않습니다. 내 목록이면 그 판이 더한 기기만 폐기해 내 목록에 덧붙인 판(`DeviceList::heal`)을
//   내어, 폐기를 아직 모르고 그 판을 받은 피어도 고칩니다. 위조 판의 폐기·상자 키는 가져오지 않습니다.
// - 활성 기기의 공동 서명이 없는 판은 신원 키를 가졌어도 받지 않습니다. 새 기기는 사람이 승인한 내 기기가 목록에 넣어 줄
//   때만 들어옵니다(`devlink`). 크루 해소기는 지난 기록을 다시 적용할 때 같은 결과가 나와야 해서 멤버가 전한 복구 판을
//   그대로 받습니다(`machine.rs`, docs/decisions.md "기기 연결").
// - 폐기된 기기가 인증을 걸면 `Revoked`(그 기기를 폐기한 목록)로 답하고, 그 기기는 확인한 뒤 로컬 데이터를 모두 지웁니다.
//
// 새로 연결된 기기는 내 다른 기기가 알려 주는 공간을 바로 받아들이고 그 기기에게서 따라잡습니다.

/// 받아들인 공간(`identity.adopt.<공간>`). 상태가 생길 때까지 내 다른 기기에게서 받습니다.
const ADOPT_PREFIX: &str = "identity.adopt.";
/// 받아들였지만 이 기간 안에 해소되지 않은 공간은 포기합니다(표시와 빈 런타임을 지움).
const ADOPT_EXPIRY_MS: u64 = 30 * 24 * 60 * 60 * 1000;
/// `SocialReply::spaces`·`contacts` 상한.
const MAX_REPLY_SPACES: usize = 1024;
/// 연락처가 바뀐 뒤 내 다른 기기에 알리기까지 기다리는 시간(몰린 변경을 한 번에).
const OWN_SHARE_DELAY: Duration = Duration::from_millis(300);
/// 받아들인 공간의 상태·크루 키가 생길 때까지 바로 다시 시도하는 횟수와 간격 상한(그 뒤는 주기 동기화가 맡음).
const ADOPT_RETRIES: u32 = 40;
const ADOPT_RETRY_MAX: Duration = Duration::from_secs(5);
/// 받아들인 공간이 모두 서고 따라잡기가 없는 때가 이만큼 이어지면 첫 동기화가 끝난 것입니다.
const FIRST_SYNC_QUIET: Duration = Duration::from_secs(5);
/// 공간이 서지 못해도 따라잡기 없이 이만큼 지나면 끝냅니다(상대가 사라져도 화면에 갇히지 않게). 다시 시도 간격보다 깁니다.
const FIRST_SYNC_STALL: Duration = Duration::from_secs(45);
const FIRST_SYNC_POLL: Duration = Duration::from_millis(250);
/// 인증 직후 보낸 요청이 상대가 아직 인증을 끝내지 못해 거절될 때 다시 보내는 횟수(100ms부터 두 배).
const AFTER_AUTH_RETRIES: u32 = 5;
/// 지운 기기에 남길 네트워크 설정 접두어(STUN 서버 목록 등: 지운 뒤 다시 시작해도 고른 망 설정이 풀려 IP가 드러나지 않게).
const NET_SETTINGS_PREFIX: &str = "net.";

#[derive(Serialize, Deserialize)]
struct Adoption {
    genesis: EventId,
    #[serde(default)]
    at: Timestamp,
    /// 내 기기가 함께 알려 준 이 공간의 연락처 기록(상태가 서면 [`restore_friend`]가 씀).
    #[serde(default)]
    contact: Option<OwnContact>,
}

fn adopt_key(space: &SpaceId) -> String {
    format!("{ADOPT_PREFIX}{space}")
}

/// `node.me`를 바꿉니다(`identity_lock` 안에서).
fn set_me(node: &Node, me: &Me, devices: Option<SignedDeviceList>, profile: Option<SignedProfile>) {
    *node.me.write() = Some(Arc::new(Me {
        identity: me.identity.clone(),
        device: me.device.clone(),
        devices: devices.unwrap_or_else(|| me.devices.clone()),
        profile: profile.unwrap_or_else(|| me.profile.clone()),
    }));
}

/// 받은 서명 프로필을 검증해 저장합니다(더 새 버전일 때만 바뀜). 내 프로필이면(다른 기기에서 바꿈) 이 기기도 씁니다.
/// 바뀌면 `profile.updated`를 알리고(앱이 친구 목록·멤버를 다시 읽게) 친구의 새 사진을 바로 받습니다.
pub(crate) fn accept_profile(node: &Node, p: &SignedProfile) -> Result<bool> {
    p.verify()?;
    let changed = {
        let _g = node.identity_lock.lock();
        let changed = node.store.put_profile(p)?;
        match node.me() {
            Ok(me) if me.user() == p.user && p.profile.version > me.profile.profile.version => {
                set_me(node, &me, None, Some(p.clone()));
                true
            }
            _ => changed,
        }
    };
    if changed {
        crate::blobs::on_profile_changed(node, &p.user, &p.profile);
        node.notify
            .publish(Notification::ProfileUpdated { user: p.user });
    }
    Ok(changed)
}

/// 받은 목록의 모양 검사(항목 수·중복). 서명 확인·이어받기 비교 전에 해서 큰 목록으로 시간을 끌지 못하게 합니다.
pub(crate) fn check_list_shape(l: &SignedDeviceList) -> Result<()> {
    if l.list.well_formed(MAX_DEVICE_ENTRIES) {
        Ok(())
    } else {
        Err(Error::invalid(format!(
            "기기 목록은 {MAX_DEVICE_ENTRIES}개 이하, 기기 중복 없이"
        )))
    }
}

/// 받은 서명 기기 목록을 반영합니다. 내 목록이면 [`adopt_own_list`], 남의 목록이면 처음 보거나 아는 목록을 이어받고
/// 그 목록에서 활성인 기기가 공동 서명했을 때만 저장합니다. 바뀌었으면 true.
pub(crate) fn accept_device_list(node: &Arc<Node>, l: &SignedDeviceList) -> Result<bool> {
    let me = node.me()?;
    // 아는 판보다 높지 않은 남의 목록은 어차피 받지 않으므로(`extends`) 서명을 확인하지 않습니다. 시작 때 다시 세우면
    // 가입·기기 목록 갱신마다 지난 판을 다시 보는데, 판마다 혼합 서명을 확인하면 3만 명 크루에서 수 초가 듭니다.
    // 저장된 판은 오르기만 하므로 아래 잠금 안의 판정과 같습니다.
    if l.list.user != me.user()
        && node
            .store
            .get_device_list(&l.list.user)?
            .is_some_and(|s| s.list.version >= l.list.version)
    {
        return Ok(false);
    }
    check_list_shape(l)?;
    l.verify()?;
    if l.list.user == me.user() {
        return adopt_own_list(node, l);
    }
    {
        let _g = node.identity_lock.lock();
        let stored = node.store.get_device_list(&l.list.user)?;
        let change = match &stored {
            None => ListChange::Authorized,
            Some(s) if !l.list.extends(&s.list) => return Ok(false),
            Some(s) => l.classify(&s.list),
        };
        if change != ListChange::Authorized {
            // 공동 서명 기기를 아는 목록에서 아예 모르면 그 기기를 더한 사이 판이 늦게 오는 중일 수 있습니다(직접
            // 전송은 순서를 지키지 않음). 버리면 그 사이 판을 받은 뒤에도 이 판(예: 폐기)을 다시 받을 길이 없어
            // 미뤄 둡니다. 아는 목록에서 폐기된 기기가 서명한 판은 그냥 버립니다.
            let unknown_cosigner = l.cosigner().is_some_and(|c| {
                stored
                    .as_ref()
                    .is_some_and(|s| !s.list.devices.iter().any(|d| d.device == c))
            });
            if unknown_cosigner {
                defer_list(node, l);
            }
            tracing::debug!(user = %l.list.user.short(), deferred = unknown_cosigner, "공동 서명 기기가 활성이 아닌 기기 목록을 받지 않음");
            return Ok(false);
        }
        if !node.store.put_device_list(l)? {
            return Ok(false);
        }
    }
    forget_revoked(node, &l.list);
    retry_deferred(node, &l.list.user);
    Ok(true)
}

/// 공동 서명 기기를 아직 모르는 남의 목록을 미뤄 둡니다(사용자 수·판 수·시간에 상한). 그 사용자의 다음 판을 저장하면
/// [`retry_deferred`]가 다시 봅니다.
fn defer_list(node: &Node, l: &SignedDeviceList) {
    let now = Instant::now();
    let mut map = node.identity.deferred_lists.lock();
    map.retain(|_, (at, _)| now.duration_since(*at) < DEFERRED_TTL);
    if !map.contains_key(&l.list.user) && map.len() >= DEFERRED_USERS {
        let oldest = map.iter().min_by_key(|(_, (at, _))| *at).map(|(u, _)| *u);
        if let Some(u) = oldest {
            map.remove(&u);
        }
    }
    let (at, lists) = map.entry(l.list.user).or_insert_with(|| (now, Vec::new()));
    *at = now;
    if !lists.contains(l) {
        lists.push(l.clone());
        // 높은 판을 남깁니다(사이 판은 공간 상태를 따라잡으며 차례대로도 옴).
        lists.sort_by_key(|x| std::cmp::Reverse(x.list.version));
        lists.truncate(DEFERRED_PER_USER);
    }
}

/// 미뤄 둔 그 사용자의 목록을 낮은 판부터 다시 판정합니다(여전히 판정할 수 없으면 다시 미뤄짐).
fn retry_deferred(node: &Arc<Node>, user: &UserId) {
    let Some((_, mut lists)) = node.identity.deferred_lists.lock().remove(user) else {
        return;
    };
    lists.sort_by_key(|x| x.list.version);
    for l in lists {
        if let Err(e) = accept_device_list(node, &l) {
            tracing::debug!(user = %user.short(), error = %e, "미뤄 둔 기기 목록 다시 보기 실패");
        }
    }
}

/// 새 내 목록을 저장하고 이 기기의 `Me`를 바꿉니다(`identity_lock` 안에서).
fn adopt_locked(node: &Node, me: &Me, next: SignedDeviceList) -> Result<OwnChange> {
    if !node.store.put_device_list(&next)? {
        return Err(Error::conflict("기기 목록이 바뀌었습니다"));
    }
    set_me(node, me, Some(next.clone()), None);
    Ok(OwnChange::Adopted(next))
}

/// 내 목록을 받았을 때 할 일.
enum OwnChange {
    /// 받지 않음.
    Ignored,
    /// 새 판을 채택했음(이어받으면 그대로, 아니면 합쳐 내 공동 서명).
    Adopted(SignedDeviceList),
    /// 이 기기를 폐기한 판: 로컬 데이터를 지웁니다.
    Wipe,
}

/// 내 목록을 받습니다(바뀌면 true). 활성 기기가 만든 판은 채택하고(이 기기를 폐기했으면 지움), 폐기된 기기가 기기를 더한
/// 판은 그 기기들만 폐기한 새 판으로 바로잡습니다. 채택하면 공간·피어에 알립니다.
fn adopt_own_list(node: &Arc<Node>, l: &SignedDeviceList) -> Result<bool> {
    let change = {
        let _g = node.identity_lock.lock();
        let me = node.me()?;
        own_change(node, &me, l)?
    };
    let next = match change {
        OwnChange::Ignored => return Ok(false),
        OwnChange::Wipe => {
            spawn_wipe(node);
            return Ok(false);
        }
        OwnChange::Adopted(next) => next,
    };
    tracing::info!(
        devices = next.list.devices.len(),
        version = next.list.version,
        "내 기기 목록 갱신"
    );
    forget_revoked(node, &next.list);
    node.spawn_detached(|node| async move { announce(&node).await });
    // 내 기기가 늘거나 줄었으면 내 기기끼리의 직접 연결을 맞춥니다.
    crate::p2p::reconcile(node);
    Ok(true)
}

/// 받은 내 목록을 어떻게 할지.
fn own_change(node: &Node, me: &Me, l: &SignedDeviceList) -> Result<OwnChange> {
    let cur = &me.devices.list;
    if l.list == *cur || cur.extends(&l.list) {
        return Ok(OwnChange::Ignored);
    }
    match l.classify(cur) {
        ListChange::Authorized if l.list.is_revoked(&me.device_id()) => Ok(OwnChange::Wipe),
        ListChange::Authorized => {
            let next = if l.list.extends(cur) {
                l.clone()
            } else {
                let merged = cur.merge(&l.list);
                // 목록은 늘기만 하므로 평생 기기 수에 상한이 있습니다(넘으면 공간이 받지 않음).
                if !merged.well_formed(MAX_DEVICE_ENTRIES) {
                    tracing::warn!(
                        devices = merged.devices.len(),
                        "합친 내 기기 목록이 상한을 넘어 채택하지 않음"
                    );
                    return Err(Error::conflict("기기 목록이 가득 찼습니다"));
                }
                merged.sign_with(&me.identity.signing, &me.device.signing)?
            };
            adopt_locked(node, me, next)
        }
        // 살아 있는 기기의 승인 없이 신원 키로 자기를 더한 판은 받지 않습니다(`devlink`).
        ListChange::Recovery(_) | ListChange::OtpMissing(_) => Ok(OwnChange::Ignored),
        ListChange::Rejected => {
            // 폐기된 기기가 신원 키로 기기를 더한 판: 내 목록 그대로에 그 기기들만 폐기로 더한 판을 내어, 폐기를 아직
            // 모르고 그 판을 받은 피어도 바로잡습니다. 위조 판의 폐기·상자 키는 가져오지 않습니다(`DeviceList::heal`).
            // 공동 서명 기기를 모르면(사이 판을 놓침) 정상 판일 수 있어 건드리지 않습니다.
            let by_revoked = l.cosigner().is_some_and(|c| cur.is_revoked(&c));
            let adds = l
                .list
                .active()
                .any(|d| !cur.devices.iter().any(|c| c.device == d.device));
            if !by_revoked || !adds {
                tracing::debug!("공동 서명 기기가 활성이 아닌 내 기기 목록을 받지 않음");
                return Ok(OwnChange::Ignored);
            }
            let healed = cur.heal(&l.list, Timestamp::now());
            if !healed.well_formed(MAX_DEVICE_ENTRIES) {
                return Err(Error::conflict("기기 목록이 가득 찼습니다"));
            }
            tracing::warn!(
                devices = healed.devices.len() - cur.devices.len(),
                "폐기된 기기가 더한 기기를 폐기한 판을 냄"
            );
            adopt_locked(
                node,
                me,
                healed.sign_with(&me.identity.signing, &me.device.signing)?,
            )
        }
    }
}

/// 폐기된 기기의 인증된 연결을 잊습니다(다시 인증하면 거절됨).
fn forget_revoked(node: &Node, list: &DeviceList) {
    let revoked: Vec<DeviceId> = list
        .devices
        .iter()
        .filter(|d| d.revoked_at.is_some())
        .map(|d| d.device)
        .collect();
    node.peers.forget_devices(&revoked);
}

/// 없어진 OTP 기능이 남긴 비밀(내 계정 비밀, 예전에 친구가 맡긴 비밀)을 지웁니다.
pub(crate) fn forget_old_otp(store: &yangpa_store::Store) {
    for (key, _) in store.settings_with_prefix("otp.").unwrap_or_default() {
        let _ = store.delete_setting(&key);
    }
}

/// 기기 이름. 공간 상태에는 이름이 없으므로(간추린 기록) 저장된 그 사용자의 서명 기기 목록에서 같은 기기·같은 키
/// 약속의 항목 이름을 씁니다. 모르면 빈 문자열.
pub(crate) fn device_name(node: &Node, user: &UserId, rec: &DeviceRecord) -> String {
    node.store
        .get_device_list(user)
        .ok()
        .flatten()
        .and_then(|l| {
            l.list
                .devices
                .iter()
                .find(|e| e.device == rec.device && e.key_commit() == rec.key_commit)
                .map(|e| e.name.clone())
        })
        .unwrap_or_default()
}

// ───────────── 인증 거절·폐기 알림·지우기 ─────────────

/// 인증을 걸어 온 기기를 내가 아는 그 사용자의 목록에 비춘 결과.
pub(crate) enum AuthStatus {
    /// 받을 수 있는 기기(친구·멤버인지는 따로 봄).
    Active,
    /// 아는 목록에서 폐기된 기기. 그 목록.
    Revoked(SignedDeviceList),
    /// 아는 목록에 없고 활성 기기의 승인도 없는 기기.
    Unknown,
}

/// 인증을 걸어 온 기기의 상태. 내 기기면 내 목록, 남이면 저장된 그 사용자의 목록(처음 보면 제시된 목록)에 비춥니다.
pub(crate) fn auth_status(node: &Node, auth: &PeerAuth) -> AuthStatus {
    let Ok(me) = node.me() else {
        return AuthStatus::Unknown;
    };
    let known = if auth.user == me.user() {
        me.devices.clone()
    } else {
        match node.store.get_device_list(&auth.user).ok().flatten() {
            Some(k) => k,
            None => return AuthStatus::Active,
        }
    };
    if known.list.is_revoked(&auth.device) {
        return AuthStatus::Revoked(known);
    }
    if known.list.contains_active(&auth.device)
        || (auth.devices.list.extends(&known.list)
            && auth.devices.classify(&known.list) == ListChange::Authorized)
    {
        return AuthStatus::Active;
    }
    AuthStatus::Unknown
}

/// 믿지 않은 `Auth`에 대한 답: 폐기된 기기면 `Revoked`(지우게), 나머지는 `NotMember`. 목록은 그 사용자의 신원 키를 가진
/// 쪽(제시한 목록이 신원 키 서명)에게만 돌려줍니다.
pub(crate) fn refuse_auth(node: &Node, auth: &PeerAuth) -> YpResponse {
    if known_user(node, &auth.user)
        && let AuthStatus::Revoked(devices) = auth_status(node, auth)
    {
        tracing::info!(device = %auth.device.short(), "폐기된 기기의 인증 거절");
        return YpResponse::Revoked { devices };
    }
    YpResponse::error(WireErrorCode::NotMember, "친구·멤버가 아님")
}

/// 상대가 이 기기를 폐기된 기기로 알려 옴: 그 목록이 내 활성 기기가 만든 것이면(채택하며) 로컬 데이터를 지웁니다.
///
/// 내가 모르는 기기가 만든 목록이어도(이 기기가 꺼져 있는 동안 더해진 기기가 이 기기를 폐기함), 다른 사용자의 기기로
/// 알고 있던 노드가 알려 줬고 내 목록을 그대로 이어받으면 지웁니다. 그 사용자는 자기가 받아들인 목록만 돌려주므로,
/// 폐기된 기기가 지어낸 목록으로 살아 있는 기기를 지우려면 친구까지 속여야 합니다.
pub(crate) fn on_revoked(node: &Arc<Node>, peer: PeerId, devices: &SignedDeviceList) {
    let Ok(me) = node.me() else { return };
    if devices.list.user != me.user() || !devices.list.is_revoked(&me.device_id()) {
        return;
    }
    if vouched_revocation(node, &me, peer, devices) {
        tracing::warn!("다른 사용자가 알려 준, 이 기기를 폐기한 목록을 따름");
        spawn_wipe(node);
        return;
    }
    if let Err(e) = accept_device_list(node, devices) {
        tracing::debug!(error = %e, "폐기 알림의 기기 목록을 받지 않음");
    }
}

/// `on_revoked`의 다른 사용자 보증: 그 노드를 다른 사용자의 기기로 알고 있고, 목록이 서명·모양이 맞고 내 목록을 이어받으며,
/// 공동 서명 기기를 내 목록에서 모름(아는 기기면 보통 판정을 따름).
fn vouched_revocation(node: &Node, me: &Me, peer: PeerId, l: &SignedDeviceList) -> bool {
    let other_user = node
        .store
        .get_peer(&peer.to_bytes())
        .ok()
        .flatten()
        .and_then(|r| r.user)
        .is_some_and(|u| u != me.user());
    let cur = &me.devices.list;
    other_user
        && check_list_shape(l).is_ok()
        && l.verify().is_ok()
        && l.list.extends(cur)
        && l.cosigner()
            .is_some_and(|c| !cur.devices.iter().any(|d| d.device == c))
}

/// 이 기기가 폐기됐음을 확인했을 때: 로컬 데이터를 지웁니다(백그라운드, 적용 잠금을 기다림).
/// 지운 뒤 다시 뜰 때까지. Android는 프로세스째 다시 떠 화면이 지워지므로 안내를 읽을 틈을 둡니다.
const WIPE_RESTART_DELAY: Duration = Duration::from_secs(3);

fn spawn_wipe(node: &Arc<Node>) {
    node.spawn_detached(|node| async move {
        if let Err(e) = wipe(&node).await {
            tracing::warn!(error = %e, "폐기된 기기의 로컬 데이터 지우기 실패");
        }
    });
}

/// 신원·기기 키·네트워크 키·onion 키·공간·메시지·블롭·설정을 모두 지웁니다. 네트워크 설정(`net.*`: STUN 서버 목록 등)만
/// 남겨, 다시 시작해도 사용자가 고른 망 설정이 풀려 IP가 드러나지 않게 합니다. 지운 뒤 데몬이 다시 떠 새 PeerId와 새
/// onion 주소를 씁니다(폐기된 신원과 이어지지 않게).
pub(crate) async fn wipe(node: &Arc<Node>) -> Result<()> {
    let l = ingest::lock(node).await;
    {
        let _g = node.identity_lock.lock();
        if node.me.read().is_none() {
            return Ok(());
        }
        *node.me.write() = None;
    }
    let devices: Vec<DeviceId> = node
        .peers
        .authed_peers()
        .into_iter()
        .map(|(_, _, d)| d)
        .collect();
    node.peers.forget_devices(&devices);
    node.spaces.write().clear();
    node.sync.index.clear();
    node.devkeys.clear();
    node.keygroup.clear();
    crate::ratelimit::wipe_events(node);
    crate::volatile::wipe_all(node);
    crate::moderation::wipe_all(node);
    // 네트워크 설정은 지우기와 같은 트랜잭션 안에서 남깁니다(블롭 폴더를 지우다 실패해도 사라지지 않게).
    node.store.wipe_all(&[], &[NET_SETTINGS_PREFIX])?;
    // onion 키도 지워 다시 시작하면 새 주소를 씁니다(친구에게 준 주소로 지운 신원과 이어지지 않게).
    node.net.forget_onion_keys();
    drop(l);
    tracing::warn!("이 기기의 로컬 데이터를 지움");
    node.notify.publish(Notification::DeviceWiped);
    // 키를 잃은 onion 서비스는 새 연결을 받지 못해, 이대로 새 계정을 만들면 이미 연결된 노드만 닿습니다.
    crate::presence::request_restart(node, WIPE_RESTART_DELAY);
    Ok(())
}

/// 내 새 기기 목록을 알립니다: 내 기기가 상태에서 활성인 공간에는 직접 `DeviceListUpdate`, 인증된 피어(친구·멤버·
/// 내 기기)에게는 `Social(Devices)`(받은 쪽이 저장하고, 내가 아직 쓸 수 없는 공간에 대신 전함).
async fn announce(node: &Arc<Node>) {
    let Ok(me) = node.me() else { return };
    let list = me.devices.clone();
    for space in node.space_ids() {
        let _ = relay_list(node, space, &list).await;
    }
    notify_peers(node, &list);
}

/// 인증된 피어(나 말고)에게 `Social(Devices)`를 보냅니다(백그라운드).
fn notify_peers(node: &Arc<Node>, list: &SignedDeviceList) {
    let Ok(me) = node.me() else { return };
    for (peer, user, device) in node.peers.authed_peers() {
        if device == me.device_id() {
            continue;
        }
        let list = list.clone();
        node.spawn_detached(move |node| async move {
            send_devices(&node, peer, user, list).await;
        });
    }
}

/// 이 공간 상태의 그 사용자 기기 목록보다 `sdl`이 새것이고(이어받음) 그 목록에서 활성인 기기가 공동 서명했으면 내가 이
/// 공간에 쓸 수 있을 때 `DeviceListUpdate`를 냅니다. 확정 기기는 적용된 목록의 새 활성 기기를 다음 키 갱신에 넣습니다
/// (상자 키는 목록에 있음). 냈으면 `Ok(true)`, 낼 필요가 없으면 `Ok(false)`, 상태가 받지 않았으면 오류.
async fn relay_list(node: &Arc<Node>, space: SpaceId, sdl: &SignedDeviceList) -> Result<bool> {
    let me = node.me()?;
    let needed = |st: &SpaceState| {
        !st.disbanded
            && st.is_member(&me.user())
            && st.device_belongs(&me.user(), &me.device_id())
            && st.is_member(&sdl.list.user)
            && st.devices.get(&sdl.list.user).is_none_or(|cur| {
                sdl.list.extends(&**cur) && sdl.classify(&**cur) == ListChange::Authorized
            })
    };
    let Some(state) = node.space_state(&space) else {
        return Ok(false);
    };
    if !needed(&state) {
        return Ok(false);
    }
    let l = ingest::lock_space(node, space).await;
    let Some(state) = node.space_state(&space) else {
        return Ok(false);
    };
    if !needed(&state) {
        return Ok(false);
    }
    let body = EventBody::DeviceListUpdate {
        devices: sdl.clone(),
    };
    match author::publish_locked(node, &l, space, None, &body, KeyChoice::State, &[]) {
        Ok((ev, report)) => {
            drop(l);
            ingest::after_unlock(node, space, &report);
            sync_service::propagate(node, space, vec![ev]);
            Ok(true)
        }
        Err(e) => {
            tracing::debug!(space = %space.short(), error = %e, "기기 목록 전달 실패");
            Err(e)
        }
    }
}

/// 인증 직후 보내는 요청: 상대가 내 `Auth` 답을 아직 처리하지 못해 `Unauthenticated`로 답하면 잠시 뒤 다시 보냅니다.
pub(crate) async fn request_after_auth(
    node: &Node,
    peer: PeerId,
    req: YpRequest,
) -> Result<YpResponse> {
    let mut wait = Duration::from_millis(100);
    for attempt in 0..=AFTER_AUTH_RETRIES {
        let r = node.net.request(peer, req.clone()).await?;
        match r {
            YpResponse::Error {
                code: WireErrorCode::Unauthenticated,
                ..
            } if attempt < AFTER_AUTH_RETRIES && node.peers.is_connected(&peer) => {
                tokio::time::sleep(wait).await;
                wait *= 2;
            }
            r => return Ok(r),
        }
    }
    Err(Error::Network("상대가 인증을 끝내지 않음".into()))
}

/// `Social(Devices)`를 보내고 답을 반영합니다.
async fn send_devices(node: &Arc<Node>, peer: PeerId, peer_user: UserId, list: SignedDeviceList) {
    // 공간·연락처는 내 기기에게만(친구·멤버에게도 같은 요청으로 기기 목록을 알림).
    let own = node.me().is_ok_and(|me| me.user() == peer_user);
    let (spaces, contacts, cut) = match node.me() {
        Ok(me) if own => (
            own_spaces(node, &me),
            own_contacts(node),
            friends::cut_spaces(node),
        ),
        _ => Default::default(),
    };
    let req = YpRequest::Social(SocialRequest::Devices {
        devices: Box::new(list),
        spaces,
        contacts,
        cut,
        mobile: own && MOBILE,
    });
    match request_after_auth(node, peer, req).await {
        Ok(YpResponse::Social(reply)) => on_devices_reply(node, peer, peer_user, reply).await,
        Ok(YpResponse::Error { code, .. }) => {
            tracing::debug!(peer = %peer, ?code, "기기 목록 알림 거절")
        }
        Ok(_) => tracing::debug!(peer = %peer, "기기 목록 알림에 다른 응답"),
        Err(e) => tracing::debug!(peer = %peer, error = %e, "기기 목록 알림 실패"),
    }
}

/// `Social(Devices)`의 답. 상대가 아는 내 기기 목록(신원 키 서명이라 누가 전해도 됨)을 먼저 반영하고, 내 기기의 답이면
/// 프로필과 내가 멤버인 공간을 받아들입니다.
async fn on_devices_reply(node: &Arc<Node>, peer: PeerId, peer_user: UserId, reply: SocialReply) {
    let Ok(me) = node.me() else { return };
    if let Some(l) = &reply.devices
        && l.list.user == me.user()
        && let Err(e) = accept_device_list(node, l)
    {
        tracing::debug!(error = %e, "받은 내 기기 목록 반영 실패");
    }
    if peer_user != me.user() {
        if let Some(p) = &reply.profile
            && p.user == peer_user
            && known_user(node, &peer_user)
        {
            let _ = accept_profile(node, p);
        }
        return;
    }
    if let Some(p) = &reply.profile
        && p.user == me.user()
    {
        let _ = accept_profile(node, p);
    }
    note_mobile(node, peer, reply.mobile);
    take_own_state(node, peer, reply.cut, reply.spaces, reply.contacts).await;
    node.identity.own_spaces_known.store(true, Ordering::SeqCst);
}

/// 내 다른 기기가 알려 준 공간과 연락처 기록을 받습니다(`Social(Devices)`의 요청·답).
async fn take_own_state(
    node: &Arc<Node>,
    peer: PeerId,
    cut: Vec<SpaceId>,
    spaces: Vec<(SpaceId, EventId)>,
    contacts: Vec<OwnContact>,
) {
    // 먼저 지웁니다: 함께 온 공간·연락처에 끊기 전 소식이 섞여 있어도 되살아나지 않게.
    for space in cut.into_iter().take(MAX_REPLY_SPACES) {
        friends::take_cut(node, space).await;
    }
    let mut contacts: Vec<OwnContact> = contacts
        .into_iter()
        .take(MAX_REPLY_SPACES)
        .filter(|c| !friends::is_cut(node, &c.space))
        .collect();
    for c in &mut contacts {
        if let Some(p) = c.profile.take().filter(|p| p.user == c.user) {
            let _ = accept_profile(node, &p);
        }
    }
    let by_space: HashMap<SpaceId, &OwnContact> = contacts.iter().map(|c| (c.space, c)).collect();
    let mut adopted = Vec::new();
    for (space, genesis) in spaces.into_iter().take(MAX_REPLY_SPACES) {
        // 받아들임 기록과 연락처 기록을 함께 씁니다: 그 사이에 상태가 서면 기록 없이 지나갑니다.
        let contact = by_space.get(&space).map(|c| (*c).clone());
        match adopt_space(node, space, genesis, contact).await {
            Ok(true) => adopted.push(space),
            Ok(false) => {}
            Err(e) => tracing::debug!(error = %e, "공간 받아들이기 실패"),
        }
    }
    for c in contacts {
        take_contact(node, c).await;
    }
    for space in adopted {
        spawn_adoption_catch_up(node, space, peer);
    }
}

/// 내 기기가 알려 준 연락처 기록 하나. 공간이 아직 서지 않았으면 설 때 쓰도록 받아들임 기록에 둡니다(서기 전에 기록을 만들면
/// `friend.open`이 그 공간을 쓸 수 없음).
async fn take_contact(node: &Arc<Node>, c: OwnContact) {
    if adoption(node, &c.space).is_some() {
        let _l = ingest::lock_space(node, c.space).await;
        if node.space_state(&c.space).is_none()
            && let Some(a) = adoption(node, &c.space)
        {
            let key = adopt_key(&c.space);
            let _ = node.store.put_setting(
                &key,
                &Adoption {
                    contact: Some(c),
                    ..a
                },
            );
            return;
        }
    }
    friends::merge_own_contact(node, &c);
}

/// 연락처 기록이나 내가 멤버인 공간이 바뀌면 인증된 내 다른 기기에 바로 알립니다. 꺼져 있던 기기는 다음 인증 때 받습니다.
/// 받은 쪽이 바뀌어 다시 알려도 이쪽이 바뀌지 않으면 거기서 그칩니다.
pub(crate) fn spawn_own_share(node: &Arc<Node>) {
    use tokio::sync::broadcast::error::RecvError;
    let weak = Arc::downgrade(node);
    let mut rx = node.notify.subscribe();
    let mut mine: HashSet<SpaceId> = node.space_ids().into_iter().collect();
    node.tasks.spawn(async move {
        loop {
            match rx.recv().await {
                Ok(
                    Notification::FriendAccepted { .. }
                    | Notification::FriendUpdated { .. }
                    | Notification::FriendRequest { .. },
                )
                | Err(RecvError::Lagged(_)) => {}
                Ok(Notification::SpaceUpdated { space }) if !mine.contains(&space) => {
                    let Some(node) = weak.upgrade() else { break };
                    let member = node.me().is_ok_and(|me| {
                        node.space_state(&space)
                            .is_some_and(|st| st.is_member(&me.user()))
                    });
                    if !member {
                        continue;
                    }
                    mine.insert(space);
                }
                Ok(_) => continue,
                Err(RecvError::Closed) => break,
            }
            tokio::time::sleep(OWN_SHARE_DELAY).await;
            rx = rx.resubscribe();
            let Some(node) = weak.upgrade() else { break };
            let Ok(me) = node.me() else { continue };
            for (peer, user, device) in node.peers.authed_peers() {
                if user == me.user() && device != me.device_id() {
                    let list = me.devices.clone();
                    node.spawn_detached(move |node| async move {
                        send_devices(&node, peer, user, list).await;
                    });
                }
            }
        }
    });
}

/// 내 기기가 알려 준 공간의 런타임을 만듭니다(그 시작 이벤트로 고정). 이미 아는 공간이면 false.
async fn adopt_space(
    node: &Arc<Node>,
    space: SpaceId,
    genesis: EventId,
    contact: Option<OwnContact>,
) -> Result<bool> {
    // 폐기된 기기 지우기(`wipe`)와 겹치지 않게 적용 잠금 안에서 신원을 확인하고 씁니다(지운 뒤에 공간이 다시 생기지 않게).
    // 그 공간 잠금이면 됩니다: `wipe`는 모든 공간 잠금(관문 쓰기)이라 어느 공간 잠금과도 겹치지 않음.
    let l = ingest::lock_space(node, space).await;
    node.me()?;
    if friends::is_cut(node, &space)
        || node.spaces.read().contains_key(&space)
        || spaces::load_genesis(node, &space)?.is_some()
    {
        return Ok(false);
    }
    node.store.put_setting(
        &adopt_key(&space),
        &Adoption {
            genesis,
            at: Timestamp::now(),
            contact,
        },
    )?;
    spaces::ensure_runtime(node, &space, Some(genesis))?;
    drop(l);
    tracing::info!(space = %space.short(), "공간 받아들임");
    Ok(true)
}

/// 받아들인 공간의 상태와 크루 키가 생길 때까지 따라잡기를 몇 번 더 돌립니다(기기 목록 전달·키 갱신을 기다림). 그 뒤는
/// 주기 동기화(`sync_service::catch_up_peer`가 받아들이는 중인 공간도 봄)와 [`adoption_tick`]이 맡습니다.
fn spawn_adoption_catch_up(node: &Arc<Node>, space: SpaceId, peer: PeerId) {
    node.spawn_detached(move |node| async move {
        let mut wait = Duration::from_millis(250);
        for _ in 0..ADOPT_RETRIES {
            let ready = space_ready(&node, &space);
            // 상태가 선 뒤에도 한 번 더 돕니다: 채널 스트림은 상태가 있어야 맞춥니다.
            sync_service::spawn_catch_up(&node, space, peer);
            if ready {
                break;
            }
            tokio::time::sleep(wait).await;
            wait = (wait * 2).min(ADOPT_RETRY_MAX);
        }
    });
}

fn space_ready(node: &Node, space: &SpaceId) -> bool {
    node.space_state(space).is_some() && crate::keygroup::has_crew_key(node, space)
}

/// 받아들인 공간이 모두 상태와 크루 키를 얻었는지.
fn adoptions_ready(node: &Node) -> bool {
    node.store
        .settings_with_prefix(ADOPT_PREFIX)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(key, _)| key.strip_prefix(ADOPT_PREFIX)?.parse::<SpaceId>().ok())
        .all(|space| space_ready(node, &space))
}

/// 내 기기로 연결된 뒤 첫 동기화 중인지(`identity.get`의 `first_syncing`).
pub(crate) fn first_syncing(node: &Node) -> bool {
    node.identity.first_sync.load(Ordering::SeqCst)
}

/// 연결된 기기의 첫 동기화가 끝나기를 기다려 `device.synced`를 알립니다. 끝은 [`FIRST_SYNC_QUIET`], 받을 곳이 사라졌을
/// 때는 [`FIRST_SYNC_STALL`]로 정합니다. 따라잡기가 도는 동안은 기록이 아무리 많아도 기다립니다.
pub(crate) fn spawn_first_sync(node: &Arc<Node>) {
    node.spawn_detached(|node| async move {
        let mut quiet = Instant::now();
        let mut idle = Instant::now();
        while quiet.elapsed() < FIRST_SYNC_QUIET && idle.elapsed() < FIRST_SYNC_STALL {
            tokio::time::sleep(FIRST_SYNC_POLL).await;
            if !sync_service::catch_up_idle(&node) {
                idle = Instant::now();
                quiet = idle;
            } else if !node.identity.own_spaces_known.load(Ordering::SeqCst)
                || !adoptions_ready(&node)
            {
                quiet = Instant::now();
            }
        }
        node.identity.first_sync.store(false, Ordering::SeqCst);
        tracing::info!("연결된 기기의 첫 동기화 끝");
        node.notify.publish(Notification::DeviceSynced);
    });
}

fn adoption(node: &Node, space: &SpaceId) -> Option<Adoption> {
    node.store.get_setting(&adopt_key(space)).ok().flatten()
}

/// 지운 공간의 받아들임 기록을 지웁니다.
pub(crate) fn forget_adoption(node: &Node, space: &SpaceId) {
    let _ = node.store.delete_setting(&adopt_key(space));
}

/// 받아들였지만 아직 상태가 없는 공간을 `user`의 기기에게서 받을지(내 다른 기기).
pub(crate) fn is_adopting(node: &Node, space: &SpaceId, user: &UserId) -> bool {
    node.space_state(space).is_none() && adopting_from(node, space, user)
}

/// 받아들인 공간의 기록이 남아 있고 `user`가 나인지. 그동안 내 기기가 준 받은편지함 키 묶음은 상태가 없을 때처럼 믿습니다(`ingest`).
pub(crate) fn adopting_from(node: &Node, space: &SpaceId, user: &UserId) -> bool {
    adoption(node, space).is_some() && node.me().is_ok_and(|m| m.user() == *user)
}

/// 주기 작업(`sync_service::spawn_periodic`): 오래 해소되지 않은 받아들인 공간을 포기합니다.
pub(crate) fn adoption_tick(node: &Arc<Node>) {
    let now = Timestamp::now();
    let mut stale = Vec::new();
    let mut settled = Vec::new();
    for (key, v) in node
        .store
        .settings_with_prefix(ADOPT_PREFIX)
        .unwrap_or_default()
    {
        let Some(space) = key
            .strip_prefix(ADOPT_PREFIX)
            .and_then(|s| s.parse::<SpaceId>().ok())
        else {
            let _ = node.store.delete_setting(&key);
            continue;
        };
        if node.space_state(&space).is_some() {
            settled.push(key);
            continue;
        }
        let expired = serde_json::from_value::<Adoption>(v)
            .map(|a| a.at.0.saturating_add(ADOPT_EXPIRY_MS) < now.0)
            .unwrap_or(true);
        if expired {
            stale.push((key, space));
        }
    }
    if !stale.is_empty() || !settled.is_empty() {
        node.spawn_detached(move |node| async move {
            // 상태가 선 공간의 기록은 적용 잠금 안에서 지웁니다: 상태를 세우는 적용이 [`on_state_applied`]로 기록을 보기
            // 전에 지우지 않게.
            let l = ingest::lock(&node).await;
            for key in settled {
                let _ = node.store.delete_setting(&key);
            }
            for (key, space) in stale {
                if node.space_state(&space).is_none() {
                    let _ = spaces::discard_space_locked(&node, &l, &space);
                }
                let _ = node.store.delete_setting(&key);
                tracing::info!(space = %space.short(), "해소되지 않은 공간을 포기");
            }
        });
    }
}

/// 아는 사용자인지(내 다른 기기, 친구, 같은 공간 멤버).
fn known_user(node: &Node, user: &UserId) -> bool {
    node.me().is_ok_and(|m| m.user() == *user)
        || node.store.get_friend(user).ok().flatten().is_some()
        || node
            .space_ids()
            .iter()
            .any(|s| node.space_state(s).is_some_and(|st| st.is_member(user)))
}

/// 더는 아는 사람이 아니면 그 사람의 프로필·기기 목록·메모를 지웁니다. 차단 목록에 보일 사람은 둡니다.
pub(crate) fn forget_stranger(node: &Arc<Node>, user: &UserId) {
    if known_user(node, user) || crate::moderation::is_blocked(node, user) {
        return;
    }
    let _ = node.store.delete_profile(user);
    let _ = node.store.delete_device_list(user);
    if notes::get(node, user).is_ok_and(|n| !n.is_empty()) {
        let _ = notes::set(node, user, "");
    }
}

/// 인증을 마친 직후(연결을 건 쪽): 프로필·링크용 주소를 주고받고, 내 다른 기기면 기기 목록·메모를 맞추고, 전하지 못한
/// 친구 삭제를 전합니다.
pub(crate) fn on_authenticated(node: &Arc<Node>, peer: PeerId, user: UserId, device: DeviceId) {
    crate::supporter::share_to(node, peer, user);
    crate::badges::on_authed(node, peer, user, device);
    crate::cards::on_peer(node);
    node.spawn_detached(move |node| async move {
        exchange_profile(&node, peer, user).await;
        // 한 번 왕복으로 서로의 지금 링크용 주소를 남깁니다(Tor 스웜은 identify가 없어 들어온 연결의 주소를 모름).
        peers::exchange_contact(&node, peer).await;
        if let Ok(me) = node.me()
            && me.user() == user
        {
            send_devices(&node, peer, user, me.devices.clone()).await;
            notes::sync_with(&node, peer, device).await;
        }
        friends::deliver_leaves(&node, peer, &user).await;
    });
}

/// 상대가 건 인증을 받아 준 직후(받은 쪽): 전하지 못한 친구 삭제를 전합니다. 프로필 교환은 건 쪽이 합니다.
pub(crate) fn on_auth_accepted(node: &Arc<Node>, peer: PeerId, user: UserId) {
    crate::supporter::share_to(node, peer, user);
    if let Some((_, device)) = node.peers.identity_of(&peer) {
        crate::badges::on_authed(node, peer, user, device);
    }
    crate::cards::on_peer(node);
    node.spawn_detached(move |node| async move {
        friends::deliver_leaves(&node, peer, &user).await;
    });
}

/// 내 프로필을 보내고 상대 프로필을 받습니다.
async fn exchange_profile(node: &Arc<Node>, peer: PeerId, user: UserId) {
    let Ok(me) = node.me() else { return };
    let req = YpRequest::Social(SocialRequest::Profile(Box::new(me.profile.clone())));
    match node.net.request(peer, req).await {
        Ok(YpResponse::Social(r)) => {
            if let Some(p) = r.profile
                && p.user == user
            {
                let _ = accept_profile(node, &p);
            }
        }
        Ok(_) => tracing::debug!(peer = %peer, "프로필 교환에 다른 응답"),
        Err(e) => tracing::debug!(peer = %peer, error = %e, "프로필 교환 실패"),
    }
}

/// 바뀐 내 프로필을 퍼뜨립니다: 지금 인증된 피어(친구·멤버·내 기기)에게 바로 보내고, 셋 이상인 공간에는 상태 이벤트로
/// 실어 직접 연결되지 않는 멤버도 동기화로 받게 합니다(`cards::spread`).
pub(crate) fn push_profile(node: &Arc<Node>) {
    for (peer, user, device) in node.peers.authed_peers() {
        if crate::archive::archive_only(node, &user, &device) {
            continue;
        }
        node.spawn_detached(move |node| async move {
            exchange_profile(&node, peer, user).await;
        });
    }
    node.spawn_detached(|node| async move {
        let spaces = node.space_ids();
        crate::cards::spread(&node, spaces).await;
    });
}

/// 받은 `Social` 요청(인증된 피어만).
pub(crate) async fn handle_social(
    node: &Arc<Node>,
    peer: PeerId,
    req: SocialRequest,
) -> YpResponse {
    let Some((user, device)) = node.peers.identity_of(&peer) else {
        return YpResponse::error(WireErrorCode::Unauthenticated, "인증 전");
    };
    if node.me().is_err() {
        return YpResponse::error(WireErrorCode::Internal, "신원 없음");
    }
    match req {
        SocialRequest::Profile(p) => {
            if p.user != user {
                return YpResponse::error(WireErrorCode::BadRequest, "다른 사용자의 프로필");
            }
            if accept_profile(node, &p).is_err() {
                return YpResponse::error(WireErrorCode::BadRequest, "프로필 서명이 올바르지 않음");
            }
            YpResponse::Social(SocialReply {
                profile: node.me().ok().map(|m| m.profile.clone()),
                ..Default::default()
            })
        }
        SocialRequest::Devices {
            devices,
            spaces,
            contacts,
            cut,
            mobile,
        } => handle_devices(node, peer, user, *devices, spaces, contacts, cut, mobile),
        SocialRequest::Cut { space } => friends::handle_cut(node, user, space).await,
        SocialRequest::Notes { sealed } => notes::handle_notes(node, peer, user, device, &sealed),
        SocialRequest::Propose => friends::handle_propose(node, peer, user),
        SocialRequest::ContactExpired => friends::handle_contact_expired(node, peer, user),
        SocialRequest::DmSpace { space, events } => {
            friends::handle_dm_space(node, peer, user, space, events).await
        }
        SocialRequest::Contact { addrs, nodes } => {
            // 보관 관계로만 믿는 상대와는 주소를 주고받지 않습니다(프로필 교환과 같음).
            if crate::archive::archive_only(node, &user, &device) {
                return YpResponse::error(WireErrorCode::Forbidden, "보관 노드");
            }
            peers::record_contact(node, &peer, &addrs);
            // 노드 카드는 친구·내 기기와만 주고받습니다.
            let close = peers::close_contact(node, &user);
            if close {
                peers::record_node_contacts(node, &peer, user, device, &nodes);
            }
            YpResponse::Social(SocialReply {
                addrs: peers::my_contact_addrs(node),
                nodes: if close {
                    crate::tor::node_contacts(node).await
                } else {
                    Vec::new()
                },
                ..Default::default()
            })
        }
    }
}

/// 받은 `Social(Devices)`: 저장(내 목록이면 채택)하고, 남의 새 목록이면 함께 있는 공간에 대신 전합니다. 내 기기끼리는 실린
/// 공간·연락처를 받고 답에 내 것을 싣습니다.
#[allow(clippy::too_many_arguments)]
fn handle_devices(
    node: &Arc<Node>,
    peer: PeerId,
    user: UserId,
    sdl: SignedDeviceList,
    spaces: Vec<(SpaceId, EventId)>,
    contacts: Vec<OwnContact>,
    cut: Vec<SpaceId>,
    mobile: bool,
) -> YpResponse {
    if sdl.list.user != user || !sdl.list.well_formed(MAX_DEVICE_ENTRIES) || sdl.verify().is_err() {
        return YpResponse::error(WireErrorCode::BadRequest, "기기 목록이 올바르지 않음");
    }
    if let Err(e) = accept_device_list(node, &sdl) {
        tracing::debug!(error = %e, "받은 기기 목록 반영 실패");
    }
    let Ok(me) = node.me() else {
        return YpResponse::error(WireErrorCode::Internal, "신원 없음");
    };
    let own = user == me.user();
    if own {
        note_mobile(node, peer, mobile);
        node.spawn_detached(move |node| async move {
            take_own_state(&node, peer, cut, spaces, contacts).await;
        });
    }
    let newest = if own {
        Some(me.devices.clone())
    } else {
        node.store.get_device_list(&user).ok().flatten()
    };
    // 남의 새 목록은 함께 있는 공간에 대신 전합니다(내 목록은 채택할 때 알림).
    if !own && let Some(n) = newest.clone() {
        let spaces: Vec<SpaceId> = node
            .space_ids()
            .into_iter()
            .filter(|s| {
                node.space_state(s)
                    .is_some_and(|st| st.is_member(&me.user()) && st.is_member(&user))
            })
            .collect();
        node.spawn_detached(move |node| async move {
            for s in spaces {
                let _ = relay_list(&node, s, &n).await;
            }
        });
    }
    YpResponse::Social(SocialReply {
        devices: newest,
        spaces: if own {
            own_spaces(node, &me)
        } else {
            Vec::new()
        },
        contacts: if own { own_contacts(node) } else { Vec::new() },
        cut: if own {
            friends::cut_spaces(node)
        } else {
            Vec::new()
        },
        mobile: own && MOBILE,
        profile: Some(me.profile.clone()),
        ..Default::default()
    })
}

/// 내 다른 기기가 알린 휴대폰 여부를 남깁니다.
fn note_mobile(node: &Node, peer: PeerId, mobile: bool) {
    let device = node.peers.authed_peers().into_iter().find(|p| p.0 == peer);
    if mobile && let Some((_, _, device)) = device {
        let _ = node
            .store
            .put_setting(&format!("{MOBILE_PREFIX}{device}"), &true);
    }
}

/// 내 기기가 휴대폰인지(모르면 아닌 것으로).
pub(crate) fn is_mobile(node: &Node, me: &Me, device: &DeviceId) -> bool {
    if *device == me.device_id() {
        return MOBILE;
    }
    matches!(
        node.store
            .get_setting::<bool>(&format!("{MOBILE_PREFIX}{device}")),
        Ok(Some(true))
    )
}

/// 내가 멤버인 모든 공간과 시작 이벤트(내 다른 기기에게). 비활성화된 1:1 공간은 뺍니다: 만료돼 읽기만 되는 대화를 새 기기에
/// 늘리지 않습니다.
fn own_spaces(node: &Node, me: &Me) -> Vec<(SpaceId, EventId)> {
    let all: Vec<(SpaceId, EventId)> = {
        let spaces = node.spaces.read();
        spaces
            .iter()
            .filter_map(|(id, rt)| {
                let st = rt.state.as_ref()?;
                let g = rt.genesis?;
                st.is_member(&me.user()).then_some((*id, g))
            })
            .collect()
    };
    all.into_iter()
        .filter(|(s, _)| !friends::space_deactivated(node, s))
        .take(MAX_REPLY_SPACES)
        .collect()
}

/// 이 기기의 연락처 기록(내 다른 기기에게).
fn own_contacts(node: &Node) -> Vec<OwnContact> {
    node.store
        .list_friends()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|f| {
            Some(OwnContact {
                space: f.space?,
                kind: f.kind,
                expires_at: f.expires_at,
                link_hash: f.link_hash,
                profile: node.store.get_profile(&f.user).ok().flatten(),
                request: friends::own_request(node, &f.user),
                user: f.user,
            })
        })
        .take(MAX_REPLY_SPACES)
        .collect()
}

/// 이 친구와의 1:1 공간(친구 기록의 공간이 아직 둘만의 공간일 때).
pub(crate) fn friend_space(node: &Node, me: &Me, user: &UserId) -> Option<(SpaceId, EventId)> {
    let space = node.store.get_friend(user).ok().flatten()?.space?;
    let spaces = node.spaces.read();
    let rt = spaces.get(&space)?;
    let st = rt.state.as_ref()?;
    (st.kind == SpaceKind::Space && st.is_member(&me.user()) && st.is_member(user))
        .then_some((space, rt.genesis?))
}

/// 사람이 승인한 새 기기를 내 목록에 더한 판을 내 공동 서명으로 채택하고 뒤에서 알립니다. 활성 기기가 만든 판이라
/// 친구·멤버·공간이 그대로 받고, 확정 기기가 다음 키 갱신에 그 기기를 넣습니다.
pub(crate) fn add_device(node: &Arc<Node>, entry: DeviceEntry) -> Result<SignedDeviceList> {
    let next = {
        let _g = node.identity_lock.lock();
        let me = node.me()?;
        if me.devices.list.is_revoked(&me.device_id()) {
            return Err(Error::forbidden("이 기기는 이미 폐기됐습니다"));
        }
        if me
            .devices
            .list
            .devices
            .iter()
            .any(|d| d.device == entry.device)
        {
            return Err(Error::conflict("이미 내 기기 목록에 있는 기기입니다"));
        }
        adopt_edit(node, &me, me.devices.list.with_entry(entry))?
    };
    let list = next.clone();
    node.spawn_detached(move |node| async move { publish_own_list(&node, &list).await });
    Ok(next)
}

/// 기기 폐기: 내 목록에 폐기 표시를 남긴 새 판을 내 공동 서명으로 채택하고, 내가 쓸 수 있는 모든 공간에
/// `DeviceListUpdate`를 냅니다. 확정 기기들이 그 기기를 키 그룹에서 빼는 회전(`KeyUpdate{Rotate}`)을 바로 내며, 피어들은 그 기기의
/// 인증을 거절하고 폐기를 알려 지우게 합니다.
pub(crate) async fn revoke_device(node: &Arc<Node>, device: DeviceId) -> Result<()> {
    let next = {
        let _g = node.identity_lock.lock();
        let me = node.me()?;
        if device == me.device_id() {
            return Err(Error::invalid(
                "지금 쓰는 기기는 폐기할 수 없습니다(다른 기기에서 폐기하세요)",
            ));
        }
        if me.devices.list.is_revoked(&me.device_id()) {
            return Err(Error::forbidden("이 기기는 이미 폐기됐습니다"));
        }
        let entry = me
            .devices
            .list
            .devices
            .iter()
            .find(|d| d.device == device)
            .cloned()
            .ok_or_else(|| Error::not_found("내 기기가 아닙니다"))?;
        if entry.revoked_at.is_some() {
            return Err(Error::conflict("이미 폐기된 기기입니다"));
        }
        // 폐기 기록은 되살아남을 막으려 남기고(서명 목록은 늘기만 함) 이름은 지웁니다.
        adopt_edit(
            node,
            &me,
            me.devices.list.with_entry(DeviceEntry {
                name: String::new(),
                revoked_at: Some(Timestamp::now()),
                ..entry
            }),
        )?
    };
    tracing::info!(device = %device.short(), "기기 폐기");
    forget_revoked(node, &next.list);
    publish_own_list(node, &next).await;
    Ok(())
}

/// 내 기기 이름을 바꿉니다. 이름도 서명 목록에 있어 새 판으로 알립니다.
pub(crate) async fn rename_device(node: &Arc<Node>, device: DeviceId, name: &str) -> Result<()> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > crate::devlink::MAX_NAME {
        return Err(Error::invalid("기기 이름이 비었거나 너무 깁니다"));
    }
    let next = {
        let _g = node.identity_lock.lock();
        let me = node.me()?;
        if me.devices.list.is_revoked(&me.device_id()) {
            return Err(Error::forbidden("이 기기는 이미 폐기됐습니다"));
        }
        let entry = me
            .devices
            .list
            .devices
            .iter()
            .find(|d| d.device == device)
            .cloned()
            .ok_or_else(|| Error::not_found("내 기기가 아닙니다"))?;
        if entry.revoked_at.is_some() {
            return Err(Error::conflict("이미 폐기된 기기입니다"));
        }
        if entry.name == name {
            return Ok(());
        }
        adopt_edit(
            node,
            &me,
            me.devices.list.with_entry(DeviceEntry {
                name: name.to_owned(),
                ..entry
            }),
        )?
    };
    publish_own_list(node, &next).await;
    Ok(())
}

/// 시작할 때 내 목록의 예전 이름을 고칩니다: 이 기기의 "이 기기"는 기본 이름으로, 폐기된 기기의 이름은 지움.
pub(crate) fn spawn_tidy_own_list(node: &Arc<Node>) {
    node.spawn_detached(move |node| async move {
        let next = {
            let _g = node.identity_lock.lock();
            let Ok(me) = node.me() else { return };
            let mine = me.device_id();
            let mut list = me.devices.list.clone();
            let mut changed = false;
            for d in &mut list.devices {
                let name = if d.revoked_at.is_some() {
                    ""
                } else if d.device == mine && d.name == OLD_DEFAULT_NAME {
                    &crate::config::default_device_name()
                } else {
                    continue;
                };
                changed |= d.name != name;
                d.name = name.to_owned();
            }
            if !changed || list.is_revoked(&mine) {
                return;
            }
            list.version = list.version.saturating_add(1);
            match adopt_edit(&node, &me, list) {
                Ok(next) => next,
                Err(e) => {
                    tracing::debug!(error = %e, "내 기기 목록 이름 고치기 실패");
                    return;
                }
            }
        };
        publish_own_list(&node, &next).await;
    });
}

/// 고친 내 목록을 내 공동 서명으로 채택합니다(`identity_lock` 안에서).
fn adopt_edit(node: &Node, me: &Me, list: DeviceList) -> Result<SignedDeviceList> {
    if !list.well_formed(MAX_DEVICE_ENTRIES) {
        return Err(Error::conflict("기기 목록이 가득 찼습니다"));
    }
    let signed = list.sign_with(&me.identity.signing, &me.device.signing)?;
    if !node.store.put_device_list(&signed)? {
        return Err(Error::conflict("기기 목록이 바뀌었습니다(다시 시도하세요)"));
    }
    set_me(node, me, Some(signed.clone()), None);
    Ok(signed)
}

/// 바꾼 내 목록을 내 기기 직접 연결에 맞추고 공간·피어에 알립니다. 공간 전달(로컬 적용)은 기다리고, 피어 알림은
/// 백그라운드로. 받지 않은 공간이 있으면 경고로 남깁니다(그 공간의 키 그룹에는 아직 반영되지 않음).
async fn publish_own_list(node: &Arc<Node>, next: &SignedDeviceList) {
    crate::p2p::reconcile(node);
    let mut failed = 0usize;
    for space in node.space_ids() {
        if relay_list(node, space, next).await.is_err() {
            failed += 1;
        }
    }
    if failed > 0 {
        tracing::warn!(spaces = failed, "내 기기 목록을 받지 않은 공간이 있음");
    }
    notify_peers(node, next);
}

/// 상태 이벤트 본문에 실린 서명 기기 목록·프로필을 반영합니다(적용 잠금 안, 해소기 판정 전). 서명이 맞는 목록은
/// 이벤트가 거부돼도 그 사용자가 서명한 것이므로 이어받으면 저장하고, 내 목록이면 채택합니다.
pub(crate) fn observe_state_body(node: &Arc<Node>, body: &EventBody) {
    match body {
        EventBody::DeviceListUpdate { devices, .. } => {
            let _ = accept_device_list(node, devices);
        }
        EventBody::MemberJoin {
            devices, profile, ..
        } => {
            let _ = accept_device_list(node, devices);
            if let Some(p) = profile {
                let _ = accept_profile(node, p);
            }
        }
        EventBody::MemberProfile {
            profile: Some(p), ..
        } => {
            let _ = accept_profile(node, p);
        }
        // 내 카드면 기준을 다시 보느라 공간 상태를 읽으므로 적용 잠금 밖에서.
        EventBody::MemberBadges { card } => {
            let card = card.clone();
            node.spawn_detached(move |node| async move {
                let _ = crate::badges::accept_card(&node, &card);
            });
        }
        EventBody::SpaceCreate { devices, .. } => {
            for d in devices {
                let _ = accept_device_list(node, d);
            }
        }
        _ => {}
    }
}

/// 공간 상태가 바뀐 뒤(적용 잠금 안). 받아들인 공간이 처음 해소되면 표시를 지우고, 1:1 공간이면 친구 기록을 되살립니다.
/// 친구 공간에서 누가 나가면 친구 기록을 지웁니다(`friends::on_state_applied`).
pub(crate) fn on_state_applied(
    node: &Arc<Node>,
    space: &SpaceId,
    old: Option<&SpaceState>,
    new: &SpaceState,
) {
    if old.is_none()
        && let Some(a) = adoption(node, space)
    {
        let _ = node.store.delete_setting(&adopt_key(space));
        restore_friend(node, space, new, a.contact.as_ref());
    }
    friends::on_state_applied(node, space, old, new);
}

pub(crate) fn is_friend_space_with(me: &Me, st: &SpaceState, user: &UserId) -> bool {
    st.kind == SpaceKind::Space
        && st.members.len() == 2
        && st.is_member(&me.user())
        && st.is_member(user)
}

/// 내 기기가 알려 준 공간의 상태가 섬: 함께 받은 연락처 기록을 맞춥니다(기록 없는 공간에 연락처를 지어내지 않음. 내 기기도
/// 연락처로 두지 않은 1:1 공간일 수 있음). 1:1을 크루로 바꾼 뒤 내 다른 기기가 새로 만들었거나 받은 1:1 공간이면 기록을
/// 그쪽으로 옮깁니다(`friends::follow_dm`).
fn restore_friend(
    node: &Arc<Node>,
    space: &SpaceId,
    st: &SpaceState,
    contact: Option<&OwnContact>,
) {
    let Ok(me) = node.me() else { return };
    if let Some(c) = contact {
        friends::merge_own_contact(node, c);
    }
    let Some(other) = st.members.keys().find(|u| **u != me.user()).copied() else {
        return;
    };
    if !is_friend_space_with(&me, st, &other) {
        return;
    }
    if friends::follow_dm(node, &other, *space) {
        node.notify
            .publish(Notification::FriendUpdated { user: other });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_key_storage_never_replaces_key_of_existing_db() {
        let k = || Secret32([3; 32]);
        // 파일로 만든 설치는 파일.
        assert_eq!(
            auto_choice(true, true, KeyringProbe::Unavailable),
            AutoChoice::File
        );
        assert_eq!(
            auto_choice(false, true, KeyringProbe::Found(k())),
            AutoChoice::Keyring(k())
        );
        // DB가 있는데 키 저장소가 잠겼거나 항목이 없으면 새 키를 만들지 않고 멈춥니다.
        assert!(matches!(
            auto_choice(false, true, KeyringProbe::Unavailable),
            AutoChoice::Fail(_)
        ));
        assert!(matches!(
            auto_choice(false, true, KeyringProbe::Missing),
            AutoChoice::Fail(_)
        ));
        // 처음 설치.
        assert_eq!(
            auto_choice(false, false, KeyringProbe::Missing),
            AutoChoice::CreateKeyring
        );
        assert_eq!(
            auto_choice(false, false, KeyringProbe::Unavailable),
            AutoChoice::CreateFile
        );
    }

    #[test]
    fn file_storage_refuses_to_invent_key_for_existing_db() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = NodeConfig::for_test(dir.path());
        cfg.key_storage = KeyStorage::File;
        std::fs::write(dir.path().join(yangpa_store::DB_FILE_NAME), b"x").unwrap();
        assert!(load_or_create_db_key(&cfg).is_err());
        assert!(!dir.path().join(DB_KEY_FILE).exists());
    }

    #[test]
    fn keyring_account_follows_moved_data() {
        let (old, new) = (Path::new("/a/Data"), Path::new("/b/Data"));
        let first = account_from(None, old);
        assert_ne!(first, account_from(None, new));
        // 옮긴 폴더는 적어 둔 이름으로 같은 키를 찾음.
        assert_eq!(account_from(Some(&format!("{first}\r\n")), new), first);
        // 형식이 다르면 경로 해시.
        for bad in [
            "",
            "db-xyz",
            "db-0123456789abcdeg",
            "../db-0123456789abcdef",
        ] {
            assert_eq!(account_from(Some(bad), new), account_from(None, new));
        }
    }
}
