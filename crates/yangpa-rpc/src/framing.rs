//! 로컬 API 프레이밍: `u32 BE 길이 | u8 종류 | 본문`.
//!
//! - 길이는 그 뒤에 오는 바이트 수(종류 1바이트 + 본문)입니다. 0은 허용하지 않습니다.
//! - 종류 `1` = JSON-RPC 메시지(UTF-8 JSON 하나), 종류 `2` = 바이너리(`u64 BE 요청 ID | 바이트`).
//! - 프레임 하나는 최대 [`MAX_FRAME`](16MiB)입니다. 넘으면 연결을 끊습니다(상대가 거대한 길이로 메모리를 잡지 못하게).

use bytes::{Buf, BufMut, Bytes, BytesMut};
use thiserror::Error;
use tokio_util::codec::{Decoder, Encoder};

/// 프레임 최대 크기(길이 필드가 가리키는 바이트 수 기준).
pub const MAX_FRAME: usize = 16 * 1024 * 1024;
/// 길이 필드 크기.
pub const LEN_BYTES: usize = 4;
/// JSON-RPC 프레임 종류.
pub const KIND_JSON: u8 = 1;
/// 바이너리 프레임 종류.
pub const KIND_BINARY: u8 = 2;
/// 바이너리 프레임의 요청 ID 크기.
const BIN_ID_BYTES: usize = 8;

/// 바이너리 프레임에 실을 수 있는 최대 바이트 수.
pub const MAX_BINARY_PAYLOAD: usize = MAX_FRAME - 1 - BIN_ID_BYTES;
/// JSON 프레임 본문 최대 크기.
pub const MAX_JSON_PAYLOAD: usize = MAX_FRAME - 1;

/// 프레임 하나.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Frame {
    /// JSON-RPC 메시지 하나(UTF-8 JSON). 해석은 [`crate::jsonrpc`]가 합니다.
    Json(Bytes),
    /// 바이너리 응답(예: `blob.read`의 `binary: true`). `id`는 JSON-RPC 요청 ID.
    Binary { id: u64, data: Bytes },
}

impl Frame {
    /// 직렬화된 JSON 값으로 프레임을 만듭니다.
    pub fn json<T: serde::Serialize>(value: &T) -> Result<Frame, serde_json::Error> {
        Ok(Frame::Json(Bytes::from(serde_json::to_vec(value)?)))
    }

    /// 길이 필드를 뺀 인코딩 크기.
    pub fn wire_len(&self) -> usize {
        match self {
            Frame::Json(b) => 1 + b.len(),
            Frame::Binary { data, .. } => 1 + BIN_ID_BYTES + data.len(),
        }
    }
}

#[derive(Debug, Error)]
pub enum FrameError {
    #[error("입출력 오류: {0}")]
    Io(#[from] std::io::Error),
    #[error("프레임이 너무 큼: {0}바이트(최대 {MAX_FRAME})")]
    TooLarge(usize),
    #[error("빈 프레임")]
    Empty,
    #[error("알 수 없는 프레임 종류: {0}")]
    UnknownKind(u8),
    #[error("바이너리 프레임이 너무 짧음")]
    BinaryTooShort,
}

/// 길이 접두 프레임 코덱.
#[derive(Clone, Debug)]
pub struct FrameCodec {
    max_frame: usize,
}

impl Default for FrameCodec {
    fn default() -> Self {
        FrameCodec {
            max_frame: MAX_FRAME,
        }
    }
}

impl FrameCodec {
    pub fn new() -> Self {
        Self::default()
    }

    /// 더 작은 상한이 필요할 때(테스트 등). [`MAX_FRAME`]보다 크게 잡을 수는 없습니다.
    pub fn with_max_frame(max_frame: usize) -> Self {
        FrameCodec {
            max_frame: max_frame.clamp(1 + BIN_ID_BYTES, MAX_FRAME),
        }
    }

    pub fn max_frame(&self) -> usize {
        self.max_frame
    }

