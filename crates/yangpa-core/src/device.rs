//! 신원 키가 서명한 기기 목록과 프로필.

use serde::{Deserialize, Serialize};

use crate::body::Secret32;
use crate::error::CoreError;
use crate::ids::{BlobId, DeviceId, UserId};
use crate::sign::{PQ_PUBLIC_LEN, Sig, SignDomain, SigningKeypair, key_id, pq_public_hash, verify};
use crate::time::Timestamp;

/// 봉인 상자 공개키: X-Wing(X25519 + ML-KEM-768 혼합) 캡슐화 키.
///
/// 만들 때(받은 기기 목록을 풀 때 포함) 키로 쓸 수 있는 값인지 검사합니다. 여러 기기에게 한 번에 봉인하므로, 한
/// 멤버가 망가진 키를 실어 다른 사람들의 키 배포를 막지 못하게 하려는 것입니다.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct BoxPublicKey(Vec<u8>);

impl BoxPublicKey {
    /// X-Wing 캡슐화 키 길이.
    pub const LEN: usize = x_wing::ENCAPSULATION_KEY_SIZE;

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, CoreError> {
        if bytes.len() != Self::LEN || x_wing::EncapsulationKey::try_from(bytes).is_err() {
            return Err(CoreError::InvalidEncoding);
        }
        Ok(BoxPublicKey(bytes.to_vec()))
    }

    /// 시드가 모두 `b`인 상자 키의 공개키. 시험에서 서로 다른 키를 만들 때만 씁니다.
    pub fn filled(b: u8) -> Self {
        use x_wing::{Decapsulator, KeyExport};
        let dk = x_wing::DecapsulationKey::from([b; 32]);
        BoxPublicKey(dk.encapsulation_key().to_bytes().to_vec())
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl std::fmt::Debug for BoxPublicKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "BoxPublicKey({})", hex::encode(&self.0[..4]))
    }
}

/// 이진 형식에서는 바이트 그대로, JSON(로컬 API)에서는 base64 문자열(숫자 1216개 배열보다 짧게).
impl Serialize for BoxPublicKey {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        if s.is_human_readable() {
            use base64::Engine;
            s.serialize_str(&base64::engine::general_purpose::STANDARD.encode(&self.0))
        } else {
            serde_bytes::Bytes::new(&self.0).serialize(s)
        }
    }
}

impl<'de> Deserialize<'de> for BoxPublicKey {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let b = if d.is_human_readable() {
            use base64::Engine;
            let s = String::deserialize(d)?;
            base64::engine::general_purpose::STANDARD
                .decode(s)
                .map_err(D::Error::custom)?
        } else {
            serde_bytes::ByteBuf::deserialize(d)?.into_vec()
        };
        BoxPublicKey::from_bytes(&b).map_err(|_| D::Error::custom("상자 공개키가 아님"))
    }
}

/// 기기 서명 공개키 묶음(Ed25519 + FN-DSA-512). 기기 ID는 이 둘의 키 ID입니다([`key_id`]).
///
/// 기기 목록 항목에 실어 두면, 나중에 이벤트 서명에서 공개키를 빼고(얇은 서명) 받는 쪽이 여기서 찾아 쓸 수 있습니다.
/// 어느 값이 어느 기기 것인지는 [`DeviceEntry::keys_match`]가 기기 ID로 확인합니다.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct DeviceSignKeys {
    pub ed_pk: [u8; 32],
    /// FN-DSA-512 공개키([`PQ_PUBLIC_LEN`]바이트).
    pub pq_pk: Box<[u8]>,
}

impl DeviceSignKeys {
    /// 서명 키 쌍의 공개키 묶음.
    pub fn of(kp: &SigningKeypair) -> Self {
        DeviceSignKeys {
            ed_pk: kp.ed_public(),
            pq_pk: kp.pq_public().into(),
        }
    }

    /// 모든 바이트가 `b`인 값. 기기 ID와 맞지 않으므로 서명·구조 검사를 거치지 않는 시험에서만 씁니다.
    pub fn filled(b: u8) -> Self {
        DeviceSignKeys {
            ed_pk: [b; 32],
            pq_pk: vec![b; PQ_PUBLIC_LEN].into(),
        }
    }

    /// FN-DSA 공개키 해시(키 ID의 두 번째 인자).
    pub fn pq_hash(&self) -> [u8; 32] {
        pq_public_hash(&self.pq_pk)
    }

    /// 이 공개키들의 키 ID(기기 ID가 돼야 하는 값).
    pub fn device_id(&self) -> DeviceId {
        DeviceId(key_id(&self.ed_pk, &self.pq_hash()))
    }
}

impl std::fmt::Debug for DeviceSignKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "DeviceSignKeys({})", hex::encode(&self.ed_pk[..4]))
    }
}

/// [`DeviceSignKeys`]의 직렬화 모양.
#[derive(Serialize, Deserialize)]
struct SignKeysRepr {
    #[serde(with = "key_bytes")]
    ed_pk: Vec<u8>,
    #[serde(with = "key_bytes")]
    pq_pk: Vec<u8>,
}

impl Serialize for DeviceSignKeys {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        SignKeysRepr {
            ed_pk: self.ed_pk.to_vec(),
            pq_pk: self.pq_pk.to_vec(),
        }
        .serialize(s)
    }
}

/// 길이만 확인합니다(기기 ID와 맞는지는 [`DeviceEntry::keys_match`]).
impl<'de> Deserialize<'de> for DeviceSignKeys {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let r = SignKeysRepr::deserialize(d)?;
        let ed_pk: [u8; 32] = r
            .ed_pk
            .try_into()
            .map_err(|_| D::Error::custom("Ed25519 공개키 길이 오류"))?;
        if r.pq_pk.len() != PQ_PUBLIC_LEN {
            return Err(D::Error::custom("FN-DSA 공개키 길이 오류"));
        }
        Ok(DeviceSignKeys {
            ed_pk,
            pq_pk: r.pq_pk.into(),
        })
    }
}

/// 키 바이트: 이진 형식에서는 그대로, 사람이 읽는 형식(로컬 API JSON)에서는 base64.
mod key_bytes {
    use base64::Engine;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(v: &[u8], s: S) -> Result<S::Ok, S::Error> {
        if s.is_human_readable() {
            s.serialize_str(&base64::engine::general_purpose::STANDARD.encode(v))
        } else {
            serde_bytes::Bytes::new(v).serialize(s)
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        if d.is_human_readable() {
            let s = String::deserialize(d)?;
            base64::engine::general_purpose::STANDARD
                .decode(s)
                .map_err(serde::de::Error::custom)
        } else {
            Ok(serde_bytes::ByteBuf::deserialize(d)?.into_vec())
        }
    }
}

/// 32바이트 값: 이진 형식에서는 고정 길이 그대로, 사람이 읽는 형식에서는 16진 문자열.
mod bytes32 {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &[u8; 32], s: S) -> Result<S::Ok, S::Error> {
        if s.is_human_readable() {
            s.serialize_str(&hex::encode(v))
        } else {
            serde::Serialize::serialize(v, s)
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 32], D::Error> {
        use serde::de::Error;
        if d.is_human_readable() {
            let s = String::deserialize(d)?;
            let v = hex::decode(s).map_err(D::Error::custom)?;
            v.try_into()
                .map_err(|_| D::Error::custom("32바이트가 아님"))
        } else {
            <[u8; 32]>::deserialize(d)
        }
    }
}

/// `Option<[u8; 32]>`: 값이 있으면 [`bytes32`]와 같은 모양.
mod opt_bytes32 {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    #[derive(Serialize, Deserialize)]
    struct W(#[serde(with = "super::bytes32")] [u8; 32]);

    pub fn serialize<S: Serializer>(v: &Option<[u8; 32]>, s: S) -> Result<S::Ok, S::Error> {
        v.map(W).serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<[u8; 32]>, D::Error> {
        Ok(Option::<W>::deserialize(d)?.map(|w| w.0))
    }
}

/// 없어진 계정 OTP의 키 ID: OTP 비밀에서 만든 서명 키([`otp_signing_key`])의 키 ID. 서명 바이트에 들어가고 해소기가 옛
/// 복구 판을 가릴 때 보므로 남깁니다([`SignedDeviceList::classify`]).
pub type OtpKeyId = [u8; 32];

/// OTP 비밀에서 기기 목록 OTP 서명 키를 만듭니다. 같은 비밀이면 늘 같은 키입니다. 옛 복구 판을 재현하는 시험만 씁니다.
pub fn otp_signing_key(secret: &[u8]) -> SigningKeypair {
    let seed = zeroize::Zeroizing::new(blake3::derive_key("yangpa/v3/otp-sign-key", secret));
    SigningKeypair::from_seed(&seed)
}

/// 기기 키 약속: 상자 키·서명 키 전부를 묶은 해시. 공간 상태의 간추린 기기 기록([`DeviceRecord`])은 키 대신 이 값만
/// 들고, 키 자체는 이 값으로 찾는 내용 주소 표에 둡니다. 같은 기기 ID에 다른 상자 키를 실은 위조 목록이 와도 약속이
/// 달라 진짜 키 자리를 덮지 못합니다. 가변 길이 값은 길이를 앞에 붙여 경계를 옮겨 맞추지 못하게 합니다.
pub fn key_commit(box_key: &BoxPublicKey, sign: &DeviceSignKeys) -> [u8; 32] {
    let mut h = blake3::Hasher::new_derive_key("yangpa/v3/device-key-commit");
    h.update(&(box_key.as_bytes().len() as u64).to_le_bytes());
    h.update(box_key.as_bytes());
    h.update(&sign.ed_pk);
    h.update(&(sign.pq_pk.len() as u64).to_le_bytes());
    h.update(&sign.pq_pk);
    *h.finalize().as_bytes()
}

/// 기기 하나의 정보. `box_key`는 봉인 상자(기기 대상 암호화)용 공개키, `sign_keys`는 기기 서명 공개키입니다.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceEntry {
    pub device: DeviceId,
    pub box_key: BoxPublicKey,
    pub sign_keys: DeviceSignKeys,
    pub name: String,
    pub added_at: Timestamp,
    /// 폐기 시각. 폐기된 기기는 다음 회전에서 모든 키 그룹에서 빠집니다.
    #[serde(default)]
    pub revoked_at: Option<Timestamp>,
}

impl DeviceEntry {
    /// 기기 ID가 실린 서명 공개키의 키 ID인지. 서명 확인보다 훨씬 싸서 받은 목록은 이것부터 봅니다.
    pub fn keys_match(&self) -> bool {
        self.sign_keys.device_id() == self.device
    }

