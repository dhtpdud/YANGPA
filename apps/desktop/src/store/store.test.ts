import { afterEach, describe, expect, it, vi } from 'vitest';
import { RpcError, type ApiClient } from '../api/client';
import { ErrorCode, type IdentityView, type MessageView, type NetStatus, type SpaceView } from '../api/types';
import { applyBatch, emptyChannel } from './apply';
import { ANON_CALL_ENDED, ANON_VOICE_OFF_WARNING } from '../lib/anon';
import { p2pOffWarning, p2pOnWarning, sanitizePrefs, useStore } from './store';

const identity = {
  user: 'u1',
  device: 'd1',
  profile: { version: 1, name: '나', bio: '', avatar: null, banner: null, theme_color: null, status: '' },
  devices: [],
};

describe('앱을 띄운 알림 링크', () => {
  it('데몬이 뜨기 전에 온 채널 링크는 다 읽은 뒤에 열어 내용을 읽고 휘발성 보호도 건다', async () => {
    const view = {
      space: 'S',
      members: [],
      roles: [],
      channels: [{ channel: 'C', unread: 0, p2p: false, settings: { name: '휘발', kind: 'text', security: 'volatile' } }],
    };
    const call = vi.fn(async (method: string) => {
      if (method === 'identity.get') return identity;
      if (method === 'space.list') return [{ space: 'S', kind: 'crew' }];
      if (method === 'space.get') return view;
      if (method === 'message.list') return { messages: [], gaps: [] };
      if (method.endsWith('.list') || method === 'friend.requests') return [];
      return null;
    });
    const setContentProtection = vi.fn(async () => true);
    useStore.setState({
      client: { call, transport: { setContentProtection } } as unknown as ApiClient,
      phase: 'loading',
      views: {},
      channels: {},
    });

    useStore.getState().openChannelLink('S', 'C');
    expect(call).not.toHaveBeenCalled();

    await useStore.getState().loadAll();
    expect(useStore.getState().selected).toEqual({ space: 'S', channel: 'C' });
    expect(call).toHaveBeenCalledWith('message.list', expect.objectContaining({ space: 'S', channel: 'C' }));
    expect(setContentProtection).toHaveBeenCalledWith(true);
  });
});

describe('휴대폰 뒤로 가기', () => {
  it('라이브러리는 파일 → 폴더 목록 → 홈 순서로 돌아가고 홈에서는 앱에 맡긴다', () => {
    useStore.setState({ modal: null, library: true, mobilePane: 'chat' });
    const { mobileBack } = useStore.getState();

    expect(mobileBack()).toBe(true);
    expect(useStore.getState()).toMatchObject({ library: true, mobilePane: 'list' });

    expect(mobileBack()).toBe(true);
    expect(useStore.getState()).toMatchObject({ library: false, mobilePane: 'list' });

    expect(mobileBack()).toBe(false);
  });
});

