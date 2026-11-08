import { describe, expect, it } from 'vitest';
import { ANONYMOUS_USER, type MessageView, type SpaceView } from '../api/types';
import {
  applyBatch,
  chKey,
  emptyChannel,
  mergeGaps,
  pruneExpired,
  pruneReplyTo,
  pruneTyping,
  type ApplyState,
} from './apply';

const ME = 'M'.repeat(32);
const OTHER = 'O'.repeat(32);

function msg(id: string, lamport: number, extra: Partial<MessageView> = {}): MessageView {
  return {
    id,
    author: OTHER,
    author_device: 'd',
    created_at: 1000 + lamport,
    lamport,
    text: 'hi',
    attachments: [],
    embeds: [],
    reactions: [],
    deleted: false,
    volatile: false,
    ...extra,
  };
}

function view(): SpaceView {
  return {
    space: 'S',
    kind: 'crew',
    settings: { name: '크루', description: '', icon: null, allow_external_emoji: false },
    owner: ME,
    committer: 'd',
    members: [{ user: OTHER, profile: { version: 1, name: '상대', bio: '', avatar: null, banner: null, theme_color: null, status: '' }, roles: [], online: false, automation: false }],
    roles: [],
    channels: [
      { channel: 'A', settings: {} as never, unread: 0, api_read_allowed: false, p2p: false, p2p_joined: false },
      { channel: 'B', settings: {} as never, unread: 0, api_read_allowed: false, p2p: false, p2p_joined: false },
    ],
    emojis: [],
    my_permissions: [],
    pending_commit: false,
    dormant: false,
    trusted: false,
  };
}

function state(patch: Partial<ApplyState> = {}): ApplyState {
  return {
    me: ME,
    active: chKey('S', 'A'),
    focused: true,
    channels: { [chKey('S', 'A')]: { ...emptyChannel(), loaded: true, messages: [msg('m1', 1)] } },
    spaces: [{ space: 'S', kind: 'crew', name: '크루', unread: 0, mention: 0, dormant: false, member_count: 2 }],
    views: { S: view() },
    friends: [],
    requests: [],
    confirmations: [],
    net: null,
    presence: null,
    flair: {},
    rates: null,
    blobProgress: {},
    voiceRosters: {},
    forumTick: 0,
    reportsNew: {},
    typing: {},
    ...patch,
  };
}

