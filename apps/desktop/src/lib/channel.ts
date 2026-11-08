// 채널 설정 폼 ↔ channel.create·channel.update 인자 변환(순수 함수).
//
// 규칙(api/params.rs, CLI와 같음):
// - 수명: 초 단위면 ttl_secs만, 퇴장 시 삭제면 ttl_on_leave만, '없음'으로 바꿀 때는 ttl_secs 0 + ttl_on_leave false.
// - channel.update는 바뀐 필드만 보냅니다. 다른 도구(CLI·MCP)로 바꾼 설정을 폼의 값으로 덮어쓰지 않기 위해서입니다.
// - 입장 방식·수명은 보안 등급과 상관없이 텍스트 채널이면 쓸 수 있습니다(decisions.md: 일반 등급 채널의 수명 허용).
// - P2P 전용은 텍스트·통화 채널만, 내 기기끼리 공간은 안 됩니다(데몬 해소 규칙). 만든 뒤에는 `channel.set_p2p`로 바꿉니다.
// - 익명은 크루의 카테고리가 아닌 채널만 됩니다. 만들 때는 함께 보내고, 만든 뒤에는 `channel.update`의 `anonymous`만 따로 보냅니다.
// - 권한은 `overwrites`(목록 전체)로 보냅니다. 비공개 스위치도 그 목록을 보는 창이라 `private_roles`는 쓰지 않습니다.
// - 포럼 태그는 포럼 채널만, 고칠 수 있는 사람(채널 관리 권한)일 때만 `forum_tags`(목록 전체)로 보냅니다.
// - 첨부 한도는 크루의 카테고리가 아닌 채널만 `attach_limit`(바이트, null이면 무제한, 0이면 첨부 금지)으로 정합니다.
//   만들 때 빠뜨리면 데몬이 크루의 휘발성 채널만 10MiB로 두므로, 폼은 만들 때도 늘 값을 보냅니다.
// - 수정·삭제 금지도 크루의 카테고리가 아닌 채널만 `no_edit`·`no_delete`로 정합니다.

import type {
  ChannelCreateParams,
  ChannelId,
  ChannelKind,
  ChannelSettings,
  ChannelUpdateParams,
  ChannelView,
  ChatTtl,
  ForumTag,
  HistoryVisibility,
  JoinMode,
  PermissionOverwrite,
  SecurityLevel,
  SpaceId,
  SpaceKind,
} from '../api/types';
import { t } from '../i18n';
import { formatDuration } from './format';
import { cleanTag, sameTags } from './forumTags';
import { compactOverwrites, sameOverwrites } from './perms';

export const TTL_MIN_SECS = 5;
const MIB = 1024 * 1024;
/** 채널 첨부 한도의 최솟값(데몬 `MIN_ATTACH_LIMIT`과 같음). 0(첨부 금지)은 따로 됩니다. */
export const ATTACH_LIMIT_MIN = MIB;
/** 휘발성 채널 첨부 한도의 기본값(데몬 `VOLATILE_DEFAULT_ATTACH_LIMIT`과 같음). 넘으면 RAM 경고를 띄웁니다. */
export const VOLATILE_ATTACH_DEFAULT = 10 * MIB;
export const TTL_MAX_SECS = 30 * 86_400;

export interface ChannelForm {
  name: string;
  topic: string;
  parent: ChannelId | null;
  joinMode: JoinMode;
  ttl: ChatTtl;
  history: HistoryVisibility;
  linkPreviews: boolean;
  /** 권한 덮어쓰기(빈 항목이 섞여 있어도 됨, 보낼 때 버림). */
  overwrites: PermissionOverwrite[];
  /** 만들 때만: P2P 전용 채널로 만들기. */
  p2p?: boolean;
  /** 만들 때만: 익명 채널로 만들기. */
  anonymous?: boolean;
  /** 포럼 태그(목록 전체). 고칠 수 없으면 undefined(보내지 않음). */
  forumTags?: ForumTag[];
  /** 첨부 하나의 최대 바이트(null이면 무제한, 0이면 첨부 금지). 고칠 수 없으면 undefined(보내지 않음). */
  attachLimit?: number | null;
  /** 수정·삭제 금지. 고칠 수 없으면 undefined(보내지 않음). */
  noEdit?: boolean;
  noDelete?: boolean;
  /** 상시 휘발성 채널의 참가 기기 상한. 고칠 수 없으면 undefined(보내지 않음). */
  volatileCap?: number;
}

