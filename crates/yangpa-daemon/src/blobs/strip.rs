//! 보내는 첨부의 메타데이터 지우기(`AttachmentInput::keep_metadata`가 거짓일 때, 기본). 프로필 이미지·크루 아이콘·
//! 이모지는 늘 지웁니다([`clean_image`]).
//!
//! 다시 인코딩하지 않고 파일 구조만 따라가 지울 부분을 뺀 "이어 붙인 읽기"([`Spliced`])를 만듭니다. 원본 파일은
//! 건드리지 않고, 임시 파일도 쓰지 않으며(휘발성 채널도 같은 길), 수 GB 동영상도 RAM에 올리지 않습니다. 같은
//! 파일이면 늘 같은 결과라 `blob.relink`가 원본 파일로 다시 이을 때도 같은 블롭이 나옵니다.
//!
//! - JPEG: JFIF(APP0)·ICC(APP2)·Adobe(APP14) 밖의 APPn(EXIF·XMP·IPTC·MPF)과 주석을 빼고 EOI 뒤(모션 포토 등)를
//!   자릅니다. 회전값은 화면이 돌아가지 않게 회전만 담은 작은 EXIF로 남깁니다.
//! - PNG: 글(tEXt·zTXt·iTXt)·eXIf·tIME 상자를 빼고 IEND 뒤를 자릅니다.
//! - WebP: EXIF·XMP 상자를 빼고 VP8X 표시를 맞춥니다(회전값은 JPEG처럼 남김).
//! - GIF: 주석과 반복 재생(NETSCAPE2.0·ANIMEXTS1.0) 밖의 앱 확장(XMP 등)을 빼고 끝 표시 뒤를 자릅니다.
//! - MP4·MOV: `udta`·`meta`(촬영 위치·기기)와 XMP `uuid` 상자를 같은 크기의 `free`로 바꿔 0으로 채우고
//!   `mvhd`·`tkhd`·`mdhd`의 만든·고친 시각을 0으로 둡니다. 길이가 그대로라 `stco` 위치가 어긋나지 않습니다.
//! - 다른 형식(HEIC·AVIF·PDF·오피스·MP3 태그 등)은 그대로 보냅니다(decisions.md).

use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};

use yangpa_rpc::RpcError;
use zeroize::Zeroizing;

use super::media::sniff_magic;
use crate::paste::{Shared, Source};

/// 상자·블록을 이만큼까지만 따라갑니다(깨진 파일에서 오래 돌지 않게).
const MAX_STEPS: usize = 1 << 20;

/// 결과의 한 조각.
enum Part {
    /// 원본의 `[at, at+len)`.
    Orig {
        at: u64,
        len: u64,
    },
    Bytes(Zeroizing<Vec<u8>>),
    Zero(u64),
}

impl Part {
    fn len(&self) -> u64 {
        match self {
            Part::Orig { len, .. } | Part::Zero(len) => *len,
            Part::Bytes(b) => b.len() as u64,
        }
    }
}

/// 원본 일부와 새 바이트를 이어 붙여 읽습니다(되감기·건너뛰기 가능).
pub(crate) struct Spliced<R> {
    inner: R,
    /// (결과에서의 시작, 조각). 빈 조각은 없습니다.
    parts: Vec<(u64, Part)>,
    len: u64,
    pos: u64,
    /// 안쪽 원본의 지금 위치(이어 읽을 때 다시 찾지 않게).
    inner_pos: Option<u64>,
}

impl<R> Spliced<R> {
    fn new(inner: R, parts: Vec<Part>) -> Self {
        let mut at = 0;
        let parts = parts
            .into_iter()
            .filter(|p| p.len() > 0)
            .map(|p| {
                let start = at;
                at += p.len();
                (start, p)
            })
            .collect();
        Spliced {
            inner,
            parts,
            len: at,
            pos: 0,
            inner_pos: None,
        }
    }
}

