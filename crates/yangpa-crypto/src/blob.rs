//! 블롭(첨부·이모지 원본) 조각 단위 AEAD.
//!
//! - 평문을 `chunk_size - 16`바이트씩 잘라 조각마다 XChaCha20-Poly1305로 암호화합니다. 그래서 암호문 조각은
//!   마지막을 빼고 정확히 `chunk_size`바이트이고, 매니페스트([`BlobManifest`])의 조각 경계와 일치합니다.
//!   시더는 키 없이 암호문 조각의 해시만으로 검증·제공할 수 있습니다.
//! - 논스는 조각 번호로 정합니다(논스를 싣지 않아 조각 크기가 고정됨). 블롭 키는 첨부마다 무작위이고, 여기서
//!   다시 HKDF로 블롭 전용 키를 파생하므로 같은 (키, 논스) 쌍이 다른 용도로 쓰이지 않습니다.
//!   **같은 블롭 키를 두 블롭에 쓰면 안 됩니다.**
//! - AD에 조각 번호·전체 조각 수·마지막 조각 표시를 묶어 조각 순서 바꾸기·자르기·덧붙이기를 막습니다(STREAM 방식).
//! - 빈 평문도 태그만 든 조각 하나를 만들어, 조각이 0개인 블롭(자르기와 구별 불가)이 생기지 않게 합니다.

use std::io::{ErrorKind, Read, Write};

use chacha20poly1305::aead::{AeadInPlace, KeyInit};
use chacha20poly1305::{Tag, XChaCha20Poly1305, XNonce};
use yangpa_core::blob::{BlobManifest, MAX_CHUNK_SIZE, MAX_CHUNKS, MIN_CHUNK_SIZE};
use yangpa_core::body::Secret32;
use zeroize::Zeroizing;

use crate::aead::TAG_LEN;
use crate::error::{CryptoError, Result};
use crate::kdf::derive;

const AD_PREFIX: &[u8] = b"yangpa/v1/blob-chunk\0";
const NONCE_PREFIX: &[u8] = b"yangpa/blob\0";

fn blob_cipher(key: &Secret32) -> XChaCha20Poly1305 {
    let k = derive(&key.0, b"yangpa/v1/blob", b"chunk-key");
    XChaCha20Poly1305::new((&k.0).into())
}

fn nonce(index: u32) -> XNonce {
    let mut n = [0u8; 24];
    n[..NONCE_PREFIX.len()].copy_from_slice(NONCE_PREFIX);
    n[20..].copy_from_slice(&index.to_be_bytes());
    XNonce::clone_from_slice(&n)
}

fn chunk_ad(index: u32, total: u32) -> [u8; AD_PREFIX.len() + 9] {
    let mut ad = [0u8; AD_PREFIX.len() + 9];
    let p = AD_PREFIX.len();
    ad[..p].copy_from_slice(AD_PREFIX);
    ad[p..p + 4].copy_from_slice(&index.to_be_bytes());
    ad[p + 4..p + 8].copy_from_slice(&total.to_be_bytes());
    ad[p + 8] = u8::from(index + 1 == total);
    ad
}

/// 평문 길이와 요청한 조각 크기로 실제 조각 크기를 정합니다. 허용 범위로 맞추고, 조각 수가
/// [`MAX_CHUNKS`]를 넘으면 조각 크기를 키웁니다. 그래도 안 되면(약 1TiB 초과) 실패합니다.
pub fn effective_chunk_size(plaintext_len: u64, requested: u32) -> Result<u32> {
    let mut cs = requested.clamp(MIN_CHUNK_SIZE, MAX_CHUNK_SIZE);
    if chunk_count(plaintext_len, cs) > MAX_CHUNKS as u64 {
        let per_chunk = plaintext_len.div_ceil(MAX_CHUNKS as u64) + TAG_LEN as u64;
        if per_chunk > MAX_CHUNK_SIZE as u64 {
            return Err(CryptoError::TooLarge("블롭 크기"));
        }
        cs = per_chunk as u32;
    }
    Ok(cs)
}

