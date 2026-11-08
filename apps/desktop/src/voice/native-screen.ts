// 화면공유 ffmpeg 인코더의 웹뷰 쪽(메시 통화만, 데스크톱 앱만).
//
// 보내기: 앱의 Rust 쪽이 ffmpeg로 캡처·인코딩한 프레임(AV1·H.264·HEVC)을 받아 → 조각으로 나눠 프레임 암호화 워커가 봉인
// (통화 미디어 키, 보낸 기기를 AAD에) → 그 코덱을 풀 수 있다고 알린 시청자마다 데이터 채널(순서 없음, 0.5초 안에서만
// 재전송)로 보냅니다. 같은 프레임을 이 기기에서도 풀어 트랙으로 만들고(미리보기), 그 트랙을 그 코덱을 풀지 못하는
// 시청자·SFU 통화에 웹뷰(WebRTC) 인코더로 다시 보냅니다(세션이 고름).
//
// 받기: 조각을 열고(보낸 기기로 검사) 모아 WebCodecs로 풀어 트랙으로 보여 줍니다. 받은 바이트는 웹뷰 디코더에만 넣고
// ffmpeg에는 넣지 않습니다.
//
// 데이터 채널은 대역에 맞춰 줄이지 않으므로, 상대 버퍼가 차면 그 상대에게는 다음 키 프레임까지 보내지 않습니다(지연이
// 쌓이지 않게). ffmpeg는 실행 중에 키 프레임을 요청할 수 없어, 늦게 들어온 시청자·끊긴 시청자는 다음 주기 키 프레임
// (설정의 키 프레임 간격)부터 봅니다.
//
// 평문 조각: [종류 u8: 1 조각][프레임 번호 u32 LE][조각 번호 u16 LE][조각 수 u16 LE][프레임 바이트 일부].
// 프레임 바이트: [플래그 u8: 1 키, 2 설정 있음][코덱 u8: 1 AV1, 2 H.264, 3 HEVC]
//   (설정 있음이면 [레코드 길이 u16 LE][가로 u16 LE][세로 u16 LE][avcC·hvcC·av1C])[프레임].
// hello: [종류 u8: 2][풀 수 있는 코덱 비트 u8: 1 AV1, 2 H.264, 4 HEVC].

import type { ScreenSource, ScreenTransport } from '../api/client';
import { t } from '../i18n';
import type { WorkerData, WorkerInMessage } from './e2ee-worker';
import type { NativeEncodeSettings, NativeEncoderId } from './media';
import { FrameAssembler } from './tor-screen';

/** 데이터 채널(양쪽이 같은 번호로 미리 정함, 신호 없이 열림). 신경망 음성은 7. */
export const SCREEN_CHANNEL = { label: 'yangpa-screen', id: 8 } as const;

export type VideoCodecId = 'av1' | 'h264' | 'hevc';
const CODEC_BYTE: Record<VideoCodecId, number> = { av1: 1, h264: 2, hevc: 3 };
const CODEC_OF: Record<number, VideoCodecId> = { 1: 'av1', 2: 'h264', 3: 'hevc' };
const CAP_BIT: Record<VideoCodecId, number> = { av1: 1, h264: 2, hevc: 4 };

const KIND_PART = 1;
const KIND_HELLO = 2;
const PART_HEADER = 9;
/** 조각 하나의 프레임 바이트(봉인 여유를 두고 16KiB 안: 큰 메시지는 다른 데이터를 막음). */
export const PART_BYTES = 16 * 1024 - 64;
/** 프레임 하나의 조각 수 상한(약 16MiB). */
export const MAX_PARTS = 1024;
/** 상대 하나에게서 덜 모인 프레임에 쥐고 있을 바이트 상한. */
const MAX_PENDING_BYTES = 24 * 1024 * 1024;
/** 이보다 큰 봉인 메시지는 조각(hello는 수십 바이트). 보지 않는 상대의 조각은 열지도 않고 버립니다. */
const HELLO_MAX_SEALED = 128;
const FLAG_KEY = 1;
const FLAG_CONFIG = 2;
/** 상대 버퍼가 이만큼 차면 그 상대에게는 다음 키 프레임까지 보내지 않습니다. */
const HIGH_WATER = 2 * 1024 * 1024;
/** 봉인을 기다리는 프레임이 이만큼 밀리면(이 기기가 못 따라감) 다음 키 프레임까지 버립니다. */
const MAX_BACKLOG = 8;
const HELLO_MS = 4000;
/** 워커 요청 번호를 신경망 음성(1부터)과 겹치지 않게. */
const ID_BASE = 0x4000_0000;

