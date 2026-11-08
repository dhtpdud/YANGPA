// 음성 통화. 협상은 데몬 경유(voice.signal API ↔ 'voice.signal' 알림), 미디어는 웹뷰 WebRTC.
//
// Tor 통화(`topology: 'tor'`, Tor로 도는 기기의 일반 채널)는 서로 신뢰한 상대(참가자의 `direct`)와만 WebRTC 메시로 잇고,
// 나머지 상대에게는 RTCPeerConnection을 만들지 않고(만들기만 해도 IP 후보가 모임) 소리를 봉인한 패킷으로 데몬에 넘깁니다
// (tor.ts). 직접 잇는 상대도 연결되기 전·끊긴 동안은 Tor로 보냅니다. Tor 상대의 화면공유는 비상용으로만(경고를 확인한 뒤,
// 낮은 해상도·초당 2프레임 VP8을 '화면 보기'를 누른 사람에게만, tor-screen.ts).
//
// 토폴로지는 데몬(키 관리 기기의 결정)이 'voice.participants'로 알려 줍니다(protocol.md "SFU").
// - 메시: 참가자 쌍마다 RTCPeerConnection 하나. 기기 ID가 작은 쪽이 먼저 제안하고(오디오·비디오 트랜시버를 미리
//   확보), 충돌은 'perfect negotiation' 규칙으로 풉니다(큰 ID 쪽이 polite). 화면공유는 미리 확보한 비디오 트랜시버에
//   replaceTrack으로 켜고 끕니다(재협상 없음). '보기'를 누른 시청자에게만 보냅니다.
// - SFU: 호스트 기기에 RTCPeerConnection 하나. 세션을 열 때 오디오와 화면(simulcast 3층) 송신 트랙을 담아 한 번만
//   제안하고, 참가자가 늘고 줄 때의 재협상은 호스트만 제안합니다(글레어 없음). 받는 트랙은 호스트가 보낸 트랙 표
//   (mid → 출처 기기·종류)로 가립니다. 화면은 '보기'를 누른 것만 받고(sfu_select) 화질 층을 고를 수 있습니다.
//   호스트가 바뀌거나 세션이 닫히면 더 큰 세션 번호로 다시 붙습니다.
// - 모든 프레임은 워커에서 AES-GCM으로 암호화합니다(보내는 트랙·받는 트랙마다 변환). SFU 호스트는 프레임을 풀지 않고
//   넘기므로 키가 없어도 됩니다. 암호화 변환을 붙일 수 없는 웹뷰면 통화를 시작하지 않습니다.
// - ICE 서버(STUN)는 데몬의 voice.join 응답을 따릅니다. SFU에도 같은 ICE 설정을 씁니다.
// - 화면 인코더로 ffmpeg 인코더(AOM AV1·SVT-AV1·NVENC)를 고르면(데스크톱 앱, 메시·SFU 통화) 앱이 고른 창·모니터를
//   캡처·인코딩하고, 그 코덱을 풀 수 있다고 알린 메시 시청자에게는 프레임을 암호화해 데이터 채널로 보냅니다
//   (native-screen.ts). 이 기기에서 푼 같은 화면을 트랙으로 만들어, 나머지 시청자·SFU에는 지금처럼 WebRTC로 보냅니다.
// - 화면 소리(메시, 데스크톱 앱): 앱이 잡은 소리(screen-audio.ts)를 마이크와 따로 된 셋째 m-line(둘째 오디오)으로 시청자에게만
//   보냅니다. 그 m-line의 Opus는 음악용 값(스테레오·128Kbps)입니다.

import type { ApiClient, NeuralCodecId, ScreenSource } from '../api/client';
import type {
  DeviceId,
  SfuLayer,
  SfuTrack,
  SignalPayload,
  Topology,
  TrackSource,
  UserId,
  VoiceJoinResult,
  VoiceParticipant,
} from '../api/types';
import { t } from '../i18n';
import { joinedAnonymously } from '../lib/anon';
import { base64ToBytes } from '../lib/misc';
import type { WorkerData, WorkerStats } from './e2ee-worker';
import { applySink, createMicGain, findDevice, type DevicePrefs, type MicGain } from './devices';
import { createFakeScreenStream } from './fake';
import type { MediaKind } from './frame';
import { CandidateBuffer, SerialQueue } from './queue';
import {
  DEFAULT_MEDIA,
  micConstraints,
  nativeMaxHeight,
  nativeSettings,
  opusFmtp,
  pickSendCodec,
  screenDegradation,
  screenFps,
  screenScale,
  setOpusFmtp,
  setScreenAudioFmtp,
  videoCodecOrder,
  type MediaPrefs,
  type ScreenCodec,
} from './media';
import { NativeScreen } from './native-screen';
import { NeuralLink } from './neural';
import { isLinkUp } from './policy';
import { TorAudio, type TorCodec } from './tor';
import { startScreenAudio, type ScreenAudio } from './screen-audio';
import { enableOpusDtx, onlyVp8 } from './sdp';
import { attachReceiver, attachSender, createE2eeWorker, detectTransformMode, postKey, type TransformMode } from './transform';

export type ContentHint = 'detail' | 'motion';
/**
 * 상대 소리를 받는 상태(진단용, 지난 몇 초): 'ok' 받아서 풀고 있음, 'bad' 받지만 통화 키가 맞지 않아 풀지 못함, 'none' 아무것도
 * 오지 않음. 프레임 암호화 워커가 보낸 기기별로 센 값으로 정합니다.
 */
export type RxState = 'ok' | 'bad' | 'none';
/** 받을 화면공유 화질(SFU simulcast 층). */
export type ScreenLayer = Exclude<SfuLayer, 'off'>;

export interface VoiceSnapshot {
  space: string;
  channel: string;
  call: string;
  topology: Topology;
  /** SFU 호스트 기기(SFU일 때만, 넘겨받는 중이면 없음). */
  sfuHost?: DeviceId;
  /** SFU 호스트 기기의 사용자(통화 밖 호스트 노드도 누구 것인지 보여 줌). */
  sfuHostUser?: UserId;
  /** SFU 연결 상태(SFU일 때만). 'waiting'은 호스트가 정해지기를 기다리거나 다시 붙기 전. */
  sfuState?: RTCPeerConnectionState | 'waiting';
  generation: number;
  participants: VoiceParticipant[];
  /** 익명 채널로 들어간 통화(참가 응답의 참가자에 번호가 있었음). 익명이 꺼지면 데몬이 통화에서 나오므로 앱도 끊습니다. */
  anonymous?: boolean;
  speaking: Record<DeviceId, boolean>;
  connection: Record<DeviceId, string>;
  muted: boolean;
  deafened: boolean;
  sharing: boolean;
  contentHint: ContentHint;
  /** 내가 보고 있는 원격 화면. */
  watching: DeviceId[];
  /** 보는 화면마다 고른 화질(SFU에서만 쓰임, 없으면 auto). */
  layers: Record<DeviceId, ScreenLayer>;
  /** 내 화면을 보는 기기. */
  viewers: DeviceId[];
  /** 보는 화면마다 고른 화면 소리 음량(%, 0~100, 없으면 100). 스피커 음량에 곱해 겁니다. 이 통화 동안만. */
  screenVolume: Record<DeviceId, number>;
  e2ee: TransformMode | 'simulated';
  micError?: string;
  stats?: WorkerStats;
  /** 신경망 음성 코덱으로 보내는 중이면 그 코덱과 받는 상대(메시만). */
  neural?: { codec: NeuralCodecId; to: DeviceId[] };
  /** 이 기기에서 쓸 수 있는(모델이 있는) 신경망 코덱. */
  neuralReady?: NeuralCodecId[];
  /** Tor 통화에서 지금 보내는 소리 코덱(Tor 통화만). */
  torCodec?: TorCodec;
  /** 상대마다 소리를 받는 상태(진단). 막 들어온 상대·음소거한 상대는 'none'을 알리지 않습니다. */
  rx?: Record<DeviceId, RxState>;
  /** 이번 참가 동안 소리(프레임)가 한 번이라도 도착한 상대(연결 표시용). 나가면 빠지고 다시 들어오면 새로 셉니다. */
  heard?: DeviceId[];
}

export interface VoiceSessionOptions {
  client: ApiClient;
  space: string;
  channel: string;
  myDevice: DeviceId;
  myUser: UserId;
  join: VoiceJoinResult;
  iceServers: RTCIceServer[];
  /** MOCK 백엔드: 실제 상대가 없으므로 연결을 만들지 않고 화면·말하기를 흉내 냅니다. */
  simulated: boolean;
  /** 음성·화면공유 세부 설정(없으면 기본값). 통화 중 바꾸면 `setMedia`. */
  media?: MediaPrefs;
  /** ffmpeg 인코더로 공유할 창·모니터 고르기(store가 선택 창을 띄움). 취소하면 null. */
  pickScreenSource?: () => Promise<ScreenSource | null>;
  onChange: (s: VoiceSnapshot) => void;
}

interface Peer {
  device: DeviceId;
  pc: RTCPeerConnection;
  polite: boolean;
  makingOffer: boolean;
  ignoreOffer: boolean;
  audioTx?: RTCRtpTransceiver;
  videoTx?: RTCRtpTransceiver;
  /** 화면 소리(둘째 오디오 m-line). */
  screenAudioTx?: RTCRtpTransceiver;
  audioEl?: HTMLAudioElement;
  screenAudioEl?: HTMLAudioElement;
  remoteVideo?: MediaStream;
}

/** SFU 호스트와의 세션 하나. */
interface SfuLink {
  host: DeviceId;
  session: number;
  pc: RTCPeerConnection;
  audioTx: RTCRtpTransceiver;
  screenTx: RTCRtpTransceiver;
  /** 받는 m-line → 출처(호스트가 보낸 트랙 표). */
  tracks: Map<string, { device: DeviceId; source: TrackSource }>;
  /** 출처 기기 → 소리 요소. */
  audio: Map<DeviceId, HTMLAudioElement>;
  /** 출처 기기 → 화면 스트림('보기' 전에도 m-line은 있지만 프레임은 오지 않음). */
  screens: Map<DeviceId, MediaStream>;
  /** 제안을 보내기 전에 생긴 내 ICE 후보(제안보다 먼저 닿지 않게). */
  earlyIce: SignalPayload[];
  offered: boolean;
  closed: boolean;
}

/** 이번에 처리할 신호가 SFU 세션 신호인지. */
type SfuPayload = Extract<SignalPayload, { type: `sfu_${string}` }>;

const SPEAKING_RMS = 0.02;
/** 상대가 들어온 뒤 이만큼 지나도 아무것도 오지 않아야 '소리가 오지 않음'으로 봅니다(연결·첫 키를 기다리는 시간). */
const RX_GRACE_MS = 8_000;
/** Tor 통화에서 아직 직접 상대로 보지 않는 기기의 제안·후보를 받아 두는 수(기기마다). */
const MAX_HELD = 64;

/** Tor 통화에서 화면공유를 켜기 전 경고(store가 확인 창으로 보임). */
export const TOR_SCREEN_WARNING =
  t('Tor 통화의 화면공유는 비상용입니다. Tor는 대역폭이 작아 낮은 화질(가로 최대 1280px)·초당 2프레임 정도로, 몇 초 늦게 보입니다.') +
  ' ' +
  t('보는 사람에게만 보내지만, 그동안 같은 Tor 회로의 소리·메시지가 느려질 수 있습니다. IP는 드러나지 않습니다.');

/** 화면공유 simulcast 층(낮은 층부터). SFU는 SDP에 적힌 이 순서를 층 번호로 씁니다. */
function screenEncodings(): RTCRtpEncodingParameters[] {
  return [
    { rid: 'q', scaleResolutionDownBy: 4 },
    { rid: 'h', scaleResolutionDownBy: 2 },
    { rid: 'f', scaleResolutionDownBy: 1 },
  ];
}

/** 인코딩 하나의 원하는 값. */
interface ScreenEncoding {
  maxBitrate: number;
  maxFramerate: number;
  scaleResolutionDownBy: number;
}