describe('알림 묶음 적용', () => {
  it('쓰는 중 알림을 채널별로 모으고, 글을 올리면 빼고, 시간이 지나면 지운다', () => {
    const key = chKey('S', 'A');
    const { patch } = applyBatch(state(), [
      { type: 'channel.typing', space: 'S', channel: 'A', user: OTHER },
      { type: 'channel.typing', space: 'S', channel: 'A', user: OTHER },
      { type: 'channel.typing', space: 'S', channel: 'A', user: ANONYMOUS_USER, anon_name: '졸린 수달' },
      { type: 'channel.typing', space: 'S', channel: 'A', user: ME },
    ]);
    expect(patch.typing![key].map((t) => t.id)).toEqual([OTHER, 'anon:졸린 수달']);
    const after = applyBatch(state({ typing: patch.typing }), [
      { type: 'message.new', space: 'S', channel: 'A', message: msg('m2', 2), notify: { title: 't' } },
    ]);
    expect(after.patch.typing![key].map((t) => t.id)).toEqual(['anon:졸린 수달']);
    const until = after.patch.typing![key][0].until;
    expect(pruneTyping(after.patch.typing!, until - 1)).toBeNull();
    expect(pruneTyping(after.patch.typing!, until)).toEqual({});
    // 쓰는 중이 아닌 사람의 글은 typing을 건드리지 않습니다.
    const none = applyBatch(state(), [
      { type: 'message.new', space: 'S', channel: 'A', message: msg('m3', 3), notify: { title: 't' } },
    ]);
    expect(none.patch.typing).toBeUndefined();
  });

  it('보고 있는 채널의 새 메시지를 넣고 읽음 처리한다', () => {
    const s = state();
    const { patch, effects } = applyBatch(s, [
      { type: 'message.new', space: 'S', channel: 'A', message: msg('m3', 3), notify: { title: 't' } },
      { type: 'message.new', space: 'S', channel: 'A', message: msg('m2', 2), notify: { title: 't' } },
    ]);
    expect(patch.channels![chKey('S', 'A')].messages.map((m) => m.id)).toEqual(['m1', 'm2', 'm3']);
    expect(effects.filter((e) => e.type === 'markRead')).toHaveLength(2);
    expect(patch.spaces).toBeUndefined();
    // 원래 상태는 그대로(불변)
    expect(s.channels[chKey('S', 'A')].messages).toHaveLength(1);
  });

  it('잠긴 P2P 전용 채널은 열어 두어도 읽음 처리하지 않고 안 읽음 수를 올린다', () => {
    const v = view();
    v.channels[0] = { ...v.channels[0], p2p: true, p2p_joined: false };
    const { patch, effects } = applyBatch(state({ views: { S: v }, channels: {} }), [
      { type: 'message.new', space: 'S', channel: 'A', message: msg('m2', 2), notify: { title: 't' } },
    ]);
    expect(effects.filter((e) => e.type === 'markRead')).toHaveLength(0);
    expect(patch.views!.S.channels[0].unread).toBe(1);
  });

  it('보고 있는 채널의 글타래 답글·포럼 댓글도 읽음 처리한다', () => {
    const { effects } = applyBatch(state(), [
      { type: 'message.new', space: 'S', channel: 'A', message: msg('t1', 4, { thread: 'm1' }), notify: { title: 't' } },
    ]);
    expect(effects).toContainEqual({ type: 'markRead', space: 'S', channel: 'A', event: 't1' });
  });

  it('다른 채널이면 안 읽음·멘션 수를 올린다', () => {
    const s = state();
    const { patch } = applyBatch(s, [
      { type: 'message.new', space: 'S', channel: 'B', message: msg('x', 5, { text: `<@${ME}> 봐` }), notify: { title: 't' } },
      { type: 'message.new', space: 'S', channel: 'B', message: msg('y', 6), notify: { title: 't' } },
      { type: 'message.new', space: 'S', channel: 'B', message: msg('z', 7, { author: ME }), notify: { title: 't' } },
    ]);
    expect(patch.spaces![0]).toMatchObject({ unread: 2, mention: 1 });
    expect(patch.views!.S.channels.find((c) => c.channel === 'B')!.unread).toBe(2);
    expect(patch.channels).toBeUndefined();
  });

  it('전송 속도 알림은 묶음의 마지막 값으로 바꾸고, 받는 사람이 없으면 uploads 없이 둔다', () => {
    const up = { space: 'S', blob: 'B', user: OTHER, bps: 300 };
    const { patch, effects } = applyBatch(state(), [
      { type: 'transfer.rates', up_bps: 10, down_bps: 20, uploads: [up] },
      { type: 'transfer.rates', up_bps: 30, down_bps: 40, uploads: [up] },
    ]);
    expect(patch.rates).toEqual({ up_bps: 30, down_bps: 40, uploads: [up] });
    expect(effects).toEqual([]);
    const next = applyBatch(state({ rates: patch.rates! }), [{ type: 'transfer.rates', up_bps: 0, down_bps: 0 }]);
    expect(next.patch.rates).toEqual({ up_bps: 0, down_bps: 0 });
    // 다른 알림만 있으면 속도는 그대로입니다.
    expect(applyBatch(state(), [{ type: 'space.updated', space: 'S' }]).patch.rates).toBeUndefined();
  });

  it('만료 알림은 채널과 그 글타래에서 지운다', () => {
    const s = state({
      channels: {
        [chKey('S', 'A')]: { ...emptyChannel(), loaded: true, messages: [msg('a', 1), msg('b', 2)] },
        [chKey('S', 'A', 'T')]: { ...emptyChannel(), loaded: true, messages: [msg('c', 3)] },
      },
    });
    const { patch } = applyBatch(s, [{ type: 'message.expired', space: 'S', channel: 'A', ids: ['a', 'c'] }]);
    expect(patch.channels![chKey('S', 'A')].messages.map((m) => m.id)).toEqual(['b']);
    expect(patch.channels![chKey('S', 'A', 'T')].messages).toEqual([]);
  });

  it('공간 갱신은 한 번만, 접속·음성·진행률을 반영한다', () => {
    const { patch, effects } = applyBatch(state(), [
      { type: 'space.updated', space: 'S' },
      { type: 'space.updated', space: 'S' },
      { type: 'presence', user: OTHER, online: true },
      {
        type: 'voice.participants',
        space: 'S',
        channel: 'B',
        participants: [{ user: OTHER, device: 'd1', muted: true, deafened: false, screen: false }],
        generation: 2,
        media_key: 'AAAA',
        topology: 'sfu',
        sfu_host: 'd1',
        sfu_host_user: OTHER,
      },
      { type: 'blob.progress', space: 'S', blob: 'X', have: 1, total: 4 },
      { type: 'events.dropped', count: 3 },
    ]);
    expect(effects.filter((e) => e.type === 'refreshSpace')).toHaveLength(1);
    expect(effects.some((e) => e.type === 'resync')).toBe(true);
    // 토폴로지와 SFU 호스트도 통화 세션에 넘깁니다.
    expect(effects.find((e) => e.type === 'voiceRoster')).toMatchObject({ topology: 'sfu', sfuHost: 'd1', sfuHostUser: OTHER, generation: 2 });
    expect(patch.views!.S.members[0].online).toBe(true);
    expect(patch.views!.S.channels.find((c) => c.channel === 'B')!.participants).toEqual([OTHER]);
    expect(patch.voiceRosters![chKey('S', 'B')]).toMatchObject({ generation: 2, topology: 'sfu', sfuHost: 'd1', sfuHostUser: OTHER });
    expect(patch.blobProgress!.X).toEqual({ have: 1, total: 4 });
  });

  it('자리 비움·마지막 동기화와 내 표시 상태를 반영한다', () => {
    const away = applyBatch(state(), [{ type: 'presence', user: OTHER, online: true, away: true }]).patch;
    expect(away.views!.S.members[0]).toMatchObject({ online: true, away: true });
    const gone = applyBatch(state(), [{ type: 'presence', user: OTHER, online: false, last_seen: 1234 }]).patch;
    expect(gone.views!.S.members[0]).toMatchObject({ online: false, away: false, last_seen: 1234 });
    const self = { mode: 'invisible' as const, until: 99, idle: false, hide_last_seen: true, offline: false };
    expect(applyBatch(state(), [{ type: 'presence.self', ...self }]).patch.presence).toEqual(self);
  });

  it('만료 시각이 지난 메시지를 화면에서 바로 지운다', () => {
    const channels = { k: { ...emptyChannel(), loaded: true, messages: [msg('a', 1, { expires_at: 500 }), msg('b', 2, { expires_at: 5000 }), msg('c', 3)] } };
    const out = pruneExpired(channels, 1000);
    expect(out!.k.messages.map((m) => m.id)).toEqual(['b', 'c']);
    expect(pruneExpired(out!, 1000)).toBeNull();
  });

  it('새 기기가 허용되면 신원을 읽어 첫 동기화 화면으로 가고, 끝나면 알린다', () => {
    const approved = applyBatch(state(), [{ type: 'device.link_result', status: 'approved' }]).effects;
    expect(approved).toEqual([{ type: 'deviceLinkResult', status: 'approved' }, { type: 'loadAll' }]);
    const rejected = applyBatch(state(), [{ type: 'device.link_result', status: 'rejected' }]).effects;
    expect(rejected).toEqual([{ type: 'deviceLinkResult', status: 'rejected' }]);
    const synced = applyBatch(state(), [{ type: 'device.synced' }]).effects;
    expect(synced.filter((e) => e.type === 'toast').map((t) => (t.type === 'toast' ? t.kind : null))).toEqual(['success']);
    expect(synced.some((e) => e.type === 'refreshIdentity')).toBe(true);
  });

  it('기기 폐기는 다시 불러오고, 새 기기의 연결 요청은 허용 창을 띄우게 한다', () => {
    const request = { name: '새 휴대폰', code: '123456', contested: false };
    const { effects } = applyBatch(state(), [{ type: 'device.link_request', ...request }, { type: 'device.wiped' }]);
    expect(effects.filter((e) => e.type === 'toast').map((t) => (t.type === 'toast' ? t.kind : null))).toEqual(['error']);
    expect(effects.some((e) => e.type === 'resync')).toBe(true);
    expect(effects).toContainEqual({ type: 'deviceLinkRequest', request });
  });

  it('같은 구간은 한 번만 합친다', () => {
    expect(mergeGaps([{ after: 'a', before: 'b' }], [{ after: 'a', before: 'b' }, { before: 'c' }])).toEqual([{ after: 'a', before: 'b' }, { before: 'c' }]);
  });
});

