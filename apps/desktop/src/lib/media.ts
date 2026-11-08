// 동영상·음성 첨부: 보내기 전에 웹뷰의 <video>·<audio>로 길이·크기를 읽고 장면 하나를 JPEG 썸네일로 만듭니다.
// 데몬은 영상 디코더가 없어 이 값을 받되 다시 검사하고 썸네일을 JPEG로 다시 만듭니다(MP4·MOV 길이는 데몬이 직접 읽은 값이 우선).
// 여기서는 데몬 한도(64KiB·긴 변 640px)에 맞추기만 하고, 읽지 못하면 힌트 없이 보냅니다(보내기를 막지 않음).
// 파일은 Tauri의 임시 미리보기 주소(transport.openLocalPreview)로 읽고, 다 쓰면 바로 닫습니다. 캔버스·요소는 메모리에만 있습니다.

import type { Transport } from '../api/client';

export interface MediaHints {
  thumbnail_base64?: string;
  width?: number;
  height?: number;
  duration_ms?: number;
}

export type MediaKind = 'video' | 'audio';

const VIDEO_EXT = /\.(mp4|m4v|webm|mkv|ogv|mov)$/i;
const AUDIO_EXT = /\.(mp3|m4a|aac|ogg|oga|opus|wav|flac|weba)$/i;

/** 파일 이름으로 본 종류(미리보기를 열지 말지만 정함, 실제 형식은 Tauri가 바이트로 가림). */
export function mediaKindOf(name: string): MediaKind | null {
  if (VIDEO_EXT.test(name)) return 'video';
  if (AUDIO_EXT.test(name)) return 'audio';
  return null;
}

/** 썸네일 긴 변(데몬이 받아 주는 최대는 640). 차례로 줄여 가며 바이트 한도에 맞춥니다. */
export const THUMB_EDGES = [320, 240, 160];
const THUMB_QUALITIES = [0.8, 0.6];
/** 데몬 한도 64KiB보다 조금 작게. */
export const THUMB_MAX_BYTES = 60 * 1024;
/** 메타데이터·장면 하나를 기다리는 시간. */
export const LOAD_TIMEOUT_MS = 10_000;
/** 데몬이 받아 주는 동영상 가로·세로 상한. */
const MAX_VIDEO_EDGE = 16_384;
/** 데몬이 받아 주는 재생 길이 상한(7일). 넘으면(깨진 파일 등) 길이를 보내지 않습니다. */
export const MAX_DURATION_MS = 7 * 24 * 60 * 60 * 1000;

/** 재생 시간 표시(0:05, 12:34, 1:02:03). */
export function formatClock(ms: number): string {
  const total = Math.max(0, Math.round(ms / 1000));
  const h = Math.floor(total / 3600);
  const m = Math.floor((total % 3600) / 60);
  const s = String(total % 60).padStart(2, '0');
  return h > 0 ? `${h}:${String(m).padStart(2, '0')}:${s}` : `${m}:${s}`;
}

/** 긴 변이 `edge` 이하가 되게 줄인 크기(키우지 않음, 최소 1). */
export function fitEdge(width: number, height: number, edge: number): { width: number; height: number } {
  const s = Math.min(1, edge / Math.max(width, height));
  return { width: Math.max(1, Math.round(width * s)), height: Math.max(1, Math.round(height * s)) };
}

/** base64 문자열이 담은 바이트 수. */
export function base64Bytes(b64: string): number {
  const pad = b64.endsWith('==') ? 2 : b64.endsWith('=') ? 1 : 0;
  return Math.floor((b64.length * 3) / 4) - pad;
}

/** 시험에서 가짜 요소를 넣을 수 있게 만드는 곳을 받습니다. */
export interface MediaEnv {
  createMedia(kind: MediaKind): HTMLMediaElement;
  createCanvas(): HTMLCanvasElement;
  timeoutMs?: number;
}

const domEnv: MediaEnv = {
  createMedia: (kind) => document.createElement(kind),
  createCanvas: () => document.createElement('canvas'),
};

/** 이벤트 하나를 기다립니다. 오류 이벤트나 시간 초과면 거절합니다. */
function once(el: HTMLMediaElement, event: string, timeoutMs: number): Promise<void> {
  return new Promise((resolve, reject) => {
    const done = (ok: boolean) => {
      clearTimeout(timer);
      el.removeEventListener(event, onOk);
      el.removeEventListener('error', onErr);
      if (ok) resolve();
      else reject(new Error(`미디어 ${event} 실패`)); // i18n-ignore: 호출자가 모두 잡아 버리는 내부 오류
    };
    const onOk = () => done(true);
    const onErr = () => done(false);
    const timer = setTimeout(() => done(false), timeoutMs);
    el.addEventListener(event, onOk);
    el.addEventListener('error', onErr);
  });
}

