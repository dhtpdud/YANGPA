//! DAC(Descript Audio Codec, 24kHz): 24kHz 모노, 320샘플(13.3ms)마다 코드 32개(코드북 1024개, 약 24kbps).
//!
//! 실시간 스트리밍용 모델이 아니라 합성곱이 앞뒤를 함께 봅니다. 그래서 160ms(12프레임) 덩어리마다 앞 6프레임·뒤 4프레임을
//! 문맥으로 붙여 인코딩하고, 받는 쪽도 다음 덩어리 앞부분이 와야 지금 덩어리를 풀어 지연이 큽니다(약 0.4~0.5초).
//! 문맥을 붙이면 통째로 처리한 것과 거의 같은 소리가 납니다(상관 0.999, 실험). 디코더 출력은 입력보다 8샘플 앞에서
//! 시작합니다(출력 i = 입력 i+8).

use std::collections::VecDeque;
use std::sync::Arc;

use ort::session::Session;
use ort::value::Tensor;
use parking_lot::Mutex;

use super::{NeuralError, Result};

pub const SAMPLE_RATE: u32 = 24_000;
pub const HOP: usize = 320;
pub const CODEBOOKS: usize = 32;
pub const CODEBOOK_SIZE: u16 = 1024;
/// 한 덩어리의 코드 프레임 수(160ms).
pub const CHUNK: usize = 12;
/// 앞 문맥 프레임.
pub const LEFT: usize = 6;
/// 뒤 문맥 프레임(보내는 쪽은 이만큼 소리를 더 기다리고, 받는 쪽은 다음 덩어리를 기다림).
pub const RIGHT: usize = 4;
/// 디코더 출력이 입력보다 앞에서 잘리는 샘플 수.
const DECODER_SHIFT: usize = 8;

pub type CodeFrame = [u16; CODEBOOKS];

pub struct DacModels {
    pub encoder: Mutex<Session>,
    pub decoder: Mutex<Session>,
}

/// 보내는 쪽 창 나누기: 소리를 모아 [앞 문맥 | 덩어리 | 뒤 문맥] 창을 내고, 창의 덩어리 부분 코드만 씁니다.
#[derive(Default)]
pub struct EncWindows {
    buf: Vec<f32>,
    started: bool,
}

impl EncWindows {
    /// 소리를 넣고, 인코딩할 창들(길이 (LEFT+CHUNK+RIGHT)*HOP)을 돌려줍니다.
    pub fn push(&mut self, pcm: &[f32]) -> Vec<Vec<f32>> {
        if !self.started {
            // 첫 덩어리의 앞 문맥은 무음.
            self.buf.resize(LEFT * HOP, 0.0);
            self.started = true;
        }
        self.buf.extend_from_slice(pcm);
        let win = (LEFT + CHUNK + RIGHT) * HOP;
        let mut out = Vec::new();
        while self.buf.len() >= win {
            out.push(self.buf[..win].to_vec());
            self.buf.drain(..CHUNK * HOP);
        }
        out
    }
}

/// 받는 쪽 창 나누기: 덩어리를 하나 늦게 풉니다(다음 덩어리 앞부분이 뒤 문맥).
#[derive(Default)]
pub struct DecWindows {
    left: VecDeque<CodeFrame>,
    pending: Option<Vec<CodeFrame>>,
}

/// 풀 창 하나: 코드 프레임들과, 그 출력에서 쓸 구간(샘플).
#[derive(Debug, PartialEq)]
pub struct DecodeJob {
    pub frames: Vec<CodeFrame>,
    /// 출력에서 앞에 0을 더 붙일 샘플 수(앞 문맥이 디코더 밀림보다 짧을 때).
    pub pad_front: usize,
    pub start: usize,
    pub len: usize,
}

