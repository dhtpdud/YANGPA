// 권한 도우미(순수 함수): 권한 템플릿(역할·채널 덮어쓰기 공용), 역할 표시 설정, 채널 권한 덮어쓰기(거부·기본·허용)와 카테고리 맞추기.
//
// 덮어쓰기 규칙(데몬 channel_permissions와 같음): @everyone → 역할(겹치면 허용이 이김) → 멤버 순으로 적용하고, 관리자는 늘
// 모든 권한을 가집니다. 카테고리 덮어쓰기는 안의 채널로 물려주지 않으므로, 맞추기(같은 목록 복사)는 앱이 합니다.

import {
  EVERYONE_ROLE,
  PERMISSION_BITS,
  type ChannelId,
  type ChannelKind,
  type ChannelView,
  type MemberView,
  type OverwriteTarget,
  type PermissionOverwrite,
  type RoleDisplay,
  type RoleId,
  type RoleUpdateParams,
  type SpaceView,
  type UserId,
} from '../api/types';
import { t } from '../i18n';

/** 권한 화면의 묶음(역할 편집·채널 권한이 같은 순서로 씁니다). */
export const PERM_GROUPS: Array<[string, string[]]> = [
  [t('일반'), ['view_channel', 'create_invite']],
  [t('텍스트'), ['send_messages', 'read_history', 'add_reactions', 'attach_files', 'embed_links', 'use_external_emoji', 'create_posts', 'mention_many']],
  [t('통화'), ['connect_voice', 'speak', 'stream']],
  // 크루 단위 권한이라 채널 덮어쓰기에는 나오지 않습니다(channelPermGroups가 거름).
  [t('연결'), ['use_crew_stun']],
  [t('관리'), ['manage_messages', 'manage_channels', 'manage_roles', 'kick', 'ban', 'manage_emoji', 'manage_voice', 'manage_space', 'administrator']],
];

const ALL_PERMS = PERMISSION_BITS.map(([n]) => n);
const BIT = new Map(PERMISSION_BITS.map(([n, b]) => [n, 2 ** b]));

/** 아는 권한만, 중복 없이 비트 순서로. */
function canonPerms(list: readonly string[]): string[] {
  const set = new Set(list);
  return ALL_PERMS.filter((n) => set.has(n));
}

function sameIdSet(a: readonly string[] | null, b: readonly string[] | null): boolean {
  if (a === null || b === null) return a === b;
  const sa = new Set(a);
  const sb = new Set(b);
  return sa.size === sb.size && [...sb].every((x) => sa.has(x));
}

// ───────────── 역할 권한 템플릿 ─────────────

export interface PermTemplate {
  name: string;
  /** 허용(역할에서는 켤 권한). */
  permissions: string[];
  /** 채널 덮어쓰기에서 거부할 권한. 없으면 채널 화면에서 허용 밖의 권한을 모두 거부합니다(역할에는 쓰지 않음). */
  deny?: string[];
}

export const MAX_TEMPLATES = 50;
export const MAX_TEMPLATE_NAME = 50;

const MEMBER_PERMS = canonPerms([
  'view_channel',
  'send_messages',
  'read_history',
  'add_reactions',
  'attach_files',
  'embed_links',
  'use_external_emoji',
  'connect_voice',
  'speak',
  'stream',
  'create_posts',
  'create_invite',
]);

/** 기본 제공 템플릿(지울 수 없음). */
export const BUILTIN_TEMPLATES: readonly PermTemplate[] = [
  { name: t('관리자'), permissions: ALL_PERMS },
  {
    name: t('운영자'),
    permissions: canonPerms([...MEMBER_PERMS, 'manage_messages', 'kick', 'ban', 'manage_voice', 'create_invite', 'use_crew_stun', 'mention_many']),
  },
  { name: t('일반 멤버'), permissions: MEMBER_PERMS },
  { name: t('손님'), permissions: canonPerms(['view_channel', 'read_history', 'add_reactions', 'connect_voice']) },
];

/** 저장할 템플릿 이름(앞뒤 공백을 빼고 길이 상한까지). */
export function templateName(raw: string): string {
  return raw.trim().slice(0, MAX_TEMPLATE_NAME).trim();
}