impl<R: Read + Seek> Read for Spliced<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.pos >= self.len || buf.is_empty() {
            return Ok(0);
        }
        let i = self.parts.partition_point(|(s, _)| *s <= self.pos) - 1;
        let (start, part) = &self.parts[i];
        let off = self.pos - start;
        let want = usize::try_from(part.len() - off)
            .unwrap_or(usize::MAX)
            .min(buf.len());
        let n = match part {
            Part::Orig { at, .. } => {
                let src = at + off;
                if self.inner_pos != Some(src) {
                    self.inner.seek(SeekFrom::Start(src))?;
                }
                let n = self.inner.read(&mut buf[..want])?;
                if n == 0 {
                    return Err(std::io::ErrorKind::UnexpectedEof.into());
                }
                self.inner_pos = Some(src + n as u64);
                n
            }
            Part::Bytes(b) => {
                let o = off as usize;
                buf[..want].copy_from_slice(&b[o..o + want]);
                want
            }
            Part::Zero(_) => {
                buf[..want].fill(0);
                want
            }
        };
        self.pos += n as u64;
        Ok(n)
    }
}

impl<R> Seek for Spliced<R> {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        let to = match pos {
            SeekFrom::Start(n) => Some(n),
            SeekFrom::End(d) => self.len.checked_add_signed(d),
            SeekFrom::Current(d) => self.pos.checked_add_signed(d),
        };
        self.pos = to.ok_or(std::io::ErrorKind::InvalidInput)?;
        Ok(self.pos)
    }
}

/// `src`(처음부터 `len`바이트)에서 메타데이터를 뺀 원본과 그 길이. 지울 것이 없거나 모르는 형식이면 그대로
/// 돌려줍니다. 돌려준 원본은 처음 위치에 있습니다.
pub(crate) fn clean(src: Source, len: u64) -> Result<(Source, u64), RpcError> {
    let fail = || {
        RpcError::invalid_input(
            "첨부의 메타데이터를 지우지 못했습니다(파일이 깨졌을 수 있음). 메타데이터 지우기를 끄면 그대로 보낼 수 있습니다",
        )
    };
    let mut rd = Rd {
        r: BufReader::new(src),
        pos: 0,
    };
    let mut head = [0u8; 12];
    let got = rd.r.read(&mut head).map_err(|_| fail())?;
    rd.r.seek(SeekFrom::Start(0)).map_err(|_| fail())?;
    let parts = match sniff_magic(&head[..got]) {
        Some("image/jpeg") => jpeg(&mut rd, len),
        Some("image/png") => png(&mut rd, len),
        Some("image/webp") => webp(&mut rd, len),
        Some("image/gif") => gif(&mut rd, len),
        Some("video/mp4" | "video/quicktime" | "audio/mp4") => mp4(&mut rd, len),
        _ => Some(vec![Part::Orig { at: 0, len }]),
    }
    .ok_or_else(fail)?;
    let mut src = rd.r.into_inner();
    if let [Part::Orig { at: 0, len: l }] = parts[..]
        && l == len
    {
        src.seek(SeekFrom::Start(0)).map_err(|_| fail())?;
        return Ok((src, len));
    }
    let s = Spliced::new(src, parts);
    let n = s.len;
    Ok((Source::Spliced(Box::new(s)), n))
}

/// 프로필 이미지·크루 아이콘·이모지용: 설정과 상관없이 늘 지우고, 깨진 이미지는 받지 않습니다.
pub(crate) fn clean_image(src: Source, len: u64) -> Result<(Source, u64), RpcError> {
    clean(src, len).map_err(|_| broken_image())
}

/// 메모리의 이미지(이모지)에서 메타데이터를 뺀 바이트.
pub(crate) fn clean_image_bytes(b: Vec<u8>) -> Result<Vec<u8>, RpcError> {
    let n = b.len() as u64;
    let (mut s, len) = clean_image(Source::mem(Shared::new(b)), n)?;
    let mut out = Vec::with_capacity(usize::try_from(len).unwrap_or(0));
    s.read_to_end(&mut out).map_err(|_| broken_image())?;
    Ok(out)
}

fn broken_image() -> RpcError {
    RpcError::invalid_input("이미지가 깨져 있어 쓸 수 없습니다")
}

/// 결과 조각을 모읍니다(붙은 원본 범위는 하나로).
#[derive(Default)]
struct Out(Vec<Part>);