const sameCodec = (a: RTCRtpCodec | undefined, b: RTCRtpCodec | undefined) =>
  (a?.mimeType ?? '').toLowerCase() === (b?.mimeType ?? '').toLowerCase() && (a?.sdpFmtpLine ?? '') === (b?.sdpFmtpLine ?? '');

/**
 * 화면 송신기의 인코딩 값을 바뀐 때만 고칩니다. 같은 값이라도 setParameters를 부르면 인코더가 다시 설정되어(키 프레임·잠깐 멈춤)
 * 참가자 변화 때마다 화면이 끊겼습니다. `codec`은 협상된 코덱 중 보낼 것(없으면 그대로).
 */
function tuneScreenSender(
  sender: RTCRtpSender,
  degradation: RTCDegradationPreference,
  encodingFor: (i: number) => ScreenEncoding,
  codec?: RTCRtpCodec,
) {
  const params = sender.getParameters();
  let changed = false;
  if (params.degradationPreference !== degradation) {
    params.degradationPreference = degradation;
    changed = true;
  }
  params.encodings?.forEach((enc, i) => {
    const want = encodingFor(i);
    if (
      enc.maxBitrate !== want.maxBitrate ||
      enc.maxFramerate !== want.maxFramerate ||
      (enc.scaleResolutionDownBy ?? 1) !== want.scaleResolutionDownBy
    ) {
      enc.maxBitrate = want.maxBitrate;
      enc.maxFramerate = want.maxFramerate;
      enc.scaleResolutionDownBy = want.scaleResolutionDownBy;
      changed = true;
    }
    const e = enc as RTCRtpEncodingParameters & { codec?: RTCRtpCodec };
    if (codec && !sameCodec(e.codec, codec)) {
      e.codec = codec;
      changed = true;
    }
  });
  if (changed) void sender.setParameters(params).catch(() => undefined);
}

/** 층별 최대 비트레이트(낮은 층부터). */
const SCREEN_BITRATES: Record<ContentHint, readonly number[]> = {
  detail: [300_000, 900_000, 2_500_000],
  motion: [500_000, 1_500_000, 4_000_000],
};

/** 던져진 값의 문구(데스크톱 앱 명령은 Error가 아닌 값으로 거절할 수 있음). */
const errorText = (e: unknown): string =>
  e instanceof Error ? e.message : typeof e === 'string' ? e : ((e as { message?: string } | null)?.message ?? t('알 수 없는 오류'));

/** SFU 세션을 다시 열기까지 기다리는 시간(실패할 때마다 두 배, 상한까지). */
const SFU_RETRY_MIN_MS = 250;
const SFU_RETRY_MAX_MS = 5_000;

/** 연결의 오디오 트랜시버(m-line 순서). */
function audioTransceivers(pc: RTCPeerConnection): RTCRtpTransceiver[] {
  return pc.getTransceivers().filter((t) => t.receiver.track.kind === 'audio');
}

function isSfuPayload(p: SignalPayload): p is SfuPayload {
  return p.type.startsWith('sfu_');
}

/** 메시 영상 트랜시버의 코덱 협상 순서(고른 코덱 먼저, VP8 함께, 프레임 암호화와 맞지 않는 코덱은 뺌). */
function preferVideoCodecs(t: RTCRtpTransceiver, prefer: ScreenCodec) {
  try {
    const caps = RTCRtpReceiver.getCapabilities('video');
    if (caps) t.setCodecPreferences(videoCodecOrder(caps.codecs, prefer));
  } catch {
    // 코덱 선호를 못 바꿔도 VP8이 기본 후보에 있음
  }
}

/** SFU 영상 트랜시버는 VP8만(호스트가 VP8만 나름, 평문 접두 규칙도 VP8 기준). */
function preferVp8(t: RTCRtpTransceiver) {
  try {
    const caps = RTCRtpReceiver.getCapabilities('video');
    if (caps) t.setCodecPreferences(onlyVp8(caps.codecs));
  } catch {
    // 코덱 선호를 못 바꿔도 VP8이 기본 후보에 있음
  }
}

export class VoiceSession {
  private readonly o: VoiceSessionOptions;
  private readonly peers = new Map<DeviceId, Peer>();
  private readonly transformed = new WeakSet<RTCRtpSender | RTCRtpReceiver>();
  private readonly remoteVideos = new Map<DeviceId, MediaStream>();
  private readonly analysers = new Map<DeviceId, AnalyserNode>();
  private worker: Worker | null = null;
  private mode: TransformMode | 'simulated' = 'none';
  private mic: MediaStreamTrack | null = null;
  private screen: MediaStreamTrack | null = null;
  private audioCtx: AudioContext | null = null;
  /** getUserMedia로 연 마이크 그대로. 보내는 트랙(`mic`)은 마이크 음량이 100%가 아니면 증폭한 트랙입니다. */
  private rawMic: MediaStreamTrack | null = null;
  private micGain: MicGain | null = null;
  /** 받은 소리를 틀 스피커 장치('' 기본). */
  private sink = '';
  /** 진단: 지난 통계의 보낸 기기별 값, 상대가 처음 보인 때, 연달아 아무것도 오지 않은 횟수. */
  private rxPrev: Record<DeviceId, { ok: number; bad: number }> = {};
  private rxSince = new Map<DeviceId, number>();
  private rxQuiet = new Map<DeviceId, number>();
  private heard = new Set<DeviceId>();
  private levelTimer: ReturnType<typeof setInterval> | null = null;
  /** 암호화 워커를 갖춘 뒤(SFU 세션은 그 전에 열지 않음). */
  private started = false;
  private screenPreview: MediaStream | null = null;
  private media: MediaPrefs;
  private neural: NeuralLink | null = null;
  /** ffmpeg 인코더 화면공유(보내기·받기, 메시만). */
  private native: NativeScreen | null = null;
  /** 지금 공유가 ffmpeg 인코더로 만든 것인지(그때 `screen`은 이 기기에서 푼 트랙). */
  private nativeSharing = false;
  /** Tor 통화의 소리(서로 신뢰한 상대 밖에는 RTCPeerConnection을 만들지 않음). */
  private tor: TorAudio | null = null;
  private readonly torMode: boolean;
  /** Tor 비상용 화면을 보내는 중(이번 공유에서 경고를 확인함). */
  private torScreenOn = false;
  /** Tor 통화에서 아직 직접 상대로 보지 않는 기기가 먼저 보낸 제안·후보(이쪽도 직접이 되면 씀, 상대의 신뢰 안내가 늦게 닿는 동안). */
  private readonly held = new Map<DeviceId, SignalPayload[]>();
  private closed = false;
  private snap: VoiceSnapshot;
  /** 받은 신호를 도착 순서대로 하나씩 처리(제안 처리 중에 온 후보가 앞지르지 않게). */
  private readonly inbox = new SerialQueue();
  /** 보내는 신호도 순서대로(제안보다 후보가 먼저 닿지 않게). */
  private readonly outbox = new SerialQueue();
  /** 원격 설명이 정해지기 전에 온 ICE 후보. */
  private readonly pendingIce = new CandidateBuffer<RTCIceCandidateInit>();
  private sfu: SfuLink | null = null;
  private sfuSession = 0;
  private sfuAttempts = 0;
  private sfuTimer: ReturnType<typeof setTimeout> | null = null;
  private shareStarting = false;
  /** 보내는 화면 소리와, 그 소리를 잡을 창(ffmpeg 인코더로 창을 공유할 때만, 없으면 이 앱을 뺀 시스템 소리). */
  private screenAudio: ScreenAudio | null = null;
  private shareWindow: string | null = null;
  /** 이번 공유를 이미 저절로 켠 기기('그만 보기'를 누르면 그 공유가 끝날 때까지 다시 켜지 않음). */
  private readonly autoWatched = new Set<DeviceId>();

  constructor(o: VoiceSessionOptions) {
    this.media = o.media ?? DEFAULT_MEDIA;
    this.o = o;
    this.torMode = o.join.topology === 'tor';
    this.snap = {
      space: o.space,
      channel: o.channel,
      call: o.join.call,
      topology: o.join.topology,
      sfuHost: o.join.topology === 'sfu' ? o.join.sfu_host : undefined,
      sfuHostUser: o.join.topology === 'sfu' && o.join.sfu_host ? o.join.sfu_host_user : undefined,
      generation: o.join.generation,
      participants: o.join.participants,
      anonymous: joinedAnonymously(o.join.participants),
      speaking: {},
      connection: {},
      muted: false,
      deafened: false,
      sharing: false,
      contentHint: 'detail',
      watching: [],
      layers: {},
      viewers: [],
      screenVolume: {},
      e2ee: 'none',
      rx: {},
    };
  }

  get snapshot(): VoiceSnapshot {
    return this.snap;
  }

  /** 원격 화면 스트림(보기 중일 때만). */
  remoteScreen(device: DeviceId): MediaStream | undefined {
    return this.remoteVideos.get(device);
  }

  /**
   * 내 화면 미리보기 스트림. 같은 공유 동안 늘 같은 객체를 돌려줍니다: 화면이 다시 그려질 때마다(말하기 표시로 초당 여러 번)
   * 새 스트림을 주면 <video>가 처음부터 다시 붙어 미리보기가 깜박입니다.
   */
  localScreen(): MediaStream | undefined {
    if (!this.screen) return undefined;
    if (this.screenPreview?.getVideoTracks()[0] !== this.screen) this.screenPreview = new MediaStream([this.screen]);
    return this.screenPreview;
  }

  private emit(patch: Partial<VoiceSnapshot>) {
    this.snap = { ...this.snap, ...patch };
    this.o.onChange(this.snap);
  }

  async start(): Promise<void> {
    if (this.o.simulated) {
      this.mode = 'simulated';
    } else {
      // Tor 통화도 서로 신뢰한 상대와는 WebRTC라 변환을 찾습니다(없으면 그 상대와도 Tor로만).
      this.mode = detectTransformMode();
      if (this.mode === 'none' && !this.torMode) {
        throw new Error(t('이 웹뷰는 프레임 단위 암호화(Encoded Transform)를 지원하지 않아 통화를 시작할 수 없습니다.'));
      }
      this.worker = createE2eeWorker();
      this.worker.onmessage = (e: MessageEvent<WorkerStats | WorkerData>) => {
        if (e.data?.type === 'stats') this.onStats(e.data);
        else if (e.data?.type === 'data') {
          this.neural?.onWorkerData(e.data);
          this.native?.onWorkerData(e.data);
          this.tor?.onWorkerData(e.data);
        }
      };
      postKey(this.worker, this.o.join.generation, base64ToBytes(this.o.join.media_key));
      // 직접 잇는 상대와의 음성은 Opus(신경망 코덱은 Tor 상대에게만, tor.ts). 화면은 그 상대에게도 ffmpeg 인코더로.
      if (this.torMode) this.startTor();
      else this.startNeural();
      this.startNative();
    }
    this.started = true;
    this.emit({ e2ee: this.mode });

    void this.resolveSink().then(() => this.applyOutput());
    try {
      const raw = await this.openMic();
      if (this.closed) {
        raw?.stop();
        return;
      }
      this.rawMic = raw;
      this.mic = this.micTrackFor(raw);
      // 권한 전에는 스피커 장치도 찾지 못하므로 마이크를 연 뒤 한 번 더 찾습니다.
      if (this.devices.output && !this.sink) void this.resolveSink().then(() => this.applyOutput());
      if (this.mic) this.watchLevel(this.o.myDevice, new MediaStream([this.mic]));
      this.neural?.setMic(this.mic);
      this.tor?.setMic(this.mic);
    } catch (e) {
      this.emit({ micError: e instanceof Error ? e.message : t('마이크를 열 수 없습니다'), muted: true });
    }
    if (this.closed) return;
    // 마이크 허락을 기다리는 동안 생긴 연결에도 마이크를 붙입니다.
    for (const p of this.peers.values()) this.prepareTransceivers(p);
    if (this.mic && this.sfu && !this.sfu.audioTx.sender.track) {
      void this.sfu.audioTx.sender.replaceTrack(this.mic).catch(() => undefined);
    }
    this.levelTimer = setInterval(() => this.sampleLevels(), 150);
    this.applyTopology(this.snap.topology, this.snap.sfuHost, this.snap.sfuHostUser);
    this.syncPeers(this.snap.participants);
  }

