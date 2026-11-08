//! 혼합 서명(Ed25519 + FN-DSA-512). 서명 대상마다 도메인 분리 문자열을 앞에 붙입니다.
//!
//! - 키 ID(사용자·기기 ID) = 두 공개키를 묶은 해시([`key_id`]). 보통은 서명마다 두 공개키를 함께 실어, ID만 알면
//!   누구나 검증할 수 있습니다(받는 쪽이 기기 목록에서 키를 찾지 않아도 됨).
//! - 공개키를 뺀 **짧은 서명**([`Sig::SLIM_LEN`], 730B)도 있습니다. 채널 스트림 이벤트처럼 작성 기기가 멤버의 기기라
//!   받는 쪽이 키를 이미 아는 곳에서 씁니다([`verify_with_keys`]). 키 묶음은 키 ID로 스스로 확인되므로 어디서 받아도 됩니다.
//! - 두 서명이 모두 맞아야 통과합니다. 양자 컴퓨터로 Ed25519를 깨도 FN-DSA 서명을 위조할 수 없고, FN-DSA에 아직
//!   모르는 약점이 있어도 Ed25519가 버팁니다.
//! - FN-DSA 키는 32바이트 시드에서 결정적으로 만듭니다(시드 하나가 신원 키 전체를 되살리게). FN-DSA 서명은
//!   매번 새 난수를 씁니다(같은 내용을 다시 서명해도 서명이 다름).

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use fn_dsa::{
    DOMAIN_NONE, FN_DSA_LOGN_512, HASH_ID_RAW, KeyPairGenerator, KeyPairGeneratorStandard,
    SigningKey as _, SigningKeyStandard, VerifyingKey as _, VerifyingKeyStandard, sign_key_size,
    signature_size, vrfy_key_size,
};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::error::CoreError;

/// 서명 도메인. 같은 키로 만든 서명이 다른 용도로 재사용되지 않게 합니다.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignDomain {
    Event,
    DeviceList,
    DeviceCert,
    Profile,
    PeerAuth,
    LinkRedeem,
    Locator,
    Backup,
    /// 초대 토큰에서 만든 키로 하는 가입 증명(`body::InviteProof`).
    InviteProof,
    /// 기기 목록 공동 서명: 새 판을 만든 기기가 기기 키로 함께 서명합니다(`device::DeviceCosig`).
    DeviceListCosign,
    /// 초대로 들어오는 기기가 가입 증명에 기기 키로 하는 서명(`body::InviteProof::joiner`).
    InviteJoiner,
    /// 보관 노드에 보내는 멤버 명단(`archive::SignedArchiveRoster`).
    ArchiveRoster,
    /// 확정 기기가 다음 확정 기기에게 넘기는 키 그룹 표(`yangpa_crypto::keytree::seal_handover`).
    KeyTable,
    /// 소유자 신원이 상태 체크포인트 본문에 하는 서명(`checkpoint::SignedCheckpoint::owner_sig`).
    Checkpoint,
    /// 체크포인트를 쓴 기기가 같은 본문에 기기 키로 하는 서명(`checkpoint::SignedCheckpoint::device_sig`).
    CheckpointDevice,
    /// 소유자가 다른 기기에게 체크포인트 작성을 맡기는 인증서(`checkpoint::DelegationCert`).
    CheckpointDelegation,
    /// 소유권 이전 인증서(`checkpoint::OwnershipCert`): 앞 소유자 신원과 이전한 기기가 함께 서명.
    OwnershipCert,
    /// 보관 노드 명단 차이(`archive::SignedArchiveRosterDelta`).
    ArchiveRosterDelta,
    /// 옛 복구 판에 OTP 키가 한 서명(`device::SignedDeviceList::otp_sig`).
    DeviceListOtp,
    /// 뱃지 카드(`badge::SignedBadgeCard`).
    Badges,
    /// 개발자 계정의 원년멤버 증서(`badge::FounderGrant`).
    FounderGrant,
}

