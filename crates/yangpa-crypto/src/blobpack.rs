//! 블롭 평문의 압축 머리글: 조각 암호화([`crate::blob`]) 앞에서 파일 내용을 OpenZL로 압축합니다.
//!
//! 블롭 평문 = 머리글 14바이트(`"YPZL"`, 판 1, 방식, 내용 길이 u64 LE) + 몸통.
//! - [`BlobLayout::Raw`]: 몸통이 파일 그대로. 사진·영상처럼 줄지 않는 파일과 [`MAX_ZL_CONTENT`]보다 큰 파일. 위치를
//!   머리글 길이만큼 밀어 조각 단위 범위 읽기(영상 넘겨 보기)가 그대로 됩니다.
//! - [`BlobLayout::Zl`]: 몸통이 OpenZL 프레임. [`MIN_SAVING`] 이상 줄 때만. 읽을 때는 전체를 풀어야 하므로 크기를
//!   제한합니다(풀린 크기 = 머리글의 내용 길이, [`MAX_ZL_CONTENT`] 이하만 받아들임).
//! - [`BlobLayout::Legacy`]: 머리글이 없는 예전 판 블롭(평문이 곧 파일).
//!
//! 같은 파일·키는 늘 같은 블롭이 됩니다(다시 잇기가 블롭 ID를 다시 계산함): 압축 여부와 OpenZL 출력이 결정적입니다.

use std::io::{Chain, Cursor, Read};

use yangpa_core::blob::BlobManifest;
use zeroize::Zeroizing;

use crate::blob::{locate, plaintext_chunk_len, plaintext_len};
use crate::body::zl_err;
use crate::error::{CryptoError, Result};

pub const MAGIC: [u8; 4] = *b"YPZL";
pub const VERSION: u8 = 1;
pub const HEADER_LEN: usize = 14;
const MODE_RAW: u8 = 0;
const MODE_ZL: u8 = 1;
/// 이보다 큰 파일은 압축하지 않습니다(읽을 때 전체를 메모리에 풀어야 하고, 큰 파일은 대개 영상·압축 파일).
pub const MAX_ZL_CONTENT: u64 = 32 * 1024 * 1024;
/// 압축본이 원본의 이 비율(%) 이하일 때만 압축합니다(조금 주는 정도면 범위 읽기를 잃을 만하지 않음).
pub const MIN_SAVING: u64 = 95;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlobLayout {
    Legacy,
    Raw,
    Zl,
}

fn header(mode: u8, len: u64) -> [u8; HEADER_LEN] {
    let mut h = [0u8; HEADER_LEN];
    h[..4].copy_from_slice(&MAGIC);
    h[4] = VERSION;
    h[5] = mode;
    h[6..].copy_from_slice(&len.to_le_bytes());
    h
}

/// 메모리의 내용을 블롭 평문으로(머리글 + 그대로 또는 OpenZL).
pub fn pack_blob(content: &[u8]) -> Zeroizing<Vec<u8>> {
    let len = content.len() as u64;
    if len <= MAX_ZL_CONTENT
        && len >= yangpa_zl::MIN_COMPRESS as u64
        && let Ok(z) = yangpa_zl::compress(content)
        && (z.len() as u64) * 100 <= len * MIN_SAVING
    {
        let mut out = Zeroizing::new(Vec::with_capacity(HEADER_LEN + z.len()));
        out.extend_from_slice(&header(MODE_ZL, len));
        out.extend_from_slice(&z);
        return out;
    }
    let mut out = Zeroizing::new(Vec::with_capacity(HEADER_LEN + content.len()));
    out.extend_from_slice(&header(MODE_RAW, len));
    out.extend_from_slice(content);
    out
}

/// [`BlobEncryptor`](crate::BlobEncryptor)에 넣을 평문 읽기기.
pub enum PackedSource<R> {
    Mem(Cursor<Zeroizing<Vec<u8>>>),
    Stream(Chain<Cursor<[u8; HEADER_LEN]>, R>),
}

impl<R: Read> Read for PackedSource<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            PackedSource::Mem(c) => c.read(buf),
            PackedSource::Stream(c) => c.read(buf),
        }
    }
}

/// `len`바이트 내용을 읽는 `reader`를 블롭 평문 읽기기로 감쌉니다. 반환: (읽기기, 평문 길이).
///
/// [`MAX_ZL_CONTENT`] 이하면 전부 읽어 압축해 보고(줄지 않으면 그대로), 더 크면 머리글만 앞에 붙여 흘려 보냅니다.
/// 내용이 `len`보다 짧거나 길면 [`CryptoError::BlobLength`].
pub fn packed_source<R: Read>(mut reader: R, len: u64) -> Result<(PackedSource<R>, u64)> {
    if len > MAX_ZL_CONTENT {
        let src = Cursor::new(header(MODE_RAW, len)).chain(reader);
        return Ok((PackedSource::Stream(src), len + HEADER_LEN as u64));
    }
    let mut content = Zeroizing::new(Vec::with_capacity(len as usize));
    (&mut reader)
        .take(len + 1)
        .read_to_end(&mut content)
        .map_err(CryptoError::Io)?;
    if content.len() as u64 != len {
        return Err(CryptoError::BlobLength);
    }
    let packed = pack_blob(&content);
    let n = packed.len() as u64;
    Ok((PackedSource::Mem(Cursor::new(packed)), n))
}

