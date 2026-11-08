// Tor 통화의 비상용 화면공유. WebRTC 없이, 화면을 낮은 해상도·초당 몇 프레임의 VP8(WebCodecs)로 만들어 조각으로 나눠
// 봉인한 뒤 '화면 보기'를 누른 사람에게만 데몬을 거쳐 Tor로 보냅니다(tor.ts가 봉인·전송). 받는 쪽은 조각을 모아 풀고
// 캔버스에 그려 스트림으로 보여 줍니다.
//
// 조각 본문: [프레임 번호 u32 LE][조각 번호 u8][조각 수 u8][플래그 u8: 1 키 프레임][폭 u16 LE][높이 u16 LE][VP8 바이트].
// 키 프레임은 10초마다, 그리고 새 시청자가 생기면 바로 보냅니다(중간에 들어온 시청자가 풀 수 있게).

import { t } from '../i18n';

const HEADER = 11;
/** 조각 하나의 VP8 바이트(봉인 여유를 두고 데몬 한도 15KiB 안). */
export const PART_BYTES = 12 * 1024;
const MAX_PARTS = 64;
export const SCREEN_FPS = 2;
const MAX_WIDTH = 1280;
const BITRATE = 300_000;
const KEY_EVERY = 10 * SCREEN_FPS;
/** 늦게 끝난 프레임을 이만큼만 기다립니다(그 뒤 프레임이 먼저 다 오면 버림). */
const KEEP_FRAMES = 8;

export interface ScreenPart {
  frame: number;
  part: number;
  parts: number;
  key: boolean;
  width: number;
  height: number;
  data: Uint8Array;
}

export function encodePart(p: ScreenPart): Uint8Array {
  const out = new Uint8Array(HEADER + p.data.length);
  const v = new DataView(out.buffer);
  v.setUint32(0, p.frame >>> 0, true);
  out[4] = p.part;
  out[5] = p.parts;
  out[6] = p.key ? 1 : 0;
  v.setUint16(7, p.width, true);
  v.setUint16(9, p.height, true);
  out.set(p.data, HEADER);
  return out;
}

export function decodePart(b: Uint8Array): ScreenPart | null {
  if (b.length <= HEADER) return null;
  const v = new DataView(b.buffer, b.byteOffset, b.byteLength);
  const parts = b[5];
  const part = b[4];
  if (parts === 0 || parts > MAX_PARTS || part >= parts) return null;
  return {
    frame: v.getUint32(0, true),
    part,
    parts,
    key: (b[6] & 1) !== 0,
    width: v.getUint16(7, true),
    height: v.getUint16(9, true),
    data: b.subarray(HEADER),
  };
}

/** 한 프레임을 조각으로. */
export function splitFrame(frame: number, key: boolean, width: number, height: number, data: Uint8Array): ScreenPart[] | null {
  const parts = Math.max(1, Math.ceil(data.length / PART_BYTES));
  if (parts > MAX_PARTS) return null;
  const out: ScreenPart[] = [];
  for (let i = 0; i < parts; i++) {
    out.push({ frame, part: i, parts, key, width, height, data: data.subarray(i * PART_BYTES, (i + 1) * PART_BYTES) });
  }
  return out;
}

/** 조각을 프레임으로 모읍니다. 다 모인 프레임을 번호 순으로 돌려주고, 더 새 프레임이 다 모이면 그 전 것은 버립니다. */
export class FrameAssembler {
  private readonly pending = new Map<number, { parts: (Uint8Array | undefined)[]; got: number; key: boolean }>();
  private last: number | undefined;
  private bytes = 0;

  /** maxBytes: 덜 모인 프레임에 쥐고 있을 바이트 상한(넘으면 오래된 프레임부터 버림). 상대가 조각을 다 보내지 않아도
   * 메모리가 그만큼만 듭니다. */
  constructor(private readonly maxBytes = Infinity) {}

  private drop(id: number) {
    const f = this.pending.get(id);
    if (!f) return;
    for (const x of f.parts) this.bytes -= x?.length ?? 0;
    this.pending.delete(id);
  }

  push(p: ScreenPart): { frame: number; key: boolean; data: Uint8Array } | null {
    if (this.last !== undefined && ((p.frame - this.last) >>> 0) > 0x8000_0000) return null;
    if (p.frame === this.last) return null;
    let f = this.pending.get(p.frame);
    if (!f) {
      f = { parts: new Array(p.parts), got: 0, key: p.key };
      this.pending.set(p.frame, f);
    }
    if (f.parts.length !== p.parts || f.parts[p.part]) return null;
    f.parts[p.part] = p.data.slice();
    f.got++;
    this.bytes += p.data.length;
    if (f.got < p.parts) {
      if (this.pending.size > KEEP_FRAMES) this.drop(Math.min(...this.pending.keys()));
      while (this.bytes > this.maxBytes && this.pending.size > 0) this.drop(Math.min(...this.pending.keys()));
      return null;
    }
    // 다 모임: 그보다 앞선 덜 모인 프레임은 버립니다.
    for (const id of [...this.pending.keys()]) if (((p.frame - id) >>> 0) < 0x8000_0000) this.drop(id);
    this.last = p.frame;
    const size = f.parts.reduce((n, x) => n + (x?.length ?? 0), 0);
    const data = new Uint8Array(size);
    let at = 0;
    for (const x of f.parts) {
      data.set(x!, at);
      at += x!.length;
    }
    return { frame: p.frame, key: f.key, data };
  }
}