describe('P2P 전용 채널', () => {
  const P2P = { channel: 'P', unread: 3, p2p: true, p2p_joined: false, settings: { name: '직접', kind: 'text', security: 'normal' } };
  const VOICE = { channel: 'V', unread: 0, p2p: true, p2p_joined: false, settings: { name: '통화', kind: 'voice', security: 'normal' } };
  const TOR_VOICE = { channel: 'W', unread: 0, p2p: false, p2p_joined: false, settings: { name: '일반 통화', kind: 'voice', security: 'normal' } };
  const viewOf = (joined: boolean) =>
    ({ space: 'S', members: [], roles: [], channels: [{ ...P2P, p2p_joined: joined }, VOICE, TOR_VOICE] }) as unknown as SpaceView;
  const msg = { id: 'e1', lamport: 1, created_at: 1, text: '안녕' } as unknown as MessageView;

  function setup(joined = false) {
    let current = joined;
    const call = vi.fn(async (method: string) => {
      if (method === 'channel.p2p_join') current = true;
      if (method === 'channel.p2p_leave') current = false;
      if (method === 'space.get') return viewOf(current);
      if (method === 'message.list') return { messages: [msg], gaps: [] };
      return {};
    });
    const transport = { setContentProtection: vi.fn(async () => true), savePrefs: vi.fn(async () => undefined) };
    useStore.setState({
      client: { call, transport } as unknown as ApiClient,
      views: { S: viewOf(joined) },
      channels: {},
      spaces: [],
      modal: null,
      toasts: [],
      voice: null,
    });
    return call;
  }

  afterEach(() => {
    useStore.setState({ client: null, views: {}, channels: {}, modal: null, selected: { space: 'home', channel: null }, active: null });
  });

  it('P2P 전용으로 바꾸기는 멤버가 경고를 받아들여야 다시 쓸 수 있다고 확인받은 뒤에만 보낸다', async () => {
    const call = setup();
    useStore.getState().setChannelP2p('S', 'P', true);
    expect(call).not.toHaveBeenCalled();
    const m = useStore.getState().modal;
    expect(m?.kind).toBe('confirm');
    if (m?.kind !== 'confirm') return;
    expect(m.body).toBe(p2pOnWarning('직접'));
    expect(m.body).toContain('IP 주소가 보입니다');
    expect(m.body).toContain('경고를 받아들이고 참가해야');
    await m.onConfirm();
    expect(call).toHaveBeenCalledWith('channel.set_p2p', { space: 'S', channel: 'P', enabled: true });
    // 바뀐 채널 보기를 다시 읽습니다(다음 시험의 가짜 데몬에 늦게 닿지 않게 여기서 기다림).
    await vi.waitFor(() => expect(call).toHaveBeenCalledWith('space.get', { space: 'S' }));
  });

  it('Tor로 되돌리기는 기록이 모두에게 동기화되고 보인 IP는 다시 숨길 수 없다고 확인받는다', async () => {
    const call = setup(true);
    useStore.getState().setChannelP2p('S', 'P', false);
    expect(call).not.toHaveBeenCalled();
    const m = useStore.getState().modal;
    if (m?.kind !== 'confirm') throw new Error('확인 창이 없음');
    expect(m.body).toBe(p2pOffWarning('직접'));
    expect(m.body).toContain('Tor를 거쳐 모든 멤버에게 동기화됩니다');
    expect(m.body).toContain('다시 숨길 수 없습니다');
    await m.onConfirm();
    expect(call).toHaveBeenCalledWith('channel.set_p2p', { space: 'S', channel: 'P', enabled: false });
    await vi.waitFor(() => expect(call).toHaveBeenCalledWith('space.get', { space: 'S' }));
  });

  it('잠긴 채널은 열어도 내용을 읽지 않고, 참가하면 그때 읽는다', async () => {
    const call = setup();
    await useStore.getState().selectChannel('S', 'P');
    expect(call).not.toHaveBeenCalledWith('message.list', expect.anything());
    expect(call).not.toHaveBeenCalledWith('channel.mark_read', expect.anything());
    expect(useStore.getState().channels['S/P']).toBeUndefined();
    await useStore.getState().joinP2p('S', 'P');
    expect(call).toHaveBeenCalledWith('channel.p2p_join', { space: 'S', channel: 'P' });
    expect(useStore.getState().views.S.channels[0].p2p_joined).toBe(true);
    expect(useStore.getState().channels['S/P']?.messages.map((x) => x.id)).toEqual(['e1']);
  });

  it('참여를 그만두면 읽어 둔 내용을 화면 메모리에서 버린다', async () => {
    const call = setup(true);
    await useStore.getState().selectChannel('S', 'P');
    expect(useStore.getState().channels['S/P']?.loaded).toBe(true);
    await useStore.getState().leaveP2p('S', 'P');
    expect(call).toHaveBeenCalledWith('channel.p2p_leave', { space: 'S', channel: 'P' });
    expect(useStore.getState().views.S.channels[0].p2p_joined).toBe(false);
    expect(useStore.getState().channels['S/P']).toBeUndefined();
  });

  it('검색 결과로 옮겨도 검색 패널을 닫지 않고(스크롤 유지), 채널을 직접 고르면 닫는다', async () => {
    setup();
    const search = { query: '양파', results: [], total: 0, offset: 0, loading: false } as never;
    useStore.setState({ search });
    await useStore.getState().jumpToMessage('S', 'P', 'e1');
    expect(useStore.getState().search).toBe(search);
    await useStore.getState().selectChannel('S', 'P');
    expect(useStore.getState().search).toBeNull();
  });

  it('잠긴 통화 채널에는 통화로 들어가지 않는다', async () => {
    const call = setup();
    useStore.setState({ identity: { user: 'u1', device: 'd1' } as unknown as IdentityView, net: { mode: 'tor' } as NetStatus });
    await useStore.getState().joinVoice('S', 'V');
    expect(call).not.toHaveBeenCalledWith('voice.join', expect.anything());
    expect(useStore.getState().toasts.at(-1)).toMatchObject({ kind: 'warn' });
    expect(useStore.getState().voice).toBeNull();
  });

  it('Tor로 도는 기기의 P2P 전용이 아닌 통화 채널은 막지 않고 데몬에 참가를 묻는다(Tor 통화)', async () => {
    const call = setup();
    useStore.setState({ identity: { user: 'u1', device: 'd1' } as unknown as IdentityView, net: { mode: 'tor' } as NetStatus });
    await useStore.getState().joinVoice('S', 'W');
    expect(call).toHaveBeenCalledWith('voice.join', { space: 'S', channel: 'W' });
    expect(useStore.getState().toasts.some((t) => t.text.includes('P2P 전용 채널'))).toBe(false);
  });
});

