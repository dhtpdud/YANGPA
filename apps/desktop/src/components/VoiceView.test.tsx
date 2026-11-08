// @vitest-environment happy-dom
// 통화 화면의 토폴로지 표시(메시/SFU·중계 기기와 그 주인·넘겨받는 중)와 SFU 화면 화질 고르기, 익명 채널의 참가자 이름.

import { act } from 'react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { ANONYMOUS_USER, type ChannelView, type IdentityView, type MemberView, type SpaceView, type VoiceParticipant } from '../api/types';
import { chKey, useStore } from '../store/store';
import { cleanup, render } from '../test/dom';
import type { VoiceSnapshot } from '../voice/session';
import { topologyLabel, VoiceView } from './VoiceView';

const SPACE = 'S'.repeat(32);
const CH = 'C'.repeat(32);

const member = (user: string, name: string): MemberView => ({
  user,
  profile: { name } as MemberView['profile'],
  roles: [],
  online: true,
  automation: false,
});

const space = {
  space: SPACE,
  kind: 'crew',
  settings: { name: '크루' },
  members: [member('u-a', '앨리스'), member('u-b', '밥'), member('u-c', '캐럴')],
  owner: 'u-a',
  my_permissions: ['connect_voice', 'stream'],
} as unknown as SpaceView;

const channel = { channel: CH, settings: { name: '수다방', kind: 'voice' }, unread: 0, api_read_allowed: false } as unknown as ChannelView;

const parts: VoiceParticipant[] = [
  { user: 'u-a', device: 'A', muted: false, deafened: false, screen: true },
  { user: 'u-b', device: 'B', muted: false, deafened: false, screen: false },
  { user: 'u-c', device: 'C', muted: false, deafened: false, screen: false },
];

function snapshot(patch: Partial<VoiceSnapshot>): VoiceSnapshot {
  return {
    space: SPACE,
    channel: CH,
    call: 'c1',
    topology: 'mesh',
    generation: 1,
    participants: parts,
    speaking: {},
    connection: {},
    muted: false,
    deafened: false,
    sharing: false,
    contentHint: 'detail',
    watching: [],
    layers: {},
    viewers: [],
    screenVolume: {},
    e2ee: 'script-transform',
    ...patch,
  };
}

afterEach(async () => {
  await cleanup();
  useStore.setState({ voice: null, identity: null, voiceRosters: {} });
});

describe('통화 토폴로지 표시', () => {
  it('메시, SFU 중계 기기와 연결 상태, 넘겨받는 중, 통화 밖 호스트를 구분한다', () => {
    const members = new Map(space.members.map((m) => [m.user, m]));
    expect(topologyLabel(snapshot({}), 'B', members).text).toBe('메시 · 서로 직접 연결');
    expect(topologyLabel(snapshot({ topology: 'sfu', sfuHost: 'A', sfuState: 'connected' }), 'B', members).text).toBe('SFU · 중계: 앨리스');
    expect(topologyLabel(snapshot({ topology: 'sfu', sfuHost: 'A', sfuState: 'connecting' }), 'B', members).text).toBe(
      'SFU · 중계: 앨리스 (연결 중)',
    );
    expect(topologyLabel(snapshot({ topology: 'sfu', sfuHost: 'B', sfuState: 'connected' }), 'B', members).text).toBe('SFU · 중계: 이 기기');
    expect(topologyLabel(snapshot({ topology: 'sfu', sfuHost: 'Z', sfuState: 'connected' }), 'B', members).text).toBe(
      'SFU · 중계: 통화 밖 호스트 노드',
    );
    // 통화 밖 호스트 노드(시더)는 누구 것인지 함께 보여 줍니다.
    expect(topologyLabel(snapshot({ topology: 'sfu', sfuHost: 'Z', sfuHostUser: 'u-c', sfuState: 'connected' }), 'B', members).text).toBe(
      'SFU · 중계: 캐럴의 통화 밖 노드',
    );
    expect(topologyLabel(snapshot({ topology: 'sfu', sfuState: 'waiting' }), 'B', members).text).toBe('SFU · 중계 기기를 넘겨받는 중…');
  });

  it('SFU에서 보는 화면마다 화질을 고를 수 있고, 메시에서는 고르는 칸이 없다', async () => {
    const setScreenLayer = vi.fn();
    useStore.setState({
      identity: { user: 'u-b', device: 'B', profile: { name: '밥' }, devices: [] } as unknown as IdentityView,
      voice: snapshot({ topology: 'sfu', sfuHost: 'A', sfuState: 'connected', watching: ['A'], layers: { A: 'medium' } }),
      setScreenLayer,
    });
    const { el } = await render(<VoiceView space={space} channel={channel} />);
    expect(el.querySelector('.vr-topology')?.textContent).toContain('SFU · 중계: 앨리스');
    const select = el.querySelector<HTMLSelectElement>('select.layer');
    expect(select?.value).toBe('medium');
    await act(async () => {
      select!.value = 'low';
      select!.dispatchEvent(new Event('change', { bubbles: true }));
    });
    expect(setScreenLayer).toHaveBeenCalledWith('A', 'low');

    await act(async () => {
      useStore.setState({ voice: snapshot({ watching: ['A'] }) });
    });
    expect(el.querySelector('.vr-topology')?.textContent).toContain('메시 · 서로 직접 연결');
    expect(el.querySelector('select.layer')).toBeNull();
  });

  it('보는 화면 오른쪽 아래에 소리 크기·그만 보기·전체화면을 모으고, 화면 소리가 오지 않는 SFU에는 소리 크기가 없다', async () => {
    const setScreenVolume = vi.fn();
    useStore.setState({
      identity: { user: 'u-b', device: 'B', profile: { name: '밥' }, devices: [] } as unknown as IdentityView,
      voice: snapshot({ watching: ['A'], screenVolume: { A: 30 } }),
      setScreenVolume,
    });
    const { el } = await render(<VoiceView space={space} channel={channel} />);
    const actions = el.querySelector('.vtile-actions')!;
    expect(actions.querySelector('.watch')?.textContent).toContain('그만 보기');
    expect(actions.querySelector('[aria-label="전체화면"]')).not.toBeNull();
    // 이 웹뷰(jsdom)는 PiP를 지원하지 않아 버튼을 감춥니다.
    expect(actions.querySelector('[aria-label="PiP로 보기"]')).toBeNull();
    const range = actions.querySelector<HTMLInputElement>('input[aria-label="화면 소리 크기"]')!;
    expect(range.value).toBe('30');
    await act(async () => {
      const set = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')!.set!;
      set.call(range, '70');
      range.dispatchEvent(new Event('input', { bubbles: true }));
    });
    expect(setScreenVolume).toHaveBeenCalledWith('A', 70);

    await act(async () => {
      useStore.setState({ voice: snapshot({ topology: 'sfu', sfuHost: 'A', sfuState: 'connected', watching: ['A'] }) });
    });
    expect(el.querySelector('input[aria-label="화면 소리 크기"]')).toBeNull();
  });
});

