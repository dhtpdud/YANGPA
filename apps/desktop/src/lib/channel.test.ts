import { describe, expect, it } from 'vitest';
import type { ChannelSettings, ChannelView } from '../api/types';
import { EVERYONE_ROLE } from '../api/types';
import {
  ATTACH_LIMIT_MIN,
  parseAttachLimit,
  volatileLimitTooBig,
  attachLimitMib,
  canBeP2p,
  canHaveAttachLimit,
  channelCreateParams,
  channelUpdatePatch,
  isP2pLocked,
  leaveEffectText,
  parseVolatileCap,
  planChannelMove,
  ttlParams,
  volatileLifetimeText,
  type ChannelForm,
} from './channel';

function settings(over: Partial<ChannelSettings> = {}): ChannelSettings {
  return {
    name: '잡담',
    kind: 'text',
    topic: '',
    parent: null,
    position: 0,
    security: 'normal',
    join_mode: 'always',
    ttl: { mode: 'none' },
    history: 'private',
    link_previews: true,
    overwrites: [],
    forum_tags: [],
    ...over,
  };
}

function formOf(s: ChannelSettings, over: Partial<ChannelForm> = {}): ChannelForm {
  return {
    name: s.name,
    topic: s.topic,
    parent: s.parent,
    joinMode: s.join_mode,
    ttl: s.ttl,
    history: s.history,
    linkPreviews: s.link_previews,
    overwrites: s.overwrites,
    ...over,
  };
}

describe('수명 → API 필드(CLI와 같은 규칙)', () => {
  it('퇴장 시 삭제는 ttl_on_leave만 보낸다(ttl_secs 0을 함께 보내지 않음)', () => {
    expect(ttlParams({ mode: 'on_leave' }, true)).toEqual({ ttl_on_leave: true });
  });
  it('초 단위는 ttl_secs만, 범위를 넘으면 자른다', () => {
    expect(ttlParams({ mode: 'seconds', seconds: 30 }, true)).toEqual({ ttl_secs: 30 });
    expect(ttlParams({ mode: 'seconds', seconds: 0 }, false)).toEqual({ ttl_secs: 5 });
  });
  it('없음: 만들 때는 빼고, 바꿀 때는 지우는 값을 보낸다', () => {
    expect(ttlParams({ mode: 'none' }, false)).toEqual({});
    expect(ttlParams({ mode: 'none' }, true)).toEqual({ ttl_secs: 0, ttl_on_leave: false });
  });
});

describe('channel.update는 바뀐 필드만 보낸다', () => {
  it('아무것도 안 바꾸면 빈 인자 — CLI로 정한 입장 방식·수명을 덮어쓰지 않는다', () => {
    const s = settings({ join_mode: 'join_leave', ttl: { mode: 'seconds', seconds: 600 } });
    expect(channelUpdatePatch(s, formOf(s))).toEqual({});
  });

  it('일반 등급 채널에서도 입장 방식·수명을 바꿀 수 있다', () => {
    const s = settings();
    expect(channelUpdatePatch(s, formOf(s, { joinMode: 'join_leave', ttl: { mode: 'on_leave' } }))).toEqual({
      join_mode: 'join_leave',
      ttl_on_leave: true,
    });
  });

  it('수명을 없음으로 되돌리면 ttl_secs 0 + ttl_on_leave false', () => {
    const s = settings({ security: 'volatile', ttl: { mode: 'on_leave' } });
    expect(channelUpdatePatch(s, formOf(s, { ttl: { mode: 'none' } }))).toEqual({ ttl_secs: 0, ttl_on_leave: false });
  });

  it('카테고리를 바꾸지 않으면 parent를 보내지 않고, 빼면 null', () => {
    const s = settings({ parent: 'cat1' });
    expect(channelUpdatePatch(s, formOf(s, { name: ' 새이름 ' }))).toEqual({ name: '새이름' });
    expect(channelUpdatePatch(s, formOf(s, { parent: null }))).toEqual({ parent: null });
  });

  it('권한 덮어쓰기는 순서·빈 항목과 상관없이 달라질 때만 목록 전체를 보내고(private_roles는 쓰지 않음), 카테고리도 같다', () => {
    const everyone = { target: { type: 'role' as const, id: EVERYONE_ROLE }, allow: 0, deny: 1 };
    const r1 = { target: { type: 'role' as const, id: 'r1' }, allow: 1, deny: 0 };
    const empty = { target: { type: 'member' as const, id: 'u1' }, allow: 0, deny: 0 };
    const s = settings({ overwrites: [everyone, r1] });
    expect(channelUpdatePatch(s, formOf(s, { overwrites: [r1, empty, everyone] }))).toEqual({});
    expect(channelUpdatePatch(s, formOf(s, { overwrites: [r1, empty] }))).toEqual({ overwrites: [r1] });
    expect(channelUpdatePatch(s, formOf(s, { overwrites: [] }))).toEqual({ overwrites: [] });
    const cat = settings({ kind: 'category' });
    expect(channelUpdatePatch(cat, formOf(cat, { overwrites: [everyone] }))).toEqual({ overwrites: [everyone] });
  });

  it('통화 채널에는 입장 방식·수명·기록 필드를 보내지 않는다', () => {
    const s = settings({ kind: 'voice' });
    expect(channelUpdatePatch(s, formOf(s, { joinMode: 'join_leave', ttl: { mode: 'on_leave' }, history: 'public' }))).toEqual({});
  });
});