describe('기기 연결', () => {
  afterEach(() => useStore.setState({ client: null, toasts: [], phase: 'loading', deviceLinkRequest: null, deviceLinkUrl: null }));

  it('loadAll에서 기다리는 새 기기가 있으면 허용 창을 띄운다', async () => {
    const request = { name: '새 휴대폰', code: '123456', contested: false };
    const call = vi.fn(async (method: string) => {
      if (method === 'identity.get') return identity;
      if (method === 'device.link.pending') return request;
      if (method.endsWith('.list') || method === 'friend.requests') return [];
      return null;
    });
    useStore.setState({ client: { call } as unknown as ApiClient, phase: 'loading' });
    await useStore.getState().loadAll();
    expect(useStore.getState().deviceLinkRequest).toEqual(request);
  });

  it('OS가 넘긴 링크는 신원이 없을 때만 연결 화면에 넘긴다', () => {
    const url = 'yangpa://device/3yZe7d';
    useStore.setState({ phase: 'onboarding' });
    expect(useStore.getState().openAppLink(url)).toBe(true);
    expect(useStore.getState().deviceLinkUrl).toBe(url);
    useStore.setState({ phase: 'ready', deviceLinkUrl: null });
    useStore.getState().openAppLink(url);
    expect(useStore.getState().deviceLinkUrl).toBeNull();
    expect(useStore.getState().toasts).toHaveLength(1);
  });
});

describe('없어진 UI 설정', () => {
  it('예전 설정 파일의 창 내리면 저전력 표시는 받아들이지 않는다', () => {
    expect(sanitizePrefs({ autoLowPower: true, autoLowPowerApplied: true, theme: 'light' })).toEqual({ theme: 'light' });
  });
});

describe('콘텐츠 뷰어 설정', () => {
  it('객체일 때만 받고 범위를 맞춘다', () => {
    expect(sanitizePrefs({ viewer: { seek: 99, order: 'oldest' } }).viewer).toMatchObject({ seek: 60, order: 'oldest', fill: true });
    expect(sanitizePrefs({ viewer: 'x' })).toEqual({});
  });
});

describe('내 권한 템플릿 설정', () => {
  it('목록일 때만 받고, 모르는 권한·이름이 겹친 항목을 정리한다', () => {
    expect(
      sanitizePrefs({
        permTemplates: [
          { name: ' 스태프 ', permissions: ['kick', 'fly'] },
          { name: '스태프', permissions: ['ban'] },
          { name: '', permissions: [] },
        ],
      }),
    ).toEqual({ permTemplates: [{ name: '스태프', permissions: ['ban'] }] });
    expect(sanitizePrefs({ permTemplates: 'x' })).toEqual({});
  });
});

describe('차단 목록·신고로 숨기기', () => {
  it('모더레이션 전 데몬이면 차단 목록을 null로 두고, 있으면 그대로 읽는다', async () => {
    const missing = vi.fn(async () => {
      throw new RpcError(ErrorCode.MethodNotFound, '없는 메서드');
    });
    useStore.setState({ client: { call: missing } as unknown as ApiClient, blocked: [] });
    await useStore.getState().refreshBlocked();
    expect(useStore.getState().blocked).toBeNull();
    const list = [{ user: 'U', profile: null, blocked_at: 1 }];
    useStore.setState({ client: { call: vi.fn(async () => list) } as unknown as ApiClient });
    await useStore.getState().refreshBlocked();
    expect(useStore.getState().blocked).toEqual(list);
    expect(useStore.getState().isBlocked('U')).toBe(true);
    expect(useStore.getState().isBlocked('V')).toBe(false);
  });

  it('신고하며 숨긴 메시지는 그 채널과 글타래에서만 빠진다', () => {
    const m = (id: string) => ({ id }) as MessageView;
    useStore.setState({
      channels: {
        'S/A': { ...emptyChannel(), loaded: true, messages: [m('x'), m('y')] },
        'S/A/T': { ...emptyChannel(), loaded: true, messages: [m('y')] },
        'S/AB': { ...emptyChannel(), loaded: true, messages: [m('y')] },
      },
    });
    useStore.getState().hideMessage('S', 'A', 'y');
    const ch = useStore.getState().channels;
    expect(ch['S/A'].messages.map((x) => x.id)).toEqual(['x']);
    expect(ch['S/A/T'].messages).toEqual([]);
    expect(ch['S/AB'].messages.map((x) => x.id)).toEqual(['y']);
  });
});

describe('기기 폐기(device.wiped)', () => {
  it('화면 메모리를 버리고 신원부터 다시 읽어 온보딩으로 간다', async () => {
    const call = vi.fn(async (method: string) => {
      if (method === 'identity.get') throw new RpcError(ErrorCode.NoIdentity, '신원 없음');
      return {};
    });
    useStore.setState({
      client: { call } as unknown as ApiClient,
      phase: 'ready',
      voice: null,
      identity: { ...identity, first_syncing: false },
      me: 'u1',
      channels: { 'S/A': { ...emptyChannel(), loaded: true, messages: [{ id: 'x' } as MessageView] } },
      selected: { space: 'S', channel: 'A' },
      active: 'S/A',
    });
    await useStore.getState().resetAfterWipe();
    expect(useStore.getState()).toMatchObject({ channels: {}, identity: null, me: null, active: null, phase: 'onboarding', selected: { space: 'home' } });
  });
});

