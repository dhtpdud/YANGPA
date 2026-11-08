// 전송 계층에 무관한 API 클라이언트.
//
// - Tauri 안: invoke('rpc_call', {method, params})로 호출하고 'yangpa-events'로 알림 묶음을 받습니다.
// - 일반 브라우저(개발): 메모리 안 MOCK 백엔드(api/mock.ts)로 대신합니다.

import { lookup, t } from '../i18n';
import { isAndroid } from '../lib/platform';
import type { NativeEncodeSettings, NativeEncoderId } from '../voice/media';
import { ErrorCode, type MethodName, type Notification, type ParamsOf, type ResultOf } from './types';

export class RpcError extends Error {
  readonly code: number;
  readonly data: unknown;
  constructor(code: number, message: string, data?: unknown) {
    super(message);
    this.name = 'RpcError';
    this.code = code;
    this.data = data;
  }

  /** 1006(사람 확인 대기)이면 확인 ID. */
  get confirmationId(): string | undefined {
    const d = this.data as { confirmation_id?: unknown } | undefined;
    return typeof d?.confirmation_id === 'string' ? d.confirmation_id : undefined;
  }
}

/** 알 수 없는 오류 값을 RpcError로 바꿉니다(Tauri invoke는 직렬화된 객체로 거부합니다). 데몬·앱 셸이 보낸 한국어 문구는 화면 언어로. */
export function toRpcError(e: unknown): RpcError {
  if (e instanceof RpcError) return e;
  if (e && typeof e === 'object' && 'code' in e && 'message' in e) {
    const o = e as { code: unknown; message: unknown; data?: unknown };
    if (typeof o.code === 'number' && typeof o.message === 'string') return new RpcError(o.code, localizeError(o.code, o.message), o.data);
  }
  if (typeof e === 'string') return new RpcError(ErrorCode.Internal, localizeError(ErrorCode.Internal, e));
  if (e instanceof Error) return new RpcError(ErrorCode.Internal, e.message);
  return new RpcError(ErrorCode.Internal, t('알 수 없는 오류'));
}

/** 카탈로그에 있는 문구는 번역, 없으면(값이 섞인 문구 등) 오류 종류별 일반 문구. 한국어 화면은 원문 그대로. */
function localizeError(code: number, message: string): string {
  const known = lookup(message);
  if (known !== undefined) return known;
  switch (code) {
    case ErrorCode.DaemonUnavailable:
      return t('데몬에 연결되지 않았습니다');
    case ErrorCode.AuthRequired:
      return t('인증이 필요합니다');
    case ErrorCode.OutOfScope:
      return t('이 토큰으로는 할 수 없는 일입니다');
    case ErrorCode.Forbidden:
      return t('권한이 없습니다');
    case ErrorCode.NotFound:
      return t('찾을 수 없습니다');
    case ErrorCode.InvalidParams:
    case ErrorCode.InvalidInput:
      return t('입력이 올바르지 않습니다');
    case ErrorCode.ConfirmationRequired:
      return t('사람 확인이 필요합니다');
    case ErrorCode.VolatileReadDenied:
      return t('휘발성 내용은 읽을 수 없습니다');
    case ErrorCode.NoIdentity:
      return t('아직 계정이 없습니다');
    case ErrorCode.Network:
      return t('네트워크 오류가 났습니다');
    case ErrorCode.Conflict:
      return t('다른 변경과 겹쳤습니다. 다시 시도해 주세요');
    default:
      return t('오류가 났습니다');
  }
}

export type DaemonState =
  | { state: 'connecting' }
  | { state: 'spawning' }
  | { state: 'connected'; version?: string }
  | { state: 'error'; message: string };

export interface PickedFile {
  /** 디스크 경로(Tauri). 브라우저 MOCK에서는 가짜 경로. */
  path: string;
  name: string;
  size?: number;
  /** 클립보드에서 붙여넣어 데몬 RAM에 올린 것(blob.paste ID). 이때 `path`는 화면용 키일 뿐입니다. */
  paste?: string;
}

/** 보내기 전 동영상·음성 미리보기(로컬 파일을 웹뷰에서 읽는 임시 주소). 다 쓰면 `close`. */
export interface LocalPreview {
  url: string;
  mime: string;
  size: number;
  close(): void;
}