fn chunk_count(plaintext_len: u64, chunk_size: u32) -> u64 {
    plaintext_len
        .div_ceil(chunk_size as u64 - TAG_LEN as u64)
        .max(1)
}

/// 암호화된 블롭의 평문 크기.
pub fn plaintext_len(manifest: &BlobManifest) -> u64 {
    manifest
        .size
        .saturating_sub(manifest.chunks.len() as u64 * TAG_LEN as u64)
}

/// 조각 하나에 든 평문 최대 크기.
pub fn plaintext_chunk_len(manifest: &BlobManifest) -> u64 {
    (manifest.chunk_size as u64).saturating_sub(TAG_LEN as u64)
}

/// 평문 위치 `offset`이 들어 있는 (조각 번호, 조각 안 위치). 범위 밖이면 `None`.
pub fn locate(manifest: &BlobManifest, offset: u64) -> Option<(u32, u64)> {
    let per = plaintext_chunk_len(manifest);
    if per == 0 || offset >= plaintext_len(manifest) {
        return None;
    }
    Some(((offset / per) as u32, offset % per))
}

/// 메모리에 있는 평문 전체를 암호화합니다. 반환: (암호문, 암호문 기준 매니페스트).
///
/// `chunk_size`는 [`effective_chunk_size`] 규칙으로 맞춥니다(실제 값은 매니페스트에 있음).
///
/// # Panics
/// 평문이 약 1TiB를 넘어 매니페스트 한도를 지킬 수 없을 때만. 그럴 수 있는 입력이면 [`try_encrypt_blob`]을 씁니다.
pub fn encrypt_blob(key: &Secret32, plaintext: &[u8], chunk_size: u32) -> (Vec<u8>, BlobManifest) {
    try_encrypt_blob(key, plaintext, chunk_size)
        .expect("메모리 안 평문은 블롭 한도(약 1TiB)를 넘지 않음")
}

/// [`encrypt_blob`]의 실패를 돌려주는 판.
pub fn try_encrypt_blob(
    key: &Secret32,
    plaintext: &[u8],
    chunk_size: u32,
) -> Result<(Vec<u8>, BlobManifest)> {
    let cs = effective_chunk_size(plaintext.len() as u64, chunk_size)?;
    let pt_chunk = cs as usize - TAG_LEN;
    let total = chunk_count(plaintext.len() as u64, cs) as u32;
    let cipher = blob_cipher(key);
    let mut out = Vec::with_capacity(plaintext.len() + total as usize * TAG_LEN);
    let mut hashes = Vec::with_capacity(total as usize);
    for index in 0..total {
        let start = index as usize * pt_chunk;
        let end = (start + pt_chunk).min(plaintext.len());
        let pos = out.len();
        out.extend_from_slice(&plaintext[start..end]);
        let tag = cipher
            .encrypt_in_place_detached(&nonce(index), &chunk_ad(index, total), &mut out[pos..])
            .map_err(|_| CryptoError::TooLarge("블롭 조각"))?;
        out.extend_from_slice(&tag);
        hashes.push(*blake3::hash(&out[pos..]).as_bytes());
    }
    let manifest = BlobManifest {
        size: out.len() as u64,
        chunk_size: cs,
        chunks: hashes,
    };
    Ok((out, manifest))
}

fn split_tag(chunk: &[u8]) -> Result<(&[u8], &Tag)> {
    if chunk.len() < TAG_LEN {
        return Err(CryptoError::Malformed);
    }
    let (body, tag) = chunk.split_at(chunk.len() - TAG_LEN);
    Ok((body, Tag::from_slice(tag)))
}

/// 매니페스트 구조 검사. 암호화는 항상 조각을 하나 이상 만들므로, 조각이 0개인 매니페스트(키 검증 없이
/// 빈 평문으로 '복호화'되는 형태)는 거부합니다.
fn checked_total(manifest: &BlobManifest) -> Result<u32> {
    manifest.check()?;
    if manifest.chunks.is_empty() {
        return Err(CryptoError::Malformed);
    }
    Ok(manifest.chunks.len() as u32)
}