describe('내 프로필', () => {
  it('다른 기기에서 온 내 프로필을 다시 읽어 화면에 반영한다', async () => {
    const next = { ...identity, profile: { ...identity.profile, name: '원래 이름' }, first_syncing: false };
    useStore.setState({ client: { call: vi.fn(async () => next) } as unknown as ApiClient, identity: { ...next, profile: identity.profile } });
    await useStore.getState().refreshIdentity();
    expect(useStore.getState().identity?.profile.name).toBe('원래 이름');
  });
});

describe('채널 메시지 순서', () => {
  // 시각 순서와 반대로 가는 ID를 써서 lamport로 정렬하는지 확인합니다.
  const m = (n: number) =>
    ({ id: `e${10 - n}`, lamport: 100 + n, created_at: n * 1000, text: String(n), volatile: true }) as unknown as MessageView;
  const texts = (key: string) => useStore.getState().channels[key]?.messages.map((x) => x.text);
  const view = {
    space: 'S',
    members: [],
    roles: [],
    channels: [
      { channel: 'V', unread: 0, settings: { kind: 'text', security: 'volatile' } },
      { channel: 'N', unread: 0, settings: { kind: 'text', security: 'normal' } },
    ],
  } as unknown as SpaceView;
  const setup = (page: () => MessageView[]) => {
    const call = vi.fn(async (method: string) => (method === 'message.list' ? { messages: page(), gaps: [] } : {}));
    const transport = { setContentProtection: vi.fn(async () => true), savePrefs: vi.fn(async () => undefined) };
    useStore.setState({ client: { call, transport } as unknown as ApiClient, views: { S: view }, channels: {}, spaces: [] });
    return call;
  };

  it('휘발성 채널을 나갔다 돌아와도 데몬이 준 메시지가 시간 순으로 보이고 새 메시지는 끝에 붙는다', async () => {
    const call = setup(() => [m(1), m(2), m(3)]);
    await useStore.getState().selectChannel('S', 'V');
    expect(texts('S/V')).toEqual(['1', '2', '3']);
    await useStore.getState().selectChannel('S', 'N');
    expect(useStore.getState().channels['S/V']).toBeUndefined();
    await useStore.getState().selectChannel('S', 'V');
    expect(texts('S/V')).toEqual(['1', '2', '3']);
    useStore.setState(applyBatch(useStore.getState(), [{ type: 'message.new', space: 'S', channel: 'V', message: m(4), notify: { title: 't' } }]).patch);
    expect(texts('S/V')).toEqual(['1', '2', '3', '4']);
    // 채널을 열면 메시지를 고르지 않고 채널 전체를 읽음으로(데몬이 가장 최근 메시지까지 옮김).
    expect(call).toHaveBeenLastCalledWith('channel.mark_read', { space: 'S', channel: 'V' });
  });

  it('포럼 채널도 열면 읽음 처리하고, 창이 돌아오면 다시 읽음 처리한다', async () => {
    const call = setup(() => []);
    const forum = { channel: 'F', unread: 2, settings: { kind: 'forum', security: 'normal' } };
    useStore.setState({ views: { S: { ...view, channels: [...view.channels, forum] } as unknown as SpaceView }, focused: true });
    await useStore.getState().selectChannel('S', 'F');
    expect(call).toHaveBeenCalledWith('channel.mark_read', { space: 'S', channel: 'F' });
    expect(useStore.getState().views.S.channels.find((c) => c.channel === 'F')?.unread).toBe(0);
    call.mockClear();
    useStore.getState().setFocused(false);
    useStore.getState().setFocused(true);
    expect(call).toHaveBeenCalledWith('channel.mark_read', { space: 'S', channel: 'F' });
  });

  it('페이지 순서와 상관없이 시간 순으로 맞춘다', async () => {
    setup(() => [m(2), m(3), m(1)]);
    await useStore.getState().loadLatest('S', 'N');
    expect(texts('S/N')).toEqual(['1', '2', '3']);
  });
});

describe('답장·보내기 신호', () => {
  it('setReply는 답장 대상이 있을 때만 그 목록의 입력창 커서 요청 번호를 올린다', () => {
    useStore.setState({ replyTo: {}, composerFocus: null });
    const msg = { id: 'x' } as MessageView;
    useStore.getState().setReply('S/A', msg);
    expect(useStore.getState().composerFocus).toEqual({ key: 'S/A', n: 1 });
    useStore.getState().setReply('S/A', msg);
    expect(useStore.getState().composerFocus).toEqual({ key: 'S/A', n: 2 });
    useStore.getState().setReply('S/A', undefined);
    expect(useStore.getState().composerFocus).toEqual({ key: 'S/A', n: 2 });
  });

  it('보내기에 성공하면 목록 키와 새 메시지 ID를 남기고, 실패하면 남기지 않는다', async () => {
    let fail = false;
    const call = vi.fn(async (method: string) => {
      if (method !== 'message.send') return {};
      if (fail) throw new RpcError(ErrorCode.Internal, '실패');
      return { id: 'E1' };
    });
    useStore.setState({ client: { call } as unknown as ApiClient, channels: {}, sent: null, toasts: [] });
    expect(await useStore.getState().send('S', 'C', '안녕', [], { thread: 'T' })).toBe(true);
    expect(useStore.getState().sent).toEqual({ key: 'S/C/T', id: 'E1', at: expect.any(Number) });
    fail = true;
    expect(await useStore.getState().send('S', 'D', '안녕', [])).toBe(false);
    expect(useStore.getState().sent?.id).toBe('E1');
  });
});

