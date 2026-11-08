//! 붙여넣은 첨부(`blob.paste`): 클립보드의 이미지·파일은 웹뷰에서 경로 없이 바이트로만 오므로, 앱이 몇 조각으로
//! 나눠 미리 올리고 `message.send`·`forum.create_post` 첨부에 경로 대신 `paste` ID로 넘깁니다.
//!
//! RAM에만 둡니다. 휘발성 채널에서도 쓰므로 디스크·임시 파일에 쓰지 않고, 처음에 전체 크기만큼 잡아(이어 붙일 때
//! 다시 잡으며 옛 버퍼를 남기지 않게) 지울 때 0으로 덮어씁니다. 앱이 지우지 않아도 [`TTL`] 뒤 사라지고, 모두 합쳐
//! [`MAX_TOTAL`]을 넘지 못합니다. 보내기는 ID로 내용을 빌려 쓸 뿐 지우지 않습니다(보내기에 실패하면 다시 보낼 수 있게).

use std::collections::HashMap;
use std::fs::File;
use std::io::{Cursor, Read, Seek, SeekFrom};
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine;
use parking_lot::Mutex;
use yangpa_rpc::RpcError;
use zeroize::Zeroizing;

use crate::blobs::strip::Spliced;

/// 한 번에 이어 붙이는 조각 최대 크기(base64로 늘어도 RPC 프레임 16MiB 안).
pub(crate) const MAX_PIECE: usize = 4 * 1024 * 1024;
/// 모든 붙여넣기를 합친 RAM 상한.
pub(crate) const MAX_TOTAL: u64 = 512 * 1024 * 1024;
/// 붙여넣고 이만큼 지나면 지웁니다.
const TTL: Duration = Duration::from_secs(30 * 60);
/// 동시에 들고 있는 붙여넣기 수 상한.
const MAX_PASTES: usize = 64;

struct Entry {
    data: Arc<Zeroizing<Vec<u8>>>,
    size: u64,
    at: Instant,
}

#[derive(Default)]
pub(crate) struct Pastes {
    map: Mutex<HashMap<String, Entry>>,
}

/// 붙여넣은 바이트(빌림). 마지막 것이 사라질 때 0으로 덮어씁니다.
#[derive(Clone)]
pub(crate) struct Shared(Arc<Zeroizing<Vec<u8>>>);

impl AsRef<[u8]> for Shared {
    fn as_ref(&self) -> &[u8] {
        self.0.as_slice()
    }
}

/// 첨부 원본: 파일, 또는 붙여넣은 바이트. 둘 다 처음부터 다시 읽을 수 있어 같은 준비 과정을 씁니다.
pub(crate) enum Source {
    File(File),
    Mem(Cursor<Shared>),
    /// 메타데이터를 뺀 것(`blobs::strip`).
    Spliced(Box<Spliced<Source>>),
}

impl Source {
    pub(crate) fn mem(data: Shared) -> Self {
        Source::Mem(Cursor::new(data))
    }
}

impl Read for Source {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Source::File(f) => f.read(buf),
            Source::Mem(c) => c.read(buf),
            Source::Spliced(s) => s.read(buf),
        }
    }
}

impl Seek for Source {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        match self {
            Source::File(f) => f.seek(pos),
            Source::Mem(c) => c.seek(pos),
            Source::Spliced(s) => s.seek(pos),
        }
    }
}

fn sweep(map: &mut HashMap<String, Entry>) {
    let now = Instant::now();
    map.retain(|_, e| now.duration_since(e.at) < TTL);
}

fn decode(b64: &str) -> Result<Zeroizing<Vec<u8>>, RpcError> {
    if b64.len() > MAX_PIECE.div_ceil(3) * 4 {
        return Err(RpcError::invalid_input(format!(
            "붙여넣기 조각은 {}MiB 이하여야 합니다",
            MAX_PIECE / (1024 * 1024)
        )));
    }
    base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map(Zeroizing::new)
        .map_err(|_| RpcError::invalid_input("붙여넣기 데이터가 base64가 아닙니다"))
}

