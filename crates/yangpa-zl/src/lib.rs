//! 기기 사이를 오가는 데이터의 OpenZL(v0.3.0) 압축.
//!
//! 압축은 암호화 **앞**에서 합니다(암호문은 압축되지 않음). 짧은 메시지나 이미 압축된 사진·영상은 OpenZL 프레임이 오히려
//! 커지므로, [`pack`]은 표시 바이트 하나를 앞에 두고 더 작을 때만 압축합니다:
//! - [`TAG_RAW`] + 원래 바이트
//! - [`TAG_ZL`] + OpenZL 프레임(형식 판 [`FORMAT_VERSION`]으로 고정해, 같은 입력은 어느 기기에서나 같은 출력)
//!
//! 표시 바이트는 0xF0·0xF1이라, 예전 형식(postcard 열거 번호·CBOR 맨 앞 바이트)과 겹치지 않아 읽는 쪽이 예전 데이터를
//! 가려낼 수 있습니다([`is_packed`]). 풀 때는 부르는 쪽이 정한 상한([`unpack`]의 `max`)보다 커지면 거절해 압축 폭탄을
//! 막습니다(크기는 프레임 머리글로 먼저 봄). 평문이 지나가는 버퍼는 모두 해제할 때 0으로 덮어씁니다.

use std::cell::RefCell;
use std::ffi::CStr;
use std::ptr::NonNull;

use openzl_sys as zl;
use zeroize::Zeroizing;

/// 압축하지 않은 내용.
pub const TAG_RAW: u8 = 0xF0;
/// OpenZL 프레임.
pub const TAG_ZL: u8 = 0xF1;
/// OpenZL 인코딩 형식 판. 라이브러리 기본값은 판마다 바뀔 수 있어 고정합니다(OpenZL은 태그된 판으로 만든 프레임을
/// 이후 판에서도 풀 수 있다고 약속함). 판을 올리면 보내는 형식이 바뀌므로 문서(protocol.md "압축")도 고칩니다.
pub const FORMAT_VERSION: i32 = 27;
/// 이보다 짧으면 압축을 해 보지도 않습니다(OpenZL 프레임 머리글만으로 더 커짐).
pub const MIN_COMPRESS: usize = 64;

const _: () = assert!(
    FORMAT_VERSION as u32 >= zl::ZL_MIN_FORMAT_VERSION
        && FORMAT_VERSION as u32 <= zl::ZL_MAX_FORMAT_VERSION
);

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ZlError {
    #[error("압축 데이터 형식이 잘못됐습니다")]
    Malformed,
    #[error("풀면 너무 큽니다({size}바이트, 최대 {max}바이트)")]
    TooLarge { size: usize, max: usize },
    #[error("OpenZL 압축 실패: {0}")]
    Compress(String),
}

pub type Result<T> = std::result::Result<T, ZlError>;

fn is_error(r: zl::ZL_Report) -> bool {
    // SAFETY: ZL_isError는 값만 읽습니다.
    unsafe { zl::ZL_isError(r) != 0 }
}

fn value(r: zl::ZL_Report) -> usize {
    // SAFETY: 오류가 아닌 보고서의 공용체는 `_value`입니다(is_error로 먼저 거름).
    unsafe { r._value._value }
}

struct CCtx(NonNull<zl::ZL_CCtx>);
struct DCtx(NonNull<zl::ZL_DCtx>);

impl Drop for CCtx {
    fn drop(&mut self) {
        // SAFETY: ZL_CCtx_create로 만든 포인터를 한 번만 놓습니다.
        unsafe { zl::ZL_CCtx_free(self.0.as_ptr()) }
    }
}

impl Drop for DCtx {
    fn drop(&mut self) {
        // SAFETY: ZL_DCtx_create로 만든 포인터를 한 번만 놓습니다.
        unsafe { zl::ZL_DCtx_free(self.0.as_ptr()) }
    }
}

impl CCtx {
    fn new() -> Result<Self> {
        // SAFETY: 인자 없는 생성 함수. 실패하면 null.
        let p = NonNull::new(unsafe { zl::ZL_CCtx_create() })
            .ok_or_else(|| ZlError::Compress("문맥을 만들지 못함".into()))?;
        let c = CCtx(p);
        // 끈끈한(sticky) 매개변수: 압축할 때마다 형식 판이 기본값으로 돌아가지 않게.
        for (param, v) in [
            (zl::ZL_CParam_stickyParameters, 1),
            (zl::ZL_CParam_formatVersion, FORMAT_VERSION),
        ] {
            // SAFETY: 살아 있는 문맥에 정수 매개변수를 정합니다.
            let r = unsafe { zl::ZL_CCtx_setParameter(c.0.as_ptr(), param, v) };
            if is_error(r) {
                return Err(c.error(r));
            }
        }
        Ok(c)
    }