describe('OS 알림 설정·보고 있는 채널', () => {
  it('알림 설정 키는 아는 값만 받아들인다', () => {
    expect(sanitizePrefs({ notify: false, notifyCrew: 'mentions', notifyPreview: false })).toEqual({
      notify: false,
      notifyCrew: 'mentions',
      notifyPreview: false,
    });
    expect(sanitizePrefs({ notifyCrew: 'off' })).toEqual({ notifyCrew: 'off' });
    expect(sanitizePrefs({ notify: 'yes', notifyCrew: 'loud', notifyPreview: 1 })).toEqual({});
  });

  it('채널을 열면 그 채널을, 홈이나 채널 없는 공간이면 비워서 OS 알림 쪽에 알린다(active가 바뀔 때마다)', async () => {
    const view = {
      space: 'S',
      members: [],
      roles: [],
      channels: [{ channel: 'N', unread: 0, settings: { kind: 'voice', security: 'normal' } }],
    } as unknown as SpaceView;
    const empty = { space: 'E', members: [], roles: [], channels: [] } as unknown as SpaceView;
    const setNotifyViewing = vi.fn();
    const transport = { setContentProtection: vi.fn(async () => true), savePrefs: vi.fn(async () => undefined), setNotifyViewing };
    const call = vi.fn(async () => ({}));
    useStore.setState({ client: { call, transport } as unknown as ApiClient, views: { S: view, E: empty }, channels: {}, spaces: [] });
    await useStore.getState().selectChannel('S', 'N');
    expect(setNotifyViewing).toHaveBeenLastCalledWith('S', 'N');
    useStore.getState().selectHome();
    expect(setNotifyViewing).toHaveBeenLastCalledWith(null, null);
    await useStore.getState().selectChannel('S', 'N');
    await useStore.getState().selectSpace('E');
    expect(setNotifyViewing).toHaveBeenLastCalledWith(null, null);
  });
});

describe('신뢰 스위치(setTrust)', () => {
  const USER = { type: 'user', user: 'U', name: '민지' } as const;
  const CREW = { type: 'space', space: 'S', name: '게임 모임' } as const;

  function setup(skip = false) {
    const call = vi.fn(async (method: string) => (method === 'space.get' ? ({ space: 'S', members: [], channels: [], trusted: true } as unknown as SpaceView) : {}));
    const savePrefs = vi.fn(async (_p: Record<string, unknown>) => undefined);
    useStore.setState({
      client: { call, transport: { savePrefs } } as unknown as ApiClient,
      views: { S: { space: 'S', kind: 'crew', members: [], channels: [], trusted: false } as unknown as SpaceView },
      spaces: [],
      toasts: [],
      modal: { kind: 'friendProfile', user: 'U' },
      prefs: { ...useStore.getState().prefs, skipTrustWarning: skip },
    });
    return { call, savePrefs };
  }

  const warning = () => {
    const m = useStore.getState().modal;
    if (m?.kind !== 'trustWarning') throw new Error('경고 창이 없음');
    return m;
  };

  it('‘다시 보지 않기’는 불리언일 때만 설정에서 받아들인다', () => {
    expect(sanitizePrefs({ skipTrustWarning: true })).toEqual({ skipTrustWarning: true });
    expect(sanitizePrefs({ skipTrustWarning: false })).toEqual({ skipTrustWarning: false });
    expect(sanitizePrefs({ skipTrustWarning: 'yes' })).toEqual({});
  });

  it('트레이로 내리기는 불리언일 때만 받아들이고 기본은 켬', () => {
    expect(useStore.getState().prefs.closeToTray).toBe(true);
    expect(sanitizePrefs({ closeToTray: false })).toEqual({ closeToTray: false });
    expect(sanitizePrefs({ closeToTray: 'no' })).toEqual({});
  });

  it('윈도 시작 시 켜기는 불리언일 때만 받아들이고 기본은 켬', () => {
    expect(useStore.getState().prefs.autoStart).toBe(true);
    expect(sanitizePrefs({ autoStart: false })).toEqual({ autoStart: false });
    expect(sanitizePrefs({ autoStart: 'no' })).toEqual({});
  });

  it('시작 시 트레이로만 켜기는 불리언일 때만 받아들이고 기본은 끔', () => {
    expect(useStore.getState().prefs.autoStartToTray).toBe(false);
    expect(sanitizePrefs({ autoStartToTray: true })).toEqual({ autoStartToTray: true });
    expect(sanitizePrefs({ autoStartToTray: 1 })).toEqual({});
  });

  it('켜기는 연 창을 돌아갈 곳으로 둔 경고를 띄우고, 취소하면 보내지 않는다', async () => {
    const { call } = setup();
    const done = useStore.getState().setTrust(USER, true);
    const m = warning();
    expect(m.target).toEqual(USER);
    expect(m.back).toEqual({ kind: 'friendProfile', user: 'U' });
    m.onCancel();
    expect(await done).toBe(false);
    expect(call).not.toHaveBeenCalled();
  });

  it('확인하면 보내고, ‘다시 보지 않기’면 저장해 다음 켜기는 바로 보낸다', async () => {
    const { call, savePrefs } = setup();
    const first = useStore.getState().setTrust(USER, true);
    await warning().onConfirm(true);
    expect(await first).toBe(true);
    expect(call).toHaveBeenCalledWith('trust.set_user', { user: 'U', enabled: true });
    expect(useStore.getState().prefs.skipTrustWarning).toBe(true);
    expect(savePrefs).toHaveBeenLastCalledWith(expect.objectContaining({ skipTrustWarning: true }));

    useStore.setState({ modal: null });
    expect(await useStore.getState().setTrust(CREW, true)).toBe(true);
    expect(useStore.getState().modal).toBeNull();
    expect(call).toHaveBeenCalledWith('trust.set_space', { space: 'S', enabled: true });
    // 크루 스위치는 보기에 바로 반영하고 데몬 값으로 다시 읽습니다.
    expect(useStore.getState().views.S.trusted).toBe(true);
    await vi.waitFor(() => expect(call).toHaveBeenCalledWith('space.get', { space: 'S' }));
  });

  it('끄기는 경고 없이 바로 보낸다', async () => {
    const { call } = setup();
    expect(await useStore.getState().setTrust(USER, false)).toBe(true);
    expect(useStore.getState().modal).toEqual({ kind: 'friendProfile', user: 'U' });
    expect(call).toHaveBeenCalledWith('trust.set_user', { user: 'U', enabled: false });
  });

  it('데몬이 거절하면 거짓을 돌려주고 오류를 알린다', async () => {
    setup(true);
    const call = vi.fn(async () => {
      throw new RpcError(ErrorCode.NotFound, '친구·연락처나 같은 크루의 멤버가 아닙니다');
    });
    useStore.setState({ client: { call, transport: { savePrefs: vi.fn() } } as unknown as ApiClient });
    expect(await useStore.getState().setTrust(USER, true)).toBe(false);
    expect(useStore.getState().toasts.at(-1)).toMatchObject({ kind: 'error', text: '친구·연락처나 같은 크루의 멤버가 아닙니다' });
  });
});

