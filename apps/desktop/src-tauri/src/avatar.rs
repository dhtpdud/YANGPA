//! OS 알림에 붙이는 보낸 사람 프사.
//!
//! 프로필 이미지가 있으면 데몬의 `blob.read`로 읽어 정사각형으로 잘라 줄이고, 없거나 읽지 못하면 앱의 기본 프사와 같게
//! (ID에서 고른 색 바탕에 이름 머리글자) 그립니다. 윈도 알림은 파일 경로로만 그림을 받으므로 앱 캐시 폴더의
//! `notify-avatars`에 PNG로 둡니다(파일 이름은 사용자·이미지·이름의 해시). 휘발성 채널 알림에는 보낸 사람이 없어 쓰지 않습니다.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use ab_glyph::{Font, FontVec, PxScale, ScaleFont, point};
use image::{Rgba, RgbaImage, imageops};
use yangpa_core::UserId;
use yangpa_core::ids::{BlobId, EventId, SpaceId};
use yangpa_rpc::RpcClient;
use yangpa_rpc::api::{BLOB_READ_MAX_LEN, BlobReadParams};

/// 그림 한 변(윈도는 알림 크기·배율에 맞춰 줄여 그림).
pub const SIZE: u32 = 128;
/// 알림용으로 읽는 프로필 이미지 최대 크기.
const MAX_READ: u64 = 8 * 1024 * 1024;
/// 캐시 폴더에 두는 그림 수 상한(넘으면 비우고 다시 그림).
const MAX_FILES: usize = 512;
/// 캐시 폴더 이름.
pub const DIR: &str = "notify-avatars";

/// 알림 프사를 그릴 재료.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Spec {
    /// 프로필 이미지를 읽을 공간(알림이 가리키는 공간).
    pub space: SpaceId,
    pub user: UserId,
    pub name: String,
    pub avatar: Option<BlobId>,
}

/// 프사 PNG 경로. 이미 그려 둔 것이 있으면 그대로 씁니다. 그릴 수 없으면 `None`(알림은 앱 아이콘으로).
pub async fn prepare(client: Option<&RpcClient>, dir: &Path, spec: &Spec) -> Option<PathBuf> {
    let path = dir.join(file_name(spec));
    if path.is_file() {
        return Some(path);
    }
    let img = match (client, spec.avatar) {
        (Some(c), Some(blob)) => read_image(c, &spec.space, blob)
            .await
            .map(|i| square(&i, SIZE)),
        _ => None,
    }
    .unwrap_or_else(|| default_avatar(&spec.user.to_string(), &spec.name, SIZE));
    std::fs::create_dir_all(dir).ok()?;
    if std::fs::read_dir(dir).is_ok_and(|d| d.count() >= MAX_FILES) {
        clear(dir);
        std::fs::create_dir_all(dir).ok()?;
    }
    img.save_with_format(&path, image::ImageFormat::Png).ok()?;
    Some(path)
}

/// 캐시 폴더를 비웁니다(앱을 켤 때: 바뀐 프사가 쌓이지 않게).
pub fn clear(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
}

fn file_name(spec: &Spec) -> String {
    let mut h = DefaultHasher::new();
    (
        spec.user.to_string(),
        spec.avatar.map(|b| b.to_string()),
        &spec.name,
    )
        .hash(&mut h);
    format!("{:016x}.png", h.finish())
}

/// 프로필 이미지를 읽어 그림으로 풉니다. 프로필 이미지는 키가 서명된 프로필에 있어 키 출처 메시지가 필요 없습니다.
async fn read_image(
    client: &RpcClient,
    space: &SpaceId,
    blob: BlobId,
) -> Option<image::DynamicImage> {
    let mut out: Vec<u8> = Vec::new();
    loop {
        let offset = out.len() as u64;
        let chunk = client
            .blob_read(BlobReadParams {
                space: *space,
                blob,
                key_from: EventId([0; 32]),
                index: None,
                offset: Some(offset),
                len: Some(BLOB_READ_MAX_LEN),
                binary: None,
            })
            .await;
        let chunk = match chunk {
            Ok(c) => c,
            // 끝을 넘는 offset(크기가 창의 배수)은 잘못된 입력으로 옵니다.
            Err(_) if offset > 0 => break,
            Err(_) => return None,
        };
        if offset + chunk.len() as u64 > MAX_READ {
            return None;
        }
        out.extend_from_slice(&chunk);
        if (chunk.len() as u64) < BLOB_READ_MAX_LEN {
            break;
        }
    }
    image::load_from_memory(&out).ok()
}

