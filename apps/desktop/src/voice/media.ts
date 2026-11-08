// 음성·화면공유 세부 설정(이 기기만, 앱 설정 파일 `prefs.media`). 내가 '보내는' 음성·화면에 걸립니다.
//
// 화면 인코더는 두 갈래입니다.
// - 웹뷰 기본(WebRTC): 웹뷰가 열어 주는 것만 둡니다: 코덱(VP8·VP9)·출력 해상도·프레임·최대 비트레이트·대역 부족 시 우선순위.
//   AV1·H.264는 패킷화가 프레임 본문을 읽어 종단간 암호화와 함께 쓰면 화면이 나오지 않으므로(실험으로 확인) 고를 수 없습니다.
// - ffmpeg 인코더(데스크톱 앱, 윈도): AOM AV1·SVT-AV1·NVENC AV1/H.264/HEVC. 앱이 캡처·인코딩하고 프레임 전체를 암호화해
//   데이터 채널로 보내므로 코덱 제약이 없고, OBS처럼 비트레이트 방식·프리셋·튜닝·키 프레임 간격 등을 고릅니다
//   (voice/native-screen.ts, src-tauri/src/screen). 그 코덱을 풀지 못하는 시청자·SFU 통화에는 그 화면을 웹뷰 코덱(VP8·VP9)으로
//   다시 인코딩해 보냅니다. SFU 통화는 호스트가 VP8만 나르므로 그 경우 늘 VP8입니다.
// 음성(Opus)은 비트레이트·스테레오·FEC·DTX·패킷 길이와 마이크 처리.

import { DEFAULT_DEVICES, INPUT_VOLUME_RANGE, sanitizeDevices, type DevicePrefs } from './devices';
import type { ContentHint } from './session';
import { setFmtpParams } from './sdp';

export type ScreenCodec = 'vp8' | 'vp9';
/** ffmpeg 인코더(src-tauri/src/screen/args.rs의 `Encoder`). */
export type NativeEncoderId = 'aom-av1' | 'svt-av1' | 'nvenc-av1' | 'nvenc-h264' | 'nvenc-hevc';
/** 화면 인코더: 웹뷰 기본(WebRTC) 또는 ffmpeg 인코더. */
export type ScreenEncoder = 'webrtc' | NativeEncoderId;
export type RateControl = 'cbr' | 'vbr' | 'cq';
export type NvencTune = 'hq' | 'll' | 'ull';
export type NvencMultipass = 'disabled' | 'qres' | 'fullres';
export type H264Profile = 'baseline' | 'main' | 'high';
/** 출력 해상도: 원본 그대로 또는 세로 픽셀 상한(가로는 비율대로). */
export type ScreenResolution = 'source' | 2160 | 1440 | 1080 | 720 | 480;
export type ScreenFps = 'auto' | 15 | 30 | 60 | 120;
export type ScreenDegradation = 'auto' | 'balanced' | 'maintain-framerate' | 'maintain-resolution';
export type OpusPtime = 10 | 20 | 40 | 60;
/**
 * 음성 코덱: Opus(WebRTC 기본) 또는 신경망 코덱(Mimi·DAC, 데스크톱 앱에 모델 폴더가 있을 때). 신경망 코덱은 메시 통화에서
 * 그 코덱을 풀 수 있다고 알린 상대에게만 쓰고, 나머지는 Opus입니다(voice/neural.ts).
 */
export type AudioCodec = 'opus' | 'mimi' | 'dac';