    /// 이 항목의 기기 키 약속([`key_commit`]).
    pub fn key_commit(&self) -> [u8; 32] {
        key_commit(&self.box_key, &self.sign_keys)
    }

    /// 기기 키 표에 넣을 키 묶음.
    pub fn keys(&self) -> DeviceKeys {
        DeviceKeys {
            device: self.device,
            box_key: self.box_key.clone(),
            sign_keys: self.sign_keys.clone(),
        }
    }

    /// 공간 상태에 두는 간추린 기록(이름·키 본문을 뺌).
    pub fn record(&self) -> DeviceRecord {
        DeviceRecord {
            device: self.device,
            added_at: self.added_at,
            revoked_at: self.revoked_at,
            ed_pk: self.sign_keys.ed_pk,
            pq_hash: self.sign_keys.pq_hash(),
            key_commit: self.key_commit(),
        }
    }
}

/// 기기 키 표의 한 행: 키 약속([`key_commit`])으로 찾는 한 기기의 공개키 전부. 표는 내용 주소라 같은 기기 ID라도
/// 키가 다르면 다른 행이고, 누가 어떤 값을 넣든 상태의 기록이 가리키는 약속의 행만 쓰입니다. `GetDeviceKeys` 답으로도
/// 오갑니다(받는 쪽은 키 약속이나 증명이 붙은 잎으로만 받음).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceKeys {
    pub device: DeviceId,
    pub box_key: BoxPublicKey,
    pub sign_keys: DeviceSignKeys,
}

impl DeviceKeys {
    pub fn key_commit(&self) -> [u8; 32] {
        key_commit(&self.box_key, &self.sign_keys)
    }

    /// 기기 ID가 서명 키의 키 ID인지([`DeviceEntry::keys_match`]).
    pub fn keys_match(&self) -> bool {
        self.sign_keys.device_id() == self.device
    }
}

/// 공간 상태에 두는 기기 기록(약 140B). 상자 키(1,216B)·FN-DSA 공개키(897B)·이름은 빼고 키 약속만 둡니다. 키는
/// 약속으로 찾는 표(데몬의 기기 키 표)에 있고, 이름은 사용자의 서명된 기기 목록에 있습니다.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceRecord {
    pub device: DeviceId,
    pub added_at: Timestamp,
    #[serde(default)]
    pub revoked_at: Option<Timestamp>,
    #[serde(with = "bytes32")]
    pub ed_pk: [u8; 32],
    /// FN-DSA 공개키 해시. `device == key_id(ed_pk, pq_hash)`.
    #[serde(with = "bytes32")]
    pub pq_hash: [u8; 32],
    /// 상자 키·서명 키 약속([`key_commit`]).
    #[serde(with = "bytes32")]
    pub key_commit: [u8; 32],
}

/// 공간 상태에 두는 한 멤버의 기기 기록(서명된 [`DeviceList`]를 간추린 것).
///
/// 새 판이 이어받는지([`DeviceList::extends`]: 판, 폐기를 되돌리지 않음, 같은 기기는 같은 키 약속)를 이것만으로 볼 수
/// 있습니다. 해소기는 받은 목록을 한 번 간추린 뒤 새 판도 이 모양으로 견줍니다([`Self::extends`], [`Self::classify`]).
///
/// 직렬화는 OTP 키가 없으면 OTP 전과 바이트가 같습니다([`RecordBin`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemberDevices {
    pub version: u64,
    pub devices: Vec<DeviceRecord>,
    /// 계정 OTP 키([`DeviceList::otp`]).
    pub otp: Option<OtpKeyId>,
}

/// [`MemberDevices`]의 이진 형식 기록 하나: [`DeviceRecord`]와 같은 모양이되 폐기 칸이 열거형입니다. 0·1번은 예전
/// `Option<Timestamp>`와 바이트가 같고(postcard의 `Option`과 열거형 번호가 같은 모양), 2번은 OTP 키 표시 기록(기기
/// 칸에 OTP 키 ID, 목록 끝에 하나)입니다. 그래서 OTP 전에 저장된 상태와 체크포인트의 사용자 잎 값
/// ([`crate::checkpoint::user_value`])이 그대로입니다.
#[derive(Serialize, Deserialize)]
struct RecordBin {
    device: DeviceId,
    added_at: Timestamp,
    revoked: RevokedSlot,
    #[serde(with = "bytes32")]
    ed_pk: [u8; 32],
    #[serde(with = "bytes32")]
    pq_hash: [u8; 32],
    #[serde(with = "bytes32")]
    key_commit: [u8; 32],
}

#[derive(Serialize, Deserialize)]
enum RevokedSlot {
    Active,
    At(Timestamp),
    OtpKey,
}

/// 기록 목록(+ OTP 표시 기록)을 모아 두지 않고 씁니다.
struct RecordsBin<'a>(&'a MemberDevices);

impl Serialize for RecordsBin<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        let m = self.0;
        let mut seq = s.serialize_seq(Some(m.devices.len() + usize::from(m.otp.is_some())))?;
        for d in &m.devices {
            seq.serialize_element(&RecordBin {
                device: d.device,
                added_at: d.added_at,
                revoked: d.revoked_at.map_or(RevokedSlot::Active, RevokedSlot::At),
                ed_pk: d.ed_pk,
                pq_hash: d.pq_hash,
                key_commit: d.key_commit,
            })?;
        }
        if let Some(k) = m.otp {
            seq.serialize_element(&RecordBin {
                device: DeviceId(k),
                added_at: Timestamp(0),
                revoked: RevokedSlot::OtpKey,
                ed_pk: [0; 32],
                pq_hash: [0; 32],
                key_commit: [0; 32],
            })?;
        }
        seq.end()
    }
}

#[derive(Serialize)]
struct MemberDevicesBinRef<'a> {
    version: u64,
    devices: RecordsBin<'a>,
}

#[derive(Deserialize)]
struct MemberDevicesBin {
    version: u64,
    devices: Vec<RecordBin>,
}

/// 사람이 읽는 형식(JSON) 모양. OTP 키가 없으면 칸이 빠져 예전 모양과 같습니다.
#[derive(Serialize, Deserialize)]
struct MemberDevicesJson {
    version: u64,
    devices: Vec<DeviceRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "opt_bytes32")]
    otp: Option<OtpKeyId>,
}

impl Serialize for MemberDevices {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        if s.is_human_readable() {
            MemberDevicesJson {
                version: self.version,
                devices: self.devices.clone(),
                otp: self.otp,
            }
            .serialize(s)
        } else {
            MemberDevicesBinRef {
                version: self.version,
                devices: RecordsBin(self),
            }
            .serialize(s)
        }
    }
}

impl<'de> Deserialize<'de> for MemberDevices {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        if d.is_human_readable() {
            let j = MemberDevicesJson::deserialize(d)?;
            return Ok(MemberDevices {
                version: j.version,
                devices: j.devices,
                otp: j.otp,
            });
        }
        let b = MemberDevicesBin::deserialize(d)?;
        let n = b.devices.len();
        let mut devices = Vec::with_capacity(n);
        let mut otp = None;
        for (i, r) in b.devices.into_iter().enumerate() {
            let revoked_at = match r.revoked {
                RevokedSlot::Active => None,
                RevokedSlot::At(t) => Some(t),
                // 표시 기록은 끝에 하나만(같은 내용이 늘 같은 바이트가 되게).
                RevokedSlot::OtpKey if i + 1 == n => {
                    otp = Some(r.device.0);
                    continue;
                }
                RevokedSlot::OtpKey => return Err(D::Error::custom("OTP 표시 기록 위치 오류")),
            };
            devices.push(DeviceRecord {
                device: r.device,
                added_at: r.added_at,
                revoked_at,
                ed_pk: r.ed_pk,
                pq_hash: r.pq_hash,
                key_commit: r.key_commit,
            });
        }
        Ok(MemberDevices {
            version: b.version,
            devices,
            otp,
        })
    }
}

impl MemberDevices {
    /// 서명된 목록을 간추립니다(항목마다 키 약속을 계산).
    pub fn from_list(list: &DeviceList) -> Self {
        MemberDevices {
            version: list.version,
            devices: list.devices.iter().map(DeviceEntry::record).collect(),
            otp: list.otp,
        }
    }

