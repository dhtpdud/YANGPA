// 고정한 첨부(blob.pin): 저장 공간 정리에서 빼 둔 블롭 ID 목록.
// 첨부마다 blob.status를 묻지 않으려고(데몬이 멤버 기기마다 보유 여부를 물어봄) blob.pins를 한 번 읽어 기억합니다.
// 블롭 ID만 들고, 내용·키는 없습니다. 고정을 모르는 예전 데몬이면 고정 단추를 숨깁니다.

import { useEffect } from 'react';
import { create } from 'zustand';
import type { ApiClient } from '../api/client';
import { ErrorCode, type BlobId, type SpaceId } from '../api/types';
import { t } from '../i18n';
import { useStore } from '../store/store';

interface PinState {
  /** 'idle' → 'loading' → 'ready' | 'unsupported'. */
  status: 'idle' | 'loading' | 'ready' | 'unsupported';
  ids: ReadonlySet<string>;
}

export const usePinStore = create<PinState>(() => ({ status: 'idle', ids: new Set() }));

export async function loadPins(client: ApiClient): Promise<void> {
  usePinStore.setState({ status: 'loading' });
  try {
    const list = await client.call('blob.pins', {});
    usePinStore.setState({ status: 'ready', ids: new Set(list.map((p) => p.blob)) });
  } catch (e) {
    const code = (e as { code?: number }).code;
    usePinStore.setState({ status: code === ErrorCode.MethodNotFound ? 'unsupported' : 'idle' });
  }
}

/** 이 블롭을 고정했는지. 처음 쓰일 때 목록을 읽습니다. 고정을 지원하지 않으면 null. */
export function usePinned(blob: string, enabled = true): boolean | null {
  const client = useStore((s) => s.client);
  const status = usePinStore((s) => s.status);
  const pinned = usePinStore((s) => s.ids.has(blob));
  useEffect(() => {
    if (enabled && client && usePinStore.getState().status === 'idle') void loadPins(client);
  }, [client, enabled]);
  if (status === 'unsupported') return null;
  return pinned;
}

/** 고정하거나 풉니다. 성공하면 목록을 바로 고칩니다. */
export function markPinned(blob: string, pinned: boolean) {
  const ids = new Set(usePinStore.getState().ids);
  if (pinned) ids.add(blob);
  else ids.delete(blob);
  usePinStore.setState({ ids });
}

/** 여러 첨부를 보관하거나 풉니다(라이브러리·채팅 메뉴). 보관하면 아직 받지 않은 것은 받기 시작합니다(blob.pin). */
export async function pinBlobs(list: readonly { space: SpaceId; blob: BlobId }[], pinned: boolean): Promise<void> {
  const { client, call, toast } = useStore.getState();
  if (!client) return;
  const seen = new Set<BlobId>();
  for (const x of list) {
    if (seen.has(x.blob)) continue;
    seen.add(x.blob);
    const r = await call(client.call(pinned ? 'blob.pin' : 'blob.unpin', { space: x.space, blob: x.blob }));
    if (r === undefined) seen.delete(x.blob);
    else markPinned(x.blob, pinned);
  }
  if (seen.size === 0) return;
  toast('success', pinned ? t('파일 {n}개를 보관합니다(저장 공간 정리에서 뺌)', { n: seen.size }) : t('파일 {n}개의 보관을 풀었습니다', { n: seen.size }));
}

/** 시험용. */
export function resetPinsForTest() {
  usePinStore.setState({ status: 'idle', ids: new Set() });
}
