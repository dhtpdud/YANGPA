//! 첨부 형식 판별·이미지 크기(머리 바이트만 봄), 보내는 쪽 썸네일(이미지를 풀어 작게 줄인 JPEG),
//! MP4·MOV 재생 길이(상자 머리만 따라감).
//!
//! 썸네일은 썸네일로 실을 수 있는 네 형식(png·jpeg·gif·webp)만 만듭니다. 영상은 디코더가 없어 썸네일 없이
//! 길이만 채우고, WebM·MKV 등 다른 컨테이너의 길이는 아직 읽지 않습니다(daemon.md 알려진 빈칸).

use std::io::{BufRead, Read, Seek, SeekFrom};
use std::path::Path;

use image::codecs::jpeg::JpegEncoder;
use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader, Limits, RgbImage};

/// 썸네일로 쓸 수 있는(받는 쪽이 바로 그릴) 이미지 형식.
pub(crate) const THUMBNAIL_MIMES: [&str; 4] =
    ["image/png", "image/jpeg", "image/gif", "image/webp"];

/// 머리 바이트(마법 수)로 형식을 정하고, 모르면 확장자로, 그래도 모르면 `application/octet-stream`.
pub(crate) fn sniff_mime(head: &[u8], path: &Path) -> String {
    if let Some(m) = sniff_magic(head) {
        return m.to_owned();
    }
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    let m = match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "heic" => "image/heic",
        "avif" => "image/avif",
        "svg" => "image/svg+xml",
        "mp4" | "m4v" => "video/mp4",
        "mov" => "video/quicktime",
        "webm" => "video/webm",
        "mkv" => "video/x-matroska",
        "avi" => "video/x-msvideo",
        "mp3" => "audio/mpeg",
        "aac" => "audio/aac",
        "m4a" => "audio/mp4",
        "ogg" | "opus" => "audio/ogg",
        "wav" => "audio/wav",
        "flac" => "audio/flac",
        "txt" | "log" => "text/plain",
        "md" => "text/markdown",
        "csv" => "text/csv",
        "json" => "application/json",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        _ => "application/octet-stream",
    };
    m.to_owned()
}

/// 마법 수로 아는 형식.
pub(crate) fn sniff_magic(head: &[u8]) -> Option<&'static str> {
    let starts = |p: &[u8]| head.starts_with(p);
    if starts(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if starts(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if starts(b"GIF87a") || starts(b"GIF89a") {
        Some("image/gif")
    } else if head.len() >= 12 && starts(b"RIFF") && &head[8..12] == b"WEBP" {
        Some("image/webp")
    } else if head.len() >= 12 && starts(b"RIFF") && &head[8..12] == b"WAVE" {
        Some("audio/wav")
    } else if starts(b"%PDF-") {
        Some("application/pdf")
    } else if head.len() >= 12 && &head[4..8] == b"ftyp" {
        Some(match &head[8..12] {
            b"qt  " => "video/quicktime",
            b"M4A " => "audio/mp4",
            b"heic" | b"heix" | b"mif1" => "image/heic",
            b"avif" => "image/avif",
            _ => "video/mp4",
        })
    } else if starts(&[0x1A, 0x45, 0xDF, 0xA3]) {
        Some("video/webm")
    } else if starts(b"OggS") {
        Some("audio/ogg")
    } else if starts(b"fLaC") {
        Some("audio/flac")
    } else if starts(b"ID3") {
        Some("audio/mpeg")
    } else if starts(b"PK\x03\x04") {
        Some("application/zip")
    } else {
        mpeg_audio(head)
    }
}

/// ID3 머리가 없는 MP3(MPEG 오디오 Layer II·III 프레임)와 ADTS AAC. 데스크톱 앱의 미리보기 판별
/// (`apps/desktop/src-tauri/src/preview.rs`의 `sniff_media`)과 같은 규칙이라, 앱이 재생해 길이를 읽은 파일을 데몬도
/// 음성으로 봅니다. 동기 비트만이 아니라 판·층·비트율·표본율까지 봐 UTF-16 BOM(`FF FE`) 같은 우연한 일치를 거릅니다.
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

fn be16(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from(u16::from_be_bytes(
        b.get(at..at + 2)?.try_into().ok()?,
    )))
}

fn le16(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from(u16::from_le_bytes(
        b.get(at..at + 2)?.try_into().ok()?,
    )))
}

fn be32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn le24(b: &[u8], at: usize) -> Option<u32> {
    let s = b.get(at..at + 3)?;
    Some(u32::from(s[0]) | u32::from(s[1]) << 8 | u32::from(s[2]) << 16)
}