export function isBuiltinTemplate(name: string): boolean {
  return BUILTIN_TEMPLATES.some((t) => t.name === name);
}

/**
 * 내 템플릿에 저장합니다. 같은 이름이면 그 자리에서 덮어쓰고, 새 이름은 뒤에 붙입니다.
 * 이름이 비었거나 기본 템플릿과 같거나 이미 가득 찼으면 목록을 그대로 돌려줍니다. 거부는 허용과 겹치지 않게 남깁니다.
 */
export function upsertTemplate(list: readonly PermTemplate[], name: string, permissions: readonly string[], deny?: readonly string[]): PermTemplate[] {
  return isBuiltinTemplate(templateName(name)) ? [...list] : putTemplate(list, name, permissions, deny);
}

// 기본 템플릿 이름은 화면 언어를 따라 바뀌므로, 저장된 템플릿을 읽을 때(sanitizeTemplates)는 이름이 겹쳐도 버리지 않습니다.
function putTemplate(list: readonly PermTemplate[], name: string, permissions: readonly string[], deny?: readonly string[]): PermTemplate[] {
  const n = templateName(name);
  if (!n) return [...list];
  const allow = canonPerms(permissions);
  const t: PermTemplate = { name: n, permissions: allow };
  if (deny) t.deny = canonPerms(deny).filter((p) => !allow.includes(p));
  const i = list.findIndex((x) => x.name === n);
  if (i >= 0) return list.map((x, j) => (j === i ? t : x));
  if (list.length >= MAX_TEMPLATES) return [...list];
  return [...list, t];
}

export function removeTemplate(list: readonly PermTemplate[], name: string): PermTemplate[] {
  return list.filter((t) => t.name !== name);
}

/** 설정 파일의 내 템플릿에서 아는 형식만 받습니다(모르는 권한은 버리고, 이름이 겹치면 뒤의 것이 덮어씀, 거부가 없는 옛 항목도 받음). */
export function sanitizeTemplates(raw: unknown): PermTemplate[] {
  if (!Array.isArray(raw)) return [];
  const strings = (v: unknown[]) => v.filter((p): p is string => typeof p === 'string');
  let out: PermTemplate[] = [];
  for (const t of raw) {
    if (!t || typeof t !== 'object') continue;
    const { name, permissions, deny } = t as Record<string, unknown>;
    if (typeof name !== 'string' || !Array.isArray(permissions)) continue;
    out = putTemplate(out, name, strings(permissions), Array.isArray(deny) ? strings(deny) : undefined);
  }
  return out;
}

/**
 * 템플릿을 역할의 지금 고른 권한에 덮어씁니다(역할에는 거부가 없어 템플릿의 거부는 쓰지 않음). 내가 줄 수 없는 권한
 * (`mayGrant`가 거짓)은 지금 상태를 두고, 그래서 템플릿과 달라진 권한 수를 `skipped`로 돌려줍니다.
 */
export function applyTemplate(
  current: readonly string[],
  template: readonly string[],
  mayGrant: (perm: string) => boolean,
): { permissions: string[]; skipped: number } {
  const want = new Set(template);
  const have = new Set(current);
  let skipped = 0;
  const permissions = ALL_PERMS.filter((p) => {
    if (mayGrant(p)) return want.has(p);
    if (want.has(p) !== have.has(p)) skipped++;
    return have.has(p);
  });
  return { permissions, skipped };
}

// ───────────── 역할 표시 ─────────────

/**
 * 역할의 표시 설정(`role_display`에는 기본값과 다른 역할만 옵니다). 관리자 보기는 권한처럼 더해지므로 기본값이 역할마다 다릅니다:
 * @everyone은 켜짐(모든 멤버가 봄), 다른 역할은 꺼짐. 볼 수 있는 역할 목록에는 지금 있는 다른 역할만 남깁니다.
 */