impl SignDomain {
    pub const fn tag(self) -> &'static [u8] {
        match self {
            SignDomain::Event => b"yangpa/v2/event\0",
            SignDomain::DeviceList => b"yangpa/v2/device-list\0",
            SignDomain::DeviceCert => b"yangpa/v2/device-cert\0",
            SignDomain::Profile => b"yangpa/v2/profile\0",
            SignDomain::PeerAuth => b"yangpa/v2/peer-auth\0",
            SignDomain::LinkRedeem => b"yangpa/v2/link-redeem\0",
            SignDomain::Locator => b"yangpa/v2/locator\0",
            SignDomain::Backup => b"yangpa/v2/backup\0",
            SignDomain::InviteProof => b"yangpa/v2/invite-proof\0",
            SignDomain::DeviceListCosign => b"yangpa/v2/device-list-cosign\0",
            SignDomain::InviteJoiner => b"yangpa/v2/invite-joiner\0",
            SignDomain::ArchiveRoster => b"yangpa/v2/archive-roster\0",
            SignDomain::KeyTable => b"yangpa/v3/key-table\0",
            SignDomain::Checkpoint => b"yangpa/v3/checkpoint\0",
            SignDomain::CheckpointDevice => b"yangpa/v3/checkpoint-device\0",
            SignDomain::CheckpointDelegation => b"yangpa/v3/checkpoint-delegation\0",
            SignDomain::OwnershipCert => b"yangpa/v3/ownership-cert\0",
            SignDomain::ArchiveRosterDelta => b"yangpa/v3/archive-roster-delta\0",
            SignDomain::DeviceListOtp => b"yangpa/v3/device-list-otp\0",
            SignDomain::Badges => b"yangpa/v3/badges\0",
            SignDomain::FounderGrant => b"yangpa/v3/founder-grant\0",
        }
    }

    /// 실제로 서명하는 바이트: 도메인 || 키 ID || 내용. 두 서명이 같은 키 ID에 묶여, 한쪽 서명만 떼어 다른 키 묶음에
    /// 붙일 수 없습니다.
    fn message(self, id: &[u8; 32], msg: &[u8]) -> Vec<u8> {
        let mut m = Vec::with_capacity(self.tag().len() + 32 + msg.len());
        m.extend_from_slice(self.tag());
        m.extend_from_slice(id);
        m.extend_from_slice(msg);
        m
    }
}

const LOGN: u32 = FN_DSA_LOGN_512;
/// Ed25519 공개키 길이.
pub const ED_PUBLIC_LEN: usize = 32;
/// FN-DSA-512 공개키 길이.
pub const PQ_PUBLIC_LEN: usize = vrfy_key_size(LOGN);
const PQ_SECRET_LEN: usize = sign_key_size(LOGN);
const ED_SIG_LEN: usize = 64;
const PQ_SIG_LEN: usize = signature_size(LOGN);

/// 두 공개키(FN-DSA는 해시)를 묶은 키 ID. FN-DSA 공개키 전체를 싣기 아까운 곳(간추린 기기 기록 등)에서는 해시만
/// 싣고 이 함수로 ID를 다시 계산합니다.
pub fn key_id(ed_public: &[u8; 32], pq_public_hash: &[u8; 32]) -> [u8; 32] {
    let mut h = blake3::Hasher::new_derive_key("yangpa/v2/sign-key-id");
    h.update(ed_public);
    h.update(pq_public_hash);
    *h.finalize().as_bytes()
}

/// FN-DSA 공개키 해시([`key_id`]의 두 번째 인자).
pub fn pq_public_hash(pq_public: &[u8]) -> [u8; 32] {
    *blake3::hash(pq_public).as_bytes()
}

/// 혼합 서명: Ed25519 공개키(32) || FN-DSA 공개키(897) || Ed25519 서명(64) || FN-DSA 서명(666).
///
/// 공개키를 함께 실으면 [`verify`]가 공개키 묶음이 주어진 ID와 맞는지부터 봅니다. 짧은 서명(Ed25519 서명 || FN-DSA
/// 서명)은 공개키가 없어 [`verify_with_keys`]로만 검증합니다. 두 길이는 겹치지 않아 길이로 가립니다.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Sig(Box<[u8]>);