interface Viewer {
  assembler: FrameAssembler;
  decoder?: VideoDecoder;
  /** 키 프레임을 받은 뒤에만 풉니다(중간 프레임부터 풀면 깨짐). */
  ready: boolean;
  last?: number;
  canvas: HTMLCanvasElement;
  stream?: MediaStream;
}

export interface TorScreenOptions {
  /** 조각 본문을 봉인해 그 기기들에게 보냅니다. */
  send(body: Uint8Array, to: string[]): void;
  /** 받은 화면 스트림(처음 그릴 때 한 번). */
  onRemoteScreen(device: string, stream: MediaStream): void;
}

/** 이 웹뷰가 WebCodecs VP8을 쓸 수 있는지. */
export function screenSupported(): boolean {
  return typeof VideoEncoder !== 'undefined' && typeof VideoDecoder !== 'undefined' && typeof VideoFrame !== 'undefined';
}

/** 화면 프레임을 받는 길. 창이 뒤로 가도(게임을 공유할 때 흔함) 늦어지지 않게 트랙에서 직접 받고(MediaStreamTrackProcessor),
 * 없으면 <video>를 타이머로 그립니다(가려진 창에서는 웹뷰가 타이머를 늦춤). */
type FrameSource = { kind: 'processor'; reader: ReadableStreamDefaultReader<VideoFrame> } | { kind: 'timer'; video: HTMLVideoElement; timer: ReturnType<typeof setInterval> };

declare const MediaStreamTrackProcessor: { new (init: { track: MediaStreamTrack }): { readable: ReadableStream<VideoFrame> } } | undefined;

export class TorScreen {
  private track: MediaStreamTrack | null = null;
  private source: FrameSource | null = null;
  private lastSentUs = -Infinity;
  private canvas: HTMLCanvasElement | null = null;
  private encoder: VideoEncoder | null = null;
  private size = { width: 0, height: 0 };
  private viewers: string[] = [];
  private frame = 0;
  private count = 0;
  private needKey = true;
  private readonly remotes = new Map<string, Viewer>();

  constructor(private readonly o: TorScreenOptions) {}

  // ───────────── 보내기 ─────────────

  async start(track: MediaStreamTrack) {
    this.stop();
    if (!screenSupported()) throw new Error(t('이 웹뷰는 WebCodecs 영상 인코딩을 지원하지 않습니다'));
    const ok = await VideoEncoder.isConfigSupported({ codec: 'vp8', width: MAX_WIDTH, height: 720, bitrate: BITRATE, framerate: SCREEN_FPS })
      .then((r) => !!r.supported)
      .catch(() => false);
    if (!ok) throw new Error(t('이 웹뷰는 VP8 영상 인코딩을 지원하지 않습니다'));
    this.track = track;
    this.canvas = document.createElement('canvas');
    this.needKey = true;
    this.lastSentUs = -Infinity;
    if (typeof MediaStreamTrackProcessor === 'function') {
      const reader = new MediaStreamTrackProcessor({ track }).readable.getReader();
      this.source = { kind: 'processor', reader };
      void this.pump(reader);
      return;
    }
    const video = document.createElement('video');
    video.muted = true;
    video.playsInline = true;
    video.srcObject = new MediaStream([track]);
    await video.play().catch(() => undefined);
    const timer = setInterval(() => {
      if (video.videoWidth) this.encodeFrom(video, video.videoWidth, video.videoHeight);
    }, 1000 / SCREEN_FPS);
    this.source = { kind: 'timer', video, timer };
  }

  /** 트랙의 프레임을 받아 초당 SCREEN_FPS개만 인코딩하고 나머지는 바로 놓습니다. */
  private async pump(reader: ReadableStreamDefaultReader<VideoFrame>) {
    for (;;) {
      let r: ReadableStreamReadResult<VideoFrame>;
      try {
        r = await reader.read();
      } catch {
        return;
      }
      if (r.done) return;
      const f = r.value;
      try {
        if (this.source?.kind === 'processor' && f.timestamp - this.lastSentUs >= 1e6 / SCREEN_FPS) {
          this.lastSentUs = f.timestamp;
          this.encodeFrom(f, f.displayWidth, f.displayHeight);
        }
      } finally {
        f.close();
      }
    }
  }

  setViewers(devices: string[]) {
    if (devices.some((d) => !this.viewers.includes(d))) this.needKey = true;
    this.viewers = [...devices];
  }

