// @vitest-environment happy-dom
// 오른쪽 클릭 메뉴: 크루 메뉴(읽음 표시·알림 끄기·알림 설정·신뢰 스위치·나가기·해산·ID 복사), 채널 메뉴(휘발성 채널은 알림 규칙·ID 복사 없음),
// 알림을 끈 채널은 흐리게 그리고 안 읽음 표시를 하지 않음, 사람 메뉴(신뢰 스위치는 trust.get을 읽은 뒤, 익명 자리는 메뉴 없음).

import { act, type MouseEvent } from 'react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import type { ApiClient } from '../api/client';
import { ANONYMOUS_USER, type ChannelView, type SpaceView } from '../api/types';
import { MUTE_FOREVER } from '../lib/notify';
import { useStore } from '../store/store';
import { button, cleanup, click, render } from '../test/dom';
import { PersonMenu, openPersonMenu } from './menus';
import { ChannelRow, SpaceMenu } from './Sidebar';

const initial = useStore.getState();

afterEach(async () => {
  await cleanup();
  useStore.setState({
    client: null,
    me: null,
    views: {},
    friends: [],
    prefs: { ...initial.prefs, notifyRules: {} },
    setTrust: initial.setTrust,
    markRead: initial.markRead,
    modal: null,
  });
});

const chan = (id: string, over: Partial<ChannelView['settings']> = {}, unread = 0) =>
  ({
    channel: id,
    unread,
    api_read_allowed: false,
    p2p: false,
    p2p_joined: false,
    settings: { name: id, kind: 'text', topic: '', security: 'normal', join_mode: 'always', ttl: { mode: 'none' }, overwrites: [], ...over },
  }) as unknown as ChannelView;

const crew = (over: Partial<SpaceView> = {}) =>
  ({
    space: 'S',
    kind: 'crew',
    owner: 'owner',
    trusted: false,
    members: [
      { user: 'me', roles: [] },
      { user: 'u2', roles: [] },
    ],
    my_permissions: ['create_invite'],
    channels: [chan('C1', {}, 3), chan('C2')],
    settings: { name: '양파' },
    ...over,
  }) as unknown as SpaceView;

/** 포털로 붙는 메뉴는 document.body에서 찾습니다. */
const body = () => document.body;
const rules = () => useStore.getState().prefs.notifyRules;

async function openSpaceMenu(space = crew()) {
  return render(<SpaceMenu space={space} anchor={new DOMRect(0, 0, 0, 0)} onClose={() => undefined} />);
}

describe('크루 메뉴', () => {
  it('읽음 표시는 안 읽은 채널마다, 안 읽은 채널이 없으면 누를 수 없다', async () => {
    const markRead = vi.fn();
    useStore.setState({ markRead });
    await openSpaceMenu();
    await click(button(body(), '읽음으로 표시하기'));
    expect(markRead).toHaveBeenCalledWith('S');
    await cleanup();
    await openSpaceMenu(crew({ channels: [chan('C2')] }));
    expect(button(body(), '읽음으로 표시하기').disabled).toBe(true);
  });

  it('알림 끄기는 고른 시간 동안, 알림 켜기로 되돌린다', async () => {
    const before = Date.now();
    await openSpaceMenu();
    await click(button(body(), '크루 알림 끄기'));
    await click(button(body(), '1시간 동안'));
    expect(rules().S.muteUntil).toBeGreaterThanOrEqual(before + 3_600_000);
    await cleanup();
    await openSpaceMenu();
    expect(body().textContent).toContain('까지 끔');
    await click(button(body(), '크루 알림 켜기'));
    expect(rules()).toEqual({});
    await cleanup();
    await openSpaceMenu();
    await click(button(body(), '크루 알림 끄기'));
    await click(button(body(), '직접 켤 때까지'));
    expect(rules().S.muteUntil).toBe(MUTE_FOREVER);
  });

  it('알림 설정은 기본값을 따르거나 크루만 따로 고른다', async () => {
    await openSpaceMenu();
    // 지금 값이 부제로 보입니다(기본 설정은 모든 메시지).
    expect(button(body(), '알림 설정').textContent).toContain('모든 메시지');
    await click(button(body(), '알림 설정'));
    await click(button(body(), '@멘션만'));
    expect(rules().S).toEqual({ level: 'mentions' });
    await cleanup();
    await openSpaceMenu();
    await click(button(body(), '알림 설정'));
    await click(button(body(), '기본값 따르기'));
    expect(rules()).toEqual({});
  });

  it('신뢰 스위치는 지금 값을 보이고 누르면 바꾼다(신뢰 전 데몬이면 없음)', async () => {
    const setTrust = vi.fn(async () => true);
    useStore.setState({ setTrust });
    await openSpaceMenu();
    const sw = button(body(), '이 크루 신뢰');
    expect(sw.getAttribute('aria-checked')).toBe('false');
    await click(sw);
    expect(setTrust).toHaveBeenCalledWith({ type: 'space', space: 'S', name: '양파' }, true);
    await cleanup();
    await openSpaceMenu(crew({ trusted: undefined as unknown as boolean }));
    expect(body().textContent).not.toContain('이 크루 신뢰');
  });

  it('소유자는 나가기 대신 해산하기, 멤버는 나가기, 해산한 크루는 소유자도 나가기만', async () => {
    useStore.setState({ me: 'owner' });
    await openSpaceMenu();
    expect(body().textContent).toContain('크루 해산하기');
    expect(body().textContent).not.toContain('크루 나가기');
    await cleanup();
    useStore.setState({ me: 'me' });
    await openSpaceMenu();
    expect(body().textContent).toContain('크루 나가기');
    expect(body().textContent).not.toContain('크루 해산하기');
    await cleanup();
    useStore.setState({ me: 'owner' });
    await openSpaceMenu(crew({ disbanded: true }));
    expect(body().textContent).toContain('크루 나가기');
    expect(body().textContent).not.toContain('크루 해산하기');
  });

  it('해산하기는 확인 뒤 crew.disband를 부른다', async () => {
    const call = vi.fn(async (method: string) => (method === 'space.list' ? [] : {}));
    useStore.setState({ me: 'owner', client: { call } as unknown as ApiClient });
    await openSpaceMenu();
    await click(button(body(), '크루 해산하기'));
    const modal = useStore.getState().modal;
    expect(modal?.kind).toBe('confirm');
    await act(async () => {
      if (modal?.kind === 'confirm') await modal.onConfirm();
    });
    expect(call).toHaveBeenCalledWith('crew.disband', { space: 'S' });
  });

  it('크루 ID를 복사한다', async () => {
    const writeText = vi.fn(async () => undefined);
    Object.defineProperty(navigator, 'clipboard', { value: { writeText }, configurable: true });
    await openSpaceMenu();
    await click(button(body(), '크루 ID 복사하기'));
    expect(writeText).toHaveBeenCalledWith('S');
  });
});

