//! 다중 수신자 봉인 상자: 특정 기기들만 열 수 있는 데이터.
//!
//! 수신 기기마다 X-Wing(X25519 + ML-KEM-768 혼합 KEM) 캡슐화 → HKDF → 내용 키(cek) 감싸기, 본문은 cek로
//! XChaCha20-Poly1305. 둘 중 하나만 버텨도 내용 키가 드러나지 않으므로, 지금 모아 둔 봉투를 나중에 양자 컴퓨터로
//! (X25519를 깨서) 열 수 없습니다.

use rand::RngCore;
use serde::{Deserialize, Serialize};
use x_wing::{
    DecapsulationKeyRejectNonContrib, Decapsulator, Encapsulate, EncapsulationKey, KeyExport,
    TryDecapsulate,
};
use yangpa_core::body::Secret32;
use yangpa_core::device::BoxPublicKey;
use yangpa_core::ids::DeviceId;
use zeroize::{Zeroize, Zeroizing};

use crate::aead;
use crate::error::{CryptoError, Result};
use crate::kdf::derive;

/// 봉투 형식 판. 1판(X25519만)은 더는 열지 않습니다. 수신자 목록은 암호 없이 읽을 수 있어야 해서(해소기가 직접
/// 인코딩 수신자를 검사) 형식 판과 읽는 법은 `yangpa_core::keyupdate`에 둡니다.
const ENVELOPE_VERSION: u8 = yangpa_core::keyupdate::SEALED_ENVELOPE_VERSION;

/// 기기 상자 키(X-Wing 비밀 시드 32바이트). 해제 시 0으로 덮어씁니다.
pub struct BoxKeypair {
    seed: Zeroizing<[u8; 32]>,
    dk: DecapsulationKeyRejectNonContrib,
    public: BoxPublicKey,
}

impl BoxKeypair {
    pub fn generate() -> Self {
        let mut seed = Zeroizing::new([0u8; 32]);
        rand::rngs::OsRng.fill_bytes(&mut seed[..]);
        Self::from_bytes(*seed)
    }

    /// 시드에서 키 쌍을 펼칩니다. 같은 시드는 늘 같은 키입니다.
    pub fn from_bytes(mut bytes: [u8; 32]) -> Self {
        let seed = Zeroizing::new(bytes);
        bytes.zeroize();
        let dk = DecapsulationKeyRejectNonContrib::from(*seed);
        let public = BoxPublicKey::from_bytes(&dk.encapsulation_key().to_bytes())
            .expect("X-Wing 캡슐화 키 길이는 고정");
        BoxKeypair { seed, dk, public }
    }

    pub fn to_bytes(&self) -> Zeroizing<[u8; 32]> {
        self.seed.clone()
    }

    pub fn public(&self) -> BoxPublicKey {
        self.public.clone()
    }
}

impl Clone for BoxKeypair {
    fn clone(&self) -> Self {
        BoxKeypair::from_bytes(*self.seed)
    }
}

impl std::fmt::Debug for BoxKeypair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "BoxKeypair(pub={:?})", self.public)
    }
}

#[derive(Serialize, Deserialize)]
struct Wrap {
    device: DeviceId,
    /// X-Wing 캡슐문(1120바이트).
    #[serde(with = "serde_bytes")]
    kem_ct: Vec<u8>,
    #[serde(with = "serde_bytes")]
    wrapped: Vec<u8>,
}

#[derive(Serialize, Deserialize)]
struct Envelope {
    v: u8,
    wraps: Vec<Wrap>,
    #[serde(with = "serde_bytes")]
    ct: Vec<u8>,
}

fn kek(shared: &[u8], device: &DeviceId) -> Secret32 {
    derive(shared, device.as_bytes(), b"yangpa/v2/seal")
}

/// 수신 기기들(기기 ID, 상자 공개키)에게 봉인합니다. 수신자는 1명 이상이어야 합니다.
pub fn seal_to(recipients: &[(DeviceId, BoxPublicKey)], ad: &[u8], plaintext: &[u8]) -> Vec<u8> {
    let cek = Secret32::random();
    let wraps = recipients
        .iter()
        .map(|(device, pk)| {
            let ek = EncapsulationKey::try_from(pk.as_bytes())
                .expect("BoxPublicKey는 만들 때 키로 쓸 수 있는지 검사함");
            let (kem_ct, mut shared) = ek.encapsulate();
            let k = kek(&shared, device);
            shared.zeroize();
            Wrap {
                device: *device,
                kem_ct: kem_ct.to_vec(),
                wrapped: aead::seal(&k, device.as_bytes(), &cek.0),
            }
        })
        .collect();
    let ct = aead::seal(&cek, ad, plaintext);
    let env = Envelope {
        v: ENVELOPE_VERSION,
        wraps,
        ct,
    };
    postcard::to_stdvec(&env).expect("봉투 직렬화 실패 없음")
}