  // ───────────── 참가자·키·토폴로지 ─────────────

  updateParticipants(
    participants: VoiceParticipant[],
    generation: number,
    mediaKey: string,
    topo?: { topology: Topology; sfuHost?: DeviceId; sfuHostUser?: UserId },
  ) {
    if (this.closed) return;
    // 세대 번호가 같아도 키는 바뀔 수 있습니다(관리 기기가 바뀌며 이전 세대를 못 듣고 같은 번호로 새로 시작). 데몬이 알려 준 지금
    // 키를 늘 넘기고, 워커는 마지막에 받은 키로 보냅니다(빈 키는 버려짐).
    if (this.worker && mediaKey) postKey(this.worker, generation, base64ToBytes(mediaKey));
    this.emit({ participants, generation });
    if (topo) this.applyTopology(topo.topology, topo.sfuHost, topo.sfuHostUser);
    this.syncPeers(participants);
  }

  /** 메시 ↔ SFU 전환과 SFU 호스트 넘겨주기. */
  private applyTopology(topology: Topology, host: DeviceId | undefined, hostUser?: UserId) {
    const sfuHost = topology === 'sfu' ? host : undefined;
    const sfuHostUser = sfuHost ? hostUser : undefined;
    if (topology !== this.snap.topology || sfuHost !== this.snap.sfuHost || sfuHostUser !== this.snap.sfuHostUser) {
      this.emit({ topology, sfuHost, sfuHostUser });
    }
    if (this.mode === 'simulated') {
      this.emit({ sfuState: topology === 'sfu' ? (sfuHost ? 'connected' : 'waiting') : undefined });
      return;
    }
    // Tor 통화는 SFU 없이: 다른 기기의 결정(SFU·메시)을 따르지 않고, 서로 신뢰한 상대와의 메시 연결만 둡니다(syncPeers). 일반
    // 통화가 Tor로 바뀌었다고 알려 오면 열린 연결을 모두 닫습니다(IP가 드러나는 길을 남기지 않음).
    if (this.torMode || topology === 'tor') {
      if (!this.torMode) for (const d of [...this.peers.keys()]) this.closePeer(d);
      this.clearSfuRetry();
      this.closeSfu();
      if (this.snap.sfuState !== undefined) this.emit({ sfuState: undefined });
      // WebRTC로 시작한 통화는 도중에 Tor 통화로 바꿀 수 없어 소리가 끊깁니다. 다시 참가하면 Tor 통화로 들어갑니다.
      if (!this.torMode && topology === 'tor') this.emit({ micError: t('통화가 Tor로 바뀌었습니다. 나갔다가 다시 참가해 주세요') });
      return;
    }
    if (topology === 'mesh') {
      this.clearSfuRetry();
      this.sfuAttempts = 0;
      this.closeSfu();
      if (this.snap.sfuState !== undefined) this.emit({ sfuState: undefined });
      return;
    }
    // SFU: 메시 연결은 닫고 호스트 하나에만 붙습니다.
    for (const d of [...this.peers.keys()]) this.closePeer(d);
    this.pendingIce.clear();
    if (this.sfu && this.sfu.host !== sfuHost) {
      // 지난 호스트는 결정이 바뀌면 스스로 세션을 닫으므로 알리지 않습니다(데몬도 지금 호스트에게만 넘김).
      this.closeSfu();
      this.clearSfuRetry();
      this.sfuAttempts = 0;
    }
    if (!sfuHost) {
      this.setSfuState('waiting');
      return;
    }
    if (this.started && !this.closed && !this.sfu && !this.sfuTimer) void this.openSfu(sfuHost);
  }

  private syncPeers(participants: VoiceParticipant[]) {
    const others = new Set(participants.filter((p) => p.device !== this.o.myDevice).map((p) => p.device));
    for (const [device] of this.peers) if (!others.has(device)) this.closePeer(device);
    // 나간 상대는 '소리가 온 적 있음'을 잊습니다(워커의 수는 누적이라 다시 들어온 뒤 그것으로는 가릴 수 없음).
    const left = [...this.heard].filter((d) => !others.has(d));
    if (left.length) {
      for (const d of left) this.heard.delete(d);
      this.emit({ heard: [...this.heard] });
    }
    // 떠난 시청자·시청 대상 정리
    const viewers = this.snap.viewers.filter((d) => others.has(d));
    const watching = this.snap.watching.filter((d) => others.has(d) && participants.find((p) => p.device === d)?.screen);
    for (const d of this.snap.watching) {
      if (watching.includes(d)) continue;
      this.remoteVideos.delete(d);
      this.native?.forget(d);
      // 공유를 멈춘 화면은 다시 켜져도 받지 않게(누르기 전에는 영상 데이터를 받지 않음).
      if (this.sfu && others.has(d)) void this.sendSfu(this.sfu, this.selectPayload(d, false));
    }
    if (viewers.length !== this.snap.viewers.length || watching.length !== this.snap.watching.length) {
      const layers = Object.fromEntries(Object.entries(this.snap.layers).filter(([d]) => watching.includes(d)));
      this.emit({ viewers, watching, layers });
      this.applySfuScreen();
      this.applyNativeViewers();
    }
    // 상대가 공유를 시작하면 누르지 않아도 바로 봅니다. Tor로 받는 비상용 화면은 Tor 대역(소리·메시지와 같은 회로)을 써서 누를 때만.
    for (const p of participants) {
      if (p.device === this.o.myDevice) continue;
      if (!p.screen) this.autoWatched.delete(p.device);
      else if (this.started && !this.autoWatched.has(p.device) && (!this.torMode || this.isDirect(p.device))) {
        this.autoWatched.add(p.device);
        if (!this.snap.watching.includes(p.device)) void this.watch(p.device, true);
      }
    }
    if (this.mode === 'simulated') {
      const connection: Record<string, string> = {};
      for (const d of others) connection[d] = this.torMode || this.snap.topology === 'tor' ? 'tor' : 'connected';
      this.emit({ connection, heard: [...others] });
      return;
    }
    if (this.torMode || this.snap.topology === 'tor') {
      // 서로 신뢰를 그만둔 상대와의 연결은 닫고(IP가 드러나는 길을 남기지 않음), 새로 서로 신뢰한 상대와는 잇습니다.
      for (const [d] of this.peers) if (!this.isDirect(d)) this.closePeer(d);
      for (const d of this.held.keys()) if (!others.has(d)) this.held.delete(d);
      for (const d of others) {
        if (!this.isDirect(d) || this.peers.has(d)) continue;
        const held = this.held.get(d);
        this.held.delete(d);
        if (held) for (const p of held) void this.handleSignal(d, this.snap.call, p);
        else if (this.o.myDevice < d) this.createPeer(d);
      }
      this.syncTor();
      return;
    }
    if (this.snap.topology === 'sfu') {
      this.refreshSfuConnection();
      return;
    }
    for (const device of others) {
      if (!this.peers.has(device) && this.o.myDevice < device) this.createPeer(device);
    }
  }

  /** Tor 통화에서 이 상대와 WebRTC로 바로 잇는지: 서로 신뢰함(데몬이 알려 줌), 프레임 암호화를 쓸 수 있음. */
  private isDirect(device: DeviceId): boolean {
    return (
      this.torMode &&
      (this.mode === 'encoded-streams' || this.mode === 'script-transform') &&
      !!this.snap.participants.find((p) => p.device === device)?.direct
    );
  }

  /** Tor로 보내는 상대: WebRTC로 이어지지 않은 참가자 모두(직접 잇는 상대도 연결되기 전·끊긴 동안은 Tor로). */
  private viaTor(): DeviceId[] {
    return this.snap.participants
      .map((p) => p.device)
      .filter((d) => d !== this.o.myDevice && this.peers.get(d)?.pc.connectionState !== 'connected');
  }

  /** Tor 통화: Tor 소리·비상용 화면을 받을 상대와 연결 표시를 맞춥니다. */
  private syncTor() {
    if (!this.torMode) return;
    const tor = this.viaTor();
    this.tor?.setPeers(tor);
    this.tor?.screen.setViewers(this.torScreenOn ? this.snap.viewers.filter((d) => tor.includes(d)) : []);
    const connection: Record<string, string> = {};
    for (const p of this.snap.participants) {
      if (p.device !== this.o.myDevice) connection[p.device] = tor.includes(p.device) ? 'tor' : 'connected';
    }
    if (JSON.stringify(connection) !== JSON.stringify(this.snap.connection)) this.emit({ connection });
  }

  /** 지금 서로 신뢰해 WebRTC로 잇는 상대가 있는지. */
  private hasDirect(): boolean {
    return this.snap.participants.some((p) => p.device !== this.o.myDevice && this.isDirect(p.device));
  }

  private rtcConfig(): RTCConfiguration {
    const config: RTCConfiguration & { encodedInsertableStreams?: boolean } = {
      iceServers: this.o.iceServers,
      bundlePolicy: 'max-bundle',
      rtcpMuxPolicy: 'require',
    };
    if (this.mode === 'encoded-streams') config.encodedInsertableStreams = true;
    return config;
  }

  private encryptSender(sender: RTCRtpSender, kind: MediaKind) {
    if (!this.worker || this.mode === 'simulated' || this.transformed.has(sender)) return;
    if (attachSender(this.worker, this.mode as TransformMode, sender, kind, this.o.myDevice)) this.transformed.add(sender);
  }

  /** `origin`은 이 트랙을 보냈다고 알려진 기기: 다른 기기가 암호화한 프레임은 워커가 버립니다. */
  private decryptReceiver(receiver: RTCRtpReceiver, kind: MediaKind, origin: DeviceId) {
    if (!this.worker || this.mode === 'simulated' || this.transformed.has(receiver)) return;
    if (attachReceiver(this.worker, this.mode as TransformMode, receiver, kind, origin)) this.transformed.add(receiver);
  }

  // ───────────── 메시 연결 ─────────────

  private createPeer(device: DeviceId): Peer {
    if ((this.torMode || this.snap.topology === 'tor') && !this.isDirect(device)) {
      throw new Error(t('Tor 통화에서는 서로 신뢰한 상대와만 WebRTC로 잇습니다'));
    }
    const pc = new RTCPeerConnection(this.rtcConfig());
    const peer: Peer = { device, pc, polite: this.o.myDevice > device, makingOffer: false, ignoreOffer: false };
    this.peers.set(device, peer);
    // 신경망 코덱·화면 데이터 채널(양쪽이 같은 번호로 미리 만들어 첫 협상에 함께 실림).
    this.neural?.attach(device, pc);
    this.native?.attach(device, pc);

    // 먼저 제안하는 쪽만 트랜시버를 미리 만듭니다(응답하는 쪽은 제안에서 생긴 트랜시버를 씁니다). 마이크는 변환을 붙인
    // 뒤에 넣습니다(prepareTransceivers, 아래 설명).
    if (!peer.polite) {
      peer.audioTx = pc.addTransceiver('audio', { direction: 'sendrecv' });
      peer.videoTx = pc.addTransceiver('video', { direction: 'sendrecv' });
      peer.screenAudioTx = pc.addTransceiver('audio', { direction: 'sendrecv' });
      this.prepareTransceivers(peer);
    }

    pc.ontrack = (e) => this.onTrack(peer, e);
    pc.onicecandidate = (e) => {
      if (!e.candidate) return;
      void this.signal(device, {
        type: 'ice',
        candidate: e.candidate.candidate,
        sdp_mid: e.candidate.sdpMid,
        sdp_mline_index: e.candidate.sdpMLineIndex,
      });
    };
    pc.onnegotiationneeded = () => void this.offer(peer);
    pc.onconnectionstatechange = () => {
      if (pc.connectionState === 'failed') pc.restartIce();
      // Tor 통화: 이어지면 그 상대에게 Tor로 보내던 소리를 멈추고, 끊기면 다시 Tor로.
      if (this.torMode) this.syncTor();
      else this.emit({ connection: { ...this.snap.connection, [device]: pc.connectionState } });
    };
    if (!this.torMode) this.emit({ connection: { ...this.snap.connection, [device]: pc.connectionState } });
    return peer;
  }

