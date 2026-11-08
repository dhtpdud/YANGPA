//! 화면 고르기 창의 미리보기와 창 아이콘(윈도 GDI). 웹뷰 기본 선택 창처럼 보이게, 목록을 읽을 때마다 작게 찍어 data URL로
//! 넘깁니다. 메모리에서만 만들고 디스크·로그에 남기지 않습니다.

use super::sources::{Source, SourceKind};
use base64::Engine;
use image::{DynamicImage, ImageFormat, RgbaImage, imageops::FilterType};
use std::io::Cursor;
use std::ptr::null_mut;
use windows_sys::Win32::Foundation::{HWND, RECT};
use windows_sys::Win32::Graphics::Dwm::{DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute};
use windows_sys::Win32::Graphics::Gdi::{
    BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleBitmap, CreateCompatibleDC,
    DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDIBits, GetMonitorInfoW, GetObjectW,
    HALFTONE, HBITMAP, HDC, HMONITOR, MONITORINFO, ReleaseDC, SRCCOPY, SelectObject,
    SetStretchBltMode, StretchBlt,
};
use windows_sys::Win32::Storage::Xps::PrintWindow;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GCLP_HICON, GCLP_HICONSM, GetClassLongPtrW, GetIconInfo, GetWindowRect, HICON, ICON_BIG,
    ICON_SMALL2, ICONINFO, IsHungAppWindow, PW_RENDERFULLCONTENT, SMTO_ABORTIFHUNG,
    SendMessageTimeoutW, WM_GETICON,
};

/// 미리보기 최대 크기: 선택 창 칸의 두 배쯤(고해상도 화면에서도 흐리지 않게).
const THUMB_W: i32 = 384;
const THUMB_H: i32 = 216;
const ICON: u32 = 32;

/// 목록의 화면에 미리보기(JPEG)와, 창이면 아이콘(PNG)을 채웁니다. 못 찍으면 비워 둡니다.
pub fn fill(s: &mut Source) {
    let Ok(handle) = s.id.parse::<usize>() else {
        return;
    };
    match s.kind {
        SourceKind::Monitor => s.thumbnail = monitor_thumb(handle as HMONITOR).and_then(jpeg),
        SourceKind::Window => {
            let w = handle as HWND;
            // 응답 없는 창에 그려 달라고 하면 목록 읽기가 같이 멈춥니다.
            // SAFETY: 방금 목록에서 찾은 창 핸들을 읽기만 합니다.
            if unsafe { IsHungAppWindow(w) } != 0 {
                return;
            }
            s.thumbnail = window_thumb(w).and_then(jpeg);
            s.icon = window_icon(w).and_then(png);
        }
    }
}

fn monitor_thumb(m: HMONITOR) -> Option<RgbaImage> {
    // SAFETY: 크기 칸을 채운 MONITORINFO를 넘기고, 화면 DC는 쓴 뒤 돌려줍니다.
    unsafe {
        let mut info: MONITORINFO = std::mem::zeroed();
        info.cbSize = size_of::<MONITORINFO>() as u32;
        if GetMonitorInfoW(m, &mut info) == 0 {
            return None;
        }
        let r = info.rcMonitor;
        let screen = GetDC(null_mut());
        let img = shrink(screen, r.left, r.top, r.right - r.left, r.bottom - r.top);
        ReleaseDC(null_mut(), screen);
        img
    }
}

fn window_thumb(w: HWND) -> Option<RgbaImage> {
    // SAFETY: 만든 DC·비트맵은 모두 이 안에서 지우고, 창에는 그려 달라고만 합니다.
    unsafe {
        let mut wr: RECT = std::mem::zeroed();
        if GetWindowRect(w, &mut wr) == 0 {
            return None;
        }
        // 보이지 않는 크기 조절 테두리를 빼고 화면에 보이는 틀만 씁니다.
        let mut fr = wr;
        if DwmGetWindowAttribute(
            w,
            DWMWA_EXTENDED_FRAME_BOUNDS as u32,
            &mut fr as *mut RECT as *mut _,
            size_of::<RECT>() as u32,
        ) != 0
        {
            fr = wr;
        }
        let (ww, wh) = (wr.right - wr.left, wr.bottom - wr.top);
        if ww <= 0 || wh <= 0 {
            return None;
        }
        let screen = GetDC(null_mut());
        let dc = CreateCompatibleDC(screen);
        let bmp = CreateCompatibleBitmap(screen, ww, wh);
        let old = SelectObject(dc, bmp);
        // PW_RENDERFULLCONTENT: 크로미움·DirectX로 그리는 창도 검게 나오지 않게.
        let img = if PrintWindow(w, dc, PW_RENDERFULLCONTENT) != 0 {
            shrink(
                dc,
                fr.left - wr.left,
                fr.top - wr.top,
                fr.right - fr.left,
                fr.bottom - fr.top,
            )
        } else {
            None
        };
        SelectObject(dc, old);
        DeleteObject(bmp);
        DeleteDC(dc);
        ReleaseDC(null_mut(), screen);
        img
    }
}