/** 지금 채널의 첨부 한도·수정·삭제 금지(채널 보기에서). */
export type ChannelRules = Pick<ChannelView, 'attach_limit' | 'no_edit' | 'no_delete' | 'volatile_cap'>;

export type ChannelPatch = Omit<ChannelUpdateParams, 'space' | 'channel'>;

export function clampTtl(secs: number): number {
  if (!Number.isFinite(secs)) return TTL_MIN_SECS;
  return Math.max(TTL_MIN_SECS, Math.min(TTL_MAX_SECS, Math.round(secs)));
}

/** 입장 방식·수명을 쓸 수 있는 채널 종류. */
export function hasChatLifetime(kind: ChannelKind): boolean {
  return kind === 'text';
}

/** P2P 전용으로 둘 수 있는 채널인지(텍스트·음성, 내 기기끼리 공간 밖). */
export function canBeP2p(kind: ChannelKind, spaceKind: SpaceKind | undefined): boolean {
  return (kind === 'text' || kind === 'voice') && spaceKind !== 'self_space';
}

/** 익명으로 둘 수 있는 채널인지(크루의, 카테고리가 아닌 채널). */
export function canBeAnonymous(kind: ChannelKind, spaceKind: SpaceKind | undefined): boolean {
  return spaceKind === 'crew' && kind !== 'category';
}

/** 첨부 한도를 둘 수 있는 채널인지(크루의, 카테고리가 아닌 채널). */
export function canHaveAttachLimit(kind: ChannelKind, spaceKind: SpaceKind | undefined): boolean {
  return spaceKind === 'crew' && kind !== 'category';
}

export type AttachLimitInput = { ok: true; bytes: number | null } | { ok: false; error: string };

/** 입력한 MiB → 한도 바이트. 비우면 무제한(null), 0은 첨부 금지, 그 밖에는 1MiB 이상이어야 합니다. */
export function parseAttachLimit(text: string): AttachLimitInput {
  const s = text.trim();
  if (!s) return { ok: true, bytes: null };
  const v = Number(s);
  if (!Number.isFinite(v) || v < 0) return { ok: false, error: t('0 이상의 숫자(MiB)를 넣어 주세요') };
  if (v === 0) return { ok: true, bytes: 0 };
  const bytes = Math.round(v * MIB);
  if (bytes < ATTACH_LIMIT_MIN) return { ok: false, error: t('0(첨부 금지)이거나 1MiB 이상이어야 합니다') };
  return { ok: true, bytes };
}

/** 휘발성 채널에서 RAM 경고를 띄울 한도인지(무제한이거나 기본값보다 큼). */
export function volatileLimitTooBig(bytes: number | null): boolean {
  return bytes === null || bytes > VOLATILE_ATTACH_DEFAULT;
}

/** 한도 바이트 → 입력란 MiB(소수 둘째 자리까지). */
export function attachLimitMib(bytes: number): string {
  return String(Math.round((bytes / MIB) * 100) / 100);
}

/** P2P 전용 채널인데 이 기기가 아직 참가하지 않아 잠겨 있는지. 잠긴 동안은 내용·입력창·통화를 열지 않습니다. */
export function isP2pLocked(c: Pick<ChannelView, 'p2p' | 'p2p_joined'> | undefined): boolean {
  return !!c?.p2p && !c.p2p_joined;
}

/** 수명 → API 필드. 만들 때 '없음'은 보내지 않고, 바꿀 때는 지우는 값을 보냅니다. */
export function ttlParams(ttl: ChatTtl, forUpdate: boolean): Pick<ChannelCreateParams, 'ttl_secs' | 'ttl_on_leave'> {
  switch (ttl.mode) {
    case 'seconds':
      return { ttl_secs: clampTtl(ttl.seconds) };
    case 'on_leave':
      return { ttl_on_leave: true };
    case 'none':
      return forUpdate ? { ttl_secs: 0, ttl_on_leave: false } : {};
  }
}