/// 첫 조각 평문으로 방식과 내용 길이를 봅니다. 머리글이 이상하면(판·방식·길이) [`CryptoError::Malformed`].
pub fn parse_layout(first_chunk: &[u8], manifest: &BlobManifest) -> Result<(BlobLayout, u64)> {
    let total = plaintext_len(manifest);
    if first_chunk.len() < HEADER_LEN || first_chunk[..4] != MAGIC {
        return Ok((BlobLayout::Legacy, total));
    }
    if first_chunk[4] != VERSION {
        return Err(CryptoError::Malformed);
    }
    let len = u64::from_le_bytes(first_chunk[6..HEADER_LEN].try_into().expect("8바이트"));
    match first_chunk[5] {
        MODE_RAW if len == total - HEADER_LEN as u64 => Ok((BlobLayout::Raw, len)),
        // 압축본은 내용보다 작을 때만 만듦: 전체를 모아 풀 때 쓰는 메모리도 이것으로 묶임.
        MODE_ZL if len <= MAX_ZL_CONTENT && total <= len + HEADER_LEN as u64 => {
            Ok((BlobLayout::Zl, len))
        }
        _ => Err(CryptoError::Malformed),
    }
}

/// [`BlobReader`]의 실패: 조각을 가져오는 쪽(`fetch`·`sink`)의 오류이거나 머리글·압축이 깨진 것.
#[derive(Debug)]
pub enum ReadError<E> {
    Fetch(E),
    Crypto(CryptoError),
}

impl<E> From<CryptoError> for ReadError<E> {
    fn from(e: CryptoError) -> Self {
        ReadError::Crypto(e)
    }
}

type RResult<T, E> = std::result::Result<T, ReadError<E>>;

/// 블롭 내용(사용자가 보는 파일) 읽기. `fetch(i)`는 조각 `i`의 복호화한 평문을 돌려줍니다.
pub struct BlobReader<'m, F> {
    manifest: &'m BlobManifest,
    fetch: F,
    layout: BlobLayout,
    len: u64,
    first: Option<Zeroizing<Vec<u8>>>,
    unpacked: Option<Zeroizing<Vec<u8>>>,
}