/** ffmpeg 인코더 세부 설정(OBS의 출력 설정처럼). 해상도·프레임·목표 비트레이트는 ScreenPrefs의 것을 함께 씁니다. */
export interface NativeEncoderPrefs {
  rateControl: RateControl;
  /** VBR의 최대, 화질 고정(CQ)의 상한(Kbps). null이면 VBR은 목표의 1.5배, CQ는 상한 없음. */
  maxBitrateKbps: number | null;
  /** 화질 고정 값(작을수록 좋음): NVENC CQ 1~51, AV1 CRF 1~63. */
  cq: number;
  /** 키 프레임 간격(초). 짧을수록 늦게 들어온 시청자·끊긴 뒤 화면이 빨리 나오고, 대역을 더 씁니다. */
  keyintSec: number;
  cursor: boolean;
  nvenc: { preset: number; tune: NvencTune; multipass: NvencMultipass; lookahead: number; spatialAq: boolean; profile: H264Profile };
  aom: { cpuUsed: number; screenContent: boolean };
  /** SVT-AV1은 화면 콘텐츠를 스스로 판단하고, 저지연 구조라 VBR이 없습니다(CBR로 보냄). */
  svt: { preset: number };
}

export interface ScreenPrefs {
  encoder: ScreenEncoder;
  /** 웹뷰(WebRTC) 코덱. ffmpeg 인코더를 쓸 때는 그 코덱을 풀지 못하는 시청자에게 보낼 코덱. */
  codec: ScreenCodec;
  resolution: ScreenResolution;
  fps: ScreenFps;
  /** 최대 비트레이트(Kbps). null이면 화면 종류(선명하게·부드럽게)에 맞춘 기본값. */
  bitrateKbps: number | null;
  degradation: ScreenDegradation;
  /** 화면 소리도 보낼지(데스크톱 앱·메시 통화, 창이면 그 프로그램 소리, 화면이면 이 앱을 뺀 시스템 소리). */
  audio: boolean;
  /** 보내는 화면 소리 음량(%). 100이면 그대로, 넘으면 키움(마이크 음량과 같은 범위). */
  audioVolume: number;
  native: NativeEncoderPrefs;
}

export interface AudioPrefs {
  codec: AudioCodec;
  /** Opus 최대 비트레이트(Kbps). null이면 브라우저 기본(말소리 약 32Kbps). */
  bitrateKbps: number | null;
  stereo: boolean;
  /** 인밴드 FEC: 패킷이 빠져도 앞 패킷에 실린 정보로 메움(비트레이트가 조금 늚). */
  fec: boolean;
  /** 말하지 않을 때 거의 보내지 않음. */
  dtx: boolean;
  /** 패킷 하나에 담는 소리 길이(ms). 짧을수록 지연이 줄고 오버헤드가 늚. */
  ptime: OpusPtime;
  echoCancellation: boolean;
  noiseSuppression: boolean;
  /** 자동 음량(웹뷰 AGC). 저장 이름이 옛 `autoGainControl`과 다른 까닭은 `sanitizeMedia`에. */
  autoGain: boolean;
}

export interface MediaPrefs {
  screen: ScreenPrefs;
  audio: AudioPrefs;
  /** 마이크·스피커 장치와 음량(voice/devices.ts). */
  devices: DevicePrefs;
}

export const DEFAULT_NATIVE: NativeEncoderPrefs = {
  rateControl: 'cbr',
  maxBitrateKbps: null,
  cq: 28,
  keyintSec: 2,
  cursor: true,
  nvenc: { preset: 4, tune: 'll', multipass: 'disabled', lookahead: 0, spatialAq: false, profile: 'high' },
  aom: { cpuUsed: 8, screenContent: true },
  svt: { preset: 10 },
};

export const DEFAULT_MEDIA: MediaPrefs = {
  screen: {
    encoder: 'webrtc',
    codec: 'vp8',
    resolution: 'source',
    fps: 'auto',
    bitrateKbps: null,
    degradation: 'auto',
    audio: true,
    audioVolume: 100,
    native: DEFAULT_NATIVE,
  },
  audio: {
    codec: 'opus',
    bitrateKbps: null,
    stereo: false,
    fec: true,
    dtx: true,
    ptime: 20,
    // 웹뷰 마이크 처리는 기본으로 끔(사용자 결정, 2026-10-27): 음질을 깎고 음성 코덱·장치 쪽 처리와 겹침.
    // 자동 음량만 켬(사용자 결정, 2026-10-31): 마이크 소리가 너무 작았음.
    echoCancellation: false,
    noiseSuppression: false,
    autoGain: true,
  },
  devices: DEFAULT_DEVICES,
};