impl Pastes {
    /// 새로 만들거나(`paste` 없음, `size` 필요) 이어 붙입니다. (ID, 지금까지 받은 바이트).
    pub(crate) fn put(
        &self,
        paste: Option<&str>,
        size: Option<u64>,
        data_base64: &str,
    ) -> Result<(String, u64), RpcError> {
        let piece = decode(data_base64)?;
        let mut map = self.map.lock();
        sweep(&mut map);
        let id = match paste {
            Some(id) => id.to_owned(),
            None => {
                let size = size.ok_or_else(|| {
                    RpcError::invalid_input("처음 붙여넣을 때는 전체 크기(size)가 필요합니다")
                })?;
                if size == 0 {
                    return Err(RpcError::invalid_input("빈 파일은 붙여넣을 수 없습니다"));
                }
                let used: u64 = map.values().map(|e| e.size).sum();
                if map.len() >= MAX_PASTES || used.saturating_add(size) > MAX_TOTAL {
                    return Err(RpcError::invalid_input(format!(
                        "붙여넣은 첨부가 너무 많거나 큽니다(모두 합쳐 {}MiB까지). 큰 파일은 첨부 버튼이나 끌어 놓기로 올려 주세요",
                        MAX_TOTAL / (1024 * 1024)
                    )));
                }
                let id = bs58::encode(rand::random::<[u8; 16]>()).into_string();
                map.insert(
                    id.clone(),
                    Entry {
                        data: Arc::new(Zeroizing::new(Vec::with_capacity(size as usize))),
                        size,
                        at: Instant::now(),
                    },
                );
                id
            }
        };
        let e = map
            .get_mut(&id)
            .ok_or_else(|| RpcError::not_found("붙여넣기가 없거나 만료되었습니다"))?;
        let data = Arc::get_mut(&mut e.data)
            .ok_or_else(|| RpcError::conflict("보내는 중인 붙여넣기에는 이어 붙일 수 없습니다"))?;
        if data.len() as u64 + piece.len() as u64 > e.size {
            map.remove(&id);
            return Err(RpcError::invalid_input(
                "붙여넣기가 처음 알린 크기보다 큽니다",
            ));
        }
        // 처음에 전체 크기만큼 잡아 두었으므로 다시 잡지 않습니다(옛 버퍼에 평문이 남지 않게).
        data.extend_from_slice(&piece);
        e.at = Instant::now();
        Ok((id, data.len() as u64))
    }

    pub(crate) fn discard(&self, paste: &str) {
        self.map.lock().remove(paste);
    }

    /// 다 올린 붙여넣기의 내용(빌림).
    pub(crate) fn get(&self, paste: &str) -> Result<Shared, RpcError> {
        let mut map = self.map.lock();
        sweep(&mut map);
        let e = map.get(paste).ok_or_else(|| {
            RpcError::not_found("붙여넣은 첨부가 만료되었습니다. 다시 붙여넣어 주세요")
        })?;
        if e.data.len() as u64 != e.size {
            return Err(RpcError::invalid_input(
                "붙여넣은 첨부를 아직 다 올리지 않았습니다",
            ));
        }
        Ok(Shared(e.data.clone()))
    }
}

impl Shared {
    pub(crate) fn new(v: Vec<u8>) -> Self {
        Shared(Arc::new(Zeroizing::new(v)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b64(b: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(b)
    }

    #[test]
    fn put_in_pieces_then_read_and_discard() {
        let p = Pastes::default();
        assert!(
            p.put(None, None, &b64(b"ab")).is_err(),
            "처음엔 크기가 필요"
        );
        let (id, n) = p.put(None, Some(5), &b64(b"ab")).unwrap();
        assert_eq!(n, 2);
        assert!(p.get(&id).is_err(), "다 올리기 전");
        let (_, n) = p.put(Some(&id), None, &b64(b"cde")).unwrap();
        assert_eq!(n, 5);
        let got = p.get(&id).unwrap();
        let mut s = Source::mem(got.clone());
        let mut out = Vec::new();
        s.read_to_end(&mut out).unwrap();
        assert_eq!(out, b"abcde");
        // 빌려 쓰는 동안은 이어 붙이지 못합니다.
        assert!(p.put(Some(&id), None, &b64(b"x")).is_err());
        drop((got, s));
        p.discard(&id);
        assert!(p.get(&id).is_err());
    }

    #[test]
    fn rejects_overflow_and_unknown() {
        let p = Pastes::default();
        let (id, _) = p.put(None, Some(2), &b64(b"ab")).unwrap();
        assert!(p.put(Some(&id), None, &b64(b"c")).is_err());
        assert!(p.get(&id).is_err(), "넘치면 지움");
        assert!(p.put(Some("없음"), None, &b64(b"a")).is_err());
        assert!(p.put(None, Some(MAX_TOTAL + 1), &b64(b"a")).is_err());
        assert!(p.put(None, Some(3), "!!").is_err());
    }
}
