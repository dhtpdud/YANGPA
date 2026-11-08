// @vitest-environment happy-dom
// 채널 만들기·설정의 P2P 전용: 만들 때는 텍스트·음성(내 기기끼리 공간 밖)에만 스위치가 있고 `p2p: true`로 보냅니다.
// 만든 뒤에는 저장과 따로, 어느 쪽으로든 확인을 거쳐 `channel.set_p2p`를 부릅니다.
// 권한 탭: 비공개 스위치·역할별 덮어쓰기는 한 목록(`overwrites`)으로 보내고, 카테고리와 같던 채널은 앱이 함께 맞춥니다.
// 익명: 크루의, 카테고리가 아닌 채널에서 채널 관리 권한이 있을 때만 스위치가 있고, 통화 채널을 끌 때만 확인받습니다.

import { act } from 'react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import type { ApiClient } from '../api/client';
import type { ChannelView, SpaceKind, SpaceView } from '../api/types';
import { ANON_HINT, ANON_VOICE_OFF_WARNING } from '../lib/anon';
import { p2pOffWarning, p2pOnWarning, useStore } from '../store/store';
import { button, cleanup, click, render } from '../test/dom';
import { ChannelSettingsModal } from './ChannelSettings';

const SPACE = 'S'.repeat(32);
const CH = 'C'.repeat(32);

function voiceChannel(p2p: boolean): ChannelView {
  return {
    channel: CH,
    settings: {
      name: '통화',
      kind: 'voice',
      topic: '',
      parent: null,
      position: 0,
      security: 'normal',
      join_mode: 'always',
      ttl: { mode: 'none' },
      history: 'private',
      link_previews: true,
      overwrites: [],
      forum_tags: [],
    },
    unread: 0,
    api_read_allowed: false,
    p2p,
    p2p_joined: p2p,
  };
}

function setup(kind: SpaceKind = 'crew', channels: ChannelView[] = []) {
  const view = { space: SPACE, kind, roles: [], channels, members: [], my_permissions: ['administrator'] } as unknown as SpaceView;
  const call = vi.fn(async (method: string, _params?: unknown) => {
    if (method === 'channel.create') return { channel: 'NEW' };
    if (method === 'space.get') return view;
    return {};
  });
  useStore.setState({ client: { call } as unknown as ApiClient, views: { [SPACE]: view }, modal: null, toasts: [] });
  return call;
}

const p2pSwitch = (el: HTMLElement) => el.querySelector<HTMLButtonElement>('button[aria-label="P2P 전용"]');
const anonSwitch = (el: HTMLElement) => el.querySelector<HTMLButtonElement>('button[aria-label="익명"]');

afterEach(async () => {
  await cleanup();
  useStore.setState({ client: null, views: {}, modal: null, prefs: { ...useStore.getState().prefs, permTemplates: [] } });
});

describe('채널 만들기의 P2P 전용', () => {
  it('텍스트·음성에만 스위치가 있고 IP가 보인다는 안내를 붙인다', async () => {
    setup();
    const { el } = await render(<ChannelSettingsModal space={SPACE} onClose={() => undefined} />);
    expect(p2pSwitch(el)).not.toBeNull();
    expect(el.textContent).toContain('참가자끼리 서로 IP 주소가 보입니다');
    await click(button(el, '통화'));
    expect(p2pSwitch(el)).not.toBeNull();
    await click(button(el, '포럼'));
    expect(p2pSwitch(el)).toBeNull();
    // 카테고리는 채널 만들기 안에서 고르지 않습니다(사이드 메뉴의 '카테고리 만들기').
    expect([...el.querySelectorAll('.kind-card')].map((b) => b.textContent)).not.toContainEqual(expect.stringContaining('카테고리'));
  });

  it('카테고리 만들기는 종류 고르기 없이 카테고리를 만든다', async () => {
    const call = setup();
    const { el } = await render(<ChannelSettingsModal space={SPACE} initialKind="category" onClose={() => undefined} />);
    expect(el.textContent).toContain('카테고리 만들기');
    expect(el.querySelector('.kind-grid')).toBeNull();
    expect(p2pSwitch(el)).toBeNull();
    const name = el.querySelector<HTMLInputElement>('input.input')!;
    await act(async () => {
      const set = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')!.set!;
      set.call(name, '대화');
      name.dispatchEvent(new Event('input', { bubbles: true }));
    });
    await click(button(el, '만들기'));
    expect(call).toHaveBeenCalledWith('channel.create', expect.objectContaining({ kind: 'category', name: '대화' }));
  });

  it('내 기기끼리 공간에서는 스위치가 없다', async () => {
    setup('self_space');
    const { el } = await render(<ChannelSettingsModal space={SPACE} onClose={() => undefined} />);
    expect(p2pSwitch(el)).toBeNull();
  });

  it('켜고 만들면 p2p: true를 보낸다', async () => {
    const call = setup();
    const onClose = vi.fn();
    const { el } = await render(<ChannelSettingsModal space={SPACE} initialKind="voice" onClose={onClose} />);
    const name = el.querySelector<HTMLInputElement>('input.input')!;
    await act(async () => {
      const set = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')!.set!;
      set.call(name, '모임');
      name.dispatchEvent(new Event('input', { bubbles: true }));
    });
    await click(p2pSwitch(el)!);
    expect(p2pSwitch(el)!.getAttribute('aria-checked')).toBe('true');
    await click(button(el, '만들기'));
    expect(call).toHaveBeenCalledWith('channel.create', expect.objectContaining({ space: SPACE, kind: 'voice', name: '모임', p2p: true }));
  });
});