export const SCREEN_RESOLUTIONS: readonly ScreenResolution[] = ['source', 2160, 1440, 1080, 720, 480];
export const SCREEN_FPS_CHOICES: readonly ScreenFps[] = ['auto', 15, 30, 60, 120];
export const OPUS_PTIMES: readonly OpusPtime[] = [10, 20, 40, 60];
export const SCREEN_BITRATE_RANGE = { min: 100, max: 20_000 } as const;
export const NATIVE_ENCODERS: readonly NativeEncoderId[] = ['aom-av1', 'svt-av1', 'nvenc-av1', 'nvenc-h264', 'nvenc-hevc'];
export const KEYINT_RANGE = { min: 1, max: 10 } as const;
export const CQ_RANGE = { min: 1, max: 63 } as const;
export const NVENC_PRESETS: readonly number[] = [1, 2, 3, 4, 5, 6, 7];
export const AOM_SPEEDS: readonly number[] = [5, 6, 7, 8];
export const SVT_PRESETS: readonly number[] = [4, 5, 6, 7, 8, 9, 10, 11, 12, 13];
export const OPUS_BITRATE_RANGE = { min: 6, max: 510 } as const;

/** 화면 종류별 기본값(설정이 'auto'·null일 때). 글자 위주도 30fps(15fps는 받는 쪽에서 끊겨 보였음). */
export const SCREEN_FPS: Record<ContentHint, number> = { detail: 30, motion: 60 };
/** 화면 종류별 기본 목표 비트레이트(Kbps, 설정이 비었을 때). WebRTC의 가장 높은 층과 같습니다. */
export const SCREEN_DEFAULT_KBPS: Record<ContentHint, number> = { detail: 2500, motion: 4000 };
/**
 * 대역이 모자랄 때 무엇을 먼저 줄일지의 기본값. 글자 위주라도 'maintain-resolution'은 큰 화면(QHD 등)에서 프레임을
 * 1~2fps까지 떨어뜨리므로 'balanced'(해상도와 프레임을 함께 조금씩). 움직임 위주는 프레임을 지킵니다.
 */
export const SCREEN_DEGRADATION: Record<ContentHint, RTCDegradationPreference> = { detail: 'balanced', motion: 'maintain-framerate' };

const clampInt = (v: unknown, min: number, max: number): number | null =>
  typeof v === 'number' && Number.isFinite(v) ? Math.min(max, Math.max(min, Math.round(v))) : null;