    /// 상한을 바꿉니다(서버는 인증 전에는 작게, 인증 뒤에는 [`MAX_FRAME`]으로 씁니다).
    pub fn set_max_frame(&mut self, max_frame: usize) {
        self.max_frame = max_frame.clamp(1 + BIN_ID_BYTES, MAX_FRAME);
    }
}

/// 길이 필드만 보고 미리 잡아 둘 버퍼 상한. 길이만 보내고 본문을 안 보내는 상대가 연결마다 16MiB를 잡지 못하게,
/// 그 이상은 실제로 바이트가 들어오는 만큼만 늘립니다.
const MAX_PREALLOC: usize = 256 * 1024;

impl Decoder for FrameCodec {
    type Item = Frame;
    type Error = FrameError;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Frame>, FrameError> {
        if src.len() < LEN_BYTES {
            return Ok(None);
        }
        let mut len_bytes = [0u8; LEN_BYTES];
        len_bytes.copy_from_slice(&src[..LEN_BYTES]);
        let len = u32::from_be_bytes(len_bytes) as usize;
        if len == 0 {
            return Err(FrameError::Empty);
        }
        if len > self.max_frame {
            return Err(FrameError::TooLarge(len));
        }
        if src.len() < LEN_BYTES + len {
            // 작은 프레임은 한 번에 예약해 재할당을 줄이고, 큰 프레임은 들어오는 만큼만 늘립니다.
            src.reserve((LEN_BYTES + len - src.len()).min(MAX_PREALLOC));
            return Ok(None);
        }
        src.advance(LEN_BYTES);
        let mut body = src.split_to(len);
        let kind = body.get_u8();
        match kind {
            KIND_JSON => Ok(Some(Frame::Json(body.freeze()))),
            KIND_BINARY => {
                if body.len() < BIN_ID_BYTES {
                    return Err(FrameError::BinaryTooShort);
                }
                let id = body.get_u64();
                Ok(Some(Frame::Binary {
                    id,
                    data: body.freeze(),
                }))
            }
            other => Err(FrameError::UnknownKind(other)),
        }
    }
}

impl Encoder<Frame> for FrameCodec {
    type Error = FrameError;