describe('익명 채널 참가자', () => {
  // 익명 자리를 멤버 목록에서 찾으면 이 이름이 드러납니다(찾지 않아야 함).
  const anonSpace = { ...space, members: [...space.members, member(ANONYMOUS_USER, '드러난 이름')] } as SpaceView;
  const anonChannel = { ...channel, anonymous: true, participants: [ANONYMOUS_USER, ANONYMOUS_USER] } as ChannelView;
  const anonParts: VoiceParticipant[] = [
    { user: ANONYMOUS_USER, device: ANONYMOUS_USER, muted: false, deafened: false, screen: false, anon: 1 },
    { user: ANONYMOUS_USER, device: 'B', muted: false, deafened: false, screen: false, anon: 2, mine: true },
  ];

  const labels = (el: HTMLElement, sel: string) => [...el.querySelectorAll(sel)].map((x) => x.textContent?.trim());

  it('Tor로 아직 소리가 한 번도 오지 않은 상대는 연결 중으로 흐리게 보인다', async () => {
    useStore.setState({
      identity: { user: 'u-b', device: 'B', profile: { name: '밥' }, devices: [] } as unknown as IdentityView,
      voice: snapshot({ participants: anonParts, anonymous: true, connection: { [ANONYMOUS_USER]: 'tor' } }),
    });
    const { el } = await render(<VoiceView space={anonSpace} channel={anonChannel} />);
    expect(el.querySelectorAll('.vtile.connecting')).toHaveLength(1);
    expect(labels(el, '.vtile-wait')).toEqual(['연결 중']);
  });

  it("통화 타일은 '익명 N'으로, 내 타일은 '(나)'를 붙여 보인다", async () => {
    useStore.setState({
      identity: { user: 'u-b', device: 'B', profile: { name: '밥' }, devices: [] } as unknown as IdentityView,
      voice: snapshot({ participants: anonParts, anonymous: true, connection: { [ANONYMOUS_USER]: 'connected' } }),
    });
    const { el } = await render(<VoiceView space={anonSpace} channel={anonChannel} />);
    expect(labels(el, '.vtile-label span:not(.muted)')).toEqual(['익명 1', '익명 2 (나)']);
    expect(el.querySelector('.vtile.connecting')).toBeNull();
    expect(el.textContent).not.toContain('드러난 이름');
    expect(el.textContent).not.toContain('밥');
  });

  it('참가 전 화면은 참가자 목록이 있으면 번호로, 없으면 수만큼 익명으로 보인다', async () => {
    const first = await render(<VoiceView space={anonSpace} channel={anonChannel} />);
    expect(first.el.textContent).toContain('2명이 대화 중');
    expect(labels(first.el, '.vl-person > span:last-child')).toEqual(['익명', '익명']);
    await cleanup();
    useStore.setState({ voiceRosters: { [chKey(SPACE, CH)]: { participants: anonParts, generation: 1, topology: 'mesh' } } });
    const second = await render(<VoiceView space={anonSpace} channel={anonChannel} />);
    expect(labels(second.el, '.vl-person > span:last-child')).toEqual(['익명 1', '익명 2 (나)']);
    expect(second.el.textContent).not.toContain('드러난 이름');
  });

  it('SFU 중계 기기도 익명 번호로 보인다', () => {
    const members = new Map(anonSpace.members.map((m) => [m.user, m]));
    const v = snapshot({ topology: 'sfu', sfuHost: ANONYMOUS_USER, sfuState: 'connected', participants: anonParts });
    expect(topologyLabel(v, 'B', members).text).toBe('SFU · 중계: 익명 1');
  });
});