  /** 제안을 만들어 보냅니다(협상이 필요할 때, 그리고 음성 설정이 바뀌어 다시 협상할 때). */
  private async offer(peer: Peer) {
    const pc = peer.pc;
    try {
      peer.makingOffer = true;
      const offer = await pc.createOffer();
      if (pc.signalingState !== 'stable') return;
      await pc.setLocalDescription({ type: 'offer', sdp: enableOpusDtx(offer.sdp ?? '') });
      void this.signal(peer.device, { type: 'offer', sdp: pc.localDescription?.sdp ?? '' });
    } catch {
      // 다음 협상에서 다시 시도
    } finally {
      peer.makingOffer = false;
    }
  }

  /**
   * 트랜시버에 암호화·마이크·코덱 설정을 붙입니다(양쪽 공통, 여러 번 불러도 됨).
   *
   * 순서가 중요합니다: 크로뮴(WebView2 15x)은 트랙이 이미 붙은 송신기에 나중에 단 변환과, 제안한 쪽이 응답을 적용하며
   * 받은 ontrack에서 수신기에 단 변환을 조용히 무시합니다. 그러면 암호문이 그대로 나가거나 그대로 디코더에 들어가
   * '삐익' 소리·깨진 화면이 됩니다. 그래서 트랜시버가 생기자마자(트랙을 넣기 전에) 송신·수신 변환을 모두 달고,
   * 마이크는 그다음에 넣습니다.
   */
  private prepareTransceivers(peer: Peer) {
    // 오디오 m-line은 첫째가 마이크, 둘째가 화면 소리입니다(제안하는 쪽이 이 순서로 만듦).
    const audios = audioTransceivers(peer.pc);
    peer.audioTx ??= audios[0];
    peer.screenAudioTx ??= audios[1];
    for (const t of peer.pc.getTransceivers()) {
      const kind = t.receiver.track.kind as MediaKind;
      this.encryptSender(t.sender, kind);
      this.decryptReceiver(t.receiver, kind, peer.device);
      if (t === peer.audioTx) {
        // 신경망 코덱으로 보내는 상대에게는 Opus 트랙을 비워 둡니다.
        if (this.mic && !t.sender.track && !this.neural?.sendingTo(peer.device)) void t.sender.replaceTrack(this.mic);
        this.tuneAudioSender(t.sender);
      } else if (kind === 'video') {
        peer.videoTx ??= t;
        preferVideoCodecs(t, this.media.screen.codec);
      }
      if (t.direction !== 'sendrecv' && t.direction !== 'stopped') t.direction = 'sendrecv';
    }
    this.applyScreenTrack(peer.device);
  }

  private onTrack(peer: Peer, e: RTCTrackEvent) {
    const kind = e.track.kind as MediaKind;
    this.decryptReceiver(e.receiver, kind, peer.device);
    // 둘째 오디오 m-line은 화면 소리: 화면을 보는 동안만 옵니다(보내는 쪽이 시청자에게만 붙임). 말하기 표시에는 넣지 않습니다.
    if (kind === 'audio' && e.transceiver && audioTransceivers(peer.pc).indexOf(e.transceiver) === 1) {
      const el = peer.screenAudioEl ?? new Audio();
      el.autoplay = true;
      el.srcObject = new MediaStream([e.track]);
      el.muted = this.snap.deafened;
      this.prepareAudioEl(el);
      el.volume = this.screenGain(peer.device);
      void el.play().catch(() => undefined);
      peer.screenAudioEl = el;
      return;
    }
    if (kind === 'audio') {
      const stream = new MediaStream([e.track]);
      const el = peer.audioEl ?? new Audio();
      el.autoplay = true;
      el.srcObject = stream;
      el.muted = this.snap.deafened;
      this.prepareAudioEl(el);
      void el.play().catch(() => undefined);
      peer.audioEl = el;
      this.watchLevel(peer.device, stream);
    } else {
      const stream = new MediaStream([e.track]);
      peer.remoteVideo = stream;
      // 데이터 채널로 받는 화면(ffmpeg 인코더)이 이미 보이면 재협상으로 생긴 빈 트랙으로 바꾸지 않습니다.
      if (this.snap.watching.includes(peer.device) && !this.native?.remoteScreen(peer.device)) {
        this.remoteVideos.set(peer.device, stream);
        this.emit({});
      }
      // 보내는 쪽이 WebRTC로 바꿨으면(공유 중 코덱을 바꿔 이 기기가 못 풀거나, 풀다가 실패함) 이 트랙에 다시 영상이
      // 옵니다. 그때는 멈춘 데이터 채널 화면 대신 이 트랙을 보여 줍니다(데이터 채널로 다시 오면 다음 키 프레임에서 돌아감).
      e.track.onunmute = () => {
        if (!this.snap.watching.includes(peer.device) || this.remoteVideos.get(peer.device) === stream) return;
        if (this.peers.get(peer.device) !== peer || peer.remoteVideo !== stream) return;
        this.native?.forget(peer.device);
        this.remoteVideos.set(peer.device, stream);
        this.emit({});
      };
    }
  }

  private closePeer(device: DeviceId) {
    this.pendingIce.delete(device);
    this.neural?.detach(device);
    this.native?.detach(device);
    const p = this.peers.get(device);
    if (!p) return;
    p.pc.ontrack = null;
    p.pc.onicecandidate = null;
    p.pc.onnegotiationneeded = null;
    p.pc.onconnectionstatechange = null;
    p.pc.close();
    for (const el of [p.audioEl, p.screenAudioEl]) {
      if (!el) continue;
      el.srcObject = null;
      el.remove();
    }
    this.analysers.delete(device);
    this.remoteVideos.delete(device);
    this.peers.delete(device);
    const connection = { ...this.snap.connection };
    delete connection[device];
    this.emit({ connection });
  }

  // ───────────── SFU 연결 ─────────────

  private nextSfuSession(): number {
    // 호스트는 같은 기기의 더 큰 세션 번호만 새 세션으로 받습니다. 앱을 다시 켜도 커지도록 시각(초)에서 시작합니다.
    this.sfuSession = Math.max(this.sfuSession + 1, Math.floor(Date.now() / 1000) % 0x1_0000_0000);
    return this.sfuSession;
  }

  private async openSfu(host: DeviceId) {
    this.clearSfuRetry();
    const session = this.nextSfuSession();
    const pc = new RTCPeerConnection(this.rtcConfig());
    // 세션마다 한 번만 제안하므로 올릴 트랙을 미리 다 담습니다(화면은 공유할 때 replaceTrack).
    // 변환을 단 뒤에 마이크를 넣습니다(prepareTransceivers 설명: 트랙이 붙은 뒤 단 변환은 무시됨).
    const audioTx = pc.addTransceiver('audio', { direction: 'sendonly' });
    const screenTx = pc.addTransceiver('video', { direction: 'sendonly', sendEncodings: screenEncodings() });
    preferVp8(screenTx);
    const link: SfuLink = {
      host,
      session,
      pc,
      audioTx,
      screenTx,
      tracks: new Map(),
      audio: new Map(),
      screens: new Map(),
      earlyIce: [],
      offered: false,
      closed: false,
    };
    this.sfu = link;
    this.encryptSender(audioTx.sender, 'audio');
    this.encryptSender(screenTx.sender, 'video');
    if (this.mic) void audioTx.sender.replaceTrack(this.mic).catch(() => undefined);
    this.tuneAudioSender(audioTx.sender);
    pc.ontrack = (e) => this.onSfuTrack(link, e);
    pc.onicecandidate = (e) => {
      if (!e.candidate || link.closed) return;
      const ice: SignalPayload = {
        type: 'sfu_ice',
        session,
        candidate: e.candidate.candidate,
        sdp_mid: e.candidate.sdpMid,
        sdp_mline_index: e.candidate.sdpMLineIndex,
      };
      if (link.offered) void this.sendSfu(link, ice);
      else link.earlyIce.push(ice);
    };
    pc.onconnectionstatechange = () => {
      if (link.closed) return;
      this.setSfuState(pc.connectionState);
      if (pc.connectionState === 'connected') this.sfuAttempts = 0;
      // 재협상은 호스트만 제안하므로 ICE 재시작 대신 새 세션으로 다시 붙습니다.
      if (pc.connectionState === 'failed') this.retrySfu(link);
    };
    this.setSfuState(pc.connectionState);
    this.applySfuScreen();
    try {
      const offer = await pc.createOffer();
      if (link.closed) return;
      await pc.setLocalDescription({ type: 'offer', sdp: enableOpusDtx(offer.sdp ?? '') });
    } catch {
      this.retrySfu(link);
      return;
    }
    if (link.closed) return;
    const tracks: SfuTrack[] = [];
    if (audioTx.mid) tracks.push({ mid: audioTx.mid, source: 'audio' });
    if (screenTx.mid) tracks.push({ mid: screenTx.mid, source: 'screen' });
    const sent = this.sendSfu(link, { type: 'sfu_offer', session, sdp: pc.localDescription?.sdp ?? '', tracks });
    link.offered = true;
    for (const ice of link.earlyIce.splice(0)) void this.sendSfu(link, ice);
    // 호스트가 아직 통화방을 열지 않았거나(결정을 막 받음) 가득 찼으면 거절됩니다: 잠시 뒤 다시.
    if (!(await sent)) this.retrySfu(link);
  }

  private onSfuTrack(link: SfuLink, e: RTCTrackEvent) {
    const kind = e.track.kind as MediaKind;
    const mid = e.transceiver?.mid;
    const origin = mid ? link.tracks.get(mid) : undefined;
    if (!origin || link.closed) return;
    // 받는 트랙(=보낸 참가자)마다 따로 복호화 변환을 붙이고 트랙 표의 출처로 묶습니다. 호스트가 다른 참가자의
    // 프레임을 이 트랙에 붙이면 AAD가 맞지 않아 버려집니다.
    this.decryptReceiver(e.receiver, kind, origin.device);
    const stream = new MediaStream([e.track]);
    if (origin.source === 'audio') {
      const el = link.audio.get(origin.device) ?? new Audio();
      el.autoplay = true;
      el.srcObject = stream;
      el.muted = this.snap.deafened;
      this.prepareAudioEl(el);
      void el.play().catch(() => undefined);
      link.audio.set(origin.device, el);
      this.watchLevel(origin.device, stream);
    } else if (origin.source === 'screen') {
      link.screens.set(origin.device, stream);
      if (this.snap.watching.includes(origin.device)) {
        this.remoteVideos.set(origin.device, stream);
        this.emit({});
      }
    }
    // 카메라는 아직 데스크톱 앱이 보내지 않으므로 받아도 쓰지 않습니다.
  }

  /** 트랙 표에서 빠진 출처의 소리·화면을 치웁니다. */
  private pruneSfuTracks(link: SfuLink) {
    const has = (device: DeviceId, source: TrackSource) =>
      [...link.tracks.values()].some((t) => t.device === device && t.source === source);
    for (const [device, el] of link.audio) {
      if (has(device, 'audio')) continue;
      el.srcObject = null;
      el.remove();
      link.audio.delete(device);
      this.analysers.delete(device);
    }
    let changed = false;
    for (const device of [...link.screens.keys()]) {
      if (has(device, 'screen')) continue;
      link.screens.delete(device);
      changed = this.remoteVideos.delete(device) || changed;
    }
    if (changed) this.emit({});
  }

  private closeSfu() {
    const link = this.sfu;
    if (!link) return;
    this.sfu = null;
    link.closed = true;
    link.pc.ontrack = null;
    link.pc.onicecandidate = null;
    link.pc.onconnectionstatechange = null;
    link.pc.close();
    for (const [device, el] of link.audio) {
      el.srcObject = null;
      el.remove();
      this.analysers.delete(device);
    }
    for (const device of link.screens.keys()) this.remoteVideos.delete(device);
    if (link.screens.size > 0) this.emit({});
  }

  /** 세션을 버리고 잠시 뒤 새 세션으로 다시 붙습니다(실패할수록 오래 기다림). */
  private retrySfu(link: SfuLink) {
    if (this.sfu !== link) return;
    this.closeSfu();
    if (this.closed || this.sfuTimer) return;
    const delay = Math.min(SFU_RETRY_MAX_MS, SFU_RETRY_MIN_MS * 2 ** this.sfuAttempts);
    this.sfuAttempts++;
    this.setSfuState('waiting');
    this.sfuTimer = setTimeout(() => {
      this.sfuTimer = null;
      const host = this.snap.sfuHost;
      if (this.closed || this.snap.topology !== 'sfu' || !host || this.sfu) return;
      void this.openSfu(host);
    }, delay);
  }