describe('답장 대상에 잡아 둔 휘발성 메시지', () => {
  const loaded = (messages: MessageView[]) => ({ ...emptyChannel(), loaded: true, messages });

  it('만료 알림으로 채널에서 사라지면 답장 미리보기 사본도 지운다', () => {
    const v = msg('v1', 1, { volatile: true, text: '비밀' });
    const key = chKey('S', 'A');
    const st = state({ channels: { [key]: loaded([v]) } });
    const { patch } = applyBatch(st, [{ type: 'message.expired', space: 'S', channel: 'A', ids: ['v1'] }]);
    const out = pruneReplyTo({ [key]: v }, patch.channels!, 0);
    expect(out).toEqual({});
  });

  it('휘발성 채널 내용을 버리면(다른 곳으로 이동) 사본도 지우고, 일반 메시지는 채널 상태가 없어도 둔다', () => {
    const v = msg('v1', 1, { volatile: true });
    const n = msg('n1', 1);
    const out = pruneReplyTo({ 'S/V': v, 'S/N': n }, {}, 0);
    expect(out).toEqual({ 'S/N': n });
  });

  it('수명이 끝났거나 지워진 메시지는 지우고, 아직 살아 있으면 그대로(null)', () => {
    const key = 'S/A';
    const live = msg('a', 1, { volatile: true, expires_at: 5000 });
    expect(pruneReplyTo({ [key]: live }, { [key]: loaded([live]) }, 1000)).toBeNull();
    expect(pruneReplyTo({ [key]: live }, { [key]: loaded([live]) }, 5000)).toEqual({});
    const n = msg('n', 2);
    expect(pruneReplyTo({ [key]: n }, { [key]: loaded([{ ...n, deleted: true, text: '' }]) }, 0)).toEqual({});
  });
});

