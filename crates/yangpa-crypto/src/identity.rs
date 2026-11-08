//! 신원 키·기기 키, 안전 번호. 신원 시드 하나에서 Ed25519 키와 FN-DSA 키를 모두 만듭니다
//! (`sign::SigningKeypair::from_seed`).

use yangpa_core::device::BoxPublicKey;
use yangpa_core::ids::{DeviceId, UserId};
use yangpa_core::sign::SigningKeypair;
use zeroize::Zeroizing;

use crate::error::{CryptoError, Result};
use crate::sealed::BoxKeypair;

/// 사용자 신원 키(Ed25519 + FN-DSA 혼합). 사용자 ID = 두 공개키를 묶은 키 ID.
#[derive(Clone, Debug)]
pub struct Identity {
    pub signing: SigningKeypair,
}

impl Identity {
    pub fn generate() -> Self {
        Identity {
            signing: SigningKeypair::generate(),
        }
    }

    pub fn from_seed(seed: &[u8; 32]) -> Self {
        Identity {
            signing: SigningKeypair::from_seed(seed),
        }
    }

    pub fn user_id(&self) -> UserId {
        UserId(self.signing.public())
    }
}

/// 기기 키: 이벤트 서명용 혼합 서명 키(Ed25519 + FN-DSA)와 봉인 상자용
/// X-Wing(X25519 + ML-KEM-768) 시드.
#[derive(Clone, Debug)]
pub struct DeviceKeys {
    pub signing: SigningKeypair,
    pub boxkey: BoxKeypair,
}

/// 2판: 상자 키 32바이트가 X25519 비밀키가 아니라 X-Wing 시드이고, 서명 시드에서 FN-DSA 키도 만듭니다(기기 ID가
/// 두 공개키를 묶은 키 ID). 1판 데이터는 기기 목록·그룹 키도 예전 암호라 이어 쓸 수 없습니다([`CryptoError::PreQuantumData`]).
const DEVICE_KEYS_VERSION: u8 = 2;
const DEVICE_KEYS_LEN: usize = 1 + 32 + 32;

impl DeviceKeys {
    pub fn generate() -> Self {
        DeviceKeys {
            signing: SigningKeypair::generate(),
            boxkey: BoxKeypair::generate(),
        }
    }

    /// 기기 ID = 기기 서명 키 ID(Ed25519·FN-DSA 공개키를 묶은 해시).
    pub fn device_id(&self) -> DeviceId {
        DeviceId(self.signing.public())
    }

    /// 봉인 상자 공개키(기기 목록에 싣는 값).
    pub fn box_public(&self) -> BoxPublicKey {
        self.boxkey.public()
    }

    /// 저장용 직렬화: 버전(2) || 서명 시드(32) || 상자 시드(32).
    pub fn to_bytes(&self) -> Zeroizing<Vec<u8>> {
        let mut out = Zeroizing::new(Vec::with_capacity(DEVICE_KEYS_LEN));
        out.push(DEVICE_KEYS_VERSION);
        out.extend_from_slice(&self.signing.seed()[..]);
        out.extend_from_slice(&self.boxkey.to_bytes()[..]);
        out
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.len() == DEVICE_KEYS_LEN && bytes[0] == 1 {
            return Err(CryptoError::PreQuantumData);
        }
        if bytes.len() != DEVICE_KEYS_LEN || bytes[0] != DEVICE_KEYS_VERSION {
            return Err(CryptoError::Malformed);
        }
        let seed: Zeroizing<[u8; 32]> = Zeroizing::new(
            bytes[1..33]
                .try_into()
                .map_err(|_| CryptoError::Malformed)?,
        );
        let boxk: Zeroizing<[u8; 32]> = Zeroizing::new(
            bytes[33..65]
                .try_into()
                .map_err(|_| CryptoError::Malformed)?,
        );
        Ok(DeviceKeys {
            signing: SigningKeypair::from_seed(&seed),
            boxkey: BoxKeypair::from_bytes(*boxk),
        })
    }
}

/// 안전 번호: 두 신원 공개키를 정렬해 이어 붙인 BLAKE3 해시의 앞 30바이트를 5자리 숫자 12묶음으로.
/// 인자 순서와 상관없이 양쪽 화면에 같은 값이 나옵니다.
pub fn safety_number(a: &UserId, b: &UserId) -> String {
    let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
    let mut h = blake3::Hasher::new();
    h.update(lo.as_bytes());
    h.update(hi.as_bytes());
    let digest = h.finalize();
    let bytes = &digest.as_bytes()[..30];
    // 5바이트(40비트)마다 10자리 수(= 5자리 2묶음). 2^40 ≈ 1.1×10^12라 치우침은 무시할 만함.
    let mut groups = Vec::with_capacity(12);
    for chunk in bytes.chunks(5) {
        let mut v = 0u64;
        for b in chunk {
            v = (v << 8) | *b as u64;
        }
        let v = v % 10_000_000_000;
        groups.push(format!("{:05}", v / 100_000));
        groups.push(format!("{:05}", v % 100_000));
    }
    groups.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use yangpa_core::sign::{SignDomain, verify};

    #[test]
    fn device_keys_roundtrip() {
        let dk = DeviceKeys::generate();
        let bytes = dk.to_bytes();
        assert_eq!(bytes.len(), DEVICE_KEYS_LEN);
        let back = DeviceKeys::from_bytes(&bytes).unwrap();
        assert_eq!(back.device_id(), dk.device_id());
        assert_eq!(back.box_public(), dk.box_public());
        let sig = back.signing.sign(SignDomain::Event, b"m");
        verify(dk.device_id().as_bytes(), SignDomain::Event, b"m", &sig).unwrap();
        assert!(DeviceKeys::from_bytes(&bytes[..64]).is_err());
        let mut bad = bytes.to_vec();
        bad[0] = 3;
        assert!(DeviceKeys::from_bytes(&bad).is_err());
        // 양자 내성 전환 전(1판) 기기 키는 따로 알립니다.
        bad[0] = 1;
        assert!(matches!(
            DeviceKeys::from_bytes(&bad),
            Err(CryptoError::PreQuantumData)
        ));
        // Debug 출력에 비밀값이 나오지 않음.
        let dbg = format!("{dk:?}");
        assert!(!dbg.contains(&hex::encode(&dk.signing.seed()[..])));
    }

    #[test]
    fn safety_number_symmetric_and_formatted() {
        let a = Identity::generate().user_id();
        let b = Identity::generate().user_id();
        let s = safety_number(&a, &b);
        assert_eq!(s, safety_number(&b, &a));
        let groups: Vec<&str> = s.split(' ').collect();
        assert_eq!(groups.len(), 12);
        assert!(
            groups
                .iter()
                .all(|g| g.len() == 5 && g.bytes().all(|c| c.is_ascii_digit()))
        );
        let c = Identity::generate().user_id();
        assert_ne!(s, safety_number(&a, &c));
    }

    #[test]
    fn safety_number_known_value() {
        // 고정 입력에 대한 값이 바뀌지 않는지(양쪽 구현이 같은 표기를 쓰는지) 확인.
        let a = UserId([1; 32]);
        let b = UserId([2; 32]);
        let mut h = blake3::Hasher::new();
        h.update(&[1; 32]);
        h.update(&[2; 32]);
        let d = h.finalize();
        let first = d.as_bytes()[..5]
            .iter()
            .fold(0u64, |v, b| (v << 8) | *b as u64)
            % 10_000_000_000;
        let s = safety_number(&b, &a);
        assert_eq!(
            &s[..11],
            format!("{:05} {:05}", first / 100_000, first % 100_000)
        );
    }
}