/** ffmpeg 인코더 → 영상 코덱. */
export function encoderCodec(e: NativeEncoderId): VideoCodecId {
  return e === 'nvenc-h264' ? 'h264' : e === 'nvenc-hevc' ? 'hevc' : 'av1';
}

// ───────────── Rust 쪽 메시지 ─────────────

export type EncoderMessage =
  | { type: 'config'; codec: VideoCodecId; width: number; height: number; record: Uint8Array }
  | { type: 'frame'; key: boolean; ts: number; data: Uint8Array }
  | { type: 'end'; reason: string };

export function parseEncoderMessage(m: Uint8Array): EncoderMessage | null {
  const v = new DataView(m.buffer, m.byteOffset, m.byteLength);
  switch (m[0]) {
    case 1: {
      const codec = CODEC_OF[m[1]];
      if (!codec || m.length < 6) return null;
      return { type: 'config', codec, width: v.getUint16(2, true), height: v.getUint16(4, true), record: m.subarray(6) };
    }
    case 2:
      if (m.length < 6) return null;
      return { type: 'frame', key: (m[1] & 1) !== 0, ts: v.getUint32(2, true), data: m.subarray(6) };
    case 3:
      return { type: 'end', reason: new TextDecoder().decode(m.subarray(1)) };
    default:
      return null;
  }
}

// ───────────── 프레임·조각 ─────────────

export interface StreamConfig {
  codec: VideoCodecId;
  width: number;
  height: number;
  record: Uint8Array;
}

export interface WireFrame {
  key: boolean;
  codec: VideoCodecId;
  config?: StreamConfig;
  data: Uint8Array;
}

export function encodeFrame(f: WireFrame): Uint8Array {
  const cfg = f.config;
  const head = 2 + (cfg ? 6 + cfg.record.length : 0);
  const out = new Uint8Array(head + f.data.length);
  const v = new DataView(out.buffer);
  out[0] = (f.key ? FLAG_KEY : 0) | (cfg ? FLAG_CONFIG : 0);
  out[1] = CODEC_BYTE[f.codec];
  if (cfg) {
    v.setUint16(2, cfg.record.length, true);
    v.setUint16(4, cfg.width, true);
    v.setUint16(6, cfg.height, true);
    out.set(cfg.record, 8);
  }
  out.set(f.data, head);
  return out;
}

export function decodeFrame(b: Uint8Array): WireFrame | null {
  if (b.length < 2) return null;
  const codec = CODEC_OF[b[1]];
  if (!codec) return null;
  const key = (b[0] & FLAG_KEY) !== 0;
  if ((b[0] & FLAG_CONFIG) === 0) return { key, codec, data: b.subarray(2) };
  if (b.length < 8) return null;
  const v = new DataView(b.buffer, b.byteOffset, b.byteLength);
  const len = v.getUint16(2, true);
  if (b.length < 8 + len) return null;
  return {
    key,
    codec,
    config: { codec, width: v.getUint16(4, true), height: v.getUint16(6, true), record: b.slice(8, 8 + len) },
    data: b.subarray(8 + len),
  };
}

