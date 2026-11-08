//! 이벤트 본문 암·복호화.
//!
//! `ciphertext = seal(key, header.aead_ad(), pad(zl::pack(postcard(EventBody))))`. AD가 본문 해시·길이를 뺀 헤더
//! 전체라서 암호문을 공간·채널·작성자·논리 시계·키 참조에 묶습니다. 압축(OpenZL, 줄지 않으면 그대로) 뒤에 패딩해,
//! 압축된 길이도 Padmé 단위로만 드러납니다. 크기 한도는 종류별 본문 한도([`EventKind::max_body_len`]: 보통
//! [`MAX_BODY_LEN`], 키 갱신 조각은 960KiB)이고 압축 전 평문과 암호문에 겁니다.
//!
//! 예전 판이 저장해 둔 본문(압축 표시 바이트 없이 postcard가 바로 옴)도 읽습니다: postcard 열거 번호는 표시 바이트
//! (0xF0·0xF1)보다 한참 작습니다.

use yangpa_core::CoreError;
use yangpa_core::body::{EventBody, Secret32};
use yangpa_core::device::BoxPublicKey;
#[cfg(doc)]
use yangpa_core::event::MAX_BODY_LEN;
use yangpa_core::event::{EventHeader, EventKind, KeyRef};
use yangpa_core::ids::DeviceId;
use zeroize::Zeroizing;

use crate::aead;
use crate::error::{CryptoError, Result};
use crate::pad::{pad, unpad};
use crate::sealed::{BoxKeypair, open_sealed, seal_to};

/// 직렬화 → 종류 확인 → 패딩. 중간 평문 버퍼는 해제 시 0으로 덮어씁니다.
fn padded_plaintext(header: &EventHeader, body: &EventBody) -> Result<Zeroizing<Vec<u8>>> {
    if !body.fits(header.kind) {
        return Err(CoreError::InvalidEncoding.into());
    }
    let encoded = Zeroizing::new(body.encode()?);
    check_len(encoded.len(), header.kind)?;
    let packed = yangpa_zl::pack(&encoded);
    Ok(Zeroizing::new(pad(&packed)))
}

/// 종류별 본문 한도. 키 갱신 조각(키 합의 레인)은 일반 한도의 몇 배라, 일반 한도로 자르면 200대 넘는 직접 인코딩
/// 회전이 암호화에서 막힙니다.
fn check_len(n: usize, kind: EventKind) -> Result<()> {
    if n > kind.max_body_len() {
        return Err(CoreError::LimitExceeded("본문 크기").into());
    }
    Ok(())
}

/// 압축 표시 바이트가 있으면 풀고(한도는 그 종류의 본문 한도), 없으면 예전 판 형식으로 보고 그대로 해석합니다.
fn decode_maybe_packed(bytes: &[u8], kind: EventKind) -> Result<EventBody> {
    if !yangpa_zl::is_packed(bytes) {
        return Ok(EventBody::decode(bytes, kind)?);
    }
    let plain = yangpa_zl::unpack(bytes, kind.max_body_len()).map_err(zl_err)?;
    Ok(EventBody::decode(&plain, kind)?)
}

pub(crate) fn zl_err(e: yangpa_zl::ZlError) -> CryptoError {
    match e {
        yangpa_zl::ZlError::TooLarge { .. } => CryptoError::TooLarge("압축 해제 크기"),
        yangpa_zl::ZlError::Malformed => CryptoError::Malformed,
        yangpa_zl::ZlError::Compress(m) => CryptoError::Compression(m),
    }
}

fn check_out_len(ct: Vec<u8>, kind: EventKind) -> Result<Vec<u8>> {
    check_len(ct.len(), kind)?;
    Ok(ct)
}

fn decode_padded(header: &EventHeader, plaintext: &[u8]) -> Result<EventBody> {
    let inner = unpad(plaintext)?;
    decode_maybe_packed(inner, header.kind)
}

