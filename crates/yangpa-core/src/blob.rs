//! 블롭(첨부·이모지·봉인된 과거 기록 묶음) 매니페스트.
//!
//! 블롭 내용은 이미 암호화된 바이트열이고, 조각마다 BLAKE3 해시를 매니페스트에 둡니다.
//! 블롭 ID = BLAKE3(postcard(매니페스트))이므로, 시더는 키 없이도 조각을 검증·제공할 수 있습니다.

use serde::{Deserialize, Serialize};

use crate::error::CoreError;
use crate::ids::BlobId;

/// 기본 조각 크기(256KiB). P3에서 측정 후 조정합니다.
pub const DEFAULT_CHUNK_SIZE: u32 = 256 * 1024;
pub const MIN_CHUNK_SIZE: u32 = 16 * 1024;
pub const MAX_CHUNK_SIZE: u32 = 4 * 1024 * 1024;
/// 블롭 하나의 최대 조각 수(약 64GiB/256KiB).
pub const MAX_CHUNKS: usize = 1 << 18;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobManifest {
    pub size: u64,
    pub chunk_size: u32,
    pub chunks: Vec<[u8; 32]>,
}

impl BlobManifest {
    /// 암호화된 내용 전체로 매니페스트를 만듭니다.
    pub fn build(content: &[u8], chunk_size: u32) -> Self {
        let chunks = content
            .chunks(chunk_size as usize)
            .map(|c| *blake3::hash(c).as_bytes())
            .collect();
        BlobManifest {
            size: content.len() as u64,
            chunk_size,
            chunks,
        }
    }

    pub fn id(&self) -> BlobId {
        let mut h = blake3::Hasher::new();
        h.update(b"yangpa/v1/blob\0");
        h.update(&postcard::to_stdvec(self).expect("직렬화 실패 없음"));
        BlobId(*h.finalize().as_bytes())
    }

    pub fn check(&self) -> Result<(), CoreError> {
        if !(MIN_CHUNK_SIZE..=MAX_CHUNK_SIZE).contains(&self.chunk_size) {
            return Err(CoreError::LimitExceeded("조각 크기"));
        }
        if self.chunks.len() > MAX_CHUNKS {
            return Err(CoreError::LimitExceeded("조각 수"));
        }
        let expected = self.size.div_ceil(self.chunk_size as u64) as usize;
        if expected != self.chunks.len() {
            return Err(CoreError::InvalidEncoding);
        }
        Ok(())
    }

    /// 받은 조각을 즉시 검증합니다.
    pub fn verify_chunk(&self, index: u32, data: &[u8]) -> bool {
        let Some(expected) = self.chunks.get(index as usize) else {
            return false;
        };
        let want_len = self.chunk_len(index);
        data.len() as u64 == want_len && blake3::hash(data).as_bytes() == expected
    }

    pub fn chunk_len(&self, index: u32) -> u64 {
        let start = index as u64 * self.chunk_size as u64;
        (self.size.saturating_sub(start)).min(self.chunk_size as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_verifies_chunks() {
        let data: Vec<u8> = (0..(MIN_CHUNK_SIZE as usize * 2 + 100))
            .map(|i| (i / 7 + i / 1000) as u8)
            .collect();
        let m = BlobManifest::build(&data, MIN_CHUNK_SIZE);
        m.check().unwrap();
        assert_eq!(m.chunks.len(), 3);
        assert!(m.verify_chunk(2, &data[MIN_CHUNK_SIZE as usize * 2..]));
        assert!(!m.verify_chunk(1, &data[..MIN_CHUNK_SIZE as usize]));
        assert_eq!(m.chunk_len(2), 100);
        assert_ne!(m.id(), BlobManifest::build(&data[1..], MIN_CHUNK_SIZE).id());
    }
}
