import { describe, expect, it } from 'vitest';
import { encodeMentions, isSafeHttpUrl, mentionedUsers, plainName } from './text';

describe('본문 도우미', () => {
  it('파일 이름의 양방향 제어 문자를 빼 진짜 확장자를 보인다', () => {
    expect(plainName('photo‮gpj.exe')).toBe('photogpj.exe');
    expect(plainName('a⁦b⁩‏؜.txt')).toBe('ab.txt');
    expect(plainName('사진.jpg')).toBe('사진.jpg');
  });

  it('http(s)만 외부로 연다', () => {
    expect(isSafeHttpUrl('https://a.b/c')).toBe(true);
    expect(isSafeHttpUrl('javascript:alert(1)')).toBe(false);
    expect(isSafeHttpUrl('file:///etc/passwd')).toBe(false);
    expect(isSafeHttpUrl('https://user:pw@a.b/')).toBe(false);
    expect(isSafeHttpUrl('not a url')).toBe(false);
  });

  it('고른 멘션만 ID로 바꾸고, 긴 이름을 먼저 바꾼다', () => {
    const picked = new Map([
      ['민지', 'A'.repeat(32)],
      ['민지_2', 'B'.repeat(32)],
    ]);
    const out = encodeMentions('@민지_2 @민지 @도윤', picked);
    expect(out).toBe(`<@${'B'.repeat(32)}> <@${'A'.repeat(32)}> @도윤`);
    expect(mentionedUsers(out)).toEqual(['B'.repeat(32), 'A'.repeat(32)]);
  });
});
