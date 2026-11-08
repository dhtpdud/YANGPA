import { describe, expect, it } from 'vitest';
import { addSample, chunkRate, type RateSample } from './rate';

const build = (pairs: [number, number][]) => pairs.reduce<RateSample[]>((s, [at, have]) => addSample(s, at, have), []);

describe('받기 속도 어림', () => {
  it('같은 조각 수는 무시하고, 줄었거나 오래 끊겼으면 새로 시작한다', () => {
    const s = build([[0, 1], [1000, 2]]);
    expect(addSample(s, 1500, 2)).toBe(s);
    expect(addSample(s, 2000, 1)).toEqual([{ at: 2000, have: 1 }]);
    expect(addSample(s, 70_000, 3)).toEqual([{ at: 70_000, have: 3 }]);
  });

  it('최근 20초·16개로 줄이되 표본 둘은 남긴다', () => {
    const many = build(Array.from({ length: 30 }, (_, i) => [i * 100, i] as [number, number]));
    expect(many.length).toBe(16);
    expect(many.at(-1)).toEqual({ at: 2900, have: 29 });
    const slow = build([[0, 0], [30_000, 1]]);
    expect(slow.length).toBe(2);
  });

  it('고르게 오면 초당 조각 수를 낸다', () => {
    const s = build([[0, 0], [1000, 1], [2000, 2]]);
    expect(chunkRate(s, 2000)).toBe(1);
  });

  it('다음 조각이 늦어지면 숫자가 내려가고, 오래 멈추면 숨긴다', () => {
    const s = build([[0, 0], [1000, 1], [2000, 2]]);
    expect(chunkRate(s, 5000)).toBeCloseTo(0.4);
    expect(chunkRate(s, 2000 + 16_000)).toBeNull();
  });

  it('표본이 하나뿐이면 null, Tor처럼 아주 느려도 숫자를 낸다', () => {
    expect(chunkRate(build([[0, 0]]), 0)).toBeNull();
    expect(chunkRate(build([[0, 0], [17_000, 1]]), 17_000)).toBeCloseTo(1 / 17);
  });
});
