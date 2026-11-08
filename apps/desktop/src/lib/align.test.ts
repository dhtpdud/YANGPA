import { describe, expect, it } from 'vitest';
import { applyAlign } from './align';

/** `|`로 커서(하나) 또는 고른 범위(둘)를 표시한 글. */
function run(marked: string, align: 'left' | 'center' | 'right'): string {
  const a = marked.indexOf('|');
  const b = marked.indexOf('|', a + 1);
  const end = b < 0 ? a : b - 1;
  const text = marked.replace(/\|/g, '');
  const r = applyAlign(text, a, end, align);
  return r.selStart === r.selEnd ? r.text.slice(0, r.selStart) + '|' + r.text.slice(r.selStart) : r.text.slice(0, r.selStart) + '|' + r.text.slice(r.selStart, r.selEnd) + '|' + r.text.slice(r.selEnd);
}

describe('정렬 단추', () => {
  it('커서가 있는 줄만 감싼다', () => {
    expect(run('a\nb|c\nd', 'center')).toBe('a\n:::center\nb|c\n:::\nd');
    expect(run('|', 'right')).toBe(':::right\n|\n:::');
  });

  it('고른 여러 줄을 한 묶음으로, 다음 줄 맨 앞까지 고른 것은 그 줄을 빼고', () => {
    expect(run('a\n|b\nc\n|d', 'right')).toBe('a\n:::right\n|b\nc|\n:::\nd');
    expect(run('|a\nb|', 'center')).toBe(':::center\n|a\nb|\n:::');
  });

  it('묶음 안이면 정렬만 바꾸고, 왼쪽은 묶음을 걷어 낸다', () => {
    expect(run(':::center\nab|c\n:::', 'right')).toBe(':::right\nab|c\n:::');
    expect(run('x\n:::center\nab|c\nd\n:::\ny', 'left')).toBe('x\nab|c\nd\ny');
    expect(run(':::right\nabc|\n:::', 'left')).toBe('abc|');
    // 묶음 밖에서 왼쪽은 그대로.
    expect(run('ab|c', 'left')).toBe('ab|c');
  });

  it('코드 블록 안의 ::: 는 묶음으로 치지 않는다', () => {
    expect(run('```\n:::center\n```\na|', 'center')).toBe('```\n:::center\n```\n:::center\na|\n:::');
  });
});
