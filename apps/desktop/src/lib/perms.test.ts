import { describe, expect, it } from 'vitest';
import type { ChannelView, PermissionOverwrite, SpaceView } from '../api/types';
import { EVERYONE_ROLE, PERMISSION_BITS } from '../api/types';
import {
  BUILTIN_TEMPLATES,
  MAX_TEMPLATES,
  PERM_GROUPS,
  addTarget,
  applyTemplate,
  applyTemplateToTarget,
  channelPermGroups,
  hasChannelPerm,
  isPrivate,
  liveOverwrites,
  overwriteDelta,
  removeTarget,
  roleDisplayOf,
  roleDisplayPatch,
  sameOverwrites,
  sanitizeTemplates,
  setPrivate,
  setTriState,
  syncedChildren,
  targetTemplate,
  toggleViewRole,
  triState,
  upsertTemplate,
  viewRoles,
} from './perms';

const ALL = PERMISSION_BITS.map(([n]) => n);
const role = (id: string) => ({ type: 'role' as const, id });
const member = (id: string) => ({ type: 'member' as const, id });

function space(over: Partial<SpaceView> = {}): SpaceView {
  return {
    space: 's1',
    kind: 'crew',
    settings: { name: '크루', description: '', icon: null, allow_external_emoji: false },
    owner: 'u1',
    committer: 'd1',
    members: [],
    roles: [
      { id: EVERYONE_ROLE, name: '@everyone', color: 0, rank: 0, permissions: 0, automation: false },
      { id: 'r1', name: '운영진', color: 0, rank: 2, permissions: 0, automation: false },
      { id: 'r2', name: '손님', color: 0, rank: 1, permissions: 0, automation: false },
    ],
    channels: [],
    emojis: [],
    my_permissions: ['administrator'],
    pending_commit: false,
    dormant: false,
    trusted: false,
    ...over,
  };
}

function chan(id: string, parent: string | null, overwrites: PermissionOverwrite[]): ChannelView {
  return {
    channel: id,
    settings: {
      name: id,
      kind: parent ? 'text' : 'category',
      topic: '',
      parent,
      position: 0,
      security: 'normal',
      join_mode: 'always',
      ttl: { mode: 'none' },
      history: 'private',
      link_previews: true,
      overwrites,
      forum_tags: [],
    },
    unread: 0,
    api_read_allowed: false,
    p2p: false,
    p2p_joined: false,
  };
}

describe('권한 묶음', () => {
  it('모든 권한이 묶음 하나에만 있고, 크루 STUN 사용은 연결 묶음에 있다', () => {
    const flat = PERM_GROUPS.flatMap(([, l]) => l);
    expect([...flat].sort()).toEqual([...ALL].sort());
    expect(new Set(flat).size).toBe(flat.length);
    expect(PERM_GROUPS.find(([g]) => g === '연결')?.[1]).toEqual(['use_crew_stun']);
  });
});

describe('기본 권한 템플릿', () => {
  it('관리자는 모든 권한, 운영자는 일반 멤버에 관리 권한 몇 개를 더하고, 손님은 보기 위주', () => {
    const t = Object.fromEntries(BUILTIN_TEMPLATES.map((x) => [x.name, x.permissions]));
    expect(Object.keys(t)).toEqual(['관리자', '운영자', '일반 멤버', '손님']);
    expect(t['관리자']).toEqual(ALL);
    expect(t['일반 멤버']).not.toContain('manage_messages');
    expect(t['일반 멤버']).toContain('create_invite');
    for (const p of [...t['일반 멤버'], 'manage_messages', 'kick', 'ban', 'manage_voice']) expect(t['운영자']).toContain(p);
    expect(t['운영자']).not.toContain('administrator');
    expect(t['손님']).toEqual(['view_channel', 'read_history', 'add_reactions', 'connect_voice']);
    // 크루 STUN은 운영자에게만 기본으로(멤버도 따로 켜야 씀).
    expect(t['운영자']).toContain('use_crew_stun');
    expect(t['일반 멤버']).not.toContain('use_crew_stun');
    expect(t['손님']).not.toContain('use_crew_stun');
  });
});