impl DecWindows {
    /// 덩어리 하나를 넣습니다. `lost`면(앞 덩어리가 빠짐) 기다리던 덩어리를 뒤 문맥 없이 먼저 풀고 문맥을 비웁니다.
    pub fn push(&mut self, chunk: Vec<CodeFrame>, lost: bool) -> Vec<DecodeJob> {
        let mut jobs = Vec::new();
        if lost {
            if let Some(p) = self.pending.take() {
                jobs.push(self.job(&p, &[]));
            }
            self.left.clear();
        }
        if let Some(p) = self.pending.take() {
            let right: Vec<CodeFrame> = chunk.iter().take(RIGHT).copied().collect();
            jobs.push(self.job(&p, &right));
            for f in p {
                self.left.push_back(f);
            }
            while self.left.len() > LEFT {
                self.left.pop_front();
            }
        }
        self.pending = Some(chunk);
        jobs
    }

    fn job(&self, chunk: &[CodeFrame], right: &[CodeFrame]) -> DecodeJob {
        let left_len = self.left.len();
        let mut frames: Vec<CodeFrame> = self.left.iter().copied().collect();
        frames.extend_from_slice(chunk);
        frames.extend_from_slice(right);
        // 출력 i = 입력 i+SHIFT: 덩어리 시작(입력 left_len*HOP)은 출력 left_len*HOP-SHIFT.
        let begin = left_len * HOP;
        let (pad_front, start) = if begin >= DECODER_SHIFT {
            (0, begin - DECODER_SHIFT)
        } else {
            (DECODER_SHIFT - begin, 0)
        };
        DecodeJob {
            frames,
            pad_front,
            start,
            len: chunk.len() * HOP,
        }
    }
}

pub struct DacEncoder {
    models: Arc<DacModels>,
    windows: EncWindows,
}

impl DacEncoder {
    pub fn new(models: Arc<DacModels>) -> Self {
        DacEncoder {
            models,
            windows: EncWindows::default(),
        }
    }

    /// 소리를 넣고 다 찬 덩어리마다 코드(CHUNK 프레임)를 돌려줍니다.
    pub fn push(&mut self, pcm: &[f32]) -> Result<Vec<Vec<CodeFrame>>> {
        let mut out = Vec::new();
        for win in self.windows.push(pcm) {
            let n = win.len();
            let mut session = self.models.encoder.lock();
            let mut outputs = session.run(vec![(
                "input_values",
                ort::session::SessionInputValue::from(Tensor::from_array(([1usize, 1, n], win))?),
            )])?;
            let codes = outputs
                .remove("audio_codes")
                .ok_or_else(|| NeuralError::model("audio_codes 출력이 없습니다"))?;
            let (shape, data) = codes.try_extract_tensor::<i64>()?;
            let frames = shape.get(2).copied().unwrap_or(0) as usize;
            if shape.get(1).copied() != Some(CODEBOOKS as i64) || frames < LEFT + CHUNK {
                return Err(NeuralError::model("DAC 코드 모양이 예상과 다릅니다"));
            }
            let mut chunk = Vec::with_capacity(CHUNK);
            for f in LEFT..LEFT + CHUNK {
                let mut frame = [0u16; CODEBOOKS];
                for (q, c) in frame.iter_mut().enumerate() {
                    *c = data[q * frames + f].clamp(0, i64::from(CODEBOOK_SIZE - 1)) as u16;
                }
                chunk.push(frame);
            }
            out.push(chunk);
        }
        Ok(out)
    }
}

pub struct DacDecoder {
    models: Arc<DacModels>,
    windows: DecWindows,
}

impl DacDecoder {
    pub fn new(models: Arc<DacModels>) -> Self {
        DacDecoder {
            models,
            windows: DecWindows::default(),
        }
    }

