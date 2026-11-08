// @vitest-environment happy-dom
// 신뢰 스위치: 상대 프로필의 ‘연결’(사람 신뢰·연결 방식, IP 주소는 보이지 않음)과 크루 설정의 ‘이 크루 신뢰’.
// 켤 때마다 IP 노출 경고를 연 창 위에 겹쳐 띄우고(아래 창의 입력은 그대로), 확인해야만 보냅니다. 끄기는 바로 보냅니다.
// ‘다시 보지 않기’를 고르고 확인하면 UI 설정(skipTrustWarning)에 저장해 다음부터 건너뜁니다.

import { act } from 'react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import type { ApiClient } from '../api/client';
import type { ConnectionMode, ContactKind, FriendView, SpaceKind, SpaceView } from '../api/types';
import { useStore } from '../store/store';
import { button, cleanup, click, render } from '../test/dom';
import { ModalRoot } from './Modal';
import { TRUST_POLL_MS } from './Trust';

const ME = 'M'.repeat(32);
const PEER = 'P'.repeat(32);
const DM = 'D'.repeat(32);
const CREW = 'C'.repeat(32);
const profile = (name: string) => ({ version: 1, name, bio: '', avatar: null, banner: null, theme_color: null, status: '' });
const member = (user: string, name: string) => ({ user, profile: profile(name), roles: [], online: true, automation: false });

function space(id: string, kind: SpaceKind, name: string, members: string[]): SpaceView {
  return {
    space: id,
    kind,
    settings: { name, description: '', icon: null, allow_external_emoji: false },
    owner: ME,
    committer: 'DEV',
    members: members.map((u) => member(u, u === ME ? '세영' : '민지')),
    roles: [],
    channels: [],
    emojis: [],
    my_permissions: ['administrator'],
    pending_commit: false,
    dormant: false,
    trusted: false,
  };
}

const friend = (kind: ContactKind): FriendView => ({
  user: PEER,
  profile: profile('민지'),
  space: DM,
  online: true,
  safety_number_verified: false,
  kind,
  proposed: false,
});

interface Setup {
  kind?: ContactKind | null;
  inCrew?: boolean;
  trusted?: boolean;
  via?: string[];
  connection?: ConnectionMode;
}

function setup(o: Setup = {}) {
  const st = { trusted: o.trusted ?? false, via: o.via ?? [], connection: o.connection ?? 'tor', crew: false };
  const views: Record<string, SpaceView> = { [DM]: space(DM, 'space', '민지', [ME, PEER]) };
  views[CREW] = space(CREW, 'crew', '게임 모임', o.inCrew === false ? [ME] : [ME, PEER]);
  const call = vi.fn(async (method: string, params?: unknown) => {
    const p = params as Record<string, unknown>;
    switch (method) {
      case 'profile.get':
        return profile('민지');
      case 'note.get':
        return { text: '' };
      case 'badge.get':
        return { earned: [], shown: [], can_grant_founder: false };
      case 'friend.safety_number':
        return { digits: '1'.repeat(60), verified: false };
      case 'trust.get':
        return { user: p.user, trusted: st.trusted, via_crews: st.via, connection: st.connection };
      case 'trust.set_user':
        st.trusted = !!p.enabled;
        st.connection = st.trusted ? 'one_way_me_exposed' : 'tor';
        return {};
      case 'trust.set_space':
        st.crew = !!p.enabled;
        return {};
      case 'space.get':
        return { ...views[p.space as string], trusted: st.crew };
      case 'space.list':
      case 'crew.committer_candidates':
        return [];
      default:
        return {};
    }
  });
  const savePrefs = vi.fn(async (_prefs: Record<string, unknown>) => undefined);
  const kind = o.kind === undefined ? 'friend' : o.kind;
  useStore.setState({
    client: { call, transport: { savePrefs } } as unknown as ApiClient,
    me: ME,
    friends: kind ? [friend(kind)] : [],
    views,
    spaces: [],
    modal: null,
    toasts: [],
    selected: { space: 'home', channel: null },
    prefs: { ...useStore.getState().prefs, skipTrustWarning: false },
  });
  return { call, savePrefs, st };
}