impl Sig {
    pub const LEN: usize = ED_PUBLIC_LEN + PQ_PUBLIC_LEN + ED_SIG_LEN + PQ_SIG_LEN;
    /// 짧은 서명(공개키 없음) 길이.
    pub const SLIM_LEN: usize = ED_SIG_LEN + PQ_SIG_LEN;

    /// 길이만 확인합니다(내용은 [`verify`]·[`verify_with_keys`]가 확인).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, CoreError> {
        if bytes.len() != Self::LEN && bytes.len() != Self::SLIM_LEN {
            return Err(CoreError::InvalidLength {
                expected: Self::LEN,
                got: bytes.len(),
            });
        }
        Ok(Sig(bytes.into()))
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// 공개키를 싣지 않은 짧은 서명인지.
    pub fn is_slim(&self) -> bool {
        self.0.len() == Self::SLIM_LEN
    }

    /// 실린 (Ed25519 공개키, FN-DSA 공개키). 짧은 서명이면 `None`.
    fn publics(&self) -> Option<(&[u8; ED_PUBLIC_LEN], &[u8])> {
        if self.is_slim() {
            return None;
        }
        let ed = self.0[..ED_PUBLIC_LEN].try_into().expect("고정 길이");
        Some((ed, &self.0[ED_PUBLIC_LEN..ED_PUBLIC_LEN + PQ_PUBLIC_LEN]))
    }

    fn ed_sig(&self) -> &[u8; ED_SIG_LEN] {
        let at = self.0.len() - PQ_SIG_LEN - ED_SIG_LEN;
        self.0[at..at + ED_SIG_LEN].try_into().expect("고정 길이")
    }

    /// FN-DSA 서명 부분.
    pub fn pq_sig(&self) -> &[u8] {
        &self.0[self.0.len() - PQ_SIG_LEN..]
    }

    /// 서명을 가리키는 값(FN-DSA 서명 부분의 해시). 이벤트 ID·재사용 검사에 씁니다. Ed25519 부분은 양자 컴퓨터가
    /// 있으면 다른 값으로 바꿔 끼울 수 있어 넣지 않습니다(같은 서명을 다른 값으로 둔갑시키지 못하게).
    pub fn fingerprint(&self) -> [u8; 32] {
        *blake3::hash(self.pq_sig()).as_bytes()
    }

    /// 이 서명을 만든 키의 ID(실린 공개키로 계산). 짧은 서명이면 `None`.
    pub fn signer(&self) -> Option<[u8; 32]> {
        self.publics()
            .map(|(ed, pq)| key_id(ed, &pq_public_hash(pq)))
    }
}

impl std::fmt::Debug for Sig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Sig({})", hex::encode(&self.pq_sig()[..4]))
    }
}

/// 이진 형식에서는 바이트 그대로, JSON에서는 base64 문자열.
impl Serialize for Sig {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        if s.is_human_readable() {
            use base64::Engine;
            s.serialize_str(&base64::engine::general_purpose::STANDARD.encode(&self.0))
        } else {
            serde_bytes::Bytes::new(&self.0).serialize(s)
        }
    }
}

impl<'de> Deserialize<'de> for Sig {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let v: Vec<u8> = if d.is_human_readable() {
            use base64::Engine;
            let s = String::deserialize(d)?;
            base64::engine::general_purpose::STANDARD
                .decode(s)
                .map_err(D::Error::custom)?
        } else {
            serde_bytes::ByteBuf::deserialize(d)?.into_vec()
        };
        Sig::from_bytes(&v).map_err(|_| D::Error::custom("서명 길이 오류"))
    }
}

/// 시드에서 FN-DSA 키를 만들 때 쓰는 결정적 난수원(SHAKE256).
struct SeedRng(fn_dsa::SHAKE256);

impl SeedRng {
    fn new(seed: &[u8; 32]) -> Self {
        let mut sh = fn_dsa::SHAKE256::new();
        sh.inject(&Zeroizing::new(blake3::derive_key("yangpa/v2/fn-dsa-keygen", seed))[..]);
        sh.flip();
        SeedRng(sh)
    }
}

