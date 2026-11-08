// SPDX-License-Identifier: LicenseRef-YANGPA-Supporter
// @vitest-environment happy-dom
// 라이브러리 탭: library.list로 읽어 폴더 나무와 목록을 그리고, 폴더를 고르면 그 안만, 오른쪽 클릭 메뉴의
// 공유 중단은 메시지마다 seed.exclude_message를, 보관은 파일마다 blob.pin을, 이 기기에서 지우기는 확인 뒤
// blob.evict를 부릅니다. 받지 않은 파일은
// 보이는 것만으로 받기·상태를 묻지 않습니다. 가상 스크롤은 모든 줄을 그리는 가짜로 바꿉니다.

import { act } from 'react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import type { ApiClient } from '../api/client';
import type { AttachmentView, ChannelView, LibraryItem, MessageView, SpaceSummary, SpaceView } from '../api/types';
import { resetPinsForTest } from '../hooks/pins';
import { resetSharingForTest } from '../hooks/sharing';
import { DEFAULT_LIBRARY } from './library';
import { useStore } from '../store/store';
import { cleanup, click, render } from '../test/dom';
import { LibraryView } from './LibraryView';

vi.mock('@tanstack/react-virtual', () => ({
  useVirtualizer: (o: { count: number }) => ({
    getVirtualItems: () => Array.from({ length: o.count }, (_, i) => ({ index: i, key: i, start: i * 32, size: 32, end: (i + 1) * 32, lane: 0 })),
    getTotalSize: () => o.count * 32,
  }),
}));

const att = (blob: string, name: string, downloaded = false): AttachmentView => ({ blob, name, mime: 'application/octet-stream', size: 1000, downloaded });

function item(id: string, space: string, channel: string, at: number, atts: AttachmentView[]): LibraryItem {
  const message = { id, author: 'U'.repeat(32), author_device: 'D', created_at: at, lamport: at, text: '', attachments: atts, embeds: [], reactions: [], deleted: false, volatile: false, author_blocked: false, space, channel } as MessageView;
  return { message, received_at: at, excluded: false };
}

const ch = (channel: string, name: string): ChannelView => ({ channel, settings: { name, kind: 'text', parent: null, position: 0, overwrites: [] } }) as unknown as ChannelView;

const ITEMS = [item('M1', 'S1', 'C1', 3, [att('B1', '보고서.pdf', true), att('B2', '표.xlsx')]), item('M2', 'S1', 'C2', 2, [att('B3', '사진.png')]), item('M3', 'S2', 'C3', 1, [att('B4', '노래.mp3')])];

function setup() {
  const calls: Array<[string, unknown]> = [];
  const call = vi.fn(async (method: string, params: Record<string, unknown>) => {
    calls.push([method, params]);
    if (method === 'library.list') {
      const offset = Number(params.offset ?? 0);
      return { items: params.messages ? [] : ITEMS.slice(offset, offset + Number(params.limit)), total: ITEMS.length };
    }
    if (method === 'blob.pins') return [];
    return {};
  });
  useStore.setState({
    client: { call, transport: {} } as unknown as ApiClient,
    spaces: [
      { space: 'S1', kind: 'crew', name: '게임 모임' },
      { space: 'S2', kind: 'crew', name: '공부방' },
    ] as SpaceSummary[],
    views: {
      S1: { space: 'S1', members: [], channels: [ch('C1', '잡담'), ch('C2', '사진')] } as unknown as SpaceView,
      S2: { space: 'S2', members: [], channels: [ch('C3', '음악')] } as unknown as SpaceView,
    },
    friends: [],
    myDevice: 'ME',
    blobProgress: {},
    modal: null,
    prefs: { ...useStore.getState().prefs, library: { ...DEFAULT_LIBRARY, preview: false } },
  });
  return { call, calls };
}

const rows = (el: HTMLElement) => [...el.querySelectorAll('.lib-row .lib-name')].map((n) => n.textContent);

afterEach(async () => {
  await cleanup();
  useStore.setState({ client: null, views: {}, spaces: [], modal: null });
  resetPinsForTest();
  resetSharingForTest();
});

