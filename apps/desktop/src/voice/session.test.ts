// VoiceSession 협상 시험. 실제 WebRTC 대신 가짜 RTCPeerConnection을 씁니다(원격 설명 전 후보는 거부하고,
// 가짜 SDP의 'm=<mid>:<kind>' 토막마다 받는 트랜시버를 만들어 ontrack을 냄). ICE 재시작은 실제처럼 negotiationneeded를 냅니다.

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type { ApiClient } from '../api/client';
import type { SignalPayload, VoiceJoinResult, VoiceParticipant } from '../api/types';
import { DEFAULT_MEDIA } from './media';
import { VoiceSession, type VoiceSessionOptions } from './session';

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

type Transform = { options: { operation: string; kind: string; sender: string } };

class FakeSender {
  track: unknown = null;
  transform: Transform | undefined;
  params: { encodings: RTCRtpEncodingParameters[]; degradationPreference?: string };
  constructor(encodings: RTCRtpEncodingParameters[]) {
    this.params = { encodings };
  }
  /** 변환 없이 트랙이 붙은 적이 있는지(크로뮴은 그 뒤에 단 변환을 무시함). */
  trackBeforeTransform = false;
  async replaceTrack(t: unknown) {
    if (t && !this.transform) this.trackBeforeTransform = true;
    this.track = t;
  }
  getParameters() {
    return { ...this.params, encodings: this.params.encodings.map((e) => ({ ...e })) };
  }
  async setParameters(p: FakeSender['params']) {
    this.params = p;
  }
}

class FakeTransceiver {
  mid: string | null = null;
  sender: FakeSender;
  receiver: { track: { kind: string }; transform?: Transform };
  direction: string;
  codecs: { mimeType: string }[] | null = null;
  readonly sendEncodings: RTCRtpEncodingParameters[];
  constructor(kind: string, init?: RTCRtpTransceiverInit) {
    this.sendEncodings = init?.sendEncodings ?? [{}];
    this.sender = new FakeSender(this.sendEncodings.map((e) => ({ ...e })));
    this.receiver = { track: { kind } };
    this.direction = init?.direction ?? 'sendrecv';
  }
  setCodecPreferences(c: { mimeType: string }[]) {
    this.codecs = c;
  }
}

class FakePC {
  static all: FakePC[] = [];
  remoteDescription: RTCSessionDescriptionInit | null = null;
  localDescription: RTCSessionDescriptionInit | null = null;
  signalingState: RTCSignalingState = 'stable';
  connectionState: RTCPeerConnectionState = 'new';
  added: string[] = [];
  transceivers: FakeTransceiver[] = [];
  closed = false;
  ontrack: ((e: unknown) => void) | null = null;
  onicecandidate: ((e: { candidate: { candidate: string; sdpMid: string; sdpMLineIndex: number } | null }) => void) | null = null;
  onnegotiationneeded: (() => void) | null = null;
  onconnectionstatechange: (() => void) | null = null;
  config: RTCConfiguration;
  restarts = 0;

  constructor(config: RTCConfiguration = {}) {
    this.config = config;
    FakePC.all.push(this);
  }

  getTransceivers() {
    return this.transceivers;
  }

  addTransceiver(trackOrKind: string | { kind: string }, init?: RTCRtpTransceiverInit) {
    const kind = typeof trackOrKind === 'string' ? trackOrKind : trackOrKind.kind;
    const t = new FakeTransceiver(kind, init);
    if (typeof trackOrKind !== 'string') {
      t.sender.track = trackOrKind;
      t.sender.trackBeforeTransform = true;
    }
    this.transceivers.push(t);
    return t;
  }

  async createOffer() {
    return { type: 'offer' as const, sdp: 'local-offer' };
  }

  async setRemoteDescription(d: RTCSessionDescriptionInit) {
    // 실제처럼 원격 설명 적용에 시간이 걸립니다.
    await sleep(10);
    if (d.type === 'offer') {
      for (const [, mid, kind] of (d.sdp ?? '').matchAll(/m=(\w+):(audio|video)/g)) {
        if (this.transceivers.some((t) => t.mid === mid)) continue;
        const t = new FakeTransceiver(kind, { direction: 'recvonly' });
        t.mid = mid;
        this.transceivers.push(t);
        this.ontrack?.({ track: { kind }, receiver: t.receiver, transceiver: t, streams: [] });
      }
    }
    this.remoteDescription = d;
    this.signalingState = d.type === 'offer' ? 'have-remote-offer' : 'stable';
  }

  async createAnswer() {
    return { type: 'answer' as const, sdp: 'answer-sdp' };
  }

  async setLocalDescription(d: RTCSessionDescriptionInit) {
    if (d.type === 'offer') this.transceivers.forEach((t, i) => (t.mid ??= String(i)));
    this.localDescription = d;
    this.signalingState = d.type === 'offer' ? 'have-local-offer' : 'stable';
  }

  async addIceCandidate(c: RTCIceCandidateInit) {
    if (!this.remoteDescription) throw new Error('InvalidStateError: 원격 설명 없음');
    this.added.push(c.candidate ?? '');
  }

  close() {
    this.closed = true;
  }
  channels: FakeDataChannel[] = [];
  createDataChannel(label: string, init?: RTCDataChannelInit) {
    const dc = new FakeDataChannel(label, init);
    this.channels.push(dc);
    return dc;
  }
  restartIce() {
    this.restarts++;
    // 실제처럼 다음 차례에 협상이 필요하다고 알립니다.
    queueMicrotask(() => this.onnegotiationneeded?.());
  }
}