describe('템플릿 불러오기·저장', () => {
  it('내가 줄 수 있는 권한만 바꾸고, 못 바꾼 권한 수를 센다', () => {
    const mine = new Set(['view_channel', 'send_messages', 'kick']);
    const r = applyTemplate(['send_messages', 'ban'], ['view_channel', 'kick', 'administrator'], (p) => mine.has(p));
    // send_messages는 끄고 view_channel·kick은 켬, ban(가진 채 못 끔)·administrator(못 켬)는 그대로
    expect(r.permissions).toEqual(['view_channel', 'kick', 'ban']);
    expect(r.skipped).toBe(2);
  });

  it('같은 이름은 그 자리에서 덮어쓰고, 기본 템플릿 이름·빈 이름·상한 초과는 받지 않는다', () => {
    let list = upsertTemplate([], '  내 역할  ', ['speak', 'view_channel', 'speak', 'nope']);
    expect(list).toEqual([{ name: '내 역할', permissions: ['view_channel', 'speak'] }]);
    list = upsertTemplate(list, '둘째', []);
    list = upsertTemplate(list, '내 역할', ['kick']);
    expect(list.map((t) => [t.name, t.permissions])).toEqual([
      ['내 역할', ['kick']],
      ['둘째', []],
    ]);
    expect(upsertTemplate(list, '관리자', ['kick'])).toEqual(list);
    expect(upsertTemplate(list, '   ', ['kick'])).toEqual(list);
    const full = Array.from({ length: MAX_TEMPLATES }, (_, i) => ({ name: `t${i}`, permissions: [] }));
    expect(upsertTemplate(full, '새것', [])).toHaveLength(MAX_TEMPLATES);
    expect(upsertTemplate(full, 't3', ['kick'])[3].permissions).toEqual(['kick']);
  });

  it('거부는 허용과 겹치지 않게 남기고, 거부가 없는 옛 항목도 그대로 읽는다', () => {
    expect(upsertTemplate([], '채널용', ['speak'], ['speak', 'stream', 'nope'])).toEqual([{ name: '채널용', permissions: ['speak'], deny: ['stream'] }]);
    expect(upsertTemplate([], '역할용', ['speak'])[0]).not.toHaveProperty('deny');
    expect(
      sanitizeTemplates([
        { name: '옛것', permissions: ['kick'] },
        { name: '새것', permissions: ['speak'], deny: ['stream', 7, 'fly'] },
        { name: '틀림', permissions: [], deny: 'x' },
      ]),
    ).toEqual([
      { name: '옛것', permissions: ['kick'] },
      { name: '새것', permissions: ['speak'], deny: ['stream'] },
      { name: '틀림', permissions: [] },
    ]);
  });

  it('설정 파일에서 읽을 때 모양이 틀린 항목·모르는 권한을 버리고, 이름은 50자·목록은 50개로 자른다', () => {
    const raw = [
      { name: 'a', permissions: ['kick', 3, 'unknown'] },
      { name: 5, permissions: [] },
      'x',
      { name: 'b' },
      { name: 'a', permissions: ['ban'] },
      { name: '가'.repeat(80), permissions: [] },
      ...Array.from({ length: 60 }, (_, i) => ({ name: `n${i}`, permissions: [] })),
    ];
    const out = sanitizeTemplates(raw);
    expect(out[0]).toEqual({ name: 'a', permissions: ['ban'] });
    expect(out[1].name).toHaveLength(50);
    expect(out).toHaveLength(MAX_TEMPLATES);
    expect(sanitizeTemplates({ a: 1 })).toEqual([]);
  });
});

describe('역할 표시 설정', () => {
  it('없으면 역할마다 기본값(@everyone만 관리자 보기 켜짐), 지운 역할·자기 자신은 목록에서 뺀다', () => {
    const v = space({ role_display: { r1: { visible_to: ['r2', 'gone', 'r1'], see_admins: true } } });
    expect(roleDisplayOf(v, EVERYONE_ROLE)).toEqual({ visible_to: null, see_admins: true });
    expect(roleDisplayOf(v, 'r2')).toEqual({ visible_to: null, see_admins: false });
    expect(roleDisplayOf(v, 'r1')).toEqual({ visible_to: ['r2'], see_admins: true });
  });

  it('바뀐 값만 보내고 @everyone에는 볼 수 있는 역할을 보내지 않는다', () => {
    const before = { visible_to: null, see_admins: false };
    expect(roleDisplayPatch(before, { visible_to: null, see_admins: false }, false)).toEqual({});
    expect(roleDisplayPatch({ visible_to: ['a', 'b'], see_admins: false }, { visible_to: ['b', 'a'], see_admins: false }, false)).toEqual({});
    expect(roleDisplayPatch(before, { visible_to: [], see_admins: true }, false)).toEqual({ visible_to: [], see_admins: true });
    expect(roleDisplayPatch({ visible_to: ['a'], see_admins: true }, { visible_to: null, see_admins: true }, false)).toEqual({ visible_to: null });
    expect(roleDisplayPatch({ visible_to: null, see_admins: true }, { visible_to: ['a'], see_admins: false }, true)).toEqual({ see_admins: false });
  });
});

