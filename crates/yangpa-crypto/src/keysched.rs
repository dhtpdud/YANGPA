//! 키 일정 v3: 그룹 뿌리 T·기록 공개 키 R에서 상태 키·채널 뿌리·기록 키를 파생합니다(docs/protocol.md "키 그룹").
//!
//! | 키 | 만드는 법 | 바뀌는 때 |
//! | --- | --- | --- |
//! | T(그룹 뿌리) | 새 무작위, 또는 가입 때 [`advance`] | 가입이면 전진, 제거·시간 회전·확정 기기 변경이면 새로 |
//! | R(기록 공개) | 새 무작위, `r_ct`로 배포([`seal_r`]) | 제거·확정 기기 변경·시간 회전·채널이 그룹을 떠날 때 |
//! | 상태 키 S | [`state_key`]`(R)` | R과 함께 |
//! | 공개 채널 뿌리 | [`public_root`]`(R, 공간, 채널)` | R과 함께 |
//! | 비공개 채널 뿌리 | [`private_root`]`(T, 공간, 채널)` | T와 함께 |
//! | 기록 키 | [`record_key`]`(채널 뿌리)` | 뿌리와 함께 |
//!
//! 모든 파생은 BLAKE3 derive_key(라벨마다 다른 문맥 문자열)에 (비밀 32바이트 ‖ 고정 길이 입력들)을 넣습니다. 라벨이
//! 모두 다르고 라벨마다 입력 길이가 정해져 있어 두 용도의 입력이 같은 바이트열이 될 수 없습니다. 키 ID는 키 자체에서
//! 나오므로([`channel_root_id`]) 멤버가 이미 쓰는 ID에 다른 키를 끼워 넣지 못합니다.

use chacha20poly1305::aead::{AeadInPlace, KeyInit};
use chacha20poly1305::{Tag, XChaCha20Poly1305, XNonce};
use yangpa_core::body::Secret32;
use yangpa_core::ids::{ChannelId, ChannelRootId, DeviceId, RootKeyId, SpaceId};
use yangpa_core::keyupdate::{KeyGroupId, RCt};

use crate::error::{CryptoError, Result};

/// 라벨 `label`로 `key`와 고정 길이 `parts`에서 32바이트를 파생합니다.
pub(crate) fn kdf(label: &'static str, key: &Secret32, parts: &[&[u8]]) -> Secret32 {
    let mut h = blake3::Hasher::new_derive_key(label);
    h.update(&key.0);
    for p in parts {
        h.update(p);
    }
    Secret32(*h.finalize().as_bytes())
}

/// 상태 키 S = KDF(R, "yangpa/v3/state").
pub fn state_key(r: &Secret32) -> Secret32 {
    kdf("yangpa/v3/state", r, &[])
}

/// 공개 채널 뿌리 = KDF(R, "yangpa/v3/pub" ‖ 공간 ‖ 채널). 지금 R을 가진 멤버는 지금 볼 수 있는 공개 채널의 뿌리를
/// 계산합니다(채널이 그룹을 떠나면 R을 새로 고쳐 옛 R로 새 구간을 열지 못하게).
pub fn public_root(r: &Secret32, space: &SpaceId, channel: &ChannelId) -> Secret32 {
    kdf("yangpa/v3/pub", r, &[space.as_bytes(), channel.as_bytes()])
}

/// 비공개 채널 뿌리 = KDF(T, "yangpa/v3/priv" ‖ 공간 ‖ 채널). 가입자는 가입 뒤 T만 받으므로 가입 전 구간을 못 엽니다.
pub fn private_root(t: &Secret32, space: &SpaceId, channel: &ChannelId) -> Secret32 {
    kdf("yangpa/v3/priv", t, &[space.as_bytes(), channel.as_bytes()])
}

/// 기록 키 = KDF(채널 뿌리, "yangpa/v3/record").
pub fn record_key(root: &Secret32) -> Secret32 {
    kdf("yangpa/v3/record", root, &[])
}

/// 채널 뿌리 ID = derive_key("yangpa/v3/channel-root-id", 공간 ‖ 채널 ‖ 뿌리)의 앞 16바이트. 받는 쪽(당겨 받기 포함)은
/// ID가 키에서 나온 것인지 보고 받습니다. 계산은 `yangpa_core::body::channel_root_id` 하나뿐입니다(키 묶음의
/// `SharedKey::bound_to`가 같은 함수로 확인).
pub fn channel_root_id(space: &SpaceId, channel: &ChannelId, root: &Secret32) -> ChannelRootId {
    yangpa_core::body::channel_root_id(space, channel, root)
}

/// 그룹 뿌리 R의 ID(`SharedKey::Root`, 계산은 `yangpa_core::body::group_root_id`).
pub fn group_root_id(space: &SpaceId, group: &KeyGroupId, r: &Secret32) -> RootKeyId {
    yangpa_core::body::group_root_id(space, group, r)
}

