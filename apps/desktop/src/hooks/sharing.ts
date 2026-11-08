// 공유 중단·재개(seed.exclude_message): 채팅 메시지 메뉴와 라이브러리가 함께 씁니다.
// 공간마다 seed.get을 한 번 읽어 멈춘 메시지를 기억합니다(메시지마다 묻지 않게).

import { create } from 'zustand';
import type { ChannelId, EventId, SpaceId } from '../api/types';
import { t } from '../i18n';
import { useStore } from '../store/store';

interface SharingState {
  /** 공간 → 공유를 멈춘 메시지. 읽은 공간만 들어 있습니다. */
  spaces: Record<SpaceId, ReadonlySet<EventId>>;
}

export const useSharingStore = create<SharingState>(() => ({ spaces: {} }));

const loading = new Set<SpaceId>();

/** 이 공간의 공유 중단 목록을 아직 읽지 않았으면 읽습니다. 못 읽으면 모르는 채로 둡니다(메뉴는 '공유 중단'만). */
export async function loadSharing(space: SpaceId): Promise<void> {
  const { client } = useStore.getState();
  if (!client || loading.has(space) || useSharingStore.getState().spaces[space]) return;
  loading.add(space);
  try {
    const p = await client.call('seed.get', { space });
    useSharingStore.setState((s) => ({ spaces: { ...s.spaces, [space]: new Set(p.excluded_events ?? []) } }));
  } catch {
    // 예전 데몬·연결 끊김: 다음에 메뉴를 열 때 다시 읽습니다.
  } finally {
    loading.delete(space);
  }
}

/** 이 메시지의 공유를 멈췄는지. 아직 모르면 false. */
export function useStopped(space: SpaceId, id: EventId): boolean {
  return useSharingStore((s) => !!s.spaces[space]?.has(id));
}

function mark(space: SpaceId, id: EventId, stopped: boolean) {
  useSharingStore.setState((s) => {
    const cur = s.spaces[space];
    // 아직 읽지 않은 공간은 나중에 읽을 때 데몬 값을 씁니다.
    if (!cur) return s;
    const next = new Set(cur);
    if (stopped) next.add(id);
    else next.delete(id);
    return { spaces: { ...s.spaces, [space]: next } };
  });
}

export interface ShareTarget {
  space: SpaceId;
  channel: ChannelId;
  id: EventId;
}

/** 메시지마다 공유를 멈추거나(`on` false) 다시 켭니다. `onDone`은 바뀐 메시지 ID를 받습니다. */
export async function setSharing(targets: readonly ShareTarget[], on: boolean, onDone?: (ids: Set<EventId>) => void) {
  const { client, call, toast } = useStore.getState();
  if (!client) return;
  const done = new Set<EventId>();
  for (const x of targets) {
    if (done.has(x.id)) continue;
    const r = await call(client.call('seed.exclude_message', { space: x.space, channel: x.channel, id: x.id, excluded: !on }));
    if (r === undefined) continue;
    done.add(x.id);
    mark(x.space, x.id, !on);
  }
  if (done.size === 0) return;
  onDone?.(done);
  toast('success', on ? t('공유를 다시 시작했습니다') : t('공유를 멈췄습니다'));
}

/** 시험용. */
export function resetSharingForTest() {
  useSharingStore.setState({ spaces: {} });
}
