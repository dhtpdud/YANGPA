//! 신경망 음성 코덱(Mimi·DAC). 통화 음성을 Opus 대신 신경망 코덱으로 보낼 때 웹뷰가 이 명령들로 인코딩·디코딩합니다
//! (코드는 웹뷰가 프레임 암호화한 뒤 WebRTC 데이터 채널로 나름, apps/desktop/src/voice/neural.ts).
//!
//! 모델과 ONNX Runtime은 실행 파일·설치 파일에 넣지 않고 모델 폴더에서 엽니다([`models_dir`]):
//! `onnxruntime/onnxruntime.dll`(리눅스 `libonnxruntime.so`, macOS `libonnxruntime.dylib`),
//! `mimi/encoder_model.onnx`·`decoder_model.onnx`, `dac/encoder_model.onnx`·`decoder_model.onnx`.
//! 런타임은 처음 쓸 때 한 번 열고, 모델은 코덱마다 처음 쓸 때 읽어 모든 스트림이 함께 씁니다(세션 실행은 잠금으로 하나씩).
//!
//! 웹뷰 명령: `neural_status`(JSON), `neural_encode`·`neural_decode`(바이트 본문·바이트 응답, 머리글 `x-stream`·`x-codec`,
//! 디코드는 `x-lost`), `neural_close`. 소리는 24kHz 모노 f32(LE), 코드는 u16(LE)로 프레임 순.

pub mod dac;
pub mod mimi;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ort::environment::Environment;
use ort::session::Session;
use ort::session::builder::GraphOptimizationLevel;
use parking_lot::Mutex;
use serde::Serialize;
use tauri::State;
use tauri::ipc::{InvokeBody, Request, Response};

use crate::error::CmdError;

#[derive(Debug)]
pub struct NeuralError(String);

impl NeuralError {
    pub fn model(msg: impl Into<String>) -> Self {
        NeuralError(msg.into())
    }
}

impl std::fmt::Display for NeuralError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl<R> From<ort::Error<R>> for NeuralError {
    fn from(e: ort::Error<R>) -> Self {
        NeuralError(format!("ONNX Runtime: {e}"))
    }
}

impl From<NeuralError> for CmdError {
    fn from(e: NeuralError) -> Self {
        CmdError::internal(e.0)
    }
}

pub type Result<T> = std::result::Result<T, NeuralError>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Codec {
    Mimi,
    Dac,
}

impl Codec {
    pub const ALL: [Codec; 2] = [Codec::Mimi, Codec::Dac];

    fn dir(self) -> &'static str {
        match self {
            Codec::Mimi => "mimi",
            Codec::Dac => "dac",
        }
    }

    fn parse(s: &str) -> std::result::Result<Self, CmdError> {
        match s {
            "mimi" => Ok(Codec::Mimi),
            "dac" => Ok(Codec::Dac),
            _ => Err(CmdError::invalid_params("모르는 코덱입니다")),
        }
    }
}

pub const MODEL_FILES: [&str; 2] = ["encoder_model.onnx", "decoder_model.onnx"];

/// 배포 모델 묶음의 파일 목록(모델 폴더 안 경로와 sha256). 빌드 스크립트와 같은 파일을 씁니다.
const PINNED: &str = include_str!("../../../../../scripts/neural-models.txt");

/// 모델 폴더 안 경로(`/` 구분)의 고정된 sha256. 목록에 없으면 `None`(윈도 밖의 런타임 라이브러리 등 개발용).
fn pinned_sha256(rel: &str) -> Option<&'static str> {
    crate::pinned::sha256_of(PINNED, rel)
}

/// 파일이 배포본과 같은지 봅니다. 모델 폴더는 사용자가 쓸 수 있는 곳이라, 바뀐 `onnxruntime.dll`을 열면 그 코드가 키를 가진
/// 앱 프로세스 안에서 돕니다. 바뀌었거나 손상된 모델도 열지 않고 상태에 드러내 Opus로 돌아가게 합니다.
fn verify(dir: &Path, rel: &str) -> Result<()> {
    let Some(want) = pinned_sha256(rel) else {
        if cfg!(windows) {
            return Err(NeuralError::model(format!(
                "배포 목록에 없는 파일입니다: {rel}"
            )));
        }
        return Ok(());
    };
    let got = crate::pinned::sha256_cached(&dir.join(rel))
        .map_err(|e| NeuralError::model(format!("{rel}을(를) 읽지 못했습니다: {e}")))?;
    if got == want {
        Ok(())
    } else {
        Err(NeuralError::model(format!(
            "{rel}이(가) 배포본과 다릅니다(바뀌었거나 손상됨). 모델 묶음을 다시 풀어 주세요"
        )))
    }
}