impl Out {
    fn keep(&mut self, at: u64, len: u64) {
        if let Some(Part::Orig { at: a, len: l }) = self.0.last_mut()
            && *a + *l == at
        {
            *l += len;
            return;
        }
        self.0.push(Part::Orig { at, len });
    }

    fn add(&mut self, b: Vec<u8>) {
        self.0.push(Part::Bytes(Zeroizing::new(b)));
    }

    fn total(&self) -> u64 {
        self.0.iter().map(Part::len).sum()
    }
}

/// 위치를 세며 읽습니다.
struct Rd<R> {
    r: BufReader<R>,
    pos: u64,
}

impl<R: Read + Seek> Rd<R> {
    fn byte(&mut self) -> Option<u8> {
        Some(self.exact::<1>()?[0])
    }

    fn exact<const N: usize>(&mut self) -> Option<[u8; N]> {
        let mut b = [0u8; N];
        self.r.read_exact(&mut b).ok()?;
        self.pos += N as u64;
        Some(b)
    }

    fn bytes(&mut self, n: usize) -> Option<Vec<u8>> {
        let mut b = vec![0u8; n];
        self.r.read_exact(&mut b).ok()?;
        self.pos += n as u64;
        Some(b)
    }

    fn skip(&mut self, n: u64) -> Option<()> {
        self.r.seek_relative(i64::try_from(n).ok()?).ok()?;
        self.pos += n;
        Some(())
    }

    fn goto(&mut self, at: u64) -> Option<()> {
        self.r.seek(SeekFrom::Start(at)).ok()?;
        self.pos = at;
        Some(())
    }
}

// ───────────── 회전값(EXIF) ─────────────