describe('채널 권한 덮어쓰기', () => {
  it('거부·기본·허용을 대상별로 바꾸고 허용과 거부에 같은 비트를 두지 않는다', () => {
    let l = setTriState([], role('r1'), 'send_messages', 'allow');
    expect(l).toEqual([{ target: role('r1'), allow: 2, deny: 0 }]);
    l = setTriState(l, role('r1'), 'send_messages', 'deny');
    expect(l).toEqual([{ target: role('r1'), allow: 0, deny: 2 }]);
    expect(triState(l, role('r1'), 'send_messages')).toBe('deny');
    expect(triState(l, role('r2'), 'send_messages')).toBe('inherit');
    l = setTriState(l, role('r1'), 'send_messages', 'inherit');
    expect(l).toEqual([{ target: role('r1'), allow: 0, deny: 0 }]);
    // 관리자(비트 31)도 부호 문제 없이
    l = setTriState(l, role('r1'), 'administrator', 'allow');
    expect(l[0].allow).toBe(2 ** 31);
    expect(triState(l, role('r1'), 'administrator')).toBe('allow');
  });

  it('대상 추가·삭제, 순서와 빈 항목을 무시한 비교', () => {
    const a = [{ target: role(EVERYONE_ROLE), allow: 0, deny: 1 }, { target: member('u1'), allow: 2, deny: 0 }];
    const b = addTarget([a[1], a[0]], role('r2'));
    expect(b).toHaveLength(3);
    expect(sameOverwrites(a, b)).toBe(true);
    expect(sameOverwrites(a, removeTarget(b, member('u1')))).toBe(false);
    expect(sameOverwrites(a, setTriState(a, member('u1'), 'speak', 'deny'))).toBe(false);
  });

  it('비공개 스위치와 역할 칩은 @everyone 보기 거부·역할 보기 허용과 같다', () => {
    let l = setPrivate([], true);
    expect(isPrivate(l)).toBe(true);
    l = toggleViewRole(toggleViewRole(l, 'r1'), 'r2');
    expect(viewRoles(l)).toEqual(['r1', 'r2']);
    l = toggleViewRole(l, 'r1');
    expect(viewRoles(l)).toEqual(['r2']);
    l = setPrivate(l, false);
    expect(isPrivate(l)).toBe(false);
    expect(triState(l, role(EVERYONE_ROLE), 'view_channel')).toBe('inherit');
  });

  it('채널 종류마다 고칠 수 있는 권한, 카테고리는 전부', () => {
    const flat = (k: Parameters<typeof channelPermGroups>[0]) => channelPermGroups(k).flatMap(([, l]) => l);
    expect(flat('voice').sort()).toEqual(['connect_voice', 'manage_channels', 'manage_voice', 'speak', 'stream', 'view_channel'].sort());
    expect(flat('text')).not.toContain('create_posts');
    expect(flat('forum')).toContain('create_posts');
    expect(flat('category').sort()).toEqual([...new Set([...flat('forum'), ...flat('voice')])].sort());
    expect(channelPermGroups('voice').map(([g]) => g)).toEqual(['일반', '통화', '관리']);
    // 크루 STUN 사용은 크루 단위라 어느 채널에도 없습니다.
    expect(flat('category')).not.toContain('use_crew_stun');
  });

  it('채널 대상에 템플릿을 쓰면 보이는 권한만 다루고, 거부가 없으면 나머지를 모두 거부한다', () => {
    const shown = channelPermGroups('text').flatMap(([, l]) => l);
    const guest = BUILTIN_TEMPLATES.find((t) => t.name === '손님')!;
    const r = applyTemplateToTarget([], role('r2'), shown, guest, () => true);
    expect(r.skipped).toBe(0);
    expect(targetTemplate(r.overwrites, role('r2'), shown)).toEqual({
      permissions: ['view_channel', 'read_history', 'add_reactions'],
      deny: shown.filter((p) => !['view_channel', 'read_history', 'add_reactions'].includes(p)),
    });
    // 관리자 비트는 채널에서 보이지 않아도 '관리자' 템플릿은 보이는 권한을 모두 허용합니다.
    const admin = applyTemplateToTarget([], role('r2'), channelPermGroups('voice').flatMap(([, l]) => l), BUILTIN_TEMPLATES[0], () => true);
    expect(admin.overwrites).toEqual([{ target: role('r2'), allow: 1 + 2 ** 7 + 2 ** 8 + 2 ** 9 + 2 ** 17 + 2 ** 22, deny: 0 }]);
  });

  it('거부가 있는 템플릿은 나머지를 기본으로 두고, 내게 없는 권한은 지금 상태를 두며 달라진 수를 센다', () => {
    const shown = ['view_channel', 'send_messages', 'read_history'];
    const before = [{ target: role('r1'), allow: 0, deny: 4 }];
    const t = { name: 't', permissions: ['send_messages'], deny: ['view_channel'] };
    const r = applyTemplateToTarget(before, role('r1'), shown, t, (p) => p !== 'send_messages');
    expect(r.skipped).toBe(1);
    expect(targetTemplate(r.overwrites, role('r1'), shown)).toEqual({ permissions: [], deny: ['view_channel'] });
  });

  it('데몬처럼 바뀐 비트와 사라진 대상의 비트를 센다', () => {
    const old = [{ target: role('r1'), allow: 1, deny: 2 }, { target: member('u1'), allow: 0, deny: 128 }];
    expect(overwriteDelta(old, old)).toEqual([]);
    expect(overwriteDelta(old, [{ target: role('r1'), allow: 1, deny: 0 }])).toEqual(['send_messages', 'connect_voice']);
  });

  it('나간 멤버 항목은 빼고, 카테고리 권한과 같은 채널만 따라오는 채널로 센다', () => {
    const cat = [{ target: role(EVERYONE_ROLE), allow: 0, deny: 1 }];
    const v = space({
      members: [{ user: 'u1', profile: {} as never, roles: [], online: true, automation: false }],
      channels: [
        chan('cat', null, cat),
        chan('same', 'cat', [...cat, { target: member('gone'), allow: 2, deny: 0 }]),
        chan('diff', 'cat', []),
        chan('other', null, cat),
      ],
    });
    expect(liveOverwrites(v.channels[1].settings.overwrites, v.members)).toEqual(cat);
    expect(syncedChildren(v, 'cat', cat).map((c) => c.channel)).toEqual(['same']);
  });
});

