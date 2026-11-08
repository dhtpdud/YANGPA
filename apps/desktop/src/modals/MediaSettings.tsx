// 사용자 설정 → 음성·화면공유: 내가 보내는 화면·음성의 세부 설정(OBS의 출력 설정처럼). 이 기기만의 설정이고 통화 중에 바꿔도
// 바로 걸립니다(Opus 스테레오·FEC·DTX·패킷 길이는 다음 협상부터, ffmpeg 인코더는 인코더를 다시 띄움). 규칙과 기본값은
// voice/media.ts.

import { useEffect, useState } from 'react';
import type { NeuralCodecId, NeuralStatus, ScreenStatus } from '../api/client';
import { Toggle } from '../components/common';
import { currentLang, t } from '../i18n';
import { cx } from '../lib/misc';
import { isAndroid } from '../lib/platform';
import { useStore } from '../store/store';
import {
  AOM_SPEEDS,
  CQ_RANGE,
  DEFAULT_MEDIA,
  KEYINT_RANGE,
  NATIVE_ENCODERS,
  NVENC_PRESETS,
  OPUS_BITRATE_RANGE,
  OPUS_PTIMES,
  SCREEN_BITRATE_RANGE,
  SCREEN_FPS_CHOICES,
  SCREEN_RESOLUTIONS,
  SVT_PRESETS,
  type AudioCodec,
  type AudioPrefs,
  type H264Profile,
  type MediaPrefs,
  type NativeEncoderId,
  type NativeEncoderPrefs,
  type NvencMultipass,
  type NvencTune,
  type RateControl,
  type ScreenCodec,
  type ScreenEncoder,
  type ScreenDegradation,
  type ScreenFps,
  type ScreenPrefs,
  type ScreenResolution,
} from '../voice/media';
import { INPUT_VOLUME_RANGE } from '../voice/devices';
import { AudioDevicesSection, VolumeSlider } from './AudioDevices';
import { Field } from './Modal';

export const MEDIA_NOTE = t(
  '내가 보내는 화면·음성에 걸리는 이 기기만의 설정입니다. 통화 중에 바꿔도 바로 적용되고, 스테레오·FEC·DTX·패킷 길이는 다음 연결부터 적용됩니다.',
);
export const ENCODER_LIMIT_NOTE = t(
  '웹뷰(WebRTC) 인코더는 프리셋·키프레임 간격·멀티패스·Look-ahead 같은 인코더 내부 값을 앱에 열어 주지 않습니다. 이런 값을 고르려면 화면 인코더를 ffmpeg 인코더로 바꾸세요(데스크톱 앱, 윈도).',
);
export const CODEC_NOTE = t(
  '웹뷰 인코더는 종단간 암호화 때문에 VP8·VP9만 쓸 수 있습니다(AV1·H.264는 패킷화가 암호화된 본문을 읽어 화면이 나오지 않음). 상대가 고른 코덱을 받지 못하면 VP8로 보냅니다. 7명 이상 통화(SFU)는 늘 VP8입니다.',
);
export const NATIVE_CODEC_NOTE = t(
  'ffmpeg 인코더 화면을 풀지 못하는 시청자(예: HEVC 하드웨어 디코더가 없는 PC)에게는 이 기기에서 푼 화면을 이 코덱으로 다시 인코딩해 보냅니다. 7명 이상 통화(SFU)에서 공유를 시작하면 웹뷰 인코더(이 코덱)로 공유합니다.',
);
export const NATIVE_NOTE = t(
  'ffmpeg 인코더는 앱이 고른 창·모니터를 직접 캡처·인코딩하고, 프레임 전체를 종단간 암호화해 7명 이하 통화(메시)의 시청자에게 보냅니다. 설정을 바꾸면 인코더를 다시 띄워(잠깐 멈춤) 곧바로 걸립니다. 지연을 늘리는 B-프레임은 쓰지 않고, 늦게 들어온 시청자는 다음 키 프레임부터 봅니다.',
);
const FFMPEG_LICENSE = t(
  'FFmpeg 9.0.2 © FFmpeg 개발자(LGPL 2.1 이상, 화면공유에 쓰는 것만 넣은 빌드, GPL 부품 없음). libaom·SVT-AV1·nv-codec-headers가 함께 들어 있습니다. 앱은 ffmpeg.exe를 따로 띄울 뿐 링크하지 않습니다. 라이선스 전문은 ffmpeg 폴더의 LICENSE.txt·THIRD-PARTY-NOTICES.txt.',
);

