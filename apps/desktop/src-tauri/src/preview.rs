//! 보내기 전 동영상·음성 미리보기: `yangpa-blob://localhost/local/<토큰>`(윈도: `http://yangpa-blob.localhost/local/<토큰>`).
//!
//! 데몬에는 영상 디코더가 없어서, 앱이 보내기 전에 웹뷰의 `<video>`·`<audio>`로 첫 장면·길이·크기를 읽어
//! `message.send`에 싣습니다(데몬이 검사하고 JPEG로 다시 만듦). 웹뷰는 파일 경로를 직접 열 수 없으므로 사용자가 고른
//! 파일을 `local_preview_open`으로 등록하면 무작위 토큰 주소로 그 파일 하나만 내줍니다.
//! - 앞부분 바이트로 가린, 웹뷰가 재생할 수 있는 동영상·음성 일반 파일만 등록합니다(그 밖은 거절, 앱은 힌트 없이 보냄).
//! - 등록은 최대 [`MAX_ENTRIES`]개이고 [`TTL`]이 지나면 지웁니다. 앱은 다 읽으면 `local_preview_close`로 닫습니다.
//! - 응답은 한 번에 최대 [`WINDOW`]바이트 구간이고, 캐시 금지·출처 제한 헤더는 블롭 응답과 같습니다.
//! - 경로·토큰은 로그에 남기지 않습니다.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use rand::RngCore;
use serde::Serialize;
use tauri::http::{StatusCode, header};
use tauri::{AppHandle, Manager, Runtime, State, UriSchemeResponder};

use crate::blob::{RangeSpec, base, error_response, parse_range, resolve_range};
use crate::error::CmdError;

/// 동시에 등록해 둘 수 있는 파일 수(첨부는 한 번에 10개까지).
pub const MAX_ENTRIES: usize = 16;
/// 등록이 남는 시간.
pub const TTL: Duration = Duration::from_secs(10 * 60);
/// 한 응답의 최대 바이트(동영상 요소는 구간을 나눠 다시 요청합니다).
pub const WINDOW: u64 = 4 * 1024 * 1024;
/// 주소 경로 앞부분.
pub const PREFIX: &str = "/local/";

/// 앞부분 바이트로 가린 형식 중 웹뷰가 재생할 수 있는 동영상·음성. 모르면 `None`.
pub fn sniff_media(head: &[u8]) -> Option<&'static str> {
    let starts = |p: &[u8]| head.starts_with(p);
    if head.len() >= 12 && &head[4..8] == b"ftyp" {
        return match &head[8..12] {
            b"M4A " | b"M4B " => Some("audio/mp4"),
            // 사진 형식(HEIC·AVIF)과 퀵타임(웹뷰마다 재생 여부가 다름)은 뺍니다.
            b"heic" | b"heix" | b"mif1" | b"avif" | b"avis" | b"qt  " => None,
            _ => Some("video/mp4"),
        };
    }
    if starts(&[0x1A, 0x45, 0xDF, 0xA3]) {
        Some("video/webm")
    } else if starts(b"OggS") {
        Some("audio/ogg")
    } else if head.len() >= 12 && starts(b"RIFF") && &head[8..12] == b"WAVE" {
        Some("audio/wav")
    } else if starts(b"ID3") {
        Some("audio/mpeg")
    } else {
        mpeg_audio(head)
    }
}