/// 가입 기기 목록의 해시(정렬·중복 없는 목록이어야 함). 전진에 섞어, 같은 순번에 다른 가입 묶음을 낸 두 판이 같은 T를
/// 만들지 않게 합니다.
pub fn joined_hash(joined: &[DeviceId]) -> [u8; 32] {
    debug_assert!(joined.windows(2).all(|w| w[0] < w[1]), "정렬된 가입 목록");
    let mut h = blake3::Hasher::new_derive_key("yangpa/v3/joined");
    h.update(&(joined.len() as u64).to_be_bytes());
    for d in joined {
        h.update(d.as_bytes());
    }
    *h.finalize().as_bytes()
}

/// 직접 인코딩의 가입 전진 T' = KDF(T, "yangpa/v3/advance" ‖ 그룹 ‖ seq ‖ 가입 해시). 한 방향이라 T'에서 T를 못 구합니다.
pub fn advance(t: &Secret32, group: &KeyGroupId, seq: u64, joined_hash: &[u8; 32]) -> Secret32 {
    kdf(
        "yangpa/v3/advance",
        t,
        &[group.as_bytes(), &seq.to_be_bytes(), joined_hash],
    )
}

/// 뿌리 약속 = derive_key("yangpa/v3/key-commit", 공간 ‖ 그룹 ‖ seq ‖ T ‖ R). T·R은 256비트 비밀이라 약속에서 드러나지
/// 않습니다. 받는 쪽은 계산한 T·R로 다시 계산해 [`ct_eq`]로 비교합니다. R까지 묶어, 가입자가 봉인 묶음에서 받은 R(이나
/// 멤버가 중계해 준 T·R)도 그룹 모두와 같은 값인지 확인됩니다.
pub fn root_commit(
    space: &SpaceId,
    group: &KeyGroupId,
    seq: u64,
    t: &Secret32,
    r: &Secret32,
) -> [u8; 32] {
    let mut h = blake3::Hasher::new_derive_key("yangpa/v3/key-commit");
    h.update(space.as_bytes());
    h.update(group.as_bytes());
    h.update(&seq.to_be_bytes());
    h.update(&t.0);
    h.update(&r.0);
    *h.finalize().as_bytes()
}

// ───────────── 휘발성 채널 세션 키(RAM 전용) ─────────────
//
// 세션 키는 크루 키·상자 비밀에서 파생하지 않습니다. 새 키는 무작위(`LockedKeyring`), 가입만 있을 때는 부모 세대 키에서
// [`vol_join`]으로 각자 계산합니다(한 방향이라 가입자는 부모 키를 못 구함).

/// 가입 유도 K = KDF(K_부모, "yangpa/v3/vol-join" ‖ 공간 ‖ 채널 ‖ 세대 ‖ BLAKE3(가입 기기 오름차순)).
pub fn vol_join(
    parent: &Secret32,
    space: &SpaceId,
    channel: &ChannelId,
    generation: u64,
    joiners: &[DeviceId],
) -> Secret32 {
    let j = device_set_hash("yangpa/v3/vol-joiners", joiners);
    kdf(
        "yangpa/v3/vol-join",
        parent,
        &[
            space.as_bytes(),
            channel.as_bytes(),
            &generation.to_be_bytes(),
            &j,
        ],
    )
}

/// 키 약속값 C = derive_key("yangpa/v3/vol-key-id", 공간 ‖ 채널 ‖ 세대 ‖ K). 받는 쪽은 감싸기·유도로 얻은 키를 이것과
/// [`ct_eq`]로 견줍니다(참가자가 다른 기기에게 다른 키를 건네지 못함).
pub fn vol_key_commit(
    space: &SpaceId,
    channel: &ChannelId,
    generation: u64,
    k: &Secret32,
) -> [u8; 32] {
    let mut h = blake3::Hasher::new_derive_key("yangpa/v3/vol-key-id");
    h.update(space.as_bytes());
    h.update(channel.as_bytes());
    h.update(&generation.to_be_bytes());
    h.update(&k.0);
    *h.finalize().as_bytes()
}

/// 세대 받는 기기 목록 요약(오름차순 목록).
pub fn vol_recipients_hash(devices: &[DeviceId]) -> [u8; 32] {
    device_set_hash("yangpa/v3/vol-recipients", devices)
}

