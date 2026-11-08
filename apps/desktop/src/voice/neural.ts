// 통화 음성을 신경망 코덱(Mimi·DAC)으로 보내고 받습니다(메시 통화만, 데스크톱 앱만).
//
// 흐름: 마이크 → 24kHz 컨텍스트의 캡처 워클릿(80ms 프레임) → 앱의 Rust 쪽 ONNX Runtime이 코드로 → 프레임 암호화 워커가
// 봉인(통화 미디어 키, 보낸 기기를 AAD에) → 상대마다 데이터 채널(순서·재전송 없음) → 받는 쪽이 열고(보낸 기기로 검사) →
// 코드를 소리로 → 지터 버퍼 재생 워클릿.
//
// 상대가 그 코덱을 풀 수 있다고 알린(hello) 경우에만 그 상대에게 신경망 코덱으로 보내고, 그동안 그 상대로 가는 Opus 트랙은
// 비웁니다(세션이 `onSending`으로 바꿈). 모르는 상대(예전 판·모델 없음)에게는 늘 Opus.
//
// 평문 패킷: [종류 u8: 1 소리, 2 hello][코덱 u8: 1 Mimi, 2 DAC][번호 u32 LE][코드 u16 LE…]. hello의 코덱 칸은 풀 수 있는
// 코덱 비트(1 Mimi, 2 DAC).

import type { NeuralCodecId, NeuralTransport } from '../api/client';
import { applySink } from './devices';
import type { WorkerData, WorkerInMessage } from './e2ee-worker';
import workletUrl from './neural-worklets.ts?worker&url';

export const NEURAL_RATE = 24_000;
/** 캡처 프레임(80ms). DAC는 Rust 쪽이 두 프레임(160ms)을 모아 덩어리 하나를 냅니다. */
export const CAPTURE_FRAME = 1920;
/** 데이터 채널(양쪽이 같은 번호로 미리 정함, 신호 없이 열림). */
export const NEURAL_CHANNEL = { label: 'yangpa-neural', id: 7 } as const;

const KIND_AUDIO = 1;
const KIND_HELLO = 2;
const CODEC_BYTE: Record<NeuralCodecId, number> = { mimi: 1, dac: 2 };
const CODEC_OF: Record<number, NeuralCodecId> = { 1: 'mimi', 2: 'dac' };
/** 패킷 하나의 코드 수. */
export const PACKET_CODES: Record<NeuralCodecId, number> = { mimi: 8, dac: 12 * 32 };
/** 재생 지터 버퍼(샘플): 모일 때까지 기다릴 양과 쌓아 둘 상한. DAC는 덩어리(160ms) 단위로 와서 더 깁니다. */
const PLAYER_BUFFER: Record<NeuralCodecId, { target: number; max: number }> = {
  mimi: { target: 2 * CAPTURE_FRAME, max: 8 * CAPTURE_FRAME },
  dac: { target: 2 * CAPTURE_FRAME, max: 10 * CAPTURE_FRAME },
};
/** hello를 다시 보내는 간격(순서·재전송 없는 채널이라 빠질 수 있음). */
const HELLO_MS = 4000;
/** 이만큼 넘게 밀린 인코딩은 버립니다(이 기기가 실시간으로 못 따라갈 때 지연이 쌓이지 않게). */
const MAX_ENCODE_BACKLOG = 4;

export function encodePacket(kind: number, codec: number, seq: number, codes?: Uint16Array): Uint8Array {
  const n = codes?.length ?? 0;
  const out = new Uint8Array(6 + n * 2);
  const v = new DataView(out.buffer);
  out[0] = kind;
  out[1] = codec;
  v.setUint32(2, seq >>> 0, true);
  for (let i = 0; i < n; i++) v.setUint16(6 + i * 2, codes![i], true);
  return out;
}