/// ONNX Runtime 라이브러리 파일 이름(모델 폴더의 `onnxruntime/` 아래).
pub fn runtime_file() -> &'static str {
    if cfg!(windows) {
        "onnxruntime.dll"
    } else if cfg!(target_os = "macos") {
        "libonnxruntime.dylib"
    } else {
        "libonnxruntime.so"
    }
}

/// 모델 폴더: `YANGPA_MODELS_DIR` → 실행 파일 옆 `models`(포터블, 있을 때만) → 데이터 경로 옆 `models`
/// (윈도 `%APPDATA%\yangpa\models`).
pub fn models_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("YANGPA_MODELS_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(d);
    }
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|p| p.join("models")))
        && dir.is_dir()
    {
        return dir;
    }
    match yangpa_rpc::endpoint::data_dir() {
        Ok(data) => data
            .parent()
            .map(|p| p.join("models"))
            .unwrap_or_else(|| data.join("models")),
        Err(_) => PathBuf::from("models"),
    }
}

/// 이 코덱에 빠졌거나 배포본과 다른 파일(모델 폴더 기준 상대 경로, 다른 것은 뒤에 표시). 비었으면 쓸 수 있습니다.
pub fn missing_files(dir: &Path, codec: Codec) -> Vec<String> {
    let rt = format!("onnxruntime/{}", runtime_file());
    let models = MODEL_FILES.map(|f| format!("{}/{f}", codec.dir()));
    std::iter::once(rt)
        .chain(models)
        .filter_map(|rel| {
            if !dir.join(&rel).is_file() {
                Some(rel)
            } else if verify(dir, &rel).is_err() {
                Some(format!("{rel}(배포본과 다름)"))
            } else {
                None
            }
        })
        .collect()
}

enum Stream {
    MimiEnc(mimi::MimiEncoder),
    MimiDec(mimi::MimiDecoder),
    DacEnc(dac::DacEncoder),
    DacDec(dac::DacDecoder),
}

#[derive(Default)]
pub struct Neural {
    runtime: Mutex<Option<Arc<Environment>>>,
    mimi: Mutex<Option<Arc<mimi::MimiModels>>>,
    dac: Mutex<Option<Arc<dac::DacModels>>>,
    streams: Mutex<HashMap<String, Arc<Mutex<Stream>>>>,
}

/// 세션 하나가 쓰는 스레드(통화 상대가 여럿이면 세션 실행이 겹치지 않으므로 적게).
const INTRA_THREADS: usize = 2;
/// 동시에 열어 둘 스트림 수 상한(내 인코더 + 상대마다 디코더).
const MAX_STREAMS: usize = 32;

impl Neural {
    fn runtime(&self, dir: &Path) -> Result<Arc<Environment>> {
        let mut g = self.runtime.lock();
        if let Some(env) = g.as_ref() {
            return Ok(env.clone());
        }
        let lib = dir.join("onnxruntime").join(runtime_file());
        if !lib.is_file() {
            return Err(NeuralError::model(format!(
                "모델 폴더에 ONNX Runtime({})이 없습니다",
                runtime_file()
            )));
        }
        verify(dir, &format!("onnxruntime/{}", runtime_file()))?;
        let env = ort::init_from(&lib)
            .map_err(|e| NeuralError::model(format!("ONNX Runtime을 열지 못했습니다: {e}")))?
            .with_name("yangpa-neural")
            .build()?;
        let env = Arc::new(env);
        *g = Some(env.clone());
        Ok(env)
    }

    fn load(env: &Environment, dir: &Path, codec: Codec, file: &str) -> Result<Session> {
        let rel = format!("{}/{file}", codec.dir());
        let path = dir.join(codec.dir()).join(file);
        if !path.is_file() {
            return Err(NeuralError::model(format!(
                "모델 파일이 없습니다: {}",
                path.display()
            )));
        }
        verify(dir, &rel)?;
        Ok(Session::builder(env)?
            .with_intra_threads(INTRA_THREADS)?
            .with_optimization_level(GraphOptimizationLevel::All)?
            .commit_from_file(path)?)
    }