    /// 덩어리를 넣고 풀 수 있게 된 소리를 돌려줍니다(한 덩어리 늦음).
    pub fn push(&mut self, chunk: Vec<CodeFrame>, lost: bool) -> Result<Vec<f32>> {
        let mut out = Vec::new();
        for job in self.windows.push(chunk, lost) {
            let n = job.frames.len();
            let mut data = vec![0i64; CODEBOOKS * n];
            for (f, frame) in job.frames.iter().enumerate() {
                for (q, c) in frame.iter().enumerate() {
                    data[q * n + f] = i64::from(*c);
                }
            }
            let mut session = self.models.decoder.lock();
            let mut outputs = session.run(vec![(
                "audio_codes",
                ort::session::SessionInputValue::from(Tensor::from_array((
                    [1usize, CODEBOOKS, n],
                    data,
                ))?),
            )])?;
            let audio = outputs
                .remove("audio_values")
                .ok_or_else(|| NeuralError::model("audio_values 출력이 없습니다"))?;
            let (_, pcm) = audio.try_extract_tensor::<f32>()?;
            let mut seg = vec![0f32; job.pad_front];
            let end = (job.start + job.len - job.pad_front).min(pcm.len());
            seg.extend_from_slice(&pcm[job.start.min(end)..end]);
            seg.resize(job.len, 0.0);
            out.extend(seg);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(v: u16) -> CodeFrame {
        [v; CODEBOOKS]
    }

    #[test]
    fn encoder_windows_start_with_silence_and_slide_by_a_chunk() {
        let mut w = EncWindows::default();
        let win = (LEFT + CHUNK + RIGHT) * HOP;
        // 첫 창: 앞 문맥(무음) + 덩어리 + 뒤 문맥만큼 소리가 모여야 나옴.
        assert!(w.push(&vec![1.0; (CHUNK + RIGHT) * HOP - 1]).is_empty());
        let first = w.push(&[1.0]);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].len(), win);
        assert!(first[0][..LEFT * HOP].iter().all(|x| *x == 0.0));
        assert!(first[0][LEFT * HOP..].iter().all(|x| *x == 1.0));
        // 다음 창은 덩어리만큼 더 모이면.
        assert!(w.push(&vec![2.0; CHUNK * HOP - 1]).is_empty());
        let second = w.push(&[2.0]);
        assert_eq!(second.len(), 1);
        assert_eq!(second[0][(LEFT + CHUNK + RIGHT) * HOP - 1], 2.0);
    }

    #[test]
    fn decoder_waits_one_chunk_and_uses_left_and_right_context() {
        let mut w = DecWindows::default();
        let a: Vec<CodeFrame> = (0..CHUNK as u16).map(frame).collect();
        let b: Vec<CodeFrame> = (100..100 + CHUNK as u16).map(frame).collect();
        let c: Vec<CodeFrame> = (200..200 + CHUNK as u16).map(frame).collect();
        assert!(w.push(a.clone(), false).is_empty());
        let jobs = w.push(b.clone(), false);
        // 첫 덩어리: 앞 문맥 없음 → 디코더 밀림만큼 앞에 0을 붙임.
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].frames.len(), CHUNK + RIGHT);
        assert_eq!(jobs[0].frames[CHUNK], b[0]);
        assert_eq!(
            (jobs[0].pad_front, jobs[0].start, jobs[0].len),
            (DECODER_SHIFT, 0, CHUNK * HOP)
        );
        let jobs = w.push(c.clone(), false);
        // 둘째: 앞 문맥 LEFT 프레임(첫 덩어리 끝), 뒤 문맥 RIGHT 프레임(셋째 덩어리 앞).
        assert_eq!(jobs[0].frames.len(), LEFT + CHUNK + RIGHT);
        assert_eq!(jobs[0].frames[0], a[CHUNK - LEFT]);
        assert_eq!(jobs[0].frames[LEFT], b[0]);
        assert_eq!(jobs[0].frames[LEFT + CHUNK], c[0]);
        assert_eq!(
            (jobs[0].pad_front, jobs[0].start),
            (0, LEFT * HOP - DECODER_SHIFT)
        );
    }

    #[test]
    fn decoder_flushes_the_waiting_chunk_when_one_was_lost() {
        let mut w = DecWindows::default();
        let a: Vec<CodeFrame> = (0..CHUNK as u16).map(frame).collect();
        let c: Vec<CodeFrame> = (200..200 + CHUNK as u16).map(frame).collect();
        w.push(a.clone(), false);
        let jobs = w.push(c, true);
        // 기다리던 덩어리를 뒤 문맥 없이 풀고, 앞 문맥은 비웁니다.
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].frames, a);
        assert!(w.left.is_empty());
    }
}