describe('모더레이션 알림', () => {
  it('report.new는 공간별 새 신고 수를 늘리고 내용 없이 알린다', () => {
    const s = state({ reportsNew: { S: 1 } });
    const { patch, effects } = applyBatch(s, [
      { type: 'report.new', space: 'S', report_id: 'r1' },
      { type: 'report.new', space: 'T', report_id: 'r2' },
    ]);
    expect(patch.reportsNew).toEqual({ S: 2, T: 1 });
    // 원래 상태는 그대로(불변)
    expect(s.reportsNew).toEqual({ S: 1 });
    const toasts = effects.filter((e) => e.type === 'toast');
    expect(toasts).toHaveLength(2);
    expect(toasts[0]).toMatchObject({ kind: 'warn' });
    expect(JSON.stringify(toasts[0])).toContain('「크루」');
  });
});

describe('프로필 변경', () => {
  const friend = (user: string) => ({
    user,
    profile: view().members[0].profile,
    space: 'F',
    online: true,
    safety_number_verified: false,
    kind: 'friend' as const,
    proposed: false,
  });

  it('친구면 친구 목록을 한 번, 멤버로 있는 공간은 다시 읽는다', () => {
    const s = state({ friends: [friend(OTHER), friend('X'.repeat(32))] });
    const { effects } = applyBatch(s, [
      { type: 'profile.updated', user: OTHER },
      { type: 'profile.updated', user: 'X'.repeat(32) },
    ]);
    expect(effects.filter((e) => e.type === 'refreshFriends')).toHaveLength(1);
    expect(effects.filter((e) => e.type === 'refreshSpace')).toEqual([{ type: 'refreshSpace', space: 'S' }]);
  });

  it('내 프로필이면 내 신원을 한 번 다시 읽는다', () => {
    const { effects } = applyBatch(state({ me: OTHER }), [
      { type: 'profile.updated', user: OTHER },
      { type: 'profile.updated', user: OTHER },
    ]);
    expect(effects.filter((e) => e.type === 'refreshIdentity')).toHaveLength(1);
  });

  it('모르는 사람이면 아무것도 하지 않는다', () => {
    const { effects, patch } = applyBatch(state(), [{ type: 'profile.updated', user: 'Z'.repeat(32) }]);
    expect(effects).toEqual([]);
    expect(patch).toEqual({});
  });
});