export function splitParts(frame: number, bytes: Uint8Array): Uint8Array[] | null {
  const parts = Math.max(1, Math.ceil(bytes.length / PART_BYTES));
  if (parts > MAX_PARTS) return null;
  const out: Uint8Array[] = [];
  for (let i = 0; i < parts; i++) {
    const chunk = bytes.subarray(i * PART_BYTES, (i + 1) * PART_BYTES);
    const p = new Uint8Array(PART_HEADER + chunk.length);
    const v = new DataView(p.buffer);
    p[0] = KIND_PART;
    v.setUint32(1, frame >>> 0, true);
    v.setUint16(5, i, true);
    v.setUint16(7, parts, true);
    p.set(chunk, PART_HEADER);
    out.push(p);
  }
  return out;
}

export function parsePart(b: Uint8Array): { frame: number; part: number; parts: number; data: Uint8Array } | null {
  if (b.length <= PART_HEADER || b[0] !== KIND_PART) return null;
  const v = new DataView(b.buffer, b.byteOffset, b.byteLength);
  const part = v.getUint16(5, true);
  const parts = v.getUint16(7, true);
  if (parts === 0 || parts > MAX_PARTS || part >= parts) return null;
  return { frame: v.getUint32(1, true), part, parts, data: b.subarray(PART_HEADER) };
}

// ───────────── 코덱 문자열(WebCodecs) ─────────────

const hex2 = (n: number) => n.toString(16).padStart(2, '0');

/** 설정 레코드(avcC·hvcC·av1C) → WebCodecs 코덱 문자열. 읽을 수 없으면 null. */
export function codecString(codec: VideoCodecId, r: Uint8Array): string | null {
  if (codec === 'h264') {
    if (r.length < 4 || r[0] !== 1) return null;
    return `avc1.${hex2(r[1])}${hex2(r[2])}${hex2(r[3])}`;
  }
  if (codec === 'hevc') {
    if (r.length < 13 || r[0] !== 1) return null;
    const space = ['', 'A', 'B', 'C'][r[1] >> 6];
    const tier = (r[1] & 0x20) !== 0 ? 'H' : 'L';
    const profile = r[1] & 0x1f;
    const compat = ((r[2] << 24) | (r[3] << 16) | (r[4] << 8) | r[5]) >>> 0;
    // 호환 플래그는 비트 순서를 뒤집어 16진으로(ISO/IEC 14496-15 부록 E).
    let rev = 0;
    for (let i = 0; i < 32; i++) if (compat & (1 << i)) rev |= 1 << (31 - i);
    const cons = [...r.subarray(6, 12)];
    while (cons.length > 0 && cons[cons.length - 1] === 0) cons.pop();
    const tail = cons.map((b) => b.toString(16).toUpperCase()).join('.');
    return `hvc1.${space}${profile}.${(rev >>> 0).toString(16).toUpperCase()}.${tier}${r[12]}${tail ? `.${tail}` : ''}`;
  }
  if (r.length < 4 || (r[0] & 0x80) === 0) return null;
  const profile = r[1] >> 5;
  const level = r[1] & 0x1f;
  const tier = (r[2] & 0x80) !== 0 ? 'H' : 'M';
  const depth = (r[2] & 0x40) === 0 ? 8 : (r[2] & 0x20) !== 0 ? 12 : 10;
  return `av01.${profile}.${String(level).padStart(2, '0')}${tier}.${String(depth).padStart(2, '0')}`;
}

/** 디코더 설정. H.264·HEVC는 설정 레코드를 description으로(프레임이 길이 붙은 NAL), AV1은 키 프레임에 시퀀스 헤더가 있음. */
export function decoderConfig(cfg: StreamConfig): VideoDecoderConfig | null {
  const codec = codecString(cfg.codec, cfg.record);
  if (!codec) return null;
  return {
    codec,
    codedWidth: cfg.width || undefined,
    codedHeight: cfg.height || undefined,
    optimizeForLatency: true,
    ...(cfg.codec === 'av1' ? {} : { description: cfg.record }),
  };
}

