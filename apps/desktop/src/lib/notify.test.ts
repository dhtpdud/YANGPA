import { describe, expect, it } from 'vitest';
import { MAX_NOTIFY_RULES, MUTE_FOREVER, isMuted, muteLabel, muteUntil, ruleKey, sanitizeRules, withRule } from './notify';

const NOW = 1_000_000;

describe('크루·채널 알림 규칙', () => {
  it('키는 크루면 공간 ID, 채널이면 공간/채널', () => {
    expect(ruleKey('S')).toBe('S');
    expect(ruleKey('S', 'C')).toBe('S/C');
    expect(ruleKey('S', null)).toBe('S');
  });

  it('규칙을 바꾸고, 수준도 없고 끄지도 않은 규칙은 뺀다', () => {
    let r = withRule({}, 'S', { level: 'mentions' }, NOW);
    expect(r).toEqual({ S: { level: 'mentions' } });
    r = withRule(r, 'S/C', { muteUntil: muteUntil(60_000, NOW) }, NOW);
    expect(isMuted(r, 'S/C', NOW)).toBe(true);
    expect(isMuted(r, 'S/C', NOW + 60_000)).toBe(false);
    expect(isMuted(r, 'S', NOW)).toBe(false);
    // 수준을 '따르기'로 돌리면 크루 규칙이 사라집니다.
    r = withRule(r, 'S', { level: undefined }, NOW);
    expect(r).toEqual({ 'S/C': { muteUntil: NOW + 60_000 } });
    expect(withRule(r, 'S/C', { muteUntil: undefined }, NOW)).toEqual({});
  });

  it('직접 켤 때까지 끄기는 시각이 지나도 남고, 부제가 다르다', () => {
    const r = withRule({}, 'S', { muteUntil: muteUntil(MUTE_FOREVER, NOW) }, NOW);
    expect(r.S.muteUntil).toBe(MUTE_FOREVER);
    expect(isMuted(r, 'S', Date.now() + 10 * 365 * 86_400_000)).toBe(true);
    expect(muteLabel(MUTE_FOREVER)).toBe('직접 켤 때까지 끔');
    expect(muteLabel(NOW + 60_000, NOW)).toMatch(/까지 끔$/);
  });

  it('상한을 넘으면 오래 손대지 않은 규칙부터 버린다', () => {
    let r = {};
    for (let i = 0; i <= MAX_NOTIFY_RULES; i++) r = withRule(r, `S${i}`, { level: 'off' }, NOW);
    expect(Object.keys(r)).toHaveLength(MAX_NOTIFY_RULES);
    expect(r).not.toHaveProperty('S0');
    // 다시 바꾼 규칙은 맨 뒤로 가서 남습니다.
    r = withRule(r, 'S1', { level: 'all' }, NOW);
    r = withRule(r, 'new', { level: 'all' }, NOW);
    expect(r).toHaveProperty('S1');
    expect(r).not.toHaveProperty('S2');
  });

  it('저장된 값에서 아는 모양만 받고 끝난 끄기는 버린다', () => {
    expect(sanitizeRules(null, NOW)).toEqual({});
    expect(sanitizeRules('x', NOW)).toEqual({});
    expect(
      sanitizeRules(
        {
          A: { level: 'mentions', muteUntil: NOW + 5 },
          B: { level: 'loud', muteUntil: NOW - 1 },
          C: { muteUntil: 1.5 },
          D: 3,
          E: { level: 'off', muteUntil: 'soon' },
          ['x'.repeat(97)]: { level: 'all' },
        },
        NOW,
      ),
    ).toEqual({ A: { level: 'mentions', muteUntil: NOW + 5 }, E: { level: 'off' } });
  });
});
