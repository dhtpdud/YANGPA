//! 앱이 넘긴 동영상·음성의 썸네일·크기·길이(`AttachmentInput`의 미디어 힌트) 검사.
//!
//! 데몬은 영상 디코더가 없어 동영상 썸네일을 만들 수 없으므로 앱(웹뷰의 `<video>` + canvas)이 만든 것을 받되,
//! 그대로 믿지 않습니다.
//! - 썸네일: PNG·JPEG·WebP, 64KiB 이하, 긴 변 [`MAX_HINT_EDGE`]px 이하로 끝까지 풀리는 것만. 푼 뒤 데몬이 JPEG로
//!   다시 만들어 싣습니다(메타데이터가 따라가지 않고 받는 쪽 썸네일 형식이 하나로 같음).
//! - 가로·세로: 함께, 1~[`MAX_VIDEO_EDGE`]. 음성 파일이면 버립니다.
//! - 길이: 1밀리초~[`MAX_DURATION_MS`]. 데몬이 파일에서 읽을 수 있으면(MP4·MOV) 그 값을 씁니다.
//! - 파일 내용으로 가린 형식이 동영상·음성이 아니면(앱과 데몬의 형식 판별이 다를 수 있음) 힌트를 버리고 그냥
//!   보냅니다. 이미지면 데몬이 직접 썸네일을 만듭니다. 힌트는 있으면 좋은 정보라 보내기를 막지 않고, 잘못 만든
//!   값(깨진 base64, 큰 썸네일, 한쪽만 준 가로·세로, 범위 밖 값)만 거절합니다.

use std::io::Cursor;

use base64::Engine;
use tracing::debug;
use yangpa_rpc::RpcError;
use yangpa_rpc::api::AttachmentInput;
use yangpa_sync::state::limits::MAX_THUMBNAIL_BYTES;

use super::media::{DecodeLimits, make_thumbnail, sniff_magic};

/// 앱이 넘긴 썸네일의 긴 변 상한.
pub(super) const MAX_HINT_EDGE: u32 = 640;
/// 동영상 가로·세로 상한.
pub(super) const MAX_VIDEO_EDGE: u32 = 16_384;
/// 재생 길이 상한(7일).
pub(super) const MAX_DURATION_MS: u64 = 7 * 24 * 60 * 60 * 1000;
/// 앱 썸네일을 풀 때의 한도(긴 변 640px RGBA 몇 장 분량).
const HINT_DECODE: DecodeLimits = DecodeLimits {
    max_edge: MAX_HINT_EDGE,
    max_alloc: 16 * 1024 * 1024,
};
const HINT_MIMES: [&str; 3] = ["image/png", "image/jpeg", "image/webp"];

/// 형식만 본(아직 풀지 않은) 힌트.
#[derive(Debug, Default)]
pub(super) struct Hints {
    thumbnail: Option<(Vec<u8>, &'static str)>,
    size: Option<(u32, u32)>,
    duration_ms: Option<u64>,
}

/// 첨부에 실을 값.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Applied {
    pub thumbnail: Vec<u8>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub duration_ms: Option<u64>,
}

fn invalid(msg: &str) -> RpcError {
    RpcError::invalid_input(msg.to_owned())
}

