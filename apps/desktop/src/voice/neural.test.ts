// 신경망 코덱 데이터 채널 패킷: 모양, 빠진 패킷 세기, hello 코덱 비트.

import { describe, expect, it } from 'vitest';
import { codecMask, decodePacket, encodePacket, lostBetween, PACKET_CODES } from './neural';

describe('신경망 코덱 패킷', () => {
  it('종류·코덱·번호·코드를 담고 되읽는다', () => {
    const codes = Uint16Array.from({ length: PACKET_CODES.mimi }, (_, i) => i * 300);
    const p = decodePacket(encodePacket(1, 1, 0xfffffffe, codes))!;
    expect(p.kind).toBe(1);
    expect(p.codec).toBe(1);
    expect(p.seq).toBe(0xfffffffe);
    expect([...p.codes]).toEqual([...codes]);
    expect(decodePacket(new Uint8Array(5))).toBeNull();
    expect(decodePacket(new Uint8Array(7))).toBeNull();
  });

  it('hello는 풀 수 있는 코덱을 비트로 알린다', () => {
    expect(codecMask([])).toBe(0);
    expect(codecMask(['mimi'])).toBe(1);
    expect(codecMask(['mimi', 'dac'])).toBe(3);
  });

  it('빠진 패킷 수: 처음은 0, 이어지면 0, 건너뛰면 그만큼, 늦거나 같은 번호는 버림, 크게 튀면 새로 시작', () => {
    expect(lostBetween(undefined, 5)).toBe(0);
    expect(lostBetween(5, 6)).toBe(0);
    expect(lostBetween(5, 9)).toBe(3);
    expect(lostBetween(9, 9)).toBeNull();
    expect(lostBetween(9, 7)).toBeNull();
    expect(lostBetween(0xffffffff, 0)).toBe(0);
    expect(lostBetween(1, 5000)).toBe(0);
  });
});