    /// 간추린 새 판(`self`)이 `old`를 이어받는지. 같은 목록으로 [`DeviceList::extends`]에 `old`를 넘긴 것과 같은
    /// 판정입니다(사용자는 부르는 쪽이 봄).
    pub fn extends(&self, old: &MemberDevices) -> bool {
        if self.version <= old.version {
            return false;
        }
        // 같은 기기가 두 번 있으면 뒤의 항목(목록의 `entries_by_id`와 같게).
        let mine: std::collections::HashMap<&DeviceId, &DeviceRecord> =
            self.devices.iter().map(|d| (&d.device, d)).collect();
        old.devices.iter().all(|o| {
            mine.get(&o.device).is_some_and(|n| {
                n.key_commit == o.key_commit && (o.revoked_at.is_none() || n.revoked_at.is_some())
            })
        })
    }

    /// 간추린 새 판(`self`, 공동 서명 기기 `cosigner`, OTP 키 서명이 있는지 `otp_signed`)을 `known`에 비춘 판정. 같은
    /// 목록의 [`SignedDeviceList::classify`]와 같습니다(서명이 맞는지는 부르는 쪽이 이미 봄).
    pub fn classify(
        &self,
        cosigner: Option<DeviceId>,
        otp_signed: bool,
        known: &MemberDevices,
    ) -> ListChange {
        let Some(c) = cosigner else {
            return ListChange::Rejected;
        };
        if known.contains_active(&c) {
            return ListChange::Authorized;
        }
        if known.get(&c).is_some() {
            return ListChange::Rejected;
        }
        let only_self_added = self
            .active()
            .filter(|d| known.get(&d.device).is_none())
            .all(|d| d.device == c);
        // 같은 기기가 여러 번이면 하나라도 폐기면 폐기([`DeviceList::is_revoked`]와 같게).
        let revokes_known = known.active().any(|k| {
            self.devices
                .iter()
                .any(|d| d.device == k.device && d.revoked_at.is_some())
        });
        if !(self.extends(known) && self.contains_active(&c) && only_self_added && !revokes_known) {
            return ListChange::Rejected;
        }
        recovery_otp(c, self.otp, otp_signed, known.otp)
    }

    pub fn active(&self) -> impl Iterator<Item = &DeviceRecord> {
        self.devices.iter().filter(|d| d.revoked_at.is_none())
    }

    pub fn get(&self, device: &DeviceId) -> Option<&DeviceRecord> {
        self.devices.iter().find(|d| d.device == *device)
    }

    pub fn contains_active(&self, device: &DeviceId) -> bool {
        self.active().any(|d| d.device == *device)
    }

    /// 이 기록에서 폐기된 기기인지(없는 기기는 거짓).
    pub fn is_revoked(&self, device: &DeviceId) -> bool {
        self.get(device).is_some_and(|d| d.revoked_at.is_some())
    }
}

/// 받는 쪽이 한 사용자에 대해 아는 기기 목록: 서명된 목록 그대로([`DeviceList`])이거나 공간 상태의 간추린 기록
/// ([`MemberDevices`]). 새 판을 이 목록에 비춰 볼 때([`DeviceList::extends`], [`SignedDeviceList::classify`]) 씁니다.
pub trait KnownDevices {
    fn known_version(&self) -> u64;
    /// 이 목록에 있는 기기인지(폐기 포함).
    fn knows(&self, device: &DeviceId) -> bool;
    fn knows_active(&self, device: &DeviceId) -> bool;
    fn active_ids(&self) -> Vec<DeviceId>;
    /// `new`가 이 목록의 기기를 모두 같은 키로 담고 폐기된 기기는 계속 폐기하는지(판·사용자는 따로 봄).
    fn kept_by(&self, new: &DeviceList) -> bool;
    /// 목록의 사용자. 간추린 기록은 사용자를 들고 있지 않아(상태의 표 열쇠) 부르는 쪽이 봅니다.
    fn known_user(&self) -> Option<UserId> {
        None
    }
    /// 계정 OTP 키.
    fn known_otp(&self) -> Option<OtpKeyId>;
}

/// 모양이 맞는 복구 판(새 기기 `c`)의 OTP 판정. 복구 판은 OTP 설정을 바꾸지 못하고(복구 문구만으로 OTP를 끄거나 다른
/// 키로 바꾸지 못하게), 아는 목록에 OTP 키가 있으면 그 키의 서명이 있어야 받습니다.
fn recovery_otp(
    c: DeviceId,
    new_otp: Option<OtpKeyId>,
    otp_signed: bool,
    known_otp: Option<OtpKeyId>,
) -> ListChange {
    if new_otp != known_otp {
        ListChange::Rejected
    } else if known_otp.is_some() && !otp_signed {
        ListChange::OtpMissing(c)
    } else {
        ListChange::Recovery(c)
    }
}

/// 새 판의 항목을 기기 ID로 찾는 표(받은 목록이 클 수 있어 항목 수의 곱만큼 비교하지 않게).
fn entries_by_id(list: &DeviceList) -> std::collections::HashMap<&DeviceId, &DeviceEntry> {
    list.devices.iter().map(|d| (&d.device, d)).collect()
}

impl KnownDevices for DeviceList {
    fn known_version(&self) -> u64 {
        self.version
    }

    fn knows(&self, device: &DeviceId) -> bool {
        self.devices.iter().any(|d| d.device == *device)
    }

    fn knows_active(&self, device: &DeviceId) -> bool {
        self.contains_active(device)
    }

    fn active_ids(&self) -> Vec<DeviceId> {
        self.active().map(|d| d.device).collect()
    }

    fn kept_by(&self, new: &DeviceList) -> bool {
        let mine = entries_by_id(new);
        self.devices.iter().all(|o| {
            mine.get(&o.device).is_some_and(|n| {
                n.box_key == o.box_key
                    && n.sign_keys == o.sign_keys
                    && (o.revoked_at.is_none() || n.revoked_at.is_some())
            })
        })
    }

    fn known_user(&self) -> Option<UserId> {
        Some(self.user)
    }

    fn known_otp(&self) -> Option<OtpKeyId> {
        self.otp
    }
}

impl KnownDevices for MemberDevices {
    fn known_version(&self) -> u64 {
        self.version
    }

    fn knows(&self, device: &DeviceId) -> bool {
        self.get(device).is_some()
    }

    fn knows_active(&self, device: &DeviceId) -> bool {
        self.contains_active(device)
    }

    fn active_ids(&self) -> Vec<DeviceId> {
        self.active().map(|d| d.device).collect()
    }

    fn kept_by(&self, new: &DeviceList) -> bool {
        let mine = entries_by_id(new);
        self.devices.iter().all(|o| {
            mine.get(&o.device).is_some_and(|n| {
                n.key_commit() == o.key_commit && (o.revoked_at.is_none() || n.revoked_at.is_some())
            })
        })
    }

    fn known_otp(&self) -> Option<OtpKeyId> {
        self.otp
    }
}

/// 신원 키가 서명한 기기 목록. 버전이 높은 것이 최신입니다.
///
/// 서명 대상 바이트는 이 구조의 postcard 인코딩이고, OTP 키가 없으면 그 칸이 빠져 OTP 전의 목록과 같은 바이트입니다
/// (예전 서명이 그대로 맞음). 다른 구조 안에서는 [`SignedDeviceList`]의 직렬화를 거칩니다.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceList {
    pub user: UserId,
    pub version: u64,
    pub devices: Vec<DeviceEntry>,
    /// 없어진 계정 OTP의 키(예전에 켠 계정). 다음 판이 그대로 이어받습니다.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "opt_bytes32")]
    pub otp: Option<OtpKeyId>,
}

/// 기기 목록 공동 서명: 이 판을 만든 기기가 기기 키로 한 서명.
///
/// 신원 키는 모든 기기에 있으므로 신원 서명만으로는 누가 판을 만들었는지 알 수 없습니다. 받는 쪽은
/// 공동 서명 기기가 자기가 아는 이전 목록에서 활성인지로 판을 가립니다([`SignedDeviceList::classify`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceCosig {
    pub device: DeviceId,
    pub sig: Sig,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignedDeviceList {
    pub list: DeviceList,
    pub sig: Sig,
    /// 판을 만든 기기의 서명. 없으면 처음 보는 사용자의 목록으로만 받습니다.
    pub cosig: Option<DeviceCosig>,
    /// `list.otp` 키의 서명(같은 바이트). 옛 빌드의 복구 판에만 있습니다.
    pub otp_sig: Option<Sig>,
}

/// [`SignedDeviceList`]의 사람이 읽는 형식(JSON) 모양.
#[derive(Serialize, Deserialize)]
struct SignedListJson {
    list: DeviceList,
    sig: Sig,
    #[serde(default)]
    cosig: Option<DeviceCosig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    otp_sig: Option<Sig>,
}

/// 이진 형식의 목록 본문: OTP 키를 뺀 예전 [`DeviceList`] 모양(OTP 키는 [`CosigSlot::Ext`]에).
#[derive(Serialize)]
struct ListBodyRef<'a> {
    user: &'a UserId,
    version: u64,
    devices: &'a [DeviceEntry],
}

#[derive(Deserialize)]
struct ListBody {
    user: UserId,
    version: u64,
    devices: Vec<DeviceEntry>,
}