/// `src`의 (x, y, w, h)를 미리보기 크기 안으로 줄여 읽습니다.
///
/// # Safety
/// `src`는 살아 있는 DC여야 합니다.
unsafe fn shrink(src: HDC, x: i32, y: i32, w: i32, h: i32) -> Option<RgbaImage> {
    if w <= 0 || h <= 0 {
        return None;
    }
    let s = (THUMB_W as f32 / w as f32)
        .min(THUMB_H as f32 / h as f32)
        .min(1.0);
    let (tw, th) = (
        ((w as f32 * s) as i32).max(1),
        ((h as f32 * s) as i32).max(1),
    );
    // SAFETY: 만든 DC·비트맵은 이 안에서 지웁니다. GetDIBits 전에 비트맵을 DC에서 뺍니다.
    unsafe {
        let dc = CreateCompatibleDC(src);
        let bmp = CreateCompatibleBitmap(src, tw, th);
        let old = SelectObject(dc, bmp);
        SetStretchBltMode(dc, HALFTONE);
        let ok = StretchBlt(dc, 0, 0, tw, th, src, x, y, w, h, SRCCOPY) != 0;
        SelectObject(dc, old);
        let img = if ok { read(dc, bmp, tw, th) } else { None };
        DeleteObject(bmp);
        DeleteDC(dc);
        img
    }
}

/// DC에 끼우지 않은 비트맵을 위에서 아래로 BGRA로 읽어 RGBA로 바꿉니다.
///
/// # Safety
/// `dc`·`bmp`는 살아 있어야 하고, `bmp`는 어떤 DC에도 끼워져 있지 않아야 합니다.
unsafe fn read(dc: HDC, bmp: HBITMAP, w: i32, h: i32) -> Option<RgbaImage> {
    if w <= 0 || h <= 0 {
        return None;
    }
    // SAFETY: 헤더를 채운 BITMAPINFO와 w*h*4바이트 버퍼를 넘깁니다.
    unsafe {
        let mut info: BITMAPINFO = std::mem::zeroed();
        info.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
        info.bmiHeader.biWidth = w;
        info.bmiHeader.biHeight = -h;
        info.bmiHeader.biPlanes = 1;
        info.bmiHeader.biBitCount = 32;
        info.bmiHeader.biCompression = BI_RGB;
        let mut px = vec![0u8; w as usize * h as usize * 4];
        if GetDIBits(
            dc,
            bmp,
            0,
            h as u32,
            px.as_mut_ptr().cast(),
            &mut info,
            DIB_RGB_COLORS,
        ) != h
        {
            return None;
        }
        for p in px.as_chunks_mut::<4>().0 {
            p.swap(0, 2);
        }
        RgbaImage::from_raw(w as u32, h as u32, px)
    }
}

fn window_icon(w: HWND) -> Option<RgbaImage> {
    let mut icon = 0usize;
    // SAFETY: 창 핸들로 아이콘 핸들만 묻습니다. 늦게 답하는 창은 기다리지 않습니다.
    unsafe {
        for size in [ICON_BIG, ICON_SMALL2] {
            SendMessageTimeoutW(
                w,
                WM_GETICON,
                size as usize,
                0,
                SMTO_ABORTIFHUNG,
                50,
                &mut icon,
            );
            if icon != 0 {
                break;
            }
        }
        if icon == 0 {
            icon = GetClassLongPtrW(w, GCLP_HICON);
        }
        if icon == 0 {
            icon = GetClassLongPtrW(w, GCLP_HICONSM);
        }
        if icon == 0 {
            return None;
        }
        icon_pixels(icon as HICON)
    }
}

/// 아이콘을 RGBA로(창의 아이콘이라 지우지 않고, GetIconInfo가 만든 비트맵만 지웁니다).
///
/// # Safety
/// `icon`은 살아 있는 아이콘 핸들이어야 합니다.
unsafe fn icon_pixels(icon: HICON) -> Option<RgbaImage> {
    // SAFETY: GetIconInfo가 만든 비트맵 둘과 화면 DC는 이 안에서 돌려줍니다.
    unsafe {
        let mut ii: ICONINFO = std::mem::zeroed();
        if GetIconInfo(icon, &mut ii) == 0 {
            return None;
        }
        let screen = GetDC(null_mut());
        let img = (|| {
            // 흑백 아이콘(색 비트맵 없음)은 건너뜁니다.
            if ii.hbmColor.is_null() {
                return None;
            }
            let mut bm: BITMAP = std::mem::zeroed();
            if GetObjectW(
                ii.hbmColor,
                size_of::<BITMAP>() as i32,
                &mut bm as *mut BITMAP as *mut _,
            ) == 0
            {
                return None;
            }
            let (w, h) = (bm.bmWidth, bm.bmHeight);
            let mut img = read(screen, ii.hbmColor, w, h)?;
            // 알파가 없는 옛 아이콘은 마스크(흰색 = 투명)로 알파를 만듭니다.
            if img.pixels().all(|p| p[3] == 0) {
                let mask = read(screen, ii.hbmMask, w, h)?;
                for (p, m) in img.pixels_mut().zip(mask.pixels()) {
                    p[3] = if m[0] == 0 { 255 } else { 0 };
                }
            }
            Some(image::imageops::resize(
                &img,
                ICON,
                ICON,
                FilterType::Triangle,
            ))
        })();
        if !ii.hbmColor.is_null() {
            DeleteObject(ii.hbmColor);
        }
        if !ii.hbmMask.is_null() {
            DeleteObject(ii.hbmMask);
        }
        ReleaseDC(null_mut(), screen);
        img
    }
}

fn jpeg(img: RgbaImage) -> Option<String> {
    data_url(
        DynamicImage::ImageRgb8(DynamicImage::ImageRgba8(img).to_rgb8()),
        ImageFormat::Jpeg,
    )
}

fn png(img: RgbaImage) -> Option<String> {
    data_url(DynamicImage::ImageRgba8(img), ImageFormat::Png)
}

fn data_url(img: DynamicImage, format: ImageFormat) -> Option<String> {
    let mut out = Cursor::new(Vec::new());
    img.write_to(&mut out, format).ok()?;
    let b64 = base64::engine::general_purpose::STANDARD.encode(out.get_ref());
    Some(format!("data:{};base64,{b64}", format.to_mime_type()))
}