  private clearSfuRetry() {
    if (this.sfuTimer) clearTimeout(this.sfuTimer);
    this.sfuTimer = null;
  }

  private setSfuState(state: RTCPeerConnectionState | 'waiting') {
    if (this.snap.sfuState !== state) this.emit({ sfuState: state });
    this.refreshSfuConnection();
  }

  /** SFU에서는 모든 참가자가 호스트 연결 하나를 거치므로 그 상태를 참가자마다 보여 줍니다. */
  private refreshSfuConnection() {
    if (this.snap.topology !== 'sfu' || this.mode === 'simulated') return;
    const state = this.sfu?.pc.connectionState ?? 'connecting';
    const connection: Record<string, string> = {};
    for (const p of this.snap.participants) if (p.device !== this.o.myDevice) connection[p.device] = state;
    const prev = this.snap.connection;
    const same =
      Object.keys(prev).length === Object.keys(connection).length && Object.entries(connection).every(([k, v]) => prev[k] === v);
    if (!same) this.emit({ connection });
  }

  /** 호스트에게 SFU 신호를 보냅니다(순서 유지). 지난 세션 신호는 버리고, 데몬이 거절하면 false. */
  private sendSfu(link: SfuLink, payload: SignalPayload, evenIfClosed = false): Promise<boolean> {
    if (this.mode === 'simulated') return Promise.resolve(true);
    const params = { space: this.o.space, channel: this.o.channel, call: this.snap.call, to: link.host, payload };
    return this.outbox.run(async () => {
      if (link.closed && !evenIfClosed) return false;
      try {
        await this.o.client.call('voice.signal', params);
        return true;
      } catch {
        return false;
      }
    });
  }

  private selectPayload(device: DeviceId, watching: boolean): SignalPayload {
    return { type: 'sfu_select', device, source: 'screen', layer: watching ? (this.snap.layers[device] ?? 'auto') : 'off' };
  }

  private async processSfu(from: DeviceId, payload: SfuPayload) {
    const link = this.sfu;
    // 내가 호스트면 내 데몬이 from = 나로 넘겨 줍니다.
    if (!link || from !== link.host) return;
    switch (payload.type) {
      case 'sfu_answer': {
        if (payload.session !== link.session || link.pc.signalingState !== 'have-local-offer') return;
        try {
          await link.pc.setRemoteDescription({ type: 'answer', sdp: this.remoteSdp(payload.sdp) });
        } catch {
          this.retrySfu(link);
          return;
        }
        // 새 호스트는 내 화면 선택을 모르므로 다시 알립니다(기본은 받지 않음).
        for (const d of this.snap.watching) void this.sendSfu(link, this.selectPayload(d, true));
        break;
      }
      case 'sfu_offer': {
        // 호스트는 내 제안에 응답한 뒤에만 제안합니다.
        if (payload.session !== link.session || link.pc.signalingState !== 'stable' || !link.pc.remoteDescription) return;
        // 트랙 표를 먼저 바꿔 둡니다(ontrack은 원격 설명을 적용하는 도중에 옴).
        link.tracks = new Map(
          payload.tracks.flatMap((t) => (t.device ? [[t.mid, { device: t.device, source: t.source }] as const] : [])),
        );
        try {
          await link.pc.setRemoteDescription({ type: 'offer', sdp: this.remoteSdp(payload.sdp) });
          const answer = await link.pc.createAnswer();
          await link.pc.setLocalDescription({ type: 'answer', sdp: enableOpusDtx(answer.sdp ?? '') });
        } catch {
          this.retrySfu(link);
          return;
        }
        if (link.closed) return;
        // 응답이 닿지 않으면 호스트는 응답을 기다리다 세션을 닫으므로, 기다리지 않고 바로 새 세션으로 붙습니다.
        void this.sendSfu(link, { type: 'sfu_answer', session: link.session, sdp: link.pc.localDescription?.sdp ?? '', tracks: [] }).then(
          (ok) => {
            if (!ok) this.retrySfu(link);
          },
        );
        this.pruneSfuTracks(link);
        break;
      }
      case 'sfu_close':
        if (payload.session === link.session) this.retrySfu(link);
        break;
      case 'sfu_ice':
      case 'sfu_select':
        // 호스트는 보내지 않음(호스트 후보는 SDP에 있음).
        break;
    }
  }

  // ───────────── 신호 ─────────────

  /**
   * 신호를 보냅니다. 호출한 순서대로 하나씩 보내므로(큐에 넣는 것은 동기) 제안 뒤에 생긴 후보가 제안을 앞지르지 않습니다.
   */
  private signal(to: DeviceId, payload: SignalPayload): Promise<void> {
    if (this.mode === 'simulated') return Promise.resolve();
    const params = { space: this.o.space, channel: this.o.channel, call: this.snap.call, to, payload };
    const send = async () => {
      await this.o.client.call('voice.signal', params).catch(() => undefined);
    };
    // 후보는 앞 신호의 답을 기다리지 않고 바로 보냅니다: 신호는 Tor로 가서 하나에 1초 가까이 걸려, 줄을 세우면 후보 십여
    // 개에 연결이 10초 넘게 늦었습니다. 제안·응답보다 먼저 닿은 후보는 받는 쪽이 모아 두었다 넣습니다(pendingIce).
    return payload.type === 'ice' ? send() : this.outbox.run(send);
  }

  /** 'voice.signal' 알림 처리. 도착 순서대로 하나씩 처리합니다. */
  handleSignal(from: DeviceId, call: string, payload: SignalPayload): Promise<void> {
    return this.inbox.run(() => this.processSignal(from, call, payload));
  }

  /** 'voice.media' 알림(Tor 통화의 봉인한 소리 패킷). */
  handleMedia(from: DeviceId, call: string, data: string) {
    if (this.closed || call !== this.snap.call) return;
    void this.tor?.handle(from, data);
  }

  /** 원격 설명이 정해진 뒤, 먼저 와 있던 후보를 넣습니다. */
  private async flushIce(peer: Peer) {
    for (const c of this.pendingIce.take(peer.device)) {
      await peer.pc.addIceCandidate(c).catch(() => undefined);
    }
  }

  private async processSignal(from: DeviceId, call: string, payload: SignalPayload) {
    if (this.closed || call !== this.snap.call) return;
    // Tor 통화에서는 서로 신뢰한 상대의 WebRTC 신호에만 답합니다(데몬도 내가 신뢰한 상대의 것만 넘김). 상대가 먼저 이쪽을
    // 직접 상대로 본 사이에 온 제안·후보는 이쪽도 직접이 되면 씁니다(syncPeers).
    if (
      (this.torMode || this.snap.topology === 'tor') &&
      !['leave', 'state', 'join', 'watch_screen'].includes(payload.type) &&
      !(this.torMode && !isSfuPayload(payload) && this.isDirect(from))
    ) {
      if (this.torMode && (payload.type === 'offer' || payload.type === 'ice')) {
        const held = this.held.get(from) ?? [];
        held.push(payload);
        this.held.set(from, held.slice(-MAX_HELD));
      }
      return;
    }
    if (isSfuPayload(payload)) return this.processSfu(from, payload);
    if (from === this.o.myDevice) return;
    const sfu = this.snap.topology === 'sfu';
    switch (payload.type) {
      case 'offer': {
        // SFU로 바뀌었는데 아직 메시인 참가자가 보낸 제안은 받지 않습니다(그쪽도 곧 바뀜).
        if (this.mode === 'simulated' || sfu) return;
        const peer = this.peers.get(from) ?? this.createPeer(from);
        const pc = peer.pc;
        const collision = peer.makingOffer || pc.signalingState !== 'stable';
        peer.ignoreOffer = !peer.polite && collision;
        if (peer.ignoreOffer) return;
        await pc.setRemoteDescription({ type: 'offer', sdp: this.remoteSdp(payload.sdp) });
        await this.flushIce(peer);
        this.prepareTransceivers(peer);
        const answer = await pc.createAnswer();
        await pc.setLocalDescription({ type: 'answer', sdp: enableOpusDtx(answer.sdp ?? '') });
        // 기다리지 않음: 보내기는 outbox가 순서를 지키고, 데몬 응답이 늦어도 다음 신호 처리가 막히지 않게 합니다.
        void this.signal(from, { type: 'answer', sdp: pc.localDescription?.sdp ?? '' });
        break;
      }
      case 'answer': {
        const peer = this.peers.get(from);
        if (peer && peer.pc.signalingState === 'have-local-offer') {
          await peer.pc.setRemoteDescription({ type: 'answer', sdp: this.remoteSdp(payload.sdp) });
          await this.flushIce(peer);
        }
        break;
      }
      case 'ice': {
        if (this.mode === 'simulated' || sfu) return;
        const candidate: RTCIceCandidateInit = {
          candidate: payload.candidate,
          sdpMid: payload.sdp_mid ?? null,
          sdpMLineIndex: payload.sdp_mline_index ?? null,
        };
        const peer = this.peers.get(from);
        if (!peer || !peer.pc.remoteDescription) {
          // 제안·응답보다 먼저 온 후보: 지금 넣으면 실패해 버려지므로 원격 설명이 정해질 때까지 모아 둡니다.
          this.pendingIce.push(from, candidate);
          return;
        }
        try {
          await peer.pc.addIceCandidate(candidate);
        } catch {
          // 다시 시작한 ICE의 후보가 새 제안·응답보다 먼저 왔을 수 있어 모아 두었다가 다음 원격 설명 뒤에 한 번 더 넣습니다
          // (무시한 제안의 후보면 그때도 실패하고 버려짐).
          this.pendingIce.push(from, candidate);
        }
        break;
      }
      case 'watch_screen': {
        const set = new Set(this.snap.viewers);
        if (payload.watching) set.add(from);
        else set.delete(from);
        this.emit({ viewers: [...set] });
        this.applyNativeViewers();
        this.applyScreenTrack(from);
        this.applySfuScreen();
        this.syncTor();
        break;
      }
      case 'leave':
        this.closePeer(from);
        // 같은 기기가 다시 참가하면(앱을 다시 켬) 데몬이 먼저 나감을 알립니다: 지난 연결은 버리고 새로 잇습니다. 새로 들어온
        // 그 기기는 아직 내 화면을 보지 않습니다.
        if (this.snap.viewers.includes(from)) this.emit({ viewers: this.snap.viewers.filter((d) => d !== from) });
        this.syncPeers(this.snap.participants);
        this.applyScreenAll();
        break;
      case 'state':
      case 'join':
        // 참가자 상태는 'voice.participants' 알림으로 옵니다.
        break;
    }
  }

  // ───────────── 마이크·스피커 ─────────────

  private get devices(): DevicePrefs {
    return this.media.devices;
  }

  /** 고른 마이크를 엽니다. 그 장치가 없거나 열리지 않으면 시스템 기본 마이크로 엽니다(그때는 알림). */
  private async openMic(): Promise<MediaStreamTrack | null> {
    const id = await findDevice('audioinput', this.devices.input);
    const open = async (deviceId?: string) => {
      const stream = await navigator.mediaDevices.getUserMedia({ audio: micConstraints(this.media.audio, deviceId), video: false });
      return stream.getAudioTracks()[0] ?? null;
    };
    if (this.devices.input && id) {
      try {
        return await open(id);
      } catch {
        // 아래에서 기본 마이크로
      }
    }
    const track = await open();
    if (this.devices.input && !id) {
      // 앱을 켠 뒤 처음 여는 마이크면 권한 전이라 목록에 장치 이름·ID가 없습니다. 기본 마이크를 연 지금 다시 찾습니다.
      const later = await findDevice('audioinput', this.devices.input);
      if (later) {
        try {
          const chosen = await open(later);
          track?.stop();
          return chosen;
        } catch {
          // 기본 마이크 그대로
        }
      }
    }
    if (this.devices.input) {
      this.emit({ micError: t('고른 마이크({label})를 찾지 못해 기본 마이크를 씁니다', { label: this.devices.input.label || t('이름 없음') }) });
    }
    return track;
  }