/// 받은 암호문 조각 하나를 검증(매니페스트 해시·길이)하고 복호화합니다.
pub fn decrypt_blob_chunk(
    key: &Secret32,
    manifest: &BlobManifest,
    index: u32,
    chunk: &[u8],
) -> Result<Vec<u8>> {
    let total = checked_total(manifest)?;
    if !manifest.verify_chunk(index, chunk) {
        return Err(CryptoError::ChunkHash);
    }
    let (body, tag) = split_tag(chunk)?;
    let mut buf = body.to_vec();
    blob_cipher(key)
        .decrypt_in_place_detached(&nonce(index), &chunk_ad(index, total), &mut buf, tag)
        .map_err(|_| CryptoError::Decrypt)?;
    Ok(buf)
}

/// 암호문 전체를 검증·복호화합니다.
pub fn decrypt_blob(key: &Secret32, manifest: &BlobManifest, ciphertext: &[u8]) -> Result<Vec<u8>> {
    let total = checked_total(manifest)?;
    if ciphertext.len() as u64 != manifest.size {
        return Err(CryptoError::BlobLength);
    }
    let cipher = blob_cipher(key);
    // 중간에 실패하면 이미 풀어 둔 앞 조각 평문을 0으로 덮어쓰고 버립니다(휘발성 첨부도 이 경로를 씀).
    let mut out = Zeroizing::new(Vec::with_capacity(plaintext_len(manifest) as usize));
    for (index, chunk) in ciphertext.chunks(manifest.chunk_size as usize).enumerate() {
        let index = index as u32;
        if !manifest.verify_chunk(index, chunk) {
            return Err(CryptoError::ChunkHash);
        }
        let (body, tag) = split_tag(chunk)?;
        let pos = out.len();
        out.extend_from_slice(body);
        cipher
            .decrypt_in_place_detached(&nonce(index), &chunk_ad(index, total), &mut out[pos..], tag)
            .map_err(|_| CryptoError::Decrypt)?;
    }
    Ok(std::mem::take(&mut *out))
}

/// 큰 파일을 조각 단위로 읽어 암호화합니다. 파일 전체를 메모리에 두 번 올리지 않습니다.
///
/// 전체 조각 수를 AD에 묶기 때문에 평문 길이를 미리 알아야 합니다. 읽는 도중 길이가 선언과 달라지면
/// (파일이 바뀌는 등) [`CryptoError::BlobLength`]로 실패합니다.
///
/// 논스가 조각 번호로 정해지므로, 실패 후 바뀐 파일로 다시 시도할 때는 **새 블롭 키**를 써야 합니다
/// (같은 키·같은 번호로 다른 평문을 암호화하면 키 스트림이 재사용됨).
pub struct BlobEncryptor<R> {
    key: Secret32,
    reader: R,
    chunk_size: u32,
    total: u32,
    next: u32,
    remaining: u64,
    hashes: Vec<[u8; 32]>,
    size: u64,
}

impl<R: Read> BlobEncryptor<R> {
    pub fn new(key: &Secret32, reader: R, plaintext_len: u64, chunk_size: u32) -> Result<Self> {
        let cs = effective_chunk_size(plaintext_len, chunk_size)?;
        let total = chunk_count(plaintext_len, cs) as u32;
        // 블롭 전용 키만 들고 있습니다(원래 키 사본을 오래 두지 않음).
        let key = derive(&key.0, b"yangpa/v1/blob", b"chunk-key");
        Ok(BlobEncryptor {
            key,
            reader,
            chunk_size: cs,
            total,
            next: 0,
            remaining: plaintext_len,
            hashes: Vec::with_capacity(total as usize),
            size: 0,
        })
    }

    pub fn chunk_size(&self) -> u32 {
        self.chunk_size
    }

    pub fn total_chunks(&self) -> u32 {
        self.total
    }

