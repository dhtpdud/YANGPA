// @vitest-environment happy-dom
// 포럼 채널 설정의 태그: 글자 하나씩(이모지도 이름 안에) 더하고 고치고 지운 뒤 저장하면 목록 전체를 `forum_tags`로 보냅니다.
// 20개까지, 같은 이름은 막고, 그 채널의 채널 관리 권한이 없으면 보이지 않습니다. 채널 만들기에서도 함께 보냅니다.

import { act } from 'react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import type { ApiClient } from '../api/client';
import { EVERYONE_ROLE, PERMISSION_BITS, type ChannelView, type ForumTag, type SpaceView } from '../api/types';
import { useStore } from '../store/store';
import { button, cleanup, click, render } from '../test/dom';
import { ChannelSettingsModal } from './ChannelSettings';

const SPACE = 'S'.repeat(32);
const CH = 'F'.repeat(32);
const ME = 'M'.repeat(32);
const MANAGE = 2 ** PERMISSION_BITS.find(([n]) => n === 'manage_channels')![1];

function forum(forum_tags: ForumTag[], overwrites: ChannelView['settings']['overwrites'] = []): ChannelView {
  return {
    channel: CH,
    settings: {
      name: '질문방',
      kind: 'forum',
      topic: '',
      parent: null,
      position: 0,
      security: 'normal',
      join_mode: 'always',
      ttl: { mode: 'none' },
      history: 'private',
      link_previews: true,
      overwrites,
      forum_tags,
    },
    unread: 0,
    api_read_allowed: false,
  } as unknown as ChannelView;
}

function setup(channels: ChannelView[], perms = ['administrator']) {
  const view = {
    space: SPACE,
    kind: 'crew',
    settings: { name: '크루' },
    roles: [],
    channels,
    members: [{ user: ME, profile: { name: '나' }, roles: [] }],
    my_permissions: perms,
  } as unknown as SpaceView;
  const call = vi.fn(async (method: string, _params?: unknown) => {
    if (method === 'channel.create') return { channel: 'NEW' };
    if (method === 'space.get') return view;
    return {};
  });
  useStore.setState({ client: { call } as unknown as ApiClient, views: { [SPACE]: view }, me: ME, modal: null, toasts: [] });
  return call;
}

const sent = (call: ReturnType<typeof setup>, method: string) => call.mock.calls.filter((c) => c[0] === method).map((c) => c[1] as Record<string, unknown>);
const section = (el: HTMLElement) => el.querySelector<HTMLElement>('section[aria-label="포럼 태그"]');
const save = (el: HTMLElement) => el.querySelector<HTMLButtonElement>('.modal-foot .btn.primary')!;
const q = (el: HTMLElement, label: string) => el.querySelector<HTMLInputElement>(`input[aria-label="${label}"]`)!;

async function typeInto(i: HTMLInputElement, value: string) {
  await act(async () => {
    Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')!.set!.call(i, value);
    i.dispatchEvent(new Event('input', { bubbles: true }));
  });
}

async function addTag(el: HTMLElement, name: string) {
  await typeInto(q(el, '새 태그 이름'), name);
  await click(button(section(el)!, '태그 추가'));
}

afterEach(async () => {
  await cleanup();
  useStore.setState({ client: null, views: {}, me: null, modal: null });
});

