// 가상 스크롤 목록의 행 만들기: 날짜 구분선, '받지 않은 기록' 표시, 같은 작성자 연속 메시지 묶음, 포럼 글타래의 게시글.

import type { GapView, MessageView } from '../api/types';
import { isAnonymousMessage } from './anon';
import { dayKey } from './format';

export type Row =
  | { kind: 'start'; key: string }
  | { kind: 'loading'; key: string }
  | { kind: 'day'; key: string; at: number }
  | { kind: 'gap'; key: string; gap: GapView }
  | { kind: 'message'; key: string; message: MessageView; grouped: boolean }
  /** 포럼 글타래 맨 위의 게시글 자체(데몬이 글타래 첫 페이지에 함께 줌). 글타래의 시작 표시를 대신합니다. */
  | { kind: 'post'; key: string; message: MessageView };

/** 같은 작성자 메시지를 묶는 최대 간격(디스코드와 비슷하게 7분). */
export const GROUP_WINDOW_MS = 7 * 60 * 1000;

export interface RowInput {
  messages: MessageView[];
  gaps: GapView[];
  reachedStart: boolean;
  loadingOlder: boolean;
  /** 포럼 글타래면 게시글 ID. 목록에 그 메시지가 있으면 맨 위 게시글 행으로 따로 그립니다. */
  post?: string;
}

function canGroup(prev: MessageView | undefined, m: MessageView): boolean {
  if (!prev) return false;
  // 익명 글은 글쓴이가 가려져 있어 가명으로 묶습니다. 가명은 사람마다 정해져 화면에 이미 보이므로 묶어도 더 드러나지 않고,
  // 가명이 없는 옛 데몬 글은 모두 '익명'이라 묶으면 연달아 쓴 사람이 드러나 하나씩 그립니다.
  const anon = isAnonymousMessage(m);
  if (anon !== isAnonymousMessage(prev)) return false;
  if (anon ? !m.anon_name || prev.anon_name !== m.anon_name : prev.author !== m.author) return false;
  if (prev.deleted || m.deleted) return false;
  if (m.reply_to) return false;
  if (m.created_at - prev.created_at > GROUP_WINDOW_MS || m.created_at < prev.created_at) return false;
  return dayKey(prev.created_at) === dayKey(m.created_at);
}

export function buildRows({ messages, gaps, reachedStart, loadingOlder, post }: RowInput): Row[] {
  const rows: Row[] = [];
  // 게시글은 글타래에서 가장 오래된 메시지라, 있으면 이미 맨 앞까지 읽은 것입니다.
  const head = post ? messages.find((m) => m.id === post) : undefined;
  if (head) rows.push({ kind: 'post', key: head.id, message: head });
  else if (reachedStart) rows.push({ kind: 'start', key: 'start' });
  else if (loadingOlder) rows.push({ kind: 'loading', key: 'loading' });

  // 구간을 어디에 꽂을지: before가 보이면 그 앞, 아니면 after가 보이면 그 뒤.
  const ids = new Set(messages.map((m) => m.id));
  const gapBefore = new Map<string, GapView[]>();
  const gapAfter = new Map<string, GapView[]>();
  const leading: GapView[] = [];
  for (const g of gaps) {
    if (g.before && ids.has(g.before)) push(gapBefore, g.before, g);
    else if (g.after && ids.has(g.after)) push(gapAfter, g.after, g);
    else if (!g.after && !g.before) leading.push(g);
  }
  for (const g of leading) rows.push({ kind: 'gap', key: gapKey(g), gap: g });
  // 게시글 행은 루프 밖에서 그리므로, 게시글에 붙은 구간은 여기서 넣어야 사라지지 않습니다.
  if (head) {
    for (const g of [...(gapBefore.get(head.id) ?? []), ...(gapAfter.get(head.id) ?? [])]) rows.push({ kind: 'gap', key: gapKey(g), gap: g });
  }

  let prev: MessageView | undefined;
  let prevDay: string | undefined;
  for (const m of messages) {
    if (m === head) continue;
    const before = gapBefore.get(m.id);
    let brokeByGap = false;
    if (before) {
      for (const g of before) rows.push({ kind: 'gap', key: gapKey(g), gap: g });
      brokeByGap = true;
    }
    const d = dayKey(m.created_at);
    let newDay = false;
    if (d !== prevDay) {
      rows.push({ kind: 'day', key: `day-${d}`, at: m.created_at });
      prevDay = d;
      newDay = true;
    }
    rows.push({ kind: 'message', key: m.id, message: m, grouped: !brokeByGap && !newDay && canGroup(prev, m) });
    prev = m;
    const after = gapAfter.get(m.id);
    if (after) {
      for (const g of after) rows.push({ kind: 'gap', key: gapKey(g), gap: g });
      prev = undefined; // 구간 뒤에서는 묶지 않음
    }
  }
  return rows;
}

function push<K, V>(map: Map<K, V[]>, k: K, v: V) {
  const list = map.get(k);
  if (list) list.push(v);
  else map.set(k, [v]);
}

export function gapKey(g: GapView): string {
  return `gap-${g.after ?? '^'}-${g.before ?? '$'}`;
}

/** (lamport, id) 순으로 정렬된 배열에 메시지를 넣거나 바꿉니다. 새 배열을 돌려줍니다. */
export function upsertSorted(list: MessageView[], m: MessageView): MessageView[] {
  const idx = list.findIndex((x) => x.id === m.id);
  if (idx >= 0) {
    const copy = list.slice();
    copy[idx] = m;
    return copy;
  }
  // 대부분 끝에 붙으므로 뒤에서부터 찾습니다.
  let i = list.length;
  while (i > 0 && compareMsg(list[i - 1], m) > 0) i--;
  const copy = list.slice();
  copy.splice(i, 0, m);
  return copy;
}

export function compareMsg(a: MessageView, b: MessageView): number {
  if (a.lamport !== b.lamport) return a.lamport - b.lamport;
  return a.id < b.id ? -1 : a.id > b.id ? 1 : 0;
}

/** 더 오래된 쪽 페이지를 앞에 붙입니다(중복 제거). */
export function prependOlder(list: MessageView[], older: MessageView[]): MessageView[] {
  if (older.length === 0) return list;
  const have = new Set(list.map((m) => m.id));
  const fresh = older.filter((m) => !have.has(m.id)).sort(compareMsg);
  if (fresh.length === 0) return list;
  // 겹치는 구간이 있으면 정렬로 합칩니다.
  if (list.length > 0 && compareMsg(fresh[fresh.length - 1], list[0]) > 0) {
    return [...list, ...fresh].sort(compareMsg);
  }
  return [...fresh, ...list];
}
