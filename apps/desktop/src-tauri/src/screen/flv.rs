//! ffmpeg가 표준 출력으로 내는 FLV에서 영상 설정 레코드와 프레임을 꺼냅니다. FLV를 고른 까닭은 태그마다 길이·키 프레임
//! 표시·시각이 붙어 있어, 다음 프레임이 올 때까지 기다리지 않고 프레임 하나가 다 오면 바로 넘길 수 있기 때문입니다.
//!
//! H.264는 옛 AVC 태그(코덱 7, avcC·4바이트 길이 NAL), HEVC·AV1은 확장 FLV(FourCC `hvc1`·`av01`, hvcC·av1C)로 옵니다
//! (ffmpeg libavformat/flvenc.c). AV1 키 프레임에 시퀀스 헤더 OBU가 없으면 av1C의 것을 앞에 붙여, 받는 쪽 디코더가 설명
//! 없이 키 프레임만으로 시작할 수 있게 합니다.

/// 화면 스트림의 영상 코덱.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoCodec {
    Av1 = 1,
    H264 = 2,
    Hevc = 3,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Event {
    /// 디코더 설정 레코드(avcC·hvcC·av1C).
    Config { codec: VideoCodec, record: Vec<u8> },
    /// 프레임 하나(H.264·HEVC는 4바이트 길이 NAL, AV1은 OBU).
    Frame {
        codec: VideoCodec,
        key: bool,
        ts_ms: u32,
        data: Vec<u8>,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub struct FlvError(pub &'static str);

const TAG_VIDEO: u8 = 9;
const EX_HEADER: u8 = 0x80;
const PKT_SEQUENCE_START: u8 = 0;
const PKT_CODED_FRAMES: u8 = 1;
const PKT_CODED_FRAMES_X: u8 = 3;
const FRAME_KEY: u8 = 1;
const LEGACY_AVC: u8 = 7;
/// 태그 하나의 상한(FLV 길이 칸이 24비트).
const MAX_TAG: usize = 1 << 24;
const OBU_SEQUENCE_HEADER: u8 = 1;

#[derive(Default)]
pub struct FlvReader {
    buf: Vec<u8>,
    /// 읽고 버릴 앞부분(파일 머리글 뒤).
    at: usize,
    header_done: bool,
    av1_config: Option<Vec<u8>>,
}

impl FlvReader {
    /// 받은 바이트를 넣고, 다 온 태그에서 나온 이벤트를 돌려줍니다.
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<Event>, FlvError> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        if !self.header_done {
            if self.buf.len() < 9 {
                return Ok(out);
            }
            if &self.buf[..3] != b"FLV" {
                return Err(FlvError("FLV 머리글이 아닙니다"));
            }
            let offset =
                u32::from_be_bytes([self.buf[5], self.buf[6], self.buf[7], self.buf[8]]) as usize;
            if !(9..=1024).contains(&offset) {
                return Err(FlvError("FLV 머리글 길이가 잘못됐습니다"));
            }
            // 머리글 뒤 첫 '앞 태그 길이'(0)까지 건너뜁니다.
            if self.buf.len() < offset + 4 {
                return Ok(out);
            }
            self.at = offset + 4;
            self.header_done = true;
        }
        loop {
            let rest = &self.buf[self.at..];
            if rest.len() < 11 {
                break;
            }
            let size = u32::from_be_bytes([0, rest[1], rest[2], rest[3]]) as usize;
            if size >= MAX_TAG {
                return Err(FlvError("FLV 태그가 너무 큽니다"));
            }
            let total = 11 + size + 4;
            if rest.len() < total {
                break;
            }
            let kind = rest[0] & 0x1f;
            let ts = u32::from_be_bytes([rest[7], rest[4], rest[5], rest[6]]) & 0x7fff_ffff;
            if kind == TAG_VIDEO {
                let body = rest[11..11 + size].to_vec();
                if let Some(e) = self.video(&body, ts)? {
                    out.push(e);
                }
            }
            self.at += total;
        }
        // 다 읽은 앞부분을 버립니다(남은 조각만 앞으로).
        if self.at > 0 {
            self.buf.drain(..self.at);
            self.at = 0;
        }
        Ok(out)
    }

    fn video(&mut self, d: &[u8], ts: u32) -> Result<Option<Event>, FlvError> {
        let Some(&b0) = d.first() else {
            return Ok(None);
        };
        let frame_type = (b0 >> 4) & 7;
        let (codec, packet, payload) = if b0 & EX_HEADER != 0 {
            let pkt = b0 & 0x0f;
            if d.len() < 5 {
                return Err(FlvError("확장 FLV 태그가 짧습니다"));
            }
            let codec = match &d[1..5] {
                b"av01" => VideoCodec::Av1,
                b"avc1" => VideoCodec::H264,
                b"hvc1" => VideoCodec::Hevc,
                _ => return Err(FlvError("모르는 영상 코덱입니다")),
            };
            match pkt {
                PKT_SEQUENCE_START => (codec, PKT_SEQUENCE_START, &d[5..]),
                // H.264·HEVC는 합성 시각(3바이트)이 붙습니다. B-프레임을 쓰지 않아 늘 0이라 버립니다.
                PKT_CODED_FRAMES if codec != VideoCodec::Av1 => {
                    if d.len() < 8 {
                        return Err(FlvError("확장 FLV 프레임이 짧습니다"));
                    }
                    (codec, PKT_CODED_FRAMES, &d[8..])
                }
                PKT_CODED_FRAMES | PKT_CODED_FRAMES_X => (codec, PKT_CODED_FRAMES, &d[5..]),
                // 시퀀스 끝·메타데이터(colorInfo) 등은 쓰지 않습니다.
                _ => return Ok(None),
            }
        } else {
            if b0 & 0x0f != LEGACY_AVC {
                return Ok(None);
            }
            if d.len() < 5 {
                return Err(FlvError("AVC 태그가 짧습니다"));
            }
            match d[1] {
                0 => (VideoCodec::H264, PKT_SEQUENCE_START, &d[5..]),
                1 => (VideoCodec::H264, PKT_CODED_FRAMES, &d[5..]),
                _ => return Ok(None),
            }
        };
        if packet == PKT_SEQUENCE_START {
            // libaom은 머리글 때 빈 레코드를 먼저 쓰고, 첫 패킷의 새 설정으로 진짜 레코드를 한 번 더 씁니다.
            if payload.is_empty() {
                return Ok(None);
            }
            if codec == VideoCodec::Av1 {
                self.av1_config = Some(payload.to_vec());
            }
            return Ok(Some(Event::Config {
                codec,
                record: payload.to_vec(),
            }));
        }
        if payload.is_empty() {
            return Ok(None);
        }
        let key = frame_type == FRAME_KEY;
        let mut data = payload.to_vec();
        if codec == VideoCodec::Av1
            && key
            && !has_sequence_header(&data)
            && let Some(cfg) = self.av1_config.as_ref().filter(|c| c.len() > 4)
        {
            // av1C의 configOBUs(4바이트 뒤)는 시퀀스 헤더 OBU입니다.
            let mut with = cfg[4..].to_vec();
            with.extend_from_slice(&data);
            data = with;
        }
        Ok(Some(Event::Frame {
            codec,
            key,
            ts_ms: ts,
            data,
        }))
    }
}

/// OBU 목록에 시퀀스 헤더가 있는지. 크기 칸이 없는 OBU를 만나면(마지막 OBU) 거기서 멈춥니다.
pub fn has_sequence_header(mut d: &[u8]) -> bool {
    while let Some(&h) = d.first() {
        let kind = (h >> 3) & 0x0f;
        if kind == OBU_SEQUENCE_HEADER {
            return true;
        }
        let ext = h & 0x04 != 0;
        let has_size = h & 0x02 != 0;
        let mut at = 1 + usize::from(ext);
        if !has_size {
            return false;
        }
        let Some((size, n)) = leb128(d.get(at..).unwrap_or(&[])) else {
            return false;
        };
        at += n;
        let Some(next) = at.checked_add(size).and_then(|end| d.get(end..)) else {
            return false;
        };
        d = next;
    }
    false
}

fn leb128(d: &[u8]) -> Option<(usize, usize)> {
    let mut v: u64 = 0;
    for (i, &b) in d.iter().take(8).enumerate() {
        v |= u64::from(b & 0x7f) << (7 * i);
        if b & 0x80 == 0 {
            return usize::try_from(v).ok().map(|v| (v, i + 1));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header() -> Vec<u8> {
        let mut h = b"FLV\x01\x01".to_vec();
        h.extend_from_slice(&9u32.to_be_bytes());
        h.extend_from_slice(&0u32.to_be_bytes());
        h
    }

    fn tag(kind: u8, ts: u32, body: &[u8]) -> Vec<u8> {
        let mut t = vec![kind];
        t.extend_from_slice(&(body.len() as u32).to_be_bytes()[1..]);
        t.extend_from_slice(&ts.to_be_bytes()[1..]);
        t.push((ts >> 24) as u8);
        t.extend_from_slice(&[0, 0, 0]);
        t.extend_from_slice(body);
        t.extend_from_slice(&((body.len() + 11) as u32).to_be_bytes());
        t
    }

    #[test]
    fn legacy_avc_config_and_frames() {
        let mut s = header();
        s.extend(tag(TAG_VIDEO, 0, &[0x17, 0, 0, 0, 0, 1, 0x64, 0, 0x28]));
        s.extend(tag(
            TAG_VIDEO,
            33,
            &[0x17, 1, 0, 0, 0, 0, 0, 0, 2, 0x65, 0x88],
        ));
        s.extend(tag(TAG_VIDEO, 66, &[0x27, 1, 0, 0, 0, 0, 0, 0, 1, 0x41]));
        // 메타데이터·소리 태그는 건너뜁니다.
        s.extend(tag(0x12, 0, &[2, 0, 1, b'x']));
        let mut r = FlvReader::default();
        let ev = r.push(&s).unwrap();
        assert_eq!(
            ev,
            vec![
                Event::Config {
                    codec: VideoCodec::H264,
                    record: vec![1, 0x64, 0, 0x28]
                },
                Event::Frame {
                    codec: VideoCodec::H264,
                    key: true,
                    ts_ms: 33,
                    data: vec![0, 0, 0, 2, 0x65, 0x88]
                },
                Event::Frame {
                    codec: VideoCodec::H264,
                    key: false,
                    ts_ms: 66,
                    data: vec![0, 0, 0, 1, 0x41]
                },
            ]
        );
    }

    #[test]
    fn split_reads_wait_for_whole_tags() {
        let mut s = header();
        s.extend(tag(TAG_VIDEO, 1, &[0x17, 1, 0, 0, 0, 9, 9]));
        s.extend(tag(TAG_VIDEO, 0x0100_0002, &[0x27, 1, 0, 0, 0, 8]));
        let mut r = FlvReader::default();
        let mut ev = Vec::new();
        for b in s.chunks(3) {
            ev.extend(r.push(b).unwrap());
        }
        assert_eq!(ev.len(), 2);
        assert!(
            matches!(&ev[0], Event::Frame { key: true, ts_ms: 1, data, .. } if data == &vec![9, 9])
        );
        // 시각 확장 바이트(위 8비트)도 읽습니다.
        assert!(matches!(
            &ev[1],
            Event::Frame {
                key: false,
                ts_ms: 0x0100_0002,
                ..
            }
        ));
    }

    #[test]
    fn enhanced_hevc_with_and_without_composition_time() {
        let mut s = header();
        s.extend(tag(TAG_VIDEO, 0, &[0x90, b'h', b'v', b'c', b'1', 1, 2, 3]));
        s.extend(tag(
            TAG_VIDEO,
            5,
            &[0x91, b'h', b'v', b'c', b'1', 0, 0, 0, 0xaa],
        ));
        s.extend(tag(TAG_VIDEO, 6, &[0xa3, b'h', b'v', b'c', b'1', 0xbb]));
        // colorInfo 메타데이터(패킷 4, 프레임 종류 5)는 버립니다.
        s.extend(tag(TAG_VIDEO, 0, &[0xd4, b'h', b'v', b'c', b'1', 2, 0]));
        let ev = FlvReader::default().push(&s).unwrap();
        assert_eq!(ev.len(), 3);
        assert_eq!(
            ev[0],
            Event::Config {
                codec: VideoCodec::Hevc,
                record: vec![1, 2, 3]
            }
        );
        assert!(
            matches!(&ev[1], Event::Frame { codec: VideoCodec::Hevc, key: true, data, .. } if data == &vec![0xaa])
        );
        assert!(matches!(&ev[2], Event::Frame { key: false, data, .. } if data == &vec![0xbb]));
    }

    #[test]
    fn av1_key_frame_gets_sequence_header() {
        // av1C: 4바이트 + 시퀀스 헤더 OBU(종류 1, 크기 2).
        let seq = [0x0a, 2, 0xde, 0xad];
        let mut av1c = vec![0x81, 0x08, 0x0c, 0x00];
        av1c.extend_from_slice(&seq);
        // 프레임 OBU(종류 6, 크기 1).
        let frame = [0x32, 1, 0x10];
        let mut s = header();
        // 빈 레코드(libaom의 머리글)는 건너뜁니다.
        s.extend(tag(TAG_VIDEO, 0, &[0x90, b'a', b'v', b'0', b'1']));
        let mut cfg = vec![0x90, b'a', b'v', b'0', b'1'];
        cfg.extend_from_slice(&av1c);
        s.extend(tag(TAG_VIDEO, 0, &cfg));
        let mut key = vec![0x91, b'a', b'v', b'0', b'1'];
        key.extend_from_slice(&frame);
        s.extend(tag(TAG_VIDEO, 0, &key));
        let mut delta = vec![0xa1, b'a', b'v', b'0', b'1'];
        delta.extend_from_slice(&frame);
        s.extend(tag(TAG_VIDEO, 16, &delta));
        // 이미 시퀀스 헤더가 있는 키 프레임은 그대로.
        let mut key2 = vec![0x91, b'a', b'v', b'0', b'1'];
        key2.extend_from_slice(&seq);
        key2.extend_from_slice(&frame);
        s.extend(tag(TAG_VIDEO, 33, &key2));
        let ev = FlvReader::default().push(&s).unwrap();
        let mut want_key = seq.to_vec();
        want_key.extend_from_slice(&frame);
        assert!(
            matches!(&ev[1], Event::Frame { codec: VideoCodec::Av1, key: true, data, .. } if *data == want_key)
        );
        assert!(matches!(&ev[2], Event::Frame { key: false, data, .. } if data == &frame.to_vec()));
        assert!(matches!(&ev[3], Event::Frame { key: true, data, .. } if *data == want_key));
    }

    #[test]
    fn sequence_header_search() {
        // 시간 구분자(종류 2, 크기 0) 뒤 시퀀스 헤더.
        assert!(has_sequence_header(&[0x12, 0, 0x0a, 1, 0]));
        assert!(!has_sequence_header(&[0x12, 0, 0x32, 1, 0]));
        // 크기 칸이 없는 OBU에서 멈춤.
        assert!(!has_sequence_header(&[0x30, 1, 2]));
        // 길이가 넘치면 거짓.
        assert!(!has_sequence_header(&[0x32, 0x7f]));
    }

    #[test]
    fn rejects_garbage() {
        assert!(FlvReader::default().push(b"NOTFLV0000000").is_err());
        let mut s = header();
        s.extend(tag(TAG_VIDEO, 0, &[0x90, b'x', b'x', b'x', b'x']));
        assert!(FlvReader::default().push(&s).is_err());
    }
}
