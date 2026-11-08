//! 배포 목록(scripts/*.txt)에 sha256을 고정한 파일 확인. 모델·ffmpeg 폴더는 사용자가 쓸 수 있는 곳이라, 바뀐 DLL·실행
//! 파일을 열거나 띄우기 전에 배포본과 같은지 봅니다.
//!
//! 목록 형식: `<폴더 안 경로> <sha256> <주소> [zip 안 경로]`, `#`으로 시작하는 줄은 주석.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use parking_lot::Mutex;
use sha2::{Digest, Sha256};

/// 목록에서 그 경로(`/` 구분)의 sha256. 없으면 `None`.
pub fn sha256_of(list: &'static str, rel: &str) -> Option<&'static str> {
    list.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .find_map(|l| {
            let mut f = l.split_whitespace();
            (f.next() == Some(rel)).then(|| f.next()).flatten()
        })
}

/// 목록의 모든 경로.
pub fn paths(list: &'static str) -> impl Iterator<Item = &'static str> {
    list.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| l.split_whitespace().next())
}

/// 해시한 파일(경로, 크기, 수정 시각, sha256). 같은 파일을 다시 해시하지 않습니다(수백 MB).
static HASHED: Mutex<Vec<(PathBuf, u64, SystemTime, String)>> = Mutex::new(Vec::new());

/// 파일의 sha256(소문자 16진). 크기·수정 시각이 같으면 지난 값을 씁니다.
pub fn sha256_cached(path: &Path) -> std::io::Result<String> {
    let meta = std::fs::metadata(path)?;
    let stamp = (meta.len(), meta.modified().unwrap_or(std::time::UNIX_EPOCH));
    if let Some((.., h)) = HASHED
        .lock()
        .iter()
        .find(|(p, len, at, _)| p == path && (*len, *at) == stamp)
    {
        return Ok(h.clone());
    }
    let h = sha256_reader(&mut std::fs::File::open(path)?)?;
    let mut seen = HASHED.lock();
    seen.retain(|(p, ..)| p != path);
    seen.push((path.to_path_buf(), stamp.0, stamp.1, h.clone()));
    Ok(h)
}

/// 끝까지 읽은 sha256(소문자 16진). 이미 연 파일을 그대로 해시할 때 씁니다.
pub fn sha256_reader(r: &mut impl Read) -> std::io::Result<String> {
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = r.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST: &str = "# 주석\n\na/b.bin 00ff https://example.invalid/x.zip a/b.bin\nc.txt 11aa https://example.invalid/c\n";

    #[test]
    fn reads_list() {
        assert_eq!(sha256_of(LIST, "a/b.bin"), Some("00ff"));
        assert_eq!(sha256_of(LIST, "c.txt"), Some("11aa"));
        assert_eq!(sha256_of(LIST, "a"), None);
        assert_eq!(paths(LIST).collect::<Vec<_>>(), vec!["a/b.bin", "c.txt"]);
    }

    #[test]
    fn hashes_and_notices_changes() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f");
        std::fs::write(&p, b"abc").unwrap();
        assert_eq!(
            sha256_cached(&p).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        std::fs::write(&p, b"abcd").unwrap();
        assert_eq!(
            sha256_cached(&p).unwrap(),
            "88d4266fd4e6338d13b845fcf289579d209c897823b9217da3e161936f031589"
        );
    }
}
