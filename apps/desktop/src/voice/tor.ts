// Tor 통화(`topology: 'tor'`)의 소리. WebRTC를 쓰지 않습니다: RTCPeerConnection은 만들기만 해도 IP 후보를 모으므로, 이
// 통화에서는 하나도 만들지 않습니다.
//
// 흐름: 마이크 → 24kHz 캡처 워클릿(80ms 프레임) → 코덱(Opus는 WebCodecs, Mimi·DAC는 앱의 Rust 쪽 ONNX Runtime) → 프레임 암호화
// 워커가 봉인(통화 미디어 키, 보낸 기기를 AAD에) → `voice.media`(데몬이 Tor 연결로 참가자 모두에게) → 받는 데몬의 `voice.media`
// 알림 → 보낸 기기로 검사해 열고 → 번호 순으로 맞춰(동시에 보낸 묶음끼리 순서가 바뀔 수 있음) 풀어서 → 지터 버퍼 재생.
//
// 패킷(평문): [종류 u8: 1 소리, 2 hello, 3 화면 조각][코덱 u8: 1 Mimi, 2 DAC, 3 Opus, 0x10 VP8][번호 u32 LE][본문]. 본문은
// Mimi·DAC면 코드 u16 LE…, Opus면 Opus 패킷 그대로, 화면 조각은 tor-screen.ts의 형식. hello의 코덱 칸은 풀 수 있는 코덱 비트
// (1 Mimi, 2 DAC, 4 Opus). 비상용 화면공유는 '화면 보기'를 누른 사람에게만 보냅니다(tor-screen.ts).
//
// 보낼 코덱: 고른 코덱을 모든 상대가 풀 수 있으면 그것, 아니면 Opus. 한 번 보내 모두에게 가므로(데몬이 나눠 줌) 코덱은 하나입니다.

import type { NeuralCodecId, NeuralTransport } from '../api/client';
import { t } from '../i18n';
import { base64ToBytes, bytesToBase64 } from '../lib/misc';
import { applySink } from './devices';
import type { WorkerData, WorkerInMessage } from './e2ee-worker';
import { CAPTURE_FRAME, NEURAL_RATE, PACKET_CODES, lostBetween } from './neural';
import workletUrl from './neural-worklets.ts?worker&url';
import { TorScreen } from './tor-screen';

export type TorCodec = 'opus' | NeuralCodecId;

const KIND_AUDIO = 1;
const KIND_HELLO = 2;
const KIND_SCREEN = 3;
const CODEC_VP8 = 0x10;
const CODEC_BYTE: Record<TorCodec, number> = { mimi: 1, dac: 2, opus: 3 };
const CODEC_OF: Record<number, TorCodec> = { 1: 'mimi', 2: 'dac', 3: 'opus' };
const CAP_BIT: Record<TorCodec, number> = { mimi: 1, dac: 2, opus: 4 };
/** Opus 한 패킷 길이(µs). Tor는 왕복이 길어 패킷 수를 줄이려고 길게 잡습니다. */
const OPUS_FRAME_US = 60_000;
const OPUS_BITRATE = 24_000;
/** Tor 지터가 커서 재생 버퍼를 넉넉히(시작 0.5초, 상한 3초). */
const PLAYER_BUFFER = { target: 12_000, max: 72_000 };
const HELLO_MS = 4000;
/** 빠진 패킷을 기다리는 시간. 지나면 건너뛰고 다음 것부터 풉니다. */
const REORDER_MS = 200;
/** 보내는 중인 `voice.media` 호출이 이만큼 밀리면 새 패킷을 버립니다(데몬·로컬 연결이 못 따라갈 때). */
const MAX_SENDING = 32;
const MAX_ENCODE_BACKLOG = 4;
/** 데몬의 소리 패킷 한도(`voice::media::MAX_PACKET`)보다 큰 Opus 패킷은 보내지 않습니다. */
const MAX_PACKET = 4096 - 64;

export function encodeTorPacket(kind: number, codec: number, seq: number, body?: Uint8Array): Uint8Array {
  const out = new Uint8Array(6 + (body?.length ?? 0));
  out[0] = kind;
  out[1] = codec;
  new DataView(out.buffer).setUint32(2, seq >>> 0, true);
  if (body) out.set(body, 6);
  return out;
}

