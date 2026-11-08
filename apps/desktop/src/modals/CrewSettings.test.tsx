// @vitest-environment happy-dom
// 역할 편집: 표시(이 역할을 볼 수 있는 사람·관리자 보기)는 저장 흐름에 함께 들어가 바뀐 값만 보내고, 권한 템플릿은 저장 전 스위치만
// 바꿉니다(내게 없는 권한은 그대로). 관리자를 볼 수 없는 사람에게는 소유자 표시를 숨깁니다.
// 크루 STUN 서버: 크루 관리 권한이 있어 목록이 오는 사람만 고치고(crew.set_servers), 쓸 수 있는 크루 서버가 있는 멤버는
// 이 기기에서 쓸지 스스로 켭니다(crew.use_servers).

import { act } from 'react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { RpcError, type ApiClient } from '../api/client';
import type { MemberView, Role, SpaceView } from '../api/types';
import { EVERYONE_ROLE, ErrorCode } from '../api/types';
import { permissionBits } from '../lib/misc';
import { useStore } from '../store/store';
import { button, cleanup, click, render } from '../test/dom';
import { CREW_SERVERS_MEMBER_NOTE, CREW_SERVERS_USE_HINT } from './CrewServers';
import { CrewSettingsModal } from './CrewSettings';

const SPACE = 'S'.repeat(32);

const role = (id: string, name: string, rank: number, permissions: string[] = []): Role => ({
  id,
  name,
  color: 0,
  rank,
  permissions: permissionBits(permissions),
  automation: false,
});

const member = (user: string, roles: string[]): MemberView => ({
  user,
  profile: { version: 1, name: user, bio: '', avatar: null, banner: null, theme_color: null, status: '' },
  roles,
  online: true,
  automation: false,
});

function setup(over: Partial<SpaceView> = {}) {
  const view = {
    space: SPACE,
    kind: 'crew',
    settings: { name: '모임', description: '', icon: null, allow_external_emoji: false },
    owner: 'owner',
    committer: 'D'.repeat(32),
    members: [member('owner', []), member('me', ['top']), member('guest', ['r2'])],
    roles: [role(EVERYONE_ROLE, '@everyone', 0), role('top', '운영진장', 10), role('r1', '스태프', 5, ['view_channel']), role('r2', '손님', 1)],
    channels: [],
    emojis: [],
    my_permissions: ['administrator'],
    pending_commit: false,
    dormant: false,
    trusted: false,
    ...over,
  } as SpaceView;
  // 앞 시험이 건 공간 새로 읽기(80ms 뒤)가 이 시험 안에서 돌아도 보기를 빈 값으로 덮지 않게 space.get은 지금 보기를 돌려줍니다.
  const call = vi.fn(async (method: string, _params?: unknown) => (method === 'space.get' ? useStore.getState().views[SPACE] : {}));
  const savePrefs = vi.fn(async () => undefined);
  useStore.setState({
    client: { call, transport: { savePrefs } } as unknown as ApiClient,
    me: 'me',
    views: { [SPACE]: view },
    modal: null,
    toasts: [],
    prefs: { ...useStore.getState().prefs, permTemplates: [] },
  });
  return { call, savePrefs };
}

const sw = (el: HTMLElement, label: string) => el.querySelector<HTMLButtonElement>(`button[aria-label="${label}"]`);
/** 표시 칸의 역할 칩(역할 목록·템플릿과 이름이 겹치므로 따로 찾음). */
const chip = (el: HTMLElement, name: string) => [...el.querySelectorAll<HTMLButtonElement>('.perm-block .chip')].find((b) => b.textContent === name)!;
const pick = (el: HTMLElement, name: string) => click([...el.querySelectorAll<HTMLButtonElement>('.role-pick')].find((b) => b.textContent?.includes(name))!);
const save = (el: HTMLElement) => click(el.querySelector<HTMLButtonElement>('.save-bar .btn.primary')!);