/// 파일을 읽기 전에 할 수 있는 검사(base64·바이트 수·형식·범위).
pub(super) fn parse(inp: &AttachmentInput) -> Result<Hints, RpcError> {
    let thumbnail = match &inp.thumbnail_base64 {
        None => None,
        Some(s) => {
            // base64는 원본의 4/3배라 먼저 길이로 거릅니다(큰 문자열을 풀지 않게).
            if s.len() > MAX_THUMBNAIL_BYTES.div_ceil(3) * 4 {
                return Err(invalid("썸네일이 너무 큽니다(최대 64KiB)"));
            }
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(s)
                .map_err(|_| invalid("썸네일 base64가 올바르지 않습니다"))?;
            if bytes.is_empty() || bytes.len() > MAX_THUMBNAIL_BYTES {
                return Err(invalid("썸네일이 너무 큽니다(최대 64KiB)"));
            }
            let mime = sniff_magic(&bytes)
                .filter(|m| HINT_MIMES.contains(m))
                .ok_or_else(|| invalid("썸네일은 PNG·JPEG·WebP여야 합니다"))?;
            Some((bytes, mime))
        }
    };
    let size = match (inp.width, inp.height) {
        (None, None) => None,
        (Some(w), Some(h))
            if (1..=MAX_VIDEO_EDGE).contains(&w) && (1..=MAX_VIDEO_EDGE).contains(&h) =>
        {
            Some((w, h))
        }
        (Some(_), Some(_)) => {
            return Err(RpcError::invalid_input(format!(
                "가로·세로는 1~{MAX_VIDEO_EDGE}이어야 합니다"
            )));
        }
        _ => return Err(invalid("가로·세로는 함께 줘야 합니다")),
    };
    let duration_ms = match inp.duration_ms {
        None => None,
        Some(d) if (1..=MAX_DURATION_MS).contains(&d) => Some(d),
        Some(_) => return Err(invalid("재생 길이가 범위를 벗어났습니다(최대 7일)")),
    };
    Ok(Hints {
        thumbnail,
        size,
        duration_ms,
    })
}

