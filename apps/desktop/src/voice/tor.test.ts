import { afterEach, describe, expect, it, vi } from 'vitest';
import { Reorder, bytesToCodes, codesToBytes, decodeTorPacket, encodeTorPacket, pickTorCodec } from './tor';

describe('Tor 통화 패킷', () => {
  it('머리말·본문을 그대로 오간다', () => {
    const body = new Uint8Array([9, 8, 7]);
    const p = decodeTorPacket(encodeTorPacket(1, 3, 0xfffffffe, body))!;
    expect(p).toMatchObject({ kind: 1, codec: 3, seq: 0xfffffffe });
    expect([...p.body]).toEqual([9, 8, 7]);
    expect(decodeTorPacket(new Uint8Array(5))).toBeNull();
    const codes = new Uint16Array([0, 2047, 1024]);
    expect([...bytesToCodes(codesToBytes(codes))!]).toEqual([...codes]);
    expect(bytesToCodes(new Uint8Array(3))).toBeNull();
  });

  it('고른 코덱은 모든 상대가 풀 수 있을 때만, 아니면 모두가 푸는 코덱', () => {
    const MIMI = 1;
    const OPUS = 4;
    expect(pickTorCodec('mimi', ['opus', 'mimi'], [MIMI | OPUS, MIMI | OPUS])).toBe('mimi');
    expect(pickTorCodec('mimi', ['opus', 'mimi'], [MIMI | OPUS, OPUS])).toBe('opus');
    expect(pickTorCodec('mimi', ['opus'], [MIMI | OPUS])).toBe('opus');
    expect(pickTorCodec('opus', ['opus', 'mimi'], [MIMI])).toBe('mimi');
    expect(pickTorCodec('mimi', ['opus', 'mimi'], [0, MIMI | OPUS])).toBe('mimi');
    expect(pickTorCodec('mimi', ['opus'], [MIMI])).toBe('opus');
  });
});

describe('재정렬 버퍼', () => {
  afterEach(() => vi.useRealTimers());

  it('순서가 바뀐 패킷을 번호 순으로 내보내고, 지난 번호는 버린다', () => {
    const out: [number, number][] = [];
    const r = new Reorder<number>((x, lost) => out.push([x, lost]));
    r.push(10, 10);
    r.push(12, 12);
    r.push(11, 11);
    r.push(11, 11);
    r.push(9, 9);
    expect(out).toEqual([
      [10, 0],
      [11, 0],
      [12, 0],
    ]);
  });

  it('빠진 번호는 잠깐 기다렸다가 건너뛰고 빠진 수를 알린다', () => {
    vi.useFakeTimers();
    const out: [number, number][] = [];
    const r = new Reorder<number>((x, lost) => out.push([x, lost]), 200);
    r.push(1, 1);
    r.push(4, 4);
    r.push(5, 5);
    expect(out).toEqual([[1, 0]]);
    vi.advanceTimersByTime(250);
    expect(out).toEqual([
      [1, 0],
      [4, 2],
      [5, 0],
    ]);
    r.close();
  });
  it('보낸 쪽이 번호를 처음부터 다시 쓰면 다시 맞춘다', () => {
    const out: number[] = [];
    const r = new Reorder<number>((x) => out.push(x));
    r.push(5000, 5000);
    r.push(5001, 5001);
    r.push(4990, 4990);
    r.push(0, 0);
    r.push(1, 1);
    expect(out).toEqual([5000, 5001, 0, 1]);
  });

  it('많이 쌓여 건너뛴 뒤 지난 번호를 붙잡아 두지 않는다', () => {
    vi.useFakeTimers();
    const out: [number, number][] = [];
    const r = new Reorder<number>((x, lost) => out.push([x, lost]), 200);
    r.push(7, 7);
    for (let s = 10; s <= 74; s++) r.push(s, s);
    r.push(9, 9);
    r.push(76, 76);
    vi.advanceTimersByTime(1000);
    const seqs = out.map(([x]) => x);
    expect(seqs).toEqual([...seqs].sort((a, b) => a - b));
    expect(seqs).not.toContain(9);
    vi.useRealTimers();
  });
});
