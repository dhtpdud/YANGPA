// 보내기 전 동영상·음성 힌트: 길이·크기·썸네일을 읽고, 한도에 맞추고, 실패하면 힌트 없이(null) 보냅니다.
// 실제 디코더 대신 가짜 미디어 요소·캔버스로 이벤트 순서를 흉내 냅니다.

import { describe, expect, it, vi } from 'vitest';
import type { LocalPreview, Transport } from '../api/client';
import {
  MAX_DURATION_MS,
  THUMB_MAX_BYTES,
  base64Bytes,
  captureMediaHints,
  fitEdge,
  formatClock,
  mediaKindOf,
  readMediaHints,
  type MediaEnv,
  type MediaKind,
} from './media';

interface FakeOpts {
  duration?: number;
  width?: number;
  height?: number;
  /** 메타데이터 대신 오류를 냅니다. */
  fail?: boolean;
  /** 아무 이벤트도 내지 않습니다(시간 초과). */
  silent?: boolean;
}

class FakeMedia extends EventTarget {
  preload = '';
  muted = false;
  crossOrigin: string | null = null;
  duration = NaN;
  videoWidth = 0;
  videoHeight = 0;
  readyState = 1;
  srcSet: string[] = [];
  removed = 0;
  private time = 0;
  constructor(
    private kind: MediaKind,
    private o: FakeOpts,
  ) {
    super();
  }
  set src(v: string) {
    this.srcSet.push(v);
    if (this.o.silent) return;
    queueMicrotask(() => {
      if (this.o.fail) {
        this.dispatchEvent(new Event('error'));
        return;
      }
      this.duration = this.o.duration ?? NaN;
      if (this.kind === 'video') {
        this.videoWidth = this.o.width ?? 0;
        this.videoHeight = this.o.height ?? 0;
      }
      this.dispatchEvent(new Event('loadedmetadata'));
    });
  }
  get currentTime() {
    return this.time;
  }
  set currentTime(v: number) {
    this.time = v;
    queueMicrotask(() => this.dispatchEvent(new Event('seeked')));
  }
  removeAttribute(name: string) {
    if (name === 'src') this.removed++;
  }
  load() {}
}

/** 그린 크기와 품질을 기록하고, 정해 둔 길이의 base64를 돌려주는 캔버스. */
function fakeCanvas(sizes: Array<[number, number, number]>, b64Len: (w: number, q: number) => number, mime = 'image/jpeg') {
  return () => {
    const c = {
      width: 0,
      height: 0,
      getContext: () => ({ drawImage: vi.fn() }),
      toDataURL: (_type: string, q: number) => {
        sizes.push([c.width, c.height, q]);
        return `data:${mime};base64,${'A'.repeat(b64Len(c.width, q))}`;
      },
    };
    return c as unknown as HTMLCanvasElement;
  };
}

function env(kind: MediaKind, o: FakeOpts, canvas = fakeCanvas([], () => 400)): MediaEnv & { el: FakeMedia } {
  const el = new FakeMedia(kind, o);
  return { el, createMedia: () => el as unknown as HTMLMediaElement, createCanvas: canvas, timeoutMs: 50 };
}

describe('표시·계산 도우미', () => {
  it('재생 시간을 시계 모양으로', () => {
    expect(formatClock(0)).toBe('0:00');
    expect(formatClock(5_400)).toBe('0:05');
    expect(formatClock(754_000)).toBe('12:34');
    expect(formatClock(3_723_000)).toBe('1:02:03');
  });

  it('긴 변을 줄이되 키우지 않는다', () => {
    expect(fitEdge(1920, 1080, 320)).toEqual({ width: 320, height: 180 });
    expect(fitEdge(1080, 1920, 320)).toEqual({ width: 180, height: 320 });
    expect(fitEdge(100, 50, 320)).toEqual({ width: 100, height: 50 });
    expect(fitEdge(10_000, 1, 320).height).toBe(1);
  });

  it('base64 바이트 수', () => {
    expect(base64Bytes('AAAA')).toBe(3);
    expect(base64Bytes('AAA=')).toBe(2);
    expect(base64Bytes('AA==')).toBe(1);
  });

  it('이름으로 동영상·음성을 가린다', () => {
    expect(mediaKindOf('클립.MP4')).toBe('video');
    expect(mediaKindOf('a.webm')).toBe('video');
    expect(mediaKindOf('노래.mp3')).toBe('audio');
    expect(mediaKindOf('voice.ogg')).toBe('audio');
    expect(mediaKindOf('사진.png')).toBeNull();
    expect(mediaKindOf('문서.pdf')).toBeNull();
  });
});