  /** 보낼 트랙: 마이크 음량이 100%면 마이크 그대로, 아니면 증폭한 트랙(만들지 못하면 그대로). */
  private micTrackFor(raw: MediaStreamTrack | null): MediaStreamTrack | null {
    this.micGain?.close();
    this.micGain = null;
    if (!raw || this.devices.inputVolume === 100) return raw;
    this.micGain = createMicGain(raw, this.devices.inputVolume);
    return this.micGain?.track ?? raw;
  }

  /** 보내는 마이크 트랙을 갈아 끼웁니다(메시 송신기·SFU·신경망·Tor·말하기 표시). */
  private swapMic(next: MediaStreamTrack | null) {
    const prev = this.mic;
    if (next === prev) return;
    this.mic = next;
    if (next) next.enabled = !this.snap.muted;
    for (const p of this.peers.values()) {
      const sender = p.audioTx?.sender;
      // 신경망 코덱으로 보내는 상대는 Opus 트랙을 비워 둔 그대로.
      if (sender && sender.track === prev && !this.neural?.sendingTo(p.device)) void sender.replaceTrack(next).catch(() => undefined);
    }
    if (this.sfu && this.sfu.audioTx.sender.track === prev) void this.sfu.audioTx.sender.replaceTrack(next).catch(() => undefined);
    this.neural?.setMic(next);
    this.tor?.setMic(next);
    if (next) this.watchLevel(this.o.myDevice, new MediaStream([next]));
    else this.analysers.delete(this.o.myDevice);
  }

  /** 고른 마이크가 바뀌었을 때 새로 엽니다(못 열면 지금 마이크를 그대로 씀). */
  private async reopenMic() {
    if (this.mode === 'simulated') return;
    let raw: MediaStreamTrack | null;
    try {
      raw = await this.openMic();
    } catch (e) {
      this.emit({ micError: e instanceof Error ? e.message : t('마이크를 열 수 없습니다') });
      return;
    }
    if (this.closed) {
      raw?.stop();
      return;
    }
    const oldRaw = this.rawMic;
    const oldGain = this.micGain;
    this.micGain = null;
    this.rawMic = raw;
    const next = raw && this.devices.inputVolume !== 100 ? createMicGain(raw, this.devices.inputVolume) : null;
    this.micGain = next;
    this.swapMic(next?.track ?? raw);
    oldGain?.close();
    if (oldRaw !== raw) oldRaw?.stop();
    if (raw && this.snap.micError) this.emit({ micError: undefined });
  }

  /** 마이크 음량만 바뀌었을 때: 증폭 줄이 있으면 값만, 없으면 만들어 트랙을 갈아 끼웁니다. */
  private applyMicVolume() {
    if (this.micGain) {
      this.micGain.setVolume(this.devices.inputVolume);
      return;
    }
    if (!this.rawMic || this.devices.inputVolume === 100) return;
    const g = createMicGain(this.rawMic, this.devices.inputVolume);
    if (!g) return;
    this.micGain = g;
    this.swapMic(g.track);
  }

  private async resolveSink() {
    this.sink = (await findDevice('audiooutput', this.devices.output)) ?? '';
  }

  /** 받은 소리 요소에 스피커 장치·음량을 겁니다. */
  private prepareAudioEl(el: HTMLAudioElement) {
    el.volume = Math.min(1, Math.max(0, this.devices.outputVolume / 100));
    if (this.sink) applySink(el, this.sink);
  }

  /** 스피커 장치·음량을 지금 소리 모두에(메시·SFU 소리 요소, 신경망·Tor 재생기). */
  private applyOutput() {
    if (this.closed) return;
    const els = [...[...this.peers.values()].flatMap((p) => [p.audioEl, p.screenAudioEl]), ...(this.sfu?.audio.values() ?? [])];
    for (const el of els) {
      if (!el) continue;
      el.volume = Math.min(1, Math.max(0, this.devices.outputVolume / 100));
      applySink(el, this.sink);
    }
    for (const p of this.peers.values()) if (p.screenAudioEl) p.screenAudioEl.volume = this.screenGain(p.device);
    const volume = this.devices.outputVolume / 100;
    this.tor?.setOutput(this.sink, volume);
    this.neural?.setOutput(this.sink, volume);
  }

  /** 화면 소리 크기(0~1): 스피커 음량 × 그 화면에 고른 음량. */
  private screenGain(device: DeviceId): number {
    return Math.min(1, Math.max(0, (this.devices.outputVolume / 100) * ((this.snap.screenVolume[device] ?? 100) / 100)));
  }

  /** 보는 화면의 소리 크기를 바꿉니다(%, 이 통화 동안만). 화면 소리는 메시·직접 연결로만 옵니다. */
  setScreenVolume(device: DeviceId, percent: number) {
    this.emit({ screenVolume: { ...this.snap.screenVolume, [device]: Math.min(100, Math.max(0, Math.round(percent))) } });
    const el = this.peers.get(device)?.screenAudioEl;
    if (el) el.volume = this.screenGain(device);
  }

  /** 워커 통계(2초마다)로 상대마다 소리를 받는 상태를 정합니다. */
  private onStats(st: WorkerStats) {
    const now = Date.now();
    const rx: Record<DeviceId, RxState> = {};
    const others = this.snap.participants.filter((p) => p.device !== this.o.myDevice);
    for (const p of others) {
      const d = p.device;
      if (!this.rxSince.has(d)) this.rxSince.set(d, now);
      const cur = st.rx?.[d] ?? { ok: 0, bad: 0 };
      const prev = this.rxPrev[d] ?? { ok: 0, bad: 0 };
      const ok = cur.ok - prev.ok;
      const bad = cur.bad - prev.bad;
      if (ok > 0) {
        rx[d] = 'ok';
        this.heard.add(d);
        this.rxQuiet.delete(d);
      } else if (bad > 0) {
        rx[d] = 'bad';
      } else {
        // 두 번 연달아(약 4초) 아무것도 없고, 들어온 지 조금 지났고, 음소거하지 않은 상대만.
        const quiet = (this.rxQuiet.get(d) ?? 0) + 1;
        this.rxQuiet.set(d, quiet);
        const connecting = this.snap.connection[d] !== undefined && !isLinkUp(this.snap.connection[d]);
        if (quiet >= 2 && !p.muted && !connecting && now - (this.rxSince.get(d) ?? now) >= RX_GRACE_MS) rx[d] = 'none';
      }
    }
    for (const d of [...this.rxSince.keys()]) {
      if (!others.some((p) => p.device === d)) {
        this.rxSince.delete(d);
        this.rxQuiet.delete(d);
      }
    }
    this.rxPrev = st.rx ?? {};
    this.emit({ stats: st, rx, heard: [...this.heard] });
  }

  async setMuted(muted: boolean) {
    if (this.mic) this.mic.enabled = !muted;
    this.neural?.setMuted(muted);
    this.tor?.setMuted(muted);
    this.emit({ muted });
    await this.pushState();
  }

  async setDeafened(deafened: boolean) {
    for (const p of this.peers.values()) {
      for (const el of [p.audioEl, p.screenAudioEl]) if (el) el.muted = deafened;
    }
    for (const el of this.sfu?.audio.values() ?? []) el.muted = deafened;
    // 디스코드처럼 듣기를 끄면 말하기도 끕니다.
    const muted = deafened ? true : this.snap.muted;
    if (this.mic) this.mic.enabled = !muted;
    this.neural?.setDeafened(deafened);
    this.neural?.setMuted(muted);
    this.tor?.setDeafened(deafened);
    this.tor?.setMuted(muted);
    this.emit({ deafened, muted });
    await this.pushState();
  }

  private async pushState() {
    await this.o.client
      .call('voice.state', {
        space: this.o.space,
        channel: this.o.channel,
        muted: this.snap.muted,
        deafened: this.snap.deafened,
        screen: this.snap.sharing,
      })
      .catch(() => undefined);
  }

  // ───────────── 화면공유 ─────────────

  /**
   * `torScreen`: Tor 통화에서 Tor로 잇는 상대에게도 비상용 화면을 보낼지(store가 경고를 확인받음). 서로 신뢰해 WebRTC로 잇는
   * 상대에게는 메시 통화처럼 보냅니다.
   */
  async startScreenShare(hint: ContentHint, torScreen = false) {
    // 공유를 시작하는 동안 한 번 더 누르면 무시합니다.
    if (this.screen || this.shareStarting || this.closed) return;
    if (this.torMode && !this.hasDirect()) {
      if (!torScreen) throw new Error(t('Tor 비상용 화면공유는 경고를 확인한 뒤에만 합니다'));
      return this.startTorScreen(hint);
    }
    // ffmpeg 인코더를 이 기기에서 쓸 수 없으면(폴더 없음·GPU 없음) 웹뷰 인코더로 공유합니다. Tor 상대에게도 보내면 그 트랙을
    // 비상용 인코더에도 넘기므로 웹뷰 인코더로.
    if (!torScreen && this.useNative() && (await this.nativeUsable())) return this.startNativeScreen(hint);
    if (this.screen || this.shareStarting || this.closed) return;
    this.shareStarting = true;
    try {
      const stream = await navigator.mediaDevices.getDisplayMedia({
        video: { frameRate: screenFps(this.media.screen, hint) },
        audio: false,
      });
      const track = stream.getVideoTracks()[0];
      if (!track) throw new Error(t('화면을 가져오지 못했습니다'));
      if (this.closed || track.readyState === 'ended') {
        track.stop();
        return;
      }
      track.contentHint = hint;
      track.onended = () => void this.stopScreenShare();
      this.screen = track;
      this.shareWindow = null;
      if (torScreen && this.torMode && this.tor) {
        try {
          await this.tor.screen.start(track);
          this.torScreenOn = true;
        } catch (e) {
          this.emit({ micError: t('Tor로 잇는 상대에게는 화면을 보내지 못합니다: {error}', { error: errorText(e) }) });
        }
      }
      this.emit({ sharing: true, contentHint: hint });
      this.applyScreenAll();
      void this.openScreenAudio();
      await this.pushState();
    } finally {
      this.shareStarting = false;
    }
  }

  /** Tor 통화의 비상용 화면공유(store가 경고를 확인받은 뒤에 부름). */
  private async startTorScreen(hint: ContentHint) {
    const tor = this.tor;
    if (!tor && this.mode !== 'simulated') throw new Error(t('Tor 통화가 아직 준비되지 않았습니다'));
    this.shareStarting = true;
    try {
      const stream = await navigator.mediaDevices.getDisplayMedia({ video: { frameRate: 5 }, audio: false });
      const track = stream.getVideoTracks()[0];
      if (!track) throw new Error(t('화면을 가져오지 못했습니다'));
      if (this.closed || track.readyState === 'ended') {
        track.stop();
        return;
      }
      track.contentHint = hint;
      track.onended = () => void this.stopScreenShare();
      if (tor) {
        try {
          await tor.screen.start(track);
        } catch (e) {
          track.stop();
          throw e;
        }
      }
      this.torScreenOn = true;
      this.screen = track;
      this.emit({ sharing: true, contentHint: hint });
      this.applyScreenAll();
      await this.pushState();
    } finally {
      this.shareStarting = false;
    }
  }

  /**
   * 고른 화면 인코더가 ffmpeg 인코더이고 이 앱에서 쓸 수 있는지(데스크톱 앱, 선택 창이 있을 때, 메시 통화). SFU 통화에서는
   * 데이터 채널로 받을 시청자가 없어 앱 인코딩 → 이 기기에서 풀기 → VP8로 다시 인코딩만 남으므로 웹뷰 인코더로 바로 보냅니다.
   */
  private useNative(): boolean {
    return (
      this.media.screen.encoder !== 'webrtc' &&
      (this.snap.topology === 'mesh' || this.torMode) &&
      !!this.native &&
      !!this.o.client.transport?.screen &&
      !!this.o.pickScreenSource
    );
  }