export function decodePacket(data: Uint8Array): { kind: number; codec: number; seq: number; codes: Uint16Array } | null {
  if (data.length < 6 || (data.length - 6) % 2 !== 0) return null;
  const v = new DataView(data.buffer, data.byteOffset, data.byteLength);
  const codes = new Uint16Array((data.length - 6) / 2);
  for (let i = 0; i < codes.length; i++) codes[i] = v.getUint16(6 + i * 2, true);
  return { kind: data[0], codec: data[1], seq: v.getUint32(2, true), codes };
}

/** hello의 코덱 비트. */
export function codecMask(codecs: readonly NeuralCodecId[]): number {
  return codecs.reduce((m, c) => m | CODEC_BYTE[c], 0);
}

/** 받은 번호와 지난 번호 사이에서 빠진 패킷 수. 늦게 온(이미 지난) 패킷이면 null(버림). 크게 튀면 0(새로 시작). */
export function lostBetween(last: number | undefined, seq: number): number | null {
  if (last === undefined) return 0;
  const d = (seq - last) >>> 0;
  if (d === 0 || d > 0x8000_0000) return null;
  return d - 1 > 1000 ? 0 : d - 1;
}

interface Peer {
  device: string;
  dc: RTCDataChannel;
  /** 상대가 풀 수 있다고 알린 코덱 비트. */
  caps: number;
  sending: boolean;
  helloTimer: ReturnType<typeof setInterval> | null;
  recv: Map<NeuralCodecId, { last?: number; chain: Promise<void>; node: AudioWorkletNode }>;
}

export interface NeuralLinkOptions {
  transport: NeuralTransport;
  /** 프레임 암호화 워커(세션의 것). 'data' 응답은 세션이 `onWorkerData`로 넘깁니다. */
  worker: Worker;
  myDevice: string;
  call: string;
  /** 그 상대에게 신경망 코덱으로 보내기 시작·멈춤(세션이 Opus 트랙을 비우거나 되돌림). */
  onSending(device: string, sending: boolean): void;
  /** 상대의 신경망 소리를 처음 틀 때(말하기 표시용 스트림). */
  onRemoteStream(device: string, stream: MediaStream): void;
}

export class NeuralLink {
  /** 처음 쓸 때 만듭니다(신경망 코덱을 쓰지 않는 통화에서는 열지 않음). */
  private audio: { ctx: AudioContext; gain: GainNode; ready: Promise<void> } | null = null;
  /** 내가 풀 수 있는 코덱(모델이 있는 것). 모델 확인 전에는 비어 hello가 아무것도 알리지 않습니다. */
  private decodable: NeuralCodecId[] = [];
  private deafened = false;
  /** 스피커 장치('' 기본)와 음량(0~1). */
  private sink = '';
  private volume = 1;
  /** 오디오 장치·워클릿을 못 열었으면 보내지 않습니다(그 상대는 Opus로 되돌아감). */
  private captureFailed = false;
  private readonly peers = new Map<string, Peer>();
  private codec: NeuralCodecId | null = null;
  private mic: MediaStreamTrack | null = null;
  private source: MediaStreamAudioSourceNode | null = null;
  private capture: AudioWorkletNode | null = null;
  private muted = false;
  private seq = 0;
  private backlog = 0;
  private encodeChain: Promise<void> = Promise.resolve();
  private readonly pending = new Map<number, (d: ArrayBuffer | null) => void>();
  private nextId = 1;
  private closed = false;
  private readonly encStream: string;