describe('채널 설정의 P2P 전용(channel.set_p2p)', () => {
  it('켜기는 멤버가 경고를 받아들여야 다시 쓸 수 있다고 확인받은 뒤 보낸다', async () => {
    const call = setup('crew', [voiceChannel(false)]);
    const { el } = await render(<ChannelSettingsModal space={SPACE} channel={CH} onClose={() => undefined} />);
    expect(p2pSwitch(el)!.getAttribute('aria-checked')).toBe('false');
    await click(p2pSwitch(el)!);
    expect(call).not.toHaveBeenCalledWith('channel.set_p2p', expect.anything());
    const m = useStore.getState().modal;
    if (m?.kind !== 'confirm') throw new Error('확인 창이 없음');
    expect(m.title).toBe('P2P 전용으로 바꾸기');
    expect(m.body).toBe(p2pOnWarning('통화'));
    await act(async () => {
      await m.onConfirm();
    });
    expect(call).toHaveBeenCalledWith('channel.set_p2p', { space: SPACE, channel: CH, enabled: true });
    await vi.waitFor(() => expect(call).toHaveBeenCalledWith('space.get', { space: SPACE }));
  });

  it('끄기(Tor로 되돌리기)는 기록이 Tor로 모두에게 가고 보인 IP는 못 숨긴다고 확인받은 뒤 보낸다', async () => {
    const call = setup('crew', [voiceChannel(true)]);
    const { el } = await render(<ChannelSettingsModal space={SPACE} channel={CH} onClose={() => undefined} />);
    expect(p2pSwitch(el)!.getAttribute('aria-checked')).toBe('true');
    expect(el.textContent).toContain('이미 보인 IP 주소는 다시 숨길 수 없습니다');
    await click(p2pSwitch(el)!);
    expect(call).not.toHaveBeenCalledWith('channel.set_p2p', expect.anything());
    const m = useStore.getState().modal;
    if (m?.kind !== 'confirm') throw new Error('확인 창이 없음');
    expect(m.title).toBe('Tor로 되돌리기');
    expect(m.body).toBe(p2pOffWarning('통화'));
    await act(async () => {
      await m.onConfirm();
    });
    expect(call).toHaveBeenCalledWith('channel.set_p2p', { space: SPACE, channel: CH, enabled: false });
    await vi.waitFor(() => expect(call).toHaveBeenCalledWith('space.get', { space: SPACE }));
  });
});

// ───────────── 권한 탭 ─────────────

const role = (id: string, name: string, rank: number) => ({ id, name, color: 0, rank, permissions: 0, automation: false });
const member = (user: string) => ({
  user,
  profile: { version: 1, name: user, bio: '', avatar: null, banner: null, theme_color: null, status: '' },
  roles: [],
  online: true,
  automation: false,
});
const DENY_VIEW = { target: { type: 'role' as const, id: '1111111111111111' }, allow: 0, deny: 1 };
const ALLOW_R1 = { target: { type: 'role' as const, id: 'r1' }, allow: 1, deny: 0 };