/** 설정 파일에서 읽은 값 → 쓸 수 있는 값(모르는 값은 기본값). */
export function sanitizeMedia(raw: unknown): MediaPrefs {
  const r = (raw && typeof raw === 'object' ? raw : {}) as { screen?: Record<string, unknown>; audio?: Record<string, unknown>; devices?: unknown };
  const s = r.screen ?? {};
  const a = r.audio ?? {};
  const d = DEFAULT_MEDIA;
  const bool = (v: unknown, def: boolean) => (typeof v === 'boolean' ? v : def);
  return {
    screen: {
      encoder: NATIVE_ENCODERS.includes(s.encoder as NativeEncoderId) ? (s.encoder as NativeEncoderId) : 'webrtc',
      codec: s.codec === 'vp9' ? 'vp9' : 'vp8',
      resolution: SCREEN_RESOLUTIONS.includes(s.resolution as ScreenResolution) ? (s.resolution as ScreenResolution) : d.screen.resolution,
      fps: SCREEN_FPS_CHOICES.includes(s.fps as ScreenFps) ? (s.fps as ScreenFps) : d.screen.fps,
      bitrateKbps: s.bitrateKbps == null ? null : clampInt(s.bitrateKbps, SCREEN_BITRATE_RANGE.min, SCREEN_BITRATE_RANGE.max),
      degradation: ['auto', 'balanced', 'maintain-framerate', 'maintain-resolution'].includes(s.degradation as string)
        ? (s.degradation as ScreenDegradation)
        : d.screen.degradation,
      audio: bool(s.audio, d.screen.audio),
      audioVolume: clampInt(s.audioVolume, INPUT_VOLUME_RANGE.min, INPUT_VOLUME_RANGE.max) ?? d.screen.audioVolume,
      native: sanitizeNative(s.native),
    },
    audio: {
      codec: a.codec === 'mimi' || a.codec === 'dac' ? a.codec : 'opus',
      bitrateKbps: a.bitrateKbps == null ? null : clampInt(a.bitrateKbps, OPUS_BITRATE_RANGE.min, OPUS_BITRATE_RANGE.max),
      stereo: bool(a.stereo, d.audio.stereo),
      fec: bool(a.fec, d.audio.fec),
      dtx: bool(a.dtx, d.audio.dtx),
      ptime: OPUS_PTIMES.includes(a.ptime as OpusPtime) ? (a.ptime as OpusPtime) : d.audio.ptime,
      echoCancellation: bool(a.echoCancellation, d.audio.echoCancellation),
      noiseSuppression: bool(a.noiseSuppression, d.audio.noiseSuppression),
      // 옛 `autoGainControl`은 읽지 않습니다: 기본 꺼짐 시절에 장치 설정과 함께 저장된 값이라, 기본을 켬으로 바꾸며 한 번 되돌림.
      autoGain: bool(a.autoGain, d.audio.autoGain),
    },
    devices: sanitizeDevices(r.devices),
  };
}

function sanitizeNative(raw: unknown): NativeEncoderPrefs {
  const r = (raw && typeof raw === 'object' ? raw : {}) as Record<string, unknown>;
  const d = DEFAULT_NATIVE;
  const obj = (v: unknown) => (v && typeof v === 'object' ? (v as Record<string, unknown>) : {});
  const bool = (v: unknown, def: boolean) => (typeof v === 'boolean' ? v : def);
  const pick = <T>(v: unknown, list: readonly T[], def: T): T => (list.includes(v as T) ? (v as T) : def);
  const nv = obj(r.nvenc);
  const aom = obj(r.aom);
  const svt = obj(r.svt);
  return {
    rateControl: pick<RateControl>(r.rateControl, ['cbr', 'vbr', 'cq'], d.rateControl),
    maxBitrateKbps: r.maxBitrateKbps == null ? null : clampInt(r.maxBitrateKbps, SCREEN_BITRATE_RANGE.min, SCREEN_BITRATE_RANGE.max * 3),
    cq: clampInt(r.cq, CQ_RANGE.min, CQ_RANGE.max) ?? d.cq,
    keyintSec: clampInt(r.keyintSec, KEYINT_RANGE.min, KEYINT_RANGE.max) ?? d.keyintSec,
    cursor: bool(r.cursor, d.cursor),
    nvenc: {
      preset: pick(nv.preset, NVENC_PRESETS, d.nvenc.preset),
      tune: pick<NvencTune>(nv.tune, ['hq', 'll', 'ull'], d.nvenc.tune),
      multipass: pick<NvencMultipass>(nv.multipass, ['disabled', 'qres', 'fullres'], d.nvenc.multipass),
      lookahead: clampInt(nv.lookahead, 0, 32) ?? d.nvenc.lookahead,
      spatialAq: bool(nv.spatialAq, d.nvenc.spatialAq),
      profile: pick<H264Profile>(nv.profile, ['baseline', 'main', 'high'], d.nvenc.profile),
    },
    aom: { cpuUsed: pick(aom.cpuUsed, AOM_SPEEDS, d.aom.cpuUsed), screenContent: bool(aom.screenContent, d.aom.screenContent) },
    svt: { preset: pick(svt.preset, SVT_PRESETS, d.svt.preset) },
  };
}