export function roleDisplayOf(view: SpaceView, role: RoleId): RoleDisplay {
  const everyone = role === EVERYONE_ROLE;
  const d = view.role_display?.[role];
  const list = everyone ? null : (d?.visible_to ?? null);
  return {
    visible_to: list && list.filter((r) => r !== role && r !== EVERYONE_ROLE && view.roles.some((x) => x.id === r)),
    see_admins: typeof d?.see_admins === 'boolean' ? d.see_admins : everyone,
  };
}

/** 바뀐 표시 설정만 담은 role.update 필드. @everyone에는 볼 수 있는 역할을 보내지 않습니다(데몬이 거절). */
export function roleDisplayPatch(before: RoleDisplay, after: RoleDisplay, everyone: boolean): Pick<RoleUpdateParams, 'visible_to' | 'see_admins'> {
  const p: Pick<RoleUpdateParams, 'visible_to' | 'see_admins'> = {};
  if (!everyone && !sameIdSet(before.visible_to, after.visible_to)) p.visible_to = after.visible_to && [...after.visible_to];
  if (before.see_admins !== after.see_admins) p.see_admins = after.see_admins;
  return p;
}

// ───────────── 채널 권한 덮어쓰기 ─────────────

export type TriState = 'deny' | 'inherit' | 'allow';

export const EVERYONE_TARGET: OverwriteTarget = { type: 'role', id: EVERYONE_ROLE };

const VIEW = 'view_channel';
const TEXT_PERMS = ['send_messages', 'read_history', 'add_reactions', 'attach_files', 'embed_links', 'use_external_emoji', 'mention_many', 'manage_messages'];
const VOICE_PERMS = ['connect_voice', 'speak', 'stream', 'manage_voice'];

// 비트 31(관리자)은 32비트 정수 연산에서 부호가 뒤집히므로 나눗셈으로 셉니다.
function hasBit(v: number, bit: number): boolean {
  return Math.floor(v / bit) % 2 === 1;
}

function withBit(v: number, bit: number, on: boolean): number {
  if (hasBit(v, bit) === on) return v;
  return on ? v + bit : v - bit;
}

export function sameTarget(a: OverwriteTarget, b: OverwriteTarget): boolean {
  return a.type === b.type && a.id === b.id;
}

export function targetKey(t: OverwriteTarget): string {
  return `${t.type}:${t.id}`;
}

/** 채널 종류별로 덮어쓸 권한을 묶음별로. 카테고리는 안의 채널에 맞출 수 있게 모든 종류의 권한을 보입니다. */
export function channelPermGroups(kind: ChannelKind): Array<[string, string[]]> {
  const set = new Set([VIEW, 'manage_channels']);
  const text = kind === 'text' || kind === 'forum' || kind === 'category';
  if (text) TEXT_PERMS.forEach((p) => set.add(p));
  if (kind === 'forum' || kind === 'category') set.add('create_posts');
  if (kind === 'voice' || kind === 'category') VOICE_PERMS.forEach((p) => set.add(p));
  return PERM_GROUPS.map(([g, list]): [string, string[]] => [g, list.filter((p) => set.has(p))]).filter(([, list]) => list.length > 0);
}

export function triState(list: readonly PermissionOverwrite[], target: OverwriteTarget, perm: string): TriState {
  const o = list.find((x) => sameTarget(x.target, target));
  const bit = BIT.get(perm);
  if (!o || bit === undefined) return 'inherit';
  if (hasBit(o.deny, bit)) return 'deny';
  return hasBit(o.allow, bit) ? 'allow' : 'inherit';
}

/** 한 대상의 한 권한을 바꾼 새 목록. 대상이 없으면 붙입니다(비어도 남겨 화면 목록에 보이고, 보낼 때 버립니다). */
export function setTriState(list: readonly PermissionOverwrite[], target: OverwriteTarget, perm: string, state: TriState): PermissionOverwrite[] {
  const bit = BIT.get(perm);
  if (bit === undefined) return [...list];
  const i = list.findIndex((x) => sameTarget(x.target, target));
  const o = i >= 0 ? list[i] : { target, allow: 0, deny: 0 };
  const next = { target: o.target, allow: withBit(o.allow, bit, state === 'allow'), deny: withBit(o.deny, bit, state === 'deny') };
  return i >= 0 ? list.map((x, j) => (j === i ? next : x)) : [...list, next];
}