    fn encode(&mut self, item: Frame, dst: &mut BytesMut) -> Result<(), FrameError> {
        let len = item.wire_len();
        if len > self.max_frame {
            return Err(FrameError::TooLarge(len));
        }
        dst.reserve(LEN_BYTES + len);
        // max_frame ≤ 16MiB이므로 u32에 들어갑니다.
        dst.put_u32(len as u32);
        match item {
            Frame::Json(b) => {
                dst.put_u8(KIND_JSON);
                dst.extend_from_slice(&b);
            }
            Frame::Binary { id, data } => {
                dst.put_u8(KIND_BINARY);
                dst.put_u64(id);
                dst.extend_from_slice(&data);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(frames: Vec<Frame>) -> Vec<Frame> {
        let mut codec = FrameCodec::new();
        let mut buf = BytesMut::new();
        for f in frames {
            codec.encode(f, &mut buf).unwrap();
        }
        let mut out = Vec::new();
        while let Some(f) = codec.decode(&mut buf).unwrap() {
            out.push(f);
        }
        assert!(buf.is_empty());
        out
    }

    #[test]
    fn json_and_binary_roundtrip() {
        let frames = vec![
            Frame::Json(Bytes::from_static(
                br#"{"jsonrpc":"2.0","id":1,"method":"system.version"}"#,
            )),
            Frame::Binary {
                id: 42,
                data: Bytes::from(vec![0u8, 1, 2, 255]),
            },
            Frame::Binary {
                id: u64::MAX,
                data: Bytes::new(),
            },
            Frame::Json(Bytes::from("{\"한글\":\"양파\"}".as_bytes().to_vec())),
        ];
        assert_eq!(roundtrip(frames.clone()), frames);
    }

    #[test]
    fn wire_layout_is_len_kind_body() {
        let mut codec = FrameCodec::new();
        let mut buf = BytesMut::new();
        codec
            .encode(
                Frame::Binary {
                    id: 7,
                    data: Bytes::from_static(b"ab"),
                },
                &mut buf,
            )
            .unwrap();
        assert_eq!(
            &buf[..],
            &[0, 0, 0, 11, KIND_BINARY, 0, 0, 0, 0, 0, 0, 0, 7, b'a', b'b']
        );
        let mut buf = BytesMut::new();
        codec
            .encode(Frame::Json(Bytes::from_static(b"{}")), &mut buf)
            .unwrap();
        assert_eq!(&buf[..], &[0, 0, 0, 3, KIND_JSON, b'{', b'}']);
    }

    #[test]
    fn partial_input_waits_for_more() {
        let mut codec = FrameCodec::new();
        let mut full = BytesMut::new();
        codec
            .encode(Frame::Json(Bytes::from_static(b"{\"a\":1}")), &mut full)
            .unwrap();
        let mut buf = BytesMut::new();
        for (i, b) in full.iter().enumerate() {
            buf.put_u8(*b);
            let r = codec.decode(&mut buf).unwrap();
            if i + 1 < full.len() {
                assert!(r.is_none());
            } else {
                assert_eq!(r, Some(Frame::Json(Bytes::from_static(b"{\"a\":1}"))));
            }
        }
    }

    #[test]
    fn oversize_rejected_without_waiting_for_body() {
        let mut codec = FrameCodec::new();
        let mut buf = BytesMut::new();
        buf.put_u32((MAX_FRAME + 1) as u32);
        buf.put_u8(KIND_JSON);
        assert!(
            matches!(codec.decode(&mut buf), Err(FrameError::TooLarge(n)) if n == MAX_FRAME + 1)
        );

        let mut small = FrameCodec::with_max_frame(16);
        let mut out = BytesMut::new();
        let big = Frame::Binary {
            id: 1,
            data: Bytes::from(vec![0u8; 64]),
        };
        assert!(matches!(
            small.encode(big, &mut out),
            Err(FrameError::TooLarge(_))
        ));
        assert!(out.is_empty());
    }

    #[test]
    fn header_alone_does_not_preallocate_full_frame() {
        let mut codec = FrameCodec::new();
        let mut buf = BytesMut::new();
        buf.put_u32(MAX_FRAME as u32);
        buf.put_u8(KIND_JSON);
        assert!(codec.decode(&mut buf).unwrap().is_none());
        assert!(buf.capacity() <= MAX_PREALLOC + 64, "{}", buf.capacity());
    }

    #[test]
    fn max_frame_can_be_changed() {
        let mut codec = FrameCodec::with_max_frame(64);
        let mut buf = BytesMut::new();
        buf.put_u32(100);
        assert!(matches!(
            codec.decode(&mut buf),
            Err(FrameError::TooLarge(100))
        ));
        codec.set_max_frame(usize::MAX);
        assert_eq!(codec.max_frame(), MAX_FRAME);
        let mut buf = BytesMut::new();
        buf.put_u32(100);
        assert!(codec.decode(&mut buf).unwrap().is_none());
    }

    #[test]
    fn exact_max_frame_accepted() {
        let mut codec = FrameCodec::new();
        let mut buf = BytesMut::new();
        let data = Bytes::from(vec![9u8; MAX_BINARY_PAYLOAD]);
        codec
            .encode(
                Frame::Binary {
                    id: 3,
                    data: data.clone(),
                },
                &mut buf,
            )
            .unwrap();
        assert_eq!(buf.len(), LEN_BYTES + MAX_FRAME);
        assert_eq!(
            codec.decode(&mut buf).unwrap(),
            Some(Frame::Binary { id: 3, data })
        );
        let too_big = Frame::Binary {
            id: 3,
            data: Bytes::from(vec![0u8; MAX_BINARY_PAYLOAD + 1]),
        };
        assert!(codec.encode(too_big, &mut BytesMut::new()).is_err());
    }

    #[test]
    fn malformed_frames_rejected() {
        let mut codec = FrameCodec::new();
        let mut buf = BytesMut::from(&[0u8, 0, 0, 0][..]);
        assert!(matches!(codec.decode(&mut buf), Err(FrameError::Empty)));

        let mut buf = BytesMut::from(&[0u8, 0, 0, 2, 9, 0][..]);
        assert!(matches!(
            codec.decode(&mut buf),
            Err(FrameError::UnknownKind(9))
        ));

        let mut buf = BytesMut::from(&[0u8, 0, 0, 4, KIND_BINARY, 0, 0, 1][..]);
        assert!(matches!(
            codec.decode(&mut buf),
            Err(FrameError::BinaryTooShort)
        ));
    }
}