describe('포럼 태그 편집', () => {
  it('더하고 이름을 바꾸고 지운 뒤 저장하면 목록 전체를 보낸다(새 태그에는 이모지 없음)', async () => {
    const call = setup([forum([{ id: 7, name: '질문', emoji: '❓' }, { id: 9, name: '버그' }])]);
    const onClose = vi.fn();
    const { el } = await render(<ChannelSettingsModal space={SPACE} channel={CH} onClose={onClose} />);
    const sec = section(el)!;
    expect(sec).not.toBeNull();
    // 예전 이모지는 보이기만 하고 고치는 칸은 없습니다.
    expect(sec.querySelector('.ft-emoji')?.textContent).toBe('❓');
    expect(sec.querySelectorAll('input')).toHaveLength(3);
    await addTag(el, ' 🎉 공지 ');
    expect(q(el, '태그 이름 3').value).toBe('🎉 공지');
    await typeInto(q(el, '태그 이름 1'), '물어봐요');
    await click(sec.querySelector<HTMLButtonElement>('button[aria-label="버그 태그 지우기"]')!);
    expect(sec.textContent).toContain('2/20개');
    await click(save(el));
    // 저장해도 창은 닫지 않습니다.
    await vi.waitFor(() => expect(sent(call, 'channel.update')).toHaveLength(1));
    expect(onClose).not.toHaveBeenCalled();
    const [p] = sent(call, 'channel.update');
    const tags = p.forum_tags as ForumTag[];
    expect(tags).toHaveLength(2);
    expect(tags[0]).toEqual({ id: 7, name: '물어봐요', emoji: '❓' });
    expect(tags[1]).toEqual({ id: tags[1].id, name: '🎉 공지' });
    expect(tags[1].id).not.toBe(10);
    expect([7, 9, 0]).not.toContain(tags[1].id);
  });

  it('바꾸지 않으면 forum_tags를 보내지 않는다', async () => {
    const call = setup([forum([{ id: 1, name: '질문' }])]);
    const { el } = await render(<ChannelSettingsModal space={SPACE} channel={CH} onClose={() => undefined} />);
    await typeInto(el.querySelector<HTMLInputElement>('input.input')!, '질문방2');
    await click(save(el));
    await vi.waitFor(() => expect(sent(call, 'channel.update')).toHaveLength(1));
    expect(sent(call, 'channel.update')[0]).not.toHaveProperty('forum_tags');
  });

  it('같은 이름은 더할 수 없고, 고쳐서 겹치면 저장을 막는다', async () => {
    const call = setup([forum([{ id: 1, name: '질문' }, { id: 2, name: '버그' }])]);
    const { el } = await render(<ChannelSettingsModal space={SPACE} channel={CH} onClose={() => undefined} />);
    await addTag(el, '질문');
    expect(section(el)!.querySelector('.ft-err')?.textContent).toBe('같은 이름의 태그가 이미 있습니다');
    expect(section(el)!.querySelectorAll('input[aria-label^="태그 이름"]')).toHaveLength(2);
    await typeInto(q(el, '태그 이름 2'), '질문');
    expect(save(el).disabled).toBe(true);
    await typeInto(q(el, '태그 이름 2'), '');
    expect(section(el)!.textContent).toContain('태그 이름을 넣으세요');
    expect(save(el).disabled).toBe(true);
    expect(sent(call, 'channel.update')).toHaveLength(0);
  });

  it('20개면 더 더할 수 없다', async () => {
    setup([forum(Array.from({ length: 19 }, (_, i) => ({ id: i + 1, name: `t${i}` })))]);
    const { el } = await render(<ChannelSettingsModal space={SPACE} channel={CH} onClose={() => undefined} />);
    await addTag(el, '스무 번째');
    expect(section(el)!.textContent).toContain('20/20개');
    expect(q(el, '새 태그 이름').disabled).toBe(true);
    expect(button(section(el)!, '태그 추가').disabled).toBe(true);
  });

  it('그 채널의 채널 관리 권한이 없으면 보이지 않는다', async () => {
    // 크루에서는 채널 관리가 있지만 이 채널에서 @everyone 덮어쓰기로 막혔습니다.
    setup([forum([{ id: 1, name: '질문' }], [{ target: { type: 'role', id: EVERYONE_ROLE }, allow: 0, deny: MANAGE }])], ['view_channel', 'manage_channels']);
    const { el } = await render(<ChannelSettingsModal space={SPACE} channel={CH} onClose={() => undefined} />);
    expect(section(el)).toBeNull();
    await cleanup();
    setup([forum([{ id: 1, name: '질문' }])], ['view_channel', 'manage_channels']);
    const again = await render(<ChannelSettingsModal space={SPACE} channel={CH} onClose={() => undefined} />);
    expect(section(again.el)).not.toBeNull();
  });

  it('포럼 채널을 만들 때 태그를 함께 보낸다', async () => {
    const call = setup([]);
    const { el } = await render(<ChannelSettingsModal space={SPACE} onClose={() => undefined} />);
    expect(section(el)).toBeNull();
    await click(button(el, '포럼'));
    await typeInto(el.querySelector<HTMLInputElement>('input.input')!, '질문방');
    await addTag(el, '질문');
    await click(save(el));
    await vi.waitFor(() => expect(sent(call, 'channel.create')).toHaveLength(1));
    const tags = sent(call, 'channel.create')[0].forum_tags as ForumTag[];
    expect(tags.map((t) => t.name)).toEqual(['질문']);
    expect(tags[0]).not.toHaveProperty('emoji');
  });
});