/// 공동 서명 칸. 0·1번은 예전 `Option<DeviceCosig>`와 바이트가 같고(postcard의 `Option`과 열거형 번호가 같은 모양),
/// 2번이 OTP 확장입니다. 그래서 OTP 전에 저장·서명된 목록(크루 상태 이벤트·체크포인트 안의 것 포함)을 그대로 읽습니다.
#[derive(Deserialize)]
enum CosigSlot {
    None,
    Some(DeviceCosig),
    Ext(Box<ListExt>),
}

#[derive(Serialize)]
enum CosigSlotRef<'a> {
    None,
    Some(&'a DeviceCosig),
    Ext(ListExtRef<'a>),
}

#[derive(Deserialize)]
struct ListExt {
    cosig: Option<DeviceCosig>,
    #[serde(with = "opt_bytes32")]
    otp: Option<OtpKeyId>,
    otp_sig: Option<Sig>,
}

#[derive(Serialize)]
struct ListExtRef<'a> {
    cosig: &'a Option<DeviceCosig>,
    #[serde(with = "opt_bytes32")]
    otp: Option<OtpKeyId>,
    otp_sig: &'a Option<Sig>,
}

#[derive(Serialize)]
struct SignedListBinRef<'a> {
    list: ListBodyRef<'a>,
    sig: &'a Sig,
    cosig: CosigSlotRef<'a>,
}

#[derive(Deserialize)]
struct SignedListBin {
    list: ListBody,
    sig: Sig,
    cosig: CosigSlot,
}

impl Serialize for SignedDeviceList {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        if s.is_human_readable() {
            return SignedListJson {
                list: self.list.clone(),
                sig: self.sig.clone(),
                cosig: self.cosig.clone(),
                otp_sig: self.otp_sig.clone(),
            }
            .serialize(s);
        }
        let plain = self.list.otp.is_none() && self.otp_sig.is_none();
        let cosig = match (&self.cosig, plain) {
            (None, true) => CosigSlotRef::None,
            (Some(c), true) => CosigSlotRef::Some(c),
            (cosig, false) => CosigSlotRef::Ext(ListExtRef {
                cosig,
                otp: self.list.otp,
                otp_sig: &self.otp_sig,
            }),
        };
        SignedListBinRef {
            list: ListBodyRef {
                user: &self.list.user,
                version: self.list.version,
                devices: &self.list.devices,
            },
            sig: &self.sig,
            cosig,
        }
        .serialize(s)
    }
}

impl<'de> Deserialize<'de> for SignedDeviceList {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        if d.is_human_readable() {
            let j = SignedListJson::deserialize(d)?;
            return Ok(SignedDeviceList {
                list: j.list,
                sig: j.sig,
                cosig: j.cosig,
                otp_sig: j.otp_sig,
            });
        }
        let b = SignedListBin::deserialize(d)?;
        let (cosig, otp, otp_sig) = match b.cosig {
            CosigSlot::None => (None, None, None),
            CosigSlot::Some(c) => (Some(c), None, None),
            // 빈 확장은 0·1번으로 써야 할 값입니다(같은 내용이 늘 같은 바이트가 되게).
            CosigSlot::Ext(e) if e.otp.is_none() && e.otp_sig.is_none() => {
                return Err(D::Error::custom("빈 OTP 확장"));
            }
            CosigSlot::Ext(e) => (e.cosig, e.otp, e.otp_sig),
        };
        Ok(SignedDeviceList {
            list: DeviceList {
                user: b.list.user,
                version: b.list.version,
                devices: b.list.devices,
                otp,
            },
            sig: b.sig,
            cosig,
            otp_sig,
        })
    }
}

/// 받는 쪽이 아는 목록에 비춘 새 판의 판정([`SignedDeviceList::classify`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListChange {
    /// 아는 목록에서 활성인 기기가 만든 판. 바로 받습니다.
    Authorized,
    /// 아는 목록에 없던 기기가 자기 자신만 더한 판(살아 있는 기기 없이 시드로 복구). 아는 목록을 그대로 이어받고
    /// (상자 키를 바꾸거나 기기를 빼지 않음) 아는 기기를 폐기하지 않고 OTP 설정을 그대로 둘 때만이며, 아는 목록에 OTP
    /// 키가 있으면 그 키의 서명도 있어야 합니다. 받는 쪽은 바로 받습니다.
    Recovery(DeviceId),
    /// 복구 판의 모양은 맞지만 아는 목록에 OTP 키가 있는데 그 키의 서명이 없음. 그 기기가 OTP 비밀을 받아 다시 서명해야
    /// 받습니다.
    OtpMissing(DeviceId),
    /// 공동 서명이 없거나, 아는 목록에서 폐기된 기기가 만들었거나, 복구인데 아는 기기를 폐기하거나 다른 기기를 더함.
    Rejected,
}

/// 이보다 높은 판은 따라가지 않습니다(바로잡는 판의 번호를 위조 판 너머로 올릴 때). 폐기된 기기가 판을 `u64::MAX`로 낸
/// 위조 목록을 따라가면 그 뒤로 어떤 판도 낼 수 없게 되기 때문입니다.
pub const MAX_FOLLOWED_VERSION: u64 = u64::MAX / 2;

impl DeviceList {
    /// 신원 키로만 서명합니다(공동 서명 없음). 처음 보는 사용자의 목록으로만 받아들여지므로 시험·검사용입니다.
    pub fn sign(self, identity: &SigningKeypair) -> Result<SignedDeviceList, CoreError> {
        let bytes = postcard::to_stdvec(&self)?;
        let sig = identity.sign(SignDomain::DeviceList, &bytes);
        Ok(SignedDeviceList {
            list: self,
            sig,
            cosig: None,
            otp_sig: None,
        })
    }

    /// 신원 키로 서명하고, 이 판을 만든 기기(`device`)가 함께 서명합니다.
    pub fn sign_with(
        self,
        identity: &SigningKeypair,
        device: &SigningKeypair,
    ) -> Result<SignedDeviceList, CoreError> {
        let bytes = postcard::to_stdvec(&self)?;
        let sig = identity.sign(SignDomain::DeviceList, &bytes);
        let cosig = DeviceCosig {
            device: DeviceId(device.public()),
            sig: device.sign(SignDomain::DeviceListCosign, &bytes),
        };
        Ok(SignedDeviceList {
            list: self,
            sig,
            cosig: Some(cosig),
            otp_sig: None,
        })
    }

    pub fn active(&self) -> impl Iterator<Item = &DeviceEntry> {
        self.devices.iter().filter(|d| d.revoked_at.is_none())
    }

    pub fn contains_active(&self, device: &DeviceId) -> bool {
        self.active().any(|d| d.device == *device)
    }

    /// 이 목록에서 폐기된 기기인지(없는 기기는 거짓).
    pub fn is_revoked(&self, device: &DeviceId) -> bool {
        self.devices
            .iter()
            .any(|d| d.device == *device && d.revoked_at.is_some())
    }

    /// 이 목록이 `old`를 이어받는지: 같은 사용자, 더 높은 판, `old`의 기기를 모두 같은 키(상자 키·서명 키, 간추린
    /// 기록이면 키 약속)로 담고, 폐기된 기기는 계속 폐기. 목록이 이렇게 늘기만 하므로 한 기기가 만든 새 판이 다른
    /// 기기를 몰래 빼거나 폐기된 기기를 되살리지 못합니다(빼려면 폐기 표시를 남겨야 함). `old`가 간추린 기록이면 같은
    /// 사용자인지는 부르는 쪽이 봅니다(상태의 표 열쇠).
    pub fn extends<K: KnownDevices + ?Sized>(&self, old: &K) -> bool {
        if old.known_user().is_some_and(|u| u != self.user) || self.version <= old.known_version() {
            return false;
        }
        old.kept_by(self)
    }

    /// 항목이 `max_entries` 이하이고, 같은 기기가 두 번 나오지 않고, 기기 ID마다 실린 서명 공개키의 키 ID인지. 받은
    /// 목록은 서명 확인·이어받기·합치기 전에 이것부터 봅니다.
    pub fn well_formed(&self, max_entries: usize) -> bool {
        if self.devices.len() > max_entries {
            return false;
        }
        let mut seen = std::collections::HashSet::with_capacity(self.devices.len());
        self.devices
            .iter()
            .all(|d| seen.insert(d.device) && d.keys_match())
    }

    /// 기기 항목 하나를 넣거나(같은 기기가 있으면 바꿈) 더한 다음 판. 새 기기를 들일 때 씁니다.
    pub fn with_entry(&self, entry: DeviceEntry) -> DeviceList {
        let mut devices = self.devices.clone();
        match devices.iter_mut().find(|d| d.device == entry.device) {
            Some(d) => *d = entry,
            None => devices.push(entry),
        }
        DeviceList {
            user: self.user,
            version: self.version.saturating_add(1),
            devices,
            otp: self.otp,
        }
    }