fn parse(envelope: &[u8]) -> Result<Envelope> {
    // 판이 다르면 구조도 다르므로 첫 바이트(postcard u8)부터 봅니다.
    if envelope.first() != Some(&ENVELOPE_VERSION) {
        return Err(CryptoError::Malformed);
    }
    postcard::from_bytes(envelope).map_err(|_| CryptoError::Malformed)
}

/// 봉투에 들어 있는 수신 기기 목록(복호화 없이). 해소기와 같은 답을 내도록 공용 크레이트의 읽기를 그대로 씁니다.
pub fn sealed_recipients(envelope: &[u8]) -> Result<Vec<DeviceId>> {
    yangpa_core::keyupdate::sealed_recipients(envelope).map_err(|_| CryptoError::Malformed)
}

pub fn open_sealed(
    me: &DeviceId,
    kp: &BoxKeypair,
    ad: &[u8],
    envelope: &[u8],
) -> Result<Zeroizing<Vec<u8>>> {
    let env = parse(envelope)?;
    let wrap = env
        .wraps
        .iter()
        .find(|w| w.device == *me)
        .ok_or(CryptoError::NotARecipient)?;
    let kem_ct =
        x_wing::Ciphertext::try_from(wrap.kem_ct.as_slice()).map_err(|_| CryptoError::Malformed)?;
    let mut shared = kp
        .dk
        .try_decapsulate(&kem_ct)
        .map_err(|_| CryptoError::Decrypt)?;
    let k = kek(&shared, me);
    shared.zeroize();
    let cek_bytes = aead::open(&k, me.as_bytes(), &wrap.wrapped)?;
    let cek = Secret32(
        cek_bytes
            .as_slice()
            .try_into()
            .map_err(|_| CryptoError::Malformed)?,
    );
    aead::open(&cek, ad, &env.ct)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multi_recipient() {
        let a = BoxKeypair::generate();
        let b = BoxKeypair::generate();
        let c = BoxKeypair::generate();
        let (da, db, dc) = (DeviceId::random(), DeviceId::random(), DeviceId::random());
        let env = seal_to(
            &[(da, a.public()), (db, b.public())],
            b"ctx",
            b"secret keys",
        );
        assert_eq!(
            &**open_sealed(&da, &a, b"ctx", &env).unwrap(),
            b"secret keys"
        );
        assert_eq!(
            &**open_sealed(&db, &b, b"ctx", &env).unwrap(),
            b"secret keys"
        );
        assert!(matches!(
            open_sealed(&dc, &c, b"ctx", &env),
            Err(CryptoError::NotARecipient)
        ));
        // 다른 기기 ID로 사칭해도 키가 맞지 않음.
        assert!(open_sealed(&da, &c, b"ctx", &env).is_err());
        assert!(open_sealed(&da, &a, b"other", &env).is_err());
        assert_eq!(sealed_recipients(&env).unwrap(), vec![da, db]);
        // 해소기 쪽 읽기(공용 크레이트)와 같은 답, 같은 거부.
        let parsed: Vec<DeviceId> = parse(&env)
            .unwrap()
            .wraps
            .iter()
            .map(|w| w.device)
            .collect();
        assert_eq!(parsed, vec![da, db]);
        assert_eq!(
            yangpa_core::keyupdate::SealedBox(env.clone())
                .recipients()
                .unwrap(),
            parsed
        );
        let mut cut = env.clone();
        cut.truncate(env.len() / 2);
        assert!(sealed_recipients(&cut).is_err() && parse(&cut).is_err());
    }

    #[test]
    fn seed_roundtrip_and_bad_keys() {
        let a = BoxKeypair::generate();
        let again = BoxKeypair::from_bytes(*a.to_bytes());
        assert_eq!(a.public(), again.public());
        assert_eq!(a.public().as_bytes().len(), BoxPublicKey::LEN);
        let d = DeviceId::random();
        let env = seal_to(&[(d, a.public())], b"", b"x");
        assert_eq!(&**open_sealed(&d, &again, b"", &env).unwrap(), b"x");
        // 길이는 맞지만 ML-KEM 공개키가 아닌 값은 상자 키로 받지 않음.
        assert!(BoxPublicKey::from_bytes(&[0xFF; BoxPublicKey::LEN]).is_err());
        assert!(BoxPublicKey::from_bytes(&a.public().as_bytes()[1..]).is_err());
        // 1판 봉투(X25519만)는 열지 않음.
        let mut old = env.clone();
        old[0] = 1;
        assert!(matches!(
            open_sealed(&d, &a, b"", &old),
            Err(CryptoError::Malformed)
        ));
    }
}
