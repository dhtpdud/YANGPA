// @vitest-environment happy-dom
// 뱃지: 프로필 팝업 이름 앞뒤에 고른 뱃지, 이름 아래에 모은 뱃지 전부. 설정에서는 두 개까지만 고를 수 있습니다.

import { act } from 'react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import type { ApiClient } from '../api/client';
import type { BadgeView } from '../api/types';
import { ModalRoot } from '../modals/Modal';
import { useStore } from '../store/store';
import { cleanup, render } from '../test/dom';
import { BadgePicker } from './Badges';

const ME = 'M'.repeat(32);
const PEER = 'P'.repeat(32);
const profile = { version: 1, name: '민지', bio: '', avatar: null, banner: null, theme_color: null, status: '' };

function setup(views: Record<string, BadgeView>) {
  const call = vi.fn(async (method: string, params?: unknown) => {
    const p = params as Record<string, unknown>;
    switch (method) {
      case 'profile.get':
        return profile;
      case 'badge.get':
        return views[p.user as string];
      case 'badge.set_shown':
        return { ...views[ME], shown: p.shown };
      default:
        return {};
    }
  });
  useStore.setState({ client: { call } as unknown as ApiClient, me: ME, friends: [], views: {}, spaces: [], modal: null, toasts: [], selected: { space: 'home', channel: null } });
  return call;
}

const settle = () => act(() => new Promise<void>((r) => setTimeout(r, 0)));
const titles = (el: Element | null) => [...(el?.querySelectorAll('[role="img"]') ?? [])].map((b) => b.getAttribute('title'));

afterEach(async () => {
  await cleanup();
  useStore.setState({ client: null, me: null, modal: null });
});

describe('뱃지', () => {
  it('프로필 이름 앞뒤에 고른 뱃지를, 이름 아래에 모은 뱃지를 모두 보인다', async () => {
    setup({
      [PEER]: {
        earned: ['founder', 'support14', 'friends5'],
        shown: [
          { badge: 'friends5', after: false },
          { badge: 'founder', after: true },
        ],
        can_grant_founder: false,
      },
    });
    useStore.setState({ modal: { kind: 'friendProfile', user: PEER } });
    const { el } = await render(<ModalRoot />);
    await settle();
    const name = el.querySelector('.pf-name')!;
    expect(titles(name)).toEqual(['친구 5명 이상', '원년멤버']);
    // 이름 글자는 두 뱃지 사이에 있고, 이름 옆 뱃지는 칩 없이 이모지 그대로입니다.
    expect(name.textContent).toBe('👋민지🧅');
    expect(name.querySelectorAll('.badge-icon')).toHaveLength(0);
    expect(titles(el.querySelector('.pf-badges'))).toEqual(['원년멤버', '$14 이상 후원', '친구 5명 이상']);
    expect(el.textContent).not.toContain('원년멤버 뱃지 주기');
  });

  it('개발자 계정이면 원년멤버 뱃지 주기 단추가 보인다', async () => {
    setup({ [ME]: { earned: [], shown: [], can_grant_founder: true } });
    useStore.setState({ modal: { kind: 'friendProfile', user: ME } });
    const { el } = await render(<ModalRoot />);
    await settle();
    expect(el.textContent).toContain('원년멤버 뱃지 주기');
  });

  it('설정: 모은 뱃지만 고를 수 있고 두 개를 고르면 나머지는 막힌다', async () => {
    const call = setup({
      [ME]: {
        earned: ['support3', 'friends5', 'seed1gb'],
        shown: [
          { badge: 'support3', after: false },
          { badge: 'friends5', after: true },
        ],
        can_grant_founder: false,
      },
    });
    const { el } = await render(<BadgePicker />);
    await settle();
    const selects = [...el.querySelectorAll('select')];
    // 30개 중 모은 3개만 고르기가 있습니다.
    expect(el.querySelectorAll('.badge-row')).toHaveLength(30);
    expect(selects).toHaveLength(3);
    const seed = selects[2];
    expect([...seed.options].filter((o) => o.disabled).map((o) => o.value)).toEqual(['before', 'after']);
    // 하나를 내리면 보낸 목록에서 빠집니다.
    await act(async () => {
      selects[0].value = 'none';
      selects[0].dispatchEvent(new Event('change', { bubbles: true }));
    });
    expect(call).toHaveBeenCalledWith('badge.set_shown', { shown: [{ badge: 'friends5', after: true }] });
  });
});