describe('라이브러리 탭', () => {
  it('파일마다 한 줄, 폴더를 고르면 그 안만', async () => {
    const { calls } = setup();
    const { el } = await render(<LibraryView />);
    await act(async () => undefined);
    expect(calls.filter(([m]) => m === 'library.list')).toEqual([['library.list', { offset: 0, limit: 500 }]]);
    expect(rows(el)).toEqual(['보고서.pdf', '표.xlsx', '사진.png', '노래.mp3']);
    const folder = [...el.querySelectorAll<HTMLButtonElement>('.lib-folder-btn')].find((b) => b.textContent?.includes('공부방'))!;
    await click(folder);
    expect(rows(el)).toEqual(['노래.mp3']);
    // 받지 않은 파일이 보여도 받기·상태를 묻지 않습니다.
    expect(calls.some(([m]) => m === 'blob.fetch' || m === 'blob.status')).toBe(false);
  });

  it('오른쪽 클릭 메뉴: 공유 중단은 메시지마다 한 번, 지우기는 확인 뒤 받은 파일만', async () => {
    const { call } = setup();
    const { el } = await render(<LibraryView />);
    await act(async () => undefined);
    const row = (name: string) => [...el.querySelectorAll<HTMLElement>('.lib-row')].find((r) => r.textContent?.includes(name))!;
    await click(row('보고서.pdf'));
    await act(async () => {
      row('표.xlsx').dispatchEvent(new MouseEvent('click', { bubbles: true, ctrlKey: true }));
    });
    await act(async () => {
      row('표.xlsx').dispatchEvent(new MouseEvent('contextmenu', { bubbles: true, clientX: 5, clientY: 5 }));
    });
    const menuButton = (text: string) => [...document.querySelectorAll<HTMLButtonElement>('.popover .menu-item')].find((b) => b.textContent?.includes(text))!;
    await click(menuButton('공유 중단'));
    await act(async () => undefined);
    const excludes = call.mock.calls.filter(([m]) => m === 'seed.exclude_message');
    expect(excludes).toEqual([['seed.exclude_message', { space: 'S1', channel: 'C1', id: 'M1', excluded: true }]]);
    expect(el.querySelectorAll('.lib-off').length).toBe(2);

    await act(async () => {
      row('표.xlsx').dispatchEvent(new MouseEvent('contextmenu', { bubbles: true, clientX: 5, clientY: 5 }));
    });
    await click(menuButton('이 기기에서 지우기'));
    const modal = useStore.getState().modal;
    expect(modal?.kind).toBe('confirm');
    await act(async () => {
      if (modal?.kind === 'confirm') await modal.onConfirm();
    });
    const evicts = call.mock.calls.filter(([m]) => m === 'blob.evict');
    expect(evicts).toEqual([['blob.evict', { space: 'S1', blob: 'B1' }]]);
  });

  it('여러 파일을 골라 보관하고 풀기: 파일마다 한 번, 이 기기에서 올린 파일은 빼고', async () => {
    const own = item('M4', 'S2', 'C3', 0, [att('B5', '내 파일.zip', true)]);
    own.message.author_device = 'ME';
    ITEMS.push(own);
    try {
      const { call } = setup();
      const { el } = await render(<LibraryView />);
      await act(async () => undefined);
      const row = (name: string) => [...el.querySelectorAll<HTMLElement>('.lib-row')].find((r) => r.textContent?.includes(name))!;
      await click(row('보고서.pdf'));
      await act(async () => {
        row('내 파일.zip').dispatchEvent(new MouseEvent('click', { bubbles: true, shiftKey: true }));
      });
      const menu = async () => {
        await act(async () => {
          row('사진.png').dispatchEvent(new MouseEvent('contextmenu', { bubbles: true, clientX: 5, clientY: 5 }));
        });
      };
      const menuButton = (text: string) => [...document.querySelectorAll<HTMLButtonElement>('.popover .menu-item')].find((b) => b.textContent?.includes(text));
      await menu();
      // 이 기기에 없는 파일은 보관하면서 받기 시작함을 알립니다.
      expect(menuButton('보관하기')?.textContent).toContain('받아서 보관');
      await click(menuButton('보관하기')!);
      await act(async () => undefined);
      const pins = (m: string) => call.mock.calls.filter(([x]) => x === m).map(([, p]) => (p as { blob: string }).blob);
      expect(pins('blob.pin')).toEqual(['B1', 'B2', 'B3', 'B4']);
      expect(el.querySelectorAll('.lib-kept').length).toBe(4);
      await menu();
      await click(menuButton('보관 풀기')!);
      await act(async () => undefined);
      expect(pins('blob.unpin')).toEqual(['B1', 'B2', 'B3', 'B4']);
      expect(el.querySelectorAll('.lib-kept').length).toBe(0);
    } finally {
      ITEMS.pop();
    }
  });

  it('두 번 누르면 화면 순서대로 뷰어를 열고, 여러 개를 골랐으면 고른 것만', async () => {
    setup();
    const { el } = await render(<LibraryView />);
    await act(async () => undefined);
    const row = (name: string) => [...el.querySelectorAll<HTMLElement>('.lib-row')].find((r) => r.textContent?.includes(name))!;
    const opened = () => {
      const m = useStore.getState().modal;
      return m?.kind === 'fileViewer' ? { keys: m.items.map((i) => i.key), types: m.items.map((i) => i.type), start: m.start } : null;
    };
    await act(async () => {
      row('사진.png').dispatchEvent(new MouseEvent('dblclick', { bubbles: true }));
    });
    expect(opened()).toEqual({ keys: ['M1:B1', 'M1:B2', 'M2:B3', 'M3:B4'], types: ['file', 'file', 'file', 'file'], start: 'M2:B3' });

    await click(row('보고서.pdf'));
    await act(async () => {
      row('노래.mp3').dispatchEvent(new MouseEvent('click', { bubbles: true, ctrlKey: true }));
    });
    await act(async () => {
      row('노래.mp3').dispatchEvent(new MouseEvent('dblclick', { bubbles: true }));
    });
    expect(opened()).toEqual({ keys: ['M1:B1', 'M3:B4'], types: ['file', 'file'], start: 'M3:B4' });
  });

  it('탭을 떠났다 돌아오면 다시 읽는다', async () => {
    const { calls } = setup();
    const first = await render(<LibraryView />);
    await first.unmount();
    await render(<LibraryView />);
    await act(async () => undefined);
    expect(calls.filter(([m, p]) => m === 'library.list' && !(p as { messages?: unknown }).messages)).toHaveLength(2);
  });
});