describe('channel.create 인자', () => {
  const base: ChannelForm = {
    name: ' 비밀 ',
    topic: '',
    parent: null,
    joinMode: 'join_leave',
    ttl: { mode: 'seconds', seconds: 30 },
    history: 'private',
    linkPreviews: false,
    overwrites: [],
  };

  it('휘발성 입장·퇴장 채널(기획서 CLI 예시와 같은 값)', () => {
    expect(channelCreateParams('sp', 'text', 'volatile', base)).toEqual({
      space: 'sp',
      kind: 'text',
      name: '비밀',
      security: 'volatile',
      join_mode: 'join_leave',
      ttl_secs: 30,
      history: 'private',
      link_previews: false,
    });
  });

  it('권한은 빈 항목을 빼고 있을 때만 overwrites로 보내고, 카테고리는 상위·보안 등급을 갖지 않는다', () => {
    const deny = { target: { type: 'role' as const, id: EVERYONE_ROLE }, allow: 0, deny: 1 };
    const empty = { target: { type: 'role' as const, id: 'r1' }, allow: 0, deny: 0 };
    expect(channelCreateParams('sp', 'text', 'normal', { ...base, overwrites: [empty] })).not.toHaveProperty('overwrites');
    const p = channelCreateParams('sp', 'category', 'volatile', { ...base, parent: 'x', overwrites: [deny, empty] });
    expect(p).toEqual({ space: 'sp', kind: 'category', name: '비밀', security: 'normal', overwrites: [deny] });
    expect(p).not.toHaveProperty('private_roles');
  });

  it('P2P 전용은 텍스트·통화 채널이고 내 기기끼리 공간이 아닐 때만 보낸다', () => {
    expect(channelCreateParams('sp', 'text', 'volatile', { ...base, p2p: true }, 'crew')).toMatchObject({ p2p: true, security: 'volatile' });
    expect(channelCreateParams('sp', 'voice', 'normal', { ...base, p2p: true }, 'space').p2p).toBe(true);
    expect(channelCreateParams('sp', 'forum', 'normal', { ...base, p2p: true }, 'crew')).not.toHaveProperty('p2p');
    expect(channelCreateParams('sp', 'category', 'normal', { ...base, p2p: true }, 'crew')).not.toHaveProperty('p2p');
    expect(channelCreateParams('sp', 'text', 'normal', { ...base, p2p: true }, 'self_space')).not.toHaveProperty('p2p');
    expect(channelCreateParams('sp', 'text', 'normal', base, 'crew')).not.toHaveProperty('p2p');
    expect(canBeP2p('voice', 'crew')).toBe(true);
    expect(canBeP2p('text', 'self_space')).toBe(false);
  });

  it('P2P 전용 채널은 이 기기가 참가하기 전까지만 잠긴다', () => {
    expect(isP2pLocked({ p2p: true, p2p_joined: false })).toBe(true);
    expect(isP2pLocked({ p2p: true, p2p_joined: true })).toBe(false);
    expect(isP2pLocked({ p2p: false, p2p_joined: false })).toBe(false);
    expect(isP2pLocked(undefined)).toBe(false);
  });
});

describe('휘발성 안내 문구는 설정과 다른 약속을 하지 않는다', () => {
  it('수명이 없으면 퇴장 시 삭제를 약속하지 않는다', () => {
    const t = volatileLifetimeText({ ttl: { mode: 'none' }, join_mode: 'always' });
    expect(t).not.toContain('퇴장');
    expect(t).toContain('앱을 끄면');
  });
  it('초 단위 수명은 기기별 수신 시점 기준', () => {
    expect(volatileLifetimeText({ ttl: { mode: 'seconds', seconds: 300 }, join_mode: 'always' })).toContain('받은 시점부터 5분');
  });
  it('퇴장 시 삭제는 입장·퇴장 채널에서만 "퇴장"이라 부른다', () => {
    expect(volatileLifetimeText({ ttl: { mode: 'on_leave' }, join_mode: 'join_leave' })).toContain('퇴장하면');
    expect(volatileLifetimeText({ ttl: { mode: 'on_leave' }, join_mode: 'always' })).toContain('나가면');
  });
  it('퇴장 효과: 퇴장 시 삭제 수명일 때만 바로 지운다고 말한다', () => {
    expect(leaveEffectText({ ttl: { mode: 'on_leave' }, security: 'volatile' })).toContain('바로 지웁니다');
    expect(leaveEffectText({ ttl: { mode: 'none' }, security: 'volatile' })).toContain('메모리에 남습니다');
    expect(leaveEffectText({ ttl: { mode: 'seconds', seconds: 60 }, security: 'normal' })).toContain('기기에 남습니다');
  });
});