    fn mimi(&self) -> Result<Arc<mimi::MimiModels>> {
        let mut g = self.mimi.lock();
        if let Some(m) = g.as_ref() {
            return Ok(m.clone());
        }
        let dir = models_dir();
        let env = self.runtime(&dir)?;
        let m = Arc::new(mimi::MimiModels {
            encoder: Mutex::new(Self::load(&env, &dir, Codec::Mimi, MODEL_FILES[0])?),
            decoder: Mutex::new(Self::load(&env, &dir, Codec::Mimi, MODEL_FILES[1])?),
        });
        *g = Some(m.clone());
        Ok(m)
    }

    fn dac(&self) -> Result<Arc<dac::DacModels>> {
        let mut g = self.dac.lock();
        if let Some(m) = g.as_ref() {
            return Ok(m.clone());
        }
        let dir = models_dir();
        let env = self.runtime(&dir)?;
        let m = Arc::new(dac::DacModels {
            encoder: Mutex::new(Self::load(&env, &dir, Codec::Dac, MODEL_FILES[0])?),
            decoder: Mutex::new(Self::load(&env, &dir, Codec::Dac, MODEL_FILES[1])?),
        });
        *g = Some(m.clone());
        Ok(m)
    }

    fn stream(
        &self,
        id: &str,
        make: impl FnOnce() -> Result<Stream>,
    ) -> Result<Arc<Mutex<Stream>>> {
        let mut g = self.streams.lock();
        if let Some(s) = g.get(id) {
            return Ok(s.clone());
        }
        if g.len() >= MAX_STREAMS {
            return Err(NeuralError::model("신경망 코덱 스트림이 너무 많습니다"));
        }
        let s = Arc::new(Mutex::new(make()?));
        g.insert(id.to_string(), s.clone());
        Ok(s)
    }

    /// 소리 → 코드(다 찬 프레임·덩어리만큼, 프레임 순 u16).
    pub fn encode(&self, id: &str, codec: Codec, pcm: &[f32]) -> Result<Vec<u16>> {
        let s = match codec {
            Codec::Mimi => {
                let m = self.mimi()?;
                self.stream(id, || Ok(Stream::MimiEnc(mimi::MimiEncoder::new(m)?)))?
            }
            Codec::Dac => {
                let m = self.dac()?;
                self.stream(id, || Ok(Stream::DacEnc(dac::DacEncoder::new(m))))?
            }
        };
        let mut s = s.lock();
        let mut out = Vec::new();
        match &mut *s {
            Stream::MimiEnc(e) => e.push(pcm)?.into_iter().for_each(|f| out.extend(f)),
            Stream::DacEnc(e) => e
                .push(pcm)?
                .into_iter()
                .flatten()
                .for_each(|f| out.extend(f)),
            _ => return Err(NeuralError::model("인코더가 아닌 스트림입니다")),
        }
        Ok(out)
    }

    /// 패킷 하나(Mimi 프레임 하나·DAC 덩어리 하나)의 코드 → 소리. `lost`는 이 패킷 앞에서 빠진 패킷 수.
    pub fn decode(&self, id: &str, codec: Codec, codes: &[u16], lost: u32) -> Result<Vec<f32>> {
        let s = match codec {
            Codec::Mimi => {
                let m = self.mimi()?;
                self.stream(id, || Ok(Stream::MimiDec(mimi::MimiDecoder::new(m)?)))?
            }
            Codec::Dac => {
                let m = self.dac()?;
                self.stream(id, || Ok(Stream::DacDec(dac::DacDecoder::new(m))))?
            }
        };
        let mut s = s.lock();
        match &mut *s {
            Stream::MimiDec(d) => {
                let frame: [u16; mimi::CODEBOOKS] = codes
                    .try_into()
                    .map_err(|_| NeuralError::model("Mimi 프레임 크기가 다릅니다"))?;
                d.push(frame, lost)
            }
            Stream::DacDec(d) => {
                if codes.len() != dac::CHUNK * dac::CODEBOOKS {
                    return Err(NeuralError::model("DAC 덩어리 크기가 다릅니다"));
                }
                let chunk = codes.as_chunks::<{ dac::CODEBOOKS }>().0.to_vec();
                d.push(chunk, lost > 0)
            }
            _ => Err(NeuralError::model("디코더가 아닌 스트림입니다")),
        }
    }

    pub fn close(&self, id: &str) {
        self.streams.lock().remove(id);
    }
}

#[derive(Serialize)]
pub struct CodecStatus {
    codec: Codec,
    /// 모델 폴더 기준으로 빠졌거나 배포본과 다른 파일. 비었으면 쓸 수 있음.
    missing: Vec<String>,
}

#[derive(Serialize)]
pub struct NeuralStatus {
    dir: String,
    codecs: Vec<CodecStatus>,
}