/** 데몬 응답(마이크로태스크)과 타이머 0을 흘려 화면을 맞춥니다. */
const settle = () => act(() => new Promise<void>((r) => setTimeout(r, 0)));
const toggle = (el: HTMLElement, label: string) => el.querySelector<HTMLButtonElement>(`button[role="switch"][aria-label="${label}"]`);
const overlays = () => document.querySelectorAll('.overlay').length;
const calls = (call: ReturnType<typeof vi.fn>, method: string) => call.mock.calls.filter(([m]) => m === method);

async function openProfile(user = PEER) {
  useStore.setState({ modal: { kind: 'friendProfile', user } });
  const r = await render(<ModalRoot />);
  await settle();
  // 신뢰 스위치는 설정 탭에 있습니다.
  const settings = [...r.el.querySelectorAll<HTMLButtonElement>('[role="tab"]')].find((b) => b.textContent === '설정');
  if (settings) await act(async () => settings.click());
  return r;
}

afterEach(async () => {
  vi.useRealTimers();
  await cleanup();
  useStore.setState({ client: null, me: null, friends: [], views: {}, modal: null });
});

describe('상대 프로필의 연결(사람 신뢰 스위치)', () => {
  it('신뢰 스위치와 연결 방식을 말로만 보이고 IP 주소는 보이지 않는다', async () => {
    setup({ connection: 'one_way_peer_exposed' });
    const { el } = await openProfile();
    const sw = toggle(el, '신뢰')!;
    expect(sw.getAttribute('aria-checked')).toBe('false');
    expect(el.textContent).toContain('연결 방식');
    expect(el.textContent).toContain('단방향: 내게 상대 IP가 보임(나는 숨음)');
    expect(el.textContent).toContain('켜면 이 사람이 이 기기의 IP 주소를 볼 수 있습니다');
    expect(el.textContent).not.toMatch(/\d{1,3}(\.\d{1,3}){3}|\/ip[46]\//);
  });

  it('켤 때는 프로필 위에 경고를 띄우고, 취소하면 보내지 않고 꺼진 채 프로필로 돌아간다', async () => {
    const { call } = setup();
    const { el } = await openProfile();
    await click(toggle(el, '신뢰')!);
    expect(useStore.getState().modal?.kind).toBe('trustWarning');
    // 프로필은 그대로 아래에 남습니다(다시 그리지 않음).
    expect(overlays()).toBe(2);
    expect(el.querySelector('.profile-modal')).not.toBeNull();
    expect(el.textContent).toContain('IP 노출 경고');
    expect(el.textContent).toContain('민지 님을 신뢰하면 그 사람이 이 기기의 IP 주소를 볼 수 있습니다.');
    expect(el.textContent).toContain('대략의 위치와 인터넷 회사(ISP)');
    expect(el.textContent).toContain('양파 앱은 IP 주소를 어디에도 보여 주지 않지만');
    await click(button(el, '취소'));
    await settle();
    expect(calls(call, 'trust.set_user')).toHaveLength(0);
    expect(useStore.getState().modal).toEqual({ kind: 'friendProfile', user: PEER });
    expect(overlays()).toBe(1);
    expect(toggle(el, '신뢰')!.getAttribute('aria-checked')).toBe('false');
    expect(calls(call, 'profile.get')).toHaveLength(1);
  });

  it('Esc는 맨 위 경고만 닫는다(취소로 보고 보내지 않음)', async () => {
    const { call } = setup();
    const { el } = await openProfile();
    await click(toggle(el, '신뢰')!);
    await act(async () => {
      document.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape' }));
    });
    expect(useStore.getState().modal).toEqual({ kind: 'friendProfile', user: PEER });
    expect(calls(call, 'trust.set_user')).toHaveLength(0);
  });

  it('확인하면 켜고 다시 읽으며, 끄기는 경고 없이 바로 보낸다', async () => {
    const { call } = setup();
    const { el } = await openProfile();
    await click(toggle(el, '신뢰')!);
    await click(button(el, '신뢰 켜기'));
    await settle();
    expect(call).toHaveBeenCalledWith('trust.set_user', { user: PEER, enabled: true });
    expect(useStore.getState().modal).toEqual({ kind: 'friendProfile', user: PEER });
    expect(toggle(el, '신뢰')!.getAttribute('aria-checked')).toBe('true');
    // 켠 뒤 trust.get을 다시 읽어 연결 방식도 따라옵니다.
    expect(calls(call, 'trust.get').length).toBeGreaterThanOrEqual(2);
    expect(el.textContent).toContain('단방향: 상대에게 내 IP가 보임(상대는 숨음)');
    // ‘다시 보지 않기’를 고르지 않았으므로 저장하지 않습니다.
    expect(useStore.getState().prefs.skipTrustWarning).toBe(false);

    await click(toggle(el, '신뢰')!);
    expect(useStore.getState().modal?.kind).toBe('friendProfile');
    await settle();
    expect(call).toHaveBeenCalledWith('trust.set_user', { user: PEER, enabled: false });
    expect(toggle(el, '신뢰')!.getAttribute('aria-checked')).toBe('false');
  });

  it('‘다시 보지 않기’를 고르고 확인하면 저장하고, 다음부터는 경고 없이 바로 켠다', async () => {
    const { call, savePrefs } = setup();
    const { el } = await openProfile();
    await click(toggle(el, '신뢰')!);
    const box = [...el.querySelectorAll<HTMLLabelElement>('label.check')].find((l) => l.textContent?.includes('다시 보지 않기'))!;
    await click(box.querySelector('input')!);
    await click(button(el, '신뢰 켜기'));
    await settle();
    expect(useStore.getState().prefs.skipTrustWarning).toBe(true);
    expect(savePrefs).toHaveBeenCalledWith(expect.objectContaining({ skipTrustWarning: true }));
    expect(call).toHaveBeenCalledWith('trust.set_user', { user: PEER, enabled: true });

    await click(toggle(el, '신뢰')!);
    await settle();
    await click(toggle(el, '신뢰')!);
    expect(useStore.getState().modal?.kind).toBe('friendProfile');
    await settle();
    expect(calls(call, 'trust.set_user').map(([, p]) => (p as { enabled: boolean }).enabled)).toEqual([true, false, true]);
  });

  it('열린 동안 몇 초마다 연결 방식을 다시 읽고, 닫으면 멈춘다', async () => {
    vi.useFakeTimers({ toFake: ['setInterval', 'clearInterval'] });
    const { call, st } = setup();
    const { el, unmount } = await openProfile();
    expect(el.textContent).toContain('Tor(서로 숨음)');
    const before = calls(call, 'trust.get').length;
    st.connection = 'p2p';
    await act(async () => {
      vi.advanceTimersByTime(TRUST_POLL_MS);
    });
    await settle();
    expect(calls(call, 'trust.get').length).toBe(before + 1);
    expect(el.textContent).toContain('P2P 직접 연결(서로 IP가 보임)');
    await unmount();
    vi.advanceTimersByTime(TRUST_POLL_MS * 3);
    expect(calls(call, 'trust.get').length).toBe(before + 1);
  });

  it('크루 신뢰로 이미 IP가 보이면 그 크루를 이름으로 알린다', async () => {
    setup({ via: [CREW] });
    const { el } = await openProfile();
    expect(el.textContent).toContain('크루 신뢰로 이 사람에게 이미 이 기기의 IP가 보입니다: 「게임 모임」');
  });

  it('나 자신, 함께 쓰는 공간이 없는 만료된 연락처에게는 구역을 숨긴다', async () => {
    setup();
    const mine = await openProfile(ME);
    expect(toggle(mine.el, '신뢰')).toBeNull();
    await mine.unmount();

    setup({ kind: 'expired', inCrew: false });
    const { el } = await openProfile();
    expect(toggle(el, '신뢰')).toBeNull();
    expect(el.textContent).not.toContain('연결 방식');
  });

  it('만료된 연락처라도 같은 크루 멤버면 보이고, 이미 켜 둔 신뢰는 끌 수 있게 남긴다', async () => {
    setup({ kind: 'expired', inCrew: true });
    const crewMate = await openProfile();
    expect(toggle(crewMate.el, '신뢰')).not.toBeNull();
    await crewMate.unmount();

    const { call } = setup({ kind: 'expired', inCrew: false, trusted: true });
    const { el } = await openProfile();
    expect(toggle(el, '신뢰')!.getAttribute('aria-checked')).toBe('true');
    expect(el.textContent).toContain('끄면 다시 켤 수 없습니다');
    await click(toggle(el, '신뢰')!);
    await settle();
    expect(call).toHaveBeenCalledWith('trust.set_user', { user: PEER, enabled: false });
    // 끈 뒤로는 켤 수 없으니 구역이 사라집니다.
    expect(toggle(el, '신뢰')).toBeNull();
  });
});

describe('크루 설정의 이 크루 신뢰', () => {
  async function openCrew() {
    useStore.setState({ modal: { kind: 'crewSettings', space: CREW } });
    const r = await render(<ModalRoot />);
    await settle();
    return r;
  }

  it('켜기는 경고를 거쳐 trust.set_space를 부르고 보기를 다시 읽으며, 크루 설정의 입력 중인 내용은 남는다', async () => {
    const { call } = setup();
    const { el } = await openCrew();
    expect(el.textContent).toContain('켜면 이 크루의 멤버가 이 기기의 IP 주소를 볼 수 있습니다');
    const name = el.querySelector<HTMLInputElement>('input.input')!;
    await act(async () => {
      Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')!.set!.call(name, '새 이름');
      name.dispatchEvent(new Event('input', { bubbles: true }));
    });
    expect(toggle(el, '이 크루 신뢰')!.getAttribute('aria-checked')).toBe('false');
    await click(toggle(el, '이 크루 신뢰')!);
    expect(useStore.getState().modal?.kind).toBe('trustWarning');
    expect(el.textContent).toContain('「게임 모임」 크루를 신뢰하면 이 크루의 멤버 모두가 이 기기의 IP 주소를 볼 수 있습니다.');
    expect(el.textContent).toContain('나중에 이 크루에 들어오는 멤버에게도 보입니다.');
    expect(calls(call, 'trust.set_space')).toHaveLength(0);
    await click(button(el, '신뢰 켜기'));
    expect(call).toHaveBeenCalledWith('trust.set_space', { space: CREW, enabled: true });
    expect(useStore.getState().modal).toEqual({ kind: 'crewSettings', space: CREW });
    expect(toggle(el, '이 크루 신뢰')!.getAttribute('aria-checked')).toBe('true');
    await vi.waitFor(() => expect(call).toHaveBeenCalledWith('space.get', { space: CREW }));
    expect(el.querySelector<HTMLInputElement>('input.input')!.value).toBe('새 이름');
  });

  it('취소하면 보내지 않고, 끄기는 경고 없이 바로 보낸다', async () => {
    const { call } = setup();
    const { el } = await openCrew();
    await click(toggle(el, '이 크루 신뢰')!);
    await click(button(el, '취소'));
    expect(calls(call, 'trust.set_space')).toHaveLength(0);
    expect(toggle(el, '이 크루 신뢰')!.getAttribute('aria-checked')).toBe('false');

    useStore.setState({ views: { ...useStore.getState().views, [CREW]: { ...useStore.getState().views[CREW], trusted: true } } });
    await settle();
    expect(toggle(el, '이 크루 신뢰')!.getAttribute('aria-checked')).toBe('true');
    await click(toggle(el, '이 크루 신뢰')!);
    expect(useStore.getState().modal?.kind).toBe('crewSettings');
    expect(call).toHaveBeenCalledWith('trust.set_space', { space: CREW, enabled: false });
  });
});