export function decodeTorPacket(data: Uint8Array): { kind: number; codec: number; seq: number; body: Uint8Array } | null {
  if (data.length < 6) return null;
  const v = new DataView(data.buffer, data.byteOffset, data.byteLength);
  return { kind: data[0], codec: data[1], seq: v.getUint32(2, true), body: data.subarray(6) };
}

export function codesToBytes(codes: Uint16Array): Uint8Array {
  const out = new Uint8Array(codes.length * 2);
  const v = new DataView(out.buffer);
  for (let i = 0; i < codes.length; i++) v.setUint16(i * 2, codes[i], true);
  return out;
}

export function bytesToCodes(b: Uint8Array): Uint16Array | null {
  if (b.length % 2 !== 0) return null;
  const v = new DataView(b.buffer, b.byteOffset, b.byteLength);
  const out = new Uint16Array(b.length / 2);
  for (let i = 0; i < out.length; i++) out[i] = v.getUint16(i * 2, true);
  return out;
}

/**
 * 보낼 코덱: 고른 코덱을 모든 상대가 풀 수 있고 내가 만들 수 있으면 그것, 아니면 모두가 푸는 Opus, 그것도 안 되면 모두가 푸는
 * 다른 코덱. 아직 hello가 오지 않은 상대(능력 0)는 따지지 않습니다. 모두가 푸는 코덱이 없으면 Opus(없으면 만들 수 있는 첫 코덱).
 */
export function pickTorCodec(want: TorCodec, encodable: readonly TorCodec[], peerCaps: readonly number[]): TorCodec {
  const known = peerCaps.filter((c) => c !== 0);
  const ok = (c: TorCodec) => encodable.includes(c) && known.every((k) => (k & CAP_BIT[c]) !== 0);
  for (const c of [want, 'opus', 'mimi', 'dac'] as const) if (ok(c)) return c;
  return encodable.includes('opus') ? 'opus' : (encodable[0] ?? 'opus');
}

/** 다음 번호보다 이만큼 넘게 뒤의 번호가 오면 보낸 쪽이 다시 시작한 것으로 봅니다(80ms 패킷으로 20초 넘게 늦은 패킷은 없음). */
const RESTART_BEHIND = 256;

/**
 * 번호 순으로 내보내는 재정렬 버퍼. 다음 번호가 오면 바로, 건너뛴 번호는 `waitMs`만큼 기다렸다가 넘깁니다. 지난 번호는 버립니다.
 */
export class Reorder<T> {
  private next: number | undefined;
  private readonly held = new Map<number, T>();
  private timer: ReturnType<typeof setTimeout> | null = null;

  constructor(
    private readonly deliver: (item: T, lost: number) => void,
    private readonly waitMs = REORDER_MS,
  ) {}

  push(seq: number, item: T) {
    seq >>>= 0;
    if (this.next !== undefined && lostBetween(this.next - 1, seq) === null) {
      // 한참 뒤의 번호는 보낸 쪽이 다시 시작한 것(앱을 다시 열어 번호가 0부터): 처음부터 다시 맞춥니다. 조금 뒤면 늦게 온 것이라 버림.
      const behind = (this.next - seq) >>> 0;
      if (behind <= RESTART_BEHIND) return;
      this.reset();
    }
    if (this.next === undefined || seq === this.next) {
      this.emit(seq, item, 0);
      this.drain();
      return;
    }
    if (this.held.size > 64) {
      this.skip();
      // 건너뛴 뒤 다음 번호가 이 번호를 지났거나 바로 이 번호면 다시 따집니다(지난 번호를 붙잡아 두지 않게).
      if (lostBetween(this.next - 1, seq) === null) return;
      if (seq === this.next) {
        this.emit(seq, item, 0);
        this.drain();
        return;
      }
    }
    this.held.set(seq, item);
    this.timer ??= setTimeout(() => {
      this.timer = null;
      this.skip();
    }, this.waitMs);
  }

  private emit(seq: number, item: T, lost: number) {
    this.next = (seq + 1) >>> 0;
    this.deliver(item, lost);
  }