impl fn_dsa::RngCore for SeedRng {
    fn next_u32(&mut self) -> u32 {
        let mut b = [0u8; 4];
        self.0.extract(&mut b);
        u32::from_le_bytes(b)
    }

    fn next_u64(&mut self) -> u64 {
        let mut b = [0u8; 8];
        self.0.extract(&mut b);
        u64::from_le_bytes(b)
    }

    fn fill_bytes(&mut self, dest: &mut [u8]) {
        self.0.extract(dest);
    }

    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), fn_dsa::RngError> {
        self.0.extract(dest);
        Ok(())
    }
}

impl fn_dsa::CryptoRng for SeedRng {}

/// 서명 키 쌍(신원 키·기기 키 공용). 32바이트 시드 하나에서 Ed25519 키와 FN-DSA 키를 모두 만듭니다. 비밀값은
/// 메모리에서 해제 시 0으로 덮어씁니다.
pub struct SigningKeypair {
    seed: Zeroizing<[u8; 32]>,
    ed: SigningKey,
    /// 인코딩한 FN-DSA 비밀키. 서명할 때마다 풉니다(푼 상태는 수십 KB라 들고 있지 않음).
    pq: Zeroizing<Vec<u8>>,
    pq_public: Box<[u8]>,
    id: [u8; 32],
}

impl SigningKeypair {
    pub fn generate() -> Self {
        let mut seed = Zeroizing::new([0u8; 32]);
        rand::rngs::OsRng.fill_bytes(&mut seed[..]);
        Self::from_seed(&seed)
    }

    /// 시드에서 키 쌍을 만듭니다. 같은 시드는 늘 같은 키(같은 ID)입니다. FN-DSA 키 생성에 수 ms가 걸립니다.
    pub fn from_seed(seed: &[u8; 32]) -> Self {
        let ed = SigningKey::from_bytes(seed);
        let mut pq = Zeroizing::new(vec![0u8; PQ_SECRET_LEN]);
        let mut pq_public = vec![0u8; PQ_PUBLIC_LEN];
        KeyPairGeneratorStandard::default().keygen(
            LOGN,
            &mut SeedRng::new(seed),
            &mut pq,
            &mut pq_public,
        );
        let id = key_id(&ed.verifying_key().to_bytes(), &pq_public_hash(&pq_public));
        SigningKeypair {
            seed: Zeroizing::new(*seed),
            ed,
            pq,
            pq_public: pq_public.into(),
            id,
        }
    }

    /// 비밀 시드(32바이트). 호출자가 안전하게 다뤄야 합니다.
    pub fn seed(&self) -> Zeroizing<[u8; 32]> {
        self.seed.clone()
    }

    /// 키 ID(사용자·기기 ID로 쓰는 값).
    pub fn public(&self) -> [u8; 32] {
        self.id
    }

    /// Ed25519 공개키.
    pub fn ed_public(&self) -> [u8; 32] {
        self.ed.verifying_key().to_bytes()
    }

    /// FN-DSA 공개키 해시([`key_id`]로 키 ID를 다시 계산할 때 씀).
    pub fn pq_public_hash(&self) -> [u8; 32] {
        pq_public_hash(&self.pq_public)
    }

    /// FN-DSA 공개키(기기 목록 항목에 실음, `device::DeviceSignKeys`).
    pub fn pq_public(&self) -> &[u8] {
        &self.pq_public
    }

    pub fn sign(&self, domain: SignDomain, msg: &[u8]) -> Sig {
        self.sign_as(domain, msg, false)
    }

    /// 공개키를 싣지 않은 짧은 서명([`Sig::SLIM_LEN`]). 받는 쪽이 키를 알아야 검증합니다([`verify_with_keys`]).
    pub fn sign_slim(&self, domain: SignDomain, msg: &[u8]) -> Sig {
        self.sign_as(domain, msg, true)
    }