/// ID3 머리가 없는 MP3(MPEG 오디오 Layer II·III 프레임)와 ADTS AAC. 데몬의 형식 판별
/// (`crates/yangpa-daemon/src/blobs/media.rs`의 `mpeg_audio`)과 같은 규칙이라, 여기서 음성으로 보고 길이를 읽은 파일을
/// 데몬도 음성으로 봅니다. 동기 비트만이 아니라 판·층·비트율·표본율까지 봐 UTF-16 BOM(`FF FE`) 같은 우연한 일치를 거릅니다.
fn mpeg_audio(head: &[u8]) -> Option<&'static str> {
    let (&b0, &b1, &b2) = (head.first()?, head.get(1)?, head.get(2)?);
    if b0 != 0xFF || b1 & 0xE0 != 0xE0 {
        return None;
    }
    let layer = (b1 >> 1) & 0x3;
    if layer == 0 {
        // ADTS: 동기 12비트, 층 00, 표본율 색인 0~12.
        return (b1 & 0xF6 == 0xF0 && (b2 >> 2) & 0xF <= 12).then_some("audio/aac");
    }
    // 판 01은 예약. Layer I(층 11)은 드물고 `FF FE`와 겹쳐 뺍니다.
    let version = (b1 >> 3) & 0x3;
    let bitrate = b2 >> 4;
    let rate = (b2 >> 2) & 0x3;
    (version != 1 && layer != 3 && bitrate != 0 && bitrate != 0xF && rate != 3)
        .then_some("audio/mpeg")
}

struct Entry {
    path: PathBuf,
    size: u64,
    mime: &'static str,
    opened: Instant,
}

/// `local_preview_open`의 결과.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LocalPreview {
    pub token: String,
    pub mime: &'static str,
    pub size: u64,
}

/// 등록한 미리보기 파일(경로·크기·형식만, 내용은 들고 있지 않음).
#[derive(Default)]
pub struct LocalPreviews {
    entries: Mutex<HashMap<String, Entry>>,
}

