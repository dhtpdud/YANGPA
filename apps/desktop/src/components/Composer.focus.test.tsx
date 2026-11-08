// @vitest-environment happy-dom
// 입력창: 답장 단추를 누르면 바로 쓸 수 있게 그 목록의 입력창이 커서를 잡습니다.

import { act } from 'react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import type { ApiClient } from '../api/client';
import type { ChannelView, MemberView, MessageView, SpaceView } from '../api/types';
import { resetPinsForTest } from '../hooks/pins';
import { useStore } from '../store/store';
import { cleanup, click, render } from '../test/dom';
import { Composer } from './Composer';
import { MessageItem, type MessageItemProps } from './MessageItem';

const ME = 'M'.repeat(32);
const OTHER = 'O'.repeat(32);

const member: MemberView = {
  user: OTHER,
  profile: { version: 1, name: '상대', bio: '', avatar: null, banner: null, theme_color: null, status: '' },
  roles: [],
  online: true,
  automation: false,
};

function msg(): MessageView {
  return {
    id: 'm1',
    author: OTHER,
    author_device: 'd',
    created_at: 1_700_000_000_000,
    lamport: 1,
    text: '답장할 글',
    attachments: [],
    embeds: [],
    reactions: [],
    deleted: false,
    volatile: false,
  };
}

function props(m: MessageView, volatile = false): MessageItemProps {
  return {
    m,
    grouped: false,
    space: 'S',
    channel: 'A',
    msgKey: 'S/A',
    members: new Map([[OTHER, member]]),
    roles: [],
    me: ME,
    volatile,
    canManage: false,
    canReact: true,
    isEditing: false,
  };
}

function channel(volatile: boolean): ChannelView {
  return {
    channel: 'A',
    settings: {
      name: '채널',
      kind: 'text',
      topic: '',
      parent: null,
      position: 0,
      security: volatile ? 'volatile' : 'normal',
      join_mode: 'always',
      ttl: volatile ? { mode: 'seconds', seconds: 300 } : { mode: 'none' },
      history: 'private',
      link_previews: false,
      overwrites: [],
      forum_tags: [],
    },
    unread: 0,
    api_read_allowed: false,
    p2p: false,
    p2p_joined: false,
  };
}

function setup(perms: string[] = ['send_messages']) {
  const transport = { pickFiles: vi.fn(), onFileDrop: () => () => undefined };
  const view = { space: 'S', members: [], emojis: [], my_permissions: perms } as unknown as SpaceView;
  useStore.setState({
    client: { call: vi.fn(async () => ({})), transport } as unknown as ApiClient,
    views: { S: view },
    me: ME,
    replyTo: {},
    composerFocus: null,
  });
}

const blur = () => (document.activeElement as HTMLElement | null)?.blur();

afterEach(async () => {
  await cleanup();
  resetPinsForTest();
  useStore.setState({ client: null, views: {}, replyTo: {}, composerFocus: null });
});

describe('답장 단추와 입력창 커서', () => {
  it('답장을 누르면 입력창에 커서가 가고, 같은 메시지에 다시 눌러도 다시 간다', async () => {
    setup();
    const { el } = await render(
      <>
        <MessageItem {...props(msg())} />
        <Composer space="S" channel="A" channelView={channel(false)} />
      </>,
    );
    const textarea = el.querySelector('textarea')!;
    blur();
    await click(el.querySelector<HTMLButtonElement>('button[title="답장"]')!);
    expect(document.activeElement).toBe(textarea);
    expect(el.querySelector('.reply-bar')).not.toBeNull();

    blur();
    await click(el.querySelector<HTMLButtonElement>('button[title="답장"]')!);
    expect(document.activeElement).toBe(textarea);
  });

  it('휘발성 채널 입력창도 같다', async () => {
    setup();
    const { el } = await render(
      <>
        <MessageItem {...props({ ...msg(), volatile: true }, true)} />
        <Composer space="S" channel="A" channelView={channel(true)} />
      </>,
    );
    blur();
    await click(el.querySelector<HTMLButtonElement>('button[title="답장"]')!);
    expect(document.activeElement).toBe(el.querySelector('textarea'));
  });

  it('다른 목록(글타래)의 답장에는 커서를 가져가지 않는다', async () => {
    setup();
    const { el } = await render(<Composer space="S" channel="A" thread="T" channelView={channel(false)} />);
    blur();
    act(() => useStore.getState().setReply('S/A', msg()));
    expect(document.activeElement).not.toBe(el.querySelector('textarea'));
  });

  it('보낼 권한이 없으면 커서를 옮기지 않는다', async () => {
    setup([]);
    const { el } = await render(<Composer space="S" channel="A" channelView={channel(false)} />);
    blur();
    act(() => useStore.getState().setReply('S/A', msg()));
    const textarea = el.querySelector('textarea');
    if (textarea) expect(document.activeElement).not.toBe(textarea);
  });
});
