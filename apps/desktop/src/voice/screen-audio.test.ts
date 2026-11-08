import { describe, expect, it } from 'vitest';
import { applyGain } from './screen-audio';

const bytes = (...v: number[]) => new Uint8Array(new Float32Array(v).buffer);
const floats = (b: Uint8Array) => [...new Float32Array(b.buffer)];

describe('화면 소리 음량', () => {
  it('음량을 곱하고 넘친 값은 ±1로 자른다', () => {
    const b = bytes(0.25, -0.25, 0.75, -0.75);
    applyGain(b, 2);
    expect(floats(b)).toEqual([0.5, -0.5, 1, -1]);
    const q = bytes(0.5, -1);
    applyGain(q, 0);
    expect(floats(q).map(Math.abs)).toEqual([0, 0]);
  });

  it('100%면 건드리지 않는다', () => {
    const b = bytes(1.5);
    applyGain(b, 1);
    expect(floats(b)).toEqual([1.5]);
  });
});