/** 지금 장면을 한도 안의 JPEG base64로. 캔버스가 오염됐거나(출처) 그릴 수 없으면 undefined. */
export function snapshot(video: HTMLVideoElement, env: MediaEnv): string | undefined {
  for (const edge of THUMB_EDGES) {
    const { width, height } = fitEdge(video.videoWidth, video.videoHeight, edge);
    const canvas = env.createCanvas();
    canvas.width = width;
    canvas.height = height;
    const ctx = canvas.getContext('2d');
    if (!ctx) return undefined;
    try {
      ctx.drawImage(video, 0, 0, width, height);
      for (const q of THUMB_QUALITIES) {
        const url = canvas.toDataURL('image/jpeg', q);
        const b64 = url.startsWith('data:image/jpeg;base64,') ? url.slice(url.indexOf(',') + 1) : '';
        if (b64 && base64Bytes(b64) <= THUMB_MAX_BYTES) return b64;
      }
    } catch {
      return undefined;
    } finally {
      // 캔버스 메모리를 바로 놓습니다.
      canvas.width = 0;
      canvas.height = 0;
    }
  }
  return undefined;
}

/** 주소의 동영상·음성에서 길이·크기·썸네일을 읽습니다. 하나도 못 읽으면 null. */
export async function readMediaHints(url: string, kind: MediaKind, env: MediaEnv = domEnv): Promise<MediaHints | null> {
  const timeout = env.timeoutMs ?? LOAD_TIMEOUT_MS;
  const el = env.createMedia(kind);
  el.preload = 'metadata';
  el.muted = true;
  // 캔버스가 오염되지 않게(미리보기 응답은 앱 출처에 CORS를 엽니다).
  el.crossOrigin = 'anonymous';
  const out: MediaHints = {};
  try {
    const meta = once(el, 'loadedmetadata', timeout);
    el.src = url;
    await meta;
    const d = el.duration;
    if (Number.isFinite(d) && d > 0) {
      const ms = Math.max(1, Math.round(d * 1000));
      if (ms <= MAX_DURATION_MS) out.duration_ms = ms;
    }
    if (kind === 'video') {
      const v = el as HTMLVideoElement;
      if (v.videoWidth > 0 && v.videoHeight > 0 && v.videoWidth <= MAX_VIDEO_EDGE && v.videoHeight <= MAX_VIDEO_EDGE) {
        out.width = v.videoWidth;
        out.height = v.videoHeight;
        // 첫 장면은 검은 화면이 많아 조금 뒤(길이의 10%, 최대 1초)를 씁니다.
        const at = out.duration_ms ? Math.min(1, d * 0.1) : 0;
        try {
          if (at > 0) {
            const seeked = once(v, 'seeked', timeout);
            v.currentTime = at;
            await seeked;
          } else if (v.readyState < 2) {
            await once(v, 'loadeddata', timeout);
          }
          const thumb = snapshot(v, env);
          if (thumb) out.thumbnail_base64 = thumb;
        } catch {
          // 장면을 못 읽어도 길이·크기는 보냅니다.
        }
      }
    }
  } catch {
    // 메타데이터를 못 읽음.
  } finally {
    el.removeAttribute('src');
    try {
      el.load();
    } catch {
      // happy-dom 등 load가 없는 환경.
    }
  }
  return Object.keys(out).length > 0 ? out : null;
}

/** 고른 파일(경로)의 힌트. 동영상·음성이 아니거나 미리보기를 열 수 없으면 null. */
export async function captureMediaHints(transport: Transport, path: string, name: string, env?: MediaEnv): Promise<MediaHints | null> {
  if (!mediaKindOf(name) || !transport.openLocalPreview) return null;
  const preview = await transport.openLocalPreview(path);
  if (!preview) return null;
  try {
    const kind: MediaKind | null = preview.mime.startsWith('video/') ? 'video' : preview.mime.startsWith('audio/') ? 'audio' : null;
    if (!kind) return null;
    return await readMediaHints(preview.url, kind, env);
  } finally {
    preview.close();
  }
}