fn new_token() -> String {
    let mut b = [0u8; 24];
    rand::rngs::OsRng.fill_bytes(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

impl LocalPreviews {
    /// 파일을 등록합니다. 동영상·음성이 아니면 거절합니다.
    pub fn open(&self, path: &str) -> Result<LocalPreview, CmdError> {
        let path = Path::new(path);
        if !path.is_absolute() {
            return Err(CmdError::invalid_params("절대 경로가 아닙니다"));
        }
        let path = path
            .canonicalize()
            .map_err(|_| CmdError::invalid_params("파일을 찾을 수 없습니다"))?;
        let mut f =
            File::open(&path).map_err(|_| CmdError::invalid_params("파일을 열 수 없습니다"))?;
        let meta = f
            .metadata()
            .map_err(|_| CmdError::invalid_params("파일을 열 수 없습니다"))?;
        if !meta.is_file() || meta.len() == 0 {
            return Err(CmdError::invalid_params("일반 파일이 아닙니다"));
        }
        let mut head = [0u8; 16];
        let n = read_up_to(&mut f, &mut head)
            .map_err(|_| CmdError::invalid_params("파일을 읽을 수 없습니다"))?;
        let mime = sniff_media(&head[..n])
            .ok_or_else(|| CmdError::invalid_params("미리 볼 수 있는 동영상·음성이 아닙니다"))?;
        let token = new_token();
        let mut m = self.entries.lock();
        m.retain(|_, e| e.opened.elapsed() < TTL);
        if m.len() >= MAX_ENTRIES {
            // 가장 오래된 등록을 닫습니다(닫지 않고 버린 미리보기가 쌓이지 않게).
            if let Some(oldest) = m
                .iter()
                .min_by_key(|(_, e)| e.opened)
                .map(|(k, _)| k.clone())
            {
                m.remove(&oldest);
            }
        }
        m.insert(
            token.clone(),
            Entry {
                path,
                size: meta.len(),
                mime,
                opened: Instant::now(),
            },
        );
        Ok(LocalPreview {
            token,
            mime,
            size: meta.len(),
        })
    }

    pub fn close(&self, token: &str) {
        self.entries.lock().remove(token);
    }

    fn get(&self, token: &str) -> Option<(PathBuf, u64, &'static str)> {
        let mut m = self.entries.lock();
        m.retain(|_, e| e.opened.elapsed() < TTL);
        m.get(token).map(|e| (e.path.clone(), e.size, e.mime))
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.lock().len()
    }
}

fn read_up_to(f: &mut File, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match f.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}

/// 요청 구간(없으면 처음부터)을 최대 [`WINDOW`]바이트로 자른 닫힌 구간. 만족할 수 없으면 `None`(416).
pub fn window(range: Option<RangeSpec>, total: u64) -> Option<(u64, u64)> {
    let (start, end) = resolve_range(range.unwrap_or(RangeSpec::From(0)), total)?;
    Some((start, end.min(start + WINDOW - 1)))
}

/// 파일에서 구간을 읽습니다. 그사이 파일이 줄었으면 오류.
fn read_window(path: &Path, start: u64, end: u64) -> std::io::Result<Vec<u8>> {
    let mut f = File::open(path)?;
    f.seek(SeekFrom::Start(start))?;
    let mut buf = vec![0u8; (end - start + 1) as usize];
    f.read_exact(&mut buf)?;
    Ok(buf)
}

#[tauri::command]
pub fn local_preview_open(
    previews: State<'_, LocalPreviews>,
    path: String,
) -> Result<LocalPreview, CmdError> {
    previews.open(&path)
}

#[tauri::command]
pub fn local_preview_close(previews: State<'_, LocalPreviews>, token: String) {
    previews.close(&token);
}

/// `yangpa-blob` 처리기에서 `/local/<토큰>` 요청을 넘겨받아 답합니다. 늘 구간 응답(206)으로 답합니다
/// (동영상 요소는 구간 요청을 보내고, 구간 없는 요청에도 206을 받아들입니다).
pub fn respond<R: Runtime>(
    app: &AppHandle<R>,
    token: &str,
    range_header: Option<&str>,
    origin: Option<String>,
    head: bool,
    responder: UriSchemeResponder,
) {
    let Some((path, total, mime)) = app.state::<LocalPreviews>().get(token) else {
        responder.respond(error_response(
            StatusCode::NOT_FOUND,
            origin.as_deref(),
            None,
        ));
        return;
    };
    let Some((start, end)) = window(range_header.and_then(parse_range), total) else {
        responder.respond(error_response(
            StatusCode::RANGE_NOT_SATISFIABLE,
            origin.as_deref(),
            Some(total),
        ));
        return;
    };
    tauri::async_runtime::spawn_blocking(move || {
        let body = if head {
            Ok(Vec::new())
        } else {
            read_window(&path, start, end)
        };
        let resp = body.ok().and_then(|body| {
            base(StatusCode::PARTIAL_CONTENT, origin.as_deref())
                .header(header::CONTENT_TYPE, mime)
                .header(header::ACCEPT_RANGES, "bytes")
                .header(header::CONTENT_LENGTH, end - start + 1)
                .header(
                    header::CONTENT_RANGE,
                    format!("bytes {start}-{end}/{total}"),
                )
                .body(Cow::Owned(body))
                .ok()
        });
        responder.respond(
            resp.unwrap_or_else(|| error_response(StatusCode::NOT_FOUND, origin.as_deref(), None)),
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, bytes: &[u8]) -> String {
        let p = dir.join(name);
        std::fs::write(&p, bytes).unwrap();
        p.to_string_lossy().into_owned()
    }

    #[test]
    fn sniffs_only_playable_media() {
        assert_eq!(
            sniff_media(b"\0\0\0\x18ftypisom\0\0\0\0"),
            Some("video/mp4")
        );
        assert_eq!(
            sniff_media(b"\0\0\0\x18ftypM4A \0\0\0\0"),
            Some("audio/mp4")
        );
        assert_eq!(sniff_media(b"\0\0\0\x18ftypheic\0\0\0\0"), None);
        assert_eq!(sniff_media(b"\0\0\0\x18ftypqt  \0\0\0\0"), None);
        assert_eq!(
            sniff_media(&[0x1A, 0x45, 0xDF, 0xA3, 1]),
            Some("video/webm")
        );
        assert_eq!(sniff_media(b"OggS\0"), Some("audio/ogg"));
        assert_eq!(sniff_media(b"RIFF\0\0\0\0WAVEfmt "), Some("audio/wav"));
        assert_eq!(sniff_media(b"ID3\x04"), Some("audio/mpeg"));
        assert_eq!(sniff_media(&[0xFF, 0xFB, 0x90]), Some("audio/mpeg"));
        // ADTS AAC는 데몬과 같이 audio/aac. UTF-16 BOM·잘못된 비트율은 음성이 아님.
        assert_eq!(sniff_media(&[0xFF, 0xF1, 0x50, 0x80]), Some("audio/aac"));
        assert_eq!(sniff_media(&[0xFF, 0xFE, b'h', 0]), None);
        assert_eq!(sniff_media(&[0xFF, 0xFB, 0xF0, 0]), None);
        assert_eq!(sniff_media(b"\x89PNG\r\n\x1a\n"), None);
        assert_eq!(sniff_media(b"%PDF-1.4"), None);
        assert_eq!(sniff_media(b""), None);
    }

    #[test]
    fn opens_media_files_only() {
        let dir = tempfile::tempdir().unwrap();
        let p = LocalPreviews::default();
        let ok = write(dir.path(), "a.webm", &[0x1A, 0x45, 0xDF, 0xA3, 0, 0, 0, 0]);
        let r = p.open(&ok).unwrap();
        assert_eq!((r.mime, r.size), ("video/webm", 8));
        assert_eq!(r.token.len(), 48);
        assert!(p.get(&r.token).is_some());
        assert!(p.get("없는-토큰").is_none());
        // 형식이 다르거나, 빈 파일·폴더·상대 경로·없는 파일은 거절.
        let pdf = write(dir.path(), "b.pdf", b"%PDF-1.4");
        assert!(p.open(&pdf).is_err());
        let empty = write(dir.path(), "c.mp4", b"");
        assert!(p.open(&empty).is_err());
        assert!(p.open(&dir.path().to_string_lossy()).is_err());
        assert!(p.open("a.webm").is_err());
        assert!(
            p.open(&dir.path().join("없음.mp4").to_string_lossy())
                .is_err()
        );
        p.close(&r.token);
        assert!(p.get(&r.token).is_none());
    }

    #[test]
    fn keeps_at_most_max_entries() {
        let dir = tempfile::tempdir().unwrap();
        let p = LocalPreviews::default();
        let f = write(dir.path(), "a.ogg", b"OggS\0\0\0\0");
        let first = p.open(&f).unwrap().token;
        for _ in 0..MAX_ENTRIES {
            p.open(&f).unwrap();
        }
        assert_eq!(p.len(), MAX_ENTRIES);
        assert!(p.get(&first).is_none(), "가장 오래된 등록을 닫음");
    }

    #[test]
    fn windows_are_bounded() {
        assert_eq!(window(None, 10), Some((0, 9)));
        assert_eq!(window(None, WINDOW * 3), Some((0, WINDOW - 1)));
        assert_eq!(
            window(Some(RangeSpec::From(WINDOW)), WINDOW * 3),
            Some((WINDOW, 2 * WINDOW - 1))
        );
        assert_eq!(window(Some(RangeSpec::FromTo(5, 7)), 10), Some((5, 7)));
        assert_eq!(window(Some(RangeSpec::Suffix(4)), 10), Some((6, 9)));
        assert_eq!(window(Some(RangeSpec::From(10)), 10), None);
        assert_eq!(window(None, 0), None);
    }

    #[test]
    fn reads_requested_window() {
        let dir = tempfile::tempdir().unwrap();
        let bytes: Vec<u8> = (0..100u8).collect();
        let f = write(dir.path(), "a.webm", &bytes);
        assert_eq!(read_window(Path::new(&f), 10, 19).unwrap(), bytes[10..20]);
        assert!(
            read_window(Path::new(&f), 90, 120).is_err(),
            "파일이 줄었으면 오류"
        );
    }
}