    fn sign_as(&self, domain: SignDomain, msg: &[u8], slim: bool) -> Sig {
        let m = domain.message(&self.id, msg);
        let mut out = Vec::with_capacity(Sig::LEN);
        if !slim {
            out.extend_from_slice(&self.ed_public());
            out.extend_from_slice(&self.pq_public);
        }
        out.extend_from_slice(&self.ed.sign(&m).to_bytes());
        let mut pq_sig = [0u8; PQ_SIG_LEN];
        SigningKeyStandard::decode(&self.pq)
            .expect("시드에서 만든 FN-DSA 비밀키")
            .sign(
                &mut rand::rngs::OsRng,
                &DOMAIN_NONE,
                &HASH_ID_RAW,
                &m,
                &mut pq_sig,
            )
            .expect("올바른 FN-DSA 비밀키로는 서명이 실패하지 않음");
        out.extend_from_slice(&pq_sig);
        Sig(out.into())
    }
}

impl Clone for SigningKeypair {
    fn clone(&self) -> Self {
        SigningKeypair {
            seed: self.seed.clone(),
            ed: SigningKey::from_bytes(&self.seed),
            pq: self.pq.clone(),
            pq_public: self.pq_public.clone(),
            id: self.id,
        }
    }
}

impl std::fmt::Debug for SigningKeypair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SigningKeypair(id={})", hex::encode(&self.id[..4]))
    }
}

/// 키 ID로 서명을 검증합니다: 서명에 실린 공개키 묶음이 `id`의 것이고, Ed25519·FN-DSA 서명이 모두 맞아야 합니다.
/// 짧은 서명은 키가 없어 늘 거절합니다([`verify_with_keys`]를 씀).
pub fn verify(id: &[u8; 32], domain: SignDomain, msg: &[u8], sig: &Sig) -> Result<(), CoreError> {
    let (ed, pq) = sig.publics().ok_or(CoreError::BadSignature)?;
    verify_with_keys(id, ed, pq, domain, msg, sig)
}