/** 이 웹뷰가 풀 수 있는 코덱 비트(hello에 실음). */
export async function decodableMask(): Promise<number> {
  if (typeof VideoDecoder === 'undefined') return 0;
  const probes: Array<[VideoCodecId, string]> = [
    ['av1', 'av01.0.08M.08'],
    ['h264', 'avc1.640034'],
    ['hevc', 'hvc1.1.6.L153.B0'],
  ];
  let mask = 0;
  for (const [c, codec] of probes) {
    const ok = await VideoDecoder.isConfigSupported({ codec, optimizeForLatency: true })
      .then((r) => !!r.supported)
      .catch(() => false);
    if (ok) mask |= CAP_BIT[c];
  }
  return mask;
}

// ───────────── 풀린 프레임 → 트랙 ─────────────

declare const MediaStreamTrackGenerator:
  | { new (init: { kind: 'video' }): MediaStreamTrack & { writable: WritableStream<VideoFrame> } }
  | undefined;

/** 풀린 프레임을 트랙으로. 크로뮴의 MediaStreamTrackGenerator가 있으면 그것, 없으면 캔버스. */
class FrameSink {
  readonly stream: MediaStream;
  private writer: WritableStreamDefaultWriter<VideoFrame> | null = null;
  private canvas: HTMLCanvasElement | null = null;

  constructor(fps: number) {
    if (typeof MediaStreamTrackGenerator === 'function') {
      const gen = new MediaStreamTrackGenerator({ kind: 'video' });
      this.writer = gen.writable.getWriter();
      this.stream = new MediaStream([gen]);
    } else {
      this.canvas = document.createElement('canvas');
      this.stream = this.canvas.captureStream(fps);
    }
  }

  get track(): MediaStreamTrack {
    return this.stream.getVideoTracks()[0];
  }

  push(frame: VideoFrame) {
    const w = this.writer;
    if (w) {
      // 보여 주는 쪽이 밀리면 버립니다(write가 프레임을 넘겨받아 닫음).
      if ((w.desiredSize ?? 1) <= 0) frame.close();
      else void w.write(frame).catch(() => frame.close());
      return;
    }
    const c = this.canvas;
    try {
      if (c) {
        if (c.width !== frame.displayWidth || c.height !== frame.displayHeight) {
          c.width = frame.displayWidth;
          c.height = frame.displayHeight;
        }
        c.getContext('2d')?.drawImage(frame, 0, 0);
      }
    } finally {
      frame.close();
    }
  }

  close() {
    void this.writer?.close().catch(() => undefined);
    for (const t of this.stream.getTracks()) t.stop();
  }
}

/** 한 스트림의 디코더: 설정이 바뀌면 다시 만들고, 키 프레임 전이거나 프레임을 놓쳤으면 다음 키 프레임까지 기다립니다. */
class StreamDecoder {
  private decoder: VideoDecoder | null = null;
  private configKey = '';
  private ready = false;
  private last: number | undefined;

  /** onError: 이 코덱을 실제로는 풀지 못할 때(isConfigSupported는 됐는데 하드웨어 디코더가 실패하는 등). */
  constructor(
    private readonly sink: FrameSink,
    private readonly onError: (codec: VideoCodecId) => void = () => undefined,
  ) {}

  configure(cfg: StreamConfig) {
    const key = `${cfg.codec}:${cfg.width}x${cfg.height}:${[...cfg.record].join(',')}`;
    if (key === this.configKey && this.decoder) return;
    const conf = decoderConfig(cfg);
    this.reset();
    if (!conf) {
      this.onError(cfg.codec);
      return;
    }
    try {
      this.decoder = new VideoDecoder({
        output: (f) => this.sink.push(f),
        error: () => {
          this.decoder = null;
          this.configKey = '';
          this.ready = false;
          this.onError(cfg.codec);
        },
      });
      this.decoder.configure(conf);
      this.configKey = key;
    } catch {
      this.decoder = null;
      this.configKey = '';
      this.onError(cfg.codec);
    }
  }