/// 이미지의 (너비, 높이). png·gif·webp·jpeg만, 머리 바이트 안에 크기 정보가 있을 때.
pub(crate) fn image_size(head: &[u8]) -> Option<(u32, u32)> {
    let (w, h) = match sniff_magic(head)? {
        "image/png" => {
            if head.get(12..16)? != b"IHDR" {
                return None;
            }
            (be32(head, 16)?, be32(head, 20)?)
        }
        "image/gif" => (le16(head, 6)?, le16(head, 8)?),
        "image/webp" => match head.get(12..16)? {
            b"VP8 " => (le16(head, 26)? & 0x3FFF, le16(head, 28)? & 0x3FFF),
            b"VP8L" => {
                if *head.get(20)? != 0x2F {
                    return None;
                }
                let b = head.get(21..25)?;
                let w = 1 + (u32::from(b[0]) | (u32::from(b[1]) & 0x3F) << 8);
                let h = 1
                    + (u32::from(b[1]) >> 6
                        | u32::from(b[2]) << 2
                        | (u32::from(b[3]) & 0x0F) << 10);
                (w, h)
            }
            b"VP8X" => (le24(head, 24)? + 1, le24(head, 27)? + 1),
            _ => return None,
        },
        "image/jpeg" => jpeg_size(head)?,
        _ => return None,
    };
    (w > 0 && h > 0).then_some((w, h))
}

/// JPEG 표지를 따라가 SOF의 크기를 읽습니다.
fn jpeg_size(b: &[u8]) -> Option<(u32, u32)> {
    let mut i = 2;
    loop {
        while *b.get(i)? != 0xFF {
            i += 1;
        }
        while *b.get(i)? == 0xFF {
            i += 1;
        }
        let marker = *b.get(i)?;
        i += 1;
        match marker {
            // 길이 없는 표지.
            0xD0..=0xD9 | 0x01 => continue,
            0xC0..=0xCF if !matches!(marker, 0xC4 | 0xC8 | 0xCC) => {
                return Some((be16(b, i + 5)?, be16(b, i + 3)?));
            }
            _ => {
                let len = be16(b, i)? as usize;
                if len < 2 {
                    return None;
                }
                i += len;
            }
        }
    }
}

/// 썸네일 긴 변 후보(크기 한도를 넘으면 다음으로 줄임)와 JPEG 품질 후보.
const THUMB_EDGES: [u32; 3] = [320, 240, 160];
const THUMB_QUALITIES: [u8; 2] = [75, 55];

/// 이미지를 풀 때의 한도(아주 큰 이미지·압축 폭탄으로 메모리를 다 쓰지 않게).
#[derive(Clone, Copy, Debug)]
pub(crate) struct DecodeLimits {
    pub max_edge: u32,
    pub max_alloc: u64,
}

/// 내 파일(첨부).
pub(crate) const LOCAL_DECODE: DecodeLimits = DecodeLimits {
    max_edge: 16_384,
    max_alloc: 256 * 1024 * 1024,
};
/// 외부 사이트의 이미지(링크 미리보기).
pub(crate) const REMOTE_DECODE: DecodeLimits = DecodeLimits {
    max_edge: 8_192,
    max_alloc: 64 * 1024 * 1024,
};