const ENCODERS: Array<[ScreenEncoder, string, string]> = [
  ['webrtc', t('웹뷰 기본(WebRTC: VP8·VP9)'), t('모든 참가자가 받을 수 있고 회선에 맞춰 화질을 저절로 조절합니다. 세부 인코더 설정은 고를 수 없습니다.')],
  ['aom-av1', t('AOM AV1 (libaom, 소프트웨어)'), t('AV1 참조 인코더의 실시간 모드. 같은 비트레이트에서 가장 선명한 편이지만 CPU를 많이 씁니다.')],
  ['svt-av1', t('SVT-AV1 (소프트웨어)'), t('여러 코어를 잘 쓰는 AV1 인코더. 프리셋으로 속도와 화질을 고릅니다.')],
  ['nvenc-av1', 'NVIDIA NVENC AV1', t('GPU 인코더(RTX 40 이상). CPU를 거의 쓰지 않고 지연이 짧습니다.')],
  ['nvenc-h264', 'NVIDIA NVENC H.264', t('GPU 인코더. 거의 모든 기기가 풀 수 있는 코덱입니다.')],
  ['nvenc-hevc', 'NVIDIA NVENC HEVC', t('GPU 인코더. H.264보다 효율이 좋지만 받는 쪽에 HEVC 디코더가 있어야 합니다.')],
];

const RATE_CONTROLS: Array<[RateControl, string]> = [
  ['cbr', t('CBR(고정 비트레이트)')],
  ['vbr', t('VBR(가변 비트레이트)')],
  ['cq', t('화질 고정(CQ·CRF)')],
];
const TUNES: Array<[NvencTune, string]> = [
  ['hq', t('고화질')],
  ['ll', t('저지연')],
  ['ull', t('초저지연')],
];
const MULTIPASS: Array<[NvencMultipass, string]> = [
  ['disabled', t('끔')],
  ['qres', t('두 번(1/4 해상도)')],
  ['fullres', t('두 번(전체 해상도)')],
];
const PROFILES: Array<[H264Profile, string]> = [
  ['baseline', 'baseline'],
  ['main', 'main'],
  ['high', 'high'],
];

/** 데스크톱 앱이면 ffmpeg 폴더와 인코더 상태를 읽습니다(처음에는 인코더마다 짧게 인코딩해 봄). */
function useScreenStatus(): { status: ScreenStatus | null; supported: boolean; refresh: () => void } {
  const screen = useStore((s) => s.client?.transport?.screen);
  const [status, setStatus] = useState<ScreenStatus | null>(null);
  const [tick, setTick] = useState(0);
  useEffect(() => {
    if (!screen) return;
    let stale = false;
    void screen
      .status()
      .then((st) => !stale && setStatus(st))
      .catch(() => !stale && setStatus(null));
    return () => {
      stale = true;
    };
  }, [screen, tick]);
  return { status, supported: !!screen, refresh: () => setTick((n) => n + 1) };
}

/** 정수 칸(범위 안으로 맞춤). */
function IntInput({ value, range, label, unit, onChange }: { value: number; range: { min: number; max: number }; label: string; unit?: string; onChange: (v: number) => void }) {
  const [text, setText] = useState(String(value));
  useEffect(() => setText(String(value)), [value]);
  const commit = () => {
    const n = Number(text.trim());
    if (!text.trim() || !Number.isFinite(n)) {
      setText(String(value));
      return;
    }
    const v = Math.min(range.max, Math.max(range.min, Math.round(n)));
    setText(String(v));
    onChange(v);
  };
  return (
    <div className="row gap-s">
      <input
        className="input"
        type="text"
        inputMode="numeric"
        aria-label={label}
        value={text}
        onChange={(e) => setText(e.target.value)}
        onBlur={commit}
        onKeyDown={(e) => e.key === 'Enter' && commit()}
      />
      {unit && <span className="muted small">{unit}</span>}
    </div>
  );
}

