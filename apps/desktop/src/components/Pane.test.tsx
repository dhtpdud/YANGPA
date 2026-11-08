// @vitest-environment happy-dom
// 휘발성 채널의 AI·자동화(API 토큰) 읽기 허용: 안내 띠에서 경고·확인 뒤 허용하고, 상단 표시로 다시 막습니다.
// P2P 전용 채널: 이 기기가 참가하기 전에는 내용·입력창·통화 대신 IP 노출 경고와 ‘참가’만 보이고, 참가한 뒤에는 상단에서 그만둡니다.

import { act } from 'react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import type { ApiClient } from '../api/client';
import type { ChannelView, SpaceView } from '../api/types';
import { useStore } from '../store/store';
import { button, cleanup, click, render } from '../test/dom';
import { Composer } from './Composer';
import { P2pGate, SpacePane, TopBar, VolatileBanner } from './Pane';

const SPACE = 'S'.repeat(32);
const CH = 'C'.repeat(32);

function channel(apiRead: boolean, over: Partial<ChannelView> = {}, settings: Partial<ChannelView['settings']> = {}): ChannelView {
  return {
    channel: CH,
    settings: {
      name: '비밀',
      kind: 'text',
      topic: '',
      parent: null,
      position: 0,
      security: 'volatile',
      join_mode: 'always',
      ttl: { mode: 'seconds', seconds: 300 },
      history: 'private',
      link_previews: false,
      overwrites: [],
      forum_tags: [],
      ...settings,
    },
    unread: 0,
    api_read_allowed: apiRead,
    p2p: false,
    p2p_joined: false,
    ...over,
  };
}

const crewOf = (c: ChannelView) =>
  ({
    space: SPACE,
    kind: 'crew',
    settings: { name: '크루' },
    members: [],
    roles: [],
    owner: 'O',
    channels: [c],
    my_permissions: ['send_messages', 'attach_files', 'connect_voice'],
  }) as unknown as SpaceView;

function setup() {
  const call = vi.fn(async (_method: string, _params?: unknown) => ({}));
  useStore.setState({
    client: { call } as unknown as ApiClient,
    modal: null,
    toasts: [],
    prefs: { ...useStore.getState().prefs, bannerCollapsed: false },
  });
  return call;
}

afterEach(async () => {
  await cleanup();
  vi.useRealTimers();
  useStore.setState({ client: null, modal: null });
});

describe('휘발성 채널 API 읽기 허용', () => {
  it('기본은 막힘으로 보이고, 허용은 경고를 확인한 뒤에만 데몬에 보낸다', async () => {
    const call = setup();
    const { el } = await render(<VolatileBanner space={SPACE} channel={channel(false)} />);
    expect(el.textContent).toContain('AI·자동화 읽기(이 기기): 막힘');
    await click(button(el, '허용하기'));
    // 아직 보내지 않고 확인을 묻습니다.
    expect(call).not.toHaveBeenCalled();
    const modal = useStore.getState().modal;
    expect(modal?.kind).toBe('confirm');
    if (modal?.kind !== 'confirm') return;
    expect(modal.body).toContain('기록');
    await act(async () => {
      await modal.onConfirm();
    });
    expect(call).toHaveBeenCalledWith('channel.allow_api_read', { space: SPACE, channel: CH, allowed: true });
  });

  it('허용된 채널은 상단에 표시하고, 누르면 확인 없이 바로 다시 막는다', async () => {
    const call = setup();
    const space = { space: SPACE, kind: 'crew', settings: { name: '크루' }, members: [], owner: 'O' } as unknown as SpaceView;
    const { el } = await render(<TopBar space={space} channel={channel(true)} />);
    await click(button(el, 'AI 읽기 허용'));
    expect(useStore.getState().modal).toBeNull();
    expect(call).toHaveBeenCalledWith('channel.allow_api_read', { space: SPACE, channel: CH, allowed: false });
    // 막은 뒤에는 공간 보기를 다시 읽어 표시를 지웁니다.
    await vi.waitFor(() => expect(call).toHaveBeenCalledWith('space.get', { space: SPACE }));
  });

  it('막혀 있으면 상단 표시가 없다', async () => {
    setup();
    const space = { space: SPACE, kind: 'crew', settings: { name: '크루' }, members: [], owner: 'O' } as unknown as SpaceView;
    const { el } = await render(<TopBar space={space} channel={channel(false)} />);
    expect(el.textContent).not.toContain('AI 읽기 허용');
  });
});