export function addTarget(list: readonly PermissionOverwrite[], target: OverwriteTarget): PermissionOverwrite[] {
  return list.some((o) => sameTarget(o.target, target)) ? [...list] : [...list, { target, allow: 0, deny: 0 }];
}

export function removeTarget(list: readonly PermissionOverwrite[], target: OverwriteTarget): PermissionOverwrite[] {
  return list.filter((o) => !sameTarget(o.target, target));
}

/** 보낼 목록: 허용·거부가 모두 0인 항목을 버립니다(데몬도 버림). */
export function compactOverwrites(list: readonly PermissionOverwrite[]): PermissionOverwrite[] {
  return list.filter((o) => o.allow !== 0 || o.deny !== 0).map((o) => ({ target: { ...o.target }, allow: o.allow, deny: o.deny }));
}

/** 순서와 빈 항목을 따지지 않고 같은 덮어쓰기인지(대상별로 비교). */
export function sameOverwrites(a: readonly PermissionOverwrite[], b: readonly PermissionOverwrite[]): boolean {
  const ca = compactOverwrites(a);
  const cb = compactOverwrites(b);
  if (ca.length !== cb.length) return false;
  const byTarget = new Map(ca.map((o) => [targetKey(o.target), o]));
  return cb.every((o) => {
    const x = byTarget.get(targetKey(o.target));
    return !!x && x.allow === o.allow && x.deny === o.deny;
  });
}

/** 바꾸면 데몬이 내 권한과 맞춰 보는 권한(데몬 overwrite_delta와 같음: 대상별 허용·거부가 달라진 비트, 사라진 대상의 비트). */
export function overwriteDelta(old: readonly PermissionOverwrite[], next: readonly PermissionOverwrite[]): string[] {
  const changed = new Set<string>();
  const mark = (a: number, b: number) => {
    for (const [p, bit] of BIT) if (hasBit(a, bit) !== hasBit(b, bit)) changed.add(p);
  };
  for (const o of next) {
    const x = old.find((y) => sameTarget(y.target, o.target));
    mark(x?.allow ?? 0, o.allow);
    mark(x?.deny ?? 0, o.deny);
  }
  for (const o of old) {
    if (next.some((y) => sameTarget(y.target, o.target))) continue;
    mark(o.allow, 0);
    mark(o.deny, 0);
  }
  return ALL_PERMS.filter((p) => changed.has(p));
}

/** 나간 멤버를 대상으로 한 항목을 뺍니다(데몬이 멤버가 아닌 대상을 거절함). 내게 보이지 않는 역할의 항목은 그대로 둡니다. */
export function liveOverwrites(list: readonly PermissionOverwrite[], members: readonly MemberView[]): PermissionOverwrite[] {
  const users = new Set(members.map((m) => m.user));
  return list.filter((o) => o.target.type !== 'member' || users.has(o.target.id));
}

/**
 * 이 채널에서의 내 권한(데몬 channel_permissions와 같은 규칙). 공간 단위 권한(`my_permissions`)에 @everyone → 내 역할들
 * → 나(멤버) 순으로 덮어쓰기를 적용하고, 채널 보기가 막히면 모두 거짓입니다. 관리자·소유자는 늘 참입니다.
 */
export function hasChannelPerm(space: SpaceView | undefined, channel: ChannelView | undefined, me: UserId | null | undefined, perm: string): boolean {
  if (!space) return false;
  const base = new Set(space.my_permissions);
  if (base.has('administrator')) return true;
  if (!channel || !me) return base.has(perm);
  const list = channel.settings.overwrites ?? [];
  const roles = space.members.find((m) => m.user === me)?.roles ?? [];
  const everyone = list.find((o) => sameTarget(o.target, EVERYONE_TARGET));
  const mine = list.find((o) => o.target.type === 'member' && o.target.id === me);
  const has = (name: string) => {
    const bit = BIT.get(name);
    if (bit === undefined) return false;
    let on = base.has(name);
    const step = (allow: boolean, deny: boolean) => {
      if (deny) on = false;
      if (allow) on = true;
    };
    if (everyone) step(hasBit(everyone.allow, bit), hasBit(everyone.deny, bit));
    let allow = false;
    let deny = false;
    for (const o of list) {
      if (o.target.type !== 'role' || o.target.id === EVERYONE_ROLE || !roles.includes(o.target.id)) continue;
      allow ||= hasBit(o.allow, bit);
      deny ||= hasBit(o.deny, bit);
    }
    step(allow, deny);
    if (mine) step(hasBit(mine.allow, bit), hasBit(mine.deny, bit));
    return on;
  };
  return has(VIEW) && has(perm);
}