    /// 다음 암호문 조각 `(번호, 바이트)`. 모두 만들었으면 `None`.
    pub fn next_chunk(&mut self) -> Result<Option<(u32, Vec<u8>)>> {
        if self.next >= self.total {
            return Ok(None);
        }
        let index = self.next;
        let pt_chunk = self.chunk_size as u64 - TAG_LEN as u64;
        let n = self.remaining.min(pt_chunk) as usize;
        // 읽기·길이 검사에서 실패하면 버퍼에 평문이 남으므로 해제 시 0으로 덮어씁니다.
        let mut buf = Zeroizing::new(vec![0u8; n + TAG_LEN]);
        self.reader
            .read_exact(&mut buf[..n])
            .map_err(|e| match e.kind() {
                ErrorKind::UnexpectedEof => CryptoError::BlobLength,
                _ => CryptoError::Io(e),
            })?;
        self.remaining -= n as u64;
        if index + 1 == self.total && !at_eof(&mut self.reader)? {
            return Err(CryptoError::BlobLength);
        }
        let cipher = XChaCha20Poly1305::new((&self.key.0).into());
        let tag = cipher
            .encrypt_in_place_detached(&nonce(index), &chunk_ad(index, self.total), &mut buf[..n])
            .map_err(|_| CryptoError::TooLarge("블롭 조각"))?;
        buf[n..].copy_from_slice(&tag);
        self.hashes.push(*blake3::hash(&buf).as_bytes());
        self.size += buf.len() as u64;
        self.next += 1;
        Ok(Some((index, std::mem::take(&mut *buf))))
    }

    /// 모든 조각을 만든 뒤 매니페스트를 돌려줍니다.
    pub fn finish(self) -> Result<BlobManifest> {
        if self.next != self.total {
            return Err(CryptoError::BlobLength);
        }
        Ok(BlobManifest {
            size: self.size,
            chunk_size: self.chunk_size,
            chunks: self.hashes,
        })
    }
}

fn at_eof<R: Read>(r: &mut R) -> Result<bool> {
    let mut probe = [0u8; 1];
    loop {
        match r.read(&mut probe) {
            Ok(0) => return Ok(true),
            Ok(_) => return Ok(false),
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(CryptoError::Io(e)),
        }
    }
}

