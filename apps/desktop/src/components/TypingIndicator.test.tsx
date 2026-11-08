import { describe, expect, it } from 'vitest';
import { typingText } from './TypingIndicator';

describe('입력 중 표시', () => {
  it('세 명까지는 이름을, 그보다 많으면 여러 명으로 적는다', () => {
    expect(typingText([])).toBe('');
    expect(typingText(['세영'])).toBe('세영님이 입력 중…');
    expect(typingText(['세영', '졸린 수달'])).toBe('세영님, 졸린 수달님이 입력 중…');
    expect(typingText(['a', 'b', 'c', 'd'])).toBe('여러 명이 입력 중…');
  });
});
