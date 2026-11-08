// ffmpeg 인코더 화면공유의 웹뷰 쪽: 프레임·조각 모양, 코덱 문자열, Rust 메시지 읽기, 보내기(시청자·버퍼·키 프레임)와 받기.

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type { ScreenTransport } from '../api/client';
import { DEFAULT_MEDIA, nativeSettings } from './media';
import {
  MAX_PARTS,
  NativeScreen,
  PART_BYTES,
  SCREEN_CHANNEL,
  codecString,
  decodeFrame,
  decoderConfig,
  encodeFrame,
  encoderCodec,
  parseEncoderMessage,
  parsePart,
  splitParts,
} from './native-screen';

const cfg = { codec: 'h264' as const, width: 1920, height: 1080, record: new Uint8Array([1, 0x64, 0, 0x28, 0xff]) };

describe('프레임·조각', () => {
  it('설정이 붙은 키 프레임과 델타 프레임을 그대로 오간다', () => {
    const key = decodeFrame(encodeFrame({ key: true, codec: 'h264', config: cfg, data: new Uint8Array([9, 8]) }))!;
    expect(key.key).toBe(true);
    expect(key.config).toEqual(cfg);
    expect([...key.data]).toEqual([9, 8]);
    const delta = decodeFrame(encodeFrame({ key: false, codec: 'hevc', data: new Uint8Array([7]) }))!;
    expect(delta).toMatchObject({ key: false, codec: 'hevc' });
    expect(delta.config).toBeUndefined();
    expect(decodeFrame(new Uint8Array([2, 9]))).toBeNull();
    expect(decodeFrame(new Uint8Array([2, 1, 50, 0, 0, 0, 0, 0]))).toBeNull();
  });

  it('큰 프레임을 16KiB 안 조각으로 나누고, 잘못된 조각은 버린다', () => {
    const bytes = new Uint8Array(PART_BYTES * 2 + 3).map((_, i) => i & 0xff);
    const parts = splitParts(0xfffffffe, bytes)!;
    expect(parts).toHaveLength(3);
    expect(parts.every((p) => p.length <= 16 * 1024)).toBe(true);
    const p = parsePart(parts[2])!;
    expect(p).toMatchObject({ frame: 0xfffffffe, part: 2, parts: 3 });
    expect(p.data.length).toBe(3);
    expect(splitParts(1, new Uint8Array(PART_BYTES * MAX_PARTS + 1))).toBeNull();
    const bad = parts[0].slice();
    bad[5] = 9; // 조각 번호 ≥ 조각 수
    expect(parsePart(bad)).toBeNull();
    expect(parsePart(new Uint8Array([2, 1]))).toBeNull();
  });
});

describe('코덱 문자열', () => {
  it('avcC·hvcC·av1C에서 WebCodecs 문자열을 만든다', () => {
    expect(codecString('h264', cfg.record)).toBe('avc1.640028');
    // HEVC Main, 호환 플래그 0x60000000(비트를 뒤집으면 6), 제약 0x90, Level 4.0(120).
    const hvcc = new Uint8Array([1, 0x01, 0x60, 0, 0, 0, 0x90, 0, 0, 0, 0, 0, 120, 0, 0]);
    expect(codecString('hevc', hvcc)).toBe('hvc1.1.6.L120.90');
    // AV1 Main, level 8(4.0), Main tier, 8비트.
    expect(codecString('av1', new Uint8Array([0x81, 0x08, 0x0c, 0]))).toBe('av01.0.08M.08');
    // High tier·10비트.
    expect(codecString('av1', new Uint8Array([0x81, 0x0d, 0xc0, 0]))).toBe('av01.0.13H.10');
    expect(codecString('h264', new Uint8Array([0, 1, 2, 3]))).toBeNull();
  });

  it('H.264·HEVC만 설명(레코드)을 붙인다', () => {
    expect(decoderConfig(cfg)).toMatchObject({ codec: 'avc1.640028', description: cfg.record, codedWidth: 1920 });
    expect(decoderConfig({ codec: 'av1', width: 2, height: 2, record: new Uint8Array([0x81, 0, 0, 0]) })).not.toHaveProperty('description');
  });

  it('인코더 → 코덱', () => {
    expect(encoderCodec('aom-av1')).toBe('av1');
    expect(encoderCodec('svt-av1')).toBe('av1');
    expect(encoderCodec('nvenc-av1')).toBe('av1');
    expect(encoderCodec('nvenc-h264')).toBe('h264');
    expect(encoderCodec('nvenc-hevc')).toBe('hevc');
  });
});

