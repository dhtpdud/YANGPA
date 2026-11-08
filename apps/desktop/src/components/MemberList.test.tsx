// @vitest-environment happy-dom
// 멤버 목록의 소유자 왕관(♛)은 관리자를 볼 수 있는 사람에게만 보입니다(`admins_hidden`이면 숨김).
// 데몬이 있으면 `member.search` 쪽으로 그리고 거르기를 데몬에 넘깁니다.

import { act } from 'react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import type { ApiClient } from '../api/client';
import type { MemberPage, MemberView, SpaceView } from '../api/types';
import { useStore } from '../store/store';
import { button, cleanup, click, render } from '../test/dom';
import { MemberList } from './MemberList';

const member = (user: string): MemberView => ({
  user,
  profile: { version: 1, name: user, bio: '', avatar: null, banner: null, theme_color: null, status: '' },
  roles: [],
  online: true,
  automation: false,
});

function space(admins_hidden?: boolean): SpaceView {
  return { space: 'S', kind: 'crew', owner: 'boss', members: [member('boss'), member('me')], roles: [], channels: [], admins_hidden } as unknown as SpaceView;
}

afterEach(async () => {
  await cleanup();
  useStore.setState({ client: null });
});

const settle = () => act(async () => new Promise((r) => setTimeout(r, 300)));

describe('멤버 목록의 소유자 표시', () => {
  it('보통은 소유자에게 왕관을 붙이고, 관리자를 볼 수 없으면 숨긴다', async () => {
    useStore.setState({ selected: { space: 'home', channel: null }, blocked: [] });
    const shown = await render(<MemberList space={space()} />);
    expect(shown.el.querySelectorAll('.crown')).toHaveLength(1);
    await shown.unmount();
    const hidden = await render(<MemberList space={space(true)} />);
    expect(hidden.el.textContent).toContain('boss');
    expect(hidden.el.querySelector('.crown')).toBeNull();
  });
});

describe('데몬 멤버 검색', () => {
  it('쪽을 받아 최근 묶음을 그리고, 거르기와 더 보기를 데몬에 묻는다', async () => {
    const recent: MemberView = { ...member('gone'), online: false, last_seen: Date.now() - 1000 };
    const call = vi.fn(async (_method: string, p: { offset?: number; filter?: string }): Promise<MemberPage> =>
      p.offset ? { members: [{ ...member('late'), online: false }], total: 3, online: 1 } : { members: [member('me'), recent], total: 3, online: 1 },
    );
    useStore.setState({ selected: { space: 'home', channel: null }, blocked: [], client: { call } as unknown as ApiClient });
    const r = await render(<MemberList space={space()} />);
    await settle();
    expect(call).toHaveBeenCalledWith('member.search', expect.objectContaining({ space: 'S', filter: 'all' }));
    expect(r.el.textContent).toContain('최근 7일 — 1');
    await click(button(r.el, '1명 더 보기'));
    await settle();
    expect(call).toHaveBeenLastCalledWith('member.search', expect.objectContaining({ offset: 2 }));
    expect(r.el.textContent).toContain('late');
    await click(button(r.el, '온라인'));
    await settle();
    expect(call).toHaveBeenLastCalledWith('member.search', expect.objectContaining({ filter: 'online' }));
  });
});
