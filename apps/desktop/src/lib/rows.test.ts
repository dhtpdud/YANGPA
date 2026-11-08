import { describe, expect, it } from 'vitest';
import { ANONYMOUS_USER, type MessageView } from '../api/types';
import { GROUP_WINDOW_MS, buildRows, compareMsg, gapKey, prependOlder, upsertSorted } from './rows';

let seq = 0;
function msg(author: string, at: number, extra: Partial<MessageView> = {}): MessageView {
  seq++;
  return {
    id: `m${String(seq).padStart(4, '0')}`,
    author,
    author_device: 'd',
    created_at: at,
    lamport: seq,
    text: 't',
    attachments: [],
    embeds: [],
    reactions: [],
    deleted: false,
    volatile: false,
    ...extra,
  };
}

const T0 = new Date(2026, 9, 18, 12, 0, 0).getTime();

describe('행 만들기', () => {
  it('같은 작성자 연속 메시지를 묶고 7분이 지나면 끊는다', () => {
    const a = msg('a', T0);
    const b = msg('a', T0 + 60_000);
    const c = msg('a', T0 + 60_000 + GROUP_WINDOW_MS + 1);
    const d = msg('b', T0 + 60_000 + GROUP_WINDOW_MS + 2);
    const rows = buildRows({ messages: [a, b, c, d], gaps: [], reachedStart: true, loadingOlder: false });
    const kinds = rows.map((r) => (r.kind === 'message' ? `${r.message.id}:${r.grouped}` : r.kind));
    expect(kinds).toEqual(['start', 'day', `${a.id}:false`, `${b.id}:true`, `${c.id}:false`, `${d.id}:false`]);
  });

  it('날짜가 바뀌면 구분선을 넣고 묶지 않는다', () => {
    const a = msg('a', T0 + 11.9 * 3600_000);
    const b = msg('a', T0 + 12.05 * 3600_000);
    const rows = buildRows({ messages: [a, b], gaps: [], reachedStart: false, loadingOlder: true });
    expect(rows.map((r) => r.kind)).toEqual(['loading', 'day', 'message', 'day', 'message']);
    expect(rows[4]).toMatchObject({ grouped: false });
  });

  it('답장·삭제 메시지는 묶지 않는다', () => {
    const a = msg('a', T0);
    const b = msg('a', T0 + 1000, { reply_to: a.id });
    const c = msg('a', T0 + 2000, { deleted: true });
    const rows = buildRows({ messages: [a, b, c], gaps: [], reachedStart: false, loadingOlder: false });
    expect(rows.filter((r) => r.kind === 'message').map((r) => (r.kind === 'message' ? r.grouped : null))).toEqual([false, false, false]);
  });

  it('받지 않은 기록 구간을 before 앞(없으면 after 뒤)에 넣는다', () => {
    const a = msg('a', T0);
    const b = msg('a', T0 + 1000);
    const c = msg('a', T0 + 2000);
    const g1 = { after: a.id, before: b.id };
    const g2 = { after: c.id, before: 'unknown' };
    const g3 = {};
    const rows = buildRows({ messages: [a, b, c], gaps: [g1, g2, g3], reachedStart: false, loadingOlder: false });
    expect(rows.map((r) => (r.kind === 'message' ? r.message.id : r.kind === 'gap' ? gapKey(r.gap) : r.kind))).toEqual([
      gapKey(g3),
      'day',
      a.id,
      gapKey(g1),
      b.id,
      c.id,
      gapKey(g2),
    ]);
    // 구간 뒤 메시지는 묶지 않음
    expect(rows[4]).toMatchObject({ kind: 'message', grouped: false });
  });

  it('upsertSorted는 (lamport, id) 순서를 지키고 같은 ID는 바꾼다', () => {
    const a = msg('a', T0, { lamport: 1 });
    const b = msg('a', T0, { lamport: 3 });
    const c = msg('a', T0, { lamport: 2 });
    let list = upsertSorted([], a);
    list = upsertSorted(list, b);
    list = upsertSorted(list, c);
    expect(list.map((m) => m.lamport)).toEqual([1, 2, 3]);
    const edited = { ...c, text: '수정' };
    const next = upsertSorted(list, edited);
    expect(next).not.toBe(list);
    expect(next[1].text).toBe('수정');
    expect(next).toHaveLength(3);
  });

  it('prependOlder는 중복을 버리고 겹치면 정렬한다', () => {
    const ms = Array.from({ length: 6 }, (_, i) => msg('a', T0 + i, { lamport: i + 1 }));
    const current = ms.slice(3);
    expect(prependOlder(current, ms.slice(0, 3)).map((m) => m.lamport)).toEqual([1, 2, 3, 4, 5, 6]);
    expect(prependOlder(current, ms.slice(1, 5)).map((m) => m.lamport)).toEqual([2, 3, 4, 5, 6]);
    expect(prependOlder(current, [])).toBe(current);
    expect([...ms].reverse().sort(compareMsg).map((m) => m.lamport)).toEqual([1, 2, 3, 4, 5, 6]);
  });
});

describe('익명 글·포럼 게시글 행', () => {
  it('익명 글은 같은 가명끼리만 묶는다', () => {
    const anon = (at: number, anon_name?: string) => msg(ANONYMOUS_USER, at, { anonymous: true, anon_name });
    const ms = [
      anon(T0, '숨어 있는 박쥐'),
      anon(T0 + 1000, '숨어 있는 박쥐'),
      anon(T0 + 2000, '차분한 판다'),
      anon(T0 + 3000, '차분한 판다'),
      // 가명이 없는 옛 데몬 글은 모두 '익명'이라 묶지 않습니다.
      anon(T0 + 4000),
      anon(T0 + 5000),
      // 익명 글과 같은 사람의 실명 글은 묶지 않습니다.
      msg('a', T0 + 6000),
      msg('a', T0 + 7000, { anonymous: true, anon_name: '차분한 판다' }),
    ];
    const rows = buildRows({ messages: ms, gaps: [], reachedStart: true, loadingOlder: false });
    expect(rows.filter((r) => r.kind === 'message').map((r) => (r.kind === 'message' ? r.grouped : null))).toEqual([
      false,
      true,
      false,
      true,
      false,
      false,
      false,
      false,
    ]);
  });

  it('글타래의 게시글은 시작 표시 대신 맨 위 게시글 행으로 한 번만 그린다', () => {
    const post = msg('a', T0);
    const c1 = msg('b', T0 + 1000, { thread: post.id });
    const c2 = msg('b', T0 + 2000, { thread: post.id });
    const gap = { after: post.id, before: c1.id };
    const rows = buildRows({ messages: [post, c1, c2], gaps: [gap], reachedStart: true, loadingOlder: false, post: post.id });
    expect(rows.map((r) => (r.kind === 'message' ? r.message.id : r.kind === 'post' ? `post:${r.message.id}` : r.kind === 'gap' ? 'gap' : r.kind))).toEqual([
      `post:${post.id}`,
      'gap',
      'day',
      c1.id,
      c2.id,
    ]);
    // 게시글 아래 첫 댓글은 게시글과 묶이지 않습니다.
    expect(rows[3]).toMatchObject({ kind: 'message', grouped: false });
  });

  it('게시글을 아직 받지 못했으면 평소처럼 불러오는 중을 보인다', () => {
    const c1 = msg('b', T0 + 1000, { thread: 'p' });
    const rows = buildRows({ messages: [c1], gaps: [], reachedStart: false, loadingOlder: true, post: 'p' });
    expect(rows.map((r) => r.kind)).toEqual(['loading', 'day', 'message']);
  });
});