/** 캡처·녹화 감지(src-tauri/src/capture.rs의 `CaptureEvent`). */
export type CaptureEvent =
  | { kind: 'screenshot'; tool: string | null }
  /** 켜져 있는 녹화 프로그램. 빈 목록이면 모두 꺼짐. */
  | { kind: 'recording'; tools: string[] };

/** 웹뷰가 받으려다 취소한 다운로드(lib.rs `on_download`). `path`는 웹뷰가 이미 정한 저장 경로(Windows: 저장 창에서 고른 곳 또는
 * 받기 폴더). 없으면 앱이 저장 창을 띄웁니다. */
export interface SaveRequest {
  url: string;
  path?: string | null;
}

export interface Transport {
  readonly kind: 'tauri' | 'mock' | 'node';
  call(method: string, params: unknown): Promise<unknown>;
  /** 알림 묶음 구독. 해제 함수를 돌려줍니다. */
  subscribe(handler: (batch: Notification[]) => void): () => void;
  onDaemonState(handler: (s: DaemonState) => void): () => void;
  /** 다시 붙기 대기를 건너뛰고 바로 연결을 시도합니다. */
  retryDaemon(): Promise<void>;
  /** 정상 등급 블롭을 <img>/<video>에 바로 쓸 수 있는 URL(캐시 금지 커스텀 프로토콜). */
  blobSrc(space: string, blob: string, keyFrom: string, mime: string): string;
  /** 블롭 바이트를 메모리로 읽습니다(휘발성 채널: blob: URL로 렌더링하기 위해). */
  readBlob(space: string, blob: string, keyFrom: string, mime: string): Promise<Uint8Array>;
  pickFiles(opts: { multiple: boolean; images?: boolean }): Promise<PickedFile[]>;
  /** 저장 위치 고르기. 취소하면 null. */
  saveDialog(defaultName: string): Promise<string | null>;
  /** 창 캡처 제외(화면 녹화·스크린샷에서 창 내용 숨김). 지원 여부를 돌려줍니다. */
  setContentProtection(enabled: boolean): Promise<boolean>;
  /** 휘발성 채널을 보는 동안 캡처·녹화 감지(윈도). 해제 함수를 돌려줍니다. 지원하지 않는 전송에는 없습니다. */
  watchCapture?(handler: (e: CaptureEvent) => void): () => void;
  /** 웹뷰가 받으려다 셸이 취소한 다운로드의 주소(앱의 저장으로 잇게). 데스크톱 셸에만 있습니다. */
  watchSaveRequests?(handler: (req: SaveRequest) => void): () => void;
  openExternal(url: string): Promise<void>;
  loadPrefs(): Promise<Record<string, unknown>>;
  savePrefs(prefs: Record<string, unknown>): Promise<void>;
  /** 파일 끌어 놓기(경로). 해제 함수를 돌려줍니다. */
  onFileDrop(handler: (paths: string[]) => void): () => void;
  /**
   * 고른 동영상·음성 파일을 웹뷰에서 읽을 수 있는 임시 주소로 엽니다(썸네일·길이 읽기용). 동영상·음성이 아니거나
   * 지원하지 않는 전송이면 null.
   */
  openLocalPreview?(path: string): Promise<LocalPreview | null>;
  /** 지금 연 채널을 OS 알림 쪽에 알립니다(창이 앞에 있는 동안 그 채널은 알리지 않음). 홈이면 둘 다 null. */
  setNotifyViewing?(space: string | null, channel: string | null): void;
  /** OS 알림을 눌렀을 때 열 채널. 해제 함수를 돌려줍니다. */
  onOpenChannel?(handler: (t: { space: string; channel: string }) => void): () => void;
  /** 웹 초대 페이지가 연 `yangpa://` 링크(앱이 꺼져 있을 때 받은 것도). 해제 함수를 돌려줍니다. 데스크톱 셸에만 있습니다. */
  onOpenLink?(handler: (url: string) => void): () => void;
  /** 통화의 신경망 음성 코덱(Mimi·DAC, 데스크톱 앱만). 없으면 Opus만 씁니다. */
  neural?: NeuralTransport;
  /** 화면공유 ffmpeg 인코더(데스크톱 앱만). 없으면 웹뷰(WebRTC) 인코더만 씁니다. */
  screen?: ScreenTransport;
  /** 비밀값 복사(데스크톱 앱만, src-tauri/src/secret.rs). */
  secret?: SecretTransport;
  /** 데이터 폴더(데스크톱 앱만, 바꾸기는 윈도 설치판·포터블). */
  dataDir?: DataDirTransport;
  /** `saveDialog`가 준 경로에 데몬이 다 쓴 뒤 부릅니다(Android: 임시 파일을 저장 창이 고른 곳으로 옮김). */
  finishSave?(path: string): Promise<void>;
  /** 통화 중인지(Android: 앱을 내려도 마이크가 끊기지 않게 서비스 종류를 바꿈). */
  setCallActive?(active: boolean): void;
  /** Android 뒤로 가기 단추. 해제 함수를 돌려줍니다. */
  onBackButton?(handler: () => void): () => void;
  /** 앱을 뒤로 내립니다(Android: 첫 화면에서 뒤로 가기). */
  minimize?(): void;
}

