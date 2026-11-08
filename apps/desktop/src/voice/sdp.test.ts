import { describe, expect, it } from 'vitest';
import { enableOpusDtx, onlyVp8, opusPayloadTypes, setFmtpParams } from './sdp';

const SDP = [
  'v=0',
  'm=audio 9 UDP/TLS/RTP/SAVPF 111 63',
  'a=rtpmap:111 opus/48000/2',
  'a=fmtp:111 minptime=10;useinbandfec=0',
  'a=rtpmap:63 red/48000/2',
  'a=fmtp:63 111/111',
  '',
].join('\r\n');

describe('Opus DTX', () => {
  it('기존 fmtp에 usedtx=1을 넣고 FEC를 켠다', () => {
    const out = enableOpusDtx(SDP);
    expect(out).toContain('a=fmtp:111 minptime=10;useinbandfec=1;usedtx=1');
    expect(out).toContain('a=fmtp:63 111/111');
    expect(out.split('\r\n').length).toBe(SDP.split('\r\n').length);
  });

  it('fmtp 줄이 없으면 만든다(줄바꿈 유지)', () => {
    const sdp = 'v=0\nm=audio 9 RTP 109\na=rtpmap:109 opus/48000/2\na=sendrecv\n';
    const out = enableOpusDtx(sdp);
    expect(out).toBe('v=0\nm=audio 9 RTP 109\na=rtpmap:109 opus/48000/2\na=fmtp:109 usedtx=1;useinbandfec=1\na=sendrecv\n');
    expect(opusPayloadTypes(out)).toEqual(['109']);
  });

  it('Opus가 없으면 그대로', () => {
    expect(enableOpusDtx('v=0\r\nm=video 9 RTP 96\r\n')).toBe('v=0\r\nm=video 9 RTP 96\r\n');
    expect(setFmtpParams('a=1; b ;c=3', { B: '2' })).toBe('a=1;b=2;c=3');
  });
});

describe('영상 코덱', () => {
  const c = (mimeType: string) => ({ mimeType, clockRate: 90000 }) as RTCRtpCodec;
  it('VP8과 보조 코덱만 남긴다', () => {
    const out = onlyVp8([c('video/H264'), c('video/VP8'), c('video/rtx'), c('video/VP9'), c('video/red')]);
    expect(out.map((x) => x.mimeType)).toEqual(['video/VP8', 'video/rtx', 'video/red']);
  });
  it('VP8이 없으면 원래 목록', () => {
    const list = [c('video/H264')];
    expect(onlyVp8(list)).toBe(list);
  });
});