  decode(frame: number, key: boolean, data: Uint8Array) {
    const follows = this.last !== undefined && frame === ((this.last + 1) >>> 0);
    this.last = frame;
    if (!key && (!this.ready || !follows)) {
      this.ready = false;
      return;
    }
    const d = this.decoder;
    if (!d || d.state !== 'configured') {
      this.ready = false;
      return;
    }
    // 디코더가 밀리면 다음 키 프레임까지 버립니다.
    if (!key && d.decodeQueueSize > 8) {
      this.ready = false;
      return;
    }
    try {
      d.decode(new EncodedVideoChunk({ type: key ? 'key' : 'delta', timestamp: frame * 1000, data }));
      this.ready = true;
    } catch {
      this.ready = false;
    }
  }

  reset() {
    try {
      if (this.decoder && this.decoder.state !== 'closed') this.decoder.close();
    } catch {
      // 이미 닫힘
    }
    this.decoder = null;
    this.configKey = '';
    this.ready = false;
  }
}

// ───────────── 링크 ─────────────

interface Peer {
  device: string;
  dc: RTCDataChannel;
  /** 상대가 풀 수 있다고 알린 코덱 비트(hello 전에는 null). */
  caps: number | null;
  /** 다음 키 프레임까지 보내지 않음(처음·버퍼가 찼을 때). */
  needKey: boolean;
  helloTimer: ReturnType<typeof setInterval> | null;
  recv?: { assembler: FrameAssembler; decoder: StreamDecoder; sink: FrameSink; shown: boolean };
}

interface Local {
  gen: number;
  transport: ScreenTransport;
  source: Pick<ScreenSource, 'kind' | 'id'>;
  sink: FrameSink;
  preview: StreamDecoder;
  codec: VideoCodecId;
  config: StreamConfig | null;
  fps: number;
  id: number | null;
  frame: number;
  chain: Promise<void>;
  backlog: number;
  dropping: boolean;
}

export interface NativeScreenOptions {
  /** 프레임 암호화 워커(세션의 것). 'data' 응답은 세션이 `onWorkerData`로 넘깁니다. */
  worker: Worker;
  myDevice: string;
  /** 받은 화면을 처음 그릴 때. */
  onRemoteScreen(device: string, stream: MediaStream): void;
  /** 그 기기의 화면을 보는 중인지('보기'를 누르기 전·끈 뒤에 온 조각은 풀지 않고 버림). */
  isWatching(device: string): boolean;
  /** 상대가 알린 코덱이 바뀌었을 때(세션이 그 상대의 WebRTC 화면 트랙을 다시 고름). */
  onCapsChanged(device: string): void;
  /** 내 공유가 스스로 끝났을 때(창을 닫음·인코더 오류). 정상 종료면 빈 문자열. */
  onLocalEnded(reason: string): void;
}

export class NativeScreen {
  private readonly peers = new Map<string, Peer>();
  private local: Local | null = null;
  private gen = 0;
  private viewers = new Set<string>();
  private caps = 0;
  /** 풀다가 실패한 코덱 비트(hello에서 빼서 보내는 쪽이 WebRTC로 보내게 함). */
  private failed = 0;
  private readonly capsReady: Promise<number>;
  private readonly pending = new Map<number, (d: ArrayBuffer | null) => void>();
  private nextId = ID_BASE;
  private closed = false;

  constructor(private readonly o: NativeScreenOptions) {
    this.capsReady = decodableMask().then((m) => {
      this.caps = m & ~this.failed;
      for (const p of this.peers.values()) void this.sendHello(p);
      return m;
    });
  }

  /** 이 기기가 그 인코더의 코덱을 풀 수 있는지. 못 풀면 미리보기와 WebRTC로 다시 보내는 트랙이 비므로 쓰지 않습니다. */
  async canDecode(encoder: NativeEncoderId): Promise<boolean> {
    await this.capsReady;
    return (this.caps & CAP_BIT[encoderCodec(encoder)]) !== 0;
  }