/** 비공개: @everyone의 채널 보기가 거부됨. */
export function isPrivate(list: readonly PermissionOverwrite[]): boolean {
  return triState(list, EVERYONE_TARGET, VIEW) === 'deny';
}

export function setPrivate(list: readonly PermissionOverwrite[], on: boolean): PermissionOverwrite[] {
  return setTriState(list, EVERYONE_TARGET, VIEW, on ? 'deny' : 'inherit');
}

/** 채널 보기가 허용된 역할(@everyone 제외). 비공개 채널에서 볼 수 있는 역할입니다. */
export function viewRoles(list: readonly PermissionOverwrite[]): RoleId[] {
  return list.filter((o) => o.target.type === 'role' && o.target.id !== EVERYONE_ROLE && triState([o], o.target, VIEW) === 'allow').map((o) => o.target.id);
}

export function toggleViewRole(list: readonly PermissionOverwrite[], role: RoleId): PermissionOverwrite[] {
  const t: OverwriteTarget = { type: 'role', id: role };
  return setTriState(list, t, VIEW, triState(list, t, VIEW) === 'allow' ? 'inherit' : 'allow');
}

/**
 * 템플릿을 덮어쓰기 한 대상에 씁니다. 이 화면이 보이는 권한(`perms`)만 다룹니다: 허용 = 템플릿 권한, 거부 = 템플릿의 거부
 * (없으면 허용 밖의 나머지 전부라, 기본 '손님'은 이 채널에서 손님처럼 굴게 됨), 그 밖은 기본. 내가 줄 수 없는 권한은 지금
 * 상태를 두고 템플릿과 달라진 수를 `skipped`로 돌려줍니다.
 */
export function applyTemplateToTarget(
  list: readonly PermissionOverwrite[],
  target: OverwriteTarget,
  perms: readonly string[],
  template: PermTemplate,
  mayGrant: (perm: string) => boolean,
): { overwrites: PermissionOverwrite[]; skipped: number } {
  const allow = new Set(template.permissions);
  const deny = template.deny ? new Set(template.deny) : null;
  let overwrites = [...list];
  let skipped = 0;
  for (const p of perms) {
    const want: TriState = allow.has(p) ? 'allow' : !deny || deny.has(p) ? 'deny' : 'inherit';
    if (mayGrant(p)) overwrites = setTriState(overwrites, target, p, want);
    else if (triState(list, target, p) !== want) skipped++;
  }
  return { overwrites, skipped };
}

/** 한 대상의 지금 설정을 템플릿 값으로(이 화면이 보이는 권한만): 허용은 permissions, 거부는 deny. */
export function targetTemplate(list: readonly PermissionOverwrite[], target: OverwriteTarget, perms: readonly string[]): { permissions: string[]; deny: string[] } {
  return {
    permissions: perms.filter((p) => triState(list, target, p) === 'allow'),
    deny: perms.filter((p) => triState(list, target, p) === 'deny'),
  };
}

/** 카테고리 안에서 권한이 카테고리(`categoryOverwrites`, 바꾸기 전 값)와 같은 채널. 카테고리를 바꿀 때 함께 맞춥니다. */
export function syncedChildren(view: SpaceView, category: ChannelId, categoryOverwrites: readonly PermissionOverwrite[]): ChannelView[] {
  return view.channels.filter(
    (c) => c.settings.parent === category && c.channel !== category && sameOverwrites(liveOverwrites(c.settings.overwrites, view.members), categoryOverwrites),
  );
}