  private async nativeUsable(): Promise<boolean> {
    const encoder = this.media.screen.encoder;
    if (encoder === 'webrtc' || !this.native) return false;
    const status = await this.o.client.transport?.screen?.status().catch(() => null);
    // 이 기기가 풀지 못하는 코덱이면 미리보기와 WebRTC로 다시 보낼 트랙이 비므로 웹뷰 인코더로.
    return !!status?.encoders.find((e) => e.id === encoder)?.available && (await this.native.canDecode(encoder));
  }

  /** ffmpeg 인코더로 공유: 창·모니터를 고르고, 앱이 캡처·인코딩을 시작합니다. */
  private async startNativeScreen(hint: ContentHint) {
    const native = this.native;
    const transport = this.o.client.transport?.screen;
    const encoder = this.media.screen.encoder;
    if (!native || !transport || encoder === 'webrtc' || this.screen || this.shareStarting || this.closed) return;
    this.shareStarting = true;
    try {
      const source = await this.o.pickScreenSource?.();
      if (!source || this.closed) return;
      const track = await native.start(transport, source, nativeMaxHeight(this.media.screen), nativeSettings(this.media.screen, encoder, hint));
      if (this.closed) {
        native.stop();
        return;
      }
      track.contentHint = hint;
      this.screen = track;
      this.nativeSharing = true;
      this.shareWindow = source.kind === 'window' ? source.id : null;
      this.emit({ sharing: true, contentHint: hint });
      this.applyScreenAll();
      void this.openScreenAudio();
      await this.pushState();
    } finally {
      this.shareStarting = false;
    }
  }

  /** ffmpeg 인코더 공유가 스스로 끝났을 때(공유한 창을 닫음·인코더 오류). */
  private onNativeEnded(reason: string) {
    if (!this.nativeSharing) return;
    void this.stopScreenShare();
    if (reason) this.emit({ micError: t('화면공유가 끝났습니다: {reason}', { reason }) });
  }

  /** 화면 소리를 잡아 시청자에게 보냅니다(메시, 데스크톱 앱, 설정이 켜졌을 때). 못 잡으면 화면만 보내고 알립니다. */
  private async openScreenAudio() {
    const screenApi = this.o.client.transport?.screen;
    const screen = this.screen;
    // Tor 통화는 WebRTC로 잇는 시청자에게만 갑니다(applyScreenTrack).
    const mesh = this.snap.topology === 'mesh' || this.torMode;
    if (!screenApi || !screen || !this.media.screen.audio || this.screenAudio || !mesh || this.mode === 'simulated') return;
    let a: ScreenAudio | null = null;
    try {
      a = await startScreenAudio(
        screenApi,
        this.shareWindow,
        (reason) => {
          if (!a || this.screenAudio !== a) return;
          this.closeScreenAudio();
          if (reason) this.emit({ micError: t('화면 소리가 끊겼습니다: {reason}', { reason }) });
        },
        this.media.screen.audioVolume,
      );
    } catch (e) {
      if (this.screen === screen) this.emit({ micError: t('화면 소리를 보내지 못했습니다: {error}', { error: errorText(e) }) });
      return;
    }
    if (!a) return;
    // 기다리는 동안 공유를 멈췄거나 다시 시작했으면 버립니다.
    if (this.screen !== screen || this.screenAudio || this.closed) {
      a.stop();
      return;
    }
    this.screenAudio = a;
    // 여는 동안 음량을 바꿨을 수 있습니다.
    a.setVolume(this.media.screen.audioVolume);
    this.applyScreenAll();
  }

  private closeScreenAudio() {
    if (!this.screenAudio) return;
    this.screenAudio.stop();
    this.screenAudio = null;
    this.applyScreenAll();
  }

  /** 공유 중에 인코더 갈래(웹뷰 ↔ ffmpeg)를 바꾸면 멈추고 새 인코더로 다시 공유합니다(공유할 화면을 다시 고름). */
  private async restartShare() {
    const hint = this.snap.contentHint;
    const torScreen = this.torScreenOn;
    await this.stopScreenShare();
    try {
      await this.startScreenShare(hint, torScreen);
    } catch (e) {
      if ((e as Error)?.name !== 'NotAllowedError') this.emit({ micError: t('화면공유를 다시 시작하지 못했습니다: {error}', { error: errorText(e) }) });
    }
  }

  async stopScreenShare() {
    this.tor?.screen.stop();
    this.torScreenOn = false;
    if (this.nativeSharing) this.native?.stop();
    this.nativeSharing = false;
    this.screenAudio?.stop();
    this.screenAudio = null;
    this.shareWindow = null;
    if (this.screen) {
      this.screen.onended = null;
      this.screen.stop();
    }
    this.screen = null;
    this.screenPreview = null;
    this.emit({ sharing: false });
    this.applyScreenAll();
    await this.pushState();
  }

  /** SFU 세션을 지금 설정으로 새로 엽니다(앱의 제안은 세션마다 한 번뿐이라 바뀐 음성 설정은 새 세션으로). */
  private reopenSfu() {
    const link = this.sfu;
    if (!link || !this.started) return;
    this.closeSfu();
    this.clearSfuRetry();
    this.sfuAttempts = 0;
    void this.openSfu(link.host);
  }

  /** 음성(Opus) 협상 값이 바뀌었을 때: 메시는 새 제안, SFU는 새 세션. 협상 중인 연결은 그 협상이 끝날 때 새 값이 걸립니다. */
  private renegotiateAudio() {
    if (this.mode === 'simulated' || this.closed) return;
    for (const p of this.peers.values()) if (p.pc.signalingState === 'stable' && p.pc.remoteDescription) void this.offer(p);
    this.reopenSfu();
  }

  setContentHint(hint: ContentHint) {
    const before = this.snap.contentHint;
    if (this.screen) {
      this.screen.contentHint = hint;
      if (this.media.screen.fps === 'auto' && !this.nativeSharing) {
        void this.screen.applyConstraints?.({ frameRate: screenFps(this.media.screen, hint) }).catch(() => undefined);
      }
    }
    this.emit({ contentHint: hint });
    if (this.nativeSharing && hint !== before) this.restartNative(this.media.screen, before);
    this.applyScreenAll();
  }

  /** ffmpeg 인코더 화면 받기 준비(메시 통화만): 데이터 채널은 연결마다 붙이고, 풀 수 있는 코덱을 알립니다. */
  private startNative() {
    if (!this.worker || this.mode === 'simulated') return;
    this.native = new NativeScreen({
      worker: this.worker,
      myDevice: this.o.myDevice,
      onRemoteScreen: (device, stream) => {
        if (!this.snap.watching.includes(device)) return;
        this.remoteVideos.set(device, stream);
        this.emit({});
      },
      isWatching: (device) => this.snap.watching.includes(device),
      onCapsChanged: (device) => this.applyScreenTrack(device),
      onLocalEnded: (reason) => this.onNativeEnded(reason),
    });
  }

  /** 공유 중인 ffmpeg 인코더 설정이 바뀌었으면 새 설정으로 다시 띄웁니다(트랙은 그대로). */
  private restartNative(before: MediaPrefs['screen'], hintBefore: ContentHint = this.snap.contentHint) {
    const sp = this.media.screen;
    if (!this.native || sp.encoder === 'webrtc') return;
    const prevEncoder = before.encoder === 'webrtc' ? sp.encoder : before.encoder;
    const want = nativeSettings(sp, sp.encoder, this.snap.contentHint);
    const had = nativeSettings(before, prevEncoder, hintBefore);
    if (JSON.stringify(want) === JSON.stringify(had) && nativeMaxHeight(sp) === nativeMaxHeight(before)) return;
    void this.native.restart(nativeMaxHeight(sp), want);
  }

  /** ffmpeg 인코더로 보낼 시청자: 내 화면을 보는 메시 상대. */
  private applyNativeViewers() {
    if (!this.native) return;
    const viewers = this.nativeSharing ? this.snap.viewers : [];
    this.native.setViewers(viewers);
  }

  /** Tor 통화의 소리 준비: Opus(WebCodecs)는 바로, 모델이 있으면 Mimi·DAC도 풀 수 있다고 알립니다. */
  private startTor() {
    if (!this.worker) return;
    const neural = this.o.client.transport?.neural;
    this.tor = new TorAudio({
      client: { call: (method, params) => this.o.client.call(method, params) },
      space: this.o.space,
      channel: this.o.channel,
      call: this.o.join.call,
      myDevice: this.o.myDevice,
      worker: this.worker,
      neural,
      onRemoteStream: (device, stream) => this.watchLevel(device, stream),
      onCodec: (codec) => this.emit({ torCodec: codec ?? undefined }),
      onRemoteScreen: (device, stream) => this.onTorScreen(device, stream),
      onError: (message) => this.emit({ micError: message }),
    });
    this.tor.setPreferred(this.media.audio.codec);
    this.tor.setOutput(this.sink, this.devices.outputVolume / 100);
    if (!neural) return;
    void neural
      .status()
      .then((st) => {
        if (this.closed || !this.tor) return;
        const ready = st.codecs.filter((c) => c.missing.length === 0).map((c) => c.codec);
        this.tor.setNeuralReady(ready);
        this.emit({ neuralReady: ready });
      })
      .catch(() => undefined);
  }

  /**
   * 신경망 코덱 준비(데스크톱 앱만): 데이터 채널은 바로 붙이고, 모델 폴더를 확인한 뒤 풀 수 있는 코덱을 알리고 고른 코덱으로
   * 보내기 시작합니다. 모델이 없거나 확인에 실패하면 Opus만 씁니다.
   */
  private startNeural() {
    const transport = this.o.client.transport?.neural;
    if (!transport || !this.worker || this.mode === 'simulated') return;
    this.neural = new NeuralLink({
      transport,
      worker: this.worker,
      myDevice: this.o.myDevice,
      call: this.o.join.call,
      onSending: (device, sending) => this.onNeuralSending(device, sending),
      onRemoteStream: (device, stream) => this.watchLevel(device, stream),
    });
    this.neural.setOutput(this.sink, this.devices.outputVolume / 100);
    void transport
      .status()
      .then((st) => {
        if (this.closed || !this.neural) return;
        const ready = st.codecs.filter((c) => c.missing.length === 0).map((c) => c.codec);
        this.neural.setDecodable(ready);
        this.emit({ neuralReady: ready });
        this.applyNeuralCodec();
      })
      .catch(() => undefined);
  }

  /** 고른 음성 코덱이 신경망 코덱이고 모델이 있으면 그것으로, 아니면 Opus. */
  private applyNeuralCodec() {
    if (!this.neural) return;
    const want = this.media.audio.codec;
    this.neural.setCodec(want !== 'opus' && this.snap.neuralReady?.includes(want) ? want : null);
    this.emitNeural();
  }

  /** 그 상대에게 신경망 코덱으로 보내기 시작하면 Opus 트랙을 비우고, 멈추면 되돌립니다. */
  private onNeuralSending(device: DeviceId, sending: boolean) {
    const sender = this.peers.get(device)?.audioTx?.sender;
    if (sender) {
      const track = sending ? null : this.mic;
      if (sender.track !== track) void sender.replaceTrack(track).catch(() => undefined);
    }
    this.emitNeural();
  }

  private emitNeural() {
    const want = this.media.audio.codec;
    const to = this.neural?.sendingDevices() ?? [];
    const neural = want !== 'opus' && to.length > 0 ? { codec: want, to } : undefined;
    if (JSON.stringify(neural) !== JSON.stringify(this.snap.neural)) this.emit({ neural });
  }

  /** 상대 설명에 내 Opus 설정을 넣습니다(받는 쪽 fmtp를 내 인코더가 따르므로, 이것이 내가 보내는 음성 설정이 됨). */
  private remoteSdp(sdp: string): string {
    return setScreenAudioFmtp(setOpusFmtp(sdp, opusFmtp(this.media.audio)));
  }

  /** 마이크 송신기의 최대 비트레이트(설정이 없으면 브라우저 기본). 바뀐 때만 고칩니다. */
  private tuneAudioSender(sender: RTCRtpSender) {
    const params = sender.getParameters();
    const enc = params.encodings?.[0];
    if (!enc) return;
    const want = this.media.audio.bitrateKbps != null ? this.media.audio.bitrateKbps * 1000 : undefined;
    if (enc.maxBitrate === want) return;
    if (want === undefined) delete enc.maxBitrate;
    else enc.maxBitrate = want;
    void sender.setParameters(params).catch(() => undefined);
  }

