import { describe, expect, it } from 'vitest';
import { emojiPlain, formatEmoji, hasEmoji, isEmojiOnly, parseEmojiLimits, parseEmojiTokens, parseReactionEmoji, validEmojiName } from './emoji';

describe('커스텀 이모지 표기', () => {
  it('정지·애니메이션 표기를 위치와 함께 찾는다', () => {
    const text = '안녕 <:yangpa:123> 그리고 <a:dance:18446744073709551615>!';
    const toks = parseEmojiTokens(text);
    expect(toks).toHaveLength(2);
    expect(toks[0]).toMatchObject({ animated: false, name: 'yangpa', id: '123' });
    expect(text.slice(toks[0].start, toks[0].end)).toBe('<:yangpa:123>');
    expect(toks[1]).toMatchObject({ animated: true, name: 'dance', id: '18446744073709551615' });
  });

  it('u64를 넘는 ID, 잘못된 이름, 깨진 표기는 무시한다', () => {
    expect(parseEmojiTokens('<:big:18446744073709551616>')).toEqual([]);
    expect(parseEmojiTokens('<:a:1>')).toEqual([]); // 이름 1자
    expect(parseEmojiTokens('<:bad-name:1>')).toEqual([]);
    expect(parseEmojiTokens('<:name:12a>')).toEqual([]);
    expect(parseEmojiTokens('<:name:>')).toEqual([]);
    expect(parseEmojiTokens('<b:name:1>')).toEqual([]);
    expect(parseEmojiTokens('<:name:1')).toEqual([]);
  });

  it('한글 이름과 앞자리 0을 허용한다', () => {
    expect(validEmojiName('양파')).toBe(true);
    expect(validEmojiName('x'.repeat(33))).toBe(false);
    expect(parseEmojiTokens('<:양파_웃음:007>')[0]).toMatchObject({ name: '양파_웃음', id: '007' });
  });

  it('formatEmoji와 parseReactionEmoji가 왕복한다', () => {
    const s = formatEmoji('gg', '42', true);
    expect(s).toBe('<a:gg:42>');
    expect(parseReactionEmoji(s)).toMatchObject({ name: 'gg', id: '42', animated: true });
    expect(parseReactionEmoji('👍')).toBeNull();
    expect(parseReactionEmoji(' <:gg:42>')).toBeNull();
  });

  it('이모지만 있는 메시지를 가린다(숫자는 이모지가 아님)', () => {
    expect(isEmojiOnly('🔥🔥')).toBe(true);
    expect(isEmojiOnly('<:yangpa:1> <:gg:2>')).toBe(true);
    expect(isEmojiOnly('👍🏽 ❤️')).toBe(true);
    expect(isEmojiOnly('123')).toBe(false);
    expect(isEmojiOnly('#')).toBe(false);
    expect(isEmojiOnly('좋아요 👍')).toBe(false);
    expect(isEmojiOnly('')).toBe(false);
  });
});

describe('역할 이름 등의 이모지 표기·크루 이모지 한도', () => {
  it('표기를 :이름:으로 바꾼다', () => {
    expect(emojiPlain('운영 <:yangpa:1> 팀 <a:dance:2>')).toBe('운영 :yangpa: 팀 :dance:');
    expect(emojiPlain('보통 이름')).toBe('보통 이름');
  });

  it('한도 입력을 읽는다(빈 칸은 한도 없음)', () => {
    expect(parseEmojiLimits('', '')).toEqual({ ok: true, limits: { max_count: null, max_bytes: null } });
    expect(parseEmojiLimits(' 50 ', '512')).toEqual({ ok: true, limits: { max_count: 50, max_bytes: 512 * 1024 } });
    expect(parseEmojiLimits('0', '').ok).toBe(false);
    expect(parseEmojiLimits('501', '').ok).toBe(false);
    expect(parseEmojiLimits('1.5', '').ok).toBe(false);
    expect(parseEmojiLimits('', '0').ok).toBe(false);
    expect(parseEmojiLimits('', 'abc').ok).toBe(false);
  });
});

// 데몬 시험(yangpa-core badge.rs `emoji_in_names`)과 같은 예.
describe('이름의 이모지', () => {
  it('그림 문자·국기·키캡·이모지 모양 선택자를 찾는다', () => {
    for (const s of ['철수🧅', '☕', '⛺', '★별', '♥', '🇰🇷', '1\uFE0F\u20E3', '👨‍👩‍👧', '⭐', '⌚']) expect(hasEmoji(s), s).toBe(true);
  });
  it('글자와 글자 기호는 둔다', () => {
    for (const s of ['철수', 'Alice_99', '日本語', '©®™', '→←', 'a\u200Db', '‼', '(・ω・)', 'Ωμ', '١٢٣']) expect(hasEmoji(s), s).toBe(false);
  });
});