/// 가운데를 정사각형으로 잘라 `size`로 줄입니다(앱의 `object-fit: cover`와 같음).
pub fn square(img: &image::DynamicImage, size: u32) -> RgbaImage {
    let (w, h) = (img.width(), img.height());
    let side = w.min(h).max(1);
    let cropped = img.crop_imm((w - side.min(w)) / 2, (h - side.min(h)) / 2, side, side);
    imageops::resize(
        &cropped.to_rgba8(),
        size,
        size,
        imageops::FilterType::Triangle,
    )
}

/// 앱의 기본 프사(`components/common.tsx`의 `Avatar`)와 같은 그림: ID 해시로 고른 색 바탕에 흰 머리글자.
pub fn default_avatar(id: &str, name: &str, size: u32) -> RgbaImage {
    let [r, g, b] = color_from_id(id);
    let mut img = RgbaImage::from_pixel(size, size, Rgba([r, g, b, 255]));
    if let Some(font) = font() {
        draw_text(&mut img, font, &initials(name), size as f32 * 0.38);
    }
    img
}

/// `lib/misc.ts`의 `colorFromId`: UTF-16 코드 단위의 FNV-1a 해시로 고른 색상(채도 55%, 밝기 48%).
pub fn color_from_id(id: &str) -> [u8; 3] {
    let mut h: u32 = 2_166_136_261;
    for u in id.encode_utf16() {
        h ^= u32::from(u);
        h = h.wrapping_mul(16_777_619);
    }
    hsl_to_rgb((h % 360) as f32, 0.55, 0.48)
}

fn hsl_to_rgb(h: f32, s: f32, l: f32) -> [u8; 3] {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let hp = h / 60.0;
    let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
    let (r, g, b) = match hp as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    let to = |v: f32| ((v + m) * 255.0).round().clamp(0.0, 255.0) as u8;
    [to(r), to(g), to(b)]
}

/// `lib/misc.ts`의 `initials`: 두 낱말 이상이면 앞 두 낱말의 첫 글자, 아니면 앞 두 글자(대문자로). 비었으면 `?`.
pub fn initials(name: &str) -> String {
    let t = name.trim();
    if t.is_empty() {
        return "?".into();
    }
    let words: Vec<&str> = t.split_whitespace().collect();
    let s: String = if words.len() >= 2 {
        words[..2].iter().filter_map(|w| w.chars().next()).collect()
    } else {
        t.chars().take(2).collect()
    };
    s.to_uppercase()
}

/// 머리글자를 그릴 시스템 글꼴(한 번만 읽음). 한글이 있는 글꼴을 먼저 찾습니다. 없으면 바탕색만 그립니다.
fn font() -> Option<&'static FontVec> {
    static FONT: OnceLock<Option<FontVec>> = OnceLock::new();
    FONT.get_or_init(|| {
        font_candidates()
            .into_iter()
            .filter_map(|p| std::fs::read(p).ok())
            .find_map(|b| FontVec::try_from_vec(b).ok())
    })
    .as_ref()
}

fn font_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if cfg!(windows) {
        let win =
            std::env::var_os("WINDIR").map_or_else(|| PathBuf::from("C:\\Windows"), PathBuf::from);
        for f in [
            "malgunbd.ttf",
            "malgun.ttf",
            "segoeuib.ttf",
            "segoeui.ttf",
            "arial.ttf",
        ] {
            out.push(win.join("Fonts").join(f));
        }
    } else {
        for f in [
            "/usr/share/fonts/truetype/nanum/NanumGothicBold.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
            "/usr/share/fonts/TTF/DejaVuSans-Bold.ttf",
            "/usr/share/fonts/dejavu-sans-fonts/DejaVuSans-Bold.ttf",
            "/System/Library/Fonts/Supplemental/Arial Bold.ttf",
        ] {
            out.push(PathBuf::from(f));
        }
    }
    out
}