class FakeDataChannel {
  readyState: RTCDataChannelState = 'connecting';
  binaryType = 'blob';
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

/** 신경망 코덱 캡처가 여는 24kHz 컨텍스트 흉내(워클릿을 붙일 수 있음). */
class FakeAudioContext {
  state = 'running';
  sampleRate = 24000;
  destination = {};
  audioWorklet = { addModule: async () => undefined };
  createGain() {
    return { gain: { value: 1 }, connect() {} };
  }
  createMediaStreamSource() {
    return { connect() {}, disconnect() {} };
  }
  createMediaStreamDestination() {
    return { stream: {} };
  }
  createAnalyser() {
    return { fftSize: 0, getFloatTimeDomainData() {} };
  }
  async resume() {}
  async close() {}
}

class FakeAudioWorkletNode {
  port = { onmessage: null as unknown, postMessage() {} };
  connect() {}
  disconnect() {}
}

class FakeStream {
  constructor(readonly tracks: unknown[] = []) {}
  getAudioTracks() {
    return this.tracks;
  }
  getVideoTracks() {
    return this.tracks;
  }
}

class FakeAudio {
  static all: FakeAudio[] = [];
  autoplay = false;
  muted = false;
  srcObject: FakeStream | null = null;
  removed = false;
  constructor() {
    FakeAudio.all.push(this);
  }
  play() {
    return Promise.resolve();
  }
  remove() {
    this.removed = true;
  }
}

class FakeWorker {
  static last: FakeWorker | null = null;
  onmessage: ((e: { data: unknown }) => void) | null = null;
  /** 받은 키(세대, 키 바이트 길이). */
  keys: Array<{ generation: number; bytes: number }> = [];
  constructor() {
    FakeWorker.last = this;
  }
  /** 봉인·열기는 그대로 돌려줍니다(암호는 frame.test가 봄). */
  postMessage(msg: { type?: string; id?: number; data?: ArrayBuffer; generation?: number; key?: ArrayBuffer }) {
    if (msg?.type === 'setKey') this.keys.push({ generation: msg.generation ?? -1, bytes: msg.key?.byteLength ?? 0 });
    if (msg?.type === 'seal' || msg?.type === 'open') {
      queueMicrotask(() => this.onmessage?.({ data: { type: 'data', id: msg.id, data: msg.data } }));
    }
  }
  terminate() {}
}

class FakeScriptTransform {
  constructor(
    readonly worker: unknown,
    readonly options: { operation: string; kind: string; sender: string },
  ) {}
}

const mic = { kind: 'audio', enabled: true, stop() {}, applyConstraints: async () => undefined };
const screenTrack = { kind: 'video', contentHint: '', onended: null as unknown, stop() {} };

type Sent = { to: string; payload: SignalPayload };
type Params = { to?: string; payload?: SignalPayload };

function session(
  call: (method: string, params: Params) => Promise<unknown>,
  join: Partial<VoiceJoinResult> = {},
  myDevice = 'B',
  opts: Partial<VoiceSessionOptions> = {},
) {
  const client = { call: vi.fn(call) } as unknown as ApiClient;
  const s = new VoiceSession({
    client,
    space: 'sp',
    channel: 'ch',
    // 기본은 'B' > 'A'라 이쪽이 polite(상대 제안을 받는 쪽)입니다.
    myDevice,
    myUser: `u-${myDevice}`,
    join: {
      call: 'c1',
      participants: [],
      media_key: '',
      generation: 0,
      topology: 'mesh',
      ice_servers: [],
      ...join,
    },
    iceServers: [],
    simulated: false,
    onChange: () => undefined,
    ...opts,
  });
  return { s, client };
}

/** 보낸 voice.signal을 모으는 데몬 흉내. `reject`가 참을 돌려주면 그 신호를 거절합니다. */
function recorder(reject: (p: SignalPayload) => boolean = () => false) {
  const sent: Sent[] = [];
  const call = async (method: string, params: Params) => {
    if (method === 'voice.signal' && params.payload) {
      sent.push({ to: params.to ?? '', payload: params.payload });
      if (reject(params.payload)) throw new Error('SFU가 맡지 않은 통화입니다');
    }
    return {};
  };
  const of = <T extends SignalPayload['type']>(type: T) =>
    sent.filter((x) => x.payload.type === type).map((x) => ({ to: x.to, ...(x.payload as Extract<SignalPayload, { type: T }>) }));
  return { sent, call, of };
}

const parts = (...devices: string[]): VoiceParticipant[] =>
  devices.map((device) => ({ user: `u-${device}`, device, muted: false, deafened: false, screen: false }));

const ice = (candidate: string): SignalPayload => ({ type: 'ice', candidate, sdp_mid: '0', sdp_mline_index: 0 });

beforeEach(() => {
  FakePC.all = [];
  FakeAudio.all = [];
  vi.stubGlobal('RTCPeerConnection', FakePC);
  vi.stubGlobal('RTCRtpScriptTransform', FakeScriptTransform);
  vi.stubGlobal('Worker', FakeWorker);
  vi.stubGlobal('MediaStream', FakeStream);
  vi.stubGlobal('Audio', FakeAudio);
  vi.stubGlobal('RTCRtpReceiver', {
    getCapabilities: () => ({ codecs: [{ mimeType: 'video/H264' }, { mimeType: 'video/VP8' }, { mimeType: 'video/rtx' }] }),
  });
  vi.stubGlobal('navigator', {
    mediaDevices: {
      getUserMedia: async () => new FakeStream([mic]),
      getDisplayMedia: async () => new FakeStream([screenTrack]),
    },
  });
});

afterEach(() => {
  vi.unstubAllGlobals();
});

describe('VoiceSession 신호 순서', () => {
  it('제안보다 먼저 오거나 제안 처리 중에 온 ICE 후보를 버리지 않는다', async () => {
    const { s } = session(async () => ({}));
    // 알림은 기다리지 않고 연달아 넘어옵니다(store가 void로 부름).
    const all = [
      s.handleSignal('A', 'c1', ice('cand-before-offer')),
      s.handleSignal('A', 'c1', { type: 'offer', sdp: 'offer-sdp' }),
      s.handleSignal('A', 'c1', ice('cand-after-offer')),
    ];
    await Promise.all(all);
    expect(FakePC.all).toHaveLength(1);
    expect(FakePC.all[0].added).toEqual(['cand-before-offer', 'cand-after-offer']);
  });

  it('다른 통화의 신호와 떠난 기기의 모아 둔 후보는 쓰지 않는다', async () => {
    const { s } = session(async () => ({}));
    await s.handleSignal('A', 'other-call', ice('wrong-call'));
    await s.handleSignal('A', 'c1', ice('stale'));
    await s.handleSignal('A', 'c1', { type: 'leave' });
    await s.handleSignal('A', 'c1', { type: 'offer', sdp: 'offer-sdp' });
    expect(FakePC.all[0].added).toEqual([]);
  });

  it('후보는 앞 신호(Tor로 느린 응답)를 기다리지 않고 바로 나간다', async () => {
    const delivered: string[] = [];
    const { s } = session(async (method, params) => {
      if (method !== 'voice.signal' || !params.payload) return {};
      // 응답(answer) 전송이 느린 경우
      if (params.payload.type === 'answer') await sleep(30);
      delivered.push(params.payload.type === 'ice' ? `ice:${params.payload.candidate}` : params.payload.type);
      return {};
    });
    await s.handleSignal('A', 'c1', { type: 'offer', sdp: 'offer-sdp' });
    FakePC.all[0].onicecandidate?.({ candidate: { candidate: 'local-1', sdpMid: '0', sdpMLineIndex: 0 } });
    // 둘 다 나갈 때까지 기다린 뒤 순서를 봅니다.
    await vi.waitFor(() => expect(delivered).toHaveLength(2));
    expect(delivered).toEqual(['ice:local-1', 'answer']);
  });

  it('제안·응답보다 먼저 와서 넣지 못한 후보는 다음 원격 설명 뒤에 다시 넣는다', async () => {
    const { s } = session(async () => ({}));
    await s.handleSignal('A', 'c1', { type: 'offer', sdp: 'offer-sdp' });
    const pc = FakePC.all[0];
    const add = pc.addIceCandidate.bind(pc);
    let fail = true;
    pc.addIceCandidate = async (c: RTCIceCandidateInit) => {
      if (fail) throw new Error('ufrag가 다름');
      return add(c);
    };
    await s.handleSignal('A', 'c1', ice('restarted'));
    fail = false;
    await s.handleSignal('A', 'c1', { type: 'offer', sdp: 'offer-sdp-2' });
    expect(pc.added).toEqual(['restarted']);
  });

  it('같은 기기가 다시 참가하면(나감 알림 뒤에도 참가자) 지난 연결을 버리고 새로 잇는다', async () => {
    const { s } = session(async () => ({}), { topology: 'mesh', participants: parts('A', 'B') }, 'A');
    await s.start();
    const old = FakePC.all[0];
    await s.handleSignal('B', 'c1', { type: 'leave' });
    expect(old.closed).toBe(true);
    expect(FakePC.all).toHaveLength(2);
    expect(FakePC.all[1].closed).toBe(false);
    await s.leave();
  });
});

describe('VoiceSession 프레임 출처', () => {
  it('메시에서는 받는 트랙을 상대 기기로, 보내는 트랙을 내 기기로 묶는다', async () => {
    const { s } = session(async () => ({}), { topology: 'mesh', participants: parts('A', 'B') });
    await s.start();
    await s.handleSignal('A', 'c1', { type: 'offer', sdp: 'm=0:audio m=1:video' });
    const pc = FakePC.all[0];
    expect(pc.transceivers.map((t) => t.receiver.transform?.options)).toEqual([
      { operation: 'decode', kind: 'audio', sender: 'A' },
      { operation: 'decode', kind: 'video', sender: 'A' },
    ]);
    expect(pc.transceivers.map((t) => t.sender.transform?.options.sender)).toEqual(['B', 'B']);
    await s.leave();
  });

  it('먼저 제안하는 쪽은 트랜시버를 만들 때 바로 복호화를 붙인다(ontrack을 기다리면 크로뮴이 적용하지 않음)', async () => {
    // 'A' < 'B'라 A가 먼저 제안합니다.
    const { s } = session(async () => ({}), { topology: 'mesh', participants: parts('A', 'B') }, 'A');
    await s.start();
    const pc = FakePC.all[0];
    expect(pc.transceivers.map((t) => t.receiver.transform?.options)).toEqual([
      { operation: 'decode', kind: 'audio', sender: 'B' },
      { operation: 'decode', kind: 'video', sender: 'B' },
      { operation: 'decode', kind: 'audio', sender: 'B' },
    ]);
    const before = pc.transceivers.map((t) => t.receiver.transform);
    // 응답이 와서 ontrack이 나도 변환을 다시 만들지 않습니다.
    await s.handleSignal('B', 'c1', { type: 'answer', sdp: 'm=0:audio m=1:video' });
    expect(pc.transceivers.map((t) => t.receiver.transform)).toEqual(before);
    await s.leave();
  });
});

describe('VoiceSession 변환을 다는 순서', () => {
  // 크로뮴은 트랙이 붙은 송신기에 나중에 단 변환과, 협상 도중 늦게 단 수신기 변환을 무시해 암호문이 그대로 디코더에 들어갑니다.
  it('먼저 제안하는 쪽은 트랜시버를 만들자마자 송·수신 변환을 달고 마이크는 그 뒤에 넣는다', async () => {
    const { s } = session(async () => ({}), { topology: 'mesh', participants: parts('A', 'B') }, 'A');
    await s.start();
    const pc = FakePC.all[0];
    expect(pc.transceivers.map((t) => t.receiver.transform?.options)).toEqual([
      { operation: 'decode', kind: 'audio', sender: 'B' },
      { operation: 'decode', kind: 'video', sender: 'B' },
      { operation: 'decode', kind: 'audio', sender: 'B' },
    ]);
    expect(pc.transceivers[0].sender.track).toBe(mic);
    // 셋째(둘째 오디오)는 화면 소리 자리라 마이크를 넣지 않습니다.
    expect(pc.transceivers[2].sender.track).toBeNull();
    expect(pc.transceivers.some((t) => t.sender.trackBeforeTransform)).toBe(false);
    await s.leave();
  });

  it('응답하는 쪽도 제안에서 생긴 송신기에 변환을 단 뒤 마이크를 넣는다', async () => {
    const { s } = session(async () => ({}), { topology: 'mesh', participants: parts('A', 'B') });
    await s.start();
    await s.handleSignal('A', 'c1', { type: 'offer', sdp: 'm=0:audio m=1:video' });
    const pc = FakePC.all[0];
    expect(pc.transceivers[0].sender.track).toBe(mic);
    expect(pc.transceivers.some((t) => t.sender.trackBeforeTransform)).toBe(false);
    await s.leave();
  });

  it('SFU 송신기도 변환을 단 뒤 마이크를 넣는다', async () => {
    const { s } = session(async () => ({}), { topology: 'sfu', sfu_host: 'H', participants: parts('A', 'B') });
    await s.start();
    const pc = FakePC.all[0];
    expect(pc.transceivers[0].sender.track).toBe(mic);
    expect(pc.transceivers.some((t) => t.sender.trackBeforeTransform)).toBe(false);
    await s.leave();
  });

  it('내 화면 미리보기는 공유하는 동안 같은 스트림이다(다시 그릴 때마다 새로 붙어 깜박이지 않게)', async () => {
    const { s } = session(async () => ({}), { topology: 'mesh', participants: parts('A', 'B') });
    await s.start();
    expect(s.localScreen()).toBeUndefined();
    await s.startScreenShare('detail');
    const first = s.localScreen();
    expect(first).toBeDefined();
    expect(s.localScreen()).toBe(first);
    await s.stopScreenShare();
    expect(s.localScreen()).toBeUndefined();
    await s.leave();
  });

  it('화면 인코딩 값이 그대로면 setParameters를 다시 부르지 않는다(인코더가 다시 설정되며 끊기지 않게)', async () => {
    const r = recorder();
    const { s } = session(r.call, { topology: 'sfu', sfu_host: 'H', participants: parts('A', 'B') });
    await s.start();
    const sender = FakePC.all[0].transceivers[1].sender;
    const spy = vi.spyOn(sender, 'setParameters');
    await s.startScreenShare('motion');
    await s.handleSignal('A', 'c1', { type: 'watch_screen', watching: true });
    expect(spy).toHaveBeenCalledTimes(1);
    s.setContentHint('motion');
    await s.handleSignal('A', 'c1', { type: 'watch_screen', watching: true });
    expect(spy).toHaveBeenCalledTimes(1);
    s.setContentHint('detail');
    expect(spy).toHaveBeenCalledTimes(2);
    await s.leave();
  });
});

describe('VoiceSession 세부 설정(media)', () => {
  const offer = ['m=0:audio m=1:video', 'a=rtpmap:111 opus/48000/2', 'a=fmtp:111 minptime=10', ''].join('\r\n');

  it('상대 설명의 Opus fmtp에 내 설정을 넣어 적용하고(내 인코더가 따름), 마이크 비트레이트를 건다', async () => {
    const media = { ...DEFAULT_MEDIA, audio: { ...DEFAULT_MEDIA.audio, bitrateKbps: 64, stereo: true, ptime: 60 as const } };
    const { s } = session(async () => ({}), { topology: 'mesh', participants: parts('A', 'B') }, 'B', { media });
    await s.start();
    await s.handleSignal('A', 'c1', { type: 'offer', sdp: offer });
    const pc = FakePC.all[0];
    expect(pc.remoteDescription?.sdp).toContain('a=fmtp:111 minptime=10;usedtx=1;useinbandfec=1;stereo=1;sprop-stereo=1;ptime=60;maxaveragebitrate=64000');
    expect(pc.transceivers[0].sender.params.encodings[0].maxBitrate).toBe(64_000);
    await s.leave();
  });

  it('통화 중에 바꾸면 화면 해상도·프레임·비트레이트와 음성 비트레이트가 바로 바뀐다', async () => {
    const { s } = session(async () => ({}), { topology: 'mesh', participants: parts('A', 'B') });
    await s.start();
    await s.handleSignal('A', 'c1', { type: 'offer', sdp: offer });
    await s.startScreenShare('motion');
    await s.handleSignal('A', 'c1', { type: 'watch_screen', watching: true });
    const [audioTx, videoTx] = FakePC.all[0].transceivers;
    expect(videoTx.sender.params.encodings[0]).toMatchObject({ maxBitrate: 4_000_000, maxFramerate: 60, scaleResolutionDownBy: 1 });
    s.setMedia({
      ...DEFAULT_MEDIA,
      screen: { ...DEFAULT_MEDIA.screen, fps: 30, bitrateKbps: 1500, resolution: 720, degradation: 'maintain-resolution' },
      audio: { ...DEFAULT_MEDIA.audio, bitrateKbps: 32 },
    });
    // 가짜 화면 트랙은 원본 크기를 모르므로 축소 배율은 1.
    expect(videoTx.sender.params.encodings[0]).toMatchObject({ maxBitrate: 1_500_000, maxFramerate: 30, scaleResolutionDownBy: 1 });
    expect(videoTx.sender.params.degradationPreference).toBe('maintain-resolution');
    expect(audioTx.sender.params.encodings[0].maxBitrate).toBe(32_000);
    s.setMedia(DEFAULT_MEDIA);
    expect(audioTx.sender.params.encodings[0].maxBitrate).toBeUndefined();
    await s.leave();
  });

  it('통화 중에 Opus 협상 값(FEC 등)을 바꾸면 바로 다시 제안한다', async () => {
    const sent: string[] = [];
    const { s } = session(
      async (method, params) => {
        if (method === 'voice.signal' && params.payload) sent.push(params.payload.type);
        return {};
      },
      { topology: 'mesh', participants: parts('A', 'B') },
    );
    await s.start();
    await s.handleSignal('A', 'c1', { type: 'offer', sdp: offer });
    s.setMedia({ ...DEFAULT_MEDIA, audio: { ...DEFAULT_MEDIA.audio, fec: false } });
    await vi.waitFor(() => expect(sent).toContain('offer'));
    await s.leave();
  });
});

describe('VoiceSession 신경망 코덱', () => {
  const tick = () => new Promise((r) => setTimeout(r, 0));

  function neuralSession(codec: 'mimi' | 'opus', ready: Array<'mimi' | 'dac'>) {
    const neural = {
      status: vi.fn(async () => ({ dir: 'm', codecs: (['mimi', 'dac'] as const).map((c) => ({ codec: c, missing: ready.includes(c) ? [] : ['x'] })) })),
      encode: vi.fn(async () => new Uint16Array(0)),
      decode: vi.fn(async () => new Float32Array(0)),
      close: vi.fn(async () => undefined),
    };
    const client = { call: vi.fn(async () => ({})), transport: { neural } } as unknown as ApiClient;
    const media = { ...DEFAULT_MEDIA, audio: { ...DEFAULT_MEDIA.audio, codec } };
    const s = new VoiceSession({
      client,
      space: 'sp',
      channel: 'ch',
      myDevice: 'A',
      myUser: 'u-A',
      join: { call: 'c1', participants: parts('A', 'B'), media_key: '', generation: 0, topology: 'mesh', ice_servers: [] },
      iceServers: [],
      simulated: false,
      media,
      onChange: () => undefined,
    });
    return { s, neural };
  }

  const hello = (mask: number) => {
    const b = new Uint8Array(6);
    b[0] = 2;
    b[1] = mask;
    return b.buffer;
  };

  it('상대 연결마다 미리 정한 번호의 데이터 채널을 붙이고, 열리면 풀 수 있는 코덱을 알린다', async () => {
    const { s } = neuralSession('mimi', ['mimi']);
    await s.start();
    await tick();
    const dc = FakePC.all[0].channels[0];
    expect(dc.init).toMatchObject({ negotiated: true, id: 7, ordered: false, maxRetransmits: 0 });
    expect(s.snapshot.neuralReady).toEqual(['mimi']);
    dc.open();
    await tick();
    // hello: 종류 2, 코덱 비트 1(Mimi)
    expect([...dc.sent[0].slice(0, 2)]).toEqual([2, 1]);
    await s.leave();
  });

  it('상대가 같은 코덱을 풀 수 있다고 하면 그 상대의 Opus 트랙을 비우고, 채널이 닫히면 되돌린다', async () => {
    vi.stubGlobal('AudioContext', FakeAudioContext);
    vi.stubGlobal('AudioWorkletNode', FakeAudioWorkletNode);
    const { s } = neuralSession('mimi', ['mimi', 'dac']);
    await s.start();
    await tick();
    const pc = FakePC.all[0];
    const dc = pc.channels[0];
    const audio = pc.transceivers[0].sender;
    expect(audio.track).toBe(mic);
    dc.open();
    dc.onmessage?.({ data: hello(2) });
    await tick();
    // 상대는 DAC만: 내 코덱(Mimi)을 못 풀어 Opus 그대로.
    expect(audio.track).toBe(mic);
    expect(s.snapshot.neural).toBeUndefined();
    dc.onmessage?.({ data: hello(3) });
    await tick();
    expect(audio.track).toBeNull();
    expect(s.snapshot.neural).toEqual({ codec: 'mimi', to: ['B'] });
    dc.close();
    dc.onclose?.();
    await tick();
    expect(audio.track).toBe(mic);
    expect(s.snapshot.neural).toBeUndefined();
    await s.leave();
  });

  it('오디오 장치를 못 열면 Opus로 되돌린다(소리가 끊기지 않게)', async () => {
    const { s } = neuralSession('mimi', ['mimi']);
    await s.start();
    await tick();
    const pc = FakePC.all[0];
    const dc = pc.channels[0];
    dc.open();
    dc.onmessage?.({ data: hello(1) });
    await tick();
    await tick();
    // 이 시험 환경에는 AudioContext가 없어 캡처를 못 엶 → 그 상대에게는 Opus.
    expect(pc.transceivers[0].sender.track).toBe(mic);
    expect(s.snapshot.neural).toBeUndefined();
    await s.leave();
  });

  it('Opus를 골랐거나 모델이 없으면 신경망 코덱으로 보내지 않는다', async () => {
    for (const [codec, ready] of [
      ['opus', ['mimi']],
      ['mimi', []],
    ] as const) {
      FakePC.all = [];
      const { s } = neuralSession(codec, [...ready]);
      await s.start();
      await tick();
      const dc = FakePC.all[0].channels[0];
      dc.open();
      dc.onmessage?.({ data: hello(3) });
      await tick();
      expect(FakePC.all[0].transceivers[0].sender.track).toBe(mic);
      expect(s.snapshot.neural).toBeUndefined();
      await s.leave();
    }
  });
});

describe('VoiceSession SFU', () => {
  it('호스트 하나에 송신 트랙을 담아 한 번 제안하고, 받는 트랙마다 출처를 가려 복호화 변환을 붙인다', async () => {
    const r = recorder();
    const { s } = session(r.call, { topology: 'sfu', sfu_host: 'H', sfu_host_user: 'u-seeder', participants: parts('A', 'B', 'C') });
    await s.start();
    // 메시 연결 없이 호스트에게 하나.
    expect(FakePC.all).toHaveLength(1);
    const pc = FakePC.all[0];
    await vi.waitFor(() => expect(r.of('sfu_offer')).toHaveLength(1));
    const offer = r.of('sfu_offer')[0];
    expect(offer.to).toBe('H');
    expect(offer.tracks).toEqual([
      { mid: '0', source: 'audio' },
      { mid: '1', source: 'screen' },
    ]);
    const [audioTx, screenTx] = pc.transceivers;
    expect(audioTx.direction).toBe('sendonly');
    expect(audioTx.sender.track).toBe(mic);
    // 화면공유 simulcast: 낮은 층부터 q·h·f.
    expect(screenTx.sendEncodings.map((e) => [e.rid, e.scaleResolutionDownBy])).toEqual([
      ['q', 4],
      ['h', 2],
      ['f', 1],
    ]);
    expect(screenTx.codecs?.map((c) => c.mimeType)).toEqual(['video/VP8', 'video/rtx']);
    // 보내는 트랙마다 프레임 암호화(내 기기를 AAD에 묶음).
    expect(audioTx.sender.transform?.options).toEqual({ operation: 'encode', kind: 'audio', sender: 'B' });
    expect(screenTx.sender.transform?.options).toEqual({ operation: 'encode', kind: 'video', sender: 'B' });
    expect(s.snapshot).toMatchObject({ topology: 'sfu', sfuHost: 'H', sfuHostUser: 'u-seeder' });

    // 내 ICE 후보는 세션 번호를 달고 제안 뒤에 나갑니다.
    pc.onicecandidate?.({ candidate: { candidate: 'cand-1', sdpMid: '0', sdpMLineIndex: 0 } });
    await vi.waitFor(() => expect(r.of('sfu_ice')).toEqual([expect.objectContaining({ to: 'H', session: offer.session, candidate: 'cand-1' })]));
    expect(r.sent.findIndex((x) => x.payload.type === 'sfu_offer')).toBeLessThan(r.sent.findIndex((x) => x.payload.type === 'sfu_ice'));

    await s.handleSignal('H', 'c1', { type: 'sfu_answer', session: offer.session, sdp: 'answer' });
    expect(pc.remoteDescription?.type).toBe('answer');

    // 호스트의 재협상 제안: A·C의 소리와 A의 화면.
    await s.handleSignal('H', 'c1', {
      type: 'sfu_offer',
      session: offer.session,
      sdp: 'm=2:audio m=3:audio m=4:video',
      tracks: [
        { mid: '2', device: 'A', source: 'audio' },
        { mid: '3', device: 'C', source: 'audio' },
        { mid: '4', device: 'A', source: 'screen' },
      ],
    });
    await vi.waitFor(() => expect(r.of('sfu_answer')).toEqual([expect.objectContaining({ to: 'H', session: offer.session })]));
    const recv = pc.transceivers.slice(2);
    // 트랙 표의 출처로 묶어, 호스트가 다른 참가자의 프레임을 붙이면 버려집니다.
    expect(recv.map((t) => t.receiver.transform?.options)).toEqual([
      { operation: 'decode', kind: 'audio', sender: 'A' },
      { operation: 'decode', kind: 'audio', sender: 'C' },
      { operation: 'decode', kind: 'video', sender: 'A' },
    ]);
    // 보낸 참가자(받는 트랙)마다 따로 변환.
    expect(new Set(recv.map((t) => t.receiver.transform)).size).toBe(3);
    expect(FakeAudio.all).toHaveLength(2);
    expect(FakeAudio.all.every((a) => a.autoplay && a.srcObject)).toBe(true);

    // A가 화면공유를 시작하면 누르지 않아도 봅니다: 호스트에 층 선택, 화질 바꾸기, 그만 보기.
    const sharing = parts('A', 'B', 'C').map((p) => (p.device === 'A' ? { ...p, screen: true } : p));
    s.updateParticipants(sharing, 0, '', { topology: 'sfu', sfuHost: 'H' });
    expect(s.snapshot.watching).toEqual(['A']);
    expect(s.remoteScreen('A')).toBeInstanceOf(FakeStream);
    s.setScreenLayer('A', 'low');
    await s.watch('A', false);
    // '그만 보기'한 공유는 다시 켜지 않고, 공유를 멈췄다 다시 시작하면 다시 봅니다.
    s.updateParticipants(sharing, 0, '', { topology: 'sfu', sfuHost: 'H' });
    expect(s.snapshot.watching).toEqual([]);
    await vi.waitFor(() => expect(r.of('sfu_select').map((x) => x.layer)).toEqual(['auto', 'low', 'off']));
    s.updateParticipants(parts('A', 'B', 'C'), 0, '', { topology: 'sfu', sfuHost: 'H' });
    s.updateParticipants(sharing, 0, '', { topology: 'sfu', sfuHost: 'H' });
    expect(s.snapshot.watching).toEqual(['A']);
    await s.watch('A', false);
    // 고른 화질은 그 기기에 남습니다.
    await vi.waitFor(() => expect(r.of('sfu_select').map((x) => x.layer)).toEqual(['auto', 'low', 'off', 'low', 'off']));
    expect(r.of('sfu_select').every((x) => x.to === 'H' && x.device === 'A' && x.source === 'screen')).toBe(true);
    // SFU에서는 메시의 watch_screen 신호를 따로 보내지 않습니다(voice.watch가 알림).
    expect(r.of('watch_screen')).toEqual([]);

    // 호스트가 아닌 기기의 SFU 신호, 지난 세션의 제안, 메시 제안은 무시.
    await s.handleSignal('A', 'c1', { type: 'sfu_offer', session: offer.session, sdp: 'm=9:audio', tracks: [{ mid: '9', device: 'A', source: 'audio' }] });
    await s.handleSignal('H', 'c1', { type: 'sfu_offer', session: offer.session - 1, sdp: 'm=8:audio', tracks: [] });
    await s.handleSignal('A', 'c1', { type: 'offer', sdp: 'offer-sdp' });
    expect(FakePC.all).toHaveLength(1);
    expect(pc.transceivers).toHaveLength(5);

    // C가 나가 호스트가 C의 m-line을 멈추면 C의 소리를 치웁니다.
    await s.handleSignal('H', 'c1', {
      type: 'sfu_offer',
      session: offer.session,
      sdp: 'm=2:audio m=4:video',
      tracks: [
        { mid: '2', device: 'A', source: 'audio' },
        { mid: '4', device: 'A', source: 'screen' },
      ],
    });
    expect(FakeAudio.all.map((a) => a.removed)).toEqual([false, true]);

    await s.leave();
    expect(pc.closed).toBe(true);
    await vi.waitFor(() => expect(r.of('sfu_close')).toEqual([expect.objectContaining({ to: 'H', session: offer.session })]));
  });

  it('화면은 보는 사람이 있을 때만 호스트에 올리고 층마다 비트레이트를 다르게 둔다', async () => {
    const r = recorder();
    const { s } = session(r.call, { topology: 'sfu', sfu_host: 'H', participants: parts('A', 'B') });
    await s.start();
    const screenTx = FakePC.all[0].transceivers[1];
    await s.startScreenShare('motion');
    // 미리보기는 같은 스트림이라 다시 그려도 영상을 새로 붙이지 않습니다(깜박임).
    expect(s.localScreen()).toBeDefined();
    expect(s.localScreen()).toBe(s.localScreen());
    expect(screenTx.sender.track).toBeNull();
    await s.handleSignal('A', 'c1', { type: 'watch_screen', watching: true });
    expect(s.snapshot.viewers).toEqual(['A']);
    expect(screenTx.sender.track).toBe(screenTrack);
    expect(screenTx.sender.params.encodings.map((e) => [e.rid, e.maxBitrate, e.maxFramerate])).toEqual([
      ['q', 500_000, 30],
      ['h', 1_500_000, 60],
      ['f', 4_000_000, 60],
    ]);
    s.setContentHint('detail');
    // 글자 위주도 30fps, 대역이 모자라면 해상도와 프레임을 함께 줄임(큰 화면에서 1~2fps로 떨어지지 않게).
    expect(screenTx.sender.params.degradationPreference).toBe('balanced');
    expect(screenTx.sender.params.encodings.map((e) => [e.maxBitrate, e.maxFramerate])).toEqual([
      [300_000, 30],
      [900_000, 30],
      [2_500_000, 30],
    ]);
    await s.handleSignal('A', 'c1', { type: 'watch_screen', watching: false });
    expect(screenTx.sender.track).toBeNull();
    await s.leave();
  });

  it('메시 ↔ SFU 전환, 호스트 넘겨주기, 거절·닫힘 뒤 더 큰 세션으로 다시 붙는다', async () => {
    let rejectNext = false;
    const r = recorder((p) => {
      if (p.type !== 'sfu_offer' || !rejectNext) return false;
      rejectNext = false;
      return true;
    });
    const { s } = session(r.call, { topology: 'mesh', participants: parts('A', 'B', 'C') });
    await s.start();
    // 메시: 기기 ID가 작은 B가 C에게 먼저 연결을 엽니다(A는 B에게).
    expect(FakePC.all).toHaveLength(1);
    const mesh = FakePC.all[0];
    expect(s.snapshot.topology).toBe('mesh');

    // SFU로: 메시 연결을 닫고 호스트 C 하나에.
    s.updateParticipants(parts('A', 'B', 'C'), 0, '', { topology: 'sfu', sfuHost: 'C' });
    expect(mesh.closed).toBe(true);
    await vi.waitFor(() => expect(r.of('sfu_offer')).toHaveLength(1));
    expect(r.of('sfu_offer')[0].to).toBe('C');
    expect(s.snapshot.connection).toEqual({ A: 'new', C: 'new' });

    // 호스트가 떠나 넘겨받는 중: 연결을 닫고 기다립니다.
    s.updateParticipants(parts('A', 'B'), 0, '', { topology: 'sfu', sfuHostUser: 'u-C' });
    expect(FakePC.all[1].closed).toBe(true);
    expect(s.snapshot).toMatchObject({ sfuState: 'waiting', sfuHost: undefined, sfuHostUser: undefined });

    // 새 호스트 A. 첫 제안은 거절(호스트가 아직 통화방을 열지 않음) → 잠시 뒤 더 큰 세션으로 다시.
    rejectNext = true;
    s.updateParticipants(parts('A', 'B'), 0, '', { topology: 'sfu', sfuHost: 'A' });
    await vi.waitFor(() => expect(r.of('sfu_offer')).toHaveLength(3), { timeout: 3000 });
    const [first, rejected, retried] = r.of('sfu_offer');
    expect([rejected.to, retried.to]).toEqual(['A', 'A']);
    expect(rejected.session).toBeGreaterThan(first.session);
    expect(retried.session).toBeGreaterThan(rejected.session);
    expect(FakePC.all.filter((pc) => !pc.closed)).toHaveLength(1);

    // 호스트가 세션을 닫으면 다시 붙습니다.
    await s.handleSignal('A', 'c1', { type: 'sfu_close', session: retried.session });
    expect(s.snapshot.sfuState).toBe('waiting');
    await vi.waitFor(() => expect(r.of('sfu_offer')).toHaveLength(4), { timeout: 3000 });
    expect(r.of('sfu_offer')[3].session).toBeGreaterThan(retried.session);

    // 메시로 돌아가면 SFU 연결을 닫고 다시 참가자끼리.
    const sfuPc = FakePC.all.at(-1)!;
    s.updateParticipants(parts('B', 'C'), 0, '', { topology: 'mesh' });
    expect(sfuPc.closed).toBe(true);
    expect(FakePC.all.at(-1)).not.toBe(sfuPc);
    expect(FakePC.all.at(-1)!.closed).toBe(false);
    expect(s.snapshot).toMatchObject({ topology: 'mesh', sfuHost: undefined, sfuState: undefined });
    await s.leave();
  });

  it('호스트의 재협상에 낸 응답이 닿지 않으면 응답 시한을 기다리지 않고 더 큰 세션으로 다시 붙는다', async () => {
    let rejectAnswer = true;
    const r = recorder((p) => {
      if (p.type !== 'sfu_answer' || !rejectAnswer) return false;
      rejectAnswer = false;
      return true;
    });
    const { s } = session(r.call, { topology: 'sfu', sfu_host: 'H', participants: parts('A', 'B') });
    await s.start();
    await vi.waitFor(() => expect(r.of('sfu_offer')).toHaveLength(1));
    const offer = r.of('sfu_offer')[0];
    await s.handleSignal('H', 'c1', { type: 'sfu_answer', session: offer.session, sdp: 'answer' });
    await s.handleSignal('H', 'c1', {
      type: 'sfu_offer',
      session: offer.session,
      sdp: 'm=2:audio',
      tracks: [{ mid: '2', device: 'A', source: 'audio' }],
    });
    await vi.waitFor(() => expect(r.of('sfu_answer')).toHaveLength(1));
    await vi.waitFor(() => expect(r.of('sfu_offer')).toHaveLength(2), { timeout: 3000 });
    expect(FakePC.all[0].closed).toBe(true);
    expect(r.of('sfu_offer')[1].session).toBeGreaterThan(offer.session);
    expect(FakePC.all.filter((pc) => !pc.closed)).toHaveLength(1);
    await s.leave();
  });

  it('출처가 트랙 표에 없는 트랙은 복호화하지도 틀지도 않는다', async () => {
    const r = recorder();
    const { s } = session(r.call, { topology: 'sfu', sfu_host: 'H', participants: parts('A', 'B') });
    await s.start();
    await vi.waitFor(() => expect(r.of('sfu_offer')).toHaveLength(1));
    const offer = r.of('sfu_offer')[0];
    await s.handleSignal('H', 'c1', { type: 'sfu_answer', session: offer.session, sdp: 'answer' });
    await s.handleSignal('H', 'c1', { type: 'sfu_offer', session: offer.session, sdp: 'm=2:audio', tracks: [] });
    const recv = FakePC.all[0].transceivers[2];
    expect(recv.receiver.transform).toBeUndefined();
    expect(FakeAudio.all).toHaveLength(0);
    await s.leave();
  });

  it('내가 호스트면 내 데몬이 넘긴 신호(from = 나)로 협상한다', async () => {
    const r = recorder();
    const { s } = session(r.call, { topology: 'sfu', sfu_host: 'B', participants: parts('A', 'B') });
    await s.start();
    await vi.waitFor(() => expect(r.of('sfu_offer')).toHaveLength(1));
    const offer = r.of('sfu_offer')[0];
    expect(offer.to).toBe('B');
    await s.handleSignal('B', 'c1', { type: 'sfu_answer', session: offer.session, sdp: 'answer' });
    expect(FakePC.all[0].remoteDescription?.type).toBe('answer');
    // 메시 신호는 나에게서 오면 여전히 버립니다.
    await s.handleSignal('B', 'c1', { type: 'watch_screen', watching: true });
    expect(s.snapshot.viewers).toEqual([]);
    await s.leave();
  });
});

describe('VoiceSession 화면공유(메시)', () => {
  it('ICE를 다시 시작하지 않고 보기를 누른 상대에게 바로 화면을 붙인다', async () => {
    const r = recorder();
    const { s } = session(r.call, { topology: 'mesh', participants: parts('A', 'B') }, 'B');
    await s.start();
    await s.handleSignal('A', 'c1', { type: 'offer', sdp: 'm=0:audio m=1:video' });
    await s.handleSignal('A', 'c1', { type: 'watch_screen', watching: true });
    const pc = FakePC.all[0];
    expect(pc.config.iceTransportPolicy).toBeUndefined();
    await s.startScreenShare('detail');
    expect(pc.transceivers[1].sender.track).toBe(screenTrack);
    expect(pc.restarts).toBe(0);
    await s.stopScreenShare();
    expect(pc.transceivers[1].sender.track).toBeNull();
    expect(pc.restarts).toBe(0);
    await s.leave();
  });
});

describe('통화 키', () => {
  it('세대 번호가 같아도 데몬이 알려 준 키를 늘 워커에 넘긴다(빈 키는 넘기지 않음)', async () => {
    const { s } = session(async () => ({}), { participants: parts('A', 'B'), generation: 1, media_key: btoa('k'.repeat(32)) });
    await s.start();
    const w = FakeWorker.last!;
    expect(w.keys).toEqual([{ generation: 1, bytes: 32 }]);
    // 관리 기기가 바뀌며 같은 세대 번호로 새 키를 냄: 예전에는 세대가 같아 넘기지 않아 이 기기만 옛 키로 보냈습니다.
    s.updateParticipants(parts('A', 'B'), 1, btoa('n'.repeat(32)));
    s.updateParticipants(parts('A', 'B'), 0, '');
    expect(w.keys).toEqual([
      { generation: 1, bytes: 32 },
      { generation: 1, bytes: 32 },
    ]);
    await s.leave();
  });
});

describe('소리 받는 상태(진단)', () => {
  const stats = (rx: Record<string, { ok: number; bad: number }>) => ({ data: { type: 'stats', encrypted: 0, decrypted: 0, dropped: 0, rx } });

  it('푼 것이 있으면 ok, 받기만 하고 못 풀면 bad, 한동안 아무것도 없으면 none', async () => {
    vi.useFakeTimers({ toFake: ['Date'] });
    try {
      const { s } = session(async () => ({}), { participants: parts('A', 'B', 'C', 'D') });
      await s.start();
      for (const d of ['A', 'C', 'D']) s.snapshot.connection[d] = 'connected';
      const w = FakeWorker.last!;
      s.updateParticipants([...parts('A', 'B', 'C'), { ...parts('D')[0], muted: true }], 0, '');
      w.onmessage?.(stats({ A: { ok: 5, bad: 0 }, C: { ok: 0, bad: 3 } }));
      expect(s.snapshot.rx).toEqual({ A: 'ok', C: 'bad' });
      // 아무것도 오지 않는 상대는 들어온 지 조금 지나고 두 번 연달아일 때만, 음소거한 상대(D)는 알리지 않습니다.
      vi.setSystemTime(Date.now() + 10_000);
      w.onmessage?.(stats({ A: { ok: 9, bad: 0 }, C: { ok: 0, bad: 3 } }));
      w.onmessage?.(stats({ A: { ok: 12, bad: 0 }, C: { ok: 0, bad: 3 } }));
      expect(s.snapshot.rx).toEqual({ A: 'ok', C: 'none' });
      await s.leave();
    } finally {
      vi.useRealTimers();
    }
  });

  it('소리가 온 상대를 기억하고, 나갔다 다시 들어오면 새 소리가 와야 다시 센다', async () => {
    const { s } = session(async () => ({}), { participants: parts('A', 'B') });
    await s.start();
    const w = FakeWorker.last!;
    w.onmessage?.(stats({ A: { ok: 5, bad: 0 } }));
    expect(s.snapshot.heard).toEqual(['A']);
    s.updateParticipants(parts('B'), 0, '');
    expect(s.snapshot.heard).toEqual([]);
    s.updateParticipants(parts('A', 'B'), 0, '');
    // 워커의 수는 누적이라 예전 5는 새 소리가 아닙니다.
    w.onmessage?.(stats({ A: { ok: 5, bad: 0 } }));
    expect(s.snapshot.heard).toEqual([]);
    w.onmessage?.(stats({ A: { ok: 6, bad: 0 } }));
    expect(s.snapshot.heard).toEqual(['A']);
    await s.leave();
  });
});

describe('마이크·스피커 장치', () => {
  class GainCtx extends FakeAudioContext {
    static made = 0;
    gains: Array<{ gain: { value: number } }> = [];
    sinks: string[] = [];
    constructor() {
      super();
      GainCtx.made++;
    }
    override createGain() {
      const g = { gain: { value: 1 }, connect() {} };
      this.gains.push(g);
      return g;
    }
    override createMediaStreamDestination() {
      return { stream: new FakeStream([boosted]) };
    }
    async setSinkId(id: string) {
      this.sinks.push(id);
    }
  }
  const boosted = { kind: 'audio', enabled: true, stop() {} };

  const devices = [
    { kind: 'audioinput', deviceId: 'default', label: '기본값 - 헤드셋 마이크', groupId: '' },
    { kind: 'audioinput', deviceId: 'mic-2', label: '헤드셋 마이크', groupId: '' },
    { kind: 'audiooutput', deviceId: 'spk-9', label: '헤드셋 스피커', groupId: '' },
  ];

  it('고른 마이크를 이름으로 찾아 열고, 음량이 100%가 아니면 증폭한 트랙을 보낸다', async () => {
    vi.stubGlobal('AudioContext', GainCtx);
    const opened: unknown[] = [];
    vi.stubGlobal('navigator', {
      mediaDevices: {
        enumerateDevices: async () => devices,
        getUserMedia: async (c: { audio: MediaTrackConstraints }) => {
          opened.push(c.audio.deviceId);
          return new FakeStream([mic]);
        },
        getDisplayMedia: async () => new FakeStream([screenTrack]),
      },
    });
    // 웹뷰를 다시 켜 ID가 바뀐 경우: 이름으로 찾습니다.
    const media = { ...DEFAULT_MEDIA, devices: { ...DEFAULT_MEDIA.devices, input: { id: 'old-id', label: '헤드셋 마이크' }, inputVolume: 150 } };
    const { s } = session(async () => ({}), { topology: 'mesh', participants: parts('A', 'B') }, 'B', { media });
    await s.start();
    expect(opened).toEqual([{ exact: 'mic-2' }]);
    await s.handleSignal('A', 'c1', { type: 'offer', sdp: 'm=0:audio m=1:video' });
    const [audioTx] = FakePC.all[0].transceivers;
    expect(audioTx.sender.track).toBe(boosted);
    // 통화 중 음량을 100%로: 증폭 줄은 남기고 값만 바꿉니다(트랙을 다시 붙이지 않음).
    s.setMedia({ ...media, devices: { ...media.devices, inputVolume: 100 } });
    expect(audioTx.sender.track).toBe(boosted);
    await s.leave();
  });

  it('앱을 켠 뒤 처음(권한 전)이면 기본 마이크로 권한을 받고 고른 마이크·스피커로 바꾼다', async () => {
    const sinks: string[] = [];
    class SinkAudio extends FakeAudio {
      async setSinkId(id: string) {
        sinks.push(id);
      }
    }
    vi.stubGlobal('Audio', SinkAudio);
    const opened: unknown[] = [];
    const first = { kind: 'audio', enabled: true, stopped: false, stop() { this.stopped = true; }, applyConstraints: async () => undefined };
    let granted = false;
    vi.stubGlobal('navigator', {
      mediaDevices: {
        enumerateDevices: async () => (granted ? devices : [{ kind: 'audioinput', deviceId: '', label: '', groupId: '' }]),
        getUserMedia: async (c: { audio: MediaTrackConstraints }) => {
          opened.push(c.audio.deviceId);
          const t = granted ? mic : first;
          granted = true;
          return new FakeStream([t]);
        },
      },
    });
    const media = {
      ...DEFAULT_MEDIA,
      devices: { ...DEFAULT_MEDIA.devices, input: { id: 'mic-2', label: '헤드셋 마이크' }, output: { id: 'spk-9', label: '헤드셋 스피커' } },
    };
    const { s } = session(async () => ({}), { topology: 'mesh', participants: parts('A', 'B') }, 'B', { media });
    await s.start();
    expect(opened).toEqual([undefined, { exact: 'mic-2' }]);
    expect(first.stopped).toBe(true);
    expect(s.snapshot.micError).toBeFalsy();
    await s.handleSignal('A', 'c1', { type: 'offer', sdp: 'm=0:audio m=1:video' });
    await vi.waitFor(() => expect(sinks).toContain('spk-9'));
    await s.leave();
  });

  it('고른 마이크를 열지 못하면 기본 마이크로 열고 알린다', async () => {
    const opened: unknown[] = [];
    vi.stubGlobal('navigator', {
      mediaDevices: {
        enumerateDevices: async () => devices,
        getUserMedia: async (c: { audio: MediaTrackConstraints }) => {
          opened.push(c.audio.deviceId);
          if (c.audio.deviceId) throw new Error('NotReadableError');
          return new FakeStream([mic]);
        },
      },
    });
    const media = { ...DEFAULT_MEDIA, devices: { ...DEFAULT_MEDIA.devices, input: { id: 'mic-2', label: '헤드셋 마이크' } } };
    const { s } = session(async () => ({}), { participants: parts('A', 'B') }, 'B', { media });
    await s.start();
    expect(opened).toEqual([{ exact: 'mic-2' }, undefined]);
    expect(s.snapshot.micError).toContain('기본 마이크');
    expect(s.snapshot.muted).toBe(false);
    await s.leave();
  });

  it('받은 소리에 스피커 장치와 음량을 걸고, 통화 중 바꾸면 바로 바꾼다', async () => {
    const sinks: string[] = [];
    class SinkAudio extends FakeAudio {
      volume = 1;
      async setSinkId(id: string) {
        sinks.push(id);
      }
    }
    vi.stubGlobal('Audio', SinkAudio);
    vi.stubGlobal('navigator', {
      mediaDevices: { enumerateDevices: async () => devices, getUserMedia: async () => new FakeStream([mic]) },
    });
    const media = { ...DEFAULT_MEDIA, devices: { ...DEFAULT_MEDIA.devices, output: { id: 'spk-9', label: '헤드셋 스피커' }, outputVolume: 40 } };
    const { s } = session(async () => ({}), { topology: 'mesh', participants: parts('A', 'B') }, 'B', { media });
    await s.start();
    // 셋째 m-line(둘째 오디오)은 A의 화면 소리: 스피커 음량 × 그 화면에 고른 음량.
    await s.handleSignal('A', 'c1', { type: 'offer', sdp: 'm=0:audio m=1:video m=2:audio' });
    const el = FakeAudio.all[0] as SinkAudio;
    const screenEl = FakeAudio.all[1] as SinkAudio;
    expect(el.volume).toBeCloseTo(0.4);
    expect(screenEl.volume).toBeCloseTo(0.4);
    expect(sinks).toContain('spk-9');
    s.setScreenVolume('A', 50);
    expect(s.snapshot.screenVolume).toEqual({ A: 50 });
    expect(screenEl.volume).toBeCloseTo(0.2);
    expect(el.volume).toBeCloseTo(0.4);
    s.setMedia({ ...media, devices: { ...media.devices, output: null, outputVolume: 80 } });
    await vi.waitFor(() => expect(sinks.at(-1)).toBe(''));
    expect(el.volume).toBeCloseTo(0.8);
    expect(screenEl.volume).toBeCloseTo(0.4);
    await s.leave();
  });
});

describe('Tor 통화', () => {
  it('RTCPeerConnection을 하나도 만들지 않고, WebRTC 신호에 답하지 않으며, 소리는 voice.media로 보낸다', async () => {
    // 만들면 바로 실패하게: Tor 통화에서는 연결을 만들기만 해도 IP 후보가 모입니다.
    vi.stubGlobal(
      'RTCPeerConnection',
      class {
        constructor() {
          throw new Error('Tor 통화에서 RTCPeerConnection을 만듦');
        }
      },
    );
    vi.stubGlobal('AudioContext', FakeAudioContext);
    vi.stubGlobal('AudioWorkletNode', FakeAudioWorkletNode);
    const media: unknown[] = [];
    const r = recorder();
    const { s } = session(
      async (method, params) => {
        if (method === 'voice.media') media.push(params);
        return r.call(method, params);
      },
      { topology: 'tor', participants: parts('A', 'B') },
      'B',
    );
    await s.start();
    s.updateParticipants(parts('A', 'B', 'C'), 0, '', { topology: 'tor' });
    await s.handleSignal('A', 'c1', { type: 'offer', sdp: 'offer-sdp' });
    await s.handleSignal('A', 'c1', ice('cand'));
    // 다른 기기의 결정이 SFU라 해도 따르지 않습니다.
    s.updateParticipants(parts('A', 'B', 'C'), 0, '', { topology: 'sfu', sfuHost: 'A' });
    expect(r.sent).toEqual([]);
    expect(s.snapshot.connection).toEqual({ A: 'tor', C: 'tor' });
    // 풀 수 있는 코덱을 알리는 hello가 voice.media로 나갑니다.
    await vi.waitFor(() => expect(media.length).toBeGreaterThan(0));
    expect(media[0]).toMatchObject({ space: 'sp', channel: 'ch', call: 'c1' });
    // 화면공유는 경고를 확인한 뒤 WebCodecs로만(WebRTC 없이). 이 시험 환경처럼 영상 인코더가 없으면 실패하고 화면을 놓습니다.
    await expect(s.startScreenShare('detail')).rejects.toThrow('경고');
    const stopped = vi.spyOn(screenTrack, 'stop');
    await expect(s.startScreenShare('detail', true)).rejects.toThrow('WebCodecs');
    expect(stopped).toHaveBeenCalled();
    expect(s.snapshot.sharing).toBe(false);
    expect(FakePC.all).toHaveLength(0);
    await s.leave();
  });

  it('서로 신뢰한 상대(direct)와만 WebRTC로 잇고, 소리는 Tor로 잇는 상대에게만 voice.media로 보낸다', async () => {
    vi.stubGlobal('AudioContext', FakeAudioContext);
    vi.stubGlobal('AudioWorkletNode', FakeAudioWorkletNode);
    const media: { to?: string[] }[] = [];
    const r = recorder();
    const direct = (...ds: string[]) => parts('A', 'B', 'C').map((p) => ({ ...p, direct: ds.includes(p.device) }));
    // 이쪽 A가 가장 작아 먼저 제안합니다. C와 서로 신뢰, B와는 아님.
    const { s } = session(
      async (method, params) => {
        if (method === 'voice.media') media.push(params as { to?: string[] });
        return r.call(method, params);
      },
      { topology: 'tor', participants: direct('C') },
      'A',
    );
    await s.start();
    expect(FakePC.all).toHaveLength(1);
    const pc = FakePC.all[0];
    pc.onnegotiationneeded?.();
    await vi.waitFor(() => expect(r.of('offer').map((o) => o.to)).toEqual(['C']));
    // 이어지기 전에는 C에게도 Tor로 보냅니다.
    expect(s.snapshot.connection).toEqual({ B: 'tor', C: 'tor' });
    await vi.waitFor(() => expect(media.length).toBeGreaterThan(0));
    expect(media.at(-1)?.to?.sort()).toEqual(['B', 'C']);
    pc.connectionState = 'connected';
    pc.onconnectionstatechange?.();
    expect(s.snapshot.connection).toEqual({ B: 'tor', C: 'connected' });
    // 신뢰하지 않는 B의 제안은 받지 않습니다.
    await s.handleSignal('B', 'c1', { type: 'offer', sdp: 'offer-sdp' });
    expect(FakePC.all).toHaveLength(1);
    // C가 신뢰를 끄면 연결을 닫고 다시 Tor로.
    s.updateParticipants(direct(), 0, '', { topology: 'tor' });
    expect(pc.closed).toBe(true);
    expect(s.snapshot.connection).toEqual({ B: 'tor', C: 'tor' });
    await s.leave();
  });

  it('상대가 먼저 직접으로 본 사이 온 제안은 이쪽도 직접이 되면 받는다', async () => {
    vi.stubGlobal('AudioContext', FakeAudioContext);
    vi.stubGlobal('AudioWorkletNode', FakeAudioWorkletNode);
    const r = recorder();
    const { s } = session(r.call, { topology: 'tor', participants: parts('A', 'B') }, 'B');
    await s.start();
    await s.handleSignal('A', 'c1', ice('cand'));
    await s.handleSignal('A', 'c1', { type: 'offer', sdp: 'offer-sdp' });
    expect(FakePC.all).toHaveLength(0);
    s.updateParticipants(
      parts('A', 'B').map((p) => ({ ...p, direct: p.device === 'A' })),
      0,
      '',
      { topology: 'tor' },
    );
    await vi.waitFor(() => expect(r.of('answer').map((a) => a.to)).toEqual(['A']));
    expect(FakePC.all).toHaveLength(1);
    await s.leave();
  });
});

describe('VoiceSession ffmpeg 인코더 화면공유', () => {
  const tick = async () => {
    for (let i = 0; i < 10; i++) await new Promise((r) => setTimeout(r, 0));
  };

  class Gen {
    kind = 'video';
    contentHint = '';
    stopped = false;
    writable = { getWriter: () => ({ desiredSize: 1, write: async () => undefined, close: async () => undefined }) };
    stop() {
      this.stopped = true;
    }
  }
  class Stream {
    constructor(readonly tracks: unknown[] = []) {}
    getAudioTracks() {
      return this.tracks;
    }
    getVideoTracks() {
      return this.tracks;
    }
    getTracks() {
      return this.tracks;
    }
  }
  class Decoder {
    state = 'unconfigured';
    decodeQueueSize = 0;
    configure() {
      this.state = 'configured';
    }
    decode() {}
    close() {
      this.state = 'closed';
    }
    static async isConfigSupported() {
      return { supported: true };
    }
  }

  function nativeSession(available: boolean) {
    let onData: ((m: Uint8Array) => void) | null = null;
    const screen = {
      status: vi.fn(async () => ({
        dir: 'f',
        missing: [],
        encoders: [{ id: 'nvenc-h264', available, ...(available ? {} : { error: 'GPU 없음' }) }],
      })),
      sources: vi.fn(async () => []),
      start: vi.fn(async (_src: unknown, _h: unknown, _s: unknown, cb: (m: Uint8Array) => void) => {
        onData = cb;
        return 1;
      }),
      stop: vi.fn(async () => undefined),
    };
    const pick = vi.fn(async () => ({ kind: 'monitor' as const, id: '7', title: '화면 1', width: 1920, height: 1080, primary: true }));
    const client = { call: vi.fn(async () => ({})), transport: { screen } } as unknown as ApiClient;
    const s = new VoiceSession({
      client,
      space: 'sp',
      channel: 'ch',
      myDevice: 'A',
      myUser: 'u-A',
      join: { call: 'c1', participants: parts('A', 'B', 'C'), media_key: '', generation: 0, topology: 'mesh', ice_servers: [] },
      iceServers: [],
      simulated: false,
      media: { ...DEFAULT_MEDIA, screen: { ...DEFAULT_MEDIA.screen, encoder: 'nvenc-h264' } },
      pickScreenSource: pick,
      onChange: () => undefined,
    });
    return { s, screen, pick, emit: (m: number[]) => onData?.(new Uint8Array(m)) };
  }

  beforeEach(() => {
    vi.stubGlobal('MediaStreamTrackGenerator', Gen);
    vi.stubGlobal('MediaStream', Stream);
    vi.stubGlobal('VideoDecoder', Decoder);
    vi.stubGlobal('EncodedVideoChunk', class {});
  });

  it('고른 화면을 앱이 인코딩해, 그 코덱을 받는 시청자에게는 데이터 채널로, 아닌 시청자에게는 푼 트랙을 WebRTC로 보낸다', async () => {
    const { s, screen, pick, emit } = nativeSession(true);
    await s.start();
    await tick();
    const [pcB, pcC] = FakePC.all;
    const dcB = pcB.channels.find((c) => c.label === 'yangpa-screen')!;
    const dcC = pcC.channels.find((c) => c.label === 'yangpa-screen')!;
    dcB.open();
    dcC.open();
    // B는 H.264를 풀 수 있고, C는 아무것도(예전 판·디코더 없음).
    dcB.onmessage?.({ data: new Uint8Array([2, 2]).buffer });
    dcC.onmessage?.({ data: new Uint8Array([2, 0]).buffer });
    await tick();
    await s.startScreenShare('detail');
    expect(pick).toHaveBeenCalledTimes(1);
    expect(screen.start).toHaveBeenCalledTimes(1);
    expect(screen.start.mock.calls[0][0]).toMatchObject({ kind: 'monitor', id: '7' });
    expect(screen.start.mock.calls[0][2]).toMatchObject({ encoder: 'nvenc-h264', fps: 30, bitrateKbps: 2500 });
    expect(s.snapshot.sharing).toBe(true);
    await s.handleSignal('B', 'c1', { type: 'watch_screen', watching: true });
    await s.handleSignal('C', 'c1', { type: 'watch_screen', watching: true });
    await tick();
    const video = (pc: FakePC) => pc.transceivers.find((t) => t.receiver.track.kind === 'video')!.sender;
    expect(video(pcB).track).toBeNull();
    expect(video(pcC).track).toBeInstanceOf(Gen);
    const before = dcB.sent.length;
    emit([1, 2, 0x80, 0x07, 0x38, 0x04, 1, 0x64, 0, 0x28]);
    emit([2, 1, 0, 0, 0, 0, 0x65]);
    await tick();
    expect(dcB.sent.length).toBeGreaterThan(before);
    expect(dcC.sent.every((b) => b[0] === 2)).toBe(true);
    await s.stopScreenShare();
    expect(screen.stop).toHaveBeenCalledWith(1);
    expect(s.snapshot.sharing).toBe(false);
    expect(video(pcC).track).toBeNull();
    await s.leave();
  });

  it('ffmpeg 인코더를 이 기기에서 쓸 수 없으면 웹뷰 인코더로 공유한다', async () => {
    const { s, screen, pick } = nativeSession(false);
    await s.start();
    await tick();
    await s.startScreenShare('detail');
    expect(pick).not.toHaveBeenCalled();
    expect(screen.start).not.toHaveBeenCalled();
    expect(s.snapshot.sharing).toBe(true);
    await s.leave();
  });

  it('공유한 창이 닫혀 인코더가 끝나면 공유를 멈추고 까닭을 알린다', async () => {
    const { s, emit } = nativeSession(true);
    await s.start();
    await tick();
    await s.startScreenShare('motion');
    emit([3, ...new TextEncoder().encode('창이 닫혔습니다')]);
    await tick();
    expect(s.snapshot.sharing).toBe(false);
    expect(s.snapshot.micError).toBe('화면공유가 끝났습니다: 창이 닫혔습니다');
    await s.leave();
  });
});