/// 감싸기의 연관 데이터: 공간 ‖ 채널 ‖ 세대 ‖ 받는 기기 ‖ C. 다른 세대·기기·약속으로 옮겨 쓰지 못합니다.
pub fn vol_wrap_ad(
    space: &SpaceId,
    channel: &ChannelId,
    generation: u64,
    device: &DeviceId,
    commit: &[u8; 32],
) -> Vec<u8> {
    let mut ad = b"yangpa/v3/vol-wrap".to_vec();
    ad.extend_from_slice(space.as_bytes());
    ad.extend_from_slice(channel.as_bytes());
    ad.extend_from_slice(&generation.to_be_bytes());
    ad.extend_from_slice(device.as_bytes());
    ad.extend_from_slice(commit);
    ad
}

/// 회전 공지 본문을 싸는 키(비밀 아님: 공간·채널에서만 나옴). 공지는 키를 싣지 않고 참가자에게 직접만 가므로 숨길 것이
/// 없고, 가입자도 부모 키 없이 읽어야 합니다. 휘발성 키 표시(`KeyRef::Volatile`, 세대 0)로 써서 디스크로 가는 길을 막는
/// 기존 검사를 그대로 탑니다.
pub fn vol_notice_key(space: &SpaceId, channel: &ChannelId) -> Secret32 {
    let mut h = blake3::Hasher::new_derive_key("yangpa/v3/vol-notice");
    h.update(space.as_bytes());
    h.update(channel.as_bytes());
    Secret32(*h.finalize().as_bytes())
}

fn device_set_hash(label: &'static str, devices: &[DeviceId]) -> [u8; 32] {
    debug_assert!(devices.windows(2).all(|w| w[0] < w[1]), "정렬된 기기 목록");
    let mut h = blake3::Hasher::new_derive_key(label);
    h.update(&(devices.len() as u64).to_be_bytes());
    for d in devices {
        h.update(d.as_bytes());
    }
    *h.finalize().as_bytes()
}

/// 상수 시간 비교(약속·키·토큰 비교에서 처음 다른 바이트 위치가 시간으로 드러나지 않게).
pub fn ct_eq<const N: usize>(a: &[u8; N], b: &[u8; N]) -> bool {
    let mut d = 0u8;
    for (x, y) in a.iter().zip(b) {
        d |= x ^ y;
    }
    std::hint::black_box(d) == 0
}

const R_CT_AD: &[u8] = b"yangpa/v3/r-ct";

fn r_ct_parts(space: &SpaceId, group: &KeyGroupId, seq: u64, salt: &[u8; 24]) -> Vec<u8> {
    let mut ad = Vec::with_capacity(R_CT_AD.len() + 16 + 16 + 8 + 24);
    ad.extend_from_slice(R_CT_AD);
    ad.extend_from_slice(space.as_bytes());
    ad.extend_from_slice(group.as_bytes());
    ad.extend_from_slice(&seq.to_be_bytes());
    ad.extend_from_slice(salt);
    ad
}

/// 고정 논스로 32바이트를 암호화합니다. 키가 감싸기마다 다르게 파생되는 곳에서만 씁니다(같은 키를 두 번 쓰지 않음).
pub(crate) fn seal_once(key: &Secret32, ad: &[u8], plaintext: &Secret32) -> [u8; 48] {
    let cipher = XChaCha20Poly1305::new((&key.0).into());
    let mut out = [0u8; 48];
    out[..32].copy_from_slice(&plaintext.0);
    let tag = cipher
        .encrypt_in_place_detached(&XNonce::default(), ad, &mut out[..32])
        .expect("32바이트 암호화는 실패하지 않음");
    out[32..].copy_from_slice(&tag);
    out
}

pub(crate) fn open_once(key: &Secret32, ad: &[u8], ct: &[u8; 48]) -> Option<Secret32> {
    let cipher = XChaCha20Poly1305::new((&key.0).into());
    let mut buf = Secret32([0u8; 32]);
    buf.0.copy_from_slice(&ct[..32]);
    cipher
        .decrypt_in_place_detached(
            &XNonce::default(),
            ad,
            &mut buf.0,
            Tag::from_slice(&ct[32..]),
        )
        .ok()
        .map(|()| buf)
}

/// 새 R을 새 T로 암호화합니다(`r_ct`). 키 = KDF(T, 공간 ‖ 그룹 ‖ seq ‖ salt)라 갱신마다 다르고, AD도 같은 값을 묶습니다.
pub fn seal_r(
    t: &Secret32,
    space: &SpaceId,
    group: &KeyGroupId,
    seq: u64,
    salt: &[u8; 24],
    r: &Secret32,
) -> RCt {
    let ad = r_ct_parts(space, group, seq, salt);
    let key = kdf("yangpa/v3/r-ct-key", t, &[&ad]);
    RCt {
        ct: seal_once(&key, &ad, r),
    }
}