describe('P2P 전용 채널 잠금', () => {
  const p2pText = (joined: boolean) => channel(false, { p2p: true, p2p_joined: joined }, { security: 'normal', ttl: { mode: 'none' } });

  /** 가짜 데몬: 공간 보기를 물으면 그 채널 하나만 든 크루를 줍니다. */
  function p2pSetup(c: ChannelView) {
    const call = vi.fn(async (method: string, _params?: unknown) => (method === 'space.get' ? crewOf(c) : {}));
    useStore.setState({ client: { call } as unknown as ApiClient, modal: null, toasts: [] });
    return call;
  }

  function show(c: ChannelView) {
    const call = p2pSetup(c);
    const view = crewOf(c);
    useStore.setState({ views: { [SPACE]: view }, selected: { space: SPACE, channel: CH }, friends: [], search: null, voice: null });
    return { call, view };
  }

  afterEach(() => {
    useStore.setState({ views: {}, selected: { space: 'home', channel: null } });
  });

  it('잠금 화면은 IP가 보이고 Tor를 거치지 않는다고 알리며, 참가를 누르면 channel.p2p_join을 부른다', async () => {
    const call = p2pSetup(p2pText(true));
    const { el } = await render(<P2pGate space={SPACE} channel={p2pText(false)} />);
    expect(el.textContent).toContain('이 채널의 참가자는 내 IP 주소를 볼 수 있습니다');
    expect(el.textContent).toContain('메시지·파일·통화가 Tor를 거치지 않고 참가한 기기끼리 직접 오갑니다');
    expect(el.textContent).toContain('다시 숨길 수 없습니다');
    expect(call).not.toHaveBeenCalled();
    await click(button(el, '참가'));
    expect(call).toHaveBeenCalledWith('channel.p2p_join', { space: SPACE, channel: CH });
    expect(useStore.getState().views[SPACE]?.channels[0].p2p_joined).toBe(true);
  });

  it('참가하기 전에는 메시지 목록·입력창 대신 잠금 화면을 보이고 내용을 읽지 않는다', async () => {
    const { call } = show(p2pText(false));
    const { el } = await render(<SpacePane space={crewOf(p2pText(false))} />);
    expect(el.querySelector('.p2p-gate')).not.toBeNull();
    expect(el.querySelector('textarea')).toBeNull();
    expect(el.querySelector('.msg-scroll')).toBeNull();
    expect(el.textContent).toContain('P2P 전용');
    expect(el.textContent).not.toContain('P2P 참여 그만두기');
    expect(call).not.toHaveBeenCalledWith('message.list', expect.anything());
  });

  it('통화 채널도 참가하기 전에는 통화 대신 잠금 화면을 보인다', async () => {
    const voice = channel(false, { p2p: true, p2p_joined: false }, { kind: 'voice', security: 'normal', name: '통화' });
    show(voice);
    const { el } = await render(<SpacePane space={crewOf(voice)} />);
    expect(el.querySelector('.p2p-gate')).not.toBeNull();
    expect(() => button(el, '통화 참가')).toThrow();
    expect(el.textContent).toContain('통화가 Tor를 거치지 않고');
  });

  it('입력창은 잠긴 채널에서 쓸 수 없다', async () => {
    const pickFiles = vi.fn(async () => []);
    useStore.setState({
      client: { call: vi.fn(async () => ({})), transport: { pickFiles, onFileDrop: () => () => undefined } } as unknown as ApiClient,
      views: { [SPACE]: crewOf(p2pText(false)) },
      friends: [],
    });
    const { el } = await render(<Composer space={SPACE} channel={CH} channelView={p2pText(false)} />);
    const ta = el.querySelector('textarea')!;
    expect(ta.disabled).toBe(true);
    expect(ta.getAttribute('placeholder')).toContain('P2P 전용 채널에 참가해야');
    expect(el.querySelector<HTMLButtonElement>('button[title="파일 첨부"]')!.disabled).toBe(true);
  });

  it('참가한 채널은 상단에 P2P 전용 표시와 그만두기를 보이고, 누르면 channel.p2p_leave를 부른다', async () => {
    const c = p2pText(true);
    const call = p2pSetup(p2pText(false));
    const { el } = await render(<TopBar space={crewOf(c)} channel={c} />);
    expect(el.querySelector('.badge.p2p')?.textContent).toContain('P2P 전용');
    await click(button(el, 'P2P 참여 그만두기'));
    expect(call).toHaveBeenCalledWith('channel.p2p_leave', { space: SPACE, channel: CH });
  });
});
