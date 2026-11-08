// 음성·화면공유 세부 설정: 읽어 들인 값 다듬기, 화면 축소 배율, Opus fmtp, 영상 코덱 순서.

import { describe, expect, it } from 'vitest';
import {
  DEFAULT_MEDIA,
  DEFAULT_NATIVE,
  micConstraints,
  nativeMaxHeight,
  nativeSettings,
  opusFmtp,
  pickSendCodec,
  sanitizeMedia,
  screenDegradation,
  screenFps,
  screenScale,
  setOpusFmtp,
  setScreenAudioFmtp,
  videoCodecOrder,
} from './media';

const codec = (mimeType: string, sdpFmtpLine?: string) => ({ mimeType, clockRate: 90000, ...(sdpFmtpLine ? { sdpFmtpLine } : {}) }) as RTCRtpCodec;

describe('sanitizeMedia', () => {
  it('모르는 값은 기본값, 범위 밖 숫자는 범위 안으로', () => {
    expect(sanitizeMedia(undefined)).toEqual(DEFAULT_MEDIA);
    const m = sanitizeMedia({
      screen: { codec: 'av1', resolution: 999, fps: 60, bitrateKbps: 99_999, degradation: 'x', audioVolume: 999 },
      audio: { bitrateKbps: 1, stereo: true, fec: 'yes', ptime: 60 },
    });
    expect(m.screen).toEqual({
      encoder: 'webrtc',
      codec: 'vp8',
      resolution: 'source',
      fps: 60,
      bitrateKbps: 20_000,
      degradation: 'auto',
      audio: true,
      audioVolume: 200,
      native: DEFAULT_NATIVE,
    });
    expect(sanitizeMedia({ screen: { audioVolume: 'x' } }).screen.audioVolume).toBe(100);
    expect(m.audio).toMatchObject({ bitrateKbps: 6, stereo: true, fec: true, ptime: 60, dtx: true });
  });

  it('자동 음량은 기본 켬이고, 기본 꺼짐 시절 저장된 값은 한 번 무시한다', () => {
    expect(DEFAULT_MEDIA.audio.autoGain).toBe(true);
    expect(sanitizeMedia({ audio: { autoGainControl: false } }).audio.autoGain).toBe(true);
    expect(sanitizeMedia({ audio: { autoGain: false } }).audio.autoGain).toBe(false);
    expect(micConstraints(sanitizeMedia({ audio: { autoGain: false } }).audio).autoGainControl).toBe(false);
  });

  it('ffmpeg 인코더 설정도 아는 값·범위로', () => {
    const m = sanitizeMedia({
      screen: {
        encoder: 'nvenc-hevc',
        native: { rateControl: 'cq', cq: 99, keyintSec: 0, cursor: false, nvenc: { preset: 9, tune: 'ull', lookahead: 100 }, aom: { cpuUsed: 2 }, svt: { preset: 12 } },
      },
    });
    expect(m.screen.encoder).toBe('nvenc-hevc');
    expect(m.screen.native).toMatchObject({ rateControl: 'cq', cq: 63, keyintSec: 1, cursor: false });
    expect(m.screen.native.nvenc).toMatchObject({ preset: 4, tune: 'ull', lookahead: 32, multipass: 'disabled', profile: 'high' });
    expect(m.screen.native.aom.cpuUsed).toBe(8);
    expect(m.screen.native.svt.preset).toBe(12);
    expect(sanitizeMedia({ screen: { encoder: 'x264' } }).screen.encoder).toBe('webrtc');
  });

  it('ffmpeg 인코더에 넘길 값: 비트레이트·프레임은 화면 종류 기본값, 해상도는 세로 상한', () => {
    const s = { ...DEFAULT_MEDIA.screen, encoder: 'svt-av1' as const };
    expect(nativeSettings(s, 'svt-av1', 'motion')).toMatchObject({ encoder: 'svt-av1', fps: 60, bitrateKbps: 4000, rateControl: 'cbr', keyintSec: 2 });
    expect(nativeSettings({ ...s, bitrateKbps: 8000, fps: 30 }, 'svt-av1', 'detail')).toMatchObject({ fps: 30, bitrateKbps: 8000 });
    expect(nativeMaxHeight(s)).toBeNull();
    expect(nativeMaxHeight({ ...s, resolution: 1080 })).toBe(1080);
  });
});