export const NEURAL_NOTE = t(
  '신경망 코덱은 7명 이하 통화(메시)에서, 그 코덱 모델을 가진 상대에게만 씁니다. 모델이 없는 상대·7명 이상 통화에는 Opus로 보냅니다. 모델과 ONNX Runtime은 실행 파일 옆 models 폴더에 있습니다(설치본·포터블에 함께 들어 있음).',
);

const AUDIO_CODECS: Array<[AudioCodec, string, string]> = [
  ['opus', 'Opus', t('WebRTC 표준. 모든 참가자가 지원하고 지연이 가장 짧습니다.')],
  ['mimi', 'Mimi (Kyutai)', t('신경망 코덱. 약 1.1Kbps로 매우 적은 대역, 80ms 단위(지연 약 0.2초). 말소리에 맞춰져 음악·효과음은 약합니다. CPU를 씁니다.')],
  ['dac', 'DAC (Descript Audio Codec)', t('신경망 코덱. 약 24Kbps 고음질이지만 실시간용 구조가 아니라 지연이 약 0.5초로 깁니다. CPU를 많이 씁니다.')],
];

const NEURAL_LICENSE = t('모델: Mimi © Kyutai(CC BY 4.0), DAC © Descript(MIT). ONNX Runtime © Microsoft(MIT).');

/** 데스크톱 앱이면 모델 폴더 상태를 읽습니다. 신경망 코덱을 못 쓰는 전송이면 null. */
function useNeuralStatus(): { status: NeuralStatus | null; supported: boolean; refresh: () => void } {
  const neural = useStore((s) => s.client?.transport?.neural);
  const [status, setStatus] = useState<NeuralStatus | null>(null);
  const [tick, setTick] = useState(0);
  useEffect(() => {
    if (!neural) return;
    let stale = false;
    void neural
      .status()
      .then((st) => !stale && setStatus(st))
      .catch(() => !stale && setStatus(null));
    return () => {
      stale = true;
    };
  }, [neural, tick]);
  return {
    status,
    supported: !!neural,
    refresh: () => setTick((n) => n + 1),
  };
}

const RES_LABEL = (r: ScreenResolution) => (r === 'source' ? t('원본 그대로') : t('{r}p 이하', { r }));
const FPS_LABEL = (f: ScreenFps) => (f === 'auto' ? t('자동(선명하게 30 · 부드럽게 60)') : `${f}fps`);
const DEGRADATION: Array<[ScreenDegradation, string]> = [
  ['auto', t('자동(선명하게: 균형 · 부드럽게: 프레임 유지)')],
  ['balanced', t('균형(해상도·프레임을 함께 줄임)')],
  ['maintain-framerate', t('프레임 유지(해상도를 먼저 줄임)')],
  ['maintain-resolution', t('해상도 유지(프레임을 먼저 줄임)')],
];

/** 숫자 칸: 비우면 자동(null). 칸을 벗어날 때 범위 안으로 맞춰 저장합니다. */
function KbpsInput({
  value,
  range,
  placeholder,
  label,
  onChange,
}: {
  value: number | null;
  range: { min: number; max: number };
  placeholder: string;
  label: string;
  onChange: (v: number | null) => void;
}) {
  const [text, setText] = useState(value == null ? '' : String(value));
  const commit = () => {
    const s = text.trim();
    if (!s) {
      onChange(null);
      return;
    }
    const n = Number(s);
    if (!Number.isFinite(n)) {
      setText(value == null ? '' : String(value));
      return;
    }
    const v = Math.min(range.max, Math.max(range.min, Math.round(n)));
    setText(String(v));
    onChange(v);
  };
  return (
    <div className="row gap-s">
      <input
        className="input"
        type="text"
        inputMode="numeric"
        aria-label={label}
        placeholder={placeholder}
        value={text}
        onChange={(e) => setText(e.target.value)}
        onBlur={commit}
        onKeyDown={(e) => e.key === 'Enter' && commit()}
      />
      <span className="muted small">Kbps</span>
    </div>
  );
}

function Switch({ label, hint, checked, onChange }: { label: string; hint: string; checked: boolean; onChange: (v: boolean) => void }) {
  return (
    <div className="card row between">
      <div>
        <strong>{label}</strong>
        <div className="muted small">{hint}</div>
      </div>
      <Toggle checked={checked} label={label} onChange={onChange} />
    </div>
  );
}