describe('익명 채널(setChannelAnonymous)', () => {
  const TEXT = { channel: 'T', unread: 0, p2p: false, p2p_joined: false, anonymous: true, settings: { name: '건의함', kind: 'text', security: 'normal' } };
  const VOICE = { channel: 'V', unread: 0, p2p: true, p2p_joined: true, anonymous: true, settings: { name: '수다', kind: 'voice', security: 'normal' } };
  let anonymous = true;
  const viewOf = () => ({ space: 'S', members: [], roles: [], channels: [{ ...TEXT, anonymous }, { ...VOICE, anonymous }] }) as unknown as SpaceView;

  function setup() {
    anonymous = true;
    const call = vi.fn(async (method: string, params: { anonymous?: boolean }) => {
      if (method === 'channel.update' && params.anonymous !== undefined) anonymous = params.anonymous;
      if (method === 'space.get') return viewOf();
      return {};
    });
    useStore.setState({ client: { call } as unknown as ApiClient, views: { S: viewOf() }, channels: {}, spaces: [], modal: null, toasts: [], voice: null });
    return call;
  }

  afterEach(() => {
    useStore.setState({ client: null, views: {}, channels: {}, modal: null, voice: null, toasts: [] });
  });

  it('켜기와 글 채널 끄기는 확인 없이 바로 보낸다', async () => {
    const call = setup();
    useStore.getState().setChannelAnonymous('S', 'T', false);
    await vi.waitFor(() => expect(call).toHaveBeenCalledWith('channel.update', { space: 'S', channel: 'T', anonymous: false }));
    expect(useStore.getState().modal).toBeNull();
    useStore.getState().setChannelAnonymous('S', 'V', true);
    await vi.waitFor(() => expect(call).toHaveBeenCalledWith('channel.update', { space: 'S', channel: 'V', anonymous: true }));
    await vi.waitFor(() => expect(call).toHaveBeenCalledWith('space.get', { space: 'S' }));
  });

  it('통화 채널 끄기는 통화 중인 사람이 모두 나간다고 확인받은 뒤에만 보낸다', async () => {
    const call = setup();
    useStore.getState().setChannelAnonymous('S', 'V', false);
    expect(call).not.toHaveBeenCalled();
    const m = useStore.getState().modal;
    if (m?.kind !== 'confirm') throw new Error('확인 창이 없음');
    expect(m.body).toBe(ANON_VOICE_OFF_WARNING);
    expect(m.danger).toBe(true);
    await m.onConfirm();
    expect(call).toHaveBeenCalledWith('channel.update', { space: 'S', channel: 'V', anonymous: false });
    await vi.waitFor(() => expect(call).toHaveBeenCalledWith('space.get', { space: 'S' }));
  });

  it('익명으로 들어간 통화는 채널의 익명이 꺼진 보기를 받으면 통화에서 나오고 알린다', async () => {
    setup();
    anonymous = false;
    useStore.setState({ voice: { space: 'S', channel: 'V', anonymous: true } as never });
    useStore.getState().refreshSpace('S');
    await vi.waitFor(() => expect(useStore.getState().voice).toBeNull());
    expect(useStore.getState().toasts.at(-1)).toMatchObject({ kind: 'warn', text: ANON_CALL_ENDED });
  });

  it('익명 채널이 그대로면 통화를 끊지 않는다', async () => {
    const call = setup();
    useStore.setState({ voice: { space: 'S', channel: 'V', anonymous: true } as never });
    useStore.getState().refreshSpace('S');
    // space.list는 보기를 반영하고 통화를 검사한 다음에 읽으므로, 그때까지 기다립니다.
    await vi.waitFor(() => expect(call).toHaveBeenCalledWith('space.list', {}));
    expect(useStore.getState().voice).not.toBeNull();
  });
});

