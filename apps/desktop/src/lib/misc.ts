// 작은 도우미: 색, 권한 이름, base64, 썸네일 형식 판별.

import { PERMISSION_BITS, type MemberView, type NetStatus, type ReportAction, type Role, type SpaceView } from '../api/types';
import { t } from '../i18n';
import { stripMarkdown, type MarkdownOptions } from './markdown';

/** 식별자에서 안정적인 색상(아바타 배경). */
export function colorFromId(id: string): string {
  let h = 2166136261;
  for (let i = 0; i < id.length; i++) {
    h ^= id.charCodeAt(i);
    h = Math.imul(h, 16777619);
  }
  const hue = (h >>> 0) % 360;
  return `hsl(${hue} 55% 48%)`;
}

export function roleColor(color: number): string | undefined {
  if (!color) return undefined;
  return colorHex(color);
}

/** 0xRRGGBB → `#rrggbb`(검정도 그대로). */
export function colorHex(color: number): string {
  return `#${(color & 0xffffff).toString(16).padStart(6, '0')}`;
}

export function hexToInt(hex: string): number {
  const v = parseInt(hex.replace('#', ''), 16);
  return Number.isFinite(v) ? v & 0xffffff : 0;
}

export function initials(name: string): string {
  const trimmed = name.trim();
  if (!trimmed) return '?';
  const words = trimmed.split(/\s+/).filter(Boolean);
  if (words.length >= 2) return (Array.from(words[0])[0] + Array.from(words[1])[0]).toUpperCase();
  return Array.from(trimmed).slice(0, 2).join('').toUpperCase();
}

/** 권한 비트 → 이름 목록. 2^53을 넘는 값(정밀도 손실)은 '전부'로 봅니다. */
export function permissionNames(bits: number): string[] {
  if (!Number.isFinite(bits) || bits > Number.MAX_SAFE_INTEGER) return PERMISSION_BITS.map(([n]) => n);
  const big = BigInt(Math.trunc(bits));
  return PERMISSION_BITS.filter(([, b]) => (big >> BigInt(b)) & 1n).map(([n]) => n);
}

export function permissionBits(names: string[]): number {
  let v = 0n;
  for (const [n, b] of PERMISSION_BITS) if (names.includes(n)) v |= 1n << BigInt(b);
  return Number(v);
}

export const PERMISSION_LABELS: Record<string, string> = {
  view_channel: t('채널 보기'),
  send_messages: t('메시지 보내기'),
  read_history: t('기록 읽기'),
  add_reactions: t('반응 달기'),
  attach_files: t('파일 첨부'),
  embed_links: t('링크 미리보기'),
  use_external_emoji: t('다른 공간 이모지 쓰기'),
  connect_voice: t('통화 연결'),
  speak: t('말하기'),
  stream: t('화면공유'),
  create_posts: t('포럼 글쓰기'),
  use_crew_stun: t('크루 STUN 사용'),
  mention_many: t('많이 멘션하기(20명 넘게, @everyone·@here)'),
  manage_messages: t('메시지 관리'),
  manage_channels: t('채널 관리'),
  manage_roles: t('역할 관리'),
  kick: t('추방'),
  create_invite: t('초대 만들기'),
  manage_emoji: t('이모지 관리'),
  manage_voice: t('통화 관리'),
  manage_space: t('크루 관리'),
  ban: t('밴(추방하고 다시 못 들어오게)'),
  administrator: t('관리자(모든 권한)'),
};

export function hasPerm(space: SpaceView | undefined, perm: string): boolean {
  if (!space) return false;
  return space.my_permissions.includes(perm) || space.my_permissions.includes('administrator');
}

/**
 * 다른 공간의 커스텀 이모지를 이 공간에서 쓸 수 있는지.
 * 동기화 규칙(authorize.rs check_reaction_emoji)과 같게 공간 설정과 역할 권한이 둘 다 있어야 합니다.
 * 설정이 꺼져 있으면 관리자라도 쓸 수 없습니다.
 */
export function canUseExternalEmoji(space: SpaceView | undefined): boolean {
  return !!space && space.settings.allow_external_emoji && hasPerm(space, 'use_external_emoji');
}

/** 관리자 권한 역할의 순위에 더하는 값(데몬 `ADMIN_LEVEL`, 어떤 rank보다도 큼). */
const ADMIN_LEVEL = 2 ** 32;

/** 관리자 권한(비트 31)이 있는 역할인지. */
export function isAdminRole(role: Role): boolean {
  return ((role.permissions >>> 31) & 1) === 1;
}

/**
 * 위아래를 가리는 역할 순위(데몬 `Role::level`): 관리자 권한 역할은 rank와 상관없이 모든 일반 역할보다 위이고, 관리자
 * 역할끼리·일반 역할끼리는 rank 순입니다.
 */
export function roleLevel(role: Role): number {
  return (isAdminRole(role) ? ADMIN_LEVEL : 0) + role.rank;
}

/** 높은 역할이 앞에 오게 정렬할 때 씁니다. */
export function byLevelDesc(a: Role, b: Role): number {
  return roleLevel(b) - roleLevel(a);
}

/**
 * 크루 안 순위(데몬 `SpaceState::rank_of`와 같음): 소유자는 무한대, 멤버는 가진 역할 순위(`roleLevel`)의 최댓값, 역할이
 * 없으면 0. 추방·밴·역할 바꾸기는 자기보다 순위가 낮은 상대에게만 됩니다.
 */