async function typeInto(input: HTMLInputElement, value: string) {
  await act(async () => {
    Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')!.set!.call(input, value);
    input.dispatchEvent(new Event('input', { bubbles: true }));
  });
}

afterEach(async () => {
  await cleanup();
  useStore.setState({ client: null, views: {}, modal: null, me: null });
});

describe('역할 표시 설정', () => {
  it('고른 역할만 보게 하고 관리자 보기를 켜면 같은 role.update에 함께 보낸다', async () => {
    const { call } = setup();
    // 내가 고칠 수 있는 맨 위 역할(스태프)이 열립니다.
    const { el } = await render(<CrewSettingsModal space={SPACE} tab="roles" onClose={() => undefined} />);
    await pick(el, '스태프');
    expect(el.textContent).toContain('이 역할을 볼 수 있는 사람');
    expect(el.textContent).toContain('화면에서만 숨깁니다');
    expect(sw(el, '관리자 보기')!.getAttribute('aria-checked')).toBe('false');
    await click(button(el, '고른 역할만'));
    const chips = [...el.querySelectorAll<HTMLButtonElement>('.perm-block .chip')].map((b) => b.textContent);
    expect(chips).toEqual(['운영진장', '손님']);
    await click(chip(el, '손님'));
    await click(sw(el, '관리자 보기')!);
    expect(el.textContent).toContain('@everyone의 관리자 보기가 꺼져 있을 때만');
    await save(el);
    expect(call).toHaveBeenCalledWith('role.update', expect.objectContaining({ role: 'r1', visible_to: ['r2'], see_admins: true, permissions: ['view_channel'] }));
  });

  it('지금 값을 읽어 보이고, 표시를 바꾸지 않으면 그 필드를 보내지 않으며, 되돌리기로 돌아간다', async () => {
    const { call } = setup({ role_display: { r1: { visible_to: ['r2'], see_admins: true } } });
    const { el } = await render(<CrewSettingsModal space={SPACE} tab="roles" onClose={() => undefined} />);
    await pick(el, '스태프');
    expect(button(el, '고른 역할만').className).toContain('active');
    expect(chip(el, '손님').className).toContain('active');
    expect(sw(el, '관리자 보기')!.getAttribute('aria-checked')).toBe('true');
    await click(button(el, '모든 멤버'));
    await click(button(el, '되돌리기'));
    expect(button(el, '고른 역할만').className).toContain('active');
    await click(sw(el, '채널 보기')!);
    await save(el);
    const params = call.mock.calls.find(([m]) => m === 'role.update')![1] as Record<string, unknown>;
    expect(params).not.toHaveProperty('visible_to');
    expect(params).not.toHaveProperty('see_admins');
  });

  it('@everyone에는 볼 수 있는 사람 선택이 없고, 관리자 보기(기본 켜짐)를 끄면 see_admins만 보낸다', async () => {
    const { call } = setup();
    const { el } = await render(<CrewSettingsModal space={SPACE} tab="roles" onClose={() => undefined} />);
    await pick(el, '@everyone');
    expect(el.textContent).not.toContain('이 역할을 볼 수 있는 사람');
    expect(el.textContent).toContain('켜 두면 모든 멤버가');
    expect(sw(el, '관리자 보기')!.getAttribute('aria-checked')).toBe('true');
    await click(sw(el, '관리자 보기')!);
    await save(el);
    const params = call.mock.calls.find(([m]) => m === 'role.update')![1] as Record<string, unknown>;
    expect(params).toMatchObject({ role: EVERYONE_ROLE, see_admins: false });
    expect(params).not.toHaveProperty('visible_to');
  });

  it('역할을 관리할 수 없으면 표시 설정을 보이지 않는다', async () => {
    setup({ my_permissions: ['view_channel'] });
    const { el } = await render(<CrewSettingsModal space={SPACE} tab="roles" onClose={() => undefined} />);
    expect(el.textContent).not.toContain('관리자 보기');
    expect(el.textContent).not.toContain('권한 템플릿');
  });
});