function chan(id: string, kind: ChannelView['settings']['kind'], parent: string | null, overwrites: ChannelView['settings']['overwrites']): ChannelView {
  const c = voiceChannel(false);
  return { ...c, channel: id, settings: { ...c.settings, name: id, kind, parent, overwrites } };
}

function permSetup(channels: ChannelView[] = [], my_permissions = ['administrator']) {
  const view = {
    space: SPACE,
    kind: 'crew',
    roles: [role('1111111111111111', '@everyone', 0), role('r1', '스태프', 5), role('r2', '손님', 1)],
    channels,
    members: [member('u1'), member('u2')],
    my_permissions,
  } as unknown as SpaceView;
  const call = vi.fn(async (method: string, _params?: unknown) => (method === 'channel.create' ? { channel: 'NEW' } : {}));
  useStore.setState({ client: { call } as unknown as ApiClient, views: { [SPACE]: view }, modal: null, toasts: [] });
  return call;
}

const tab = (el: HTMLElement, name: string) => click([...el.querySelectorAll<HTMLButtonElement>('[role="tab"]')].find((b) => b.textContent === name)!);
const tri = (el: HTMLElement, label: string) => el.querySelector<HTMLButtonElement>(`button[aria-label="${label}"]`)!;
/** 아래 저장·만들기(권한 탭의 '템플릿으로 저장'과 헷갈리지 않게). */
const submit = (el: HTMLElement) => el.querySelector<HTMLButtonElement>('.modal-foot .btn.primary')!;
const target = (el: HTMLElement, name: string) => click([...el.querySelectorAll<HTMLButtonElement>('.perm-target')].find((b) => b.textContent?.includes(name))!);

async function typeName(el: HTMLElement, value: string) {
  const input = el.querySelector<HTMLInputElement>('input.input')!;
  await act(async () => {
    Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')!.set!.call(input, value);
    input.dispatchEvent(new Event('input', { bubbles: true }));
  });
}

async function choose(select: HTMLSelectElement, value: string) {
  await act(async () => {
    Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype, 'value')!.set!.call(select, value);
    select.dispatchEvent(new Event('change', { bubbles: true }));
  });
}