export function MediaTab() {
  const media = useStore((s) => s.prefs.media ?? DEFAULT_MEDIA);
  const neural = useNeuralStatus();
  const missing = (c: NeuralCodecId) => neural.status?.codecs.find((x) => x.codec === c)?.missing ?? null;
  const usable = (c: AudioCodec) => c === 'opus' || (neural.supported && missing(c)?.length === 0);
  const setPrefs = useStore((s) => s.setPrefs);
  const save = (next: MediaPrefs) => setPrefs({ media: next });
  const setScreen = (patch: Partial<ScreenPrefs>) => save({ ...media, screen: { ...media.screen, ...patch } });
  const setAudio = (patch: Partial<AudioPrefs>) => save({ ...media, audio: { ...media.audio, ...patch } });
  const sc = media.screen;
  const au = media.audio;
  const screen = useScreenStatus();
  const encoderStatus = (e: NativeEncoderId) => screen.status?.encoders.find((x) => x.id === e);
  const encoderUsable = (e: ScreenEncoder) => e === 'webrtc' || (screen.supported && !!encoderStatus(e)?.available);
  const native = sc.encoder !== 'webrtc';
  const nv = sc.native;
  const setNative = (patch: Partial<NativeEncoderPrefs>) => setScreen({ native: { ...nv, ...patch } });
  return (
    <div className="stack media-settings">
      <AudioDevicesSection />

      <p className="muted small">{MEDIA_NOTE}</p>

      {!isAndroid() && (
      <>
      <h3>{t('화면공유')}</h3>
      <Field label={t('화면 인코더')} hint={ENCODERS.find(([e]) => e === sc.encoder)?.[2]}>
        <select className="select" aria-label={t('화면 인코더')} value={sc.encoder} onChange={(e) => setScreen({ encoder: e.target.value as ScreenEncoder })}>
          {ENCODERS.map(([e, label]) => (
            <option key={e} value={e} disabled={!encoderUsable(e) && sc.encoder !== e}>
              {e !== 'webrtc' && !encoderUsable(e)
                ? screen.supported
                  ? screen.status
                    ? t('{label} — 이 기기에서 쓸 수 없음', { label })
                    : t('{label} — 확인 중', { label })
                  : t('{label} — 데스크톱 앱만', { label })
                : label}
            </option>
          ))}
        </select>
      </Field>
      {native && !encoderUsable(sc.encoder) && (
        <p className="err small">
          {screen.status
            ? t('고른 ffmpeg 인코더를 이 기기에서 쓸 수 없어 웹뷰 기본 인코더로 공유합니다.')
            : t('고른 ffmpeg 인코더를 이 기기에서 쓸 수 없어 웹뷰 기본 인코더로 공유합니다(확인 중).')}
        </p>
      )}
      <div className="grid2">
        <Field label={native ? t('대체 코덱(WebRTC)') : t('비디오 코덱')} hint={native ? NATIVE_CODEC_NOTE : CODEC_NOTE}>
          <select className="select" aria-label={t('비디오 코덱')} value={sc.codec} onChange={(e) => setScreen({ codec: e.target.value as ScreenCodec })}>
            <option value="vp8">{t('VP8(호환성 최고)')}</option>
            <option value="vp9">{t('VP9(같은 비트레이트에서 더 선명, 인코딩 부담 큼)')}</option>
          </select>
        </Field>
        <Field label={t('출력 해상도')} hint={t('원본보다 크게 키우지는 않습니다. 줄이면 같은 비트레이트에서 더 부드럽습니다.')}>
          <select
            className="select"
            aria-label={t('출력 해상도')}
            value={String(sc.resolution)}
            onChange={(e) => setScreen({ resolution: (e.target.value === 'source' ? 'source' : Number(e.target.value)) as ScreenResolution })}
          >
            {SCREEN_RESOLUTIONS.map((r) => (
              <option key={r} value={String(r)}>
                {RES_LABEL(r)}
              </option>
            ))}
          </select>
        </Field>
        <Field label={t('최대 프레임')}>
          <select
            className="select"
            aria-label={t('최대 프레임')}
            value={String(sc.fps)}
            onChange={(e) => setScreen({ fps: (e.target.value === 'auto' ? 'auto' : Number(e.target.value)) as ScreenFps })}
          >
            {SCREEN_FPS_CHOICES.map((f) => (
              <option key={f} value={String(f)}>
                {FPS_LABEL(f)}
              </option>
            ))}
          </select>
        </Field>
        <Field
          label={native ? t('목표 비트레이트') : t('최대 비트레이트')}
          hint={
            native
              ? t('비우면 자동(선명하게 2,500 · 부드럽게 4,000). {min}~{max}Kbps. CBR·VBR의 목표값입니다.', {
                  min: SCREEN_BITRATE_RANGE.min,
                  max: SCREEN_BITRATE_RANGE.max.toLocaleString(currentLang()),
                })
              : t('비우면 자동(선명하게 2,500 · 부드럽게 4,000). {min}~{max}Kbps. 회선이 모자라면 이보다 낮게 보냅니다.', {
                  min: SCREEN_BITRATE_RANGE.min,
                  max: SCREEN_BITRATE_RANGE.max.toLocaleString(currentLang()),
                })
          }
        >
          <KbpsInput
            label={t('화면 최대 비트레이트')}
            value={sc.bitrateKbps}
            range={SCREEN_BITRATE_RANGE}
            placeholder={t('자동')}
            onChange={(v) => setScreen({ bitrateKbps: v })}
          />
        </Field>
      </div>
      <Field label={native ? t('대역이 모자랄 때(WebRTC로 보낼 때)') : t('대역이 모자랄 때')}>
        <div className="seg seg-wrap" role="group" aria-label={t('대역이 모자랄 때')}>
          {DEGRADATION.map(([v, label]) => (
            <button key={v} type="button" className={cx(sc.degradation === v && 'active')} aria-pressed={sc.degradation === v} onClick={() => setScreen({ degradation: v })}>
              {label}
            </button>
          ))}
        </div>
      </Field>
      <Switch
        label={t('화면 소리 보내기')}
        hint={t('화면을 보는 사람에게 소리도 보냅니다(메시 통화). 창을 공유하면 그 프로그램 소리만, 화면 전체면 이 앱(통화 목소리)을 뺀 컴퓨터 소리입니다.')}
        checked={sc.audio}
        onChange={(v) => setScreen({ audio: v })}
      />
      {sc.audio && (
        <Field label={t('화면 소리 음량')} hint={t('100%가 그대로입니다. 마이크 소리에 비해 크거나 작으면 맞추세요.')}>
          <VolumeSlider
            label={t('화면 소리 음량')}
            value={sc.audioVolume}
            range={INPUT_VOLUME_RANGE}
            onChange={(audioVolume) => {
              // 저장 직전의 값에 겹칩니다(막대가 늦게 저장해도 다른 칸을 되돌리지 않게).
              const cur = useStore.getState().prefs.media ?? DEFAULT_MEDIA;
              setPrefs({ media: { ...cur, screen: { ...cur.screen, audioVolume } } });
            }}
          />
        </Field>
      )}

      {native && (
        <section className="card stack native-encoder" aria-label={t('ffmpeg 인코더 설정')}>
          <strong>{t('ffmpeg 인코더 설정')}</strong>
          <p className="muted small">{NATIVE_NOTE}</p>
          <Field label={t('비트레이트 방식')}>
            <div className="seg seg-wrap" role="group" aria-label={t('비트레이트 방식')}>
              {RATE_CONTROLS.map(([v, label]) => {
                // SVT-AV1의 저지연 구조에는 VBR이 없습니다(CBR로 보냄).
                const off = v === 'vbr' && sc.encoder === 'svt-av1';
                return (
                  <button
                    key={v}
                    type="button"
                    className={cx(nv.rateControl === v && !off && 'active')}
                    aria-pressed={nv.rateControl === v && !off}
                    disabled={off}
                    title={off ? t('SVT-AV1의 저지연 모드에는 VBR이 없습니다') : undefined}
                    onClick={() => setNative({ rateControl: v })}
                  >
                    {label}
                  </button>
                );
              })}
            </div>
          </Field>
          <div className="grid2">
            {/* SVT-AV1의 저지연 구조는 VBR이 없고 CRF 상한도 쓰지 않습니다. */}
            {nv.rateControl !== 'cbr' && sc.encoder !== 'svt-av1' && (
              <Field label={t('최대 비트레이트')} hint={nv.rateControl === 'vbr' ? t('비우면 목표의 1.5배.') : t('비우면 상한 없음(화질만 맞춤).')}>
                <KbpsInput
                  label={t('인코더 최대 비트레이트')}
                  value={nv.maxBitrateKbps}
                  range={{ min: SCREEN_BITRATE_RANGE.min, max: SCREEN_BITRATE_RANGE.max * 3 }}
                  placeholder={t('자동')}
                  onChange={(v) => setNative({ maxBitrateKbps: v })}
                />
              </Field>
            )}
            {nv.rateControl === 'cq' && (
              <Field label={t('화질 값(CQ·CRF)')} hint={t('작을수록 선명하고 대역을 많이 씁니다. NVENC는 51까지, AV1은 63까지.')}>
                <IntInput label={t('화질 값')} value={nv.cq} range={CQ_RANGE} onChange={(v) => setNative({ cq: v })} />
              </Field>
            )}
            <Field label={t('키 프레임 간격')} hint={t('늦게 들어온 시청자·끊긴 뒤 화면이 다시 나오기까지의 최대 시간입니다.')}>
              <IntInput label={t('키 프레임 간격')} value={nv.keyintSec} range={KEYINT_RANGE} unit={t('초')} onChange={(v) => setNative({ keyintSec: v })} />
            </Field>
          </div>
          {sc.encoder.startsWith('nvenc') && (
            <div className="grid2">
              <Field label={t('프리셋')} hint={t('P1이 가장 빠르고 P7이 가장 선명합니다.')}>
                <select className="select" aria-label={t('NVENC 프리셋')} value={String(nv.nvenc.preset)} onChange={(e) => setNative({ nvenc: { ...nv.nvenc, preset: Number(e.target.value) } })}>
                  {NVENC_PRESETS.map((p) => (
                    <option key={p} value={String(p)}>
                      {p === 4 ? t('P{p}(기본)', { p }) : `P${p}`}
                    </option>
                  ))}
                </select>
              </Field>
              <Field label={t('튜닝')}>
                <select className="select" aria-label={t('NVENC 튜닝')} value={nv.nvenc.tune} onChange={(e) => setNative({ nvenc: { ...nv.nvenc, tune: e.target.value as NvencTune } })}>
                  {TUNES.map(([v, label]) => (
                    <option key={v} value={v}>
                      {label}
                    </option>
                  ))}
                </select>
              </Field>
              <Field label={t('멀티패스')} hint={t('두 번 보면 화질이 좋아지고 GPU를 더 씁니다.')}>
                <select className="select" aria-label={t('NVENC 멀티패스')} value={nv.nvenc.multipass} onChange={(e) => setNative({ nvenc: { ...nv.nvenc, multipass: e.target.value as NvencMultipass } })}>
                  {MULTIPASS.map(([v, label]) => (
                    <option key={v} value={v}>
                      {label}
                    </option>
                  ))}
                </select>
              </Field>
              <Field label="Look-ahead" hint={t('미리 볼 프레임 수(0이면 끔). 그만큼 늦어집니다.')}>
                <IntInput label="Look-ahead" value={nv.nvenc.lookahead} range={{ min: 0, max: 32 }} unit={t('프레임')} onChange={(v) => setNative({ nvenc: { ...nv.nvenc, lookahead: v } })} />
              </Field>
              {sc.encoder === 'nvenc-h264' && (
                <Field label={t('프로파일')}>
                  <select className="select" aria-label={t('H.264 프로파일')} value={nv.nvenc.profile} onChange={(e) => setNative({ nvenc: { ...nv.nvenc, profile: e.target.value as H264Profile } })}>
                    {PROFILES.map(([v, label]) => (
                      <option key={v} value={v}>
                        {label}
                      </option>
                    ))}
                  </select>
                </Field>
              )}
            </div>
          )}
          {sc.encoder.startsWith('nvenc') && (
            <Switch label={t('심리 시각 튜닝(공간 AQ)')} hint={t('평평한 곳에 비트를 더 써 띠 무늬를 줄입니다.')} checked={nv.nvenc.spatialAq} onChange={(v) => setNative({ nvenc: { ...nv.nvenc, spatialAq: v } })} />
          )}
          {sc.encoder === 'aom-av1' && (
            <>
              <Field label={t('속도(cpu-used)')} hint={t('높을수록 빠르고 덜 선명합니다. 실시간은 5~8.')}>
                <select className="select" aria-label={t('AOM 속도')} value={String(nv.aom.cpuUsed)} onChange={(e) => setNative({ aom: { ...nv.aom, cpuUsed: Number(e.target.value) } })}>
                  {AOM_SPEEDS.map((v) => (
                    <option key={v} value={String(v)}>
                      {v === 8 ? t('{v}(기본, 가장 빠름)', { v }) : v}
                    </option>
                  ))}
                </select>
              </Field>
              <Switch label={t('화면 콘텐츠 도구')} hint={t('글자·UI가 많은 화면에서 더 선명합니다(게임·영상은 끄는 편이 나음).')} checked={nv.aom.screenContent} onChange={(v) => setNative({ aom: { ...nv.aom, screenContent: v } })} />
            </>
          )}
          {sc.encoder === 'svt-av1' && (
            <Field label={t('프리셋')} hint={t('높을수록 빠르고 덜 선명합니다. 실시간은 8 이상을 권합니다. 화면 콘텐츠는 인코더가 스스로 판단합니다.')}>
                <select className="select" aria-label={t('SVT-AV1 프리셋')} value={String(nv.svt.preset)} onChange={(e) => setNative({ svt: { ...nv.svt, preset: Number(e.target.value) } })}>
                  {SVT_PRESETS.map((v) => (
                    <option key={v} value={String(v)}>
                      {v === 10 ? t('{v}(기본)', { v }) : v}
                    </option>
                  ))}
                </select>
            </Field>
          )}
          <Switch label={t('마우스 커서 보이기')} hint={t('공유 화면에 마우스 커서를 그립니다.')} checked={nv.cursor} onChange={(v) => setNative({ cursor: v })} />
        </section>
      )}
      {screen.supported && (
        <section className="card stack ffmpeg-status" aria-label={t('ffmpeg 인코더 상태')}>
          <strong>{t('ffmpeg 인코더')}</strong>
          <div className="row gap-s">
            <code className="grow neural-dir" title={screen.status?.dir}>
              {screen.status?.dir ?? t('확인 중…')}
            </code>
            <button type="button" className="btn small ghost" onClick={screen.refresh}>
              {t('다시 확인')}
            </button>
          </div>
          {screen.status && screen.status.missing.length > 0 && <p className="err small">{t('빠졌거나 바뀐 파일: {files}', { files: screen.status.missing.join(', ') })}</p>}
          {NATIVE_ENCODERS.map((e) => {
            const st = encoderStatus(e);
            return (
              <div key={e} className="small">
                <strong>{ENCODERS.find(([x]) => x === e)?.[1]}</strong>:{' '}
                {!st ? (
                  t('확인 중…')
                ) : st.available ? (
                  <span className="ok">{t('사용 가능')}</span>
                ) : (
                  <span className="err">{st.error ? t('쓸 수 없음 ({error})', { error: st.error }) : t('쓸 수 없음')}</span>
                )}
              </div>
            );
          })}
          <p className="muted small">{FFMPEG_LICENSE}</p>
        </section>
      )}

      </>
      )}
      <h3>{t('음성')}</h3>
      <div className="grid2">
        <Field label={t('오디오 코덱')} hint={AUDIO_CODECS.find(([c]) => c === au.codec)?.[2]}>
          <select className="select" aria-label={t('오디오 코덱')} value={au.codec} onChange={(e) => setAudio({ codec: e.target.value as AudioCodec })}>
            {AUDIO_CODECS.map(([c, label]) => (
              <option key={c} value={c} disabled={!usable(c) && au.codec !== c}>
                {c !== 'opus' && !usable(c) ? (neural.supported ? t('{label} — 모델 없음', { label }) : t('{label} — 데스크톱 앱만', { label })) : label}
              </option>
            ))}
          </select>
        </Field>
        <Field
          label={t('최대 비트레이트')}
          hint={t('비우면 자동(말소리 약 32Kbps). {min}~{max}Kbps. 음악은 96~128, 스테레오는 그 두 배쯤.', { min: OPUS_BITRATE_RANGE.min, max: OPUS_BITRATE_RANGE.max })}
        >
          <KbpsInput label={t('음성 최대 비트레이트')} value={au.bitrateKbps} range={OPUS_BITRATE_RANGE} placeholder={t('자동')} onChange={(v) => setAudio({ bitrateKbps: v })} />
        </Field>
        <Field label={t('패킷 길이')} hint={t('짧을수록 지연이 줄고, 길수록 대역을 아낍니다.')}>
          <select className="select" aria-label={t('패킷 길이')} value={String(au.ptime)} onChange={(e) => setAudio({ ptime: Number(e.target.value) as AudioPrefs['ptime'] })}>
            {OPUS_PTIMES.map((p) => (
              <option key={p} value={String(p)}>
                {p === 20 ? t('{p}ms(기본)', { p }) : `${p}ms`}
              </option>
            ))}
          </select>
        </Field>
      </div>
      {au.codec !== 'opus' && !usable(au.codec) && (
        <p className="err small">{t('고른 신경망 코덱의 모델이 없어 지금은 Opus로 보냅니다.')}</p>
      )}
      <section className="card stack neural-models" aria-label={t('신경망 코덱 모델')}>
        <strong>{t('신경망 코덱 모델')}</strong>
        <p className="muted small">{NEURAL_NOTE}</p>
        {!neural.supported ? (
          <p className="muted small">{t('신경망 코덱은 데스크톱 앱에서만 쓸 수 있습니다.')}</p>
        ) : (
          <>
            <div className="row gap-s">
              <code className="grow neural-dir" title={neural.status?.dir}>
                {neural.status?.dir ?? t('확인 중…')}
              </code>
              <button type="button" className="btn small ghost" onClick={neural.refresh}>
                {t('다시 확인')}
              </button>
            </div>
            {(['mimi', 'dac'] as const).map((c) => {
              const m = missing(c);
              return (
                <div key={c} className="small">
                  <strong>{c === 'mimi' ? 'Mimi' : 'DAC'}</strong>:{' '}
                  {m == null ? (
                    t('확인 중…')
                  ) : m.length === 0 ? (
                    <span className="ok">{t('사용 가능')}</span>
                  ) : (
                    <span className="err">{t('빠졌거나 바뀐 파일 {files}', { files: m.join(', ') })}</span>
                  )}
                </div>
              );
            })}
          </>
        )}
        <p className="muted small">{NEURAL_LICENSE}</p>
      </section>
      {au.codec !== 'opus' && <p className="muted small">{t('아래 Opus 설정은 신경망 코덱을 못 쓰는 상대에게 Opus로 보낼 때 씁니다.')}</p>}
      <Switch label={t('스테레오')} hint={t('마이크를 두 채널로 받아 보냅니다(악기·음악용). 말소리만이면 끄는 편이 대역을 아낍니다.')} checked={au.stereo} onChange={(v) => setAudio({ stereo: v })} />
      <Switch label={t('오류 보정(FEC)')} hint={t('패킷이 빠져도 다음 패킷에 실린 정보로 메웁니다. 끊기는 회선에서 켜 두세요.')} checked={au.fec} onChange={(v) => setAudio({ fec: v })} />
      <Switch label={t('무음 때 전송 줄이기(DTX)')} hint={t('말하지 않을 때 거의 보내지 않습니다. 배경음을 그대로 보내려면 끄세요.')} checked={au.dtx} onChange={(v) => setAudio({ dtx: v })} />
      <Switch label={t('에코 제거')} hint={t('스피커 소리가 마이크로 되돌아가지 않게 합니다.')} checked={au.echoCancellation} onChange={(v) => setAudio({ echoCancellation: v })} />
      <Switch label={t('잡음 억제')} hint={t('키보드·팬 소리 같은 배경 잡음을 줄입니다. 음악을 보낼 때는 끄세요.')} checked={au.noiseSuppression} onChange={(v) => setAudio({ noiseSuppression: v })} />
      <Switch label={t('자동 음량')} hint={t('마이크 크기를 저절로 맞춥니다.')} checked={au.autoGain} onChange={(v) => setAudio({ autoGain: v })} />

      <p className="muted small">{ENCODER_LIMIT_NOTE}</p>
      <div className="row">
        {/* 장치·음량은 위 '장치'에서 따로 고르므로 되돌리지 않습니다. */}
        <button type="button" className="btn ghost" onClick={() => save({ ...DEFAULT_MEDIA, devices: media.devices })}>
          {t('기본값으로 되돌리기')}
        </button>
      </div>
    </div>
  );
}