/// 글자를 가운데에 흰색으로 그립니다. 글꼴에 없는 글자는 건너뜁니다.
fn draw_text(img: &mut RgbaImage, font: &FontVec, text: &str, px: f32) {
    let scaled = font.as_scaled(PxScale::from(px));
    let glyphs: Vec<_> = text
        .chars()
        .map(|c| font.glyph_id(c))
        .filter(|g| g.0 != 0)
        .collect();
    if glyphs.is_empty() {
        return;
    }
    let width: f32 = glyphs.iter().map(|g| scaled.h_advance(*g)).sum();
    let (w, h) = (img.width() as f32, img.height() as f32);
    let mut x = (w - width) / 2.0;
    // 글자 높이(오름 − 내림)를 세로 가운데에 둡니다.
    let baseline = (h - (scaled.ascent() - scaled.descent())) / 2.0 + scaled.ascent();
    for g in glyphs {
        let glyph = g.with_scale_and_position(PxScale::from(px), point(x, baseline));
        x += scaled.h_advance(g);
        let Some(outlined) = font.outline_glyph(glyph) else {
            continue;
        };
        let b = outlined.px_bounds();
        outlined.draw(|gx, gy, cov| {
            let px_x = b.min.x as i64 + i64::from(gx);
            let px_y = b.min.y as i64 + i64::from(gy);
            if px_x < 0 || px_y < 0 || px_x >= w as i64 || px_y >= h as i64 {
                return;
            }
            let p = img.get_pixel_mut(px_x as u32, px_y as u32);
            let a = cov.clamp(0.0, 1.0);
            for i in 0..3 {
                p.0[i] = (f32::from(p.0[i]) * (1.0 - a) + 255.0 * a).round() as u8;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initials_match_the_app() {
        assert_eq!(initials("  "), "?");
        assert_eq!(initials("seyoung"), "SE");
        assert_eq!(initials("오 세영"), "오세");
        assert_eq!(initials("양파"), "양파");
        assert_eq!(initials("a b c"), "AB");
    }

    #[test]
    fn color_matches_the_app_hash() {
        // lib/misc.ts의 colorFromId('abc'): FNV-1a 0x1a47e90b → 색상 0x1a47e90b % 360 = 19.
        let h = {
            let mut h: u32 = 2_166_136_261;
            for u in "abc".encode_utf16() {
                h ^= u32::from(u);
                h = h.wrapping_mul(16_777_619);
            }
            h
        };
        assert_eq!(h, 0x1a47_e90b);
        assert_eq!(
            color_from_id("abc"),
            hsl_to_rgb((h % 360) as f32, 0.55, 0.48)
        );
        // hsl(0 55% 48%)
        assert_eq!(hsl_to_rgb(0.0, 0.55, 0.48), [190, 55, 55]);
    }

    #[test]
    fn default_avatar_has_the_background_color_and_size() {
        let img = default_avatar("abc", "양파", 64);
        assert_eq!(img.dimensions(), (64, 64));
        let [r, g, b] = color_from_id("abc");
        assert_eq!(img.get_pixel(0, 0).0, [r, g, b, 255]);
    }

    #[test]
    fn square_crops_the_middle() {
        let mut wide = RgbaImage::from_pixel(30, 10, Rgba([0, 0, 0, 255]));
        for y in 0..10 {
            for x in 10..20 {
                wide.put_pixel(x, y, Rgba([255, 255, 255, 255]));
            }
        }
        let out = square(&image::DynamicImage::ImageRgba8(wide), 4);
        assert_eq!(out.dimensions(), (4, 4));
        assert!(out.pixels().all(|p| p.0[0] > 200), "가운데 흰 부분만 남음");
    }

    #[test]
    fn file_name_changes_with_avatar_and_name() {
        let base = Spec {
            space: SpaceId([1; 16]),
            user: UserId([2; 32]),
            name: "a".into(),
            avatar: None,
        };
        let renamed = Spec {
            name: "b".into(),
            ..base.clone()
        };
        let with_img = Spec {
            avatar: Some(BlobId([3; 32])),
            ..base.clone()
        };
        assert_ne!(file_name(&base), file_name(&renamed));
        assert_ne!(file_name(&base), file_name(&with_img));
        // 공간이 달라도 같은 그림.
        let other_space = Spec {
            space: SpaceId([9; 16]),
            ..base.clone()
        };
        assert_eq!(file_name(&base), file_name(&other_space));
    }
}