describe('포럼 새 글(createForumPost)', () => {
  afterEach(() => {
    useStore.setState({ client: null, toasts: [] });
  });

  it('올리면 알림을 기다리지 않고 글 목록을 다시 읽게 하고 글 ID를 돌려주며, 실패하면 그대로 둔다', async () => {
    let fail = false;
    const call = vi.fn(async () => {
      if (fail) throw new RpcError(ErrorCode.Forbidden, '권한이 없습니다');
      return { id: 'p1' };
    });
    useStore.setState({ client: { call } as unknown as ApiClient, forumTick: 3, toasts: [] });
    expect(await useStore.getState().createForumPost('S', 'F', { title: '제목', text: '본문', tags: [1], attachments: [], featured: false })).toBe('p1');
    // 빈 첨부·거짓 대문은 보내지 않습니다(옛 호출과 같은 모양).
    expect(call).toHaveBeenCalledWith('forum.create_post', { space: 'S', channel: 'F', title: '제목', text: '본문', tags: [1] });
    expect(useStore.getState().forumTick).toBe(4);
    fail = true;
    expect(await useStore.getState().createForumPost('S', 'F', { title: '제목', text: '본문', tags: [] })).toBeNull();
    expect(useStore.getState().forumTick).toBe(4);
  });

  it('첨부와 대문 지정을 싣는다', async () => {
    const call = vi.fn(async (method: string) => (method === 'forum.create_post' ? { id: 'p2' } : {}));
    useStore.setState({ client: { call } as unknown as ApiClient, forumTick: 0, toasts: [] });
    const attachments = [{ path: '/f/a.png', name: 'a.png' }];
    expect(await useStore.getState().createForumPost('S', 'F', { title: '대문', text: '', tags: [], attachments, featured: true })).toBe('p2');
    expect(call).toHaveBeenCalledWith('forum.create_post', { space: 'S', channel: 'F', title: '대문', text: '', tags: [], attachments, featured: true });
  });
});

describe('포럼 보기 설정(sanitizePrefs)', () => {
  it('아는 값만 받고 모르는 값은 기본값으로 둔다', () => {
    expect(
      sanitizePrefs({
        forumView: {
          'S/F': { sort: 'created', order: 'asc', view: 'gallery', match: 'all' },
          'S/G': { sort: 'x', order: 1, view: 'table' },
          'S/H': 'gallery',
        },
      }),
    ).toEqual({
      forumView: {
        'S/F': { sort: 'created', order: 'asc', view: 'gallery', match: 'all' },
        'S/G': { sort: 'activity', order: 'desc', view: 'list', match: 'any' },
      },
    });
    expect(sanitizePrefs({ forumView: 'x' })).toEqual({});
  });
});

describe('확인 창은 연 창 위에 겹친다(openModal)', () => {
  afterEach(() => useStore.setState({ modal: null }));
  const confirm = (extra: object = {}) => ({ kind: 'confirm' as const, title: '확인', body: '본문', onConfirm: () => undefined, ...extra });

  it('설정 창에서 열면 그 창을 back으로 삼고, 확인 창 위에서 다시 열면 같은 back을 물려받는다', () => {
    const settings = { kind: 'channelSettings' as const, space: 'S', channel: 'C' };
    useStore.getState().openModal(settings);
    useStore.getState().openModal(confirm());
    expect(useStore.getState().modal).toMatchObject({ kind: 'confirm', back: settings });
    useStore.getState().openModal(confirm({ title: '다시' }));
    expect(useStore.getState().modal).toMatchObject({ kind: 'confirm', title: '다시', back: settings });
  });

  it('창 없이 열거나 back: null을 주면 돌아갈 창이 없다', () => {
    useStore.getState().openModal(confirm());
    expect(useStore.getState().modal).toMatchObject({ back: null });
    useStore.getState().openModal({ kind: 'userSettings' });
    useStore.getState().openModal(confirm({ back: null }));
    expect(useStore.getState().modal).toMatchObject({ back: null });
  });
});