    /// 폐기된 기기가 만든 위조 판(`forged`)을 바로잡는 판: 이 목록 그대로에 위조 판이 더한 기기만 `at`에 폐기된 채로
    /// 넣습니다. 위조 판의 폐기·상자 키·다른 항목은 하나도 가져오지 않습니다(위조 판으로 내 기기를 폐기하거나 키를
    /// 바꾸게 하지 않음). 판은 위조 판보다 높여, 폐기를 모르고 위조 판을 받은 피어도 이 판을 이어받게 합니다(위조 판의
    /// 번호가 터무니없이 크면 따라가지 않음).
    pub fn heal(&self, forged: &DeviceList, at: Timestamp) -> DeviceList {
        let mut devices = self.devices.clone();
        // 기기 ID와 서명 키가 맞지 않는 항목은 가져오지 않습니다(바로잡는 판이 구조 검사에 걸리지 않게).
        let mut added: Vec<DeviceEntry> = forged
            .active()
            .filter(|f| f.keys_match() && !self.devices.iter().any(|d| d.device == f.device))
            .cloned()
            .collect();
        added.sort_by_key(|d| d.device);
        added.dedup_by_key(|d| d.device);
        for mut e in added {
            e.revoked_at = Some(at);
            devices.push(e);
        }
        let base = if forged.version <= MAX_FOLLOWED_VERSION {
            self.version.max(forged.version)
        } else {
            self.version
        };
        DeviceList {
            user: self.user,
            version: base.saturating_add(1),
            devices,
            otp: self.otp,
        }
    }

    /// 두 목록을 합친 새 판: 기기 ID 순 합집합, 어느 쪽에서든 폐기된 기기는 폐기(이른 시각), 판은 둘 중 큰 것 + 1,
    /// OTP 키는 판이 높은 쪽(같으면 큰 값: 어느 쪽에서 합쳐도 같게).
    /// 같은 두 목록이면 어느 기기가 합쳐도 같은 목록이 나와 동시에 합쳐도 갈라지지 않습니다(서명은 매번 달라 받는 쪽이
    /// 목록 내용으로 비교함).
    pub fn merge(&self, other: &DeviceList) -> DeviceList {
        let mut out: std::collections::BTreeMap<DeviceId, DeviceEntry> =
            std::collections::BTreeMap::new();
        // 판이 높은 쪽 항목을 먼저 넣어, 같은 기기의 다른 항목이 있으면 그쪽을 씁니다(같으면 self 먼저).
        let (first, second) = if other.version > self.version {
            (other, self)
        } else {
            (self, other)
        };
        for e in first.devices.iter().chain(second.devices.iter()) {
            match out.get_mut(&e.device) {
                None => {
                    out.insert(e.device, e.clone());
                }
                Some(have) => {
                    have.revoked_at = match (have.revoked_at, e.revoked_at) {
                        (Some(a), Some(b)) => Some(a.min(b)),
                        (a, b) => a.or(b),
                    };
                    // 판이 같으면 먼저 넣은 쪽이 기기마다 달라, 이름을 값으로 골라야 양쪽 결과가 같습니다(빈 이름이 이김).
                    if self.version == other.version && e.name < have.name {
                        have.name = e.name.clone();
                    }
                }
            }
        }
        let otp = if self.version == other.version {
            self.otp.max(other.otp)
        } else {
            first.otp
        };
        DeviceList {
            user: self.user,
            version: self.version.max(other.version).saturating_add(1),
            devices: out.into_values().collect(),
            otp,
        }
    }
}

impl SignedDeviceList {
    /// 신원 서명과(있으면) 공동 서명을 확인합니다. 공동 서명 기기가 믿을 만한지는 [`Self::classify`]가 봅니다. 기기 ID가
    /// 실린 서명 공개키와 맞지 않는 항목이 있으면 서명을 보기 전에 거절합니다(검증한 목록의 키는 늘 그 기기 것).
    pub fn verify(&self) -> Result<(), CoreError> {
        if !self.list.devices.iter().all(DeviceEntry::keys_match) {
            return Err(CoreError::InvalidEncoding);
        }
        let bytes = postcard::to_stdvec(&self.list)?;
        verify(
            self.list.user.as_bytes(),
            SignDomain::DeviceList,
            &bytes,
            &self.sig,
        )?;
        if let Some(c) = &self.cosig {
            verify(
                c.device.as_bytes(),
                SignDomain::DeviceListCosign,
                &bytes,
                &c.sig,
            )?;
        }
        if let Some(s) = &self.otp_sig {
            let k = self.list.otp.ok_or(CoreError::BadSignature)?;
            verify(&k, SignDomain::DeviceListOtp, &bytes, s)?;
        }
        Ok(())
    }

    /// OTP 키로도 서명합니다(옛 빌드의 복구 판을 재현하는 시험용). 목록의 OTP 키가 아니면 오류.
    pub fn with_otp_sig(mut self, otp: &SigningKeypair) -> Result<Self, CoreError> {
        if self.list.otp != Some(otp.public()) {
            return Err(CoreError::BadSignature);
        }
        let bytes = postcard::to_stdvec(&self.list)?;
        self.otp_sig = Some(otp.sign(SignDomain::DeviceListOtp, &bytes));
        Ok(self)
    }

    /// 공동 서명한 기기.
    pub fn cosigner(&self) -> Option<DeviceId> {
        self.cosig.as_ref().map(|c| c.device)
    }

    /// `known`(받는 쪽이 이 사용자에 대해 아는 목록, 또는 공간 상태의 간추린 기록)에 비춘 판정. 서명 확인
    /// ([`Self::verify`])과 이어받기([`DeviceList::extends`])는 따로 봅니다.
    ///
    /// 폐기된 기기도 신원 키를 갖고 있으므로, 자기 공동 서명으로 새 기기를 더한 판은 `Rejected`입니다. 새로 만든 기기
    /// 키로 서명하면 `Recovery`가 되지만, 아는 목록에 OTP 키가 있으면 그 키의 서명([`Self::otp_sig`], 확인은
    /// [`Self::verify`])도 있어야 합니다(없으면 `OtpMissing`).
    pub fn classify<K: KnownDevices + ?Sized>(&self, known: &K) -> ListChange {
        let Some(c) = &self.cosig else {
            return ListChange::Rejected;
        };
        if known.knows_active(&c.device) {
            return ListChange::Authorized;
        }
        if known.knows(&c.device) {
            // 아는 목록에서 폐기된 기기.
            return ListChange::Rejected;
        }
        let only_self_added = self
            .list
            .active()
            .filter(|d| !known.knows(&d.device))
            .all(|d| d.device == c.device);
        let revokes_known = known.active_ids().iter().any(|k| self.list.is_revoked(k));
        // 아는 목록을 그대로 이어받아야 합니다: 상자 키를 바꾼 복구 판을 받아 두면, 그것을 바탕으로 낸 승인·거부 판이 다른
        // 피어의 목록을 이어받지 못해 조용히 버려집니다.
        if !(self.list.extends(known)
            && self.list.contains_active(&c.device)
            && only_self_added
            && !revokes_known)
        {
            return ListChange::Rejected;
        }
        recovery_otp(
            c.device,
            self.list.otp,
            self.otp_sig.is_some(),
            known.known_otp(),
        )
    }
}

/// 프로필 문서. 이미지는 블롭 해시로 참조합니다.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Profile {
    pub version: u64,
    pub name: String,
    #[serde(default)]
    pub bio: String,
    #[serde(default)]
    pub avatar: Option<BlobId>,
    #[serde(default)]
    pub banner: Option<BlobId>,
    /// 테마 색(0xRRGGBB).
    #[serde(default)]
    pub theme_color: Option<u32>,
    #[serde(default)]
    pub status: String,
    /// 아바타·배너 블롭의 복호화 키. 서명된 프로필에 실려 친구·같은 공간 멤버에게만 갑니다.
    /// 사람이 읽는 형식(로컬 API JSON)에서는 항상 비워 내보내고, 받아도 버립니다.
    #[serde(default, with = "binary_only")]
    pub avatar_key: Option<Secret32>,
    #[serde(default, with = "binary_only")]
    pub banner_key: Option<Secret32>,
}

/// 이진 형식(postcard·CBOR)에서만 값을 싣는 필드. JSON에서는 `null`로 내보내고 들어온 값은 무시합니다.
pub(crate) mod binary_only {
    use serde::de::IgnoredAny;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<T: Serialize, S: Serializer>(v: &Option<T>, s: S) -> Result<S::Ok, S::Error> {
        if s.is_human_readable() {
            s.serialize_none()
        } else {
            v.serialize(s)
        }
    }