  private drain() {
    while (this.next !== undefined && this.held.has(this.next)) {
      const s = this.next;
      const item = this.held.get(s)!;
      this.held.delete(s);
      this.emit(s, item, 0);
    }
    if (this.held.size === 0 && this.timer) {
      clearTimeout(this.timer);
      this.timer = null;
    }
  }

  /** 기다리던 번호를 포기하고 가장 이른 다음 번호부터 내보냅니다. */
  private skip() {
    if (this.held.size === 0 || this.next === undefined) return;
    const base = this.next;
    let best = -1;
    let bestDist = Infinity;
    for (const s of this.held.keys()) {
      const d = (s - base) >>> 0;
      if (d < bestDist) {
        bestDist = d;
        best = s;
      }
    }
    const item = this.held.get(best)!;
    this.held.delete(best);
    this.emit(best, item, Math.min(bestDist, 1000));
    this.drain();
    if (this.held.size > 0) {
      this.timer ??= setTimeout(() => {
        this.timer = null;
        this.skip();
      }, this.waitMs);
    }
  }

  private reset() {
    this.close();
    this.next = undefined;
  }

  close() {
    if (this.timer) clearTimeout(this.timer);
    this.timer = null;
    this.held.clear();
  }
}

/** 표본 빈도 바꾸기(선형). 받은 소리를 24kHz로(웹뷰의 Opus 디코더는 48kHz로 냄), 24kHz를 못 받는 인코더에는 48kHz로. */
export function resample(pcm: Float32Array, from: number, to = NEURAL_RATE): Float32Array {
  if (from === to) return pcm;
  const n = Math.max(1, Math.round((pcm.length * to) / from));
  const out = new Float32Array(n);
  const step = from / to;
  for (let i = 0; i < n; i++) {
    const x = i * step;
    const j = Math.floor(x);
    const t = x - j;
    out[i] = (pcm[j] ?? 0) * (1 - t) + (pcm[j + 1] ?? pcm[j] ?? 0) * t;
  }
  return out;
}

interface Remote {
  device: string;
  caps: number;
  reorder: Reorder<{ codec: TorCodec; body: Uint8Array }>;
  node?: AudioWorkletNode;
  opus?: AudioDecoder;
  chain: Promise<void>;
  opusTs: number;
}

export interface TorAudioOptions {
  client: {
    call(method: 'voice.media', params: { space: string; channel: string; call: string; data: string; to?: string[] }): Promise<unknown>;
  };
  space: string;
  channel: string;
  call: string;
  myDevice: string;
  /** 프레임 암호화 워커(세션의 것). 'data' 응답은 세션이 `onWorkerData`로 넘깁니다. */
  worker: Worker;
  /** Mimi·DAC(데스크톱 앱만). 없으면 Opus만. */
  neural?: NeuralTransport;
  /** 상대 소리를 처음 틀 때(말하기 표시용 스트림). */
  onRemoteStream(device: string, stream: MediaStream): void;
  /** 보내는 코덱이 바뀌었을 때. */
  onCodec(codec: TorCodec | null): void;
  /** 받은 화면 스트림(처음 그릴 때). */
  onRemoteScreen?(device: string, stream: MediaStream): void;
  /** 소리를 보낼 수 없게 됐을 때(웹뷰가 Opus를 지원하지 않음 등). */
  onError?(message: string): void;
}

const OPUS_CONFIG = { codec: 'opus', numberOfChannels: 1, bitrate: OPUS_BITRATE, opus: { frameDuration: OPUS_FRAME_US } };

/** 웹뷰가 받는 Opus 인코더·디코더 표본 빈도(24kHz를 먼저, 안 되면 48kHz). 못 쓰면 null. */
async function probeOpus(): Promise<{ enc: number | null; dec: number | null }> {
  const pick = async (check: (rate: number) => Promise<boolean>) => {
    for (const rate of [NEURAL_RATE, 48_000]) {
      try {
        if (await check(rate)) return rate;
      } catch {
        // 다음 빈도
      }
    }
    return null;
  };
  if (!opusSupported()) return { enc: null, dec: null };
  return {
    enc: await pick(async (sampleRate) => !!(await AudioEncoder.isConfigSupported({ ...OPUS_CONFIG, sampleRate } as AudioEncoderConfig)).supported),
    dec: await pick(async (sampleRate) => !!(await AudioDecoder.isConfigSupported({ codec: 'opus', sampleRate, numberOfChannels: 1 })).supported),
  };
}