/// 처음 확인할 때는 모델 파일을 해시하므로(수백 MB) 화면 스레드 밖에서 돕니다.
#[tauri::command]
pub async fn neural_status() -> std::result::Result<NeuralStatus, CmdError> {
    tauri::async_runtime::spawn_blocking(status)
        .await
        .map_err(|_| CmdError::internal("모델 폴더 확인이 멈췄습니다"))
}

fn status() -> NeuralStatus {
    let dir = models_dir();
    NeuralStatus {
        codecs: Codec::ALL
            .iter()
            .map(|c| CodecStatus {
                codec: *c,
                missing: missing_files(&dir, *c),
            })
            .collect(),
        dir: dir.to_string_lossy().into_owned(),
    }
}

fn header<'a>(req: &'a Request<'_>, name: &str) -> std::result::Result<&'a str, CmdError> {
    req.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| CmdError::invalid_params(format!("{name} 머리글이 없습니다")))
}

fn raw_body<'a>(req: &'a Request<'_>) -> std::result::Result<&'a [u8], CmdError> {
    match req.body() {
        InvokeBody::Raw(b) => Ok(b),
        InvokeBody::Json(_) => Err(CmdError::invalid_params("바이트 본문이 필요합니다")),
    }
}

/// 스트림 ID: 웹뷰가 정한 짧은 이름(통화·보낸 기기). 로그·파일에 남기지 않습니다.
fn stream_id(req: &Request<'_>) -> std::result::Result<String, CmdError> {
    let id = header(req, "x-stream")?;
    if id.is_empty() || id.len() > 160 {
        return Err(CmdError::invalid_params("스트림 이름이 잘못됐습니다"));
    }
    Ok(id.to_string())
}

/// 웹뷰 → f32 소리. 4바이트 단위가 아니면 거절.
fn pcm_from(bytes: &[u8]) -> std::result::Result<Vec<f32>, CmdError> {
    if !bytes.len().is_multiple_of(4) || bytes.len() > 4 * 24_000 {
        return Err(CmdError::invalid_params("소리 길이가 잘못됐습니다"));
    }
    Ok(bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| f32::from_le_bytes(*b))
        .collect())
}

fn codes_from(bytes: &[u8]) -> std::result::Result<Vec<u16>, CmdError> {
    if !bytes.len().is_multiple_of(2) || bytes.len() > 4096 {
        return Err(CmdError::invalid_params("코드 길이가 잘못됐습니다"));
    }
    Ok(bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| u16::from_le_bytes(*b))
        .collect())
}

#[tauri::command]
pub async fn neural_encode(
    neural: State<'_, Arc<Neural>>,
    request: Request<'_>,
) -> std::result::Result<Response, CmdError> {
    let id = stream_id(&request)?;
    let codec = Codec::parse(header(&request, "x-codec")?)?;
    let pcm = pcm_from(raw_body(&request)?)?;
    let neural = neural.inner().clone();
    let codes = tauri::async_runtime::spawn_blocking(move || neural.encode(&id, codec, &pcm))
        .await
        .map_err(|_| CmdError::internal("인코딩 작업이 멈췄습니다"))??;
    Ok(Response::new(
        codes
            .iter()
            .flat_map(|c| c.to_le_bytes())
            .collect::<Vec<u8>>(),
    ))
}

#[tauri::command]
pub async fn neural_decode(
    neural: State<'_, Arc<Neural>>,
    request: Request<'_>,
) -> std::result::Result<Response, CmdError> {
    let id = stream_id(&request)?;
    let codec = Codec::parse(header(&request, "x-codec")?)?;
    let lost = header(&request, "x-lost")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(0);
    let codes = codes_from(raw_body(&request)?)?;
    let neural = neural.inner().clone();
    let pcm = tauri::async_runtime::spawn_blocking(move || neural.decode(&id, codec, &codes, lost))
        .await
        .map_err(|_| CmdError::internal("디코딩 작업이 멈췄습니다"))??;
    Ok(Response::new(
        pcm.iter()
            .flat_map(|s| s.to_le_bytes())
            .collect::<Vec<u8>>(),
    ))
}