/// 대칭 키(상태·기록·휘발성 키)로 본문을 암호화합니다.
///
/// `header.key`는 대칭 키 참조여야 하고 본문 종류가 `header.kind`와 같아야 합니다.
/// 결과가 그 종류의 본문 한도를 넘으면 실패합니다(큰 데이터는 블롭으로).
pub fn encrypt_body(key: &Secret32, header: &EventHeader, body: &EventBody) -> Result<Vec<u8>> {
    if header.key == KeyRef::Sealed {
        return Err(CryptoError::Malformed);
    }
    let pt = padded_plaintext(header, body)?;
    check_out_len(aead::seal(key, &header.aead_ad(), &pt), header.kind)
}

/// [`encrypt_body`]의 역. 패딩을 벗기고 헤더 종류와 맞는지 확인합니다.
pub fn decrypt_body(key: &Secret32, header: &EventHeader, ciphertext: &[u8]) -> Result<EventBody> {
    if header.key == KeyRef::Sealed {
        return Err(CryptoError::Malformed);
    }
    check_len(ciphertext.len(), header.kind)?;
    let pt = aead::open(key, &header.aead_ad(), ciphertext)?;
    decode_padded(header, &pt)
}

/// `KeyRef::Sealed` 본문: 지정 기기들(기기 ID, 상자 공개키)에게 봉인합니다. AD는 [`EventHeader::aead_ad`].
pub fn seal_body(
    recipients: &[(DeviceId, BoxPublicKey)],
    header: &EventHeader,
    body: &EventBody,
) -> Result<Vec<u8>> {
    if header.key != KeyRef::Sealed {
        return Err(CryptoError::Malformed);
    }
    if recipients.is_empty() {
        return Err(CryptoError::NotARecipient);
    }
    let pt = padded_plaintext(header, body)?;
    check_out_len(seal_to(recipients, &header.aead_ad(), &pt), header.kind)
}