/** 이 웹뷰가 WebCodecs Opus를 쓸 수 있는지. */
export function opusSupported(): boolean {
  return typeof AudioEncoder !== 'undefined' && typeof AudioDecoder !== 'undefined' && typeof AudioData !== 'undefined';
}

export class TorAudio {
  private audio: { ctx: AudioContext; gain: GainNode; ready: Promise<void> } | null = null;
  private readonly remotes = new Map<string, Remote>();
  private decodable: TorCodec[] = opusSupported() ? ['opus'] : [];
  private encodable: TorCodec[] = opusSupported() ? ['opus'] : [];
  private want: TorCodec = 'opus';
  private codec: TorCodec | null = null;
  private mic: MediaStreamTrack | null = null;
  private source: MediaStreamAudioSourceNode | null = null;
  private capture: AudioWorkletNode | null = null;
  private muted = false;
  private deafened = false;
  /** 스피커 장치('' 기본)와 음량(0~1). */
  private sink = '';
  private volume = 1;
  private seq = 0;
  private backlog = 0;
  private sending = 0;
  private encodeChain: Promise<void> = Promise.resolve();
  private opusEnc: AudioEncoder | null = null;
  private opusTs = 0;
  /** 웹뷰가 받는 Opus 표본 빈도(확인 전에는 24kHz로 가정). */
  private opusRate = { enc: NEURAL_RATE as number | null, dec: NEURAL_RATE as number | null };
  private neuralReady: NeuralCodecId[] = [];
  private readonly pending = new Map<number, (d: ArrayBuffer | null) => void>();
  private nextId = 1;
  private helloTimer: ReturnType<typeof setInterval> | null = null;
  private closed = false;
  private readonly encStream: string;
  private screenSeq = 0;
  /** 비상용 화면공유(보내기·받기). */
  readonly screen: TorScreen;

  constructor(private readonly o: TorAudioOptions) {
    this.screen = new TorScreen({
      send: (body, to) => void this.sendScreen(body, to),
      onRemoteScreen: (device, stream) => o.onRemoteScreen?.(device, stream),
    });
    this.encStream = `${o.call}:tor:${Math.random().toString(36).slice(2)}`;
    this.helloTimer = setInterval(() => void this.sendHello(), HELLO_MS);
    void this.sendHello();
    void probeOpus().then((r) => {
      if (this.closed) return;
      this.opusRate = r;
      this.refreshCodecs();
      if (r.enc === null && this.neuralReady.length === 0) this.o.onError?.(t('이 웹뷰가 Opus 인코딩을 지원하지 않아 Tor 통화로 소리를 보낼 수 없습니다'));
    });
  }

  /** 풀고 만들 수 있는 코덱을 다시 정합니다(Opus 확인 결과·모델). */
  private refreshCodecs() {
    this.decodable = [...(this.opusRate.dec !== null && opusSupported() ? (['opus'] as TorCodec[]) : []), ...this.neuralReady];
    this.encodable = [...(this.opusRate.enc !== null && opusSupported() ? (['opus'] as TorCodec[]) : []), ...this.neuralReady];
    void this.sendHello();
    this.updateCodec();
  }

  private async ctx(): Promise<{ ctx: AudioContext; gain: GainNode } | null> {
    if (this.closed) return null;
    try {
      if (!this.audio) {
        const ctx = new AudioContext({ sampleRate: NEURAL_RATE, latencyHint: 'interactive' });
        const gain = ctx.createGain();
        gain.gain.value = this.deafened ? 0 : this.volume;
        gain.connect(ctx.destination);
        if (this.sink) applySink(ctx, this.sink);
        this.audio = { ctx, gain, ready: ctx.audioWorklet.addModule(workletUrl) };
      }
      await this.audio.ready;
    } catch {
      return null;
    }
    if (this.closed) return null;
    if (this.audio.ctx.state === 'suspended') void this.audio.ctx.resume().catch(() => undefined);
    return this.audio;
  }

