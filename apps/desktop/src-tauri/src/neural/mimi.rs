//! Mimi(Kyutai) 스트리밍 코덱: 24kHz 모노, 80ms(1920샘플)마다 코드 8개(코드북 2048개, 약 1.1kbps).
//!
//! 모델은 합성곱 상태와 트랜스포머 KV 캐시를 입출력으로 드러낸 스트리밍 ONNX(`streaming-8cb`)입니다. 스트림마다 상태를
//! 들고 프레임 하나씩 넣습니다. KV 캐시는 프레임마다 길어지므로 모델의 주의 창(250프레임, 20초) 안으로 잘라 메모리를
//! 묶습니다(한 시간 통화면 수 GB가 됨).

use std::sync::Arc;

use ort::session::Session;
use ort::value::{DynValue, Tensor};
use parking_lot::Mutex;

use super::{NeuralError, Result};

pub const SAMPLE_RATE: u32 = 24_000;
/// 코드 한 프레임의 소리 길이(샘플).
pub const FRAME_SAMPLES: usize = 1920;
pub const CODEBOOKS: usize = 8;
pub const CODEBOOK_SIZE: u16 = 2048;
/// KV 캐시를 이 길이로 자릅니다(모델의 sliding_window).
const KV_KEEP: usize = 250;
/// 이만큼 넘치면 자릅니다(프레임마다 복사하지 않게).
const KV_SLACK: usize = 50;
/// 이보다 짧게 빠진 프레임은 마지막 코드를 되풀이해 메우고, 더 길면 상태를 처음부터 다시 시작합니다.
pub const MAX_CONCEAL: u32 = 3;

pub struct MimiModels {
    pub encoder: Mutex<Session>,
    pub decoder: Mutex<Session>,
}

/// 세션 입력 중 상태(이름 `past_*`)의 처음 값.
fn initial_state(session: &Session) -> Result<Vec<(String, DynValue)>> {
    let mut out = Vec::new();
    for input in session.inputs() {
        let name = input.name();
        if let Some(rest) = name.strip_prefix("past_conv_") {
            let shape = input
                .dtype()
                .tensor_shape()
                .ok_or_else(|| NeuralError::model(format!("{rest}: 텐서가 아닌 상태")))?;
            let dims: Vec<usize> = shape.iter().map(|d| (*d).max(1) as usize).collect();
            let len = dims.iter().product();
            out.push((
                name.to_string(),
                Tensor::from_array((dims, vec![0f32; len]))?.into_dyn(),
            ));
        } else if name.starts_with("past_key_") || name.starts_with("past_value_") {
            out.push((
                name.to_string(),
                Tensor::from_array(([1usize, 8, 0, 64], Vec::<f32>::new()))?.into_dyn(),
            ));
        }
    }
    Ok(out)
}

/// 출력 `present_*`를 다음 입력 `past_*`로 옮깁니다. KV 캐시는 길면 뒤쪽만 남깁니다.
fn carry(
    outputs: &mut ort::session::SessionOutputs<'_>,
    state: &mut [(String, DynValue)],
) -> Result<()> {
    for (name, value) in state.iter_mut() {
        let present = format!("present_{}", &name["past_".len()..]);
        let Some(v) = outputs.remove(&present) else {
            return Err(NeuralError::model(format!("출력 {present}가 없습니다")));
        };
        *value = if name.starts_with("past_key_") || name.starts_with("past_value_") {
            trim_kv(v)?
        } else {
            v
        };
    }
    Ok(())
}

fn trim_kv(v: DynValue) -> Result<DynValue> {
    let (shape, data) = v.try_extract_tensor::<f32>()?;
    let dims: Vec<usize> = shape.iter().map(|d| *d as usize).collect();
    let [b, h, len, d] = dims[..] else {
        return Err(NeuralError::model("KV 캐시 모양이 [1,8,n,64]가 아닙니다"));
    };
    if len <= KV_KEEP + KV_SLACK {
        return Ok(v);
    }
    let keep = KV_KEEP;
    let mut out = Vec::with_capacity(b * h * keep * d);
    for head in 0..b * h {
        let start = head * len * d + (len - keep) * d;
        out.extend_from_slice(&data[start..start + keep * d]);
    }
    Ok(Tensor::from_array(([b, h, keep, d], out))?.into_dyn())
}

/// 보내는 쪽: 소리 → 코드.
pub struct MimiEncoder {
    models: Arc<MimiModels>,
    state: Vec<(String, DynValue)>,
    pending: Vec<f32>,
}