/// TIFF(EXIF 본문)의 IFD0 회전값. 1(그대로)이거나 없으면 `None`.
fn orientation(tiff: &[u8]) -> Option<u8> {
    let le = match tiff.get(..2)? {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    let u16_at = |i: usize| {
        let b: [u8; 2] = tiff.get(i..i + 2)?.try_into().ok()?;
        Some(if le {
            u16::from_le_bytes(b)
        } else {
            u16::from_be_bytes(b)
        })
    };
    let b: [u8; 4] = tiff.get(4..8)?.try_into().ok()?;
    let ifd = if le {
        u32::from_le_bytes(b)
    } else {
        u32::from_be_bytes(b)
    } as usize;
    for k in 0..usize::from(u16_at(ifd)?) {
        let e = ifd + 2 + 12 * k;
        if u16_at(e)? == 0x0112 {
            return u8::try_from(u16_at(e + 8)?)
                .ok()
                .filter(|o| (2..=8).contains(o));
        }
    }
    None
}

/// 회전값 하나만 담은 TIFF.
fn orientation_tiff(o: u8) -> Vec<u8> {
    let mut t = b"MM\0\x2A\0\0\0\x08\0\x01".to_vec();
    t.extend_from_slice(&[0x01, 0x12, 0, 3, 0, 0, 0, 1, 0, o, 0, 0]);
    t.extend_from_slice(&[0, 0, 0, 0]);
    t
}

// ───────────── JPEG ─────────────

fn jpeg<R: Read + Seek>(rd: &mut Rd<R>, len: u64) -> Option<Vec<Part>> {
    let mut out = Out::default();
    if rd.exact::<2>()? != [0xFF, 0xD8] {
        return None;
    }
    out.keep(0, 2);
    let mut next: Option<u8> = None;
    for _ in 0..MAX_STEPS {
        let m = match next.take() {
            Some(m) => m,
            None => {
                if rd.byte()? != 0xFF {
                    return None;
                }
                let mut m = rd.byte()?;
                // 채움 바이트.
                while m == 0xFF {
                    m = rd.byte()?;
                }
                m
            }
        };
        let at = rd.pos - 2;
        match m {
            0xD9 => {
                // EOI 뒤(모션 포토 동영상·게인 맵 등)는 자릅니다.
                out.keep(at, 2);
                return Some(out.0);
            }
            0x01 | 0xD0..=0xD7 => out.keep(at, 2),
            0xD8 => return None,
            _ => {
                let l = u64::from(u16::from_be_bytes(rd.exact::<2>()?));
                if l < 2 {
                    return None;
                }
                let body = rd.bytes(l as usize - 2)?;
                let starts = |p: &[u8]| body.starts_with(p);
                match m {
                    0xE0 if starts(b"JFIF\0") => out.keep(at, 2 + l),
                    0xE2 if starts(b"ICC_PROFILE\0") => out.keep(at, 2 + l),
                    0xEE if starts(b"Adobe") => out.keep(at, 2 + l),
                    0xE1 if starts(b"Exif\0\0") => {
                        if let Some(o) = orientation(&body[6..]) {
                            let tiff = orientation_tiff(o);
                            let mut seg = vec![0xFF, 0xE1];
                            seg.extend_from_slice(&(2 + 6 + tiff.len() as u16).to_be_bytes());
                            seg.extend_from_slice(b"Exif\0\0");
                            seg.extend_from_slice(&tiff);
                            out.add(seg);
                        }
                    }
                    0xE0..=0xEF | 0xFE => {}
                    _ => out.keep(at, 2 + l),
                }
                if m == 0xDA {
                    // 스캔 데이터: 다음 표시(FF 00·RST 밖)까지 그대로.
                    let start = rd.pos;
                    loop {
                        let buf = rd.r.fill_buf().ok()?;
                        if buf.is_empty() {
                            // EOI 없이 끝난 파일은 끝까지 둡니다.
                            out.keep(start, rd.pos - start);
                            return (rd.pos == len).then_some(out.0);
                        }
                        let Some(i) = buf.iter().position(|&b| b == 0xFF) else {
                            let n = buf.len();
                            rd.r.consume(n);
                            rd.pos += n as u64;
                            continue;
                        };
                        rd.r.consume(i + 1);
                        rd.pos += i as u64 + 1;
                        let mut b = rd.byte()?;
                        while b == 0xFF {
                            b = rd.byte()?;
                        }
                        if b != 0x00 && !(0xD0..=0xD7).contains(&b) {
                            out.keep(start, rd.pos - 2 - start);
                            next = Some(b);
                            break;
                        }
                    }
                }
            }
        }
    }
    None
}

// ───────────── PNG ─────────────

fn png<R: Read + Seek>(rd: &mut Rd<R>, len: u64) -> Option<Vec<Part>> {
    let mut out = Out::default();
    rd.skip(8)?;
    out.keep(0, 8);
    for _ in 0..MAX_STEPS {
        if rd.pos == len {
            return Some(out.0);
        }
        let at = rd.pos;
        let n = u64::from(u32::from_be_bytes(rd.exact::<4>()?));
        let ty = rd.exact::<4>()?;
        let size = 12 + n;
        if at + size > len {
            return None;
        }
        rd.skip(n + 4)?;
        // shortcut: eXIf의 회전값은 남기지 않음(PNG 사진에 회전값이 거의 없음), 문제가 되면 CRC를 다시 계산해 남김.
        if !matches!(&ty, b"tEXt" | b"zTXt" | b"iTXt" | b"eXIf" | b"tIME") {
            out.keep(at, size);
        }
        if &ty == b"IEND" {
            return Some(out.0);
        }
    }
    None
}

// ───────────── WebP ─────────────

fn webp<R: Read + Seek>(rd: &mut Rd<R>, len: u64) -> Option<Vec<Part>> {
    let riff = rd.exact::<12>()?;
    let end = (u64::from(u32::from_le_bytes(riff[4..8].try_into().ok()?)) + 8).min(len);
    let mut out = Out::default();
    // 머리(RIFF 크기)는 끝에 채웁니다.
    out.add(Vec::new());
    let mut vp8x: Option<(usize, [u8; 10])> = None;
    let mut exif_kept = false;
    for _ in 0..MAX_STEPS {
        if rd.pos + 8 > end {
            break;
        }
        let at = rd.pos;
        let ty = rd.exact::<4>()?;
        let n = u64::from(u32::from_le_bytes(rd.exact::<4>()?));
        let padded = n + (n & 1);
        if at + 8 + padded > end + (n & 1) {
            return None;
        }
        match &ty {
            b"VP8X" if n == 10 => {
                vp8x = Some((out.0.len(), rd.exact::<10>()?));
                rd.skip(padded - 10)?;
                out.add(Vec::new());
            }
            b"EXIF" => {
                let body = rd.bytes(usize::try_from(n).ok()?)?;
                rd.skip(padded - n)?;
                let tiff = body.strip_prefix(b"Exif\0\0").unwrap_or(&body);
                if let Some(o) = orientation(tiff) {
                    let t = orientation_tiff(o);
                    let mut c = b"EXIF".to_vec();
                    c.extend_from_slice(&(t.len() as u32).to_le_bytes());
                    c.extend_from_slice(&t);
                    out.add(c);
                    exif_kept = true;
                }
            }
            b"XMP " => rd.skip(padded)?,
            _ => {
                rd.skip(padded)?;
                out.keep(at, (8 + padded).min(end - at));
            }
        }
    }
    if let Some((i, mut v)) = vp8x {
        // 표시: 0x08 EXIF, 0x04 XMP.
        v[0] &= !0x04;
        if !exif_kept {
            v[0] &= !0x08;
        }
        let mut c = b"VP8X".to_vec();
        c.extend_from_slice(&10u32.to_le_bytes());
        c.extend_from_slice(&v);
        out.0[i] = Part::Bytes(Zeroizing::new(c));
    }
    let size = u32::try_from(out.total() + 12 - 8).ok()?;
    let mut h = b"RIFF".to_vec();
    h.extend_from_slice(&size.to_le_bytes());
    h.extend_from_slice(b"WEBP");
    out.0[0] = Part::Bytes(Zeroizing::new(h));
    Some(out.0)
}

// ───────────── GIF ─────────────

fn gif<R: Read + Seek>(rd: &mut Rd<R>, len: u64) -> Option<Vec<Part>> {
    let mut out = Out::default();
    let head = rd.exact::<13>()?;
    let table = |flags: u8| {
        if flags & 0x80 != 0 {
            3u64 << ((flags & 7) + 1)
        } else {
            0
        }
    };
    rd.skip(table(head[10]))?;
    out.keep(0, rd.pos);
    let sub_blocks = |rd: &mut Rd<R>| -> Option<()> {
        for _ in 0..MAX_STEPS {
            match rd.byte()? {
                0 => return Some(()),
                n => rd.skip(u64::from(n))?,
            }
        }
        None
    };
    for _ in 0..MAX_STEPS {
        let at = rd.pos;
        match rd.byte()? {
            0x2C => {
                let d = rd.exact::<9>()?;
                rd.skip(table(d[8]) + 1)?;
                sub_blocks(rd)?;
                out.keep(at, rd.pos - at);
            }
            0x21 => {
                let label = rd.byte()?;
                // 첫 블록(앱 확장이면 이름)을 읽고, 끝 표시가 아니었으면 나머지 블록을 건너뜁니다.
                let first = match rd.byte()? {
                    0 => Vec::new(),
                    n => {
                        let b = rd.bytes(usize::from(n))?;
                        sub_blocks(rd)?;
                        b
                    }
                };
                let keep = match label {
                    0xFE => false,
                    0xFF => first.starts_with(b"NETSCAPE2.0") || first.starts_with(b"ANIMEXTS1.0"),
                    _ => true,
                };
                if keep {
                    out.keep(at, rd.pos - at);
                }
            }
            0x3B => {
                out.keep(at, 1);
                return Some(out.0);
            }
            _ => return None,
        }
        if rd.pos > len {
            return None;
        }
    }
    None
}

// ───────────── MP4·MOV ─────────────

/// XMP를 담는 `uuid` 상자의 UUID.
const XMP_UUID: [u8; 16] = [
    0xBE, 0x7A, 0xCF, 0xCB, 0x97, 0xA9, 0x42, 0xE8, 0x9C, 0x71, 0x99, 0x94, 0x91, 0xE3, 0xAF, 0xAC,
];

/// 상자 하나: (시작, 머리 길이, 끝, 종류).
type BoxAt = (u64, u64, u64, [u8; 4]);

fn mp4_boxes<R: Read + Seek>(rd: &mut Rd<R>, start: u64, end: u64) -> Option<Vec<BoxAt>> {
    let mut v = Vec::new();
    let mut pos = start;
    while pos + 8 <= end {
        if v.len() >= MAX_STEPS {
            return None;
        }
        rd.goto(pos)?;
        let size = u64::from(u32::from_be_bytes(rd.exact::<4>()?));
        let ty = rd.exact::<4>()?;
        let (header, size) = match size {
            1 => (16, u64::from_be_bytes(rd.exact::<8>()?)),
            0 => (8, end - pos),
            n => (8, n),
        };
        if size < header || pos + size > end {
            return None;
        }
        v.push((pos, header, pos + size, ty));
        pos += size;
    }
    Some(v)
}

fn mp4<R: Read + Seek>(rd: &mut Rd<R>, len: u64) -> Option<Vec<Part>> {
    // (위치, 바꿀 내용). 상자끼리 겹치지 않아 위치 순으로 늘어놓으면 됩니다.
    let mut edits: Vec<(u64, Part)> = Vec::new();
    let free = |edits: &mut Vec<(u64, Part)>, (pos, header, end, _): BoxAt| {
        edits.push((pos + 4, Part::Bytes(Zeroizing::new(b"free".to_vec()))));
        edits.push((pos + header, Part::Zero(end - pos - header)));
    };
    // 만든·고친 시각(판 0: 4+4, 판 1: 8+8)을 0으로.
    let times = |rd: &mut Rd<R>, edits: &mut Vec<(u64, Part)>, (pos, header, end, _): BoxAt| {
        let body = pos + header;
        rd.goto(body)?;
        let n = if rd.byte()? == 1 { 16 } else { 8 };
        if body + 4 + n <= end {
            edits.push((body + 4, Part::Zero(n)));
        }
        Some(())
    };
    for b in mp4_boxes(rd, 0, len)? {
        match &b.3 {
            b"moov" => {
                for c in mp4_boxes(rd, b.0 + b.1, b.2)? {
                    match &c.3 {
                        b"mvhd" => times(rd, &mut edits, c)?,
                        b"udta" | b"meta" => free(&mut edits, c),
                        b"trak" => {
                            for t in mp4_boxes(rd, c.0 + c.1, c.2)? {
                                match &t.3 {
                                    b"tkhd" => times(rd, &mut edits, t)?,
                                    b"udta" | b"meta" => free(&mut edits, t),
                                    b"mdia" => {
                                        for m in mp4_boxes(rd, t.0 + t.1, t.2)? {
                                            if &m.3 == b"mdhd" {
                                                times(rd, &mut edits, m)?;
                                            }
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            b"udta" | b"meta" => free(&mut edits, b),
            b"uuid" => {
                rd.goto(b.0 + b.1)?;
                if b.2 - b.0 - b.1 >= 16 && rd.exact::<16>()? == XMP_UUID {
                    free(&mut edits, b);
                }
            }
            _ => {}
        }
    }
    let mut out = Out::default();
    let mut at = 0;
    for (pos, p) in edits {
        out.keep(at, pos - at);
        at = pos + p.len();
        out.0.push(p);
    }
    out.keep(at, len - at);
    Some(out.0)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::paste::Shared;

    fn run(data: &[u8]) -> Vec<u8> {
        let (mut s, n) = clean(Source::mem(Shared::new(data.to_vec())), data.len() as u64).unwrap();
        let mut v = Vec::new();
        s.read_to_end(&mut v).unwrap();
        assert_eq!(v.len() as u64, n);
        // 되감아 다시 읽어도 같음(암호화가 두 번 읽음).
        s.seek(SeekFrom::Start(0)).unwrap();
        let mut again = Vec::new();
        s.read_to_end(&mut again).unwrap();
        assert_eq!(v, again);
        v
    }

    fn has(hay: &[u8], needle: &[u8]) -> bool {
        hay.windows(needle.len()).any(|w| w == needle)
    }

    fn exif_tiff(orientation: u16) -> Vec<u8> {
        // IFD0: 회전값, 그리고 GPS IFD 가리킴 대신 알아보기 쉬운 표시 글자.
        let mut t = b"II\x2A\0\x08\0\0\0\x01\0".to_vec();
        t.extend_from_slice(&[0x12, 0x01, 3, 0, 1, 0, 0, 0]);
        t.extend_from_slice(&orientation.to_le_bytes());
        t.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
        t.extend_from_slice(b"GPS-SECRET");
        t
    }

    fn jpeg_with(orientation: u16) -> Vec<u8> {
        let img = image::RgbImage::from_pixel(8, 4, image::Rgb([200, 30, 30]));
        let mut plain = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut plain)
            .encode_image(&img)
            .unwrap();
        let mut body = b"Exif\0\0".to_vec();
        body.extend_from_slice(&exif_tiff(orientation));
        let mut v = plain[..2].to_vec();
        v.extend_from_slice(&[0xFF, 0xE1]);
        v.extend_from_slice(&(body.len() as u16 + 2).to_be_bytes());
        v.extend_from_slice(&body);
        v.extend_from_slice(&[0xFF, 0xFE, 0, 8]);
        v.extend_from_slice(b"author");
        v.extend_from_slice(&plain[2..]);
        // EOI 뒤에 붙은 것(모션 포토 흉내).
        v.extend_from_slice(b"TRAILING-GPS-SECRET");
        v
    }

    #[test]
    fn jpeg_drops_exif_keeps_orientation_and_decodes() {
        let out = run(&jpeg_with(6));
        assert!(!has(&out, b"GPS-SECRET"));
        assert!(!has(&out, b"author"));
        assert_eq!(&out[out.len() - 2..], &[0xFF, 0xD9]);
        let mut dec = image::codecs::jpeg::JpegDecoder::new(Cursor::new(&out)).unwrap();
        use image::ImageDecoder;
        assert_eq!(
            dec.orientation().unwrap(),
            image::metadata::Orientation::Rotate90
        );
        assert_eq!(image::load_from_memory(&out).unwrap().width(), 8);
        // 회전값 1이면 EXIF를 아예 넣지 않습니다.
        assert!(!has(&run(&jpeg_with(1)), b"Exif"));
        // 같은 입력이면 같은 결과(다시 잇기).
        assert_eq!(run(&jpeg_with(6)), out);
    }

    #[test]
    fn png_drops_text_chunks() {
        let img = image::RgbaImage::from_pixel(3, 2, image::Rgba([1, 2, 3, 255]));
        let mut plain = Vec::new();
        img.write_to(&mut Cursor::new(&mut plain), image::ImageFormat::Png)
            .unwrap();
        let data = b"Author\0GPS-SECRET";
        let mut text = (data.len() as u32).to_be_bytes().to_vec();
        text.extend_from_slice(b"tEXt");
        text.extend_from_slice(data);
        text.extend_from_slice(&[0, 0, 0, 0]);
        let mut v = plain[..33].to_vec();
        v.extend_from_slice(&text);
        v.extend_from_slice(&plain[33..]);
        v.extend_from_slice(b"junk");
        let out = run(&v);
        assert_eq!(out, plain);
        image::load_from_memory(&out).unwrap();
    }

    #[test]
    fn webp_drops_exif_and_xmp() {
        let tiff = exif_tiff(1);
        let mut chunks = b"VP8X".to_vec();
        chunks.extend_from_slice(&10u32.to_le_bytes());
        chunks.extend_from_slice(&[0x0C, 0, 0, 0, 7, 0, 0, 3, 0, 0]);
        chunks.extend_from_slice(b"VP8L");
        chunks.extend_from_slice(&3u32.to_le_bytes());
        chunks.extend_from_slice(&[1, 2, 3, 0]);
        chunks.extend_from_slice(b"EXIF");
        chunks.extend_from_slice(&(tiff.len() as u32).to_le_bytes());
        chunks.extend_from_slice(&tiff);
        chunks.extend_from_slice(b"XMP ");
        chunks.extend_from_slice(&4u32.to_le_bytes());
        chunks.extend_from_slice(b"<x/>");
        let mut v = b"RIFF".to_vec();
        v.extend_from_slice(&(chunks.len() as u32 + 4).to_le_bytes());
        v.extend_from_slice(b"WEBP");
        v.extend_from_slice(&chunks);
        let out = run(&v);
        assert!(!has(&out, b"GPS-SECRET") && !has(&out, b"<x/>"));
        assert_eq!(
            u32::from_le_bytes(out[4..8].try_into().unwrap()) as usize,
            out.len() - 8
        );
        // VP8X의 EXIF·XMP 표시가 꺼짐.
        assert_eq!(out[20] & 0x0C, 0);
        assert!(has(&out, b"VP8L"));
    }

    #[test]
    fn gif_drops_comments_keeps_loop() {
        let mut v = b"GIF89a\x01\0\x01\0\x80\0\0".to_vec();
        v.extend_from_slice(&[0, 0, 0, 255, 255, 255]);
        v.extend_from_slice(b"\x21\xFF\x0BNETSCAPE2.0\x03\x01\0\0\0");
        v.extend_from_slice(b"\x21\xFE\x0AGPS-SECRET\0");
        v.extend_from_slice(b"\x21\xFF\x0BXMP DataXMP\x04<x/>\0");
        v.extend_from_slice(b"\x2C\0\0\0\0\x01\0\x01\0\0\x02\x02\x44\x01\0");
        v.extend_from_slice(b"\x3Bjunk");
        let out = run(&v);
        assert!(!has(&out, b"GPS-SECRET") && !has(&out, b"XMP"));
        assert!(has(&out, b"NETSCAPE2.0"));
        assert_eq!(*out.last().unwrap(), 0x3B);
        image::load_from_memory(&out).unwrap();
    }

    fn mp4_box(ty: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut v = (body.len() as u32 + 8).to_be_bytes().to_vec();
        v.extend_from_slice(ty);
        v.extend_from_slice(body);
        v
    }

    #[test]
    fn mp4_frees_udta_and_zeroes_times() {
        let mut mvhd = vec![0u8; 100];
        mvhd[4..8].copy_from_slice(&0x1234_5678u32.to_be_bytes());
        mvhd[12..16].copy_from_slice(&1000u32.to_be_bytes());
        mvhd[16..20].copy_from_slice(&5000u32.to_be_bytes());
        let udta = mp4_box(b"udta", &mp4_box(b"\xA9xyz", b"+37.5+127.0/GPS-SECRET"));
        let mut moov_body = mp4_box(b"mvhd", &mvhd);
        moov_body.extend_from_slice(&udta);
        let mut v = mp4_box(b"ftyp", b"isom\0\0\0\0");
        v.extend_from_slice(&mp4_box(b"moov", &moov_body));
        v.extend_from_slice(&mp4_box(b"mdat", b"frames"));
        let out = run(&v);
        assert_eq!(out.len(), v.len());
        assert!(!has(&out, b"GPS-SECRET") && !has(&out, b"udta"));
        assert!(has(&out, b"free") && has(&out, b"frames"));
        assert!(!has(&out, &0x1234_5678u32.to_be_bytes()));
        let d = super::super::media::mp4_duration_ms(&mut Cursor::new(&out), out.len() as u64);
        assert_eq!(d, Some(5000));
    }

    /// 서명과 IHDR 머리만 있는 깨진 PNG.
    fn png_head() -> Vec<u8> {
        let mut v = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        v.extend_from_slice(&13u32.to_be_bytes());
        v.extend_from_slice(b"IHDR");
        v
    }

    #[test]
    fn image_bytes_lose_metadata() {
        let out = clean_image_bytes(jpeg_with(6)).unwrap();
        assert!(!has(&out, b"GPS-SECRET"));
        assert!(clean_image_bytes(png_head()).is_err());
    }

    #[test]
    fn other_formats_pass_through() {
        assert_eq!(run(b"%PDF-1.7 /Author (me)"), b"%PDF-1.7 /Author (me)");
        assert!(clean(Source::mem(Shared::new(b"\xFF\xD8\xFF".to_vec())), 3).is_err());
    }
}