/// `reader`의 평문(`plaintext_len`바이트)을 암호화해 `writer`에 이어 씁니다.
pub fn encrypt_blob_stream<R: Read, W: Write>(
    key: &Secret32,
    reader: R,
    plaintext_len: u64,
    chunk_size: u32,
    mut writer: W,
) -> Result<BlobManifest> {
    let mut enc = BlobEncryptor::new(key, reader, plaintext_len, chunk_size)?;
    while let Some((_, chunk)) = enc.next_chunk()? {
        writer.write_all(&chunk)?;
    }
    writer.flush()?;
    enc.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    const CS: u32 = MIN_CHUNK_SIZE;
    const PT: usize = CS as usize - TAG_LEN;

    fn data(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i % 251) as u8 ^ (i / 997) as u8).collect()
    }

    #[test]
    fn roundtrip_various_sizes() {
        let k = Secret32::random();
        for n in [0usize, 1, PT - 1, PT, PT + 1, 3 * PT, 3 * PT + 5] {
            let pt = data(n);
            let (ct, m) = encrypt_blob(&k, &pt, CS);
            m.check().unwrap();
            assert_eq!(m.chunk_size, CS);
            assert_eq!(m.size as usize, ct.len());
            assert_eq!(m.chunks.len(), n.div_ceil(PT).max(1));
            assert_eq!(plaintext_len(&m), n as u64);
            // 매니페스트는 암호문 조각 기준(시더가 키 없이 검증).
            assert_eq!(m, BlobManifest::build(&ct, CS));
            // 마지막을 빼고 모든 암호문 조각은 정확히 chunk_size.
            for (i, c) in ct.chunks(CS as usize).enumerate() {
                assert!(m.verify_chunk(i as u32, c));
                if i + 1 < m.chunks.len() {
                    assert_eq!(c.len(), CS as usize);
                }
                let p = decrypt_blob_chunk(&k, &m, i as u32, c).unwrap();
                assert_eq!(p, pt[i * PT..((i + 1) * PT).min(n)]);
            }
            assert_eq!(decrypt_blob(&k, &m, &ct).unwrap(), pt);
        }
    }

    #[test]
    fn deterministic_per_key() {
        let k = Secret32::random();
        let pt = data(PT * 2 + 3);
        let (a, ma) = encrypt_blob(&k, &pt, CS);
        let (b, mb) = encrypt_blob(&k, &pt, CS);
        assert_eq!(a, b);
        assert_eq!(ma.id(), mb.id());
        let (c, mc) = encrypt_blob(&Secret32::random(), &pt, CS);
        assert_ne!(a, c);
        assert_ne!(ma.id(), mc.id());
    }

    #[test]
    fn tamper_reorder_truncate_rejected() {
        let k = Secret32::random();
        let pt = data(PT * 3);
        let (ct, m) = encrypt_blob(&k, &pt, CS);
        let chunk = |i: usize| &ct[i * CS as usize..((i + 1) * CS as usize).min(ct.len())];

        // 조각 변조: 해시에서 걸림.
        let mut bad = chunk(1).to_vec();
        bad[3] ^= 1;
        assert!(matches!(
            decrypt_blob_chunk(&k, &m, 1, &bad),
            Err(CryptoError::ChunkHash)
        ));
        // 다른 번호로 제출.
        assert!(matches!(
            decrypt_blob_chunk(&k, &m, 0, chunk(1)),
            Err(CryptoError::ChunkHash)
        ));
        assert!(decrypt_blob_chunk(&k, &m, 9, chunk(1)).is_err());
        // 다른 키.
        assert!(matches!(
            decrypt_blob_chunk(&Secret32::random(), &m, 1, chunk(1)),
            Err(CryptoError::Decrypt)
        ));

        // 공격자가 매니페스트까지 다시 만들어 조각 순서를 바꾸면 AD(번호)에서 걸림.
        let mut swapped = chunk(1).to_vec();
        swapped.extend_from_slice(chunk(0));
        swapped.extend_from_slice(chunk(2));
        let m2 = BlobManifest::build(&swapped, CS);
        assert!(matches!(
            decrypt_blob(&k, &m2, &swapped),
            Err(CryptoError::Decrypt)
        ));

        // 자르기: 앞 두 조각만으로 매니페스트를 다시 만들면 '마지막'·전체 수 표시가 맞지 않음.
        let truncated = &ct[..2 * CS as usize];
        let m3 = BlobManifest::build(truncated, CS);
        assert!(matches!(
            decrypt_blob(&k, &m3, truncated),
            Err(CryptoError::Decrypt)
        ));
        assert!(decrypt_blob_chunk(&k, &m3, 1, chunk(1)).is_err());

        // 길이 불일치.
        assert!(matches!(
            decrypt_blob(&k, &m, &ct[..ct.len() - 1]),
            Err(CryptoError::BlobLength)
        ));
        // 잘못된 매니페스트.
        let mut m4 = m.clone();
        m4.chunks.pop();
        assert!(decrypt_blob(&k, &m4, &ct).is_err());
    }

    #[test]
    fn zero_chunk_manifest_rejected() {
        // 조각이 0개인 매니페스트는 check()는 통과하지만, 키 검증 없이 빈 평문으로 열려서는 안 된다
        // (암호화는 빈 평문도 태그 조각 하나를 만든다).
        let m = BlobManifest {
            size: 0,
            chunk_size: CS,
            chunks: vec![],
        };
        m.check().unwrap();
        let k = Secret32::random();
        assert!(matches!(
            decrypt_blob(&k, &m, &[]),
            Err(CryptoError::Malformed)
        ));
        assert!(decrypt_blob_chunk(&k, &m, 0, &[]).is_err());
        // 진짜 빈 블롭은 키가 맞아야만 열린다.
        let (ct, real) = encrypt_blob(&k, &[], CS);
        assert_eq!(real.chunks.len(), 1);
        assert!(decrypt_blob(&k, &real, &ct).unwrap().is_empty());
        assert!(matches!(
            decrypt_blob(&Secret32::random(), &real, &ct),
            Err(CryptoError::Decrypt)
        ));
    }

    #[test]
    fn chunk_size_is_clamped_and_grown() {
        assert_eq!(effective_chunk_size(10, 1).unwrap(), MIN_CHUNK_SIZE);
        assert_eq!(effective_chunk_size(10, u32::MAX).unwrap(), MAX_CHUNK_SIZE);
        // MIN 조각 크기로는 조각 수 한도를 넘는 크기 → 조각 크기를 키움.
        let big = (MIN_CHUNK_SIZE as u64 - TAG_LEN as u64) * MAX_CHUNKS as u64 + 1;
        let cs = effective_chunk_size(big, MIN_CHUNK_SIZE).unwrap();
        assert!(cs > MIN_CHUNK_SIZE);
        assert!(chunk_count(big, cs) <= MAX_CHUNKS as u64);
        assert!(effective_chunk_size(u64::MAX / 2, MIN_CHUNK_SIZE).is_err());
        // 요청보다 작은 값으로 줄이지 않는다.
        let pt = data(100);
        let (_, m) = encrypt_blob(&Secret32::random(), &pt, 1);
        assert_eq!(m.chunk_size, MIN_CHUNK_SIZE);
    }

    #[test]
    fn locate_offsets() {
        let pt = data(PT * 2 + 10);
        let (_, m) = encrypt_blob(&Secret32::random(), &pt, CS);
        assert_eq!(locate(&m, 0), Some((0, 0)));
        assert_eq!(locate(&m, PT as u64 + 3), Some((1, 3)));
        assert_eq!(locate(&m, (PT * 2 + 9) as u64), Some((2, 9)));
        assert_eq!(locate(&m, (PT * 2 + 10) as u64), None);
    }

    #[test]
    fn streaming_matches_in_memory() {
        let k = Secret32::random();
        for n in [0usize, 5, PT, PT * 4 + 77] {
            let pt = data(n);
            let (ct, m) = encrypt_blob(&k, &pt, CS);
            let mut out = Vec::new();
            let ms = encrypt_blob_stream(&k, &pt[..], n as u64, CS, &mut out).unwrap();
            assert_eq!(ms, m);
            assert_eq!(out, ct);

            let mut enc = BlobEncryptor::new(&k, &pt[..], n as u64, CS).unwrap();
            assert_eq!(enc.total_chunks() as usize, m.chunks.len());
            let mut seen = 0;
            while let Some((i, c)) = enc.next_chunk().unwrap() {
                assert_eq!(i, seen);
                assert!(m.verify_chunk(i, &c));
                seen += 1;
            }
            assert_eq!(enc.finish().unwrap(), m);
        }
    }

    #[test]
    fn streaming_length_mismatch() {
        let k = Secret32::random();
        let pt = data(PT + 10);
        // 선언보다 짧은 입력.
        let mut out = Vec::new();
        assert!(matches!(
            encrypt_blob_stream(&k, &pt[..], pt.len() as u64 + 1, CS, &mut out),
            Err(CryptoError::BlobLength)
        ));
        // 선언보다 긴 입력.
        let mut out = Vec::new();
        assert!(matches!(
            encrypt_blob_stream(&k, &pt[..], pt.len() as u64 - 1, CS, &mut out),
            Err(CryptoError::BlobLength)
        ));
        // 조각을 다 만들기 전에 끝내면 실패.
        let mut enc = BlobEncryptor::new(&k, &pt[..], pt.len() as u64, CS).unwrap();
        enc.next_chunk().unwrap();
        assert!(enc.finish().is_err());
    }
}
