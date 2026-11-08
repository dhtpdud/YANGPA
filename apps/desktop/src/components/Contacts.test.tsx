// @vitest-environment happy-dom
// 연락처 교환으로 만난 임시 연락처·만료된 연락처: 홈 오른쪽 현황은 친구·임시·만료 갈래로 사람과 접속 상태를 보이고, 임시 연락처의
// 친구 추가는 `friend.propose`로 가며 보낸 뒤에는 '요청 보냄'으로 잠깁니다. 사이드바에는 임시·만료 표시가, 1대1 대화에는 남은
// 시간 띠가 붙고, 만료된 연락처와의 대화는 입력창이 잠깁니다.

import { afterEach, describe, expect, it, vi } from 'vitest';
import type { ApiClient } from '../api/client';
import type { ContactKind, FriendView, SpaceSummary, SpaceView } from '../api/types';
import { useStore } from '../store/store';
import { button, cleanup, click, render } from '../test/dom';
import { Composer } from './Composer';
import { ContactSidePanel, HomeView } from './HomeView';
import { ContactStrip } from './Pane';
import { HomeSidebar } from './Sidebar';

const HOUR = 3600_000;
const profile = (name: string) => ({ version: 1, name, bio: '', avatar: null, banner: null, theme_color: null, status: '' });

function contact(user: string, name: string, kind: ContactKind, extra: Partial<FriendView> = {}): FriendView {
  return { user, profile: profile(name), space: `SP${user}`, online: true, safety_number_verified: false, kind, proposed: false, ...extra };
}

const MINJI = contact('U1', '민지', 'friend');
// 분 단위가 흘러도 '5시간 후 만료'로 보이게 여유를 둡니다.
const HAJUN = contact('U2', '하준', 'temporary', { expires_at: Date.now() + 5 * HOUR + 30 * 60_000 });
const YERIN = contact('U3', '예린', 'expired', { online: false });

function setup(friends: FriendView[]) {
  let list = friends;
  const call = vi.fn(async (method: string, params?: unknown) => {
    if (method === 'friend.propose') {
      const user = (params as { user: string }).user;
      list = list.map((f) => (f.user === user ? { ...f, proposed: true } : f));
      return { status: 'requested' };
    }
    if (method === 'friend.list') return list;
    if (method === 'friend.requests') return [];
    return {};
  });
  useStore.setState({ client: { call } as unknown as ApiClient, friends, requests: [], toasts: [], modal: null, homeTab: 'all' });
  return call;
}

afterEach(async () => {
  await cleanup();
  useStore.setState({ client: null, friends: [], spaces: [], views: {}, homeTab: 'online' });
});

describe('홈 오른쪽 연락처 현황', () => {
  it('친구·임시 연락처·만료된 연락처를 갈래별 사람 수와 접속 상태로 보인다', async () => {
    setup([MINJI, HAJUN, YERIN]);
    const { el } = await render(<ContactSidePanel />);
    const group = (label: string) => el.querySelector<HTMLElement>(`section[aria-label="${label}"]`)!;
    expect(group('친구').querySelector('h3')?.textContent).toBe('친구 1');
    expect(group('친구').textContent).toContain('민지');
    expect(group('임시 연락처').textContent).toContain('하준');
    expect(group('임시 연락처').textContent).toContain('온라인 · 5시간 후 만료');
    expect(group('임시 연락처').querySelector('.presence.on')).not.toBeNull();
    expect(group('만료된 연락처').querySelector('h3')?.textContent).toBe('만료된 연락처 1');
    expect(group('만료된 연락처').textContent).toContain('예린');
    expect(group('만료된 연락처').textContent).toContain('오프라인');
    expect(group('만료된 연락처').querySelector('.active-card.expired .presence.off')).not.toBeNull();
  });

  it('앱을 켠 뒤 첫 연결이 끝나지 않은 친구는 오프라인 대신 연결 중으로 보인다', async () => {
    setup([contact('U5', '서아', 'friend', { online: false }), contact('U6', '도윤', 'friend', { online: false })]);
    useStore.setState({ net: { connecting: ['U5'] } as never });
    const { el } = await render(<ContactSidePanel />);
    const cards = [...el.querySelectorAll<HTMLElement>('section[aria-label="친구"] .active-card')];
    const card = (name: string) => cards.find((c) => c.textContent?.includes(name))!;
    expect(card('서아').querySelector('.connecting .typing-dots')?.parentElement?.textContent).toBe('연결 중');
    expect(card('도윤').textContent).toContain('오프라인');
    useStore.setState({ net: null });
  });

  it('카드를 누르면 그 사람의 프로필이 열린다', async () => {
    setup([MINJI, YERIN]);
    const { el } = await render(<ContactSidePanel />);
    await click(el.querySelector<HTMLElement>('section[aria-label="만료된 연락처"] .active-card')!);
    expect(useStore.getState().modal).toEqual({ kind: 'friendProfile', user: 'U3' });
  });

  it('빈 갈래는 짧은 안내만 보이고, 만료 시각이 없으면 만료 없음으로 보인다', async () => {
    setup([contact('U4', '지우', 'temporary')]);
    const { el } = await render(<ContactSidePanel />);
    expect(el.querySelector('section[aria-label="친구"]')?.textContent).toContain('아직 친구가 없습니다.');
    expect(el.querySelector('section[aria-label="만료된 연락처"]')?.textContent).toContain('만료된 연락처가 없습니다.');
    expect(el.querySelector('section[aria-label="임시 연락처"]')?.textContent).toContain('만료 없음');
  });
});