/// 따로 받은 공개키로 서명(짧든 길든)을 검증합니다. 키 묶음이 `id`의 것이어야 하고, 긴 서명이면 실린 공개키도 같아야
/// 합니다.
pub fn verify_with_keys(
    id: &[u8; 32],
    ed_pk: &[u8; 32],
    pq_pk: &[u8],
    domain: SignDomain,
    msg: &[u8],
    sig: &Sig,
) -> Result<(), CoreError> {
    if key_id(ed_pk, &pq_public_hash(pq_pk)) != *id {
        return Err(CoreError::BadSignature);
    }
    if let Some((e, p)) = sig.publics()
        && (e != ed_pk || p != pq_pk)
    {
        return Err(CoreError::BadSignature);
    }
    let m = domain.message(id, msg);
    let vk = VerifyingKey::from_bytes(ed_pk).map_err(|_| CoreError::BadSignature)?;
    vk.verify_strict(&m, &Signature::from_bytes(sig.ed_sig()))
        .map_err(|_| CoreError::BadSignature)?;
    let pq = VerifyingKeyStandard::decode(pq_pk).ok_or(CoreError::BadSignature)?;
    if pq.verify(sig.pq_sig(), &DOMAIN_NONE, &HASH_ID_RAW, &m) {
        Ok(())
    } else {
        Err(CoreError::BadSignature)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domain_separation() {
        let k = SigningKeypair::generate();
        let sig = k.sign(SignDomain::Event, b"hello");
        assert_eq!(sig.as_bytes().len(), Sig::LEN);
        assert!(verify(&k.public(), SignDomain::Event, b"hello", &sig).is_ok());
        assert!(verify(&k.public(), SignDomain::Profile, b"hello", &sig).is_err());
        assert!(verify(&k.public(), SignDomain::Event, b"hellp", &sig).is_err());
        // 다른 키 ID로는 통과하지 않음(실린 공개키 묶음이 그 ID의 것이 아님).
        let other = SigningKeypair::generate();
        assert!(verify(&other.public(), SignDomain::Event, b"hello", &sig).is_err());
    }

    #[test]
    fn both_halves_required() {
        let k = SigningKeypair::generate();
        let sig = k.sign(SignDomain::Event, b"m");
        let ed_at = ED_PUBLIC_LEN + PQ_PUBLIC_LEN;
        // Ed25519 서명만 틀림.
        let mut b = sig.as_bytes().to_vec();
        b[ed_at] ^= 1;
        let bad = Sig::from_bytes(&b).unwrap();
        assert!(verify(&k.public(), SignDomain::Event, b"m", &bad).is_err());
        // FN-DSA 서명만 틀림.
        let mut b = sig.as_bytes().to_vec();
        b[Sig::LEN - 10] ^= 1;
        let bad = Sig::from_bytes(&b).unwrap();
        assert!(verify(&k.public(), SignDomain::Event, b"m", &bad).is_err());
        // 다른 키의 FN-DSA 반쪽으로 바꿔 끼우면 키 ID가 맞지 않음.
        let other = SigningKeypair::generate().sign(SignDomain::Event, b"m");
        let mut b = sig.as_bytes()[..ed_at + ED_SIG_LEN].to_vec();
        b.extend_from_slice(other.pq_sig());
        let mixed = Sig::from_bytes(&b).unwrap();
        assert!(verify(&k.public(), SignDomain::Event, b"m", &mixed).is_err());
        assert!(Sig::from_bytes(&sig.as_bytes()[1..]).is_err());
    }

    #[test]
    fn slim_needs_the_keys() {
        let k = SigningKeypair::generate();
        let slim = k.sign_slim(SignDomain::Event, b"m");
        assert_eq!(slim.as_bytes().len(), Sig::SLIM_LEN);
        assert!(slim.is_slim() && slim.signer().is_none());
        assert!(verify(&k.public(), SignDomain::Event, b"m", &slim).is_err());
        let ok = |kp: &SigningKeypair, sig: &Sig| {
            verify_with_keys(
                &k.public(),
                &kp.ed_public(),
                kp.pq_public(),
                SignDomain::Event,
                b"m",
                sig,
            )
        };
        assert!(ok(&k, &slim).is_ok());
        // 긴 서명도 같은 키로 검증되고, 다른 키 묶음(키 ID가 다름)으로는 둘 다 안 됨.
        assert!(ok(&k, &k.sign(SignDomain::Event, b"m")).is_ok());
        let other = SigningKeypair::generate();
        assert!(ok(&other, &slim).is_err());
        // 짧은 서명과 긴 서명의 FN-DSA 지문은 같은 방식(이벤트 ID가 서명 길이에 상관없이 같은 식).
        let back = Sig::from_bytes(slim.as_bytes()).unwrap();
        assert_eq!(back.fingerprint(), slim.fingerprint());
        let bin = postcard::to_stdvec(&slim).unwrap();
        assert_eq!(postcard::from_bytes::<Sig>(&bin).unwrap(), slim);
    }

    #[test]
    fn seed_is_deterministic_and_encodes() {
        let seed = [7u8; 32];
        let a = SigningKeypair::from_seed(&seed);
        let b = a.clone();
        assert_eq!(a.public(), SigningKeypair::from_seed(&seed).public());
        // 고정 시드 → 고정 ID. fn-dsa 판이 바뀌어 키 생성 결과가 달라지면(시드로 같은 신원이 안 나옴) 여기서 걸립니다.
        assert_eq!(
            hex::encode(a.public()),
            "96210cfcc44e5902a995bdcbc0406d5a0db6b5ef62465dd197f8e55a737d67e1"
        );
        let sig = b.sign(SignDomain::Event, b"x");
        assert!(verify(&a.public(), SignDomain::Event, b"x", &sig).is_ok());
        // FN-DSA 서명은 매번 새 난수.
        assert_ne!(sig, a.sign(SignDomain::Event, b"x"));
        let bin = postcard::to_stdvec(&sig).unwrap();
        assert_eq!(postcard::from_bytes::<Sig>(&bin).unwrap(), sig);
        let json = serde_json::to_string(&sig).unwrap();
        assert_eq!(serde_json::from_str::<Sig>(&json).unwrap(), sig);
    }
}