/// 만든 썸네일과 원본의 (회전을 반영한) 크기.
pub(crate) struct Thumbnail {
    pub jpeg: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// 이미지를 풀어 긴 변 320px 이하 JPEG 썸네일을 만듭니다(투명한 곳은 흰 바탕). `max_bytes`를 넘으면 더
/// 작게·낮은 품질로 다시 만들고, 그래도 넘거나 풀 수 없으면 `None`.
pub(crate) fn make_thumbnail<R: BufRead + Seek>(
    reader: R,
    mime: &str,
    max_bytes: usize,
    limits: DecodeLimits,
) -> Option<Thumbnail> {
    let format = match mime {
        "image/png" => ImageFormat::Png,
        "image/jpeg" => ImageFormat::Jpeg,
        "image/gif" => ImageFormat::Gif,
        "image/webp" => ImageFormat::WebP,
        _ => return None,
    };
    let mut r = ImageReader::with_format(reader, format);
    let mut l = Limits::default();
    l.max_image_width = Some(limits.max_edge);
    l.max_image_height = Some(limits.max_edge);
    l.max_alloc = Some(limits.max_alloc);
    r.limits(l);
    let mut dec = r.into_decoder().ok()?;
    let orientation = dec.orientation().unwrap_or(Orientation::NoTransforms);
    let mut img = DynamicImage::from_decoder(dec).ok()?;
    img.apply_orientation(orientation);
    let (width, height) = (img.width(), img.height());
    for edge in THUMB_EDGES {
        // 이미 작은 이미지는 키우지 않습니다.
        let rgb = if width <= edge && height <= edge {
            on_white(&img)
        } else {
            on_white(&img.thumbnail(edge, edge))
        };
        for q in THUMB_QUALITIES {
            let mut jpeg = Vec::new();
            JpegEncoder::new_with_quality(&mut jpeg, q)
                .encode_image(&rgb)
                .ok()?;
            if jpeg.len() <= max_bytes {
                return Some(Thumbnail {
                    jpeg,
                    width,
                    height,
                });
            }
        }
    }
    None
}

/// 알파를 흰 바탕에 합칩니다(JPEG에는 투명이 없음).
fn on_white(img: &DynamicImage) -> RgbImage {
    let rgba = img.to_rgba8();
    RgbImage::from_fn(rgba.width(), rgba.height(), |x, y| {
        let [r, g, b, a] = rgba.get_pixel(x, y).0;
        let mix = |c: u8| ((u16::from(c) * u16::from(a) + 255 * (255 - u16::from(a))) / 255) as u8;
        image::Rgb([mix(r), mix(g), mix(b)])
    })
}

/// 길이를 읽을 수 있는 형식(ISO BMFF).
pub(crate) fn has_mp4_duration(mime: &str) -> bool {
    matches!(mime, "video/mp4" | "video/quicktime" | "audio/mp4")
}

/// 상자를 이만큼까지만 건너뜁니다(깨진 파일에서 오래 돌지 않게).
const MAX_BOXES: usize = 4_096;

fn read_array<const N: usize, R: Read>(r: &mut R) -> Option<[u8; N]> {
    let mut b = [0u8; N];
    r.read_exact(&mut b).ok()?;
    Some(b)
}

/// `[start, end)` 안에서 `kind` 상자를 찾아 (내용 시작, 끝)을 돌려줍니다. 상자 머리만 읽고 건너뜁니다.
fn find_box<R: Read + Seek>(r: &mut R, start: u64, end: u64, kind: &[u8; 4]) -> Option<(u64, u64)> {
    let mut pos = start;
    for _ in 0..MAX_BOXES {
        if pos.checked_add(8)? > end {
            return None;
        }
        r.seek(SeekFrom::Start(pos)).ok()?;
        let size = u64::from(u32::from_be_bytes(read_array(r)?));
        let ty: [u8; 4] = read_array(r)?;
        let (header, size) = match size {
            // 64비트 크기.
            1 => (16, u64::from_be_bytes(read_array(r)?)),
            // 파일(부모 상자) 끝까지.
            0 => (8, end - pos),
            n => (8, n),
        };
        if size < header {
            return None;
        }
        let box_end = pos.checked_add(size)?.min(end);
        if &ty == kind {
            return Some((pos + header, box_end));
        }
        pos = box_end;
    }
    None
}

/// MP4·MOV의 재생 길이(밀리초): 최상위 `moov` → `mvhd`의 시간 단위와 길이. 모르면 `None`.
pub(crate) fn mp4_duration_ms<R: Read + Seek>(r: &mut R, len: u64) -> Option<u64> {
    let (moov, moov_end) = find_box(r, 0, len, b"moov")?;
    let (mvhd, mvhd_end) = find_box(r, moov, moov_end, b"mvhd")?;
    r.seek(SeekFrom::Start(mvhd)).ok()?;
    let [version, ..] = read_array::<4, _>(r)?;
    // 판 표시(4) + 만든·고친 시각, 시간 단위, 길이(v0: 4·4·4·4, v1: 8·8·4·8).
    let need = if version == 1 { 32 } else { 20 };
    if mvhd.checked_add(need)? > mvhd_end {
        return None;
    }
    let (timescale, duration) = match version {
        0 => {
            let b: [u8; 16] = read_array(r)?;
            let ts = u32::from_be_bytes(b[8..12].try_into().ok()?);
            let d = u32::from_be_bytes(b[12..16].try_into().ok()?);
            (ts, (d != u32::MAX).then_some(u64::from(d))?)
        }
        1 => {
            let b: [u8; 28] = read_array(r)?;
            let ts = u32::from_be_bytes(b[16..20].try_into().ok()?);
            let d = u64::from_be_bytes(b[20..28].try_into().ok()?);
            (ts, (d != u64::MAX).then_some(d)?)
        }
        _ => return None,
    };
    if timescale == 0 {
        return None;
    }
    u64::try_from(u128::from(duration) * 1000 / u128::from(timescale)).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny_png(w: u32, h: u32) -> Vec<u8> {
        let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
        v.extend_from_slice(&13u32.to_be_bytes());
        v.extend_from_slice(b"IHDR");
        v.extend_from_slice(&w.to_be_bytes());
        v.extend_from_slice(&h.to_be_bytes());
        v.extend_from_slice(&[8, 6, 0, 0, 0]);
        v
    }

    #[test]
    fn sniffs_and_measures() {
        let png = tiny_png(640, 480);
        assert_eq!(sniff_mime(&png, Path::new("x.bin")), "image/png");
        assert_eq!(image_size(&png), Some((640, 480)));

        let mut gif = b"GIF89a".to_vec();
        gif.extend_from_slice(&[0x40, 0x01, 0xF0, 0x00]);
        assert_eq!(image_size(&gif), Some((320, 240)));

        // SOF0 앞에 APP0 하나.
        let mut jpg = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x04, 0x00, 0x00];
        jpg.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x11, 0x08, 0x01, 0x2C, 0x01, 0x90]);
        assert_eq!(sniff_mime(&jpg, Path::new("a")), "image/jpeg");
        assert_eq!(image_size(&jpg), Some((400, 300)));

        let mut webp = b"RIFF\0\0\0\0WEBPVP8X".to_vec();
        webp.extend_from_slice(&[0; 8]);
        webp.extend_from_slice(&[0x7F, 0x02, 0x00, 0xDF, 0x01, 0x00]);
        assert_eq!(image_size(&webp), Some((640, 480)));

        assert_eq!(sniff_mime(b"hello", Path::new("notes.TXT")), "text/plain");
        assert_eq!(
            sniff_mime(b"hello", Path::new("noext")),
            "application/octet-stream"
        );
        assert_eq!(image_size(b"\x89PNG\r\n\x1a\n"), None);
    }

    #[test]
    fn sniffs_raw_mpeg_audio_and_adts() {
        // ID3 없는 MP3(MPEG-1 Layer III, 128kbps, 44.1kHz)와 MPEG-2 Layer III.
        assert_eq!(sniff_magic(&[0xFF, 0xFB, 0x90, 0x64]), Some("audio/mpeg"));
        assert_eq!(sniff_magic(&[0xFF, 0xF3, 0x48, 0xC4]), Some("audio/mpeg"));
        // ADTS AAC(MPEG-4·MPEG-2, 표본율 색인 4).
        assert_eq!(sniff_magic(&[0xFF, 0xF1, 0x50, 0x80]), Some("audio/aac"));
        assert_eq!(sniff_magic(&[0xFF, 0xF9, 0x50, 0x80]), Some("audio/aac"));
        // 확장자만으로도(머리가 없는 조각 파일 등).
        assert_eq!(sniff_mime(b"????", Path::new("a.AAC")), "audio/aac");
        // UTF-16 LE BOM·잘못된 비트율·예약된 판·JPEG는 음성이 아님.
        for head in [
            &[0xFF, 0xFE, 0x48, 0x00][..],
            &[0xFF, 0xFB, 0xF0, 0x00],
            &[0xFF, 0xFB, 0x0C, 0x00],
            &[0xFF, 0xEB, 0x90, 0x00],
            &[0xFF, 0xF1, 0x3C, 0x00],
            &[0xFF],
        ] {
            assert_eq!(sniff_magic(head), None, "{head:02X?}");
        }
        assert_eq!(
            sniff_mime(&[0xFF, 0xFE, b'h', 0], Path::new("notes.txt")),
            "text/plain"
        );
    }

    fn encode_png(img: &DynamicImage) -> Vec<u8> {
        let mut out = std::io::Cursor::new(Vec::new());
        img.write_to(&mut out, ImageFormat::Png).unwrap();
        out.into_inner()
    }

    #[test]
    fn thumbnails_are_small_jpegs() {
        // 큰 그림(1200x800, 반투명 포함) → 긴 변 320 이하 JPEG, 원본 크기는 그대로 알려 줌.
        let img = DynamicImage::ImageRgba8(image::RgbaImage::from_fn(1200, 800, |x, y| {
            image::Rgba([
                (x % 256) as u8,
                (y % 256) as u8,
                ((x * y) % 256) as u8,
                (x % 200) as u8 + 55,
            ])
        }));
        let png = encode_png(&img);
        assert!(png.len() > 64 * 1024, "원본이 썸네일 한도보다 커야 함");
        let t = make_thumbnail(
            std::io::Cursor::new(&png),
            "image/png",
            64 * 1024,
            LOCAL_DECODE,
        )
        .expect("썸네일");
        assert_eq!((t.width, t.height), (1200, 800));
        assert!(t.jpeg.len() <= 64 * 1024);
        assert_eq!(sniff_magic(&t.jpeg), Some("image/jpeg"));
        assert_eq!(image_size(&t.jpeg), Some((320, 213)));
        // 작은 그림은 키우지 않습니다.
        let small = encode_png(&DynamicImage::ImageRgba8(image::RgbaImage::new(100, 50)));
        let t = make_thumbnail(
            std::io::Cursor::new(&small),
            "image/png",
            64 * 1024,
            LOCAL_DECODE,
        )
        .expect("작은 썸네일");
        assert_eq!(image_size(&t.jpeg), Some((100, 50)));
        // 한도가 아주 작으면 만들지 않습니다. 한도를 넘는 크기·이미지가 아닌 것도.
        assert!(
            make_thumbnail(std::io::Cursor::new(&png), "image/png", 100, LOCAL_DECODE).is_none()
        );
        let tight = DecodeLimits {
            max_edge: 1000,
            max_alloc: 1 << 30,
        };
        assert!(
            make_thumbnail(std::io::Cursor::new(&png), "image/png", 64 * 1024, tight).is_none()
        );
        assert!(
            make_thumbnail(
                std::io::Cursor::new(b"not an image"),
                "image/png",
                64 * 1024,
                LOCAL_DECODE
            )
            .is_none()
        );
        assert!(
            make_thumbnail(
                std::io::Cursor::new(&png),
                "image/svg+xml",
                64 * 1024,
                LOCAL_DECODE
            )
            .is_none()
        );
    }

    fn mp4_box(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut v = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        v.extend_from_slice(kind);
        v.extend_from_slice(body);
        v
    }

    #[test]
    fn reads_mp4_duration() {
        // mvhd v0: 시간 단위 600, 길이 7500 → 12.5초.
        let mut mvhd0 = vec![0, 0, 0, 0];
        mvhd0.extend_from_slice(&[0; 8]);
        mvhd0.extend_from_slice(&600u32.to_be_bytes());
        mvhd0.extend_from_slice(&7_500u32.to_be_bytes());
        mvhd0.extend_from_slice(&[0; 80]);
        let mut file = mp4_box(b"ftyp", b"isom\0\0\0\0isommp41");
        file.extend(mp4_box(b"mdat", &[7; 1000]));
        let mut moov = mp4_box(b"trak", &[0; 16]);
        moov.splice(0..0, mp4_box(b"mvhd", &mvhd0));
        file.extend(mp4_box(b"moov", &moov));
        let len = file.len() as u64;
        assert_eq!(
            mp4_duration_ms(&mut std::io::Cursor::new(&file), len),
            Some(12_500)
        );
        assert_eq!(sniff_mime(&file, Path::new("a")), "video/mp4");
        assert!(has_mp4_duration("video/mp4") && !has_mp4_duration("video/webm"));

        // mvhd v1(64비트 길이)와 64비트 크기 상자.
        let mut mvhd1 = vec![1, 0, 0, 0];
        mvhd1.extend_from_slice(&[0; 16]);
        mvhd1.extend_from_slice(&1_000u32.to_be_bytes());
        mvhd1.extend_from_slice(&90_061_000u64.to_be_bytes());
        mvhd1.extend_from_slice(&[0; 80]);
        let inner = mp4_box(b"mvhd", &mvhd1);
        let mut big = 1u32.to_be_bytes().to_vec();
        big.extend_from_slice(b"moov");
        big.extend_from_slice(&((inner.len() + 16) as u64).to_be_bytes());
        big.extend(inner);
        let len = big.len() as u64;
        assert_eq!(
            mp4_duration_ms(&mut std::io::Cursor::new(&big), len),
            Some(90_061_000)
        );

        // moov가 없거나 잘린 파일, 크기가 이상한 상자.
        let only = mp4_box(b"ftyp", b"isom");
        assert_eq!(
            mp4_duration_ms(&mut std::io::Cursor::new(&only), only.len() as u64),
            None
        );
        // moov 머리와 mvhd 크기 칸만 남은 파일.
        let cut = &file[..file.len() - moov.len() + 4];
        assert_eq!(
            mp4_duration_ms(&mut std::io::Cursor::new(cut), cut.len() as u64),
            None
        );
        let mut bad = 4u32.to_be_bytes().to_vec();
        bad.extend_from_slice(b"moov");
        assert_eq!(mp4_duration_ms(&mut std::io::Cursor::new(&bad), 8), None);
    }
}