export function rankOf(space: SpaceView, user: string | null | undefined): number {
  if (!user) return 0;
  if (user === space.owner) return Number.POSITIVE_INFINITY;
  const m = space.members.find((x) => x.user === user);
  if (!m) return 0;
  let rank = 0;
  for (const r of space.roles) if (m.roles.includes(r.id)) rank = Math.max(rank, roleLevel(r));
  return rank;
}

/** 내가 그 사람을 추방·밴하거나 역할을 바꿀 수 있는 순위인지(권한은 따로 봅니다). */
export function outranks(space: SpaceView, me: string | null | undefined, user: string): boolean {
  return !!me && user !== me && user !== space.owner && rankOf(space, me) > rankOf(space, user);
}

/** 내 순위로 고칠 수 있는 역할인지(@everyone은 순위 0이라 역할 관리 권한이 있으면 늘 됩니다). */
export function canEditRole(space: SpaceView, me: string | null | undefined, role: Role): boolean {
  return hasPerm(space, 'manage_roles') && rankOf(space, me) > roleLevel(role);
}

/** 역할에 주거나 뺄 수 있는 권한인지(데몬 may_grant: 자기에게 있는 권한만). */
export function canGrant(space: SpaceView, perm: string): boolean {
  return hasPerm(space, perm);
}

export const REPORT_ACTION_LABELS: Record<ReportAction, string> = {
  dismiss: t('기각'),
  delete_message: t('메시지 삭제'),
  kick: t('추방'),
  ban: t('밴'),
};

/** 데몬의 처리 결과 글(`동작` 또는 `동작: 메모`)을 화면 글로. */
export function formatResolution(resolution: string | null): string {
  if (!resolution) return t('처리됨');
  const i = resolution.indexOf(':');
  const action = (i >= 0 ? resolution.slice(0, i) : resolution).trim();
  const note = i >= 0 ? resolution.slice(i + 1).trim() : '';
  const label = REPORT_ACTION_LABELS[action as ReportAction] ?? action;
  return note ? `${label} · ${note}` : label;
}

/** 한 줄 미리보기: 마크다운 서식을 빼고(스포일러는 가림) 사용자 이모지 토큰은 `:이름:`, 멘션은 `@…`로 줄이고 공백을 접습니다. */
export function excerpt(text: string, n = 90, opts?: MarkdownOptions): string {
  const flat = stripMarkdown(text, opts).replace(/\s+/g, ' ').trim();
  return flat.length > n ? flat.slice(0, n) + '…' : flat;
}

/** 멤버의 최고 순위 역할(색 표시용). */
export function topRole(member: MemberView | undefined, roles: Role[]): Role | undefined {
  if (!member) return undefined;
  let best: Role | undefined;
  for (const r of roles) {
    if (member.roles.includes(r.id) && (!best || roleLevel(r) > roleLevel(best))) best = r;
  }
  return best;
}

export function displayName(member: MemberView | undefined, fallback = t('알 수 없음')): string {
  if (!member) return fallback;
  return member.nickname || member.profile.name || fallback;
}

export function base64ToBytes(b64: string): Uint8Array {
  const bin = atob(b64);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

export function bytesToBase64(bytes: Uint8Array): string {
  let s = '';
  const chunk = 0x8000;
  for (let i = 0; i < bytes.length; i += chunk) {
    s += String.fromCharCode(...bytes.subarray(i, i + chunk));
  }
  return btoa(s);
}

/** 썸네일 base64의 형식(첫 바이트로 판별). 모르면 null. */
export function sniffImageMime(b64: string): string | null {
  if (b64.startsWith('iVBORw0KGgo')) return 'image/png';
  if (b64.startsWith('/9j/')) return 'image/jpeg';
  if (b64.startsWith('R0lGOD')) return 'image/gif';
  if (b64.startsWith('UklGR')) return 'image/webp';
  if (b64.startsWith('PHN2Zy') || b64.startsWith('PD94bWwg')) return 'image/svg+xml';
  return null;
}

export function clamp(v: number, lo: number, hi: number) {
  return Math.min(hi, Math.max(lo, v));
}

/** 클래스 이름 합치기. */
export function cx(...parts: Array<string | false | null | undefined>): string {
  return parts.filter(Boolean).join(' ');
}

/**
 * 최근 값 기억(키 수 상한). 넣은 키를 맨 뒤로 옮기고, 넘치면 가장 오래된 것부터 지웁니다.
 * 설정 파일 크기 제한(64KiB)을 넘지 않게 공간별 마지막 채널 같은 맵을 묶어 둘 때 씁니다.
 */
export function rememberRecent<V>(map: Record<string, V>, key: string, value: V, max: number): Record<string, V> {
  const out: Record<string, V> = {};
  const keys = Object.keys(map).filter((k) => k !== key);
  for (const k of keys.slice(Math.max(0, keys.length - (max - 1)))) out[k] = map[k];
  out[key] = value;
  return out;
}

/**
 * 연락처·초대 링크에 다른 네트워크에서 찾아올 주소(중계 주소·onion)가 하나도 없는지. Tor로 도는 기기는 상태에 아직 없어도
 * 데몬이 키에서 나온 onion 주소를 링크에 넣으므로 늘 거짓입니다. 링크에는 자기 IP를 넣지 않으므로, 직접 연결로 띄운 데몬이
 * 릴레이 예약을 잡기 전에 만든 링크로는 같은 망 밖의 사람이 연결할 수 없습니다. 상태를 모르면 false.
 */
export function linkLacksAddress(net: Pick<NetStatus, 'mode' | 'relay_addrs' | 'onion_addr'> | null | undefined): boolean {
  return !!net && net.mode !== 'tor' && net.relay_addrs.length === 0 && !net.onion_addr;
}