export interface SecretTransport {
  /** 클립보드 기록에서 빼고 넣은 뒤 잠시 후 비웁니다. 이 OS에서 못 하면 false(웹뷰가 직접 복사). */
  copy(text: string): Promise<boolean>;
}

export type DataDirState = 'missing' | 'empty' | 'yangpa' | 'other';
export interface DataUsage {
  bytes: number;
  files: number;
}
/** 데이터 폴더 옮기기 계획(src-tauri/src/datadir.rs의 `Plan`). */
export interface DataMovePlan {
  from: string;
  to: string;
  usage: DataUsage;
  same_volume: boolean;
  estimate_secs: number;
  /** 새 폴더 상태: missing·empty면 옮길 수 있고, yangpa면 옮기지 않고 그 데이터를 씀. */
  target: DataDirState;
}
export interface DataDirInfo {
  /** 윈도 데스크톱 앱(설치판·포터블)이면 참(폴더를 바꿀 수 있음). */
  changeable: boolean;
  dir: string;
  /** 기본 폴더(실행 파일 옆 Data, 앱 폴더에 쓸 수 없으면 OS 기본 경로). 예전 판을 쓰던 사람은 예전 위치를 그대로 써서 `dir`와 다를 수 있습니다. */
  default_dir: string | null;
  usage: DataUsage;
}
export interface DataDirTransport {
  info(): Promise<DataDirInfo>;
  pickFolder(): Promise<string | null>;
  plan(target: string): Promise<DataMovePlan>;
  /** 성공하면 앱이 다시 시작합니다. 실패하면 데몬이 꺼진 채라 `restart`를 권합니다. */
  change(target: string, mode: 'move' | 'switch'): Promise<void>;
  restart(): Promise<void>;
  /** 옮기는 동안 진행(바이트). 해제 함수를 돌려줍니다. */
  onProgress(handler: (p: { done: number; total: number }) => void): () => void;
}

export interface ScreenEncoderStatus {
  id: NativeEncoderId;
  available: boolean;
  /** 쓸 수 없는 까닭. */
  error?: string;
}

export interface ScreenStatus {
  /** ffmpeg 폴더 경로. */
  dir: string;
  /** 빠졌거나 배포본과 다른 파일. 비었으면 쓸 수 있음. */
  missing: string[];
  encoders: ScreenEncoderStatus[];
}

/** 공유할 수 있는 모니터·창(src-tauri/src/screen/sources.rs). */
export interface ScreenSource {
  kind: 'monitor' | 'window';
  id: string;
  title: string;
  width: number;
  height: number;
  primary: boolean;
  /** 미리보기(JPEG)·창 아이콘(PNG) data URL. 못 찍었으면 없음. */
  thumbnail?: string;
  icon?: string;
}

/**
 * 화면 캡처·인코딩(앱의 Rust 쪽 ffmpeg). 메시지는 src-tauri/src/screen/mod.rs의 바이트 형식(설정 레코드·프레임·끝).
 */
export interface ScreenTransport {
  status(): Promise<ScreenStatus>;
  sources(): Promise<ScreenSource[]>;
  /** 시작하고 공유 번호를 돌려줍니다. 이미 공유 중이면 그것을 멈추고 새로 띄웁니다. */
  start(
    source: Pick<ScreenSource, 'kind' | 'id'>,
    maxHeight: number | null,
    settings: NativeEncodeSettings,
    onData: (msg: Uint8Array) => void,
  ): Promise<number>;
  stop(id?: number): Promise<void>;
  /**
   * 화면 소리(윈도 WASAPI 루프백, src-tauri/src/screen/audio.rs). `window`는 공유하는 창의 핸들(그 프로그램 소리만), 없으면 이
   * 앱을 뺀 시스템 소리. 메시지는 `[1][48kHz 스테레오 f32 PCM]`, `[3][끝난 까닭]`.
   */
  audioStart?(window: string | null, onData: (msg: Uint8Array) => void): Promise<number>;
  audioStop?(id?: number): Promise<void>;
}