describe('채널에서의 내 권한(데몬 channel_permissions와 같은 규칙)', () => {
  const ME = 'me';
  const bit = (name: string) => 2 ** (PERMISSION_BITS.find(([n]) => n === name)![1]);
  const mk = (perms: string[], roles: string[], overwrites: PermissionOverwrite[]) => {
    const v = space({ my_permissions: perms, members: [{ user: ME, profile: {} as never, roles, online: true, automation: false }] });
    return { v, c: { ...chan('f', 'cat', overwrites), settings: { ...chan('f', 'cat', overwrites).settings, kind: 'forum' as const } } };
  };

  it('덮어쓰기가 없으면 공간 권한, 관리자는 늘 참', () => {
    const { v, c } = mk(['view_channel', 'manage_channels'], [], []);
    expect(hasChannelPerm(v, c, ME, 'manage_channels')).toBe(true);
    expect(hasChannelPerm(v, c, ME, 'manage_messages')).toBe(false);
    const admin = mk(['administrator'], [], [{ target: role(EVERYONE_ROLE), allow: 0, deny: bit('view_channel') }]);
    expect(hasChannelPerm(admin.v, admin.c, ME, 'manage_channels')).toBe(true);
  });

  it('@everyone → 역할(허용이 이김) → 멤버 순서로 적용한다', () => {
    const mc = bit('manage_channels');
    // 역할 덮어쓰기로 이 채널에서만 채널 관리
    let t = mk(['view_channel'], ['r1'], [{ target: role('r1'), allow: mc, deny: 0 }]);
    expect(hasChannelPerm(t.v, t.c, ME, 'manage_channels')).toBe(true);
    // 역할 둘이 엇갈리면 허용
    t = mk(['view_channel', 'manage_channels'], ['r1', 'r2'], [
      { target: role('r1'), allow: 0, deny: mc },
      { target: role('r2'), allow: mc, deny: 0 },
    ]);
    expect(hasChannelPerm(t.v, t.c, ME, 'manage_channels')).toBe(true);
    // @everyone 거부는 역할 허용이 되살리고, 멤버 거부가 마지막에 이깁니다.
    t = mk(['view_channel', 'manage_channels'], ['r1'], [
      { target: role(EVERYONE_ROLE), allow: 0, deny: mc },
      { target: role('r1'), allow: mc, deny: 0 },
      { target: member(ME), allow: 0, deny: mc },
    ]);
    expect(hasChannelPerm(t.v, t.c, ME, 'manage_channels')).toBe(false);
    // 내게 없는 역할의 덮어쓰기는 상관없습니다.
    t = mk(['view_channel'], [], [{ target: role('r1'), allow: mc, deny: 0 }]);
    expect(hasChannelPerm(t.v, t.c, ME, 'manage_channels')).toBe(false);
  });

  it('채널 보기가 막히면 모두 거짓이고, 나를 모르면 공간 권한만 본다', () => {
    const t = mk(['view_channel', 'manage_channels'], [], [{ target: role(EVERYONE_ROLE), allow: 0, deny: bit('view_channel') }]);
    expect(hasChannelPerm(t.v, t.c, ME, 'manage_channels')).toBe(false);
    expect(hasChannelPerm(t.v, t.c, null, 'manage_channels')).toBe(true);
    expect(hasChannelPerm(undefined, t.c, ME, 'manage_channels')).toBe(false);
  });
});