describe('채널 권한 탭', () => {
  it('비공개 스위치와 역할 칩은 @everyone 채널 보기 거부·역할 허용과 같은 목록이고, overwrites로 만든다', async () => {
    const call = permSetup();
    const { el } = await render(<ChannelSettingsModal space={SPACE} onClose={() => undefined} />);
    await typeName(el, '비밀');
    await tab(el, '권한');
    await click(el.querySelector<HTMLButtonElement>('button[aria-label="비공개"]')!);
    expect(tri(el, '채널 보기 거부').getAttribute('aria-checked')).toBe('true');
    await click([...el.querySelectorAll<HTMLButtonElement>('.chip')].find((b) => b.textContent === '스태프')!);
    await target(el, '스태프');
    expect(tri(el, '채널 보기 허용').getAttribute('aria-checked')).toBe('true');
    await click(button(el, '만들기'));
    const params = call.mock.calls.find(([m]) => m === 'channel.create')![1] as Record<string, unknown>;
    expect(params.overwrites).toEqual([DENY_VIEW, ALLOW_R1]);
    expect(params).not.toHaveProperty('private_roles');
  });

  it('역할·멤버를 더해 거부·기본·허용을 고르고, 지우면 목록에서 빠지며, 내게 없는 권한은 고를 수 없다', async () => {
    const call = permSetup([chan('C1', 'text', null, [])], ['manage_channels', 'view_channel', 'send_messages']);
    const { el } = await render(<ChannelSettingsModal space={SPACE} channel="C1" onClose={() => undefined} />);
    await tab(el, '권한');
    expect([...el.querySelectorAll('.perm-target')].map((b) => b.textContent)).toEqual(['@everyone']);
    await choose(el.querySelector<HTMLSelectElement>('select[aria-label="역할·멤버 추가"]')!, 'member:u2');
    expect(el.querySelector('.perm-target.active')?.textContent).toContain('u2');
    await click(tri(el, '메시지 보내기 거부'));
    expect(tri(el, '반응 달기 허용').disabled).toBe(true);
    await choose(el.querySelector<HTMLSelectElement>('select[aria-label="역할·멤버 추가"]')!, 'role:r2');
    await click(tri(el, '메시지 보내기 허용'));
    await click(button(el, '이 설정 지우기'));
    expect([...el.querySelectorAll('.perm-target')].map((b) => b.textContent)).toEqual(['@everyone', 'u2']);
    await click(submit(el));
    expect(call).toHaveBeenCalledWith('channel.update', { space: SPACE, channel: 'C1', overwrites: [{ target: { type: 'member', id: 'u2' }, allow: 0, deny: 2 }] });
  });

  it('통화 채널은 음성 권한만, 카테고리는 모든 종류의 권한을 보인다', async () => {
    permSetup([chan('V', 'voice', null, []), chan('K', 'category', null, [])]);
    const voice = await render(<ChannelSettingsModal space={SPACE} channel="V" onClose={() => undefined} />);
    await tab(voice.el, '권한');
    expect(voice.el.querySelector('[aria-label="말하기"]')).not.toBeNull();
    expect(voice.el.querySelector('[aria-label="메시지 보내기"]')).toBeNull();
    await voice.unmount();
    const cat = await render(<ChannelSettingsModal space={SPACE} channel="K" onClose={() => undefined} />);
    await tab(cat.el, '권한');
    for (const l of ['말하기', '메시지 보내기', '포럼 글쓰기', '채널 관리']) expect(cat.el.querySelector(`[aria-label="${l}"]`), l).not.toBeNull();
    expect(cat.el.textContent).toContain('비공개 카테고리');
  });

  it('카테고리 안의 채널은 카테고리 권한과 같은지 보이고, 맞추기로 복사해 저장한다', async () => {
    const call = permSetup([chan('K', 'category', null, [DENY_VIEW, ALLOW_R1]), chan('C1', 'text', 'K', [])]);
    const { el } = await render(<ChannelSettingsModal space={SPACE} channel="C1" onClose={() => undefined} />);
    await tab(el, '권한');
    expect(el.textContent).toContain('카테고리 권한과 다릅니다');
    await click(button(el, '카테고리 권한으로 맞추기'));
    expect(el.textContent).toContain('카테고리 권한과 같습니다');
    expect(el.querySelector<HTMLButtonElement>('button[aria-label="비공개"]')!.getAttribute('aria-checked')).toBe('true');
    await click(submit(el));
    expect(call).toHaveBeenCalledWith('channel.update', { space: SPACE, channel: 'C1', overwrites: [DENY_VIEW, ALLOW_R1] });
  });

  it('카테고리 안에 만들면 카테고리 권한에서 시작하고, 권한을 손대기 전까지는 카테고리를 바꿀 때 다시 맞춘다', async () => {
    const call = permSetup([chan('K', 'category', null, [DENY_VIEW]), chan('K2', 'category', null, [ALLOW_R1])]);
    const { el } = await render(<ChannelSettingsModal space={SPACE} parent="K" onClose={() => undefined} />);
    await typeName(el, '새-채널');
    const category = () => el.querySelector<HTMLSelectElement>('select.select')!;
    await choose(category(), 'K2');
    await tab(el, '권한');
    expect(el.textContent).toContain('「K2」 카테고리 권한과 같습니다');
    await click(tri(el, '메시지 보내기 거부'));
    await tab(el, '일반');
    await choose(category(), 'K');
    await click(button(el, '만들기'));
    const params = call.mock.calls.find(([m]) => m === 'channel.create')![1] as Record<string, unknown>;
    expect(params).toMatchObject({ parent: 'K', overwrites: [ALLOW_R1, { target: DENY_VIEW.target, allow: 0, deny: 2 }] });
  });

  it('카테고리 권한을 바꾸면 카테고리 권한과 같던 채널만 따로 channel.update로 맞추고 몇 개인지 알린다', async () => {
    const call = permSetup([
      chan('K', 'category', null, [DENY_VIEW]),
      chan('A', 'text', 'K', [DENY_VIEW]),
      chan('B', 'voice', 'K', [DENY_VIEW]),
      chan('C', 'text', 'K', []),
    ]);
    const { el } = await render(<ChannelSettingsModal space={SPACE} channel="K" onClose={() => undefined} />);
    await tab(el, '권한');
    expect(el.textContent).toContain('카테고리 권한과 같은 채널 2개는 저장할 때 함께 맞춥니다');
    await click(el.querySelector<HTMLButtonElement>('button[aria-label="비공개"]')!);
    await click(submit(el));
    const updates = call.mock.calls.filter(([m]) => m === 'channel.update').map(([, p]) => p);
    expect(updates).toEqual([
      { space: SPACE, channel: 'K', overwrites: [] },
      { space: SPACE, channel: 'A', overwrites: [] },
      { space: SPACE, channel: 'B', overwrites: [] },
    ]);
    expect(useStore.getState().toasts.at(-1)?.text).toContain('따라오는 채널 2개');
  });

  it('기본 템플릿을 고른 대상에 쓰면 허용 밖의 이 채널 권한은 거부하고, 내게 없는 권한은 그대로 둔다', async () => {
    const call = permSetup([chan('C1', 'text', null, [])], ['manage_channels', 'view_channel', 'send_messages', 'read_history', 'add_reactions']);
    const { el } = await render(<ChannelSettingsModal space={SPACE} channel="C1" onClose={() => undefined} />);
    await tab(el, '권한');
    await choose(el.querySelector<HTMLSelectElement>('select[aria-label="역할·멤버 추가"]')!, 'role:r2');
    await click(tri(el, '템플릿 불러오기: 손님'));
    expect(el.textContent).toContain('「손님」 템플릿을 불러왔습니다. 저장해야 채널에 적용됩니다.');
    expect(el.textContent).toMatch(/내게 없는 권한 \d+개는 그대로 뒀습니다/);
    expect(tri(el, '기록 읽기 허용').getAttribute('aria-checked')).toBe('true');
    expect(tri(el, '메시지 보내기 거부').getAttribute('aria-checked')).toBe('true');
    expect(tri(el, '파일 첨부 기본').getAttribute('aria-checked')).toBe('true');
    await click(submit(el));
    // 보기·기록 읽기·반응 허용(1+4+8), 메시지 보내기(2)·채널 관리(2^17) 거부
    expect(call).toHaveBeenCalledWith('channel.update', {
      space: SPACE,
      channel: 'C1',
      overwrites: [{ target: { type: 'role', id: 'r2' }, allow: 13, deny: 2 + 2 ** 17 }],
    });
  });

  it('고른 대상의 지금 설정(허용·거부)을 템플릿으로 저장하고, 거부가 있는 템플릿은 나머지를 기본으로 둔다', async () => {
    permSetup([chan('V', 'voice', null, [])]);
    useStore.setState({ client: { ...useStore.getState().client!, transport: { savePrefs: vi.fn(async () => undefined) } } as unknown as ApiClient });
    const { el } = await render(<ChannelSettingsModal space={SPACE} channel="V" onClose={() => undefined} />);
    await tab(el, '권한');
    expect(el.textContent).toContain('불러오면 이 대상의 권한만 바뀌고, 저장해야 채널에 적용됩니다.');
    await click(tri(el, '통화 연결 허용'));
    await click(tri(el, '화면공유 거부'));
    const name = el.querySelector<HTMLInputElement>('input[aria-label="템플릿 이름"]')!;
    await act(async () => {
      Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')!.set!.call(name, '듣기만');
      name.dispatchEvent(new Event('input', { bubbles: true }));
    });
    await click(button(el, '지금 설정을 템플릿으로 저장'));
    expect(useStore.getState().prefs.permTemplates).toEqual([{ name: '듣기만', permissions: ['connect_voice'], deny: ['stream'] }]);
    await click(button(el, '모두 기본으로'));
    await click(tri(el, '말하기 거부'));
    await click(tri(el, '템플릿 불러오기: 듣기만'));
    expect(tri(el, '통화 연결 허용').getAttribute('aria-checked')).toBe('true');
    expect(tri(el, '화면공유 거부').getAttribute('aria-checked')).toBe('true');
    expect(tri(el, '말하기 기본').getAttribute('aria-checked')).toBe('true');
  });

  it('맞추기로 내게 없는 권한이 바뀌면 저장하지 않고 알린다', async () => {
    const call = permSetup([chan('K', 'category', null, [{ ...ALLOW_R1, allow: 2 ** 7 }]), chan('C1', 'voice', 'K', [])], ['manage_channels', 'view_channel']);
    const { el } = await render(<ChannelSettingsModal space={SPACE} channel="C1" onClose={() => undefined} />);
    await tab(el, '권한');
    await click(button(el, '카테고리 권한으로 맞추기'));
    expect(el.textContent).toContain('내게 없는 권한(통화 연결)');
    expect(submit(el).disabled).toBe(true);
    expect(call).not.toHaveBeenCalledWith('channel.update', expect.anything());
  });
});

