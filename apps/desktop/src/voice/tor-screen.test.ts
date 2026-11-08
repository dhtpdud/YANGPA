import { describe, expect, it } from 'vitest';
import { FrameAssembler, PART_BYTES, decodePart, encodePart, splitFrame } from './tor-screen';

describe('Tor 화면 조각', () => {
  it('조각 머리말·본문을 그대로 오가고 잘못된 조각은 버린다', () => {
    const data = new Uint8Array([1, 2, 3]);
    const p = decodePart(encodePart({ frame: 7, part: 1, parts: 2, key: true, width: 1280, height: 720, data }))!;
    expect(p).toMatchObject({ frame: 7, part: 1, parts: 2, key: true, width: 1280, height: 720 });
    expect([...p.data]).toEqual([1, 2, 3]);
    expect(decodePart(new Uint8Array(11))).toBeNull();
    const bad = encodePart({ frame: 1, part: 2, parts: 2, key: false, width: 2, height: 2, data });
    expect(decodePart(bad)).toBeNull();
  });

  it('큰 프레임을 나눴다가 순서가 바뀌어 와도 다시 모은다', () => {
    const data = new Uint8Array(PART_BYTES * 2 + 5).map((_, i) => i & 0xff);
    const parts = splitFrame(3, true, 640, 360, data)!;
    expect(parts).toHaveLength(3);
    const a = new FrameAssembler();
    expect(a.push(parts[2])).toBeNull();
    expect(a.push(parts[0])).toBeNull();
    const f = a.push(parts[1])!;
    expect(f.frame).toBe(3);
    expect(f.key).toBe(true);
    expect(f.data).toEqual(data);
  });

  it('덜 모인 프레임이 바이트 상한을 넘으면 오래된 것부터 버린다', () => {
    const a = new FrameAssembler(PART_BYTES * 2);
    const one = splitFrame(1, true, 2, 2, new Uint8Array(PART_BYTES * 2 + 1))!;
    const two = splitFrame(2, true, 2, 2, new Uint8Array(PART_BYTES * 2 + 1))!;
    a.push(one[0]);
    a.push(one[1]);
    // 프레임 2의 조각이 들어오며 상한을 넘으니 프레임 1을 버립니다.
    a.push(two[0]);
    expect(a.push(one[2])).toBeNull();
    a.push(two[1]);
    expect(a.push(two[2])?.frame).toBe(2);
  });

  it('더 새 프레임이 먼저 다 모이면 그 전 프레임은 버린다', () => {
    const a = new FrameAssembler();
    const old = splitFrame(1, true, 2, 2, new Uint8Array(PART_BYTES + 1))!;
    const next = splitFrame(2, false, 2, 2, new Uint8Array(10))!;
    expect(a.push(old[0])).toBeNull();
    expect(a.push(next[0])?.frame).toBe(2);
    expect(a.push(old[1])).toBeNull();
    expect(a.push(next[0])).toBeNull();
  });
});