describe('화면 설정 값', () => {
  it('자동이면 화면 종류에 맞춘 값, 정했으면 그 값', () => {
    expect(screenFps(DEFAULT_MEDIA.screen, 'detail')).toBe(30);
    expect(screenFps(DEFAULT_MEDIA.screen, 'motion')).toBe(60);
    expect(screenFps({ ...DEFAULT_MEDIA.screen, fps: 15 }, 'motion')).toBe(15);
    expect(screenDegradation(DEFAULT_MEDIA.screen, 'detail')).toBe('balanced');
    expect(screenDegradation({ ...DEFAULT_MEDIA.screen, degradation: 'maintain-resolution' }, 'motion')).toBe('maintain-resolution');
  });

  it('출력 해상도는 줄이기만 한다', () => {
    const s = (resolution: 720 | 'source') => ({ ...DEFAULT_MEDIA.screen, resolution });
    expect(screenScale(s('source'), 2160)).toBe(1);
    expect(screenScale(s(720), 1080)).toBe(1.5);
    expect(screenScale(s(720), 600)).toBe(1);
    expect(screenScale(s(720), undefined)).toBe(1);
  });
});

describe('Opus fmtp', () => {
  it('설정을 fmtp 값으로(비트레이트는 정했을 때만)', () => {
    expect(opusFmtp(DEFAULT_MEDIA.audio)).toEqual({ usedtx: '1', useinbandfec: '1', stereo: '0', 'sprop-stereo': '0', ptime: '20', minptime: '10' });
    expect(opusFmtp({ ...DEFAULT_MEDIA.audio, bitrateKbps: 64, stereo: true, dtx: false, ptime: 60 })).toMatchObject({
      maxaveragebitrate: '64000',
      stereo: '1',
      usedtx: '0',
      ptime: '60',
    });
  });

  it('있는 fmtp 줄은 고치고, 없으면 rtpmap 뒤에 만든다', () => {
    const sdp = ['m=audio 9 UDP/TLS/RTP/SAVPF 111 63', 'a=rtpmap:111 opus/48000/2', 'a=fmtp:111 minptime=10;useinbandfec=1', 'a=rtpmap:63 red/48000/2', ''].join('\r\n');
    const out = setOpusFmtp(sdp, { usedtx: '0', useinbandfec: '0' });
    expect(out).toContain('a=fmtp:111 minptime=10;useinbandfec=0;usedtx=0');
    const bare = setOpusFmtp('a=rtpmap:109 opus/48000/2\r\n', { stereo: '1' });
    expect(bare).toBe('a=rtpmap:109 opus/48000/2\r\na=fmtp:109 stereo=1\r\n');
  });

  it('화면 소리(둘째 오디오 m-line)만 음악용 값으로 바꾼다', () => {
    const audio = (mid: number) => [`m=audio 9 UDP/TLS/RTP/SAVPF 111`, `a=mid:${mid}`, 'a=rtpmap:111 opus/48000/2', 'a=fmtp:111 stereo=0'];
    const sdp = ['v=0', ...audio(0), 'm=video 9 UDP/TLS/RTP/SAVPF 96', 'a=mid:1', ...audio(2), ''].join('\r\n');
    const out = setScreenAudioFmtp(sdp).split('m=');
    expect(out[1]).toContain('a=fmtp:111 stereo=0\r\n');
    expect(out[3]).toContain('a=fmtp:111 stereo=1;sprop-stereo=1;maxaveragebitrate=128000;usedtx=0');
    // 오디오가 하나면 그대로.
    const one = ['v=0', ...audio(0), ''].join('\r\n');
    expect(setScreenAudioFmtp(one)).toBe(one);
  });
});

describe('영상 코덱', () => {
  const caps = [codec('video/VP8'), codec('video/rtx'), codec('video/VP9', 'profile-id=0'), codec('video/VP9', 'profile-id=2'), codec('video/H264'), codec('video/AV1')];

  it('고른 코덱을 먼저, VP8은 늘 함께, 암호화와 맞지 않는 코덱과 VP9 고급 프로필은 뺀다', () => {
    expect(videoCodecOrder(caps, 'vp9').map((c) => `${c.mimeType} ${c.sdpFmtpLine ?? ''}`.trim())).toEqual([
      'video/VP9 profile-id=0',
      'video/VP8',
      'video/rtx',
    ]);
    expect(videoCodecOrder(caps, 'vp8').map((c) => c.mimeType)).toEqual(['video/VP8', 'video/VP9', 'video/rtx']);
  });

  it('보낼 코덱은 협상된 것 중에서만', () => {
    expect(pickSendCodec([codec('video/VP8'), codec('video/VP9')], 'vp9')?.mimeType).toBe('video/VP9');
    expect(pickSendCodec([codec('video/VP8')], 'vp9')).toBeUndefined();
    expect(pickSendCodec(undefined, 'vp8')).toBeUndefined();
  });
});