    fn error(&self, r: zl::ZL_Report) -> ZlError {
        // SAFETY: 문맥이 살아 있는 동안 유효한 C 문자열을 돌려줍니다(널이면 비움).
        let msg = unsafe {
            let p = zl::ZL_CCtx_getErrorContextString(self.0.as_ptr(), r);
            if p.is_null() {
                String::new()
            } else {
                CStr::from_ptr(p).to_string_lossy().into_owned()
            }
        };
        ZlError::Compress(msg)
    }

    /// `dst` 뒤에 `src`의 OpenZL 프레임을 덧붙입니다.
    fn compress_into(&mut self, src: &[u8], dst: &mut Vec<u8>) -> Result<()> {
        let at = dst.len();
        // SAFETY: 순수 계산 함수.
        let bound = unsafe { zl::ZL_compressBound(src.len()) };
        dst.resize(at + bound, 0);
        // SAFETY: dst[at..]는 bound바이트, src는 src.len()바이트로 유효합니다.
        let r = unsafe {
            zl::ZL_CCtx_compress(
                self.0.as_ptr(),
                dst[at..].as_mut_ptr().cast(),
                bound,
                src.as_ptr().cast(),
                src.len(),
            )
        };
        if is_error(r) {
            return Err(self.error(r));
        }
        dst.truncate(at + value(r));
        Ok(())
    }
}

impl DCtx {
    fn new() -> Result<Self> {
        // SAFETY: 인자 없는 생성 함수. 실패하면 null.
        NonNull::new(unsafe { zl::ZL_DCtx_create() })
            .map(DCtx)
            .ok_or(ZlError::Malformed)
    }
}

thread_local! {
    // 스레드마다 문맥을 다시 씁니다(만들 때 드는 할당을 줄임). OpenZL 문맥은 스레드 사이에 나누지 않습니다.
    static CCTX: RefCell<Option<CCtx>> = const { RefCell::new(None) };
    static DCTX: RefCell<Option<DCtx>> = const { RefCell::new(None) };
}

fn with_cctx<T>(f: impl FnOnce(&mut CCtx) -> Result<T>) -> Result<T> {
    CCTX.with(|c| {
        let mut c = c.borrow_mut();
        if c.is_none() {
            *c = Some(CCtx::new()?);
        }
        f(c.as_mut().expect("방금 채움"))
    })
}

/// OpenZL 프레임으로 압축합니다(표시 바이트 없이). 보통은 [`pack`]을 쓰세요.
pub fn compress(src: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
    let mut out = Zeroizing::new(Vec::new());
    with_cctx(|c| c.compress_into(src, &mut out))?;
    Ok(out)
}

/// OpenZL 프레임이 풀리면 몇 바이트인지(프레임 머리글만 봄). 풀기 전에 메모리 예산을 잡을 때 씁니다.
pub fn decompressed_size(frame: &[u8]) -> Result<usize> {
    // SAFETY: frame은 frame.len()바이트로 유효합니다.
    let size = unsafe { zl::ZL_getDecompressedSize(frame.as_ptr().cast(), frame.len()) };
    if is_error(size) {
        return Err(ZlError::Malformed);
    }
    Ok(value(size))
}

/// OpenZL 프레임을 풉니다. 풀린 크기가 `max`를 넘으면(머리글로 먼저 확인) 거절합니다.
pub fn decompress(frame: &[u8], max: usize) -> Result<Zeroizing<Vec<u8>>> {
    let size = decompressed_size(frame)?;
    if size > max {
        return Err(ZlError::TooLarge { size, max });
    }
    DCTX.with(|d| {
        let mut d = d.borrow_mut();
        if d.is_none() {
            *d = Some(DCtx::new()?);
        }
        let d = d.as_ref().expect("방금 채움");
        let mut dst = Zeroizing::new(vec![0u8; size]);
        // SAFETY: dst는 size바이트, frame은 frame.len()바이트로 유효합니다.
        let r = unsafe {
            zl::ZL_DCtx_decompress(
                d.0.as_ptr(),
                dst.as_mut_ptr().cast(),
                dst.len(),
                frame.as_ptr().cast(),
                frame.len(),
            )
        };
        if is_error(r) || value(r) != size {
            return Err(ZlError::Malformed);
        }
        Ok(dst)
    })
}

/// 표시 바이트 + 내용. 압축한 쪽이 원래보다 작을 때만 OpenZL로, 아니면 그대로 둡니다.
pub fn pack(data: &[u8]) -> Zeroizing<Vec<u8>> {
    if data.len() >= MIN_COMPRESS {
        let mut out = Zeroizing::new(vec![TAG_ZL]);
        if with_cctx(|c| c.compress_into(data, &mut out)).is_ok() && out.len() <= data.len() {
            return out;
        }
    }
    let mut out = Zeroizing::new(Vec::with_capacity(data.len() + 1));
    out.push(TAG_RAW);
    out.extend_from_slice(data);
    out
}

/// 압축하지 않고 [`pack`] 형식으로 감쌉니다(이미 암호문·압축 파일이라 줄지 않을 것이 뻔할 때).
pub fn pack_raw(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 1);
    out.push(TAG_RAW);
    out.extend_from_slice(data);
    out
}