impl<'m, F, E> BlobReader<'m, F>
where
    F: FnMut(u32) -> std::result::Result<Zeroizing<Vec<u8>>, E>,
{
    /// 첫 조각을 읽어 방식을 정합니다.
    pub fn open(manifest: &'m BlobManifest, mut fetch: F) -> RResult<Self, E> {
        if manifest.chunks.is_empty() {
            return Err(CryptoError::Malformed.into());
        }
        let first = fetch(0).map_err(ReadError::Fetch)?;
        let (layout, len) = parse_layout(&first, manifest)?;
        Ok(BlobReader {
            manifest,
            fetch,
            layout,
            len,
            first: Some(first),
            unpacked: None,
        })
    }

    pub fn layout(&self) -> BlobLayout {
        self.layout
    }

    /// 내용 길이(바이트).
    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn chunk(&mut self, i: u32) -> RResult<Zeroizing<Vec<u8>>, E> {
        if i == 0
            && let Some(c) = self.first.take()
        {
            return Ok(c);
        }
        (self.fetch)(i).map_err(ReadError::Fetch)
    }

    fn offset(&self) -> u64 {
        match self.layout {
            BlobLayout::Legacy => 0,
            _ => HEADER_LEN as u64,
        }
    }

    fn unpack(&mut self) -> RResult<(), E> {
        if self.unpacked.is_some() {
            return Ok(());
        }
        let total = plaintext_len(self.manifest);
        let mut frame = Zeroizing::new(Vec::with_capacity(total as usize));
        for i in 0..self.manifest.chunks.len() as u32 {
            let pt = self.chunk(i)?;
            frame.extend_from_slice(&pt);
        }
        let out = yangpa_zl::decompress(&frame[HEADER_LEN..], self.len as usize).map_err(zl_err)?;
        if out.len() as u64 != self.len {
            return Err(CryptoError::Malformed.into());
        }
        self.unpacked = Some(out);
        Ok(())
    }

    /// 내용의 `offset`부터 최대 `len`바이트. 끝을 넘으면 거기까지.
    pub fn read(&mut self, offset: u64, len: u64) -> RResult<Zeroizing<Vec<u8>>, E> {
        let start = offset.min(self.len);
        let end = start.saturating_add(len).min(self.len);
        if self.layout == BlobLayout::Zl {
            self.unpack()?;
            let all = self.unpacked.as_ref().expect("방금 풂");
            return Ok(Zeroizing::new(all[start as usize..end as usize].to_vec()));
        }
        let shift = self.offset();
        let mut out = Zeroizing::new(Vec::with_capacity((end - start) as usize));
        let (mut pos, end) = (start + shift, end + shift);
        while pos < end {
            let Some((i, within)) = locate(self.manifest, pos) else {
                break;
            };
            let pt = self.chunk(i)?;
            let s = within as usize;
            let take = ((end - pos) as usize).min(pt.len().saturating_sub(s));
            if take == 0 {
                break;
            }
            out.extend_from_slice(&pt[s..s + take]);
            pos += take as u64;
        }
        Ok(out)
    }

    /// 내용 전체를 앞에서부터 조각 크기 남짓씩 `sink`에 넘깁니다(파일 저장·내용 검사).
    pub fn for_each(
        &mut self,
        mut sink: impl FnMut(&[u8]) -> std::result::Result<(), E>,
    ) -> RResult<(), E> {
        if self.layout == BlobLayout::Zl {
            self.unpack()?;
            let per = plaintext_chunk_len(self.manifest).max(1) as usize;
            let all = self.unpacked.take().expect("방금 풂");
            for piece in all.chunks(per) {
                sink(piece).map_err(ReadError::Fetch)?;
            }
            return Ok(());
        }
        let shift = self.offset() as usize;
        for i in 0..self.manifest.chunks.len() as u32 {
            let pt = self.chunk(i)?;
            let piece = if i == 0 {
                &pt[shift.min(pt.len())..]
            } else {
                &pt[..]
            };
            if !piece.is_empty() {
                sink(piece).map_err(ReadError::Fetch)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blob::{BlobEncryptor, decrypt_blob_chunk, encrypt_blob};
    use yangpa_core::blob::MIN_CHUNK_SIZE;
    use yangpa_core::body::Secret32;

    fn text(n: usize) -> Vec<u8> {
        let mut v = Vec::new();
        let mut i = 0u32;
        while v.len() < n {
            v.extend(format!("{i}: 양파 첨부 문서 줄\n").as_bytes());
            i += 1;
        }
        v.truncate(n);
        v
    }

    fn random(n: usize) -> Vec<u8> {
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect()
    }

    fn encrypt(key: &Secret32, content: &[u8]) -> (Vec<u8>, BlobManifest) {
        let (src, n) = packed_source(content, content.len() as u64).unwrap();
        let mut enc = BlobEncryptor::new(key, src, n, MIN_CHUNK_SIZE).unwrap();
        let mut ct = Vec::new();
        while let Some((_, c)) = enc.next_chunk().unwrap() {
            ct.extend(c);
        }
        (ct, enc.finish().unwrap())
    }

    fn reader<'m>(
        key: &'m Secret32,
        ct: &'m [u8],
        m: &'m BlobManifest,
    ) -> BlobReader<'m, impl FnMut(u32) -> Result<Zeroizing<Vec<u8>>> + 'm> {
        let cs = m.chunk_size as usize;
        BlobReader::open(m, move |i| {
            let c = &ct[i as usize * cs..((i as usize + 1) * cs).min(ct.len())];
            decrypt_blob_chunk(key, m, i, c).map(Zeroizing::new)
        })
        .unwrap()
    }

    fn read_all<F: FnMut(u32) -> Result<Zeroizing<Vec<u8>>>>(r: &mut BlobReader<'_, F>) -> Vec<u8> {
        let mut all = Vec::new();
        r.for_each(|p| {
            all.extend_from_slice(p);
            Ok(())
        })
        .unwrap();
        all
    }

    #[test]
    fn text_is_compressed_and_reads_back() {
        let key = Secret32::random();
        let content = text(300_000);
        let (ct, m) = encrypt(&key, &content);
        assert!(ct.len() < content.len() / 5, "{}", ct.len());
        let mut r = reader(&key, &ct, &m);
        assert_eq!(r.layout(), BlobLayout::Zl);
        assert_eq!(r.len(), content.len() as u64);
        assert_eq!(&r.read(1000, 5000).unwrap()[..], &content[1000..6000]);
        assert_eq!(&r.read(299_990, 100).unwrap()[..], &content[299_990..]);
        assert_eq!(read_all(&mut r), content);
    }

    #[test]
    fn incompressible_stays_raw_with_range_reads() {
        let key = Secret32::random();
        let content = random(200_000);
        let (ct, m) = encrypt(&key, &content);
        assert_eq!(ct.len() as u64, m.size);
        assert_eq!(plaintext_len(&m), content.len() as u64 + HEADER_LEN as u64);
        let mut r = reader(&key, &ct, &m);
        assert_eq!(r.layout(), BlobLayout::Raw);
        assert_eq!(r.len(), content.len() as u64);
        // 조각 경계를 걸치는 읽기
        let per = plaintext_chunk_len(&m);
        let at = per - 20;
        assert_eq!(
            &r.read(at, 100).unwrap()[..],
            &content[at as usize..at as usize + 100]
        );
        assert_eq!(&r.read(0, 10).unwrap()[..], &content[..10]);
        assert!(r.read(content.len() as u64 + 5, 10).unwrap().is_empty());
        assert_eq!(read_all(&mut r), content);
    }

    #[test]
    fn legacy_blobs_without_header_still_read() {
        let key = Secret32::random();
        let content = text(10_000);
        let (ct, m) = encrypt_blob(&key, &content, MIN_CHUNK_SIZE);
        let mut r = reader(&key, &ct, &m);
        assert_eq!(r.layout(), BlobLayout::Legacy);
        assert_eq!(r.len(), content.len() as u64);
        assert_eq!(&r.read(5, 50).unwrap()[..], &content[5..55]);
        assert_eq!(read_all(&mut r), content);
    }

    #[test]
    fn empty_and_tiny_contents() {
        let key = Secret32::random();
        for content in [Vec::new(), b"x".to_vec()] {
            let (ct, m) = encrypt(&key, &content);
            let mut r = reader(&key, &ct, &m);
            assert_eq!(r.layout(), BlobLayout::Raw);
            assert_eq!(r.len(), content.len() as u64);
            assert_eq!(read_all(&mut r), content);
        }
    }

    #[test]
    fn same_content_gives_same_blob() {
        let key = Secret32::random();
        let content = text(100_000);
        assert_eq!(encrypt(&key, &content).1, encrypt(&key, &content).1);
    }

    #[test]
    fn large_contents_stream_without_compression() {
        let content = vec![0u8; MAX_ZL_CONTENT as usize + 1];
        let (mut src, n) = packed_source(&content[..], content.len() as u64).unwrap();
        assert!(matches!(src, PackedSource::Stream(_)));
        assert_eq!(n, content.len() as u64 + HEADER_LEN as u64);
        let mut head = [0u8; HEADER_LEN];
        src.read_exact(&mut head).unwrap();
        assert_eq!(head, header(MODE_RAW, content.len() as u64));
    }

    #[test]
    fn length_mismatch_and_bad_headers_are_rejected() {
        assert!(matches!(
            packed_source(&b"abc"[..], 4),
            Err(CryptoError::BlobLength)
        ));
        assert!(matches!(
            packed_source(&b"abcde"[..], 4),
            Err(CryptoError::BlobLength)
        ));
        let key = Secret32::random();
        // 풀린 길이가 한도를 넘는다고 적힌 머리글
        let mut bad = header(MODE_ZL, MAX_ZL_CONTENT + 1).to_vec();
        bad.extend_from_slice(&yangpa_zl::compress(&[0u8; 1000]).unwrap());
        let (ct, m) = encrypt_blob(&key, &bad, MIN_CHUNK_SIZE);
        let cs = m.chunk_size as usize;
        let open = BlobReader::open(&m, |i| {
            let c = &ct[i as usize * cs..((i as usize + 1) * cs).min(ct.len())];
            decrypt_blob_chunk(&key, &m, i, c).map(Zeroizing::new)
        });
        assert!(matches!(
            open.map(|_| ()),
            Err(ReadError::Crypto(CryptoError::Malformed))
        ));
        // 머리글 길이와 실제로 풀린 길이가 다름
        let mut lie = header(MODE_ZL, 999).to_vec();
        lie.extend_from_slice(&yangpa_zl::compress(&[0u8; 1000]).unwrap());
        let (ct, m) = encrypt_blob(&key, &lie, MIN_CHUNK_SIZE);
        let mut r = reader(&key, &ct, &m);
        assert!(r.read(0, 10).is_err());
        // 그대로 방식인데 길이가 맞지 않음
        let mut raw = header(MODE_RAW, 3).to_vec();
        raw.extend_from_slice(b"abcd");
        let (ct, m) = encrypt_blob(&key, &raw, MIN_CHUNK_SIZE);
        let cs = m.chunk_size as usize;
        let open = BlobReader::open(&m, |i| {
            let c = &ct[i as usize * cs..((i as usize + 1) * cs).min(ct.len())];
            decrypt_blob_chunk(&key, &m, i, c).map(Zeroizing::new)
        });
        assert!(matches!(
            open.map(|_| ()),
            Err(ReadError::Crypto(CryptoError::Malformed))
        ));
    }
}