/// 파일 형식(`mime`)을 안 뒤 적용합니다. 힌트가 없거나, 이미지거나, 동영상·음성이 아니면 `None`(데몬 값 그대로).
/// `measured_ms`는 데몬이 파일에서 읽은 길이(있으면 앱 값보다 우선).
pub(super) fn apply(
    h: Hints,
    mime: &str,
    measured_ms: Option<u64>,
    max_bytes: usize,
) -> Result<Option<Applied>, RpcError> {
    if h.thumbnail.is_none() && h.size.is_none() && h.duration_ms.is_none() {
        return Ok(None);
    }
    if mime.starts_with("image/") {
        return Ok(None);
    }
    let video = mime.starts_with("video/");
    if !video && !mime.starts_with("audio/") {
        // 앱은 재생할 수 있었지만 데몬은 모르는 형식(예: 판별 규칙 차이). 힌트 없이 보냅니다.
        debug!("동영상·음성으로 가리지 못한 첨부: 앱이 넘긴 미디어 정보를 버림");
        return Ok(None);
    }
    // 음성에는 화면 크기가 없습니다(앱이 동영상으로 본 파일). 크기만 버립니다.
    let size = if video { h.size } else { None };
    let thumbnail = match h.thumbnail {
        None => Vec::new(),
        Some((bytes, tmime)) => {
            // 긴 변 한도를 넘거나 깨져서 끝까지 풀리지 않으면 거절합니다.
            make_thumbnail(Cursor::new(bytes), tmime, max_bytes, HINT_DECODE)
                .ok_or_else(|| invalid("썸네일을 풀 수 없거나 너무 큽니다(긴 변 최대 640px)"))?
                .jpeg
        }
    };
    let (width, height) = size.map_or((None, None), |(w, h)| (Some(w), Some(h)));
    Ok(Some(Applied {
        thumbnail,
        width,
        height,
        duration_ms: measured_ms.or(h.duration_ms),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{DynamicImage, ImageFormat, RgbImage};

    fn png(w: u32, h: u32) -> Vec<u8> {
        let img = DynamicImage::ImageRgb8(RgbImage::from_fn(w, h, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, 90])
        }));
        let mut out = Cursor::new(Vec::new());
        img.write_to(&mut out, ImageFormat::Png).unwrap();
        out.into_inner()
    }

    fn input(thumb: Option<&[u8]>) -> AttachmentInput {
        AttachmentInput {
            path: "/x.mp4".into(),
            thumbnail_base64: thumb.map(|b| base64::engine::general_purpose::STANDARD.encode(b)),
            ..Default::default()
        }
    }

    #[test]
    fn rejects_bad_thumbnails_and_ranges() {
        let bad_b64 = AttachmentInput {
            thumbnail_base64: Some("%%%".into()),
            ..input(None)
        };
        assert!(parse(&bad_b64).is_err());
        // GIF·임의 바이트는 거절.
        assert!(parse(&input(Some(b"GIF89a\x01\x00\x01\x00"))).is_err());
        assert!(parse(&input(Some(b"not an image"))).is_err());
        let huge = vec![0u8; MAX_THUMBNAIL_BYTES + 1];
        assert!(parse(&input(Some(&huge))).is_err());
        let one_side = AttachmentInput {
            width: Some(10),
            ..input(None)
        };
        assert!(parse(&one_side).is_err());
        let zero = AttachmentInput {
            width: Some(0),
            height: Some(10),
            ..input(None)
        };
        assert!(parse(&zero).is_err());
        let long = AttachmentInput {
            duration_ms: Some(MAX_DURATION_MS + 1),
            ..input(None)
        };
        assert!(parse(&long).is_err());
        let zero_len = AttachmentInput {
            duration_ms: Some(0),
            ..input(None)
        };
        assert!(parse(&zero_len).is_err());
    }

    #[test]
    fn applies_by_file_type() {
        let thumb = png(64, 36);
        let full = AttachmentInput {
            width: Some(1920),
            height: Some(1080),
            duration_ms: Some(5_000),
            ..input(Some(&thumb))
        };
        let a = apply(parse(&full).unwrap(), "video/webm", None, 48 * 1024)
            .unwrap()
            .unwrap();
        assert_eq!(
            (a.width, a.height, a.duration_ms),
            (Some(1920), Some(1080), Some(5_000))
        );
        // 다시 만든 JPEG(원본 PNG 바이트가 그대로 실리지 않음).
        assert_eq!(sniff_magic(&a.thumbnail), Some("image/jpeg"));
        let decoded = image::load_from_memory(&a.thumbnail).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (64, 36));
        // 데몬이 읽은 길이가 우선.
        let a = apply(parse(&full).unwrap(), "video/mp4", Some(4_321), 48 * 1024)
            .unwrap()
            .unwrap();
        assert_eq!(a.duration_ms, Some(4_321));
        // 이미지·그 밖의 파일(데몬이 동영상·음성으로 가리지 못함)은 힌트를 버리고 그대로 보냅니다.
        assert_eq!(
            apply(parse(&full).unwrap(), "image/png", None, 48 * 1024).unwrap(),
            None
        );
        for other in ["application/pdf", "application/octet-stream"] {
            assert_eq!(
                apply(parse(&full).unwrap(), other, None, 48 * 1024).unwrap(),
                None,
                "{other}"
            );
        }
        // 크기를 준 음성은 크기만 버립니다(길이·썸네일은 그대로).
        let a = apply(parse(&full).unwrap(), "audio/ogg", None, 48 * 1024)
            .unwrap()
            .unwrap();
        assert_eq!(
            (a.width, a.height, a.duration_ms),
            (None, None, Some(5_000))
        );
        let audio = AttachmentInput {
            duration_ms: Some(90_000),
            ..input(None)
        };
        let a = apply(parse(&audio).unwrap(), "audio/ogg", None, 48 * 1024)
            .unwrap()
            .unwrap();
        assert_eq!(
            (a.width, a.duration_ms, a.thumbnail.len()),
            (None, Some(90_000), 0)
        );
        // 힌트가 없으면 아무것도 바꾸지 않음.
        assert_eq!(
            apply(
                parse(&input(None)).unwrap(),
                "video/mp4",
                Some(1),
                48 * 1024
            )
            .unwrap(),
            None
        );
    }

    #[test]
    fn rejects_oversized_or_broken_thumbnail_on_decode() {
        // 긴 변이 640px를 넘는 이미지(바이트 수는 한도 안).
        let wide = png(700, 4);
        assert!(wide.len() <= MAX_THUMBNAIL_BYTES);
        let h = parse(&input(Some(&wide))).unwrap();
        assert!(apply(h, "video/mp4", None, 48 * 1024).is_err());
        // 머리는 PNG지만 잘린 파일.
        let mut cut = png(64, 64);
        cut.truncate(cut.len() / 2);
        let h = parse(&input(Some(&cut))).unwrap();
        assert!(apply(h, "video/mp4", None, 48 * 1024).is_err());
    }
}