describe('권한 템플릿', () => {
  it('기본 템플릿을 불러오면 내가 줄 수 있는 권한만 바꾸고 몇 개를 건너뛰었는지 알린다', async () => {
    const { call } = setup({ my_permissions: ['manage_roles', 'view_channel', 'send_messages', 'read_history', 'kick'] });
    const { el } = await render(<CrewSettingsModal space={SPACE} tab="roles" onClose={() => undefined} />);
    await pick(el, '스태프');
    await click(sw(el, '템플릿 불러오기: 운영자')!);
    expect(el.textContent).toContain('「운영자」 템플릿을 불러왔습니다');
    expect(el.textContent).toMatch(/내게 없는 권한 \d+개는 그대로 뒀습니다/);
    expect(sw(el, '추방')!.getAttribute('aria-checked')).toBe('true');
    expect(sw(el, '메시지 관리')!.getAttribute('aria-checked')).toBe('false');
    // 저장 전에는 보내지 않습니다.
    expect(call).not.toHaveBeenCalledWith('role.update', expect.anything());
    await save(el);
    expect(call).toHaveBeenCalledWith(
      'role.update',
      expect.objectContaining({ role: 'r1', permissions: ['view_channel', 'send_messages', 'read_history', 'kick'] }),
    );
  });

  it('지금 권한을 이름 붙여 앱 설정에 저장·덮어쓰기·지우고, 기본 템플릿 이름은 쓸 수 없다', async () => {
    const { savePrefs } = setup();
    const { el } = await render(<CrewSettingsModal space={SPACE} tab="roles" onClose={() => undefined} />);
    await pick(el, '스태프');
    const name = el.querySelector<HTMLInputElement>('input[aria-label="템플릿 이름"]')!;
    await typeInto(name, '관리자');
    expect(button(el, '지금 권한을 템플릿으로 저장').disabled).toBe(true);
    expect(el.textContent).toContain('기본 템플릿과 같은 이름은 쓸 수 없습니다');
    await typeInto(name, ' 스태프용 ');
    await click(button(el, '지금 권한을 템플릿으로 저장'));
    expect(useStore.getState().prefs.permTemplates).toEqual([{ name: '스태프용', permissions: ['view_channel'] }]);
    expect(savePrefs).toHaveBeenLastCalledWith(expect.objectContaining({ permTemplates: [{ name: '스태프용', permissions: ['view_channel'] }] }));
    expect(el.textContent).toContain('「스태프용」 템플릿을 저장했습니다');

    await click(sw(el, '말하기')!);
    await typeInto(name, '스태프용');
    await click(button(el, '덮어쓰기'));
    expect(useStore.getState().prefs.permTemplates).toEqual([{ name: '스태프용', permissions: ['view_channel', 'speak'] }]);

    await click(button(el, '되돌리기'));
    await click(sw(el, '템플릿 불러오기: 스태프용')!);
    expect(sw(el, '말하기')!.getAttribute('aria-checked')).toBe('true');
    await click(sw(el, '템플릿 지우기: 스태프용')!);
    expect(useStore.getState().prefs.permTemplates).toEqual([]);
    expect(sw(el, '템플릿 불러오기: 스태프용')).toBeNull();
    // 기본 템플릿은 지울 수 없습니다.
    expect(sw(el, '템플릿 지우기: 관리자')).toBeNull();
  });

  it('채널에서 저장한 템플릿의 거부는 역할에 쓰지 않는다(역할에는 거부가 없음)', async () => {
    setup();
    useStore.setState({ prefs: { ...useStore.getState().prefs, permTemplates: [{ name: '채널용', permissions: ['speak'], deny: ['view_channel', 'stream'] }] } });
    const { el } = await render(<CrewSettingsModal space={SPACE} tab="roles" onClose={() => undefined} />);
    await pick(el, '스태프');
    await click(sw(el, '템플릿 불러오기: 채널용')!);
    expect(sw(el, '말하기')!.getAttribute('aria-checked')).toBe('true');
    expect(sw(el, '채널 보기')!.getAttribute('aria-checked')).toBe('false');
    expect(sw(el, '화면공유')!.getAttribute('aria-checked')).toBe('false');
  });
});

