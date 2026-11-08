// 전송 속도·범위별 속도 제한 표시 규칙(순수 함수). 제한은 이 기기만의 설정이고, 전체 제한과 여러 범위가 함께 걸리면
// 데몬이 가장 낮은 값을 따릅니다.

import { ANONYMOUS_USER, type FriendView, type RateLimitView, type RateScope, type SpaceView, type UploadRate, type UserId } from '../api/types';
import { t } from '../i18n';
import { ANON_NAME } from './anon';
import { displayName } from './misc';

/** 입력 단위(1024 기준, 앱의 다른 크기 표시와 같음). */
export type RateUnit = 'KiB' | 'MiB';
export const RATE_UNITS: Record<RateUnit, number> = { KiB: 1024, MiB: 1024 * 1024 };

/** 범위를 맞춰 보는 키. */
export function scopeKey(s: RateScope): string {
  switch (s.kind) {
    case 'crew':
      return `crew:${s.space}`;
    case 'channel':
      return `channel:${s.space}/${s.channel}`;
    case 'user':
      return `user:${s.user}`;
  }
}

export function findLimit(list: readonly RateLimitView[] | null | undefined, scope: RateScope): RateLimitView | undefined {
  const k = scopeKey(scope);
  return list?.find((l) => scopeKey(l.scope) === k);
}

/** 한 범위의 제한을 바꾼 목록(둘 다 null이면 뺌). 데몬 응답을 다시 읽기 전에 화면을 바로 맞추려고 씁니다. */
export function withLimit(list: readonly RateLimitView[], scope: RateScope, up: number | null, down: number | null): RateLimitView[] {
  const k = scopeKey(scope);
  const rest = list.filter((l) => scopeKey(l.scope) !== k);
  return up === null && down === null ? rest : [...rest, { scope, up_bps: up, down_bps: down }];
}

export type LimitInput = { ok: true; bps: number | null } | { ok: false; error: string };

/** 입력 칸 → 바이트/초. 비우면 제한 없음(null), 0은 그 방향을 막음(올리지·받지 않음). */
export function parseLimitInput(text: string, unit: RateUnit): LimitInput {
  const s = text.trim();
  if (!s) return { ok: true, bps: null };
  const v = Number(s);
  if (!Number.isFinite(v) || v < 0) return { ok: false, error: t('0 이상의 숫자를 넣으세요') };
  if (v === 0) return { ok: true, bps: 0 };
  const bps = Math.round(v * RATE_UNITS[unit]);
  if (bps < 1) return { ok: false, error: t('너무 작습니다(1 B/s 이상)') };
  if (bps > Number.MAX_SAFE_INTEGER) return { ok: false, error: t('너무 큽니다') };
  return { ok: true, bps };
}

/** 저장된 값 → 입력 칸(1MiB/s 이상이면 MiB, 아니면 KiB, 소수 둘째 자리까지). */
export function limitToInput(bps: number | null | undefined): { text: string; unit: RateUnit } {
  if (bps == null) return { text: '', unit: 'KiB' };
  if (bps === 0) return { text: '0', unit: 'KiB' };
  const unit: RateUnit = bps >= RATE_UNITS.MiB ? 'MiB' : 'KiB';
  return { text: String(Math.round((bps / RATE_UNITS[unit]) * 100) / 100), unit };
}

export interface ScopeNames {
  views: Record<string, SpaceView>;
  friends: FriendView[];
}

/** 사람 이름: 내가 아는 공간의 멤버 이름(별명 포함) → 친구 프로필 → 알 수 없음. 익명 자리는 '익명'. */
export function userName(user: UserId, ctx: ScopeNames, space?: string): string {
  if (user === ANONYMOUS_USER) return ANON_NAME;
  const inSpace = space ? ctx.views[space]?.members.find((m) => m.user === user) : undefined;
  if (inSpace) return displayName(inSpace);
  for (const v of Object.values(ctx.views)) {
    const m = v.members.find((x) => x.user === user);
    if (m) return displayName(m);
  }
  return ctx.friends.find((f) => f.user === user)?.profile.name || t('알 수 없는 사용자');
}

/** 공간 이름: 크루 이름, 1대1이면 상대 이름. */
export function spaceName(space: string, ctx: ScopeNames): string {
  const v = ctx.views[space];
  const friend = ctx.friends.find((f) => f.space === space);
  if (friend) return `1:1 ${friend.profile.name}`;
  if (!v) return t('알 수 없는 크루');
  return v.kind === 'crew' ? v.settings.name : `1:1 ${v.settings.name}`;
}

/** 범위의 종류 이름과 화면 이름(크루 이름, #채널 · 크루, 사람 이름). */
export function describeScope(scope: RateScope, ctx: ScopeNames): { kind: string; label: string } {
  switch (scope.kind) {
    case 'crew':
      return { kind: t('크루'), label: spaceName(scope.space, ctx) };
    case 'channel': {
      const ch = ctx.views[scope.space]?.channels.find((c) => c.channel === scope.channel);
      return { kind: t('채널'), label: t('#{channel} · {space}', { channel: ch?.settings.name ?? t('알 수 없는 채널'), space: spaceName(scope.space, ctx) }) };
    }
    case 'user':
      return { kind: t('사람'), label: userName(scope.user, ctx) };
  }
}

/** 이 첨부(공간·블롭)를 지금 받는 사람(빠른 순). */
export function uploadsFor(uploads: readonly UploadRate[] | undefined, space: string, blob: string): UploadRate[] {
  if (!uploads?.length) return EMPTY;
  const out = uploads.filter((u) => u.space === space && u.blob === blob);
  return out.length ? out.sort((a, b) => b.bps - a.bps) : EMPTY;
}

const EMPTY: UploadRate[] = [];
