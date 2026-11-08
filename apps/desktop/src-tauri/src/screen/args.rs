//! 화면공유 인코더 설정 → ffmpeg 인자. 웹뷰가 보낸 값은 모두 열거형·숫자라 그대로 인자에 넣을 문자열이 없습니다(범위는
//! 여기서 다시 맞춤).
//!
//! 캡처는 `gfxcapture`(Windows.Graphics.Capture, GPU 텍스처)로 창·모니터 핸들을 받아 그 자리에서 출력 크기로 줄이고,
//! `fps` 필터로 프레임을 고르게 맞춥니다(화면이 멈춰 있어도 프레임이 나와 늦게 들어온 시청자가 다음 키 프레임을 받음).
//! NVENC는 GPU 텍스처를 그대로, 소프트웨어 인코더(AOM·SVT-AV1)는 내려받아 BT.709 4:2:0으로 바꿔 넣습니다.
//! 지연을 늘리는 B-프레임·미리보기 큐는 쓰지 않고, 키 프레임 간격은 꼭 정합니다(끼어든 시청자·손실 뒤 회복).

use serde::Deserialize;

use super::flv::VideoCodec;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
pub enum Encoder {
    #[serde(rename = "aom-av1")]
    AomAv1,
    #[serde(rename = "svt-av1")]
    SvtAv1,
    #[serde(rename = "nvenc-av1")]
    NvencAv1,
    #[serde(rename = "nvenc-h264")]
    NvencH264,
    #[serde(rename = "nvenc-hevc")]
    NvencHevc,
}

impl Encoder {
    pub const ALL: [Encoder; 5] = [
        Encoder::AomAv1,
        Encoder::SvtAv1,
        Encoder::NvencAv1,
        Encoder::NvencH264,
        Encoder::NvencHevc,
    ];

