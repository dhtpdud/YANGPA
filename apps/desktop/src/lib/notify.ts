// 크루·채널별 알림 규칙(UI 설정 `notifyRules`). OS 알림은 src-tauri/src/notify.rs가 같은 키를 읽어 정합니다.
// 키는 크루면 공간 ID, 채널이면 `공간/채널`. 휘발성 채널 ID는 디스크에 남기지 않으므로 그 채널의 규칙은 만들지 않습니다.

import type { ChannelId, SpaceId } from '../api/types';
import { t } from '../i18n';
import type { NotifyCrew } from '../store/store';
import { formatStamp } from './format';

export interface NotifyRule {
  /** 없으면 위(채널 → 크루 → 설정의 크루 메시지 알림)를 따릅니다. */
  level?: NotifyCrew;
  /** 이 시각(ms)까지 알림 끔. 끄는 동안은 '모든 메시지'도 멘션만 알립니다. */
  muteUntil?: number;
}

export type NotifyRules = Record<string, NotifyRule>;

/** 기억하는 규칙 수(notify.rs `MAX_RULES`와 같음, 설정 파일 64KiB 안에 들게). */
export const MAX_NOTIFY_RULES = 100;
/** '직접 켤 때까지' 끔. JSON 정수로 그대로 오가는 가장 큰 값입니다. */
export const MUTE_FOREVER = Number.MAX_SAFE_INTEGER;

export const MUTE_CHOICES: ReadonlyArray<[number, string]> = [
  [15 * 60_000, t('15분 동안')],
  [60 * 60_000, t('1시간 동안')],
  [3 * 60 * 60_000, t('3시간 동안')],
  [8 * 60 * 60_000, t('8시간 동안')],
  [24 * 60 * 60_000, t('24시간 동안')],
  [MUTE_FOREVER, t('직접 켤 때까지')],
];

export const LEVEL_LABELS: Record<NotifyCrew, string> = { all: t('모든 메시지'), mentions: t('@멘션만'), off: t('알림 없음') };

export const ruleKey = (space: SpaceId, channel?: ChannelId | null): string => (channel ? `${space}/${channel}` : space);

export function isMuted(rules: NotifyRules, key: string, now = Date.now()): boolean {
  return (rules[key]?.muteUntil ?? 0) > now;
}

/** 지금 끈 이 규칙이 언제 풀리는지(메뉴 부제). */
export function muteLabel(until: number, now = Date.now()): string {
  return until >= MUTE_FOREVER ? t('직접 켤 때까지 끔') : t('{time}까지 끔', { time: formatStamp(until, now) });
}

/** 끄기 시간 → 끝나는 시각. */
export const muteUntil = (ms: number, now = Date.now()): number => (ms >= MUTE_FOREVER ? MUTE_FOREVER : now + ms);

/** 규칙 하나를 바꾼 새 목록. 빈 규칙(수준도 없고 끄지도 않음)은 빼고, 바꾼 것을 맨 뒤로 옮겨 오래된 것부터 버립니다. */
export function withRule(rules: NotifyRules, key: string, patch: NotifyRule, now = Date.now()): NotifyRules {
  const { [key]: old, ...rest } = rules;
  const next = { ...old, ...patch };
  const keep: NotifyRule = {};
  if (next.level) keep.level = next.level;
  if ((next.muteUntil ?? 0) > now) keep.muteUntil = next.muteUntil;
  const out = Object.keys(keep).length ? { ...rest, [key]: keep } : rest;
  return Object.fromEntries(Object.entries(out).slice(-MAX_NOTIFY_RULES));
}

const LEVELS: readonly string[] = ['all', 'mentions', 'off'];

/** 저장된 값에서 아는 모양만 받고, 끝난 끄기는 버립니다. */
export function sanitizeRules(raw: unknown, now = Date.now()): NotifyRules {
  if (!raw || typeof raw !== 'object') return {};
  let out: NotifyRules = {};
  for (const [k, v] of Object.entries(raw as Record<string, unknown>)) {
    if (k.length > 96 || !v || typeof v !== 'object') continue;
    const x = v as Record<string, unknown>;
    const level = typeof x.level === 'string' && LEVELS.includes(x.level) ? (x.level as NotifyCrew) : undefined;
    const until = typeof x.muteUntil === 'number' && Number.isSafeInteger(x.muteUntil) ? x.muteUntil : undefined;
    out = withRule(out, k, { level, muteUntil: until }, now);
  }
  return out;
}