describe('익명 채널 스위치', () => {
  const typeName = async (el: HTMLElement, value: string) => {
    const name = el.querySelector<HTMLInputElement>('input.input')!;
    await act(async () => {
      const set = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')!.set!;
      set.call(name, value);
      name.dispatchEvent(new Event('input', { bubbles: true }));
    });
  };

  it('크루의 텍스트·음성·포럼에는 안내와 함께 있고, 카테고리·크루가 아닌 공간·권한이 없으면 없다', async () => {
    setup();
    const { el } = await render(<ChannelSettingsModal space={SPACE} onClose={() => undefined} />);
    expect(anonSwitch(el)).not.toBeNull();
    expect(el.textContent).toContain(ANON_HINT);
    await click(button(el, '통화'));
    expect(anonSwitch(el)).not.toBeNull();
    await click(button(el, '포럼'));
    expect(anonSwitch(el)).not.toBeNull();
    await cleanup();

    const cat = await render(<ChannelSettingsModal space={SPACE} initialKind="category" onClose={() => undefined} />);
    expect(anonSwitch(cat.el)).toBeNull();
    await cleanup();

    for (const kind of ['space', 'self_space'] as const) {
      setup(kind);
      const r = await render(<ChannelSettingsModal space={SPACE} onClose={() => undefined} />);
      expect(anonSwitch(r.el)).toBeNull();
      await cleanup();
    }

    setup();
    const view = useStore.getState().views[SPACE];
    useStore.setState({ views: { [SPACE]: { ...view, my_permissions: ['send_messages'] } } });
    const noPerm = await render(<ChannelSettingsModal space={SPACE} onClose={() => undefined} />);
    expect(anonSwitch(noPerm.el)).toBeNull();
  });

  it('켜고 만들면 anonymous: true를, 끄고 만들면 anonymous 없이 보낸다', async () => {
    const call = setup();
    const { el } = await render(<ChannelSettingsModal space={SPACE} onClose={() => undefined} />);
    await typeName(el, '건의함');
    await click(anonSwitch(el)!);
    expect(anonSwitch(el)!.getAttribute('aria-checked')).toBe('true');
    await click(button(el, '만들기'));
    expect(call).toHaveBeenCalledWith('channel.create', expect.objectContaining({ kind: 'text', name: '건의함', anonymous: true }));
    await cleanup();

    const call2 = setup();
    const r = await render(<ChannelSettingsModal space={SPACE} onClose={() => undefined} />);
    await typeName(r.el, '잡담');
    await click(button(r.el, '만들기'));
    const params = call2.mock.calls.find(([m]) => m === 'channel.create')?.[1] as Record<string, unknown>;
    expect(params).not.toHaveProperty('anonymous');
  });

  it('만든 채널의 익명 켜기는 바로 보내고, 통화 채널 끄기는 통화 중인 사람이 모두 나간다고 확인받는다', async () => {
    const text = { ...voiceChannel(false), channel: 'T'.repeat(32), settings: { ...voiceChannel(false).settings, name: '글', kind: 'text' as const } };
    const voice = { ...voiceChannel(false), anonymous: true };
    const call = setup('crew', [text, voice]);
    const first = await render(<ChannelSettingsModal space={SPACE} channel={text.channel} onClose={() => undefined} />);
    expect(anonSwitch(first.el)!.getAttribute('aria-checked')).toBe('false');
    await click(anonSwitch(first.el)!);
    await vi.waitFor(() => expect(call).toHaveBeenCalledWith('channel.update', { space: SPACE, channel: text.channel, anonymous: true }));
    await cleanup();

    const second = await render(<ChannelSettingsModal space={SPACE} channel={CH} onClose={() => undefined} />);
    expect(anonSwitch(second.el)!.getAttribute('aria-checked')).toBe('true');
    await click(anonSwitch(second.el)!);
    expect(call).not.toHaveBeenCalledWith('channel.update', expect.objectContaining({ channel: CH }));
    const m = useStore.getState().modal;
    if (m?.kind !== 'confirm') throw new Error('확인 창이 없음');
    expect(m.body).toBe(ANON_VOICE_OFF_WARNING);
    await act(async () => {
      await m.onConfirm();
    });
    expect(call).toHaveBeenCalledWith('channel.update', { space: SPACE, channel: CH, anonymous: false });
    await vi.waitFor(() => expect(call).toHaveBeenCalledWith('space.get', { space: SPACE }));
  });
});