/// [`seal_body`]의 역. 이 기기가 수신자가 아니면 [`CryptoError::NotARecipient`].
pub fn open_sealed_body(
    me: &DeviceId,
    kp: &BoxKeypair,
    header: &EventHeader,
    envelope: &[u8],
) -> Result<EventBody> {
    if header.key != KeyRef::Sealed {
        return Err(CryptoError::Malformed);
    }
    check_len(envelope.len(), header.kind)?;
    let pt = open_sealed(me, kp, &header.aead_ad(), envelope)?;
    decode_padded(header, &pt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use yangpa_core::Timestamp;
    use yangpa_core::body::{KeyShareBody, MessageBody, SharedKey};
    use yangpa_core::event::{EVENT_VERSION, MAX_BODY_LEN, MAX_KEY_UPDATE_BODY_LEN};
    use yangpa_core::ids::{ChannelId, ChannelRootId, SpaceId, StateKeyId, UserId};

    fn header(kind: EventKind, key: KeyRef) -> EventHeader {
        EventHeader {
            v: EVENT_VERSION,
            space: SpaceId::random(),
            channel: Some(ChannelId::random()),
            kind,
            author: DeviceId::random(),
            user: UserId::random(),
            parents: vec![],
            lamport: 7,
            created_at: Timestamp(1),
            key,
            body_hash: [0; 32],
            body_len: 0,
            scope: None,
            state_anchor: Vec::new(),
        }
    }

    fn msg(text: &str) -> EventBody {
        EventBody::Message(MessageBody {
            text: text.into(),
            ..Default::default()
        })
    }

    #[test]
    fn roundtrip_and_context_binding() {
        let k = Secret32::random();
        let h = header(
            EventKind::Message,
            KeyRef::Record {
                root: ChannelRootId::random(),
            },
        );
        let body = msg("안녕하세요");
        let ct = encrypt_body(&k, &h, &body).unwrap();
        assert_eq!(decrypt_body(&k, &h, &ct).unwrap(), body);

        // 헤더 문맥이 바뀌면(다른 채널·논리 시계·키 참조) 열리지 않는다.
        let mut h2 = h.clone();
        h2.lamport += 1;
        assert!(matches!(
            decrypt_body(&k, &h2, &ct),
            Err(CryptoError::Decrypt)
        ));
        let mut h3 = h.clone();
        h3.channel = Some(ChannelId::random());
        assert!(decrypt_body(&k, &h3, &ct).is_err());
        // 본문 해시·길이는 AD에서 빠지므로 서명 시 채워도 그대로 열린다.
        let mut h4 = h.clone();
        h4.body_hash = [9; 32];
        h4.body_len = ct.len() as u32;
        assert_eq!(decrypt_body(&k, &h4, &ct).unwrap(), body);
        // 다른 키.
        assert!(decrypt_body(&Secret32::random(), &h, &ct).is_err());
    }

    #[test]
    fn padding_hides_exact_length() {
        let k = Secret32::random();
        let h = header(
            EventKind::Message,
            KeyRef::Record {
                root: ChannelRootId::random(),
            },
        );
        let a = encrypt_body(&k, &h, &msg(&"a".repeat(1000))).unwrap();
        let b = encrypt_body(&k, &h, &msg(&"a".repeat(1003))).unwrap();
        assert_eq!(a.len(), b.len());
        let short1 = encrypt_body(&k, &h, &msg("")).unwrap();
        let short2 = encrypt_body(&k, &h, &msg("짧은 글")).unwrap();
        assert_eq!(short1.len(), short2.len());
    }

    #[test]
    fn kind_and_keyref_checks() {
        let k = Secret32::random();
        let h = header(
            EventKind::Edit,
            KeyRef::Record {
                root: ChannelRootId::random(),
            },
        );
        assert!(encrypt_body(&k, &h, &msg("x")).is_err());
        let sealed = header(EventKind::Message, KeyRef::Sealed);
        assert!(matches!(
            encrypt_body(&k, &sealed, &msg("x")),
            Err(CryptoError::Malformed)
        ));

        // 같은 키로 암호화한 다른 종류 본문을 종류만 바꾼 헤더로 열 수 없다(AD에 종류 포함).
        let h = header(
            EventKind::Message,
            KeyRef::Record {
                root: ChannelRootId::random(),
            },
        );
        let ct = encrypt_body(&k, &h, &msg("x")).unwrap();
        let mut hk = h.clone();
        hk.kind = EventKind::Edit;
        assert!(decrypt_body(&k, &hk, &ct).is_err());
    }

    #[test]
    fn oversize_rejected() {
        let k = Secret32::random();
        let h = header(
            EventKind::Message,
            KeyRef::Record {
                root: ChannelRootId::random(),
            },
        );
        let big = msg(&"가".repeat(MAX_BODY_LEN / 3 + 10));
        assert!(matches!(
            encrypt_body(&k, &h, &big),
            Err(CryptoError::Core(CoreError::LimitExceeded(_)))
        ));
        assert!(decrypt_body(&k, &h, &vec![0u8; MAX_BODY_LEN + 1]).is_err());
        assert!(decrypt_body(&k, &h, &[1, 2, 3]).is_err());
    }

    #[test]
    fn bodies_are_compressed_and_legacy_bodies_still_open() {
        let k = Secret32::random();
        let h = header(
            EventKind::Message,
            KeyRef::Record {
                root: ChannelRootId::random(),
            },
        );
        let long = msg(&"양파 메신저의 긴 글입니다. ".repeat(2000));
        let ct = encrypt_body(&k, &h, &long).unwrap();
        assert!(ct.len() < long.encode().unwrap().len() / 10, "{}", ct.len());
        assert_eq!(decrypt_body(&k, &h, &ct).unwrap(), long);

        // 예전 판: pad(postcard)를 바로 봉인
        let body = msg("예전 글");
        let legacy = aead::seal(&k, &h.aead_ad(), &pad(&body.encode().unwrap()));
        assert_eq!(decrypt_body(&k, &h, &legacy).unwrap(), body);

        // 압축 폭탄: 한도보다 크게 풀리는 프레임은 거절
        let huge = yangpa_zl::pack(&vec![0u8; MAX_BODY_LEN * 4]);
        let bomb = aead::seal(&k, &h.aead_ad(), &pad(&huge));
        assert!(matches!(
            decrypt_body(&k, &h, &bomb),
            Err(CryptoError::TooLarge(_))
        ));
    }

    #[test]
    fn sealed_body_roundtrip() {
        let a = BoxKeypair::generate();
        let b = BoxKeypair::generate();
        let (da, db, dc) = (DeviceId::random(), DeviceId::random(), DeviceId::random());
        let mut h = header(EventKind::KeyShare, KeyRef::Sealed);
        h.channel = None;
        let body = EventBody::KeyShare(KeyShareBody {
            keys: vec![SharedKey::State {
                id: StateKeyId::random(),
                key: Secret32::random(),
            }],
            for_user: Some(UserId::random()),
        });
        let env = seal_body(&[(da, a.public()), (db, b.public())], &h, &body).unwrap();
        assert_eq!(open_sealed_body(&da, &a, &h, &env).unwrap(), body);
        assert_eq!(open_sealed_body(&db, &b, &h, &env).unwrap(), body);
        assert!(matches!(
            open_sealed_body(&dc, &a, &h, &env),
            Err(CryptoError::NotARecipient)
        ));
        let mut h2 = h.clone();
        h2.lamport = 99;
        assert!(open_sealed_body(&da, &a, &h2, &env).is_err());
        assert!(seal_body(&[], &h, &body).is_err());
        let sym = header(EventKind::KeyShare, KeyRef::State(StateKeyId::random()));
        assert!(seal_body(&[(da, a.public())], &sym, &body).is_err());
    }

    /// 키 갱신 조각은 일반 본문 한도(256KiB)보다 커도 키 합의 레인 한도(960KiB)까지 암·복호화됩니다. 나누기 목표
    /// 크기(`PART_TARGET_LEN`)의 조각이 압축 표시·Padmé 채움·태그를 더해도 한도 안이어야 합니다.
    #[test]
    fn key_update_parts_use_the_key_agreement_limit() {
        use yangpa_core::keyupdate::{
            KeyUpdateKind, KeyUpdatePayload, PART_TARGET_LEN, RCt, SealedBox,
        };
        let k = Secret32::random();
        let mut h = header(EventKind::KeyUpdate, KeyRef::State(StateKeyId::random()));
        h.channel = None;
        let space = h.space;
        let body_of = |n: usize| {
            let mut sealed = vec![0u8; n];
            rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut sealed);
            EventBody::KeyUpdate(KeyUpdatePayload {
                group: yangpa_core::ids::KeyGroupId::crew(&space),
                seq: 2,
                prev: None,
                kind: KeyUpdateKind::Rotate {
                    removed: vec![DeviceId::random()],
                    joined: Vec::new(),
                },
                root_commit: [1; 32],
                prev_root_commit: [2; 32],
                crew: None,
                bundle_root: None,
                shards: None,
                depth: 0,
                salt: [3; 24],
                layout: Vec::new(),
                wraps: Vec::new(),
                sealed: vec![SealedBox(sealed)],
                r_ct: Some(RCt { ct: [4; 48] }),
                part: 0,
                parts: 1,
            })
        };
        // 인코딩이 정확히 나누기 목표 크기인 본문.
        let base = body_of(0).encode().unwrap().len();
        let mut n = PART_TARGET_LEN - base;
        while body_of(n).encode().unwrap().len() > PART_TARGET_LEN {
            n -= 1;
        }
        let body = body_of(n);
        assert!(body.encode().unwrap().len() > MAX_BODY_LEN);
        let ct = encrypt_body(&k, &h, &body).unwrap();
        assert!(ct.len() <= MAX_KEY_UPDATE_BODY_LEN, "{}", ct.len());
        assert_eq!(decrypt_body(&k, &h, &ct).unwrap(), body);
        // 같은 크기라도 일반 종류의 한도는 그대로입니다.
        let mut hm = header(EventKind::Message, KeyRef::State(StateKeyId::random()));
        hm.channel = None;
        assert!(decrypt_body(&k, &hm, &ct).is_err());
        assert!(decrypt_body(&k, &h, &vec![0u8; MAX_KEY_UPDATE_BODY_LEN + 1]).is_err());
    }
}