  constructor(private readonly o: NeuralLinkOptions) {
    this.encStream = `${o.call}:enc:${Math.random().toString(36).slice(2)}`;
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
      // 오디오 장치·워클릿을 못 열면 신경망 코덱 없이(Opus) 갑니다.
      return null;
    }
    if (this.closed) return null;
    if (this.audio.ctx.state === 'suspended') void this.audio.ctx.resume().catch(() => undefined);
    return this.audio;
  }

  /** 모델을 확인한 뒤: 풀 수 있는 코덱을 정하고 상대들에게 다시 알립니다. */
  setDecodable(codecs: NeuralCodecId[]) {
    this.decodable = [...codecs];
    for (const p of this.peers.values()) void this.sendHello(p);
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

  /** 상대 연결에 신경망 데이터 채널을 붙입니다(양쪽이 같은 번호로 만들어 신호 없이 열림). */
  attach(device: string, pc: RTCPeerConnection) {
    if (this.closed || this.peers.has(device)) return;
    let dc: RTCDataChannel;
    try {
      dc = pc.createDataChannel(NEURAL_CHANNEL.label, { negotiated: true, id: NEURAL_CHANNEL.id, ordered: false, maxRetransmits: 0 });
    } catch {
      return;
    }
    dc.binaryType = 'arraybuffer';
    const peer: Peer = { device, dc, caps: 0, sending: false, helloTimer: null, recv: new Map() };
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
    for (const [codec, r] of p.recv) {
      r.node.disconnect();
      void this.o.transport.close(this.decStream(device, codec)).catch(() => undefined);
    }
    if (p.sending) this.o.onSending(device, false);
  }

  private encStreamOf(codec: NeuralCodecId) {
    return `${this.encStream}:${codec}`;
  }

  private decStream(device: string, codec: NeuralCodecId) {
    return `${this.o.call}:dec:${device}:${codec}`;
  }

  private async sendHello(p: Peer) {
    if (p.dc.readyState !== 'open') return;
    const sealed = await this.crypt('seal', this.o.myDevice, encodePacket(KIND_HELLO, codecMask(this.decodable), 0));
    if (sealed && p.dc.readyState === 'open') p.dc.send(sealed as Uint8Array<ArrayBuffer>);
  }

  /** 내가 보낼 신경망 코덱(null이면 Opus). */
  setCodec(codec: NeuralCodecId | null) {
    if (codec === this.codec) return;
    const before = this.codec;
    this.codec = codec;
    // 코덱마다 인코더가 따로라, 그만 쓰는 코덱의 인코더 상태는 버립니다.
    if (before) void this.o.transport.close(this.encStreamOf(before)).catch(() => undefined);
    for (const p of this.peers.values()) this.updateSending(p);
    void this.ensureCapture().catch(() => undefined);
  }

  setMic(track: MediaStreamTrack | null) {
    if (track === this.mic) return;
    this.mic = track;
    this.source?.disconnect();
    this.source = null;
    void this.ensureCapture().catch(() => undefined);
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

  /** 이 상대에게 지금 신경망 코덱으로 보내는지. */
  sendingTo(device: string): boolean {
    return !!this.peers.get(device)?.sending;
  }

  private updateSending(p: Peer) {
    const want = !!this.codec && !this.captureFailed && p.dc.readyState === 'open' && (p.caps & CODEC_BYTE[this.codec]) !== 0;
    if (want === p.sending) return;
    p.sending = want;
    this.o.onSending(p.device, want);
  }

  private async ensureCapture() {
    if (!this.mic || !this.codec) return;
    const a = await this.ctx();
    if (!a) {
      if (!this.closed && !this.captureFailed) {
        this.captureFailed = true;
        for (const p of this.peers.values()) this.updateSending(p);
      }
      return;
    }
    if (!this.mic) return;
    if (!this.capture) {
      this.capture = new AudioWorkletNode(a.ctx, 'yangpa-capture', { processorOptions: { frame: CAPTURE_FRAME }, numberOfOutputs: 0 });
      this.capture.port.onmessage = (e: MessageEvent<Float32Array>) => this.onFrame(e.data);
    }
    if (!this.source) {
      this.source = a.ctx.createMediaStreamSource(new MediaStream([this.mic]));
      this.source.connect(this.capture);
    }
  }

  private onFrame(pcm: Float32Array) {
    const codec = this.codec;
    if (!codec || this.muted || this.closed) return;
    const targets = [...this.peers.values()].filter((p) => p.sending);
    if (targets.length === 0) return;
    if (this.backlog >= MAX_ENCODE_BACKLOG) return;
    this.backlog++;
    this.encodeChain = this.encodeChain.then(async () => {
      try {
        const codes = await this.o.transport.encode(this.encStreamOf(codec), codec, pcm);
        const per = PACKET_CODES[codec];
        for (let i = 0; i + per <= codes.length; i += per) {
          const sealed = await this.crypt('seal', this.o.myDevice, encodePacket(KIND_AUDIO, CODEC_BYTE[codec], this.seq++, codes.subarray(i, i + per)));
          if (!sealed) continue;
          for (const p of this.peers.values()) {
            if (p.sending && p.dc.readyState === 'open') p.dc.send(sealed as Uint8Array<ArrayBuffer>);
          }
        }
      } catch {
        // 한 프레임 실패는 건너뜁니다(모델이 없어졌으면 다음 상태 확인에서 Opus로 돌아감).
      } finally {
        this.backlog--;
      }
    });
  }

  private async onMessage(p: Peer, data: unknown) {
    if (!(data instanceof ArrayBuffer)) return;
    // 그 상대가 봉인한 것만 엽니다(AAD의 보낸 기기).
    const plain = await this.crypt('open', p.device, new Uint8Array(data));
    if (!plain || this.closed) return;
    const pkt = decodePacket(plain);
    if (!pkt) return;
    if (pkt.kind === KIND_HELLO) {
      p.caps = pkt.codec;
      this.updateSending(p);
      return;
    }
    const codec = CODEC_OF[pkt.codec];
    if (pkt.kind !== KIND_AUDIO || !codec || !this.decodable.includes(codec) || pkt.codes.length !== PACKET_CODES[codec]) return;
    const r = await this.receiver(p, codec);
    if (!r) return;
    const lost = lostBetween(r.last, pkt.seq);
    if (lost === null) return;
    r.last = pkt.seq;
    r.chain = r.chain.then(async () => {
      try {
        const pcm = await this.o.transport.decode(this.decStream(p.device, codec), codec, pkt.codes, lost);
        if (pcm.length > 0) r.node.port.postMessage(pcm, [pcm.buffer]);
      } catch {
        // 풀지 못한 패킷은 건너뜀
      }
    });
  }

  private async receiver(p: Peer, codec: NeuralCodecId) {
    const have = p.recv.get(codec);
    if (have) return have;
    const a = await this.ctx();
    if (!a || !this.peers.has(p.device)) return null;
    const again = p.recv.get(codec);
    if (again) return again;
    const node = new AudioWorkletNode(a.ctx, 'yangpa-player', {
      numberOfInputs: 0,
      outputChannelCount: [1],
      processorOptions: PLAYER_BUFFER[codec],
    });
    node.connect(a.gain);
    const level = a.ctx.createMediaStreamDestination();
    node.connect(level);
    const r = { chain: Promise.resolve(), node } as { last?: number; chain: Promise<void>; node: AudioWorkletNode };
    p.recv.set(codec, r);
    this.o.onRemoteStream(p.device, level.stream);
    return r;
  }

  async close() {
    if (this.closed) return;
    this.closed = true;
    for (const d of [...this.peers.keys()]) this.detach(d);
    this.capture?.disconnect();
    this.source?.disconnect();
    for (const done of this.pending.values()) done(null);
    this.pending.clear();
    if (this.codec) await this.o.transport.close(this.encStreamOf(this.codec)).catch(() => undefined);
    await this.audio?.ctx.close().catch(() => undefined);
  }

  /** 지금 신경망 코덱으로 보내는 상대. */
  sendingDevices(): string[] {
    return [...this.peers.values()].filter((p) => p.sending).map((p) => p.device);
  }
}