describe('화면 고르기 창', () => {
  it('고르지 않고 닫거나 다른 창으로 바뀌면 취소를 알리고, 고르면 그 화면만 알린다', () => {
    const onCancel = vi.fn();
    const onPick = vi.fn();
    const { openModal, closeModal } = useStore.getState();
    openModal({ kind: 'screenSource', onPick, onCancel });
    closeModal();
    expect(onCancel).toHaveBeenCalledTimes(1);
    openModal({ kind: 'screenSource', onPick, onCancel });
    openModal({ kind: 'addFriend' });
    expect(onCancel).toHaveBeenCalledTimes(2);
    closeModal();
    expect(onCancel).toHaveBeenCalledTimes(2);
  });
});

describe('후원자 전용 기능', () => {
  const locked = { active: false, providers: ['github' as const], links: [], shared: [] };
  afterEach(() => useStore.setState({ supporter: null, modal: null, library: false, client: null }));

  it('후원이 확인되지 않으면 라이브러리·콘텐츠 뷰어 대신 후원 확인 창을 연다', () => {
    useStore.setState({ supporter: locked, modal: null, library: false });
    useStore.getState().selectLibrary();
    expect(useStore.getState().library).toBe(false);
    expect(useStore.getState().modal).toEqual({ kind: 'supporter' });
    useStore.setState({ modal: null });
    useStore.getState().openModal({ kind: 'viewer', space: 's', channel: 'c' });
    expect(useStore.getState().modal).toEqual({ kind: 'supporter' });
  });

  it('후원 상태를 모르면(옛 데몬) 막지 않는다', () => {
    useStore.setState({ supporter: null, modal: null, library: false });
    useStore.getState().selectLibrary();
    expect(useStore.getState().library).toBe(true);
  });

  it('구독이 끝나 잠기면 열려 있던 라이브러리·뷰어를 닫는다', async () => {
    const call = vi.fn(async () => locked);
    useStore.setState({ client: { call } as unknown as ApiClient, supporter: { ...locked, active: true }, library: true });
    await useStore.getState().refreshSupporter();
    expect(call).toHaveBeenCalledWith('supporter.status', {});
    expect(useStore.getState().library).toBe(false);
    useStore.setState({ modal: { kind: 'viewer', space: 's', channel: 'c' }, supporter: { ...locked, active: true } });
    await useStore.getState().refreshSupporter();
    expect(useStore.getState().modal).toBeNull();
  });
});

describe('앱 업데이트', () => {
  it('새 판이면 배너 상태를 두고, 받기는 이 설치본의 파일 이름으로 데몬에 맡긴다', async () => {
    const call = vi.fn(async (method: string) => (method === 'update.check' ? { current: '1.0.0', latest: '1.2.0', newer: true } : {}));
    const target = vi.fn(async () => ({ kind: 'portable' as const, suffix: 'x64_portable.zip' }));
    const openExternal = vi.fn(async () => undefined);
    useStore.setState({ client: { call, transport: { update: { target, install: vi.fn() }, openExternal } } as unknown as ApiClient, update: null });
    await useStore.getState().checkUpdate();
    expect(useStore.getState().update).toEqual({ latest: '1.2.0' });
    await useStore.getState().downloadUpdate();
    expect(call).toHaveBeenCalledWith('update.download', { version: '1.2.0', asset: 'YANGPA_1.2.0_x64_portable.zip' });
    expect(useStore.getState().update?.progress).toEqual({ done: 0, total: 0 });
    expect(openExternal).not.toHaveBeenCalled();
  });

  it('앱 안에서 설치할 수 없는 설치본(Android·개발 빌드)은 릴리스 페이지를 연다', async () => {
    const call = vi.fn(async () => ({}));
    const openExternal = vi.fn(async () => undefined);
    useStore.setState({ client: { call, transport: { openExternal } } as unknown as ApiClient, update: { latest: '1.2.0' } });
    await useStore.getState().downloadUpdate();
    expect(openExternal).toHaveBeenCalledWith('https://github.com/dhtpdud/YANGPA/releases/tag/v1.2.0');
    expect(call).not.toHaveBeenCalled();
  });

  it('최신 판이면 상태를 두지 않고, 받기 진행 알림은 효과로 넘긴다', async () => {
    const call = vi.fn(async () => ({ current: '1.2.0', latest: '1.2.0', newer: false }));
    useStore.setState({ client: { call } as unknown as ApiClient, update: null });
    await useStore.getState().checkUpdate();
    expect(useStore.getState().update).toBeNull();
    const { effects } = applyBatch(useStore.getState(), [{ type: 'update.progress', version: '1.2.0', done: 5, total: 10 }]);
    expect(effects).toEqual([{ type: 'updateProgress', version: '1.2.0', done: 5, total: 10, ready: false, error: undefined }]);
  });

  it('설정 파일의 자동 확인 값을 읽는다', () => {
    expect(sanitizePrefs({ updateCheck: false })).toEqual({ updateCheck: false });
    expect(sanitizePrefs({ updateCheck: 'no' })).toEqual({});
  });
});