describe('채널 메뉴', () => {
  const contextMenu = async (el: HTMLElement) =>
    act(async () => {
      el.querySelector('.ch-row')!.dispatchEvent(new window.MouseEvent('contextmenu', { bubbles: true, clientX: 5, clientY: 5 }));
    });

  it('글 채널은 알림 끄기·설정과 ID 복사가 있고, 휘발성 채널은 없다', async () => {
    const { el } = await render(<ChannelRow space={crew()} c={chan('C1', {}, 3)} active={false} />);
    await contextMenu(el);
    for (const t of ['읽음으로 표시하기', '채널 알림 끄기', '알림 설정', '채널 ID 복사하기']) expect(body().textContent).toContain(t);
    await cleanup();
    const v = await render(<ChannelRow space={crew()} c={chan('V', { security: 'volatile' })} active={false} />);
    await contextMenu(v.el);
    expect(body().textContent).toContain('읽음으로 표시하기');
    expect(body().textContent).not.toContain('채널 알림 끄기');
    expect(body().textContent).not.toContain('채널 ID 복사하기');
  });

  it('채널 알림 설정의 기본은 크루 규칙을 따르고, 끈 채널은 흐리고 안 읽음 표시가 없다', async () => {
    useStore.setState({ prefs: { ...useStore.getState().prefs, notifyRules: { S: { level: 'off' } } } });
    const { el } = await render(<ChannelRow space={crew()} c={chan('C1', {}, 3)} active={false} />);
    expect(el.querySelector('.ch-badge')?.textContent).toBe('3');
    await contextMenu(el);
    await click(button(body(), '알림 설정'));
    expect(body().textContent).toContain('크루 설정 따르기 (알림 없음)');
    await cleanup();
    useStore.setState({ prefs: { ...useStore.getState().prefs, notifyRules: { 'S/C1': { muteUntil: MUTE_FOREVER } } } });
    const muted = await render(<ChannelRow space={crew()} c={chan('C1', {}, 3)} active={false} />);
    expect(muted.el.querySelector('.ch-row')?.classList.contains('muted')).toBe(true);
    expect(muted.el.querySelector('.ch-row')?.classList.contains('unread')).toBe(false);
    expect(muted.el.querySelector('.ch-badge')).toBeNull();
  });
});

describe('사람 메뉴', () => {
  const at = { preventDefault: () => undefined, clientX: 5, clientY: 5 } as unknown as MouseEvent;

  it('함께 쓰는 공간이 있으면 신뢰 스위치를 trust.get 값으로 보이고, 누르면 바꾼다', async () => {
    const call = vi.fn(async () => ({ user: 'u2', trusted: false, via_crews: [], connection: 'tor' }));
    const setTrust = vi.fn(async () => true);
    useStore.setState({ client: { call } as unknown as ApiClient, me: 'me', views: { S: crew() }, setTrust });
    await render(<PersonMenu />);
    await act(async () => openPersonMenu(at, 'u2', '민지'));
    expect(call).toHaveBeenCalledWith('trust.get', { user: 'u2' });
    const sw = button(body(), '신뢰');
    expect(sw.getAttribute('aria-checked')).toBe('false');
    await click(sw);
    expect(setTrust).toHaveBeenCalledWith({ type: 'user', user: 'u2', name: '민지' }, true);
    // 누르면 메뉴는 닫힙니다(켤 때 뜨는 경고 창과 겹치지 않게).
    expect(body().textContent).not.toContain('프로필 보기');
  });

  it('함께 쓰는 공간이 없으면 신뢰 스위치 없이 프로필과 ID 복사만, 익명 자리는 메뉴가 없다', async () => {
    const call = vi.fn(async () => ({ user: 'x', trusted: false, via_crews: [], connection: 'offline' }));
    useStore.setState({ client: { call } as unknown as ApiClient, me: 'me', views: { S: crew() } });
    await render(<PersonMenu />);
    await act(async () => openPersonMenu(at, 'x', '모름'));
    expect(body().textContent).toContain('프로필 보기');
    expect(body().textContent).toContain('사용자 ID 복사하기');
    expect([...body().querySelectorAll('[role="menuitemcheckbox"]')]).toHaveLength(0);
    await click(button(body(), '프로필 보기'));
    await act(async () => openPersonMenu(at, ANONYMOUS_USER, '익명'));
    expect(body().textContent).not.toContain('프로필 보기');
  });
});