#[tauri::command]
pub fn neural_close(neural: State<'_, Arc<Neural>>, stream: String) {
    neural.close(&stream);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_list_covers_models_and_windows_runtime() {
        for codec in Codec::ALL {
            for f in MODEL_FILES {
                let h = pinned_sha256(&format!("{}/{f}", codec.dir())).unwrap();
                assert_eq!(h.len(), 64);
            }
        }
        assert!(pinned_sha256("onnxruntime/onnxruntime.dll").is_some());
        assert!(pinned_sha256("mimi/other.onnx").is_none());
    }

    #[test]
    fn verify_rejects_changed_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("mimi")).unwrap();
        std::fs::write(dir.path().join("mimi/encoder_model.onnx"), b"not the model").unwrap();
        let e = verify(dir.path(), "mimi/encoder_model.onnx").unwrap_err();
        assert!(e.to_string().contains("배포본과 다릅니다"), "{e}");
    }

    #[test]
    fn missing_files_lists_runtime_and_models() {
        let dir = tempfile::tempdir().unwrap();
        let rt = format!("onnxruntime/{}", runtime_file());
        assert_eq!(
            missing_files(dir.path(), Codec::Mimi),
            vec![
                rt.clone(),
                "mimi/encoder_model.onnx".into(),
                "mimi/decoder_model.onnx".into()
            ]
        );
        std::fs::create_dir_all(dir.path().join("onnxruntime")).unwrap();
        std::fs::write(dir.path().join("onnxruntime").join(runtime_file()), b"x").unwrap();
        std::fs::create_dir_all(dir.path().join("dac")).unwrap();
        for f in MODEL_FILES {
            std::fs::write(dir.path().join("dac").join(f), b"x").unwrap();
        }
        // 있어도 배포본과 다르면 쓸 수 없는 파일로 드러냅니다(윈도에서는 런타임도 확인).
        let dac = missing_files(dir.path(), Codec::Dac);
        assert!(dac.contains(&"dac/encoder_model.onnx(배포본과 다름)".to_string()));
        assert!(dac.contains(&"dac/decoder_model.onnx(배포본과 다름)".to_string()));
        assert_eq!(dac.len(), if cfg!(windows) { 3 } else { 2 });
        assert_eq!(
            missing_files(dir.path(), Codec::Mimi).len(),
            if cfg!(windows) { 3 } else { 2 }
        );
    }

    #[test]
    fn raw_payloads_are_checked() {
        assert_eq!(pcm_from(&1.5f32.to_le_bytes()).unwrap(), vec![1.5]);
        assert!(pcm_from(&[0; 3]).is_err());
        assert_eq!(codes_from(&[1, 0, 2, 1]).unwrap(), vec![1, 258]);
        assert!(codes_from(&[0; 5000]).is_err());
    }

    /// 모델 폴더(`YANGPA_MODELS_DIR`)가 있을 때만: 실제 모델로 인코딩·디코딩해 소리가 되돌아오는지.
    #[test]
    fn roundtrip_with_real_models_when_available() {
        let Some(dir) = std::env::var_os("YANGPA_MODELS_DIR") else {
            return;
        };
        let dir = PathBuf::from(dir);
        let n = Neural::default();
        let sr = 24_000f32;
        let pcm: Vec<f32> = (0..sr as usize * 2)
            .map(|i| 0.2 * (2.0 * std::f32::consts::PI * 220.0 * i as f32 / sr).sin())
            .collect();
        for codec in Codec::ALL {
            if !missing_files(&dir, codec).is_empty() {
                continue;
            }
            let codes = n.encode("enc", codec, &pcm).unwrap();
            let per = match codec {
                Codec::Mimi => mimi::CODEBOOKS,
                Codec::Dac => dac::CHUNK * dac::CODEBOOKS,
            };
            assert!(!codes.is_empty() && codes.len() % per == 0, "{codec:?}");
            let mut out = Vec::new();
            for p in codes.chunks_exact(per) {
                out.extend(n.decode("dec", codec, p, 0).unwrap());
            }
            // 지연(DAC는 덩어리 하나 늦음)을 찾아 원래 소리와 맞춰 봅니다.
            let mut best = 0f32;
            for lag in (0..12_000usize).step_by(2) {
                let (mut xy, mut xx, mut yy) = (0f32, 0f32, 0f32);
                for i in 12_000..24_000 {
                    let (Some(x), Some(y)) = (pcm.get(i), out.get(i + lag)) else {
                        break;
                    };
                    xy += x * y;
                    xx += x * x;
                    yy += y * y;
                }
                if xx > 0.0 && yy > 0.0 {
                    best = best.max(xy / (xx * yy).sqrt());
                }
            }
            assert!(
                best > 0.9,
                "{codec:?} 되돌린 소리가 원래와 다름: 상관 {best}"
            );
            n.close("enc");
            n.close("dec");
        }
    }
}