export type NeuralCodecId = 'mimi' | 'dac';

export interface NeuralStatus {
  /** 모델 폴더 경로. */
  dir: string;
  /** 코덱마다 빠졌거나 배포본과 다른 파일(모델 폴더 기준). 비었으면 쓸 수 있음. */
  codecs: Array<{ codec: NeuralCodecId; missing: string[] }>;
}

/** 신경망 코덱 계산(앱의 Rust 쪽 ONNX Runtime). 소리는 24kHz 모노 f32, 코드는 u16. */
export interface NeuralTransport {
  status(): Promise<NeuralStatus>;
  /** 소리를 넣고 다 찬 패킷만큼 코드를 받습니다(Mimi 프레임 8개씩, DAC 덩어리 384개씩). */
  encode(stream: string, codec: NeuralCodecId, pcm: Float32Array): Promise<Uint16Array>;
  /** 패킷 하나를 풉니다. `lost`는 이 패킷 앞에서 빠진 패킷 수. */
  decode(stream: string, codec: NeuralCodecId, codes: Uint16Array, lost: number): Promise<Float32Array>;
  close(stream: string): Promise<void>;
}

export class ApiClient {
  readonly transport: Transport;

  constructor(transport: Transport) {
    this.transport = transport;
  }

  get kind() {
    return this.transport.kind;
  }

  async call<M extends MethodName>(method: M, params: ParamsOf<M>): Promise<ResultOf<M>> {
    try {
      return (await this.transport.call(method, params ?? {})) as ResultOf<M>;
    } catch (e) {
      throw toRpcError(e);
    }
  }

  subscribe(handler: (batch: Notification[]) => void) {
    return this.transport.subscribe(handler);
  }
}

/** 문자열 메서드 이름을 그대로 넘기는 저수준 호출(설정 화면의 동적 호출용). */
export async function callRaw(client: ApiClient, method: string, params: unknown) {
  try {
    return await client.transport.call(method, params);
  } catch (e) {
    throw toRpcError(e);
  }
}

// ───────────── Tauri 전송 ─────────────

function isTauriRuntime(): boolean {
  return typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window;
}

/** 블롭 MIME 중 <img>/<video>로 직접 렌더링해도 되는 것만(나머지는 다운로드로). */
const RENDERABLE = /^(image\/(png|jpeg|gif|webp|avif)|video\/(mp4|webm)|audio\/(ogg|mpeg|aac|webm|mp4|wav|opus))$/;

export function isRenderableMime(mime: string): boolean {
  return RENDERABLE.test(mime);
}

