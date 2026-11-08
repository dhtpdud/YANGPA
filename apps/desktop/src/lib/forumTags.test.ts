// 포럼 태그 규칙: 데몬과 같은 한도(20개, 이름 1~100자, 앞뒤 공백·제어 문자 금지, ID 중복 금지)와 앱의 이름 중복 금지.
// 새 ID는 무작위 u32라 지운 태그의 ID를 다시 쓰지 않습니다.

import { describe, expect, it } from 'vitest';
import { MAX_FORUM_TAGS, cleanTag, newTagId, sameTags, tagNameError, tagsError } from './forumTags';

const tag = (id: number, name: string, emoji?: string) => (emoji ? { id, name, emoji } : { id, name });

describe('태그 이름', () => {
  it('빈 이름·앞뒤 공백·100자 넘김·제어 문자·같은 이름을 막는다', () => {
    expect(tagNameError('')).toBe('태그 이름을 넣으세요');
    expect(tagNameError('   ')).toBe('태그 이름을 넣으세요');
    expect(tagNameError(' 질문')).toBe('태그 이름 앞뒤에 공백을 둘 수 없습니다');
    expect(tagNameError('가'.repeat(100))).toBeNull();
    expect(tagNameError('가'.repeat(101))).toBe('태그 이름은 100자까지입니다');
    expect(tagNameError('줄\n바꿈')).toBe('태그 이름에 제어 문자를 쓸 수 없습니다');
    expect(tagNameError('Bug', [tag(1, 'bug')])).toBe('같은 이름의 태그가 이미 있습니다');
    expect(tagNameError('🐞 버그')).toBeNull();
  });

  it('이모지도 100자 안에 센다(코드 포인트 단위)', () => {
    expect(tagNameError('🐞'.repeat(100))).toBeNull();
    expect(tagNameError('🐞'.repeat(99) + '가나')).toBe('태그 이름은 100자까지입니다');
  });
});

describe('태그 목록', () => {
  it('20개 넘김·ID 겹침·이름 오류를 첫 오류로 알린다', () => {
    const twenty = Array.from({ length: MAX_FORUM_TAGS }, (_, i) => tag(i + 1, `t${i}`));
    expect(tagsError(twenty)).toBeNull();
    expect(tagsError([...twenty, tag(99, 'x')])).toBe('포럼 태그는 20개까지입니다');
    expect(tagsError([tag(1, 'a'), tag(1, 'b')])).toBe('태그 ID가 겹칩니다');
    expect(tagsError([tag(1, 'a'), tag(2, 'A')])).toBe('「A」: 같은 이름의 태그가 이미 있습니다');
    expect(tagsError([tag(-1, 'a')])).toBe('태그 ID가 잘못됐습니다');
  });

  it('새 ID는 쓰고 있는 ID와 0을 피한 무작위 u32', () => {
    const seq = [0, 5, 0xffffffff];
    const rand = () => seq.shift()!;
    expect(newTagId([tag(5, 'a')], rand)).toBe(0xffffffff);
    const id = newTagId([tag(1, 'a'), tag(2, 'b')]);
    expect(Number.isInteger(id) && id > 0 && id <= 0xffffffff).toBe(true);
    expect([1, 2]).not.toContain(id);
  });

  it('보낼 때 이름을 다듬고, 예전 이모지는 그대로 두며 새 태그에는 붙이지 않는다', () => {
    expect(cleanTag(tag(1, ' 질문 '))).toEqual({ id: 1, name: '질문' });
    expect(cleanTag({ id: 1, name: '질문', emoji: null })).toEqual({ id: 1, name: '질문' });
    expect(cleanTag(tag(2, '버그', '🐞'))).toEqual({ id: 2, name: '버그', emoji: '🐞' });
    expect(sameTags([tag(1, 'a')], [{ id: 1, name: 'a', emoji: null } as never])).toBe(true);
    expect(sameTags([tag(1, 'a')], [tag(1, 'b')])).toBe(false);
  });
});