    pub fn deserialize<'de, T: Deserialize<'de>, D: Deserializer<'de>>(
        d: D,
    ) -> Result<Option<T>, D::Error> {
        if d.is_human_readable() {
            IgnoredAny::deserialize(d)?;
            Ok(None)
        } else {
            Option::<T>::deserialize(d)
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedProfile {
    pub user: UserId,
    pub profile: Profile,
    pub sig: Sig,
}

impl Profile {
    pub fn sign(self, identity: &SigningKeypair) -> Result<SignedProfile, CoreError> {
        let user = UserId(identity.public());
        let bytes = postcard::to_stdvec(&(&user, &self))?;
        let sig = identity.sign(SignDomain::Profile, &bytes);
        Ok(SignedProfile {
            user,
            profile: self,
            sig,
        })
    }
}

impl SignedProfile {
    pub fn verify(&self) -> Result<(), CoreError> {
        let bytes = postcard::to_stdvec(&(&self.user, &self.profile))?;
        verify(self.user.as_bytes(), SignDomain::Profile, &bytes, &self.sig)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn box_public_key_checked_and_encoded() {
        let k = BoxPublicKey::filled(4);
        assert_eq!(k.as_bytes().len(), BoxPublicKey::LEN);
        assert_ne!(k, BoxPublicKey::filled(5));
        // 길이가 맞아도 ML-KEM 공개키로 쓸 수 없는 값, 길이가 다른 값은 받지 않음.
        assert!(BoxPublicKey::from_bytes(&[0xFF; BoxPublicKey::LEN]).is_err());
        assert!(BoxPublicKey::from_bytes(&[7; 32]).is_err());
        let bin = postcard::to_stdvec(&k).unwrap();
        assert_eq!(postcard::from_bytes::<BoxPublicKey>(&bin).unwrap(), k);
        let json = serde_json::to_string(&k).unwrap();
        assert!(json.starts_with('"'));
        assert_eq!(serde_json::from_str::<BoxPublicKey>(&json).unwrap(), k);
        // 받은 기기 목록 안의 키도 풀 때 검사함(앞쪽 ML-KEM 계수를 범위 밖 값으로).
        let mut bad = bin.clone();
        bad[2..42].fill(0xFF);
        assert!(postcard::from_bytes::<BoxPublicKey>(&bad).is_err());
    }

    #[test]
    fn device_list_signature() {
        let id = SigningKeypair::generate();
        let dev = SigningKeypair::generate();
        let list = DeviceList {
            user: UserId(id.public()),
            version: 1,
            devices: vec![DeviceEntry {
                device: DeviceId(dev.public()),
                box_key: BoxPublicKey::filled(7),
                sign_keys: DeviceSignKeys::of(&dev),
                name: "PC".into(),
                added_at: Timestamp(1),
                revoked_at: None,
            }],
            otp: None,
        };
        let signed = list.sign(&id).unwrap();
        signed.verify().unwrap();
        let mut tampered = signed.clone();
        tampered.list.version = 2;
        assert!(tampered.verify().is_err());
    }

    #[test]
    fn entries_must_carry_their_own_sign_keys() {
        let id = SigningKeypair::generate();
        let dev = SigningKeypair::generate();
        let other = SigningKeypair::generate();
        let mut e = DeviceEntry {
            device: DeviceId(dev.public()),
            box_key: BoxPublicKey::filled(7),
            sign_keys: DeviceSignKeys::of(&dev),
            name: "PC".into(),
            added_at: Timestamp(1),
            revoked_at: None,
        };
        assert!(e.keys_match());
        assert_eq!(e.record().device, e.device);
        // 다른 기기의 서명 키를 실은 항목은 구조 검사·서명 확인 모두 거절(서명이 맞아도).
        e.sign_keys = DeviceSignKeys::of(&other);
        assert!(!e.keys_match());
        let l = DeviceList {
            user: UserId(id.public()),
            version: 1,
            devices: vec![e],
            otp: None,
        };
        assert!(!l.well_formed(8));
        assert_eq!(
            l.sign(&id).unwrap().verify(),
            Err(CoreError::InvalidEncoding)
        );
    }

    #[test]
    fn sign_keys_and_records_round_trip() {
        let k = DeviceSignKeys::of(&SigningKeypair::generate());
        let bin = postcard::to_stdvec(&k).unwrap();
        assert_eq!(postcard::from_bytes::<DeviceSignKeys>(&bin).unwrap(), k);
        let json = serde_json::to_string(&k).unwrap();
        assert_eq!(serde_json::from_str::<DeviceSignKeys>(&json).unwrap(), k);
        // 길이가 틀린 키는 풀지 않음.
        let short = serde_json::json!({"ed_pk": "AAAA", "pq_pk": "AAAA"});
        assert!(serde_json::from_value::<DeviceSignKeys>(short).is_err());

        let r = entry(3, Some(9)).record();
        let rb = postcard::to_stdvec(&r).unwrap();
        assert!(rb.len() <= 140, "간추린 기록은 작아야 함: {}", rb.len());
        assert_eq!(postcard::from_bytes::<DeviceRecord>(&rb).unwrap(), r);
        let rj = serde_json::to_string(&r).unwrap();
        assert_eq!(serde_json::from_str::<DeviceRecord>(&rj).unwrap(), r);
    }

    #[test]
    fn key_commit_binds_box_and_sign_keys() {
        let a = entry(1, None);
        let mut swapped = a.clone();
        swapped.box_key = BoxPublicKey::filled(2);
        assert_ne!(a.key_commit(), swapped.key_commit());
        let mut other_sign = a.clone();
        other_sign.sign_keys = DeviceSignKeys::filled(2);
        assert_ne!(a.key_commit(), other_sign.key_commit());
        // 이름·시각은 약속에 들지 않음(같은 키면 같은 표 자리).
        let mut renamed = a.clone();
        renamed.name = "다른 이름".into();
        renamed.revoked_at = Some(Timestamp(3));
        assert_eq!(a.key_commit(), renamed.key_commit());
    }

    /// 서명 키가 모두 `seed`인 기기의 ID(키 ID와 맞음).
    fn did(seed: u8) -> DeviceId {
        DeviceSignKeys::filled(seed).device_id()
    }

    fn entry(seed: u8, revoked: Option<u64>) -> DeviceEntry {
        let sign_keys = DeviceSignKeys::filled(seed);
        DeviceEntry {
            device: sign_keys.device_id(),
            box_key: BoxPublicKey::filled(seed),
            sign_keys,
            name: format!("기기{seed}"),
            added_at: Timestamp(1),
            revoked_at: revoked.map(Timestamp),
        }
    }

    fn list(version: u64, devices: Vec<DeviceEntry>) -> DeviceList {
        DeviceList {
            user: UserId([1; 32]),
            version,
            devices,
            otp: None,
        }
    }

    #[test]
    fn device_lists_only_grow() {
        let old = list(2, vec![entry(1, None), entry(2, Some(5))]);
        // 기기 추가·폐기는 이어받음.
        assert!(list(3, vec![entry(1, None), entry(2, Some(5)), entry(3, None)]).extends(&old));
        assert!(list(3, vec![entry(1, Some(9)), entry(2, Some(5))]).extends(&old));
        // 판이 같거나 낮으면, 기기를 빼거나 폐기를 되돌리거나 상자 키를 바꾸면 아님.
        assert!(!list(2, vec![entry(1, None), entry(2, Some(5))]).extends(&old));
        assert!(!list(3, vec![entry(1, None)]).extends(&old));
        assert!(!list(3, vec![entry(1, None), entry(2, None)]).extends(&old));
        let mut other_key = entry(1, None);
        other_key.box_key = BoxPublicKey::filled(9);
        assert!(!list(3, vec![other_key.clone(), entry(2, Some(5))]).extends(&old));
        let mut other_user = list(3, vec![entry(1, None), entry(2, Some(5))]);
        other_user.user = UserId([2; 32]);
        assert!(!other_user.extends(&old));

        // 공간 상태의 간추린 기록에 비춰도 같은 판정(같은 기기는 같은 키 약속).
        let rec = MemberDevices::from_list(&old);
        assert!(list(3, vec![entry(1, None), entry(2, Some(5)), entry(3, None)]).extends(&rec));
        assert!(!list(2, vec![entry(1, None), entry(2, Some(5))]).extends(&rec));
        assert!(!list(3, vec![entry(1, None), entry(2, None)]).extends(&rec));
        assert!(!list(3, vec![other_key, entry(2, Some(5))]).extends(&rec));
        let mut other_sign = entry(1, None);
        other_sign.sign_keys = DeviceSignKeys::filled(9);
        assert!(!list(3, vec![other_sign, entry(2, Some(5))]).extends(&rec));
    }

    #[test]
    fn well_formed_rejects_oversized_and_duplicate_lists() {
        assert!(list(1, vec![entry(1, None), entry(2, None)]).well_formed(2));
        assert!(!list(1, vec![entry(1, None), entry(2, None)]).well_formed(1));
        assert!(!list(1, vec![entry(1, None), entry(1, Some(3))]).well_formed(8));
        // 같은 기기가 두 번 있으면 이어받기 판단도 흐려지므로 거절합니다.
        let big: Vec<DeviceEntry> = (0..=200u8).map(|i| entry(i, None)).collect();
        assert!(!list(1, big).well_formed(64));
    }

    #[test]
    fn merge_is_deterministic_and_extends_both() {
        // 복구한 기기의 새 목록(판이 시각이라 큼)과 예전 기기의 목록.
        let restored = list(1_000, vec![entry(3, None)]);
        let old = list(7, vec![entry(2, Some(5)), entry(1, None)]);
        let a = restored.merge(&old);
        let b = old.merge(&restored);
        assert_eq!(a, b);
        assert_eq!(a.version, 1_001);
        assert!(a.extends(&restored) && a.extends(&old));
        let ids: Vec<DeviceId> = a.devices.iter().map(|d| d.device).collect();
        let mut want = vec![did(1), did(2), did(3)];
        want.sort();
        assert_eq!(ids, want);
        assert!(a.is_revoked(&did(2)));
        // 같은 기기를 한쪽에서만 폐기했으면 폐기, 둘 다면 이른 시각.
        let x = list(4, vec![entry(1, Some(8))]);
        let y = list(4, vec![entry(1, Some(6))]);
        assert_eq!(x.merge(&y).devices[0].revoked_at, Some(Timestamp(6)));
        assert_eq!(
            x.merge(&list(4, vec![entry(1, None)])).devices[0].revoked_at,
            Some(Timestamp(8))
        );
        // 같은 판에서 이름이 갈려도 어느 쪽에서 합치든 같습니다(다르면 두 기기가 끝없이 다시 합침).
        let named = |name: &str| {
            let mut e = entry(1, None);
            e.name = name.into();
            list(4, vec![e])
        };
        let (p, q) = (named("이 기기"), named("Windows"));
        assert_eq!(p.merge(&q), q.merge(&p));
        assert_eq!(p.merge(&q).devices[0].name, "Windows");
        assert_eq!(p.merge(&named("")).devices[0].name, "");
    }

    #[test]
    fn cosigned_lists_are_classified_by_the_known_list() {
        let id = SigningKeypair::generate();
        let user = UserId(id.public());
        let a = SigningKeypair::generate();
        let r = SigningKeypair::generate();
        let x = SigningKeypair::generate();
        let e = |kp: &SigningKeypair, revoked: Option<u64>| DeviceEntry {
            device: DeviceId(kp.public()),
            box_key: BoxPublicKey::filled(1),
            sign_keys: DeviceSignKeys::of(kp),
            name: "기기".into(),
            added_at: Timestamp(1),
            revoked_at: revoked.map(Timestamp),
        };
        let l = |version, devices| DeviceList {
            user,
            version,
            devices,
            otp: None,
        };
        // 아는 목록: a 활성, r 폐기.
        let known = l(5, vec![e(&a, None), e(&r, Some(4))]);

        // 활성 기기가 만든 판은 바로.
        let by_a = l(6, vec![e(&a, None), e(&r, Some(4)), e(&x, None)])
            .sign_with(&id, &a)
            .unwrap();
        by_a.verify().unwrap();
        assert_eq!(by_a.classify(&known), ListChange::Authorized);
        assert_eq!(by_a.cosigner(), Some(DeviceId(a.public())));

        // 폐기된 기기가 신원 키로 만든 판은 무엇을 담든 거절.
        let by_r = l(6, vec![e(&a, None), e(&r, Some(4)), e(&x, None)])
            .sign_with(&id, &r)
            .unwrap();
        by_r.verify().unwrap();
        assert_eq!(by_r.classify(&known), ListChange::Rejected);
        // 공동 서명이 없어도 거절.
        let bare = l(6, vec![e(&a, None), e(&r, Some(4)), e(&x, None)])
            .sign(&id)
            .unwrap();
        assert_eq!(bare.classify(&known), ListChange::Rejected);

        // 새 기기가 자기만 더한 판은 복구(유예 대상).
        let rec = l(6, vec![e(&a, None), e(&r, Some(4)), e(&x, None)])
            .sign_with(&id, &x)
            .unwrap();
        assert_eq!(
            rec.classify(&known),
            ListChange::Recovery(DeviceId(x.public()))
        );
        // 복구하며 아는 기기를 폐기하거나 다른 기기까지 더하면 거절.
        let rec_revoke = l(6, vec![e(&a, Some(6)), e(&r, Some(4)), e(&x, None)])
            .sign_with(&id, &x)
            .unwrap();
        assert_eq!(rec_revoke.classify(&known), ListChange::Rejected);
        let y = SigningKeypair::generate();
        let rec_more = l(
            6,
            vec![e(&a, None), e(&r, Some(4)), e(&x, None), e(&y, None)],
        )
        .sign_with(&id, &x)
        .unwrap();
        assert_eq!(rec_more.classify(&known), ListChange::Rejected);

        // 복구 판이 아는 기기의 상자 키를 바꾸거나 판을 올리지 않으면(이어받지 않음) 거절.
        let mut swapped = e(&a, None);
        swapped.box_key = BoxPublicKey::filled(9);
        let rec_swap = l(6, vec![swapped, e(&r, Some(4)), e(&x, None)])
            .sign_with(&id, &x)
            .unwrap();
        assert_eq!(rec_swap.classify(&known), ListChange::Rejected);
        let rec_stale = l(5, vec![e(&a, None), e(&r, Some(4)), e(&x, None)])
            .sign_with(&id, &x)
            .unwrap();
        assert_eq!(rec_stale.classify(&known), ListChange::Rejected);

        // 공간 상태의 간추린 기록에 비춰도 판정이 같음.
        let known_rec = MemberDevices::from_list(&known);
        for (s, want) in [
            (&by_a, ListChange::Authorized),
            (&by_r, ListChange::Rejected),
            (&bare, ListChange::Rejected),
            (&rec, ListChange::Recovery(DeviceId(x.public()))),
            (&rec_revoke, ListChange::Rejected),
            (&rec_more, ListChange::Rejected),
            (&rec_swap, ListChange::Rejected),
            (&rec_stale, ListChange::Rejected),
        ] {
            assert_eq!(s.classify(&known_rec), want);
            // 해소기처럼 새 판도 간추려 견줘도 같음.
            let new_rec = MemberDevices::from_list(&s.list);
            assert_eq!(
                new_rec.classify(s.cosigner(), s.otp_sig.is_some(), &known_rec),
                want
            );
            assert_eq!(new_rec.extends(&known_rec), s.list.extends(&known_rec));
        }

        // 공동 서명을 바꿔치면 검증 실패.
        let mut forged = by_r.clone();
        forged.cosig.as_mut().unwrap().device = DeviceId(a.public());
        assert!(forged.verify().is_err());
    }

    /// 해소기는 받은 목록을 간추린 기록으로만 견줍니다. 서명 목록으로 견준 판정과 늘 같아야 합니다(같은 기기 두 번,
    /// 폐기, 키 바꿔치기, 판 섞기를 모두 돌려 봄).
    #[test]
    fn compact_extends_and_classify_match_signed_lists() {
        let mut swapped = entry(2, None);
        swapped.box_key = BoxPublicKey::filled(0xEE);
        let pool = [
            entry(1, None),
            entry(1, Some(3)),
            entry(2, None),
            entry(2, Some(4)),
            swapped,
            entry(3, None),
            entry(3, Some(2)),
        ];
        // 항목 0~3개의 모든 조합(같은 기기 두 번 포함) × 판 1~3.
        let mut lists = Vec::new();
        for n in 0..=3usize {
            let mut idx = vec![0usize; n];
            loop {
                for v in 1..=3 {
                    lists.push(list(v, idx.iter().map(|&i| pool[i].clone()).collect()));
                }
                let mut k = 0;
                while k < n {
                    idx[k] += 1;
                    if idx[k] < pool.len() {
                        break;
                    }
                    idx[k] = 0;
                    k += 1;
                }
                if k == n {
                    break;
                }
            }
        }
        let cosigners = [None, Some(did(1)), Some(did(2)), Some(did(3)), Some(did(9))];
        let mut checked = 0usize;
        for new in lists.iter().step_by(7) {
            let new_rec = MemberDevices::from_list(new);
            for old in lists.iter().step_by(11) {
                let old_rec = MemberDevices::from_list(old);
                assert_eq!(
                    new_rec.extends(&old_rec),
                    new.extends(&old_rec),
                    "{new:?} / {old:?}"
                );
                for c in cosigners {
                    let signed = SignedDeviceList {
                        list: new.clone(),
                        sig: Sig::from_bytes(&[0; Sig::LEN]).unwrap(),
                        cosig: c.map(|device| DeviceCosig {
                            device,
                            sig: Sig::from_bytes(&[0; Sig::LEN]).unwrap(),
                        }),
                        otp_sig: None,
                    };
                    assert_eq!(
                        new_rec.classify(c, false, &old_rec),
                        signed.classify(&old_rec)
                    );
                    checked += 1;
                }
            }
        }
        assert!(checked > 10_000, "{checked}");
    }

    #[test]
    fn heal_keeps_own_list_and_only_revokes_forged_additions() {
        // 내 목록: 1·2 활성, 3 폐기(훔친 기기). 3이 만든 위조 판은 1·2를 폐기하고 2의 상자 키를 바꾸고 9를 더함.
        let cur = list(5, vec![entry(1, None), entry(2, None), entry(3, Some(4))]);
        let mut swapped = entry(2, Some(7));
        swapped.box_key = BoxPublicKey::filled(0xEE);
        let forged = list(
            1_000,
            vec![entry(1, Some(7)), swapped, entry(3, None), entry(9, None)],
        );
        let healed = cur.heal(&forged, Timestamp(8));
        assert_eq!(healed.version, 1_001, "위조 판보다 높은 판");
        assert!(healed.extends(&cur));
        assert!(healed.contains_active(&did(1)));
        assert!(healed.contains_active(&did(2)));
        assert_eq!(
            healed.devices[1].box_key,
            BoxPublicKey::filled(2),
            "상자 키는 내 것 그대로"
        );
        assert!(healed.is_revoked(&did(3)));
        assert_eq!(
            healed.devices.last().map(|d| (d.device, d.revoked_at)),
            Some((did(9), Some(Timestamp(8))))
        );
        // 더하기만 한 위조 판을 받은 피어는 바로잡는 판을 이어받습니다.
        let add_only = list(
            1_000,
            vec![
                entry(1, None),
                entry(2, None),
                entry(3, Some(4)),
                entry(9, None),
            ],
        );
        assert!(cur.heal(&add_only, Timestamp(8)).extends(&add_only));
        // 판이 터무니없이 크면 따라가지 않습니다(그 뒤로 판을 낼 수 없게 되지 않게).
        let huge = list(u64::MAX, vec![entry(9, None)]);
        assert_eq!(cur.heal(&huge, Timestamp(8)).version, 6);
    }

    #[test]
    fn with_entry_adds_or_replaces_one_device() {
        let cur = list(5, vec![entry(1, None), entry(2, None)]);
        let added = cur.with_entry(entry(3, None));
        assert_eq!(added.version, 6);
        assert!(added.extends(&cur));
        assert_eq!(added, cur.with_entry(entry(3, None)), "결정적");
        let revoked = cur.with_entry(entry(2, Some(9)));
        assert!(revoked.extends(&cur) && revoked.is_revoked(&did(2)));
        assert_eq!(revoked.devices.len(), 2);
    }

    #[test]
    fn profile_signature() {
        let id = SigningKeypair::generate();
        let p = Profile {
            version: 3,
            name: "세영".into(),
            ..Default::default()
        }
        .sign(&id)
        .unwrap();
        p.verify().unwrap();
    }

    #[test]
    fn image_keys_only_in_binary_formats() {
        let id = SigningKeypair::generate();
        let p = Profile {
            version: 1,
            name: "세영".into(),
            avatar: Some(BlobId([1; 32])),
            avatar_key: Some(Secret32([9; 32])),
            ..Default::default()
        }
        .sign(&id)
        .unwrap();
        // 이진 형식(서명·저장·전송)에는 키가 실리고 서명도 맞습니다.
        let bin = postcard::to_stdvec(&p).unwrap();
        let back: SignedProfile = postcard::from_bytes(&bin).unwrap();
        assert_eq!(back, p);
        back.verify().unwrap();
        // JSON(로컬 API)에는 키가 나오지 않고, 들어와도 버립니다.
        let v = serde_json::to_value(&p.profile).unwrap();
        assert!(v["avatar_key"].is_null());
        assert!(!v.to_string().contains("9,9,9"));
        let mut v2 = v.clone();
        v2["avatar_key"] = serde_json::json!(vec![7u8; 32]);
        let from_json: Profile = serde_json::from_value(v2).unwrap();
        assert_eq!(from_json.avatar_key, None);
        assert_eq!(from_json.avatar, p.profile.avatar);
    }

    /// OTP 칸을 더하기 전 모양(그때의 derive 그대로).
    #[derive(Serialize, Deserialize)]
    struct OldList {
        user: UserId,
        version: u64,
        devices: Vec<DeviceEntry>,
    }

    #[derive(Serialize, Deserialize)]
    struct OldSigned {
        list: OldList,
        sig: Sig,
        #[serde(default)]
        cosig: Option<DeviceCosig>,
    }

    #[derive(Serialize, Deserialize)]
    struct OldMember {
        version: u64,
        devices: Vec<DeviceRecord>,
    }

    fn signed_entry(kp: &SigningKeypair, revoked: Option<u64>) -> DeviceEntry {
        DeviceEntry {
            device: DeviceId(kp.public()),
            box_key: BoxPublicKey::filled(1),
            sign_keys: DeviceSignKeys::of(kp),
            name: "기기".into(),
            added_at: Timestamp(1),
            revoked_at: revoked.map(Timestamp),
        }
    }

    #[test]
    fn lists_written_before_otp_still_decode_and_verify() {
        let id = SigningKeypair::generate();
        let a = SigningKeypair::generate();
        let b = SigningKeypair::generate();
        let old = OldList {
            user: UserId(id.public()),
            version: 3,
            devices: vec![signed_entry(&a, None), signed_entry(&b, Some(2))],
        };
        let msg = postcard::to_stdvec(&old).unwrap();
        for cosig in [
            None,
            Some(DeviceCosig {
                device: DeviceId(a.public()),
                sig: a.sign(SignDomain::DeviceListCosign, &msg),
            }),
        ] {
            let bytes = postcard::to_stdvec(&OldSigned {
                list: OldList {
                    user: old.user,
                    version: old.version,
                    devices: old.devices.clone(),
                },
                sig: id.sign(SignDomain::DeviceList, &msg),
                cosig,
            })
            .unwrap();
            let back: SignedDeviceList = postcard::from_bytes(&bytes).unwrap();
            assert_eq!(back.list.otp, None);
            back.verify().unwrap();
            // 다시 써도 같은 바이트(크루 상태 이벤트·저장된 목록의 해시가 그대로).
            assert_eq!(postcard::to_stdvec(&back).unwrap(), bytes);
            assert_eq!(postcard::to_stdvec(&back.list).unwrap(), msg);
        }

        let rec = MemberDevices::from_list(&DeviceList {
            user: old.user,
            version: old.version,
            devices: old.devices.clone(),
            otp: None,
        });
        let old_rec = OldMember {
            version: rec.version,
            devices: rec.devices.clone(),
        };
        let old_bytes = postcard::to_stdvec(&Some(&old_rec)).unwrap();
        assert_eq!(postcard::to_stdvec(&Some(&rec)).unwrap(), old_bytes);
        let back: MemberDevices =
            postcard::from_bytes(&postcard::to_stdvec(&old_rec).unwrap()).unwrap();
        assert_eq!(back, rec);
        // JSON도 예전 모양(칸 없음)을 그대로 읽고 씁니다.
        let j = serde_json::to_value(&rec).unwrap();
        assert!(j.get("otp").is_none());
        assert_eq!(serde_json::from_value::<MemberDevices>(j).unwrap(), rec);
    }

    #[test]
    fn otp_key_gates_recovery_and_round_trips() {
        let id = SigningKeypair::generate();
        let a = SigningKeypair::generate();
        let x = SigningKeypair::generate();
        let otp = otp_signing_key(b"20-byte totp secret!");
        assert_eq!(
            otp.public(),
            otp_signing_key(b"20-byte totp secret!").public()
        );
        let l = |version, devices, otp| DeviceList {
            user: UserId(id.public()),
            version,
            devices,
            otp,
        };
        let known = l(5, vec![signed_entry(&a, None)], Some(otp.public()));
        let known_rec = MemberDevices::from_list(&known);

        // 활성 기기는 OTP 키를 켜고 끌 수 있음.
        let off = l(6, vec![signed_entry(&a, None)], None)
            .sign_with(&id, &a)
            .unwrap();
        assert_eq!(off.classify(&known), ListChange::Authorized);

        // OTP 서명 없는 복구 판은 OtpMissing, OTP 키로도 서명하면 Recovery.
        let devices = vec![signed_entry(&a, None), signed_entry(&x, None)];
        let bare = l(6, devices.clone(), Some(otp.public()))
            .sign_with(&id, &x)
            .unwrap();
        bare.verify().unwrap();
        let want_missing = ListChange::OtpMissing(DeviceId(x.public()));
        assert_eq!(bare.classify(&known), want_missing);
        let signed = bare.clone().with_otp_sig(&otp).unwrap();
        signed.verify().unwrap();
        let want = ListChange::Recovery(DeviceId(x.public()));
        assert_eq!(signed.classify(&known), want);
        // 해소기처럼 간추린 기록으로 봐도 같음.
        let rec = MemberDevices::from_list(&signed.list);
        assert_eq!(
            rec.classify(signed.cosigner(), false, &known_rec),
            want_missing
        );
        assert_eq!(rec.classify(signed.cosigner(), true, &known_rec), want);

        // 복구 판은 OTP 설정을 바꾸지 못함(끄기·다른 키).
        let other = otp_signing_key(b"another secret......");
        for changed in [None, Some(other.public())] {
            let s = l(6, devices.clone(), changed).sign_with(&id, &x).unwrap();
            assert_eq!(s.classify(&known), ListChange::Rejected);
        }
        // 다른 키로는 OTP 서명을 붙이지 못하고, 바꿔 끼운 서명은 확인에서 걸림.
        assert!(bare.clone().with_otp_sig(&other).is_err());
        let mut forged = signed.clone();
        forged.otp_sig = Some(other.sign(SignDomain::DeviceListOtp, b"x"));
        assert!(forged.verify().is_err());
        // OTP를 켜지 않은 계정의 복구는 OTP 서명 없이 Recovery.
        let plain_known = l(5, vec![signed_entry(&a, None)], None);
        let plain = l(6, devices, None).sign_with(&id, &x).unwrap();
        assert_eq!(plain.classify(&plain_known), want);

        // 이진·JSON 모두 OTP 칸을 그대로 오갑니다.
        let bin = postcard::to_stdvec(&signed).unwrap();
        let back: SignedDeviceList = postcard::from_bytes(&bin).unwrap();
        assert_eq!(back, signed);
        back.verify().unwrap();
        let json: SignedDeviceList =
            serde_json::from_value(serde_json::to_value(&signed).unwrap()).unwrap();
        assert_eq!(json, signed);
        let rec_back: MemberDevices =
            postcard::from_bytes(&postcard::to_stdvec(&known_rec).unwrap()).unwrap();
        assert_eq!(rec_back, known_rec);
        let rec_json: MemberDevices =
            serde_json::from_value(serde_json::to_value(&known_rec).unwrap()).unwrap();
        assert_eq!(rec_json, known_rec);
    }

    #[test]
    fn merge_and_entry_changes_keep_the_otp_key() {
        let k1 = [1u8; 32];
        let k2 = [2u8; 32];
        let mut a = list(4, vec![entry(1, None)]);
        a.otp = Some(k1);
        let mut b = list(4, vec![entry(2, None)]);
        b.otp = Some(k2);
        // 같은 판이면 어느 쪽에서 합쳐도 같은 키.
        assert_eq!(a.merge(&b).otp, b.merge(&a).otp);
        let mut newer = list(5, vec![entry(2, None)]);
        newer.otp = None;
        assert_eq!(a.merge(&newer).otp, None);
        assert_eq!(a.with_entry(entry(3, None)).otp, Some(k1));
        assert_eq!(a.heal(&b, Timestamp(9)).otp, Some(k1));
    }
}