/** 데스크톱 셸 전송. 시험이 셸 모듈을 가짜로 바꿔 부를 수 있게 내보냅니다. */
export async function createTauriTransport(): Promise<Transport> {
  const core = await import('@tauri-apps/api/core');
  const event = await import('@tauri-apps/api/event');
  // 커스텀 프로토콜 기준 주소: 리눅스·macOS는 yangpa-blob://localhost/, 윈도는 http://yangpa-blob.localhost/
  const blobBase = core.convertFileSrc('', 'yangpa-blob');

  const blobPath = (space: string, blob: string, keyFrom: string, mime: string) =>
    `${blobBase}blob/${encodeURIComponent(space)}/${encodeURIComponent(blob)}/${encodeURIComponent(keyFrom)}?mime=${encodeURIComponent(mime)}`;

  // 캡처 감지를 지켜보는 화면 수(채널 화면·그 위의 콘텐츠 뷰어). 셸의 감지는 하나뿐이라 마지막 하나가 그만둘 때만 끕니다.
  let captureWatchers = 0;

  // 웹뷰의 요소 전체화면은 창 안만 채우므로(wry가 창을 키우지 않음) 창도 함께 전체화면으로 바꿉니다(화면공유 보기).
  document.addEventListener('fullscreenchange', () => {
    void import('@tauri-apps/api/window')
      .then(({ getCurrentWindow }) => getCurrentWindow().setFullscreen(!!document.fullscreenElement))
      .catch(() => undefined);
  });

  const transport: Transport = {
    kind: 'tauri',
    call: (method, params) => core.invoke('rpc_call', { method, params }),
    subscribe(handler) {
      let unlisten: (() => void) | undefined;
      let cancelled = false;
      void event
        .listen<Notification[]>('yangpa-events', (e) => {
          if (Array.isArray(e.payload)) handler(e.payload);
        })
        .then((u) => {
          if (cancelled) u();
          else unlisten = u;
        });
      return () => {
        cancelled = true;
        unlisten?.();
      };
    },
    onDaemonState(handler) {
      let unlisten: (() => void) | undefined;
      let cancelled = false;
      void core.invoke<DaemonState>('daemon_status').then(handler, () => undefined);
      void event
        .listen<DaemonState>('yangpa-daemon', (e) => handler(e.payload))
        .then((u) => {
          if (cancelled) u();
          else unlisten = u;
        });
      return () => {
        cancelled = true;
        unlisten?.();
      };
    },
    retryDaemon: () => core.invoke('daemon_retry'),
    blobSrc: blobPath,
    async readBlob(space, blob, keyFrom, mime) {
      // 디스크 캐시를 거치지 않게 no-store로 읽고, 호출한 쪽이 blob: URL로 바꿔 씁니다.
      const res = await fetch(blobPath(space, blob, keyFrom, mime), { cache: 'no-store', credentials: 'omit' });
      if (!res.ok) throw new RpcError(ErrorCode.NotFound, t('블롭을 읽지 못했습니다({status})', { status: res.status }));
      return new Uint8Array(await res.arrayBuffer());
    },
    async pickFiles({ multiple, images }) {
      const dialog = await import('@tauri-apps/plugin-dialog');
      const picked = await dialog.open({
        multiple,
        directory: false,
        filters: images ? [{ name: t('이미지'), extensions: ['png', 'gif', 'webp'] }] : undefined,
      });
      if (!picked) return [];
      const list = Array.isArray(picked) ? picked : [picked];
      return list.map((p) => ({ path: p, name: p.split(/[\\/]/).pop() ?? p }));
    },
    async saveDialog(defaultName) {
      const dialog = await import('@tauri-apps/plugin-dialog');
      const path = await dialog.save({ defaultPath: defaultName.replace(/[\\/:*?"<>|]/g, '_').slice(0, 120) });
      return path ?? null;
    },
    setContentProtection: (enabled) => core.invoke<boolean>('set_content_protection', { enabled }),
    watchCapture(handler) {
      let unlisten: (() => void) | undefined;
      let cancelled = false;
      captureWatchers += 1;
      void event
        .listen<CaptureEvent>('yangpa-capture', (e) => handler(e.payload))
        .then((u) => {
          if (cancelled) u();
          else unlisten = u;
          // 이벤트를 받을 준비가 된 뒤에 켭니다(켜자마자 보내는 "이미 녹화 중"을 놓치지 않게). 이미 켜져 있어도 다시 켜면
          // 감지를 새로 시작해 지금 상태를 모든 화면에 다시 보냅니다.
          if (!cancelled) void core.invoke('set_capture_watch', { enabled: true }).catch(() => undefined);
        });
      return () => {
        if (cancelled) return;
        cancelled = true;
        unlisten?.();
        captureWatchers -= 1;
        if (captureWatchers === 0) void core.invoke('set_capture_watch', { enabled: false }).catch(() => undefined);
      };
    },
    watchSaveRequests(handler) {
      let unlisten: (() => void) | undefined;
      let cancelled = false;
      void event
        .listen<SaveRequest>('yangpa-save-request', (e) => handler(e.payload))
        .then((u) => {
          if (cancelled) u();
          else unlisten = u;
        });
      return () => {
        cancelled = true;
        unlisten?.();
      };
    },
    openExternal: (url) => core.invoke('open_external', { url }),
    setNotifyViewing(space, channel) {
      void core.invoke('notify_set_viewing', { space, channel }).catch(() => undefined);
    },
    onOpenChannel(handler) {
      let unlisten: (() => void) | undefined;
      let cancelled = false;
      void event
        .listen<{ space?: unknown; channel?: unknown }>('yangpa-open-channel', (e) => {
          const { space, channel } = e.payload ?? {};
          if (typeof space === 'string' && typeof channel === 'string') handler({ space, channel });
        })
        .then((u) => {
          if (cancelled) u();
          else unlisten = u;
        });
      return () => {
        cancelled = true;
        unlisten?.();
      };
    },
    onOpenLink(handler) {
      const take = () =>
        void core
          .invoke<string | null>('deep_link_take')
          .then((url) => typeof url === 'string' && handler(url))
          .catch(() => undefined);
      let unlisten: (() => void) | undefined;
      let cancelled = false;
      // 듣기 시작한 뒤에 한 번 가져와, 창이 뜨기 전(앱 실행 인자)에 온 링크도 놓치지 않습니다.
      void event.listen('yangpa-open-link', take).then((u) => {
        if (cancelled) u();
        else {
          unlisten = u;
          take();
        }
      });
      return () => {
        cancelled = true;
        unlisten?.();
      };
    },
    neural: {
      status: () => core.invoke<NeuralStatus>('neural_status'),
      async encode(stream, codec, pcm) {
        const body = new Uint8Array(pcm.buffer, pcm.byteOffset, pcm.byteLength).slice();
        const out = await core.invoke<ArrayBuffer>('neural_encode', body, { headers: { 'x-stream': stream, 'x-codec': codec } });
        return new Uint16Array(out);
      },
      async decode(stream, codec, codes, lost) {
        const body = new Uint8Array(codes.buffer, codes.byteOffset, codes.byteLength).slice();
        const out = await core.invoke<ArrayBuffer>('neural_decode', body, {
          headers: { 'x-stream': stream, 'x-codec': codec, 'x-lost': String(lost) },
        });
        return new Float32Array(out);
      },
      close: (stream) => core.invoke('neural_close', { stream }),
    },
    dataDir: {
      info: () => core.invoke<DataDirInfo>('data_dir_info'),
      async pickFolder() {
        const dialog = await import('@tauri-apps/plugin-dialog');
        const picked = await dialog.open({ directory: true, multiple: false });
        return typeof picked === 'string' ? picked : null;
      },
      plan: (target) => core.invoke<DataMovePlan>('data_dir_plan', { target }),
      change: (target, mode) => core.invoke('data_dir_change', { target, mode }),
      restart: () => core.invoke('app_restart'),
      onProgress(handler) {
        let unlisten: (() => void) | undefined;
        let cancelled = false;
        void event
          .listen<{ done: number; total: number }>('yangpa-data-move', (e) => handler(e.payload))
          .then((u) => {
            if (cancelled) u();
            else unlisten = u;
          });
        return () => {
          cancelled = true;
          unlisten?.();
        };
      },
    },
    screen: {
      status: () => core.invoke<ScreenStatus>('screen_status'),
      sources: () => core.invoke<ScreenSource[]>('screen_sources'),
      start(source, maxHeight, settings, onData) {
        const ch = new core.Channel<ArrayBuffer>();
        ch.onmessage = (m) => {
          if (m instanceof ArrayBuffer) onData(new Uint8Array(m));
        };
        return core.invoke<number>('screen_start', { sourceKind: source.kind, sourceId: source.id, maxHeight, settings, onData: ch });
      },
      stop: (id) => core.invoke('screen_stop', { id: id ?? null }),
      audioStart(window, onData) {
        const ch = new core.Channel<ArrayBuffer>();
        ch.onmessage = (m) => {
          if (m instanceof ArrayBuffer) onData(new Uint8Array(m));
        };
        return core.invoke<number>('screen_audio_start', { window, onData: ch });
      },
      audioStop: (id) => core.invoke('screen_audio_stop', { id: id ?? null }),
    },
    secret: {
      copy: (text) => core.invoke<boolean>('secret_copy', { text }),
    },
    loadPrefs: () => core.invoke<Record<string, unknown>>('ui_prefs_get'),
    savePrefs: (prefs) => core.invoke('ui_prefs_set', { prefs }),
    async openLocalPreview(path) {
      try {
        const r = await core.invoke<{ token: string; mime: string; size: number }>('local_preview_open', { path });
        return {
          url: `${blobBase}local/${encodeURIComponent(r.token)}`,
          mime: r.mime,
          size: r.size,
          close: () => void core.invoke('local_preview_close', { token: r.token }).catch(() => undefined),
        };
      } catch {
        return null;
      }
    },
    onFileDrop(handler) {
      let unlisten: (() => void) | undefined;
      let cancelled = false;
      void import('@tauri-apps/api/webview').then(({ getCurrentWebview }) =>
        getCurrentWebview()
          .onDragDropEvent((e) => {
            if (e.payload.type === 'drop') handler(e.payload.paths);
          })
          .then((u) => {
            if (cancelled) u();
            else unlisten = u;
          }),
      );
      return () => {
        cancelled = true;
        unlisten?.();
      };
    },
  };
  return isAndroid() ? androidTransport(transport, core) : transport;
}

/** Android: 데스크톱 전용 기능을 빼고, 파일 창이 주는 `content://`를 데몬이 쓰는 앱 폴더 경로와 잇습니다. */
function androidTransport(base: Transport, core: typeof import('@tauri-apps/api/core')): Transport {
  // 임시 파일 경로 → 저장 창이 고른 주소.
  const saving = new Map<string, string>();
  // 고른 파일의 앱 캐시 복사본(평문). 데몬이 받아 간 뒤 바로 지웁니다.
  const imported = new Set<string>();
  return {
    ...base,
    async call(method, params) {
      const result = await base.call(method, params);
      if (imported.size > 0) {
        const sent = JSON.stringify(params);
        for (const path of imported) {
          if (!sent.includes(JSON.stringify(path).slice(1, -1))) continue;
          imported.delete(path);
          void core.invoke('file_release', { path }).catch(() => undefined);
        }
      }
      return result;
    },
    neural: undefined,
    screen: undefined,
    dataDir: undefined,
    onFileDrop: () => () => undefined,
    async pickFiles(opts) {
      const list = await base.pickFiles(opts);
      return Promise.all(
        list.map(async ({ path }) => {
          const local = await core.invoke<string>('file_import', { uri: path });
          imported.add(local);
          return { path: local, name: local.split('/').pop() ?? local };
        }),
      );
    },
    async saveDialog(defaultName) {
      const uri = await base.saveDialog(defaultName);
      if (!uri) return null;
      const { temp_dir: dir } = await core.invoke<{ temp_dir: string }>('app_info');
      const path = `${dir}/export-${crypto.randomUUID()}`;
      saving.set(path, uri);
      return path;
    },
    async finishSave(path) {
      const uri = saving.get(path);
      if (!uri) return;
      saving.delete(path);
      await core.invoke('file_export', { path, uri });
    },
    setCallActive: (on) => void core.invoke('call_active', { on }).catch(() => undefined),
    onBackButton(handler) {
      let listener: { unregister(): Promise<void> } | undefined;
      let cancelled = false;
      void core.addPluginListener('app', 'back-button', handler).then((l) => {
        if (cancelled) void l.unregister();
        else listener = l;
      });
      return () => {
        cancelled = true;
        void listener?.unregister();
      };
    },
    minimize: () => void core.invoke('app_minimize').catch(() => undefined),
  };
}

let clientPromise: Promise<ApiClient> | null = null;

/** 실행 환경에 맞는 클라이언트(한 번만 만듭니다). */
export function getClient(): Promise<ApiClient> {
  if (!clientPromise) {
    clientPromise = (async () => {
      if (isTauriRuntime()) return new ApiClient(await createTauriTransport());
      // 배포 번들에는 MOCK 백엔드를 싣지 않습니다(전체 API·프로토콜 표면이 읽기 쉬운 형태로 실리지 않게 — 리버스
      // 엔지니어링 비용을 높임). 배포본은 늘 Tauri 안에서 돌아 이 경로에 닿지 않습니다. 개발·스크린샷 빌드에는 그대로
      // 들어가 데몬 없이 화면을 띄울 수 있습니다. __YANGPA_SHIPPED__는 번들러가 컴파일 때 치환하고,
      // 치환되지 않는 곳(테스트)에서는 기본값(배포 아님)이 됩니다.
      const shipped = typeof __YANGPA_SHIPPED__ !== 'undefined' && __YANGPA_SHIPPED__;
      if (!shipped) {
        const { createMockTransport } = await import('./mock');
        return new ApiClient(createMockTransport());
      }
      throw new RpcError(ErrorCode.Internal, t('MOCK 백엔드는 배포 빌드에 들어 있지 않습니다'));
    })();
  }
  return clientPromise;
}