describe('관리자를 볼 수 없을 때', () => {
  it('멤버 목록·개요에서 소유자 표시와 확정 기기 후보를 숨긴다', async () => {
    setup({ admins_hidden: true, my_permissions: ['view_channel'] });
    const { el } = await render(<CrewSettingsModal space={SPACE} tab="members" onClose={() => undefined} />);
    expect(el.querySelector('.gold')).toBeNull();
    await click(button(el, '개요'));
    expect(el.textContent).not.toContain('소유자');
    expect(el.textContent).not.toContain('확정 기기');
  });

  it('보통은 소유자에게 왕관을 붙인다', async () => {
    setup();
    const { el } = await render(<CrewSettingsModal space={SPACE} tab="members" onClose={() => undefined} />);
    expect(el.querySelector('.gold')).not.toBeNull();
  });
});

describe('크루 STUN 서버', () => {
  const STUN = 'stun.crew.example:3478';
  const REJECT = 'TURN 서버는 쓸 수 없음(여기는 STUN 서버의 host:port만)';

  /** space.get이 지금 보기를 돌려주고, 서버 목록·쓰기 스위치를 보기에 반영하는 데몬 흉내. */
  function serversSetup(over: Partial<SpaceView> = {}) {
    const r = setup(over);
    const call = vi.fn(async (method: string, params?: unknown) => {
      const p = params as Record<string, unknown>;
      const view = useStore.getState().views[SPACE];
      if (method === 'space.get') return view;
      if (method === 'crew.committer_candidates') return [];
      if (method === 'crew.set_servers') {
        const stun = p.stun as string[];
        if (stun.some((x) => x.startsWith('turn'))) throw new RpcError(ErrorCode.InvalidInput, REJECT);
        useStore.setState({ views: { [SPACE]: { ...view, crew_servers: { stun } } } });
      }
      return {};
    });
    useStore.setState({ client: { call, transport: { savePrefs: r.savePrefs } } as unknown as ApiClient });
    return call;
  }

  const overview = () => render(<CrewSettingsModal space={SPACE} tab="overview" onClose={() => undefined} />);
  const box = (el: HTMLElement, label: string) => el.querySelector<HTMLTextAreaElement>(`textarea[aria-label="${label}"]`);
  const serversCard = (el: HTMLElement) => box(el, '크루 STUN 서버')!.closest<HTMLElement>('.card')!;

  async function typeArea(input: HTMLTextAreaElement, value: string) {
    await act(async () => {
      Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, 'value')!.set!.call(input, value);
      input.dispatchEvent(new Event('input', { bubbles: true }));
    });
  }

  it('크루를 관리하는 사람은 목록을 고쳐 crew.set_servers로 통째로 저장하고, TURN 칸은 없다', async () => {
    const call = serversSetup({ crew_servers: { stun: [STUN] } });
    const { el } = await overview();
    expect(box(el, '크루 STUN 서버')!.value).toBe(STUN);
    expect(el.textContent).not.toContain('TURN');
    expect(el.textContent).toContain(CREW_SERVERS_MEMBER_NOTE);
    expect(CREW_SERVERS_MEMBER_NOTE).toContain('‘크루 STUN 사용’');
    expect(CREW_SERVERS_MEMBER_NOTE).toContain('각자');
    const save = () => button(serversCard(el), '저장');
    expect(save().disabled).toBe(true);

    await typeArea(box(el, '크루 STUN 서버')!, `${STUN}
 stun2.crew.example 
`);
    await click(save());
    expect(call).toHaveBeenCalledWith('crew.set_servers', { space: SPACE, stun: [STUN, 'stun2.crew.example'] });
    expect(useStore.getState().toasts.at(-1)?.text).toBe('크루 STUN 서버를 저장했습니다');
    // 저장한 목록으로 칸을 다시 채웁니다.
    await vi.waitFor(() => expect(box(el, '크루 STUN 서버')!.value).toBe(`${STUN}
stun2.crew.example`));
  });

  it('데몬이 거절하면 이유를 알린다', async () => {
    serversSetup({ crew_servers: { stun: [] } });
    const { el } = await overview();
    await typeArea(box(el, '크루 STUN 서버')!, 'turn:crew:secret@turn.crew.example:3478');
    await click(button(serversCard(el), '저장'));
    const t = useStore.getState().toasts.at(-1)!;
    expect(t.kind).toBe('error');
    expect(t.text).toBe(REJECT);
  });

  it('목록이 오지 않는(크루 관리 권한이 없는) 멤버에게는 고치는 칸이 없다', async () => {
    serversSetup({ my_permissions: ['view_channel'], crew_stun: true });
    const { el } = await overview();
    expect(box(el, '크루 STUN 서버')).toBeNull();
  });

  it('쓸 수 있는 크루 서버가 있으면 이 기기에서 쓸지 스스로 켜고 끈다(crew.use_servers)', async () => {
    const call = serversSetup({ my_permissions: ['view_channel', 'use_crew_stun'], crew_stun: true });
    const { el } = await overview();
    const toggle = () => sw(el, '이 크루의 STUN 서버 쓰기')!;
    expect(toggle().getAttribute('aria-checked')).toBe('false');
    expect(el.textContent).toContain(CREW_SERVERS_USE_HINT);
    expect(el.textContent).toContain('내 역할로 쓸 수 있는 크루 서버: STUN.');
    expect(CREW_SERVERS_USE_HINT).toContain('이 기기의 공인 IP를 봅니다');
    expect(CREW_SERVERS_USE_HINT).toContain('내 STUN 서버를 먼저 쓰고');
    expect(CREW_SERVERS_USE_HINT).toContain('이 크루의 P2P 전용 채널·통화와 이 크루 멤버와의 신뢰 연결에만');
    await click(toggle());
    expect(call).toHaveBeenCalledWith('crew.use_servers', { space: SPACE, enabled: true });
    expect(toggle().getAttribute('aria-checked')).toBe('true');
    await vi.waitFor(() => expect(call).toHaveBeenCalledWith('space.get', { space: SPACE }));
    await click(toggle());
    expect(call).toHaveBeenCalledWith('crew.use_servers', { space: SPACE, enabled: false });
    expect(toggle().getAttribute('aria-checked')).toBe('false');
  });

  it('쓸 수 있는 크루 서버가 없으면 스위치를 숨기고, 이미 켜 둔 것은 끌 수 있게 남긴다', async () => {
    serversSetup({ my_permissions: ['view_channel'] });
    const none = await overview();
    expect(sw(none.el, '이 크루의 STUN 서버 쓰기')).toBeNull();
    await none.unmount();

    serversSetup({ my_permissions: ['view_channel'], use_crew_servers: true });
    const { el } = await overview();
    expect(sw(el, '이 크루의 STUN 서버 쓰기')!.getAttribute('aria-checked')).toBe('true');
    expect(el.textContent).toContain('지금 내 역할로 쓸 수 있는 크루 서버가 없습니다');
  });

  it('역할 권한에 크루 STUN 사용이 있고 함께 저장되며, 크루 TURN 사용은 없다', async () => {
    const { call } = setup();
    const { el } = await render(<CrewSettingsModal space={SPACE} tab="roles" onClose={() => undefined} />);
    await pick(el, '스태프');
    expect([...el.querySelectorAll('.perm-title')].map((t) => t.textContent)).toContain('연결');
    expect(sw(el, '크루 TURN 사용')).toBeNull();
    expect(sw(el, '크루 STUN 사용')!.getAttribute('aria-checked')).toBe('false');
    await click(sw(el, '크루 STUN 사용')!);
    await save(el);
    expect(call).toHaveBeenCalledWith('role.update', expect.objectContaining({ role: 'r1', permissions: ['view_channel', 'use_crew_stun'] }));
  });
});
