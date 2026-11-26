// SPDX-License-Identifier: LicenseRef-YANGPA-Supporter
// @vitest-environment happy-dom
// 역할 편집의 구독자 역할 칸: 목록 전체를 보내므로 다른 역할의 항목(맡은 기기 포함 설정)은 그대로 실어 보내야 합니다.

import { afterEach, describe, expect, it, vi } from 'vitest';
import type { ApiClient } from '../api/client';
import type { Role, SpaceView } from '../api/types';
import { EVERYONE_ROLE } from '../api/types';
import { useStore } from '../store/store';
import { cleanup, click, render } from '../test/dom';
import { RoleSubscriberSection } from './SubscriberRoles';

const SPACE = 'S'.repeat(32);
const role = (id: string, name: string, rank: number): Role => ({ id, name, color: 0, rank, permissions: 0, automation: false });

afterEach(async () => {
  await cleanup();
  useStore.setState({ client: null, views: {}, me: null, myDevice: null });
});

describe('구독자 역할 칸', () => {
  it('한 역할의 경로를 켜면 다른 역할의 항목은 그대로 함께 보낸다', async () => {
    const view = {
      space: SPACE,
      kind: 'crew',
      settings: { name: '모임', description: '', icon: null, allow_external_emoji: false },
      owner: 'me',
      members: [],
      roles: [role(EVERYONE_ROLE, '@everyone', 0), role('r1', '후원자', 5), role('r2', '큰 후원자', 6)],
      channels: [],
      emojis: [],
      my_permissions: ['administrator'],
      subscriber_roles: [{ role: 'r2', providers: ['patreon'], holder: 'OTHER' }],
    } as unknown as SpaceView;
    const call = vi.fn(async (method: string, _params?: unknown) =>
      method === 'subrole.creator_status' ? { providers: ['github', 'patreon', 'sellapp'], links: [{ provider: 'github', name: 'me', needs_login: false }] } : {},
    );
    useStore.setState({ client: { call } as unknown as ApiClient, me: 'me', myDevice: 'MINE', views: { [SPACE]: view } });
    const { el } = await render(<RoleSubscriberSection space={SPACE} role={view.roles[1]} canManage />);
    const box = el.querySelector<HTMLInputElement>('input[aria-label="GitHub Sponsors 구독자에게 주기"]')!;
    // 이 기기에 연결하지 않은 경로는 켤 수 없습니다.
    expect(el.querySelector<HTMLInputElement>('input[aria-label="SellApp 구독자에게 주기"]')!.disabled).toBe(true);
    await click(box);
    expect(call).toHaveBeenCalledWith('crew.set_subscriber_roles', {
      space: SPACE,
      roles: [
        { role: 'r2', providers: ['patreon'] },
        { role: 'r1', providers: ['github'] },
      ],
    });
  });

  it('이 기기가 확인하는 역할에만 지금 업데이트 버튼이 있고, 누르면 그 크루를 맞춘다', async () => {
    const view = {
      space: SPACE,
      kind: 'crew',
      settings: { name: '모임', description: '', icon: null, allow_external_emoji: false },
      owner: 'me',
      members: [],
      roles: [role(EVERYONE_ROLE, '@everyone', 0), role('r1', '후원자', 5), role('r2', '큰 후원자', 6)],
      channels: [],
      emojis: [],
      my_permissions: ['administrator'],
      subscriber_roles: [
        { role: 'r1', providers: ['patreon'], holder: 'MINE' },
        { role: 'r2', providers: ['patreon'], holder: 'OTHER' },
      ],
    } as unknown as SpaceView;
    const call = vi.fn(async (method: string, _params?: unknown) =>
      method === 'subrole.creator_status' ? { providers: ['patreon'], links: [{ provider: 'patreon', name: 'me', needs_login: false }] } : {},
    );
    useStore.setState({ client: { call } as unknown as ApiClient, me: 'me', myDevice: 'MINE', views: { [SPACE]: view } });
    const syncButton = (el: HTMLElement) =>
      [...el.querySelectorAll('button')].find((b) => b.textContent?.includes('구독자 정보 지금 업데이트하기'));
    const other = await render(<RoleSubscriberSection space={SPACE} role={view.roles[2]} canManage />);
    expect(syncButton(other.el)).toBeUndefined();
    await cleanup();
    const mine = await render(<RoleSubscriberSection space={SPACE} role={view.roles[1]} canManage />);
    await click(syncButton(mine.el)!);
    expect(call).toHaveBeenCalledWith('crew.sync_subscriber_roles', { space: SPACE });
  });
});