export function sameTtl(a: ChatTtl, b: ChatTtl): boolean {
  if (a.mode !== b.mode) return false;
  return a.mode !== 'seconds' || (b.mode === 'seconds' && clampTtl(a.seconds) === clampTtl(b.seconds));
}

export function channelCreateParams(
  space: SpaceId,
  kind: ChannelKind,
  security: SecurityLevel,
  form: ChannelForm,
  spaceKind?: SpaceKind,
): ChannelCreateParams {
  const p: ChannelCreateParams = {
    space,
    kind,
    name: form.name.trim(),
    security: hasChatLifetime(kind) ? security : 'normal',
  };
  if (kind !== 'category') {
    if (form.parent) p.parent = form.parent;
    if (form.topic.trim()) p.topic = form.topic.trim();
  }
  const overwrites = compactOverwrites(form.overwrites);
  if (overwrites.length > 0) p.overwrites = overwrites;
  if (kind === 'text' || kind === 'forum') {
    p.history = form.history;
    p.link_previews = form.linkPreviews;
  }
  if (hasChatLifetime(kind)) {
    p.join_mode = form.joinMode;
    Object.assign(p, ttlParams(form.ttl, false));
  }
  // 고를 수 없는 종류로 바꾼 뒤 남은 체크는 보내지 않습니다(데몬이 채널 만들기를 통째로 거절하므로).
  if (form.p2p && canBeP2p(kind, spaceKind)) p.p2p = true;
  if (form.anonymous && canBeAnonymous(kind, spaceKind)) p.anonymous = true;
  if (kind === 'forum' && form.forumTags?.length) p.forum_tags = form.forumTags.map(cleanTag);
  if (canHaveAttachLimit(kind, spaceKind)) {
    if (form.attachLimit !== undefined) p.attach_limit = form.attachLimit;
    if (form.noEdit) p.no_edit = true;
    if (form.noDelete) p.no_delete = true;
  }
  return p;
}

/** 바뀐 필드만 담은 channel.update 인자. 바뀐 것이 없으면 빈 객체. `rules`는 지금 채널의 첨부 한도·수정·삭제 금지. */
export function channelUpdatePatch(before: ChannelSettings, form: ChannelForm, rules: ChannelRules = {}): ChannelPatch {
  const p: ChannelPatch = {};
  const kind = before.kind;
  const name = form.name.trim();
  if (name !== before.name) p.name = name;
  if (kind !== 'category') {
    if (form.topic.trim() !== before.topic) p.topic = form.topic.trim();
    if ((form.parent ?? null) !== (before.parent ?? null)) p.parent = form.parent ?? null;
  }
  if (!sameOverwrites(form.overwrites, before.overwrites)) p.overwrites = compactOverwrites(form.overwrites);
  if (kind === 'text' || kind === 'forum') {
    if (form.history !== before.history) p.history = form.history;
    if (form.linkPreviews !== before.link_previews) p.link_previews = form.linkPreviews;
  }
  if (hasChatLifetime(kind)) {
    if (form.joinMode !== before.join_mode) p.join_mode = form.joinMode;
    if (!sameTtl(form.ttl, before.ttl)) Object.assign(p, ttlParams(form.ttl, true));
  }
  if (kind === 'forum' && form.forumTags) {
    const tags = form.forumTags.map(cleanTag);
    if (!sameTags(tags, before.forum_tags ?? [])) p.forum_tags = tags;
  }
  if (form.attachLimit !== undefined && form.attachLimit !== (rules.attach_limit ?? null)) p.attach_limit = form.attachLimit;
  if (form.noEdit !== undefined && form.noEdit !== !!rules.no_edit) p.no_edit = form.noEdit;
  if (form.noDelete !== undefined && form.noDelete !== !!rules.no_delete) p.no_delete = form.noDelete;
  if (form.volatileCap !== undefined && form.volatileCap !== rules.volatile_cap) p.volatile_cap = form.volatileCap;
  return p;
}

/**
 * 휘발성 채널 내용이 이 기기에서 언제 지워지는지(안내 배너 문구). 설정과 다른 약속을 하지 않도록 수명별로 씁니다.
 * 수명 타이머는 각 기기가 받은 시점부터 잽니다(기기 간 시계 차이를 피하려고).
 */