describe('Rust 쪽 메시지', () => {
  it('설정·프레임·끝을 읽는다', () => {
    expect(parseEncoderMessage(new Uint8Array([1, 3, 0x80, 0x07, 0x38, 0x04, 9]))).toMatchObject({ type: 'config', codec: 'hevc', width: 1920, height: 1080 });
    expect(parseEncoderMessage(new Uint8Array([2, 1, 2, 1, 0, 0, 7]))).toMatchObject({ type: 'frame', key: true, ts: 258 });
    expect(parseEncoderMessage(new Uint8Array([3, 0x78]))).toEqual({ type: 'end', reason: 'x' });
    expect(parseEncoderMessage(new Uint8Array([1, 9, 0, 0, 0, 0]))).toBeNull();
    expect(parseEncoderMessage(new Uint8Array([7]))).toBeNull();
  });
});

// ───────────── 보내기·받기 ─────────────

class FakeDataChannel {
  readyState: RTCDataChannelState = 'connecting';
  binaryType = 'blob';
  bufferedAmount = 0;
  bufferedAmountLowThreshold = 0;
  sent: Uint8Array[] = [];
  onopen: (() => void) | null = null;
  onclose: (() => void) | null = null;
  onmessage: ((e: { data: unknown }) => void) | null = null;
  constructor(
    readonly label: string,
    readonly init?: RTCDataChannelInit,
  ) {}
  send(d: Uint8Array) {
    this.sent.push(d);
  }
  close() {
    this.readyState = 'closed';
  }
  open() {
    this.readyState = 'open';
    this.onopen?.();
  }
}

class FakePC {
  channels: FakeDataChannel[] = [];
  createDataChannel(label: string, init?: RTCDataChannelInit) {
    const dc = new FakeDataChannel(label, init);
    this.channels.push(dc);
    return dc;
  }
}

/** 봉인·열기는 그대로 돌려줍니다(암호는 frame.test가 봄). */
function fakeWorker(target: () => NativeScreen) {
  return {
    postMessage(msg: { type: string; id: number; data: ArrayBuffer }) {
      queueMicrotask(() => target().onWorkerData({ type: 'data', id: msg.id, data: msg.data }));
    },
  } as unknown as Worker;
}

class FakeTrack {
  stopped = false;
  contentHint = '';
  stop() {
    this.stopped = true;
  }
}

class FakeGenerator extends FakeTrack {
  frames: unknown[] = [];
  writable = { getWriter: () => ({ desiredSize: 1, write: async (f: unknown) => void this.frames.push(f), close: async () => undefined }) };
}

class FakeMediaStream {
  constructor(readonly tracks: FakeTrack[] = []) {}
  getVideoTracks() {
    return this.tracks;
  }
  getTracks() {
    return this.tracks;
  }
}

class FakeDecoder {
  static all: FakeDecoder[] = [];
  state = 'unconfigured';
  decodeQueueSize = 0;
  config?: VideoDecoderConfig;
  chunks: Array<{ type: string; timestamp: number }> = [];
  constructor(readonly init: { output: (f: unknown) => void; error: (e: unknown) => void }) {
    FakeDecoder.all.push(this);
  }
  /** 하드웨어 디코더가 실제로 실패한 것처럼. */
  fail() {
    this.state = 'closed';
    this.init.error(new Error('decode'));
  }
  configure(c: VideoDecoderConfig) {
    this.config = c;
    this.state = 'configured';
  }
  decode(c: { type: string; timestamp: number }) {
    this.chunks.push(c);
    this.init.output({ close() {} });
  }
  close() {
    this.state = 'closed';
  }
  static async isConfigSupported(c: { codec: string }) {
    // 이 웹뷰는 HEVC를 풀지 못한다고 칩니다.
    return { supported: !c.codec.startsWith('hvc1') };
  }
}