  /** 세션 워커의 'data' 응답. */
  onWorkerData(msg: WorkerData) {
    const done = this.pending.get(msg.id);
    if (!done) return;
    this.pending.delete(msg.id);
    done(msg.data);
  }

  private crypt(type: 'seal' | 'open', sender: string, data: Uint8Array): Promise<Uint8Array | null> {
    const id = this.nextId++;
    const buf = data.slice().buffer as ArrayBuffer;
    return new Promise((resolve) => {
      this.pending.set(id, (d) => resolve(d ? new Uint8Array(d) : null));
      const msg: WorkerInMessage = { type, id, sender, data: buf };
      this.o.worker.postMessage(msg, [buf]);
    });
  }

  /** 모델이 있는 신경망 코덱(풀고 만들 수 있음). */
  setNeuralReady(codecs: NeuralCodecId[]) {
    this.neuralReady = [...codecs];
    this.refreshCodecs();
  }

  /** 고른 음성 코덱(설정). */
  setPreferred(codec: TorCodec) {
    this.want = codec;
    this.updateCodec();
  }

  /** 지금 참가자(나 제외). 나간 상대의 재생기를 닫습니다. */
  setPeers(devices: string[]) {
    const keep = new Set(devices);
    for (const [d, r] of this.remotes) {
      if (keep.has(d)) continue;
      this.dropRemote(r);
      this.remotes.delete(d);
      this.screen.forget(d);
    }
    const added = devices.some((d) => !this.remotes.has(d));
    for (const d of devices) this.remote(d);
    this.updateCodec();
    // 새로 Tor로 잇는 상대가 다음 hello(4초)를 기다리지 않고 내 코덱을 알게.
    if (added) void this.sendHello();
  }

  private remote(device: string): Remote {
    let r = this.remotes.get(device);
    if (!r) {
      const created: Remote = {
        device,
        caps: 0,
        chain: Promise.resolve(),
        opusTs: 0,
        reorder: new Reorder((item, lost) => this.play(created, item.codec, item.body, lost)),
      };
      r = created;
      this.remotes.set(device, r);
    }
    return r;
  }

  private dropRemote(r: Remote) {
    r.reorder.close();
    r.node?.disconnect();
    try {
      r.opus?.close();
    } catch {
      // 이미 닫힘
    }
    for (const c of ['mimi', 'dac'] as const) void this.o.neural?.close(this.decStream(r.device, c)).catch(() => undefined);
  }

  private decStream(device: string, codec: NeuralCodecId) {
    return `${this.o.call}:tordec:${device}:${codec}`;
  }

  private updateCodec() {
    const caps = [...this.remotes.values()].map((r) => r.caps);
    const next = this.encodable.length === 0 ? null : pickTorCodec(this.want, this.encodable, caps);
    if (next === this.codec) return;
    const before = this.codec;
    this.codec = next;
    if (before === 'mimi' || before === 'dac') void this.o.neural?.close(`${this.encStream}:${before}`).catch(() => undefined);
    if (before === 'opus') this.closeOpusEncoder();
    this.o.onCodec(next);
    void this.ensureCapture();
  }

  setMic(track: MediaStreamTrack | null) {
    if (track === this.mic) return;
    this.mic = track;
    this.source?.disconnect();
    this.source = null;
    void this.ensureCapture();
  }

  setMuted(muted: boolean) {
    this.muted = muted;
  }

  setDeafened(deafened: boolean) {
    this.deafened = deafened;
    if (this.audio) this.audio.gain.gain.value = deafened ? 0 : this.volume;
  }

  /** 받은 소리를 틀 스피커('' 기본)와 음량(0~1). */
  setOutput(sink: string, volume: number) {
    const sinkChanged = sink !== this.sink;
    this.sink = sink;
    this.volume = volume;
    if (!this.audio) return;
    this.audio.gain.gain.value = this.deafened ? 0 : volume;
    if (sinkChanged) applySink(this.audio.ctx, sink);
  }

  get sendingCodec(): TorCodec | null {
    return this.codec;
  }