describe('포럼 태그(forum_tags, 목록 전체)', () => {
  it('만들 때는 포럼이고 태그가 있을 때만 다듬어 보낸다', () => {
    const s = settings({ kind: 'forum' });
    const tags = [{ id: 3, name: ' 🎉 공지 ' }];
    expect(channelCreateParams('sp', 'forum', 'normal', formOf(s, { forumTags: tags })).forum_tags).toEqual([{ id: 3, name: '🎉 공지' }]);
    expect(channelCreateParams('sp', 'forum', 'normal', formOf(s, { forumTags: [] }))).not.toHaveProperty('forum_tags');
    expect(channelCreateParams('sp', 'text', 'normal', formOf(s, { forumTags: tags }))).not.toHaveProperty('forum_tags');
  });

  it('바꿀 때는 달라졌을 때만, 예전 이모지는 그대로 둔다', () => {
    const s = settings({ kind: 'forum', forum_tags: [{ id: 1, name: '질문', emoji: '❓' }] });
    expect(channelUpdatePatch(s, formOf(s, { forumTags: [{ id: 1, name: '질문 ', emoji: '❓' }] }))).toEqual({});
    expect(channelUpdatePatch(s, formOf(s))).toEqual({});
    expect(channelUpdatePatch(s, formOf(s, { forumTags: [{ id: 1, name: '물어봐요', emoji: '❓' }, { id: 77, name: '버그' }] }))).toEqual({
      forum_tags: [
        { id: 1, name: '물어봐요', emoji: '❓' },
        { id: 77, name: '버그' },
      ],
    });
    expect(channelUpdatePatch(s, formOf(s, { forumTags: [] }))).toEqual({ forum_tags: [] });
  });
});

describe('첨부 한도(attach_limit, 바이트, null이면 무제한)', () => {
  const MIB = 1024 * 1024;

  it('크루의 카테고리가 아닌 채널만', () => {
    expect(canHaveAttachLimit('text', 'crew')).toBe(true);
    expect(canHaveAttachLimit('forum', 'crew')).toBe(true);
    expect(canHaveAttachLimit('category', 'crew')).toBe(false);
    expect(canHaveAttachLimit('text', 'space')).toBe(false);
  });

  it('MiB 입력: 비우면 무제한, 0은 첨부 금지, 그 밖에는 1MiB 이상', () => {
    expect(ATTACH_LIMIT_MIN).toBe(MIB);
    expect(parseAttachLimit('8')).toEqual({ ok: true, bytes: 8 * MIB });
    expect(parseAttachLimit(' 1.5 ')).toEqual({ ok: true, bytes: 1.5 * MIB });
    // 비우면 무제한, 0은 첨부 금지, 0과 1MiB 사이는 안 됨.
    expect(parseAttachLimit('')).toEqual({ ok: true, bytes: null });
    expect(parseAttachLimit('0')).toEqual({ ok: true, bytes: 0 });
    expect(parseAttachLimit('0.5').ok).toBe(false);
    expect(parseAttachLimit('abc').ok).toBe(false);
    expect(parseAttachLimit('-1').ok).toBe(false);
    expect(volatileLimitTooBig(10 * MIB)).toBe(false);
    expect(volatileLimitTooBig(10 * MIB + 1)).toBe(true);
    expect(volatileLimitTooBig(null)).toBe(true);
    expect(volatileLimitTooBig(0)).toBe(false);
    expect(attachLimitMib(8 * MIB)).toBe('8');
    expect(attachLimitMib(1.25 * MIB)).toBe('1.25');
  });

  it('바뀌었을 때만 보낸다(고칠 수 없으면 보내지 않음)', () => {
    const s = settings();
    expect(channelUpdatePatch(s, formOf(s))).toEqual({});
    expect(channelUpdatePatch(s, formOf(s, { attachLimit: null }))).toEqual({});
    expect(channelUpdatePatch(s, formOf(s, { attachLimit: 8 * MIB }))).toEqual({ attach_limit: 8 * MIB });
    expect(channelUpdatePatch(s, formOf(s, { attachLimit: 8 * MIB }), { attach_limit: 8 * MIB })).toEqual({});
    expect(channelUpdatePatch(s, formOf(s, { attachLimit: null }), { attach_limit: 8 * MIB })).toEqual({ attach_limit: null });
    expect(channelUpdatePatch(s, formOf(s, { attachLimit: 0 }))).toEqual({ attach_limit: 0 });
    // 수정·삭제 금지는 바뀐 것만.
    expect(channelUpdatePatch(s, formOf(s, { noEdit: false, noDelete: false }))).toEqual({});
    expect(channelUpdatePatch(s, formOf(s, { noEdit: true, noDelete: false }))).toEqual({ no_edit: true });
    expect(channelUpdatePatch(s, formOf(s, { noEdit: true, noDelete: false }), { no_edit: true, no_delete: true })).toEqual({ no_delete: false });
    expect(channelUpdatePatch(s, formOf(s), { attach_limit: 8 * MIB })).toEqual({});
  });
});