/** ffmpeg 인코더에 넘길 설정(src-tauri/src/screen/args.rs의 `EncodeSettings`). */
export interface NativeEncodeSettings {
  encoder: NativeEncoderId;
  fps: number;
  rateControl: RateControl;
  bitrateKbps: number;
  maxBitrateKbps: number | null;
  cq: number;
  keyintSec: number;
  cursor: boolean;
  nvenc: NativeEncoderPrefs['nvenc'];
  aom: NativeEncoderPrefs['aom'];
  svt: NativeEncoderPrefs['svt'];
}

export function nativeSettings(p: ScreenPrefs, encoder: NativeEncoderId, hint: ContentHint): NativeEncodeSettings {
  const n = p.native;
  return {
    encoder,
    fps: screenFps(p, hint),
    // SVT-AV1의 저지연 구조에는 VBR이 없어 CBR로.
    rateControl: encoder === 'svt-av1' && n.rateControl === 'vbr' ? 'cbr' : n.rateControl,
    bitrateKbps: p.bitrateKbps ?? SCREEN_DEFAULT_KBPS[hint],
    maxBitrateKbps: n.maxBitrateKbps,
    cq: n.cq,
    keyintSec: n.keyintSec,
    cursor: n.cursor,
    nvenc: n.nvenc,
    aom: n.aom,
    svt: n.svt,
  };
}

/** ffmpeg 인코더에 넘길 세로 상한(원본이면 null). */
export function nativeMaxHeight(p: ScreenPrefs): number | null {
  return p.resolution === 'source' ? null : p.resolution;
}

/** 이번 공유의 최대 프레임. */
export function screenFps(p: ScreenPrefs, hint: ContentHint): number {
  return p.fps === 'auto' ? SCREEN_FPS[hint] : p.fps;
}

export function screenDegradation(p: ScreenPrefs, hint: ContentHint): RTCDegradationPreference {
  return p.degradation === 'auto' ? SCREEN_DEGRADATION[hint] : p.degradation;
}

/** 원본 세로 픽셀 → 가장 높은 층의 축소 배율(1 이상). 원본이 상한보다 작으면 키우지 않습니다. */
export function screenScale(p: ScreenPrefs, sourceHeight: number | undefined): number {
  if (p.resolution === 'source' || !sourceHeight || sourceHeight <= p.resolution) return 1;
  return Math.round((sourceHeight / p.resolution) * 1000) / 1000;
}

/** 마이크 제약(getUserMedia). 스테레오면 두 채널로 받습니다. `deviceId`는 고른 마이크(없으면 시스템 기본). */
export function micConstraints(a: AudioPrefs, deviceId?: string): MediaTrackConstraints {
  return {
    echoCancellation: a.echoCancellation,
    noiseSuppression: a.noiseSuppression,
    autoGainControl: a.autoGain,
    ...(a.stereo ? { channelCount: 2 } : {}),
    ...(deviceId ? { deviceId: { exact: deviceId } } : {}),
  };
}

/** 내 Opus 인코더 설정을 담을 fmtp 값. 받는 쪽이 원하는 것을 적는 자리라, 상대의 설명에 넣어 적용하면 내 인코더가 따릅니다. */
export function opusFmtp(a: AudioPrefs): Record<string, string> {
  const out: Record<string, string> = {
    usedtx: a.dtx ? '1' : '0',
    useinbandfec: a.fec ? '1' : '0',
    stereo: a.stereo ? '1' : '0',
    'sprop-stereo': a.stereo ? '1' : '0',
    ptime: String(a.ptime),
    minptime: String(Math.min(10, a.ptime)),
  };
  if (a.bitrateKbps != null) out.maxaveragebitrate = String(a.bitrateKbps * 1000);
  return out;
}