/// [`pack`]으로 만든 데이터인지(첫 바이트가 표시 바이트).
pub fn is_packed(data: &[u8]) -> bool {
    matches!(data.first(), Some(&TAG_RAW) | Some(&TAG_ZL))
}

/// [`pack`]한 데이터가 풀리면 몇 바이트인지(풀지 않고).
pub fn unpacked_size(data: &[u8]) -> Result<usize> {
    match data.split_first() {
        Some((&TAG_RAW, rest)) => Ok(rest.len()),
        Some((&TAG_ZL, rest)) => decompressed_size(rest),
        _ => Err(ZlError::Malformed),
    }
}

/// [`pack`]의 역. 풀린 크기가 `max`를 넘으면 거절합니다.
pub fn unpack(data: &[u8], max: usize) -> Result<Zeroizing<Vec<u8>>> {
    match data.split_first() {
        Some((&TAG_RAW, rest)) if rest.len() <= max => Ok(Zeroizing::new(rest.to_vec())),
        Some((&TAG_RAW, rest)) => Err(ZlError::TooLarge {
            size: rest.len(),
            max,
        }),
        Some((&TAG_ZL, rest)) => decompress(rest, max),
        _ => Err(ZlError::Malformed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(n: usize) -> Vec<u8> {
        let mut v = Vec::new();
        let mut i = 0;
        while v.len() < n {
            v.extend(format!("{i}번째 줄: 양파 메신저 동기화 기록 ok\n").as_bytes());
            i += 1;
        }
        v.truncate(n);
        v
    }

    fn random(n: usize) -> Vec<u8> {
        let mut x = 0x1234_5678_9abc_def0u64;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect()
    }

    #[test]
    fn compressible_data_shrinks_and_round_trips() {
        let d = text(200_000);
        let p = pack(&d);
        assert_eq!(p[0], TAG_ZL);
        assert!(p.len() < d.len() / 5, "{}", p.len());
        assert_eq!(unpack(&p, d.len()).unwrap().as_slice(), &d[..]);
        assert_eq!(unpacked_size(&p), Ok(d.len()));
    }

    #[test]
    fn short_or_incompressible_data_is_stored_as_is() {
        for d in [b"hi".to_vec(), random(4096)] {
            let p = pack(&d);
            assert_eq!(p[0], TAG_RAW);
            assert_eq!(&p[1..], &d[..]);
            assert_eq!(unpack(&p, d.len()).unwrap().as_slice(), &d[..]);
            assert_eq!(pack_raw(&d), *p);
        }
    }

    #[test]
    fn same_input_gives_the_same_frame() {
        let d = text(50_000);
        assert_eq!(*pack(&d), *pack(&d));
    }

    #[test]
    fn limits_and_garbage_are_rejected() {
        let zeros = vec![0u8; 10_000_000];
        let p = pack(&zeros);
        assert_eq!(p[0], TAG_ZL);
        assert!(p.len() < 10_000);
        assert_eq!(
            unpack(&p, 1 << 20).map(|_| ()),
            Err(ZlError::TooLarge {
                size: 10_000_000,
                max: 1 << 20
            })
        );
        assert_eq!(
            unpack(&[TAG_RAW, 1, 2, 3], 2).map(|_| ()),
            Err(ZlError::TooLarge { size: 3, max: 2 })
        );
        assert_eq!(
            unpack(&[TAG_ZL, 1, 2, 3, 4, 5, 6, 7, 8], 1 << 20).map(|_| ()),
            Err(ZlError::Malformed)
        );
        assert_eq!(
            unpack(&[0x01, 2, 3], 10).map(|_| ()),
            Err(ZlError::Malformed)
        );
        assert_eq!(unpack(&[], 10).map(|_| ()), Err(ZlError::Malformed));
        // 잘린 프레임
        assert!(unpack(&p[..p.len() / 2], 20_000_000).is_err());
    }

    #[test]
    #[ignore = "시간 재기: cargo test -p yangpa-zl --release -- --ignored --nocapture"]
    fn random_8mib_timing() {
        let d = random(8 << 20);
        let t = std::time::Instant::now();
        let p = pack(&d);
        eprintln!("무작위 8MiB pack: {:?}, 표시 {:#x}", t.elapsed(), p[0]);
        let d = text(8 << 20);
        let t = std::time::Instant::now();
        let p = pack(&d);
        let tc = t.elapsed();
        let t = std::time::Instant::now();
        unpack(&p, d.len()).unwrap();
        eprintln!(
            "글 8MiB pack: {tc:?} → {}바이트, unpack: {:?}",
            p.len(),
            t.elapsed()
        );
    }

    #[test]
    fn packed_data_is_recognised() {
        assert!(is_packed(&pack(b"x")));
        assert!(is_packed(&pack(&text(1000))));
        assert!(!is_packed(&[0x05, 1]));
        assert!(!is_packed(&[]));
    }
}