class FakeChunk {
  type: string;
  timestamp: number;
  constructor(init: { type: string; timestamp: number }) {
    this.type = init.type;
    this.timestamp = init.timestamp;
  }
}

function fakeTransport() {
  let onData: ((m: Uint8Array) => void) | null = null;
  const t = {
    starts: 0,
    stops: [] as Array<number | undefined>,
    status: vi.fn(),
    sources: vi.fn(),
    start: vi.fn(async (_src: unknown, _h: unknown, _s: unknown, cb: (m: Uint8Array) => void) => {
      onData = cb;
      t.starts++;
      return t.starts;
    }),
    stop: vi.fn(async (id?: number) => void t.stops.push(id)),
    emit: (m: number[] | Uint8Array) => onData?.(m instanceof Uint8Array ? m : new Uint8Array(m)),
  };
  return t;
}

const flush = async () => {
  for (let i = 0; i < 10; i++) await new Promise((r) => setTimeout(r, 0));
};

function configMsg() {
  return new Uint8Array([1, 2, 0x80, 0x07, 0x38, 0x04, ...cfg.record]);
}
function frameMsg(key: boolean, data: number[]) {
  return new Uint8Array([2, key ? 1 : 0, 0, 0, 0, 0, ...data]);
}

describe('NativeScreen', () => {
  beforeEach(() => {
    FakeDecoder.all = [];
    vi.stubGlobal('VideoDecoder', FakeDecoder);
    vi.stubGlobal('EncodedVideoChunk', FakeChunk);
    vi.stubGlobal('MediaStreamTrackGenerator', FakeGenerator);
    vi.stubGlobal('MediaStream', FakeMediaStream);
  });
  afterEach(() => {
    vi.unstubAllGlobals();
  });

  function make(myDevice: string, watching: Set<string> | null = null) {
    const ended: string[] = [];
    const caps: string[] = [];
    const remote: string[] = [];
    // eslint-disable-next-line prefer-const
    let ns: NativeScreen;
    ns = new NativeScreen({
      worker: fakeWorker(() => ns),
      myDevice,
      onRemoteScreen: (d) => remote.push(d),
      isWatching: (d) => !watching || watching.has(d),
      onCapsChanged: (d) => caps.push(d),
      onLocalEnded: (r) => ended.push(r),
    });
    return { ns, ended, caps, remote };
  }

  it('그 코덱을 풀 수 있다고 알린 시청자에게만, 키 프레임부터 보낸다', async () => {
    const a = make('A');
    await flush();
    const pcB = new FakePC();
    const pcC = new FakePC();
    a.ns.attach('B', pcB as unknown as RTCPeerConnection);
    a.ns.attach('C', pcC as unknown as RTCPeerConnection);
    const dcB = pcB.channels[0];
    const dcC = pcC.channels[0];
    expect(dcB.label).toBe(SCREEN_CHANNEL.label);
    expect(dcB.init).toMatchObject({ negotiated: true, id: SCREEN_CHANNEL.id, ordered: false });
    dcB.open();
    dcC.open();
    await flush();
    // 열리면 hello(풀 수 있는 코덱: AV1·H.264 = 3)를 보냅니다.
    expect([...dcB.sent[0]]).toEqual([2, 3]);
    // B는 H.264를 풀 수 있고 C는 HEVC만.
    dcB.onmessage?.({ data: new Uint8Array([2, 2]).buffer });
    dcC.onmessage?.({ data: new Uint8Array([2, 4]).buffer });
    await flush();
    expect(a.caps).toEqual(['B', 'C']);

    const t = fakeTransport();
    const track = await a.ns.start(t as unknown as ScreenTransport, { kind: 'monitor', id: '1' }, null, nativeSettings({ ...DEFAULT_MEDIA.screen }, 'nvenc-h264', 'detail'));
    expect(track).toBeInstanceOf(FakeGenerator);
    expect(a.ns.receivesNative('B')).toBe(true);
    expect(a.ns.receivesNative('C')).toBe(false);
    a.ns.setViewers(['B', 'C']);
    const before = dcB.sent.length;
    t.emit(configMsg());
    // 첫 프레임이 델타면 보내지 않고(키 프레임 전), 키 프레임부터.
    t.emit(frameMsg(false, [1]));
    t.emit(frameMsg(true, [2, 2]));
    t.emit(frameMsg(false, [3]));
    await flush();
    const sentB = dcB.sent.slice(before).map((b) => parsePart(b)!);
    expect(sentB.map((p) => p.frame)).toEqual([1, 2]);
    const keyFrame = decodeFrame(sentB[0].data)!;
    expect(keyFrame.key).toBe(true);
    expect(keyFrame.config?.record).toEqual(cfg.record);
    expect(decodeFrame(sentB[1].data)?.config).toBeUndefined();
    // HEVC만 푸는 C에게는 데이터 채널로 보내지 않습니다(세션이 WebRTC로).
    expect(dcC.sent.every((b) => b[0] === 2)).toBe(true);
    // 이 기기 미리보기도 같은 프레임을 풉니다(첫 델타는 키 프레임 전이라 건너뜀).
    expect(FakeDecoder.all[0].config?.codec).toBe('avc1.640028');
    expect(FakeDecoder.all[0].chunks.map((c) => c.type)).toEqual(['key', 'delta']);

    // 버퍼가 차면 그 상대에게는 다음 키 프레임까지 보내지 않습니다.
    dcB.bufferedAmount = 10 * 1024 * 1024;
    t.emit(frameMsg(false, [4]));
    await flush();
    dcB.bufferedAmount = 0;
    const mark = dcB.sent.length;
    t.emit(frameMsg(false, [5]));
    t.emit(frameMsg(true, [6]));
    await flush();
    // 프레임 번호는 버린 프레임도 세므로 받는 쪽이 빈 곳을 압니다(4번째 델타 = 프레임 4는 건너뜀, 키 프레임 = 5).
    expect(dcB.sent.slice(mark).map((b) => parsePart(b)!.frame)).toEqual([5]);

    // 인코더가 스스로 끝나면 알리고 트랙을 멈춥니다.
    t.emit(new Uint8Array([3, ...new TextEncoder().encode('창이 닫혔습니다')]));
    expect(a.ended).toEqual(['창이 닫혔습니다']);
    expect((track as unknown as FakeTrack).stopped).toBe(true);
    expect(a.ns.sharing).toBe(false);
  });

  it('받은 조각을 모아 키 프레임부터 풀고 화면을 알린다', async () => {
    const b = make('B');
    await flush();
    const pc = new FakePC();
    b.ns.attach('A', pc as unknown as RTCPeerConnection);
    const dc = pc.channels[0];
    dc.open();
    const send = (bytes: Uint8Array) => dc.onmessage?.({ data: bytes.slice().buffer });
    const big = new Uint8Array(PART_BYTES + 10).fill(5);
    // 키 프레임 전 델타는 버립니다.
    for (const p of splitParts(0, encodeFrame({ key: false, codec: 'h264', data: new Uint8Array([1]) }))!) send(p);
    for (const p of splitParts(1, encodeFrame({ key: true, codec: 'h264', config: cfg, data: big }))!.reverse()) send(p);
    for (const p of splitParts(2, encodeFrame({ key: false, codec: 'h264', data: new Uint8Array([2]) }))!) send(p);
    // 프레임 3을 놓치고 4가 오면 다음 키 프레임까지 기다립니다.
    for (const p of splitParts(4, encodeFrame({ key: false, codec: 'h264', data: new Uint8Array([4]) }))!) send(p);
    await flush();
    expect(b.remote).toEqual(['A']);
    expect(b.ns.remoteScreen('A')).toBeInstanceOf(FakeMediaStream);
    const dec = FakeDecoder.all[0];
    expect(dec.config).toMatchObject({ codec: 'avc1.640028', description: cfg.record });
    expect(dec.chunks.map((c) => [c.type, c.timestamp])).toEqual([
      ['key', 1000],
      ['delta', 2000],
    ]);
    // 풀지 못하는 코덱(HEVC)은 넣지 않습니다.
    for (const p of splitParts(5, encodeFrame({ key: true, codec: 'hevc', config: { ...cfg, codec: 'hevc' }, data: new Uint8Array([1]) }))!) send(p);
    await flush();
    expect(FakeDecoder.all).toHaveLength(1);
    b.ns.forget('A');
    expect(b.ns.remoteScreen('A')).toBeUndefined();
  });

  it("'보기'를 누르지 않은 화면의 조각은 풀지 않고, 풀다가 실패한 코덱은 hello에서 뺀다", async () => {
    const watching = new Set<string>();
    const b = make('B', watching);
    await flush();
    const pc = new FakePC();
    b.ns.attach('A', pc as unknown as RTCPeerConnection);
    const dc = pc.channels[0];
    dc.open();
    await flush();
    const send = (bytes: Uint8Array) => dc.onmessage?.({ data: bytes.slice().buffer });
    const key = (n: number) => splitParts(n, encodeFrame({ key: true, codec: 'h264', config: cfg, data: new Uint8Array(200).fill(n) }))!;
    for (const p of key(1)) send(p);
    await flush();
    expect(FakeDecoder.all).toHaveLength(0);
    expect(b.remote).toEqual([]);

    watching.add('A');
    for (const p of key(2)) send(p);
    await flush();
    expect(b.remote).toEqual(['A']);
    // 디코더가 실패하면 받던 화면을 버리고, H.264를 뺀 hello(AV1만 = 1)를 다시 보냅니다.
    const mark = dc.sent.length;
    FakeDecoder.all[0].fail();
    await flush();
    expect(b.ns.remoteScreen('A')).toBeUndefined();
    expect(dc.sent.slice(mark).map((x) => [...x])).toEqual([[2, 1]]);
    await expect(b.ns.canDecode('nvenc-h264')).resolves.toBe(false);
    await expect(b.ns.canDecode('aom-av1')).resolves.toBe(true);
  });

  it('봉인하는 사이 그만 본 시청자에게는 보내지 않는다', async () => {
    const a = make('A');
    await flush();
    const pc = new FakePC();
    a.ns.attach('B', pc as unknown as RTCPeerConnection);
    const dc = pc.channels[0];
    dc.open();
    dc.onmessage?.({ data: new Uint8Array([2, 2]).buffer });
    await flush();
    const t = fakeTransport();
    await a.ns.start(t as unknown as ScreenTransport, { kind: 'monitor', id: '1' }, null, nativeSettings({ ...DEFAULT_MEDIA.screen }, 'nvenc-h264', 'detail'));
    a.ns.setViewers(['B']);
    t.emit(configMsg());
    const mark = dc.sent.length;
    t.emit(frameMsg(true, [1]));
    a.ns.setViewers([]);
    await flush();
    expect(dc.sent.length).toBe(mark);
  });

  it('이 기기가 풀지 못하는 코덱으로 바꾸면 공유를 멈춘다', async () => {
    const a = make('A');
    const t = fakeTransport();
    const s = nativeSettings({ ...DEFAULT_MEDIA.screen }, 'svt-av1', 'detail');
    await a.ns.start(t as unknown as ScreenTransport, { kind: 'window', id: '42' }, 1080, s);
    await expect(a.ns.canDecode('nvenc-hevc')).resolves.toBe(false);
    await a.ns.restart(1080, nativeSettings({ ...DEFAULT_MEDIA.screen }, 'nvenc-hevc', 'detail'));
    expect(t.start).toHaveBeenCalledTimes(1);
    expect(a.ns.sharing).toBe(false);
    expect(a.ended).toHaveLength(1);
  });

  it('설정을 바꾸면 트랙은 그대로 두고 인코더만 다시 띄운다', async () => {
    const a = make('A');
    const t = fakeTransport();
    const s = nativeSettings({ ...DEFAULT_MEDIA.screen }, 'svt-av1', 'detail');
    const track = await a.ns.start(t as unknown as ScreenTransport, { kind: 'window', id: '42' }, 1080, s);
    await a.ns.restart(720, { ...s, bitrateKbps: 9000 });
    expect(t.start).toHaveBeenCalledTimes(2);
    expect(t.start.mock.calls[1].slice(0, 3)).toEqual([{ kind: 'window', id: '42' }, 720, { ...s, bitrateKbps: 9000 }]);
    expect((track as unknown as FakeTrack).stopped).toBe(false);
    a.ns.stop();
    expect(t.stops).toEqual([2]);
    expect((track as unknown as FakeTrack).stopped).toBe(true);
  });
});