describe('readMediaHints', () => {
  it('동영상: 길이·크기와 조금 뒤 장면의 JPEG 썸네일을 읽는다', async () => {
    const sizes: Array<[number, number, number]> = [];
    const e = env('video', { duration: 12.5, width: 1920, height: 1080 }, fakeCanvas(sizes, () => 400));
    const h = await readMediaHints('yangpa-blob://localhost/local/t', 'video', e);
    expect(h).toEqual({ duration_ms: 12_500, width: 1920, height: 1080, thumbnail_base64: 'A'.repeat(400) });
    expect(e.el.crossOrigin).toBe('anonymous');
    expect(e.el.currentTime).toBe(1);
    expect(sizes[0]).toEqual([320, 180, 0.8]);
    // 다 쓰면 주소를 떼어 냅니다(파일을 붙잡고 있지 않게).
    expect(e.el.removed).toBe(1);
  });

  it('썸네일이 한도를 넘으면 품질·크기를 줄여 다시 만든다', async () => {
    const sizes: Array<[number, number, number]> = [];
    const big = Math.ceil((THUMB_MAX_BYTES * 4) / 3) + 8;
    // 긴 변 160px, 품질 0.6에서만 한도 안.
    const e = env('video', { duration: 3, width: 640, height: 360 }, fakeCanvas(sizes, (w, q) => (w <= 160 && q < 0.7 ? 100 : big)));
    const h = await readMediaHints('u', 'video', e);
    expect(h?.thumbnail_base64).toHaveLength(100);
    expect(sizes.map(([w, , q]) => [w, q])).toEqual([
      [320, 0.8],
      [320, 0.6],
      [240, 0.8],
      [240, 0.6],
      [160, 0.8],
      [160, 0.6],
    ]);
  });

  it('JPEG로 만들 수 없으면 썸네일만 빼고 길이·크기는 보낸다', async () => {
    const e = env('video', { duration: 2, width: 64, height: 36 }, fakeCanvas([], () => 10, 'image/png'));
    expect(await readMediaHints('u', 'video', e)).toEqual({ duration_ms: 2000, width: 64, height: 36 });
  });

  it('음성: 길이만', async () => {
    const e = env('audio', { duration: 90 });
    expect(await readMediaHints('u', 'audio', e)).toEqual({ duration_ms: 90_000 });
  });

  it('데몬이 받지 않는 길이(7일 넘음, 깨진 파일)는 싣지 않는다', async () => {
    expect(await readMediaHints('u', 'audio', env('audio', { duration: MAX_DURATION_MS / 1000 }))).toEqual({ duration_ms: MAX_DURATION_MS });
    expect(await readMediaHints('u', 'audio', env('audio', { duration: MAX_DURATION_MS / 1000 + 1 }))).toBeNull();
  });

  it('길이를 모르는 스트림·오류·시간 초과는 null', async () => {
    expect(await readMediaHints('u', 'audio', env('audio', { duration: Infinity }))).toBeNull();
    expect(await readMediaHints('u', 'video', env('video', { fail: true }))).toBeNull();
    const silent = env('video', { silent: true });
    expect(await readMediaHints('u', 'video', silent)).toBeNull();
    expect(silent.el.removed).toBe(1);
  });

  it('데몬이 받지 않는 크기의 동영상은 크기·썸네일을 싣지 않는다', async () => {
    const e = env('video', { duration: 1, width: 20_000, height: 100 });
    expect(await readMediaHints('u', 'video', e)).toEqual({ duration_ms: 1000 });
  });
});

describe('captureMediaHints', () => {
  function transport(preview: Omit<LocalPreview, 'close'> | null) {
    const close = vi.fn();
    const open = vi.fn(async () => (preview ? { ...preview, close } : null));
    return { t: { openLocalPreview: open } as unknown as Transport, open, close };
  }

  it('미리보기를 열어 읽고 꼭 닫는다(형식은 Tauri가 바이트로 가린 것을 따름)', async () => {
    const { t, open, close } = transport({ url: 'yangpa-blob://localhost/local/x', mime: 'audio/ogg', size: 10 });
    const e = env('audio', { duration: 4 });
    // 이름은 동영상이지만 내용은 음성.
    expect(await captureMediaHints(t, '/a/clip.webm', 'clip.webm', e)).toEqual({ duration_ms: 4000 });
    expect(open).toHaveBeenCalledWith('/a/clip.webm');
    expect(close).toHaveBeenCalledTimes(1);
    expect(e.el.srcSet).toEqual(['yangpa-blob://localhost/local/x']);
  });

  it('동영상·음성 이름이 아니면 열지 않고, 열 수 없거나 지원하지 않으면 null', async () => {
    const a = transport({ url: 'u', mime: 'video/mp4', size: 1 });
    expect(await captureMediaHints(a.t, '/a/doc.pdf', 'doc.pdf')).toBeNull();
    expect(a.open).not.toHaveBeenCalled();
    const b = transport(null);
    expect(await captureMediaHints(b.t, '/a/clip.mp4', 'clip.mp4')).toBeNull();
    expect(await captureMediaHints({} as Transport, '/a/clip.mp4', 'clip.mp4')).toBeNull();
  });
});