/** SDP의 모든 Opus fmtp에 값을 넣습니다(줄이 없으면 만듦). */
export function setOpusFmtp(sdp: string, want: Record<string, string>): string {
  const pts = [...sdp.matchAll(/^a=rtpmap:(\d+) opus\/48000(?:\/\d+)?\r?$/gim)].map((m) => m[1]);
  if (pts.length === 0) return sdp;
  const eol = sdp.includes('\r\n') ? '\r\n' : '\n';
  const lines = sdp.split(/\r?\n/);
  const seen = new Set<string>();
  for (let i = 0; i < lines.length; i++) {
    const m = /^a=fmtp:(\d+) (.*)$/.exec(lines[i]);
    if (m && pts.includes(m[1])) {
      lines[i] = `a=fmtp:${m[1]} ${setFmtpParams(m[2], want)}`;
      seen.add(m[1]);
    }
  }
  for (const pt of pts) {
    if (seen.has(pt)) continue;
    const idx = lines.findIndex((l) => new RegExp(`^a=rtpmap:${pt} opus/48000`, 'i').test(l));
    if (idx >= 0) lines.splice(idx + 1, 0, `a=fmtp:${pt} ${setFmtpParams('', want)}`);
  }
  return lines.join(eol);
}

/** 메시의 화면 소리 m-line(둘째 오디오)에 넣을 Opus 값: 마이크 설정 대신 음악·게임 소리용(스테레오, 128Kbps, DTX 끔). */
const SCREEN_AUDIO_FMTP: Record<string, string> = { stereo: '1', 'sprop-stereo': '1', maxaveragebitrate: '128000', usedtx: '0' };

/** 둘째 오디오 m-section(화면 소리)의 Opus fmtp만 음악용으로 바꿉니다. 없으면 그대로. */
export function setScreenAudioFmtp(sdp: string): string {
  const eol = sdp.includes('\r\n') ? '\r\n' : '\n';
  const sections = sdp.split(/\r?\n(?=m=)/);
  const audio = sections.flatMap((s, i) => (s.startsWith('m=audio') ? [i] : []));
  if (audio.length < 2) return sdp;
  sections[audio[1]] = setOpusFmtp(sections[audio[1]], SCREEN_AUDIO_FMTP);
  return sections.join(eol);
}

/**
 * 영상 코덱 협상 순서: 고른 코덱을 먼저, 그다음 VP8(모든 참가자가 지원)과 재전송·오류 정정. 프레임 암호화와 맞지 않는
 * 코덱(AV1·H.264 등)은 뺍니다. 실제로 보낼 코덱은 협상 뒤 송신기의 `encodings[].codec`으로 고릅니다.
 */
export function videoCodecOrder(codecs: RTCRtpCodec[], prefer: ScreenCodec): RTCRtpCodec[] {
  const mime = (c: RTCRtpCodec) => c.mimeType.toLowerCase();
  const isAux = (m: string) => m === 'video/rtx' || m === 'video/red' || m === 'video/ulpfec' || m === 'video/flexfec-03';
  const vp8 = codecs.filter((c) => mime(c) === 'video/vp8');
  if (vp8.length === 0) return codecs;
  // VP9는 프로필 0(8비트)만: 다른 프로필은 하드웨어 디코더가 없는 기기가 많습니다.
  const vp9 = codecs.filter((c) => mime(c) === 'video/vp9' && !/profile-id=[123]/.test(c.sdpFmtpLine ?? ''));
  const main = prefer === 'vp9' ? [...vp9, ...vp8] : [...vp8, ...vp9];
  return [...main, ...codecs.filter((c) => isAux(mime(c)))];
}

/** 협상된 코덱 중 보낼 것(없으면 undefined: 브라우저 기본). */
export function pickSendCodec(negotiated: RTCRtpCodec[] | undefined, prefer: ScreenCodec): RTCRtpCodec | undefined {
  return negotiated?.find((c) => c.mimeType.toLowerCase() === `video/${prefer}`);
}