describe('상시 휘발성 채널 참가 상한', () => {
  it('2..=2000 정수만 받습니다', () => {
    expect(parseVolatileCap(' 300 ')).toEqual({ ok: true, cap: 300 });
    for (const bad of ['', '1', '2001', '2.5', 'abc']) expect(parseVolatileCap(bad).ok).toBe(false);
  });
  it('지금 값과 다를 때만 보냅니다', () => {
    const s = settings({ security: 'volatile' });
    expect(channelUpdatePatch(s, formOf(s, { volatileCap: 200 }), { volatile_cap: 200 })).toEqual({});
    expect(channelUpdatePatch(s, formOf(s, { volatileCap: 50 }), { volatile_cap: 200 })).toEqual({ volatile_cap: 50 });
    expect(channelUpdatePatch(s, formOf(s), { volatile_cap: 200 })).toEqual({});
  });
});

describe('끌어 옮기기(planChannelMove)', () => {
  const ch = (channel: string, position: number, over: Partial<ChannelSettings> = {}) => ({ channel, unread: 0, settings: settings({ name: channel, position, ...over }) }) as ChannelView;
  // sortChannels 순서: 최상위 채널 → 카테고리마다 머리글과 안의 채널. 위치가 겹치는 것(데몬 기본값)도 섞었습니다.
  const cat = (id: string, position: number) => ch(id, position, { kind: 'category' });
  const sorted = [
    ch('a', 0),
    ch('b', 5),
    ch('c', 5),
    cat('X', 0),
    ch('x1', 0, { parent: 'X' }),
    ch('x2', 1, { parent: 'X' }),
    cat('Y', 1),
    ch('y1', 0, { parent: 'Y' }),
  ];

  it('같은 묶음 안에서는 보이는 순서대로 다시 매기고 바뀐 위치만 보낸다', () => {
    expect(planChannelMove(sorted, 'c', 'a', 'before')).toEqual([
      { channel: 'c', position: 0 },
      { channel: 'a', position: 1 },
      { channel: 'b', position: 2 },
    ]);
    expect(planChannelMove(sorted, 'x1', 'x2', 'after')).toEqual([
      { channel: 'x2', position: 0 },
      { channel: 'x1', position: 1 },
    ]);
    // 제자리에 놓으면 보낼 것이 없습니다(b·c의 겹친 위치만 고침).
    expect(planChannelMove(sorted, 'x1', 'x2', 'before')).toEqual([]);
    expect(planChannelMove(sorted, 'a', 'a', 'after')).toEqual([]);
  });

  it('다른 카테고리·최상위로 옮기면 상위도 바꾼다', () => {
    expect(planChannelMove(sorted, 'a', 'y1', 'after')).toEqual([
      { channel: 'a', parent: 'Y', position: 1 },
    ]);
    expect(planChannelMove(sorted, 'x2', 'a', 'after')).toEqual([
      { channel: 'x2', parent: null, position: 1 },
      { channel: 'b', position: 2 },
      { channel: 'c', position: 3 },
    ]);
    // 카테고리 머리글에 놓으면(접혀 있어도) 맨 위로.
    expect(planChannelMove(sorted, 'x2', 'Y', 'into')).toEqual([
      { channel: 'x2', parent: 'Y', position: 0 },
      { channel: 'y1', position: 1 },
    ]);
  });

  it('카테고리는 카테고리끼리만 옮긴다', () => {
    expect(planChannelMove(sorted, 'Y', 'X', 'before')).toEqual([
      { channel: 'Y', position: 0 },
      { channel: 'X', position: 1 },
    ]);
    expect(planChannelMove(sorted, 'Y', 'X', 'into')).toEqual([]);
    expect(planChannelMove(sorted, 'Y', 'a', 'before')).toEqual([]);
    expect(planChannelMove(sorted, 'a', 'X', 'before')).toEqual([]);
    expect(planChannelMove(sorted, 'a', 'b', 'into')).toEqual([]);
  });
});
