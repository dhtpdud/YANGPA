//! HKDF-SHA256.

use hkdf::Hkdf;
use sha2::Sha256;
use yangpa_core::body::Secret32;

pub fn derive(ikm: &[u8], salt: &[u8], info: &[u8]) -> Secret32 {
    let hk = Hkdf::<Sha256>::new(Some(salt), ikm);
    let mut out = [0u8; 32];
    hk.expand(info, &mut out)
        .expect("32바이트 출력은 HKDF 한도 안");
    Secret32(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_and_separated() {
        assert_eq!(derive(b"k", b"s", b"a").0, derive(b"k", b"s", b"a").0);
        assert_ne!(derive(b"k", b"s", b"a").0, derive(b"k", b"s", b"b").0);
        assert_ne!(derive(b"k", b"s", b"a").0, derive(b"k", b"t", b"a").0);
    }
}