export function volatileLifetimeText(s: Pick<ChannelSettings, 'ttl' | 'join_mode'>): string {
  switch (s.ttl.mode) {
    case 'seconds':
      return t('각 기기가 받은 시점부터 {duration} 뒤에 그 기기에서 지웁니다', { duration: formatDuration(s.ttl.seconds) });
    case 'on_leave':
      return s.join_mode === 'join_leave' ? t('퇴장하면 이 기기에서 바로 지웁니다') : t('채널에서 나가면 이 기기에서 바로 지웁니다');
    case 'none':
      return t('수명이 없어 앱을 끌 때까지 메모리에 남고, 앱을 끄면 모두 사라집니다');
  }
}

/** 입장·퇴장 채널에서 퇴장하면 받은 내용이 어떻게 되는지. */
export function leaveEffectText(s: Pick<ChannelSettings, 'ttl' | 'security'>): string {
  if (s.ttl.mode === 'on_leave') return t('퇴장하면 이 기기의 내용과 키를 바로 지웁니다.');
  if (s.security === 'volatile') return t('퇴장해도 이미 받은 내용은 수명이 끝나거나 앱을 끌 때까지 이 기기 메모리에 남습니다.');
  return t('퇴장해도 이미 받은 기록은 이 기기에 남습니다.');
}

/** 끌어 놓을 자리: 대상 채널의 앞·뒤, 또는 카테고리 맨 위. */
export type DropWhere = 'before' | 'after' | 'into';

export type ChannelMove = Pick<ChannelUpdateParams, 'channel' | 'position' | 'parent'>;

/**
 * 끌어 옮기기 → 보낼 channel.update 인자들. `sorted`는 sortChannels 결과 전체(접힌 카테고리 안까지)여야 합니다.
 * 옮겨 갈 묶음(같은 상위의 채널들, 또는 카테고리들)만 지금 보이는 순서대로 0부터 다시 매기고 바뀐 것만 보냅니다.
 * 데몬이 새 채널 위치를 채널 수로 두어 겹치는 위치가 많고, 안 바뀐 채널까지 고치면 상태 이벤트만 늘기 때문입니다.
 * 카테고리는 카테고리끼리만, 다른 채널은 최상위·카테고리 사이를 옮깁니다.
 */
export function planChannelMove(sorted: ChannelView[], dragged: ChannelId, target: ChannelId, where: DropWhere): ChannelMove[] {
  const isCat = (c: ChannelView) => c.settings.kind === 'category';
  const d = sorted.find((c) => c.channel === dragged);
  const tg = sorted.find((c) => c.channel === target);
  if (!d || !tg || d === tg) return [];
  const cat = isCat(d);
  const into = where === 'into';
  if (cat ? !isCat(tg) || into : isCat(tg) !== into) return [];
  const parent = cat ? null : into ? tg.channel : tg.settings.parent;
  const group = sorted.filter((c) => c !== d && (cat ? isCat(c) : !isCat(c) && c.settings.parent === parent));
  group.splice(into ? 0 : group.indexOf(tg) + (where === 'after' ? 1 : 0), 0, d);
  const parentChanged = !cat && d.settings.parent !== parent;
  return group.flatMap((c, i): ChannelMove[] => {
    if (c === d && parentChanged) return [{ channel: c.channel, parent, position: i }];
    return c.settings.position === i ? [] : [{ channel: c.channel, position: i }];
  });
}

/** 상시 휘발성 채널 참가 상한의 범위(데몬과 같음)와 기본값. */
export const VOLATILE_CAP_MIN = 2;
export const VOLATILE_CAP_MAX = 2000;
export const VOLATILE_CAP_DEFAULT = 200;

/** 참가 상한 입력란 글자 → 값. 범위 밖이면 오류 문구. */
export function parseVolatileCap(text: string): { ok: true; cap: number } | { ok: false; error: string } {
  const s = text.trim();
  const n = Number(s);
  if (s === '' || !Number.isInteger(n) || n < VOLATILE_CAP_MIN || n > VOLATILE_CAP_MAX) {
    return { ok: false, error: t('{min}~{max} 사이의 정수로 적어 주세요', { min: VOLATILE_CAP_MIN, max: VOLATILE_CAP_MAX }) };
  }
  return { ok: true, cap: n };
}