    pub fn id(self) -> &'static str {
        match self {
            Encoder::AomAv1 => "aom-av1",
            Encoder::SvtAv1 => "svt-av1",
            Encoder::NvencAv1 => "nvenc-av1",
            Encoder::NvencH264 => "nvenc-h264",
            Encoder::NvencHevc => "nvenc-hevc",
        }
    }

    /// ffmpeg 인코더 이름.
    pub fn ffmpeg(self) -> &'static str {
        match self {
            Encoder::AomAv1 => "libaom-av1",
            Encoder::SvtAv1 => "libsvtav1",
            Encoder::NvencAv1 => "av1_nvenc",
            Encoder::NvencH264 => "h264_nvenc",
            Encoder::NvencHevc => "hevc_nvenc",
        }
    }

    pub fn codec(self) -> VideoCodec {
        match self {
            Encoder::AomAv1 | Encoder::SvtAv1 | Encoder::NvencAv1 => VideoCodec::Av1,
            Encoder::NvencH264 => VideoCodec::H264,
            Encoder::NvencHevc => VideoCodec::Hevc,
        }
    }

    pub fn is_nvenc(self) -> bool {
        matches!(
            self,
            Encoder::NvencAv1 | Encoder::NvencH264 | Encoder::NvencHevc
        )
    }

    /// 인코더가 받는 가장 큰 가로·세로(H.264 NVENC는 4096).
    fn max_side(self) -> u32 {
        if self == Encoder::NvencH264 {
            4096
        } else {
            8192
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RateControl {
    Cbr,
    Vbr,
    /// 화질 고정(NVENC CQ, AOM·SVT CRF). 최대 비트레이트가 있으면 그 안에서.
    Cq,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NvencTune {
    Hq,
    Ll,
    Ull,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Multipass {
    Disabled,
    Qres,
    Fullres,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum H264Profile {
    Baseline,
    Main,
    High,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NvencSettings {
    /// P1(가장 빠름) ~ P7(가장 좋음).
    pub preset: u8,
    pub tune: NvencTune,
    pub multipass: Multipass,
    /// 미리 볼 프레임 수(0이면 끔). 그만큼 늦어집니다.
    pub lookahead: u32,
    pub spatial_aq: bool,
    pub profile: H264Profile,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AomSettings {
    /// 실시간 모드의 속도(높을수록 빠름).
    pub cpu_used: u8,
    /// 화면 콘텐츠 도구(글자·UI에 유리).
    pub screen_content: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SvtSettings {
    /// 프리셋(높을수록 빠름).
    pub preset: u8,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EncodeSettings {
    pub encoder: Encoder,
    pub fps: u32,
    pub rate_control: RateControl,
    pub bitrate_kbps: u32,
    pub max_bitrate_kbps: Option<u32>,
    pub cq: u32,
    pub keyint_sec: u32,
    pub cursor: bool,
    pub nvenc: NvencSettings,
    pub aom: AomSettings,
    pub svt: SvtSettings,
}

/// 캡처할 것(gfxcapture의 핸들).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Window(u64),
    Monitor(u64),
}

/// 원본 크기와 세로 상한(None이면 원본) → 인코딩 크기(짝수, 인코더 상한 안, 비율 유지).
pub fn output_size(src: (u32, u32), max_height: Option<u32>, encoder: Encoder) -> (u32, u32) {
    let (sw, sh) = (src.0.max(2) as f64, src.1.max(2) as f64);
    let side = encoder.max_side() as f64;
    let scale = max_height
        .map_or(1.0, |h| (h as f64 / sh).min(1.0))
        .min(side / sw)
        .min(side / sh);
    let even = |v: f64| ((v / 2.0).round() as u32 * 2).max(2);
    (even(sw * scale), even(sh * scale))
}

const MIN_KBPS: u32 = 100;
const MAX_KBPS: u32 = 100_000;

/// ffmpeg 인자(실행 파일 이름 뒤).
pub fn ffmpeg_args(s: &EncodeSettings, target: Target, size: (u32, u32)) -> Vec<String> {
    let fps = s.fps.clamp(1, 120);
    let kbps = s.bitrate_kbps.clamp(MIN_KBPS, MAX_KBPS);
    let max_kbps = s.max_bitrate_kbps.map(|m| m.clamp(kbps, MAX_KBPS));
    let gop = (s.keyint_sec.clamp(1, 10) * fps).to_string();
    let (w, h) = size;
    let handle = match target {
        Target::Window(h) => format!("hwnd={h}"),
        Target::Monitor(m) => format!("hmonitor={m}"),
    };
    let mut graph = format!(
        "gfxcapture={handle}:capture_cursor={}:max_framerate={fps}:width={w}:height={h}:resize_mode=scale_aspect,fps={fps}",
        u8::from(s.cursor)
    );
    if !s.encoder.is_nvenc() {
        graph.push_str(
            ",hwdownload,format=bgra,scale=out_color_matrix=bt709:out_range=tv:flags=fast_bilinear,format=yuv420p",
        );
    }
    let mut a: Vec<String> = [
        "-hide_banner",
        "-nostdin",
        "-loglevel",
        "error",
        "-nostats",
        "-fflags",
        "nobuffer",
        "-filter_complex",
        &graph,
        "-an",
        "-c:v",
        s.encoder.ffmpeg(),
        "-g",
        &gop,
        "-bf",
        "0",
    ]
    .iter()
    .map(|x| x.to_string())
    .collect();
    let mut push = |xs: &[&str]| a.extend(xs.iter().map(|x| x.to_string()));
    let k = |v: u32| format!("{v}k");
    // 저지연 버퍼: 0.5초 분량.
    let buf = k((max_kbps.unwrap_or(kbps) / 2).max(MIN_KBPS));
    match s.encoder {
        Encoder::NvencAv1 | Encoder::NvencH264 | Encoder::NvencHevc => {
            let n = &s.nvenc;
            let preset = format!("p{}", n.preset.clamp(1, 7));
            let tune = match n.tune {
                NvencTune::Hq => "hq",
                NvencTune::Ll => "ll",
                NvencTune::Ull => "ull",
            };
            let multipass = match n.multipass {
                Multipass::Disabled => "disabled",
                Multipass::Qres => "qres",
                Multipass::Fullres => "fullres",
            };
            push(&["-preset", &preset, "-tune", tune, "-multipass", multipass]);
            push(&["-zerolatency", "1", "-delay", "0", "-forced-idr", "1"]);
            let la = n.lookahead.min(32);
            if la > 0 {
                push(&["-rc-lookahead", &la.to_string()]);
            }
            if n.spatial_aq {
                push(&["-spatial-aq", "1"]);
            }
            match s.rate_control {
                RateControl::Cbr => push(&[
                    "-rc",
                    "cbr",
                    "-b:v",
                    &k(kbps),
                    "-maxrate",
                    &k(kbps),
                    "-bufsize",
                    &buf,
                ]),
                RateControl::Vbr => push(&[
                    "-rc",
                    "vbr",
                    "-b:v",
                    &k(kbps),
                    "-maxrate",
                    &k(max_kbps.unwrap_or(kbps * 3 / 2)),
                    "-bufsize",
                    &buf,
                ]),
                RateControl::Cq => {
                    let cq = s.cq.clamp(1, 51).to_string();
                    push(&["-rc", "vbr", "-cq", &cq, "-b:v", "0"]);
                    if let Some(m) = max_kbps {
                        push(&["-maxrate", &k(m), "-bufsize", &buf]);
                    }
                }
            }
            match s.encoder {
                Encoder::NvencH264 => {
                    let p = match n.profile {
                        H264Profile::Baseline => "baseline",
                        H264Profile::Main => "main",
                        H264Profile::High => "high",
                    };
                    push(&["-profile:v", p]);
                }
                Encoder::NvencHevc => push(&["-profile:v", "main"]),
                _ => {}
            }
        }
        Encoder::AomAv1 => {
            let speed = s.aom.cpu_used.clamp(5, 8).to_string();
            push(&[
                "-usage",
                "realtime",
                "-cpu-used",
                &speed,
                "-lag-in-frames",
                "0",
                "-row-mt",
                "1",
            ]);
            // 화면이 클수록 타일을 나눠 여러 코어로.
            let tiles = if w * h >= 1920 * 1080 { "2x2" } else { "1x1" };
            push(&["-tiles", tiles]);
            if s.aom.screen_content {
                push(&["-aom-params", "tune-content=screen"]);
            }
            match s.rate_control {
                RateControl::Cbr => push(&[
                    "-b:v",
                    &k(kbps),
                    "-minrate",
                    &k(kbps),
                    "-maxrate",
                    &k(kbps),
                    "-bufsize",
                    &buf,
                ]),
                RateControl::Vbr => push(&[
                    "-b:v",
                    &k(kbps),
                    "-maxrate",
                    &k(max_kbps.unwrap_or(kbps * 3 / 2)),
                    "-bufsize",
                    &buf,
                ]),
                RateControl::Cq => {
                    let crf = s.cq.clamp(1, 63).to_string();
                    match max_kbps {
                        Some(m) => push(&["-crf", &crf, "-b:v", &k(m)]),
                        None => push(&["-crf", &crf, "-b:v", "0"]),
                    }
                }
            }
        }
        Encoder::SvtAv1 => {
            let preset = s.svt.preset.clamp(4, 13).to_string();
            push(&["-preset", &preset]);
            // 저지연 예측 구조(앞 프레임만 참조). 화면 콘텐츠는 SVT-AV1이 스스로 판단합니다(기본). 저지연 구조의 비트레이트
            // 방식은 CBR과 CRF뿐이라 VBR은 CBR로, CRF의 최대 비트레이트는 저지연에서 쓰지 않아(TPL 꺼짐) 넘기지 않습니다.
            push(&["-svtav1-params", "pred-struct=low-delay"]);
            match s.rate_control {
                RateControl::Cbr | RateControl::Vbr => {
                    push(&["-b:v", &k(kbps), "-maxrate", &k(kbps), "-bufsize", &buf])
                }
                RateControl::Cq => {
                    let crf = s.cq.clamp(1, 63).to_string();
                    push(&["-crf", &crf]);
                }
            }
        }
    }
    push(&[
        "-color_primaries",
        "bt709",
        "-color_trc",
        "bt709",
        "-colorspace",
        "bt709",
        "-color_range",
        "tv",
        "-f",
        "flv",
        "-flvflags",
        "no_metadata+no_duration_filesize+no_sequence_end",
        "-flush_packets",
        "1",
        "pipe:1",
    ]);
    a
}

/// 인코더를 이 기기에서 쓸 수 있는지 보는 짧은 인코딩(GPU·드라이버가 없으면 실패).
pub fn probe_args(encoder: Encoder) -> Vec<String> {
    [
        "-hide_banner",
        "-nostdin",
        "-loglevel",
        "error",
        "-f",
        "lavfi",
        "-i",
        "color=c=black:s=256x256:r=30",
        "-frames:v",
        "3",
        "-c:v",
        encoder.ffmpeg(),
        "-f",
        "null",
        "-",
    ]
    .iter()
    .map(|x| x.to_string())
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(encoder: Encoder, rc: RateControl) -> EncodeSettings {
        EncodeSettings {
            encoder,
            fps: 60,
            rate_control: rc,
            bitrate_kbps: 6000,
            max_bitrate_kbps: None,
            cq: 23,
            keyint_sec: 2,
            cursor: true,
            nvenc: NvencSettings {
                preset: 4,
                tune: NvencTune::Ll,
                multipass: Multipass::Disabled,
                lookahead: 0,
                spatial_aq: false,
                profile: H264Profile::High,
            },
            aom: AomSettings {
                cpu_used: 8,
                screen_content: true,
            },
            svt: SvtSettings { preset: 10 },
        }
    }

    fn has(a: &[String], pair: &[&str]) -> bool {
        a.windows(pair.len())
            .any(|w| w.iter().zip(pair).all(|(x, y)| x == y))
    }

    #[test]
    fn nvenc_h264_cbr() {
        let a = ffmpeg_args(
            &settings(Encoder::NvencH264, RateControl::Cbr),
            Target::Monitor(65537),
            (1920, 1080),
        );
        let graph = &a[a.iter().position(|x| x == "-filter_complex").unwrap() + 1];
        assert_eq!(
            graph,
            "gfxcapture=hmonitor=65537:capture_cursor=1:max_framerate=60:width=1920:height=1080:resize_mode=scale_aspect,fps=60"
        );
        assert!(has(&a, &["-c:v", "h264_nvenc"]));
        assert!(has(&a, &["-g", "120", "-bf", "0"]));
        assert!(has(
            &a,
            &[
                "-rc", "cbr", "-b:v", "6000k", "-maxrate", "6000k", "-bufsize", "3000k"
            ]
        ));
        assert!(has(&a, &["-zerolatency", "1", "-delay", "0"]));
        assert!(has(&a, &["-profile:v", "high"]));
        assert!(has(&a, &["-f", "flv"]));
        assert_eq!(a.last().unwrap(), "pipe:1");
    }

    #[test]
    fn software_encoders_download_and_convert() {
        let a = ffmpeg_args(
            &settings(Encoder::AomAv1, RateControl::Cq),
            Target::Window(0x1234),
            (1280, 720),
        );
        let graph = &a[a.iter().position(|x| x == "-filter_complex").unwrap() + 1];
        assert!(graph.starts_with("gfxcapture=hwnd=4660:"));
        assert!(graph.ends_with(",hwdownload,format=bgra,scale=out_color_matrix=bt709:out_range=tv:flags=fast_bilinear,format=yuv420p"));
        assert!(has(
            &a,
            &[
                "-usage",
                "realtime",
                "-cpu-used",
                "8",
                "-lag-in-frames",
                "0"
            ]
        ));
        assert!(has(&a, &["-tiles", "1x1"]));
        assert!(has(&a, &["-aom-params", "tune-content=screen"]));
        assert!(has(&a, &["-crf", "23", "-b:v", "0"]));

        let a = ffmpeg_args(
            &settings(Encoder::SvtAv1, RateControl::Vbr),
            Target::Window(1),
            (1920, 1080),
        );
        assert!(has(
            &a,
            &["-preset", "10", "-svtav1-params", "pred-struct=low-delay"]
        ));
        // 저지연 구조는 VBR이 없어 CBR로.
        assert!(has(&a, &["-b:v", "6000k", "-maxrate", "6000k"]));
    }

    #[test]
    fn values_are_clamped() {
        let mut s = settings(Encoder::NvencHevc, RateControl::Cq);
        s.fps = 1000;
        s.keyint_sec = 0;
        s.cq = 99;
        s.nvenc.preset = 9;
        s.nvenc.lookahead = 100;
        s.max_bitrate_kbps = Some(10);
        let a = ffmpeg_args(&s, Target::Monitor(1), (640, 360));
        assert!(has(&a, &["-g", "120"]));
        assert!(has(&a, &["-preset", "p7"]));
        assert!(has(&a, &["-rc-lookahead", "32"]));
        assert!(has(
            &a,
            &["-rc", "vbr", "-cq", "51", "-b:v", "0", "-maxrate", "6000k"]
        ));
        assert!(has(&a, &["-profile:v", "main"]));
    }

    #[test]
    fn output_sizes() {
        assert_eq!(
            output_size((2560, 1440), None, Encoder::NvencAv1),
            (2560, 1440)
        );
        assert_eq!(
            output_size((2560, 1440), Some(1080), Encoder::NvencAv1),
            (1920, 1080)
        );
        // 원본보다 키우지 않음, 홀수는 짝수로.
        assert_eq!(
            output_size((1001, 701), Some(1080), Encoder::AomAv1),
            (1002, 702)
        );
        // H.264 NVENC는 가로 4096 안으로.
        assert_eq!(
            output_size((5120, 1440), None, Encoder::NvencH264),
            (4096, 1152)
        );
        assert_eq!(
            output_size((5120, 1440), None, Encoder::NvencHevc),
            (5120, 1440)
        );
    }
}