pub fn open_r(
    t: &Secret32,
    space: &SpaceId,
    group: &KeyGroupId,
    seq: u64,
    salt: &[u8; 24],
    r_ct: &RCt,
) -> Result<Secret32> {
    let ad = r_ct_parts(space, group, seq, salt);
    let key = kdf("yangpa/v3/r-ct-key", t, &[&ad]);
    open_once(&key, &ad, &r_ct.ct).ok_or(CryptoError::Decrypt)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derivations_are_deterministic_and_separated() {
        let k = Secret32([7; 32]);
        let (s, c) = (SpaceId([1; 16]), ChannelId([2; 16]));
        let outs = [
            state_key(&k).0,
            public_root(&k, &s, &c).0,
            private_root(&k, &s, &c).0,
            record_key(&k).0,
            advance(&k, &KeyGroupId([1; 16]), 1, &[0; 32]).0,
        ];
        for (i, a) in outs.iter().enumerate() {
            for b in &outs[i + 1..] {
                assert_ne!(a, b, "라벨이 섞임");
            }
        }
        assert_eq!(public_root(&k, &s, &c).0, outs[1]);
        assert_ne!(public_root(&k, &s, &ChannelId([3; 16])).0, outs[1]);
        assert_ne!(private_root(&k, &SpaceId([9; 16]), &c).0, outs[2]);
        // 같은 그룹·순번이어도 가입 묶음이 다르면 전진한 T가 다름.
        let g = KeyGroupId([1; 16]);
        let a = joined_hash(&[DeviceId([1; 32])]);
        let b = joined_hash(&[DeviceId([1; 32]), DeviceId([2; 32])]);
        assert_ne!(advance(&k, &g, 5, &a).0, advance(&k, &g, 5, &b).0);
        assert_ne!(advance(&k, &g, 5, &a).0, advance(&k, &g, 6, &a).0);
    }

    #[test]
    fn channel_root_id_is_bound_to_the_key() {
        let (s, c) = (SpaceId([1; 16]), ChannelId([2; 16]));
        let r = Secret32::random();
        let id = channel_root_id(&s, &c, &r);
        assert_eq!(id, channel_root_id(&s, &c, &r));
        assert_ne!(id, channel_root_id(&s, &c, &Secret32::random()));
        assert_ne!(id, channel_root_id(&s, &ChannelId([3; 16]), &r));
        // 공용 크레이트로 옮겨도 값(라벨·입력 순서)은 그대로입니다.
        let mut h = blake3::Hasher::new_derive_key("yangpa/v3/channel-root-id");
        h.update(s.as_bytes());
        h.update(c.as_bytes());
        h.update(&r.0);
        assert_eq!(id.0[..], h.finalize().as_bytes()[..16]);
        let g = KeyGroupId([4; 16]);
        assert_eq!(group_root_id(&s, &g, &r), group_root_id(&s, &g, &r));
        assert_ne!(
            group_root_id(&s, &g, &r),
            group_root_id(&s, &KeyGroupId([5; 16]), &r)
        );
    }

    #[test]
    fn root_commit_and_r_ct() {
        let (s, g) = (SpaceId([1; 16]), KeyGroupId([2; 16]));
        let (t, r) = (Secret32::random(), Secret32::random());
        let c = root_commit(&s, &g, 4, &t, &r);
        assert!(ct_eq(&c, &root_commit(&s, &g, 4, &t, &r)));
        assert!(!ct_eq(&c, &root_commit(&s, &g, 5, &t, &r)));
        assert!(!ct_eq(&c, &root_commit(&s, &g, 4, &Secret32::random(), &r)));
        // R이 다르면(가입자에게 다른 R을 줌) 약속이 다름.
        assert!(!ct_eq(&c, &root_commit(&s, &g, 4, &t, &Secret32::random())));
        assert!(!ct_eq(
            &c,
            &root_commit(&s, &KeyGroupId([3; 16]), 4, &t, &r)
        ));
        let salt = [9u8; 24];
        let ct = seal_r(&t, &s, &g, 4, &salt, &r);
        assert_eq!(open_r(&t, &s, &g, 4, &salt, &ct).unwrap().0, r.0);
        // 다른 T·순번·그룹·salt면 열리지 않음.
        assert!(open_r(&Secret32::random(), &s, &g, 4, &salt, &ct).is_err());
        assert!(open_r(&t, &s, &g, 5, &salt, &ct).is_err());
        assert!(open_r(&t, &s, &KeyGroupId([3; 16]), 4, &salt, &ct).is_err());
        assert!(open_r(&t, &s, &g, 4, &[0; 24], &ct).is_err());
        // 같은 T·순번이라도 salt가 다르면 암호문이 다름(같은 키로 다른 R을 고정 논스로 암호화하지 않음).
        let other = seal_r(&t, &s, &g, 4, &[1; 24], &Secret32::random());
        assert_ne!(ct.ct[..32], other.ct[..32]);
    }
}