describe('연락처 변경', () => {
  it('friend.updated는 친구 목록과 공간 목록을 묶음마다 한 번 다시 읽는다', () => {
    const { effects, patch } = applyBatch(state(), [
      { type: 'friend.updated', user: OTHER },
      { type: 'friend.updated', user: 'X'.repeat(32) },
    ]);
    expect(effects).toEqual([{ type: 'refreshFriends' }, { type: 'refreshSpaces' }]);
    expect(patch).toEqual({});
  });
});

describe('포럼 게시글 알림', () => {
  function forumState(patch: Partial<ApplyState> = {}) {
    const v = view();
    v.channels[1] = { ...v.channels[1], settings: { kind: 'forum' } as never };
    return state({ views: { S: v }, ...patch });
  }

  it('남의 새 글·내 새 글 모두 글 목록을 다시 읽게 한다', () => {
    const s = forumState();
    const { patch } = applyBatch(s, [
      { type: 'message.new', space: 'S', channel: 'B', message: msg('p1', 5), notify: { title: 't' } },
      { type: 'message.new', space: 'S', channel: 'B', message: msg('p2', 6, { author: ME }), notify: { title: 't' } },
    ]);
    expect(patch.forumTick).toBe(2);
  });

  it('글 수정(글타래 없는 message.updated)은 목록과 열어 둔 글타래의 본문을 바꾼다', () => {
    const post = msg('p1', 1, { text: '본문' });
    const s = forumState({
      channels: { [chKey('S', 'B', 'p1')]: { ...emptyChannel(), loaded: true, reachedStart: true, messages: [post, msg('c1', 2, { thread: 'p1' })] } },
    });
    const { patch } = applyBatch(s, [{ type: 'message.updated', space: 'S', channel: 'B', message: { ...post, text: '고친 본문', edited_at: 9 } }]);
    expect(patch.forumTick).toBe(1);
    expect(patch.channels![chKey('S', 'B', 'p1')].messages.map((m) => m.text)).toEqual(['고친 본문', 'hi']);
  });

  it('포럼이 아닌 채널의 메시지는 글 목록 신호를 올리지 않고 글타래 키에도 넣지 않는다', () => {
    const s = state({
      channels: {
        [chKey('S', 'A')]: { ...emptyChannel(), loaded: true, messages: [] },
        [chKey('S', 'A', 'x')]: { ...emptyChannel(), loaded: true, messages: [] },
      },
    });
    const { patch } = applyBatch(s, [{ type: 'message.new', space: 'S', channel: 'A', message: msg('x', 5), notify: { title: 't' } }]);
    expect(patch.forumTick ?? 0).toBe(0);
    expect(patch.channels![chKey('S', 'A', 'x')].messages).toEqual([]);
  });
});

describe('익명 채널 알림', () => {
  it('가려진 내 글(mine)은 안 읽음 수를 올리지 않는다', () => {
    const { patch } = applyBatch(state(), [
      { type: 'message.new', space: 'S', channel: 'B', message: msg('a1', 5, { author: ANONYMOUS_USER, anonymous: true, mine: true }), notify: { title: 't' } },
      { type: 'message.new', space: 'S', channel: 'B', message: msg('a2', 6, { author: ANONYMOUS_USER, anonymous: true }), notify: { title: 't' } },
    ]);
    expect(patch.views!.S.channels.find((c) => c.channel === 'B')!.unread).toBe(1);
  });

  it('익명 참가자 목록은 같은 자리 표시여도 사람 수를 그대로 둔다', () => {
    const anon = (device: string, n: number, mine = false) => ({ user: ANONYMOUS_USER, device, muted: false, deafened: false, screen: false, anon: n, mine });
    const { patch } = applyBatch(state(), [
      {
        type: 'voice.participants',
        space: 'S',
        channel: 'B',
        participants: [anon(ANONYMOUS_USER, 1), anon(ANONYMOUS_USER, 2, true), anon(ANONYMOUS_USER, 3)],
        generation: 1,
        media_key: 'AAAA',
        topology: 'mesh',
      },
    ]);
    expect(patch.views!.S.channels.find((c) => c.channel === 'B')!.participants).toEqual([ANONYMOUS_USER, ANONYMOUS_USER, ANONYMOUS_USER]);
  });
});