  /** 실제로 풀지 못한 코덱을 hello에서 빼고 다시 알립니다(보내는 쪽이 그 시청자에게는 WebRTC로 보냄). */
  private decodeFailed(codec: VideoCodecId, from: Peer) {
    this.dropRecv(from);
    const bit = CAP_BIT[codec];
    if ((this.caps & bit) === 0) return;
    this.failed |= bit;
    this.caps &= ~bit;
    for (const p of this.peers.values()) void this.sendHello(p);
  }

  onWorkerData(msg: WorkerData) {
    const done = this.pending.get(msg.id);
    if (!done) return;
    this.pending.delete(msg.id);
    done(msg.data);
  }

  private crypt(type: 'seal' | 'open', sender: string, data: Uint8Array): Promise<Uint8Array | null> {
    const id = this.nextId++;
    if (this.nextId >= 0x7fff_ffff) this.nextId = ID_BASE;
    const buf = data.slice().buffer as ArrayBuffer;
    return new Promise((resolve) => {
      this.pending.set(id, (d) => resolve(d ? new Uint8Array(d) : null));
      const msg: WorkerInMessage = { type, id, sender, data: buf };
      this.o.worker.postMessage(msg, [buf]);
    });
  }

  // ───────────── 상대 ─────────────

  attach(device: string, pc: RTCPeerConnection) {
    if (this.closed || this.peers.has(device)) return;
    let dc: RTCDataChannel;
    try {
      dc = pc.createDataChannel(SCREEN_CHANNEL.label, { negotiated: true, id: SCREEN_CHANNEL.id, ordered: false, maxPacketLifeTime: 500 });
    } catch {
      return;
    }
    dc.binaryType = 'arraybuffer';
    dc.bufferedAmountLowThreshold = HIGH_WATER / 4;
    const peer: Peer = { device, dc, caps: null, needKey: true, helloTimer: null };
    this.peers.set(device, peer);
    dc.onopen = () => {
      void this.sendHello(peer);
      peer.helloTimer = setInterval(() => void this.sendHello(peer), HELLO_MS);
    };
    dc.onclose = () => this.dropPeer(device);
    dc.onmessage = (e) => void this.onMessage(peer, e.data);
  }

  detach(device: string) {
    const p = this.peers.get(device);
    if (!p) return;
    try {
      p.dc.close();
    } catch {
      // 이미 닫힘
    }
    this.dropPeer(device);
  }

  private dropPeer(device: string) {
    const p = this.peers.get(device);
    if (!p) return;
    this.peers.delete(device);
    if (p.helloTimer) clearInterval(p.helloTimer);
    this.dropRecv(p);
    this.o.onCapsChanged(device);
  }

  private async sendHello(p: Peer) {
    if (p.dc.readyState !== 'open') return;
    const sealed = await this.crypt('seal', this.o.myDevice, new Uint8Array([KIND_HELLO, this.caps]));
    if (sealed && p.dc.readyState === 'open') p.dc.send(sealed as Uint8Array<ArrayBuffer>);
  }

  /** 이 상대가 지금 공유 중인 코덱을 데이터 채널로 받을 수 있는지(아니면 세션이 WebRTC로 보냄). */
  receivesNative(device: string): boolean {
    const p = this.peers.get(device);
    const codec = this.local?.codec;
    return !!p && !!codec && p.dc.readyState === 'open' && p.caps !== null && (p.caps & CAP_BIT[codec]) !== 0;
  }

  /** 내 화면을 보낼 시청자. */
  setViewers(devices: string[]) {
    const next = new Set(devices);
    for (const d of next) {
      const p = this.peers.get(d);
      // 새 시청자는 다음 키 프레임부터.
      if (p && !this.viewers.has(d)) p.needKey = true;
    }
    this.viewers = next;
  }

  // ───────────── 내 공유 ─────────────

  get sharing(): boolean {
    return !!this.local;
  }