  /**
   * 통화 중에 세부 설정을 바꿉니다. 모두 바로 걸립니다: 마이크 처리는 마이크를 다시 열고, Opus의 스테레오·FEC·DTX·패킷
   * 길이는 바로 다시 협상하고, 인코더 갈래(웹뷰 ↔ ffmpeg)를 바꾸면 화면공유를 새 인코더로 다시 시작합니다.
   */
  setMedia(media: MediaPrefs) {
    const before = this.media;
    this.media = media;
    const same = (a: unknown, b: unknown) => JSON.stringify(a) === JSON.stringify(b);
    // 마이크 처리(에코·잡음·자동 음량·스테레오)는 열린 마이크에 applyConstraints로 걸면 웹뷰가 무시해 다시 엽니다.
    if (!same(before.devices.input, media.devices.input) || !same(micConstraints(before.audio), micConstraints(media.audio))) {
      void this.reopenMic();
    } else if (before.devices.inputVolume !== media.devices.inputVolume) {
      this.applyMicVolume();
    }
    if (!same(before.devices.output, media.devices.output)) void this.resolveSink().then(() => this.applyOutput());
    else if (before.devices.outputVolume !== media.devices.outputVolume) this.applyOutput();
    for (const p of this.peers.values()) {
      if (p.audioTx) this.tuneAudioSender(p.audioTx.sender);
      if (p.videoTx && before.screen.codec !== media.screen.codec) preferVideoCodecs(p.videoTx, media.screen.codec);
    }
    if (this.sfu) this.tuneAudioSender(this.sfu.audioTx.sender);
    if (!same(opusFmtp(before.audio), opusFmtp(media.audio))) this.renegotiateAudio();
    this.applyNeuralCodec();
    this.tor?.setPreferred(media.audio.codec);
    const webrtc = (sp: MediaPrefs['screen']) => sp.encoder === 'webrtc';
    if (this.screen && !this.torMode && webrtc(before.screen) !== webrtc(media.screen)) {
      void this.restartShare();
      return;
    }
    // ffmpeg 인코더 공유 중에 인코더 설정이 바뀌면 다시 띄웁니다.
    if (this.nativeSharing) this.restartNative(before.screen);
    else if (this.screen && before.screen.fps !== media.screen.fps) {
      void this.screen.applyConstraints?.({ frameRate: screenFps(media.screen, this.snap.contentHint) }).catch(() => undefined);
    }
    if (this.screen && before.screen.audio !== media.screen.audio) {
      if (media.screen.audio) void this.openScreenAudio();
      else this.closeScreenAudio();
    }
    this.screenAudio?.setVolume(media.screen.audioVolume);
    this.applyScreenAll();
  }

  private applyScreenAll() {
    this.applyNativeViewers();
    for (const d of this.peers.keys()) this.applyScreenTrack(d);
    this.applySfuScreen();
    // Tor 통화: Tor로 잇는 시청자에게만 화면 조각을 보냅니다.
    this.syncTor();
  }

  /** 메시: 시청자에게만 화면 트랙(과 화면 소리)을 붙이고, 나머지에는 비웁니다(인코딩 수 = 시청자 수). */
  private applyScreenTrack(device: DeviceId) {
    const peer = this.peers.get(device);
    if (!peer) return;
    const viewer = !!this.screen && this.snap.viewers.includes(device);
    // 화면 소리는 화면을 데이터 채널로 받는 시청자에게도 WebRTC로 보냅니다(소리는 ffmpeg가 만들지 않음).
    const audioSender = peer.screenAudioTx?.sender;
    const audio = viewer && this.screenAudio ? this.screenAudio.track : null;
    if (audioSender && audioSender.track !== audio) void audioSender.replaceTrack(audio).catch(() => undefined);
    const sender = peer.videoTx?.sender;
    if (!sender) return;
    // ffmpeg 인코더 화면을 데이터 채널로 받는 시청자에게는 WebRTC로 또 보내지 않습니다.
    const viaNative = this.nativeSharing && !!this.native?.receivesNative(device);
    const track = viewer && !viaNative ? this.screen : null;
    if (sender.track !== track) void sender.replaceTrack(track).catch(() => undefined);
    if (track) {
      const hint = this.snap.contentHint;
      const sp = this.media.screen;
      const maxBitrate = sp.bitrateKbps != null ? sp.bitrateKbps * 1000 : SCREEN_BITRATES[hint][2];
      const scale = screenScale(sp, this.screen?.getSettings?.().height);
      tuneScreenSender(
        sender,
        screenDegradation(sp, hint),
        () => ({ maxBitrate, maxFramerate: screenFps(sp, hint), scaleResolutionDownBy: scale }),
        pickSendCodec(sender.getParameters().codecs, sp.codec),
      );
    }
  }

  /** SFU: 보는 사람이 하나라도 있을 때만 호스트에 화면을 올리고(업로드 절약), 층별 비트레이트를 정합니다. */
  private applySfuScreen() {
    const sender = this.sfu?.screenTx.sender;
    if (!sender) return;
    const track = this.screen && this.snap.viewers.length > 0 ? this.screen : null;
    if (sender.track !== track) void sender.replaceTrack(track).catch(() => undefined);
    if (!track) return;
    const hint = this.snap.contentHint;
    const sp = this.media.screen;
    const top = sp.bitrateKbps != null ? sp.bitrateKbps * 1000 : SCREEN_BITRATES[hint][2];
    // 사용자가 정한 최대 비트레이트는 가장 높은 층에, 아래 층은 기본 비율대로 나눕니다.
    const rates = SCREEN_BITRATES[hint].map((r) => Math.round((r / SCREEN_BITRATES[hint][2]) * top));
    const fps = screenFps(sp, hint);
    const base = screenScale(sp, this.screen?.getSettings?.().height);
    const layerScale = [4, 2, 1];
    tuneScreenSender(sender, screenDegradation(sp, hint), (i) => ({
      maxBitrate: rates[Math.min(i, rates.length - 1)],
      // 가장 낮은 층은 움직임이 많아도 30fps로 묶어 느린 회선의 시청자를 지킵니다.
      maxFramerate: i === 0 ? Math.min(30, fps) : fps,
      scaleResolutionDownBy: (layerScale[i] ?? 1) * base,
    }));
  }

  /** 원격 화면 '보기' 켜고 끄기. 누르기 전에는 영상을 받지 않습니다. */
  async watch(device: DeviceId, watching: boolean) {
    if (this.torMode) return this.watchTor(device, watching);
    const set = new Set(this.snap.watching);
    if (watching) set.add(device);
    else set.delete(device);
    if (!watching) {
      this.remoteVideos.delete(device);
      this.native?.forget(device);
    } else if (this.mode === 'simulated') this.remoteVideos.set(device, createFakeScreenStream(device));
    else {
      const v = this.native?.remoteScreen(device) ?? this.sfu?.screens.get(device) ?? this.peers.get(device)?.remoteVideo;
      if (v) this.remoteVideos.set(device, v);
    }
    this.emit({ watching: [...set] });
    // SFU: 호스트에게 이 화면을 받을지(층) 알립니다. 공유하는 쪽은 아래 voice.watch로 시청자가 생긴 것을 압니다.
    if (this.sfu) void this.sendSfu(this.sfu, this.selectPayload(device, watching));
    await this.o.client
      .call('voice.watch', { space: this.o.space, channel: this.o.channel, device, watching })
      .catch(() => undefined);
    if (this.snap.topology === 'mesh') await this.signal(device, { type: 'watch_screen', watching });
  }

  /** Tor 통화: 데몬의 voice.watch가 공유하는 쪽에 알려 그쪽이 조각을 보내기 시작합니다(키 프레임부터). */
  private async watchTor(device: DeviceId, watching: boolean) {
    const set = new Set(this.snap.watching);
    if (watching) set.add(device);
    else set.delete(device);
    if (!watching) {
      this.remoteVideos.delete(device);
      this.tor?.screen.forget(device);
    } else if (this.mode === 'simulated') this.remoteVideos.set(device, createFakeScreenStream(device));
    else {
      // WebRTC로 이어진(서로 신뢰한) 상대의 화면은 메시처럼 그 연결로 옵니다.
      const v = this.viaTor().includes(device)
        ? this.tor?.screen.remoteScreen(device)
        : (this.native?.remoteScreen(device) ?? this.peers.get(device)?.remoteVideo);
      if (v) this.remoteVideos.set(device, v);
    }
    this.emit({ watching: [...set] });
    await this.o.client
      .call('voice.watch', { space: this.o.space, channel: this.o.channel, device, watching })
      .catch(() => undefined);
  }

  /** Tor 통화에서 받은 화면이 처음 그려졌을 때. */
  private onTorScreen(device: DeviceId, stream: MediaStream) {
    if (!this.snap.watching.includes(device)) return;
    this.remoteVideos.set(device, stream);
    this.emit({});
  }

  /** 보고 있는 화면의 화질 층을 고릅니다(SFU에서만 효과, 메시는 한 가지 화질). */
  setScreenLayer(device: DeviceId, layer: ScreenLayer) {
    this.emit({ layers: { ...this.snap.layers, [device]: layer } });
    if (this.sfu && this.snap.watching.includes(device)) void this.sendSfu(this.sfu, this.selectPayload(device, true));
  }

  // ───────────── 말하기 표시 ─────────────

  private watchLevel(device: DeviceId, stream: MediaStream) {
    try {
      this.audioCtx ??= new AudioContext();
      const src = this.audioCtx.createMediaStreamSource(stream);
      const an = this.audioCtx.createAnalyser();
      an.fftSize = 512;
      src.connect(an);
      this.analysers.set(device, an);
    } catch {
      // 오디오 분석을 못 해도 통화는 됩니다.
    }
  }

  private readonly buf = new Float32Array(512);
  private tick = 0;

  private sampleLevels() {
    const speaking: Record<string, boolean> = {};
    for (const [device, an] of this.analysers) {
      an.getFloatTimeDomainData(this.buf);
      let sum = 0;
      for (const v of this.buf) sum += v * v;
      const rms = Math.sqrt(sum / this.buf.length);
      speaking[device] = rms > SPEAKING_RMS && !(device === this.o.myDevice && this.snap.muted);
    }
    if (this.mode === 'simulated') {
      // 흉내: 음소거하지 않은 첫 원격 참가자가 주기적으로 말함.
      this.tick++;
      const talker = this.snap.participants.find((p) => p.device !== this.o.myDevice && !p.muted);
      if (talker) speaking[talker.device] = this.tick % 20 < 13;
    }
    const prev = this.snap.speaking;
    const changed =
      Object.keys(speaking).length !== Object.keys(prev).length || Object.entries(speaking).some(([k, v]) => prev[k] !== v);
    if (changed) this.emit({ speaking });
  }

  // ───────────── 종료 ─────────────

  async leave() {
    if (this.closed) return;
    this.closed = true;
    for (const d of [...this.peers.keys()]) this.closePeer(d);
    const link = this.sfu;
    // 호스트가 세션을 바로 비우게 알립니다(못 닿아도 데몬의 나가기로 정리됨).
    if (link) void this.sendSfu(link, { type: 'sfu_close', session: link.session }, true);
    this.closeSfu();
    this.clearSfuRetry();
    await this.neural?.close();
    this.neural = null;
    this.native?.close();
    this.native = null;
    await this.tor?.close();
    this.tor = null;
    this.mic?.stop();
    this.micGain?.close();
    this.rawMic?.stop();
    this.screenAudio?.stop();
    this.screenAudio = null;
    this.screen?.stop();
    this.mic = null;
    this.micGain = null;
    this.rawMic = null;
    this.screen = null;
    if (this.levelTimer) clearInterval(this.levelTimer);
    await this.audioCtx?.close().catch(() => undefined);
    if (this.worker) {
      this.worker.postMessage({ type: 'clearKeys' });
      this.worker.terminate();
      this.worker = null;
    }
    this.remoteVideos.clear();
    this.pendingIce.clear();
    await this.o.client.call('voice.leave', { space: this.o.space, channel: this.o.channel }).catch(() => undefined);
  }
}
