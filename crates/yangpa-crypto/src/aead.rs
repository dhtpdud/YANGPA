//! XChaCha20-Poly1305. 24바이트 무작위 논스를 암호문 앞에 붙입니다.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use rand::RngCore;
use yangpa_core::body::Secret32;
use zeroize::Zeroizing;

use crate::error::{CryptoError, Result};

pub const NONCE_LEN: usize = 24;
pub const TAG_LEN: usize = 16;
/// 암호문이 평문보다 늘어나는 크기.
pub const OVERHEAD: usize = NONCE_LEN + TAG_LEN;

pub fn seal(key: &Secret32, ad: &[u8], plaintext: &[u8]) -> Vec<u8> {
    let mut nonce = [0u8; NONCE_LEN];
    rand::rngs::OsRng.fill_bytes(&mut nonce);
    seal_with_nonce(key, &nonce, ad, plaintext)
}

/// 논스를 직접 지정(블롭 조각처럼 논스가 결정론적이어야 할 때만). 같은 키로 논스를 재사용하면 안 됩니다.
pub fn seal_with_nonce(
    key: &Secret32,
    nonce: &[u8; NONCE_LEN],
    ad: &[u8],
    plaintext: &[u8],
) -> Vec<u8> {
    let cipher = XChaCha20Poly1305::new((&key.0).into());
    let ct = cipher
        .encrypt(
            XNonce::from_slice(nonce),
            Payload {
                msg: plaintext,
                aad: ad,
            },
        )
        .expect("XChaCha20-Poly1305 암호화는 길이 제한 안에서 실패하지 않음");
    let mut out = Vec::with_capacity(NONCE_LEN + ct.len());
    out.extend_from_slice(nonce);
    out.extend_from_slice(&ct);
    out
}

pub fn open(key: &Secret32, ad: &[u8], ciphertext: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
    if ciphertext.len() < OVERHEAD {
        return Err(CryptoError::Malformed);
    }
    let (nonce, ct) = ciphertext.split_at(NONCE_LEN);
    let cipher = XChaCha20Poly1305::new((&key.0).into());
    cipher
        .decrypt(XNonce::from_slice(nonce), Payload { msg: ct, aad: ad })
        .map(Zeroizing::new)
        .map_err(|_| CryptoError::Decrypt)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_ad_binding() {
        let k = Secret32::random();
        let ct = seal(&k, b"ad", b"hello");
        assert_eq!(ct.len(), 5 + OVERHEAD);
        assert_eq!(&**open(&k, b"ad", &ct).unwrap(), b"hello");
        assert!(open(&k, b"other", &ct).is_err());
        assert!(open(&Secret32::random(), b"ad", &ct).is_err());
        let mut bad = ct.clone();
        *bad.last_mut().unwrap() ^= 1;
        assert!(open(&k, b"ad", &bad).is_err());
        assert!(open(&k, b"ad", &ct[..10]).is_err());
    }
}