  private async ensureCapture() {
    if (!this.mic || !this.codec) return;
    const a = await this.ctx();
    if (!a || !this.mic) return;
    if (!this.capture) {
      this.capture = new AudioWorkletNode(a.ctx, 'yangpa-capture', { processorOptions: { frame: CAPTURE_FRAME }, numberOfOutputs: 0 });
      this.capture.port.onmessage = (e: MessageEvent<Float32Array>) => this.onFrame(e.data);
    }
    if (!this.source) {
      this.source = a.ctx.createMediaStreamSource(new MediaStream([this.mic]));
      this.source.connect(this.capture);
    }
  }

  private async sendHello() {
    if (this.closed) return;
    const mask = this.decodable.reduce((m, c) => m | CAP_BIT[c], 0);
    const sealed = await this.crypt('seal', this.o.myDevice, encodeTorPacket(KIND_HELLO, mask, 0));
    if (sealed) this.post(sealed);
  }

  /** 소리·hello는 Tor로 잇는 상대(`setPeers`)에게만 보냅니다: 서로 신뢰해 WebRTC로 이어진 상대에게는 가지 않습니다. */
  private post(sealed: Uint8Array, to = [...this.remotes.keys()]) {
    if (this.closed || this.sending >= MAX_SENDING || to.length === 0) return;
    this.sending++;
    const params = { space: this.o.space, channel: this.o.channel, call: this.o.call, data: bytesToBase64(sealed), to };
    void this.o.client
      .call('voice.media', params)
      .catch(() => undefined)
      .finally(() => this.sending--);
  }

  private async sendAudio(codec: TorCodec, body: Uint8Array) {
    if (body.length > MAX_PACKET) return;
    const sealed = await this.crypt('seal', this.o.myDevice, encodeTorPacket(KIND_AUDIO, CODEC_BYTE[codec], this.seq++, body));
    if (sealed) this.post(sealed);
  }

  private async sendScreen(body: Uint8Array, to: string[]) {
    if (to.length === 0) return;
    const sealed = await this.crypt('seal', this.o.myDevice, encodeTorPacket(KIND_SCREEN, CODEC_VP8, this.screenSeq++, body));
    if (sealed) this.post(sealed, to);
  }

  private onFrame(pcm: Float32Array) {
    const codec = this.codec;
    if (!codec || this.muted || this.closed || this.remotes.size === 0) return;
    if (codec === 'opus') {
      this.encodeOpus(pcm);
      return;
    }
    const neural = this.o.neural;
    if (!neural || this.backlog >= MAX_ENCODE_BACKLOG) return;
    this.backlog++;
    this.encodeChain = this.encodeChain.then(async () => {
      try {
        const codes = await neural.encode(`${this.encStream}:${codec}`, codec, pcm);
        const per = PACKET_CODES[codec];
        for (let i = 0; i + per <= codes.length; i += per) await this.sendAudio(codec, codesToBytes(codes.subarray(i, i + per)));
      } catch {
        // 한 프레임 실패는 건너뜁니다
      } finally {
        this.backlog--;
      }
    });
  }

  private encodeOpus(pcm: Float32Array) {
    const rate = this.opusRate.enc;
    if (rate === null) return;
    if (!this.opusEnc) {
      try {
        const enc = new AudioEncoder({
          output: (chunk) => {
            const b = new Uint8Array(chunk.byteLength);
            chunk.copyTo(b);
            void this.sendAudio('opus', b);
          },
          error: () => this.closeOpusEncoder(),
        });
        enc.configure({ ...OPUS_CONFIG, sampleRate: rate } as AudioEncoderConfig);
        this.opusEnc = enc;
      } catch {
        return;
      }
    }
    if (this.opusEnc.encodeQueueSize > 8) return;
    try {
      const input = resample(pcm, NEURAL_RATE, rate);
      const data = new AudioData({
        format: 'f32-planar',
        sampleRate: rate,
        numberOfFrames: input.length,
        numberOfChannels: 1,
        timestamp: this.opusTs,
        data: input as Float32Array<ArrayBuffer>,
      });
      this.opusTs += Math.round((pcm.length / NEURAL_RATE) * 1e6);
      this.opusEnc.encode(data);
      data.close();
    } catch {
      this.closeOpusEncoder();
    }
  }

  private closeOpusEncoder() {
    try {
      this.opusEnc?.close();
    } catch {
      // 이미 닫힘
    }
    this.opusEnc = null;
  }

