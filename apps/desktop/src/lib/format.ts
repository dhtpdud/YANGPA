// 시각·크기·수명 표시(화면 언어). 형식기는 언어가 정해진 뒤(main.tsx) 처음 불러올 때 한 번 만듭니다.

import type { ChatTtl, ChannelSettings } from '../api/types';
import { currentLang, t } from '../i18n';

const LOCALE = currentLang();
const timeFmt = new Intl.DateTimeFormat(LOCALE, { hour: 'numeric', minute: '2-digit' });
const dayFmt = new Intl.DateTimeFormat(LOCALE, { year: 'numeric', month: 'long', day: 'numeric', weekday: 'long' });
const fullFmt = new Intl.DateTimeFormat(LOCALE, {
  year: 'numeric',
  month: 'long',
  day: 'numeric',
  hour: 'numeric',
  minute: '2-digit',
});
const shortDateFmt = new Intl.DateTimeFormat(LOCALE, { year: '2-digit', month: '2-digit', day: '2-digit' });
const relFmt = new Intl.RelativeTimeFormat(LOCALE, { numeric: 'always' });
const unit = (n: number, u: 'second' | 'minute' | 'hour' | 'day') => new Intl.NumberFormat(LOCALE, { style: 'unit', unit: u, unitDisplay: 'long' }).format(n);

/** 프로필 카드의 마지막 동기화 시각(년월일시분). */
export function formatLastSeen(ms: number): string {
  return t('마지막 동기화 {time}', { time: formatFull(ms) });
}

/** 같은 날인지 판단하는 키(현지 시각 기준). */
export function dayKey(ms: number): string {
  const d = new Date(ms);
  return `${d.getFullYear()}-${d.getMonth() + 1}-${d.getDate()}`;
}

export function formatTime(ms: number): string {
  return timeFmt.format(ms);
}

export function formatDay(ms: number): string {
  return dayFmt.format(ms);
}

export function formatFull(ms: number): string {
  return fullFmt.format(ms);
}

/** 메시지 머리글 시각: 오늘이면 '오늘 오후 3:04', 어제면 '어제 …', 그 외 날짜. */
export function formatStamp(ms: number, now = Date.now()): string {
  const today = dayKey(now);
  const yesterday = dayKey(now - 86_400_000);
  const k = dayKey(ms);
  if (k === today) return t('오늘 {time}', { time: formatTime(ms) });
  if (k === yesterday) return t('어제 {time}', { time: formatTime(ms) });
  return `${shortDateFmt.format(ms)} ${formatTime(ms)}`;
}

export function formatRelative(ms: number, now = Date.now()): string {
  const s = Math.round((now - ms) / 1000);
  if (s < 0) return formatFull(ms);
  if (s < 60) return t('방금');
  if (s < 3600) return relFmt.format(-Math.floor(s / 60), 'minute');
  if (s < 86_400) return relFmt.format(-Math.floor(s / 3600), 'hour');
  if (s < 86_400 * 30) return relFmt.format(-Math.floor(s / 86_400), 'day');
  return shortDateFmt.format(ms);
}

export function formatBytes(n: number): string {
  if (!Number.isFinite(n) || n < 0) return '-';
  if (n < 1024) return `${n} B`;
  const units = ['KiB', 'MiB', 'GiB', 'TiB'];
  let v = n / 1024;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v < 10 ? v.toFixed(1) : Math.round(v)} ${units[i]}`;
}

/** 초를 짧게: 30초, 5분, 2시간, 1일. */
export function formatDuration(secs: number): string {
  if (secs < 60) return unit(secs, 'second');
  if (secs < 3600) return secs % 60 === 0 ? unit(secs / 60, 'minute') : `${unit(Math.floor(secs / 60), 'minute')} ${unit(secs % 60, 'second')}`;
  if (secs < 86_400) return secs % 3600 === 0 ? unit(secs / 3600, 'hour') : `${unit(Math.floor(secs / 3600), 'hour')} ${unit(Math.floor((secs % 3600) / 60), 'minute')}`;
  return unit(Math.floor(secs / 86_400), 'day');
}

export function formatTtl(ttl: ChatTtl): string | null {
  switch (ttl.mode) {
    case 'none':
      return null;
    case 'on_leave':
      return t('퇴장 시 삭제');
    case 'seconds':
      return t('{time} 후 삭제', { time: formatDuration(ttl.seconds) });
  }
}

/** 남은 시간(초 단위, 내림). */
export function remainingSecs(expiresAt: number, now = Date.now()): number {
  return Math.max(0, Math.floor((expiresAt - now) / 1000));
}

/** 임시 연락처의 남은 시간: '5시간 후 만료', 만료 시각이 없으면 '만료 없음'. 시간 단위는 내림해 늦게 말하지 않습니다. */
export function formatExpiresIn(expiresAt: number | undefined, now = Date.now()): string {
  if (expiresAt === undefined) return t('만료 없음');
  const secs = remainingSecs(expiresAt, now);
  if (secs < 60) return t('곧 만료');
  const when =
    secs < 3600 ? relFmt.format(Math.floor(secs / 60), 'minute') : secs < 86_400 ? relFmt.format(Math.floor(secs / 3600), 'hour') : relFmt.format(Math.floor(secs / 86_400), 'day');
  return t('{when} 만료', { when });
}

export function formatCountdown(secs: number): string {
  if (secs >= 3600) return unit(Math.floor(secs / 3600), 'hour');
  if (secs >= 60) return `${Math.floor(secs / 60)}:${String(secs % 60).padStart(2, '0')}`;
  return unit(secs, 'second');
}

/** 대역폭(B/s). null은 무제한. */
export function formatRate(bps: number | null): string {
  return bps == null ? t('무제한') : `${formatBytes(bps)}/s`;
}

/** 속도 제한 표시: null은 무제한, 0은 막음(그 방향으로 아무것도 보내거나 받지 않음). */
export function formatLimit(bps: number | null, unlimited = t('무제한')): string {
  if (bps == null) return unlimited;
  return bps === 0 ? t('0(막음)') : formatRate(bps);
}

export function channelBadges(s: ChannelSettings): string[] {
  const out: string[] = [];
  if (s.security === 'volatile') out.push(t('휘발성'));
  if (s.join_mode === 'join_leave') out.push(t('입장·퇴장'));
  const ttl = formatTtl(s.ttl);
  if (ttl) out.push(ttl);
  return out;
}

/** 안전 번호(60자리)를 5자리 12묶음으로. */
export function groupDigits(digits: string): string[] {
  const clean = digits.replace(/\D/g, '');
  const out: string[] = [];
  for (let i = 0; i < clean.length; i += 5) out.push(clean.slice(i, i + 5));
  return out;
}