  /** 공유를 시작하고 이 기기에서 푼 트랙(미리보기·WebRTC 대체용)을 돌려줍니다. */
  async start(
    transport: ScreenTransport,
    source: Pick<ScreenSource, 'kind' | 'id'>,
    maxHeight: number | null,
    settings: NativeEncodeSettings,
  ): Promise<MediaStreamTrack> {
    this.stop();
    const sink = new FrameSink(settings.fps);
    const local: Local = {
      gen: ++this.gen,
      transport,
      source,
      sink,
      preview: new StreamDecoder(sink, () => this.previewFailed()),
      codec: encoderCodec(settings.encoder),
      config: null,
      fps: settings.fps,
      id: null,
      frame: 0,
      chain: Promise.resolve(),
      backlog: 0,
      dropping: false,
    };
    this.local = local;
    try {
      await this.launch(local, maxHeight, settings);
    } catch (e) {
      if (this.local === local) this.stop();
      throw e;
    }
    return sink.track;
  }

  /** 공유 중에 설정을 바꿉니다: ffmpeg를 새 설정으로 다시 띄우고 트랙은 그대로 둡니다(CLI라 도중에 못 바꿈). */
  async restart(maxHeight: number | null, settings: NativeEncodeSettings) {
    await this.capsReady;
    const local = this.local;
    if (!local) return;
    const codec = encoderCodec(settings.encoder);
    if ((this.caps & CAP_BIT[codec]) === 0) {
      this.stop();
      this.o.onLocalEnded(t('이 기기에서 풀 수 없는 코덱이라 공유를 멈췄습니다. 다른 인코더를 고르세요'));
      return;
    }
    local.gen = ++this.gen;
    local.codec = codec;
    local.config = null;
    local.fps = settings.fps;
    for (const p of this.peers.values()) p.needKey = true;
    try {
      await this.launch(local, maxHeight, settings);
    } catch (e) {
      if (this.local === local) this.o.onLocalEnded((e as Error).message || t('인코더를 다시 띄우지 못했습니다'));
    }
    for (const d of this.peers.keys()) this.o.onCapsChanged(d);
  }

  private async launch(local: Local, maxHeight: number | null, settings: NativeEncodeSettings) {
    const gen = local.gen;
    const id = await local.transport.start(local.source, maxHeight, settings, (m) => {
      if (this.local === local && local.gen === gen) this.onEncoder(local, m);
    });
    if (this.local !== local || local.gen !== gen) {
      void local.transport.stop(id).catch(() => undefined);
      return;
    }
    local.id = id;
  }

  /** 이 기기에서 내 화면을 풀지 못함: 미리보기와 WebRTC 시청자에게 갈 트랙이 비므로 공유를 멈춥니다. */
  private previewFailed() {
    if (!this.local) return;
    this.stop();
    this.o.onLocalEnded(t('이 기기에서 인코더 출력을 풀지 못해 공유를 멈췄습니다. 다른 인코더를 고르세요'));
  }