  /** `voice.media` 알림. 그 기기가 봉인한 것만 엽니다. */
  async handle(from: string, data: string) {
    if (this.closed || from === this.o.myDevice) return;
    let raw: Uint8Array;
    try {
      raw = base64ToBytes(data);
    } catch {
      return;
    }
    const plain = await this.crypt('open', from, raw);
    if (!plain || this.closed) return;
    const pkt = decodeTorPacket(plain);
    if (!pkt) return;
    if (pkt.kind === KIND_SCREEN) {
      if (pkt.codec === CODEC_VP8) this.screen.onPart(from, pkt.body);
      return;
    }
    // 지금 참가자가 아닌 기기(나간 뒤 늦게 온 패킷)의 재생기를 새로 만들지 않습니다.
    const r = this.remotes.get(from);
    if (!r) return;
    if (pkt.kind === KIND_HELLO) {
      if (r.caps !== pkt.codec) {
        r.caps = pkt.codec;
        this.updateCodec();
      }
      return;
    }
    const codec = CODEC_OF[pkt.codec];
    if (pkt.kind !== KIND_AUDIO || !codec || !this.decodable.includes(codec)) return;
    r.reorder.push(pkt.seq, { codec, body: pkt.body.slice() });
  }

  private async player(r: Remote): Promise<AudioWorkletNode | null> {
    if (r.node) return r.node;
    const a = await this.ctx();
    if (!a || !this.remotes.has(r.device)) return null;
    if (r.node) return r.node;
    const node = new AudioWorkletNode(a.ctx, 'yangpa-player', {
      numberOfInputs: 0,
      outputChannelCount: [1],
      processorOptions: PLAYER_BUFFER,
    });
    node.connect(a.gain);
    const level = a.ctx.createMediaStreamDestination();
    node.connect(level);
    r.node = node;
    this.o.onRemoteStream(r.device, level.stream);
    return node;
  }

  private play(r: Remote, codec: TorCodec, body: Uint8Array, lost: number) {
    r.chain = r.chain.then(async () => {
      const node = await this.player(r);
      if (!node) return;
      try {
        if (codec === 'opus') {
          this.decodeOpus(r, node, body);
          return;
        }
        const codes = bytesToCodes(body);
        if (!codes || codes.length !== PACKET_CODES[codec] || !this.o.neural) return;
        const pcm = await this.o.neural.decode(this.decStream(r.device, codec), codec, codes, lost);
        if (pcm.length > 0) node.port.postMessage(pcm, [pcm.buffer]);
      } catch {
        // 풀지 못한 패킷은 건너뜀
      }
    });
  }

  private decodeOpus(r: Remote, node: AudioWorkletNode, body: Uint8Array) {
    if (!r.opus) {
      const dec = new AudioDecoder({
        output: (ad) => {
          try {
            const pcm = new Float32Array(ad.numberOfFrames);
            ad.copyTo(pcm, { planeIndex: 0, format: 'f32-planar' });
            const out = resample(pcm, ad.sampleRate);
            node.port.postMessage(out, [out.buffer]);
          } finally {
            ad.close();
          }
        },
        error: () => {
          r.opus = undefined;
        },
      });
      dec.configure({ codec: 'opus', sampleRate: this.opusRate.dec ?? NEURAL_RATE, numberOfChannels: 1 });
      r.opus = dec;
    }
    r.opus.decode(new EncodedAudioChunk({ type: 'key', timestamp: r.opusTs, data: body }));
    r.opusTs += OPUS_FRAME_US;
  }

  async close() {
    if (this.closed) return;
    this.closed = true;
    if (this.helloTimer) clearInterval(this.helloTimer);
    this.screen.close();
    for (const r of this.remotes.values()) this.dropRemote(r);
    this.remotes.clear();
    this.capture?.disconnect();
    this.source?.disconnect();
    this.closeOpusEncoder();
    for (const done of this.pending.values()) done(null);
    this.pending.clear();
    if (this.codec === 'mimi' || this.codec === 'dac') await this.o.neural?.close(`${this.encStream}:${this.codec}`).catch(() => undefined);
    await this.audio?.ctx.close().catch(() => undefined);
  }
}