  get sharing(): boolean {
    return !!this.track;
  }

  private configure(width: number, height: number) {
    this.encoder?.close();
    this.encoder = new VideoEncoder({
      output: (chunk) => {
        const data = new Uint8Array(chunk.byteLength);
        chunk.copyTo(data);
        const parts = splitFrame(this.frame++, chunk.type === 'key', width, height, data);
        if (!parts || this.viewers.length === 0) return;
        for (const p of parts) this.o.send(encodePart(p), this.viewers);
      },
      error: () => {
        this.encoder = null;
        this.size = { width: 0, height: 0 };
      },
    });
    this.encoder.configure({ codec: 'vp8', width, height, bitrate: BITRATE, framerate: SCREEN_FPS, latencyMode: 'realtime' });
    this.size = { width, height };
    this.needKey = true;
  }

  /** 원본(트랙 프레임이나 <video>)을 줄여 그린 뒤 인코딩합니다. 보는 사람이 없으면 하지 않습니다. */
  private encodeFrom(src: CanvasImageSource, srcWidth: number, srcHeight: number) {
    const c = this.canvas;
    if (!c || this.viewers.length === 0 || !srcWidth || !srcHeight) return;
    const scale = Math.min(1, MAX_WIDTH / srcWidth);
    const width = Math.max(2, Math.round((srcWidth * scale) / 2) * 2);
    const height = Math.max(2, Math.round((srcHeight * scale) / 2) * 2);
    try {
      if (!this.encoder || this.size.width !== width || this.size.height !== height) this.configure(width, height);
      const enc = this.encoder;
      if (!enc || enc.encodeQueueSize > 2) return;
      c.width = width;
      c.height = height;
      c.getContext('2d')?.drawImage(src, 0, 0, width, height);
      const frame = new VideoFrame(c, { timestamp: Math.round(performance.now() * 1000) });
      const key = this.needKey || this.count % KEY_EVERY === 0;
      this.needKey = false;
      this.count++;
      enc.encode(frame, { keyFrame: key });
      frame.close();
    } catch {
      // 한 프레임 실패는 건너뜁니다
    }
  }

  stop() {
    const s = this.source;
    this.source = null;
    if (s?.kind === 'timer') {
      clearInterval(s.timer);
      s.video.srcObject = null;
    } else if (s?.kind === 'processor') {
      void s.reader.cancel().catch(() => undefined);
    }
    try {
      this.encoder?.close();
    } catch {
      // 이미 닫힘
    }
    this.encoder = null;
    this.size = { width: 0, height: 0 };
    this.canvas = null;
    this.track = null;
  }

  // ───────────── 받기 ─────────────

  remoteScreen(device: string): MediaStream | undefined {
    return this.remotes.get(device)?.stream;
  }

  onPart(from: string, body: Uint8Array) {
    const p = decodePart(body);
    if (!p || !screenSupported()) return;
    let r = this.remotes.get(from);
    if (!r) {
      r = { assembler: new FrameAssembler(), ready: false, canvas: document.createElement('canvas') };
      this.remotes.set(from, r);
    }
    const f = r.assembler.push(p);
    if (!f) return;
    // 키 프레임 전이거나 앞 프레임을 놓쳤으면 다음 키 프레임까지 기다립니다.
    const follows = r.last !== undefined && f.frame === ((r.last + 1) >>> 0);
    if (!f.key && (!r.ready || !follows)) {
      r.ready = false;
      return;
    }
    r.last = f.frame;
    try {
      if (!r.decoder) r.decoder = this.decoder(from, r);
      r.decoder.decode(new EncodedVideoChunk({ type: f.key ? 'key' : 'delta', timestamp: f.frame * 1000, data: f.data }));
      r.ready = true;
    } catch {
      r.ready = false;
      r.decoder = undefined;
    }
  }

  private decoder(device: string, r: Viewer): VideoDecoder {
    const dec = new VideoDecoder({
      output: (frame) => {
        try {
          r.canvas.width = frame.displayWidth;
          r.canvas.height = frame.displayHeight;
          r.canvas.getContext('2d')?.drawImage(frame, 0, 0);
        } finally {
          frame.close();
        }
        if (!r.stream) {
          r.stream = r.canvas.captureStream(SCREEN_FPS);
          this.o.onRemoteScreen(device, r.stream);
        }
      },
      error: () => {
        r.decoder = undefined;
        r.ready = false;
      },
    });
    dec.configure({ codec: 'vp8', optimizeForLatency: true });
    return dec;
  }

  /** 나간 기기·그만 본 화면의 받기 상태를 버립니다. */
  forget(device: string) {
    const r = this.remotes.get(device);
    if (!r) return;
    try {
      r.decoder?.close();
    } catch {
      // 이미 닫힘
    }
    for (const t of r.stream?.getTracks() ?? []) t.stop();
    this.remotes.delete(device);
  }

  close() {
    this.stop();
    for (const d of [...this.remotes.keys()]) this.forget(d);
  }
}
