// @vitest-environment happy-dom
// 검색 결과: 전체 수와 쪽 번호를 보이고, 쪽을 누르면 그 쪽만 데몬에서 받으며(늦게 온 이전 응답은 버림), 쪽 크기 입력칸이 쪽을 다시 나눕니다.

import { act } from 'react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import type { ApiClient } from '../api/client';
import type { MessageSearchPage, MessageSearchParams, MessageView, SpaceView } from '../api/types';
import { useStore } from '../store/store';
import { button, cleanup, click, render } from '../test/dom';
import { SearchResults } from './SearchResults';

const SPACE = 'S'.repeat(32);
const CH = 'C'.repeat(32);
const view = { space: SPACE, kind: 'crew', settings: { name: '크루' }, members: [], roles: [], owner: 'O', channels: [] } as unknown as SpaceView;

const msg = (i: number): MessageView => ({
  id: `M${String(i).padStart(4, '0')}`,
  author: 'A',
  author_device: 'D',
  created_at: 1_700_000_000_000 + i,
  lamport: i,
  text: `기록 ${i}`,
  attachments: [],
  embeds: [],
  reactions: [],
  deleted: false,
  volatile: false,
  space: SPACE,
  channel: CH,
});
// 최신순 120개.
const ALL = Array.from({ length: 120 }, (_, i) => msg(120 - i));

function pageOf(p: MessageSearchParams): MessageSearchPage {
  const limit = p.limit ?? 50;
  let offset = p.offset ?? 0;
  if (offset >= ALL.length) offset = Math.floor((ALL.length - 1) / limit) * limit;
  return { messages: ALL.slice(offset, offset + limit), total: ALL.length, offset, took_ms: 12 };
}

function setup(call: (method: string, params?: unknown) => Promise<unknown>) {
  useStore.setState({
    client: { call: vi.fn(call), transport: { savePrefs: vi.fn(async () => undefined) } } as unknown as ApiClient,
    views: { [SPACE]: view },
    selected: { space: SPACE, channel: CH },
    search: null,
    prefs: { ...useStore.getState().prefs, searchPageSize: 50 },
  });
  return useStore.getState().client!.call as unknown as ReturnType<typeof vi.fn>;
}

afterEach(async () => {
  await cleanup();
  useStore.setState({ client: null, search: null, views: {}, selected: { space: 'home', channel: null } });
});

const shown = (el: HTMLElement) => [...el.querySelectorAll('.sp-item')].map((x) => x.textContent ?? '');

describe('검색 결과 쪽', () => {
  it('전체 수와 쪽 번호를 보이고, 쪽을 누르면 그 쪽만 받는다', async () => {
    const call = setup(async (_m, p) => pageOf(p as MessageSearchParams));
    await act(() => useStore.getState().runSearch('기록'));
    expect(call).toHaveBeenLastCalledWith('message.search', { space: SPACE, query: '기록', offset: 0, limit: 50 });
    const { el } = await render(<SearchResults space={view} />);
    expect(el.textContent).toContain('120개');
    expect(el.textContent).toContain('0.012초');
    expect(el.textContent).toContain('1–50 / 120 · 1/3쪽');
    expect(shown(el)).toHaveLength(50);
    expect(shown(el)[0]).toContain('기록 120');
    await click(button(el, '3'));
    expect(call).toHaveBeenLastCalledWith('message.search', { space: SPACE, query: '기록', offset: 100, limit: 50 });
    expect(el.textContent).toContain('101–120 / 120 · 3/3쪽');
    expect(shown(el)).toHaveLength(20);
    expect(shown(el)[19]).toContain('기록 1');
    expect(el.querySelector<HTMLButtonElement>('button[title="다음 쪽"]')!.disabled).toBe(true);
  });

  it('늦게 온 이전 쪽 응답은 지금 쪽을 덮지 않는다', async () => {
    const pending: ((v: unknown) => void)[] = [];
    setup((_m, p) => {
      const params = p as MessageSearchParams;
      // 첫 쪽(0)만 늦게 답합니다.
      if (params.offset === 0 && pending.length === 0) return new Promise((r) => pending.push(() => r(pageOf(params))));
      return Promise.resolve(pageOf(params));
    });
    const first = useStore.getState().runSearch('기록');
    expect(useStore.getState().search?.loading).toBe(true);
    await act(() => useStore.getState().searchPage(50));
    expect(useStore.getState().search?.offset).toBe(50);
    pending[0](undefined);
    await act(() => first);
    const s = useStore.getState().search!;
    expect(s.offset).toBe(50);
    expect(s.results[0].text).toBe('기록 70');
    expect(s.loading).toBe(false);
  });

  it('쪽 크기를 바꾸면 보던 첫 글이 든 쪽을 새 크기로 다시 받는다', async () => {
    const call = setup(async (_m, p) => pageOf(p as MessageSearchParams));
    await act(() => useStore.getState().runSearch('기록'));
    await act(() => useStore.getState().searchPage(50));
    const { el } = await render(<SearchResults space={view} />);
    const input = el.querySelector<HTMLInputElement>('.sp-size input')!;
    await act(async () => {
      const set = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')!.set!;
      set.call(input, '20');
      input.dispatchEvent(new Event('input', { bubbles: true }));
    });
    await act(async () => {
      input.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', bubbles: true }));
    });
    expect(useStore.getState().prefs.searchPageSize).toBe(20);
    // 51번째 글이 든 쪽: 41–60.
    expect(call).toHaveBeenLastCalledWith('message.search', { space: SPACE, query: '기록', offset: 40, limit: 20 });
    expect(el.textContent).toContain('41–60 / 120 · 3/6쪽');
  });
});
