//! Padmé 패딩: 길이를 지수·가수 표현에서 하위 비트를 0으로 올림해 길이 누출을 줄입니다
//! (오버헤드 최대 약 12%). 앞에 4바이트 BE 원래 길이를 둡니다.

use crate::error::{CryptoError, Result};

/// Padmé 목표 길이.
pub fn padme_len(len: usize) -> usize {
    if len < 2 {
        return len.max(1);
    }
    let l = len as u64;
    let e = 63 - l.leading_zeros() as u64; // floor(log2 L)
    let s = 64 - e.leading_zeros() as u64; // floor(log2 e) + 1
    let z = e.saturating_sub(s);
    let mask = (1u64 << z) - 1;
    ((l + mask) & !mask) as usize
}

pub fn pad(data: &[u8]) -> Vec<u8> {
    let inner = 4 + data.len();
    let total = padme_len(inner.max(32));
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(data);
    out.resize(total, 0);
    out
}

pub fn unpad(data: &[u8]) -> Result<&[u8]> {
    if data.len() < 4 {
        return Err(CryptoError::Malformed);
    }
    let n = u32::from_be_bytes(data[..4].try_into().expect("4바이트")) as usize;
    // 32비트 대상에서 4 + n이 넘치지 않게 합니다(길이 접두는 받은 데이터라 믿을 수 없음).
    let end = n.checked_add(4).ok_or(CryptoError::Malformed)?;
    data.get(4..end).ok_or(CryptoError::Malformed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn padme_overhead_bounded() {
        for len in [1usize, 2, 9, 100, 1000, 4097, 65_000, 1_000_000] {
            let p = padme_len(len);
            assert!(p >= len);
            assert!(
                (p - len) as f64 / len as f64 <= 0.12 + 1e-9 || len < 16,
                "len={len} p={p}"
            );
        }
        // 비슷한 길이는 같은 버킷으로 모인다.
        assert_eq!(padme_len(1000), padme_len(1001));
    }

    #[test]
    fn roundtrip() {
        for n in [0usize, 1, 31, 32, 200, 5000] {
            let d: Vec<u8> = (0..n).map(|i| i as u8).collect();
            let p = pad(&d);
            assert!(p.len() >= 32);
            assert_eq!(unpad(&p).unwrap(), &d[..]);
        }
        assert!(unpad(&[0, 0, 0, 9, 1]).is_err());
        assert!(unpad(&[0xff, 0xff, 0xff, 0xff, 1]).is_err());
        assert!(unpad(&[0, 0, 0]).is_err());
    }
}
