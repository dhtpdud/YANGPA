// @vitest-environment happy-dom
// 입력창: 휘발성 채널에 첨부를 고르면 보장과 한계(화면 캡처·운영체제·직접 저장)를 알립니다.

import { afterEach, describe, expect, it, vi } from 'vitest';
import type { ApiClient } from '../api/client';
import type { ChannelView, SpaceView } from '../api/types';
import { useStore } from '../store/store';
import { cleanup, click, render } from '../test/dom';
import { Composer, VOLATILE_ATTACH_NOTE } from './Composer';

const SPACE = 'S'.repeat(32);
const CH = 'C'.repeat(32);

function channel(volatile: boolean): ChannelView {
  return {
    channel: CH,
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

function setup() {
  const pickFiles = vi.fn(async () => [{ path: '/사진/고양이.png', name: '고양이.png' }]);
  const transport = { pickFiles, onFileDrop: () => () => undefined };
  const view = { space: SPACE, members: [], emojis: [], my_permissions: ['send_messages', 'attach_files'] } as unknown as SpaceView;
  useStore.setState({
    client: { call: vi.fn(async () => ({})), transport } as unknown as ApiClient,
    views: { [SPACE]: view },
    toasts: [],
  });
  return pickFiles;
}

afterEach(async () => {
  await cleanup();
  useStore.setState({ client: null, views: {} });
});

describe('휘발성 채널 첨부 안내', () => {
  it('휘발성 채널에 파일을 고르면 보장과 한계를 알린다', async () => {
    const pickFiles = setup();
    const { el } = await render(<Composer space={SPACE} channel={CH} channelView={channel(true)} />);
    expect(el.textContent).not.toContain(VOLATILE_ATTACH_NOTE);
    await click(el.querySelector<HTMLButtonElement>('button[title="파일 첨부(메모리에만 보관)"]')!);
    expect(pickFiles).toHaveBeenCalled();
    await vi.waitFor(() => expect(el.textContent).toContain('고양이.png'));
    expect(el.textContent).toContain(VOLATILE_ATTACH_NOTE);
    expect(VOLATILE_ATTACH_NOTE).toContain('화면 캡처');
    expect(VOLATILE_ATTACH_NOTE).toContain('직접 저장');
  });

  it('일반 채널에는 안내가 없다', async () => {
    setup();
    const { el } = await render(<Composer space={SPACE} channel={CH} channelView={channel(false)} />);
    await click(el.querySelector<HTMLButtonElement>('button[title="파일 첨부"]')!);
    await vi.waitFor(() => expect(el.textContent).toContain('고양이.png'));
    expect(el.textContent).not.toContain(VOLATILE_ATTACH_NOTE);
  });
});