describe('홈 친구 목록', () => {
  it('탭은 온라인·모두·대기 중·연락처 교환이고, 모두는 친구·임시·만료 갈래로 묶는다', async () => {
    setup([MINJI, HAJUN, YERIN]);
    const { el } = await render(<HomeView />);
    expect([...el.querySelectorAll('nav.tabs button')].map((b) => b.textContent?.trim())).toEqual(['온라인', '모두', '대기 중', '연락처 교환']);
    const list = el.querySelector<HTMLElement>('.friend-list')!;
    expect([...list.querySelectorAll('section')].map((s) => s.getAttribute('aria-label'))).toEqual(['친구', '임시 연락처', '만료된 연락처']);
    expect(list.querySelector('section[aria-label="임시 연락처"]')?.textContent).toContain('5시간 후 만료');
    expect(list.querySelector('section[aria-label="만료된 연락처"] .friend-row.expired')).not.toBeNull();
  });

  it('온라인 탭에는 만료된 연락처가 나오지 않는다', async () => {
    setup([MINJI, HAJUN, { ...YERIN, online: true }]);
    useStore.setState({ homeTab: 'online' });
    const { el } = await render(<HomeView />);
    const list = el.querySelector<HTMLElement>('.friend-list')!;
    expect(list.textContent).toContain('온라인 — 2');
    expect(list.textContent).not.toContain('예린');
  });

  it('임시 연락처의 친구 추가는 friend.propose를 부르고, 보낸 뒤에는 요청 보냄으로 잠긴다', async () => {
    const call = setup([MINJI, HAJUN, YERIN]);
    const { el } = await render(<HomeView />);
    const list = el.querySelector<HTMLElement>('.friend-list')!;
    // 친구·만료된 연락처에는 친구 추가 버튼이 없습니다.
    expect([...list.querySelectorAll('button')].filter((b) => b.textContent?.includes('친구 추가'))).toHaveLength(1);
    await click(button(list, '친구 추가'));
    expect(call).toHaveBeenCalledWith('friend.propose', { user: HAJUN.user });
    await vi.waitFor(() => expect(button(list, '요청 보냄').disabled).toBe(true));
    expect(useStore.getState().toasts.some((t) => t.text.includes('친구 요청을 보냈습니다'))).toBe(true);
  });
});

describe('사이드바·1대1 대화', () => {
  const summary = (f: FriendView): SpaceSummary => ({ space: f.space, kind: 'space', name: f.profile.name, unread: 0, mention: 0, dormant: false, member_count: 2 });

  it('홈 사이드바는 연락처 교환 버튼과 임시·만료 표시를 보인다', async () => {
    setup([MINJI, HAJUN, YERIN]);
    useStore.setState({ spaces: [MINJI, HAJUN, YERIN].map(summary), homeTab: 'online' });
    const { el } = await render(<HomeSidebar />);
    await click(button(el, '연락처 교환'));
    expect(useStore.getState().homeTab).toBe('add');
    const rows = [...el.querySelectorAll<HTMLElement>('.dm-row')];
    expect(rows.map((r) => r.querySelector('.tag')?.textContent ?? '')).toEqual(['', '임시', '만료']);
    expect(rows[2].classList.contains('expired')).toBe(true);
  });

  it('1대1 대화 상대가 임시 연락처면 남은 시간과 친구 추가를, 만료됐으면 읽기만 된다는 띠를 보인다', async () => {
    const call = setup([HAJUN, YERIN]);
    const view = (f: FriendView) => ({ space: f.space, kind: 'space', members: [] }) as unknown as SpaceView;
    const temp = await render(<ContactStrip space={view(HAJUN)} />);
    expect(temp.el.textContent).toContain('임시 연락처 · 5시간 후 만료');
    await click(button(temp.el, '친구 추가'));
    expect(call).toHaveBeenCalledWith('friend.propose', { user: HAJUN.user });
    const expired = await render(<ContactStrip space={view(YERIN)} />);
    expect(expired.el.textContent).toBe('만료된 연락처입니다. 새 연락처 링크를 받으면 다시 대화할 수 있습니다.');
    const friend = await render(<ContactStrip space={view(MINJI)} />);
    expect(friend.el.textContent).toBe('');
  });

  it('만료된 연락처와의 대화는 입력창이 잠긴다', async () => {
    const call = setup([HAJUN, YERIN]);
    // 입력창은 파일 끌어 놓기를 전송에 등록합니다.
    const transport = { onFileDrop: () => () => undefined };
    useStore.setState({ client: { call, transport } as unknown as ApiClient });
    const views = Object.fromEntries(
      [HAJUN, YERIN].map((f) => [f.space, { space: f.space, kind: 'space', my_permissions: ['administrator'], members: [], emojis: [] } as unknown as SpaceView]),
    );
    useStore.setState({ views, channels: {}, replyTo: {} });
    const expired = await render(<Composer space={YERIN.space} channel={'C'.repeat(22)} placeholder="보내기" />);
    const ta = expired.el.querySelector('textarea')!;
    expect(ta.disabled).toBe(true);
    expect(ta.getAttribute('placeholder')).toBe('만료된 연락처에는 메시지를 보낼 수 없습니다');
    const temp = await render(<Composer space={HAJUN.space} channel={'C'.repeat(22)} placeholder="보내기" />);
    expect(temp.el.querySelector('textarea')!.disabled).toBe(false);
  });
});
