import { describe, expect, it } from 'vitest';
import { formatBytes, formatCountdown, formatRate, formatDuration, formatExpiresIn, formatTtl, groupDigits, remainingSecs } from './format';

describe('표시 형식', () => {
  it('시간 길이', () => {
    expect(formatDuration(30)).toBe('30초');
    expect(formatDuration(300)).toBe('5분');
    expect(formatDuration(90)).toBe('1분 30초');
    expect(formatDuration(7200)).toBe('2시간');
    expect(formatDuration(3 * 86_400)).toBe('3일');
  });
  it('채널 수명', () => {
    expect(formatTtl({ mode: 'none' })).toBeNull();
    expect(formatTtl({ mode: 'on_leave' })).toBe('퇴장 시 삭제');
    expect(formatTtl({ mode: 'seconds', seconds: 60 })).toBe('1분 후 삭제');
  });
  it('카운트다운', () => {
    expect(remainingSecs(10_500, 1_000)).toBe(9);
    expect(remainingSecs(0, 1_000)).toBe(0);
    expect(formatCountdown(42)).toBe('42초');
    expect(formatCountdown(125)).toBe('2:05');
  });
  it('임시 연락처 남은 시간', () => {
    const now = 1_000_000_000;
    expect(formatExpiresIn(undefined, now)).toBe('만료 없음');
    expect(formatExpiresIn(now + 5 * 3600_000 + 59 * 60_000, now)).toBe('5시간 후 만료');
    expect(formatExpiresIn(now + 30 * 60_000, now)).toBe('30분 후 만료');
    expect(formatExpiresIn(now + 3 * 86_400_000, now)).toBe('3일 후 만료');
    expect(formatExpiresIn(now - 1, now)).toBe('곧 만료');
  });
  it('크기와 안전 번호', () => {
    expect(formatBytes(1536)).toBe('1.5 KiB');
    expect(groupDigits('123456789012')).toEqual(['12345', '67890', '12']);
  });
  it('전송 속도(1024 기준, null은 무제한)', () => {
    expect(formatRate(0)).toBe('0 B/s');
    expect(formatRate(512)).toBe('512 B/s');
    expect(formatRate(1536)).toBe('1.5 KiB/s');
    expect(formatRate(3 * 1024 * 1024)).toBe('3.0 MiB/s');
    expect(formatRate(null)).toBe('무제한');
  });
});
