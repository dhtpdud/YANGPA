// @vitest-environment happy-dom
// 메시지 목록: 이 앱에서 보낸 메시지는 보던 위치와 상관없이 맨 아래로 내립니다.
// 가상 스크롤은 가짜로 바꿔 맨 아래로 내리는 호출만 셉니다(실제 배치는 웹뷰에서 손으로 확인).

import { act } from 'react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type { MessageView } from '../api/types';
import { emptyChannel } from '../store/apply';
import { useStore } from '../store/store';
import { cleanup, render } from '../test/dom';
import { MessageList } from './MessageList';

const fake = vi.hoisted(() => {
  const state = { count: 0 };
  const v = {
    scrollToEnd: vi.fn(),
    scrollToIndex: vi.fn(),
    getVirtualItems: () => Array.from({ length: state.count }, (_, i) => ({ index: i, key: i, start: i * 40, size: 40, end: (i + 1) * 40, lane: 0 })),
    getTotalSize: () => state.count * 40,
    isAtEnd: () => false,
    measureElement: () => undefined,
  };
  return { state, v };
});

vi.mock('@tanstack/react-virtual', () => ({
  useVirtualizer: (o: { count: number }) => {
    fake.state.count = o.count;
    return fake.v;
  },
}));

vi.mock('./MessageItem', () => ({
  MessageItem: ({ m }: { m: MessageView }) => <div className="m">{m.text}</div>,
}));

const m = (id: string, lamport: number) =>
  ({ id, lamport, author: 'O', created_at: lamport * 1000, text: id, attachments: [], embeds: [], reactions: [], deleted: false, volatile: false }) as unknown as MessageView;

function setMessages(messages: MessageView[]) {
  act(() => useStore.setState({ channels: { 'S/A': { ...emptyChannel(), loaded: true, reachedStart: true, messages } } }));
}

beforeEach(() => {
  useStore.setState({ views: {}, me: 'M', sent: null, channels: {} });
});

afterEach(async () => {
  await cleanup();
  fake.v.scrollToEnd.mockClear();
  useStore.setState({ sent: null, channels: {} });
});

describe('보낸 메시지로 내리기', () => {
  it('응답이 먼저 오면 신호에 한 번, 내 메시지가 들어올 때 한 번 더 내리고, 남의 메시지에는 더 내리지 않는다', async () => {
    setMessages([m('m1', 1)]);
    await render(<MessageList space="S" channel="A" />);
    fake.v.scrollToEnd.mockClear();
    act(() => useStore.setState({ sent: { key: 'S/A', id: 'm2', at: Date.now() } }));
    expect(fake.v.scrollToEnd).toHaveBeenCalledTimes(1);
    setMessages([m('m1', 1), m('m2', 2)]);
    expect(fake.v.scrollToEnd).toHaveBeenCalledTimes(2);
    setMessages([m('m1', 1), m('m2', 2), m('m3', 3)]);
    expect(fake.v.scrollToEnd).toHaveBeenCalledTimes(2);
  });

  it('알림이 먼저 와서 이미 목록에 있으면 한 번만 내린다', async () => {
    setMessages([m('m1', 1), m('m2', 2)]);
    await render(<MessageList space="S" channel="A" />);
    fake.v.scrollToEnd.mockClear();
    act(() => useStore.setState({ sent: { key: 'S/A', id: 'm2', at: Date.now() } }));
    expect(fake.v.scrollToEnd).toHaveBeenCalledTimes(1);
    setMessages([m('m1', 1), m('m2', 2), m('m3', 3)]);
    expect(fake.v.scrollToEnd).toHaveBeenCalledTimes(1);
  });

  it('다른 채널·글타래에서 보낸 신호나 오래된 신호로는 움직이지 않는다', async () => {
    setMessages([m('m1', 1)]);
    await render(<MessageList space="S" channel="A" />);
    fake.v.scrollToEnd.mockClear();
    act(() => useStore.setState({ sent: { key: 'S/A/T', id: 'x', at: Date.now() } }));
    act(() => useStore.setState({ sent: { key: 'S/B', id: 'y', at: Date.now() } }));
    act(() => useStore.setState({ sent: { key: 'S/A', id: 'z', at: Date.now() - 6000 } }));
    expect(fake.v.scrollToEnd).not.toHaveBeenCalled();
  });
});