  private onEncoder(local: Local, raw: Uint8Array) {
    const m = parseEncoderMessage(raw);
    if (!m) return;
    if (m.type === 'end') {
      this.stop();
      this.o.onLocalEnded(m.reason);
      return;
    }
    if (m.type === 'config') {
      if (m.record.length === 0) return;
      local.config = { codec: m.codec, width: m.width, height: m.height, record: m.record.slice() };
      local.preview.configure(local.config);
      return;
    }
    const cfg = local.config;
    if (!cfg) return;
    const frame = local.frame++;
    local.preview.decode(frame, m.key, m.data);
    if (local.dropping && !m.key) return;
    local.dropping = false;
    if (![...this.viewers].some((d) => this.receivesNative(d))) return;
    if (local.backlog >= MAX_BACKLOG) {
      local.dropping = true;
      return;
    }
    const parts = splitParts(frame, encodeFrame({ key: m.key, codec: cfg.codec, config: m.key ? cfg : undefined, data: m.data }));
    if (!parts) return;
    local.backlog++;
    local.chain = local.chain.then(async () => {
      try {
        const sealed: Uint8Array[] = [];
        for (const p of parts) {
          const s = await this.crypt('seal', this.o.myDevice, p);
          if (!s) return;
          sealed.push(s);
        }
        if (this.local !== local) return;
        // 봉인하는 사이 그만 본 시청자에게는 보내지 않게, 보낼 때 다시 고릅니다.
        for (const d of this.viewers) {
          const peer = this.peers.get(d);
          if (!peer || !this.receivesNative(d)) continue;
          if (peer.needKey && !m.key) continue;
          if (peer.dc.bufferedAmount > HIGH_WATER) {
            peer.needKey = true;
            continue;
          }
          peer.needKey = false;
          for (const s of sealed) peer.dc.send(s as Uint8Array<ArrayBuffer>);
        }
      } catch {
        // 한 프레임 실패는 건너뜁니다(다음 키 프레임에서 회복)
      } finally {
        local.backlog--;
      }
    });
  }

  stop() {
    const local = this.local;
    this.local = null;
    if (!local) return;
    if (local.id !== null) void local.transport.stop(local.id).catch(() => undefined);
    local.preview.reset();
    local.sink.close();
    for (const d of this.peers.keys()) this.o.onCapsChanged(d);
  }

  // ───────────── 받기 ─────────────

  remoteScreen(device: string): MediaStream | undefined {
    const r = this.peers.get(device)?.recv;
    return r?.shown ? r.sink.stream : undefined;
  }

  /** 그만 본 화면·나간 기기의 받기 상태를 버립니다. */
  forget(device: string) {
    const p = this.peers.get(device);
    if (p) this.dropRecv(p);
  }

  private dropRecv(p: Peer) {
    if (!p.recv) return;
    p.recv.decoder.reset();
    p.recv.sink.close();
    p.recv = undefined;
  }

  private async onMessage(p: Peer, data: unknown) {
    if (!(data instanceof ArrayBuffer)) return;
    if (data.byteLength > HELLO_MAX_SEALED && !this.o.isWatching(p.device)) {
      this.dropRecv(p);
      return;
    }
    // 그 상대가 봉인한 것만 엽니다(AAD의 보낸 기기).
    const plain = await this.crypt('open', p.device, new Uint8Array(data));
    if (!plain || this.closed || this.peers.get(p.device) !== p) return;
    if (plain[0] === KIND_HELLO) {
      if (plain.length < 2) return;
      const caps = plain[1];
      if (caps !== p.caps) {
        p.caps = caps;
        p.needKey = true;
        this.o.onCapsChanged(p.device);
      }
      return;
    }
    const part = parsePart(plain);
    if (!part || !this.o.isWatching(p.device)) return;
    if (!p.recv) {
      const sink = new FrameSink(60);
      const decoder = new StreamDecoder(sink, (c) => this.decodeFailed(c, p));
      p.recv = { assembler: new FrameAssembler(MAX_PENDING_BYTES), decoder, sink, shown: false };
    }
    const r = p.recv;
    const whole = r.assembler.push({ frame: part.frame, part: part.part, parts: part.parts, key: false, width: 0, height: 0, data: part.data });
    if (!whole) return;
    const f = decodeFrame(whole.data);
    if (!f || (this.caps & CAP_BIT[f.codec]) === 0) return;
    if (f.config) r.decoder.configure(f.config);
    r.decoder.decode(whole.frame, f.key, f.data);
    if (!r.shown && f.key) {
      r.shown = true;
      this.o.onRemoteScreen(p.device, r.sink.stream);
    }
  }

  close() {
    if (this.closed) return;
    this.stop();
    this.closed = true;
    for (const d of [...this.peers.keys()]) this.detach(d);
    for (const done of this.pending.values()) done(null);
    this.pending.clear();
  }
}