impl MimiEncoder {
    pub fn new(models: Arc<MimiModels>) -> Result<Self> {
        let state = initial_state(&models.encoder.lock())?;
        Ok(MimiEncoder {
            models,
            state,
            pending: Vec::new(),
        })
    }

    /// 소리를 넣고, 다 찬 프레임마다 코드 8개를 돌려줍니다(프레임 순, 프레임 안은 코드북 순).
    pub fn push(&mut self, pcm: &[f32]) -> Result<Vec<[u16; CODEBOOKS]>> {
        self.pending.extend_from_slice(pcm);
        let mut out = Vec::new();
        while self.pending.len() >= FRAME_SAMPLES {
            let frame: Vec<f32> = self.pending.drain(..FRAME_SAMPLES).collect();
            out.push(self.encode_frame(frame)?);
        }
        Ok(out)
    }

    fn encode_frame(&mut self, frame: Vec<f32>) -> Result<[u16; CODEBOOKS]> {
        let mut session = self.models.encoder.lock();
        let mut inputs: Vec<(String, ort::session::SessionInputValue<'_>)> = vec![(
            "input_values".into(),
            Tensor::from_array(([1usize, 1, FRAME_SAMPLES], frame))?.into(),
        )];
        for (name, value) in &self.state {
            inputs.push((name.clone(), value.view().into()));
        }
        let mut outputs = session.run(inputs)?;
        let codes = outputs
            .remove("audio_codes")
            .ok_or_else(|| NeuralError::model("audio_codes 출력이 없습니다"))?;
        let (_, data) = codes.try_extract_tensor::<i64>()?;
        if data.len() < CODEBOOKS {
            return Err(NeuralError::model("코드 수가 모자랍니다"));
        }
        let mut frame_codes = [0u16; CODEBOOKS];
        for (i, c) in frame_codes.iter_mut().enumerate() {
            *c = data[i].clamp(0, i64::from(CODEBOOK_SIZE - 1)) as u16;
        }
        carry(&mut outputs, &mut self.state)?;
        Ok(frame_codes)
    }
}

/// 받는 쪽: 코드 → 소리. 보낸 기기마다 하나.
pub struct MimiDecoder {
    models: Arc<MimiModels>,
    state: Vec<(String, DynValue)>,
    last: Option<[u16; CODEBOOKS]>,
}

impl MimiDecoder {
    pub fn new(models: Arc<MimiModels>) -> Result<Self> {
        let state = initial_state(&models.decoder.lock())?;
        Ok(MimiDecoder {
            models,
            state,
            last: None,
        })
    }

    pub fn reset(&mut self) -> Result<()> {
        self.state = initial_state(&self.models.decoder.lock())?;
        self.last = None;
        Ok(())
    }

    /// 프레임 하나를 소리(1920샘플)로. `lost`는 이 프레임 앞에서 빠진 프레임 수: 짧으면 마지막 코드로 메우고,
    /// 길면 상태를 새로 시작합니다.
    pub fn push(&mut self, codes: [u16; CODEBOOKS], lost: u32) -> Result<Vec<f32>> {
        let mut out = Vec::new();
        if lost > MAX_CONCEAL {
            self.reset()?;
        } else if let Some(last) = self.last {
            for _ in 0..lost {
                out.extend(self.decode_frame(last)?);
            }
        }
        out.extend(self.decode_frame(codes)?);
        self.last = Some(codes);
        Ok(out)
    }

    fn decode_frame(&mut self, codes: [u16; CODEBOOKS]) -> Result<Vec<f32>> {
        let mut session = self.models.decoder.lock();
        let data: Vec<i64> = codes.iter().map(|c| i64::from(*c)).collect();
        let mut inputs: Vec<(String, ort::session::SessionInputValue<'_>)> = vec![(
            "audio_codes".into(),
            Tensor::from_array(([1usize, CODEBOOKS, 1], data))?.into(),
        )];
        for (name, value) in &self.state {
            inputs.push((name.clone(), value.view().into()));
        }
        let mut outputs = session.run(inputs)?;
        let audio = outputs
            .remove("audio_values")
            .ok_or_else(|| NeuralError::model("audio_values 출력이 없습니다"))?;
        let (_, pcm) = audio.try_extract_tensor::<f32>()?;
        let pcm = pcm.to_vec();
        carry(&mut outputs, &mut self.state)?;
        Ok(pcm)
    }
}
