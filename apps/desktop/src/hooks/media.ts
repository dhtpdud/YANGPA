// 미디어 URL 훅.
// - 일반 등급: 캐시 금지 커스텀 프로토콜 주소(yangpa-blob://)를 그대로 <img>/<video>에 씁니다.
// - 휘발성: 바이트를 메모리로 읽어 blob: URL을 만들고, 화면에서 빠지거나(언마운트) 만료되면 즉시 해제합니다.
//   어떤 경우에도 localStorage·IndexedDB·Cache API에 넣지 않습니다.

import { useEffect, useState, useSyncExternalStore } from 'react';
import { isRenderableMime } from '../api/client';
import { t } from '../i18n';
import { base64ToBytes, sniffImageMime } from '../lib/misc';
import { useStore } from '../store/store';

export interface BlobSource {
  space: string;
  blob: string;
  keyFrom: string;
  mime: string;
  volatile: boolean;
}

export interface MediaUrl {
  url?: string;
  loading: boolean;
  error?: string;
}

export function useBlobUrl(src: BlobSource | null, enabled = true): MediaUrl {
  const client = useStore((s) => s.client);
  const [state, setState] = useState<MediaUrl>({ loading: false });
  const space = src?.space;
  const blob = src?.blob;
  const keyFrom = src?.keyFrom;
  const mime = src?.mime ?? '';
  const volatile = !!src?.volatile;
  const direct = !!client && !!space && !!blob && !!keyFrom && !volatile && enabled && isRenderableMime(mime);

  useEffect(() => {
    if (!client || !space || !blob || !keyFrom || !enabled || !volatile) {
      setState({ loading: false });
      return;
    }
    if (!isRenderableMime(mime)) {
      setState({ loading: false, error: t('미리 볼 수 없는 형식입니다') });
      return;
    }
    let url: string | undefined;
    let cancelled = false;
    setState({ loading: true });
    client.transport
      .readBlob(space, blob, keyFrom, mime)
      .then((bytes) => {
        if (cancelled) {
          bytes.fill(0);
          return;
        }
        url = URL.createObjectURL(new Blob([bytes as Uint8Array<ArrayBuffer>], { type: mime }));
        // Blob이 사본을 들고 있으므로 원본 배열은 지웁니다.
        bytes.fill(0);
        setState({ url, loading: false });
      })
      .catch((e: unknown) => {
        if (!cancelled) setState({ loading: false, error: e instanceof Error ? e.message : t('읽기 실패') });
      });
    return () => {
      cancelled = true;
      if (url) URL.revokeObjectURL(url);
    };
  }, [client, space, blob, keyFrom, mime, volatile, enabled]);

  if (direct) return { url: client.transport.blobSrc(space, blob, keyFrom, mime), loading: false };
  return state;
}

/** 보내는 쪽 썸네일(base64). 휘발성이면 blob: URL로, 아니면 data: URL로 그립니다. */
export function useThumbUrl(b64: string | undefined, volatile: boolean): string | undefined {
  const mime = b64 ? sniffImageMime(b64) : null;
  const [url, setUrl] = useState<string | undefined>();
  useEffect(() => {
    if (!b64 || !mime || !volatile) {
      setUrl(undefined);
      return;
    }
    let bytes: Uint8Array;
    try {
      bytes = base64ToBytes(b64);
    } catch {
      setUrl(undefined);
      return;
    }
    const u = URL.createObjectURL(new Blob([bytes as Uint8Array<ArrayBuffer>], { type: mime }));
    bytes.fill(0);
    setUrl(u);
    return () => URL.revokeObjectURL(u);
  }, [b64, mime, volatile]);
  if (!b64 || !mime) return undefined;
  return volatile ? url : `data:${mime};base64,${b64}`;
}

// ───────────── 공유 시계(카운트다운용, 타이머 하나) ─────────────

let now = Date.now();
const listeners = new Set<() => void>();
let timer: ReturnType<typeof setInterval> | null = null;

function subscribe(fn: () => void) {
  listeners.add(fn);
  if (!timer) {
    timer = setInterval(() => {
      now = Date.now();
      for (const l of listeners) l();
    }, 1000);
  }
  return () => {
    listeners.delete(fn);
    if (listeners.size === 0 && timer) {
      clearInterval(timer);
      timer = null;
    }
  };
}

/** 1초마다 바뀌는 현재 시각. 구독하는 컴포넌트만 다시 그립니다. */
export function useNow(): number {
  return useSyncExternalStore(subscribe, () => now, () => now);
}
