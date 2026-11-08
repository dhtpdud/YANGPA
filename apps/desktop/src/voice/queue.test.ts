import { describe, expect, it } from 'vitest';
import { CandidateBuffer, SerialQueue } from './queue';

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

describe('SerialQueue', () => {
  it('늦게 끝나는 작업이 있어도 넣은 순서대로 끝낸다', async () => {
    const q = new SerialQueue();
    const done: number[] = [];
    await Promise.all([
      q.run(async () => {
        await sleep(20);
        done.push(1);
      }),
      q.run(async () => {
        done.push(2);
      }),
      q.run(async () => {
        await sleep(5);
        done.push(3);
      }),
    ]);
    expect(done).toEqual([1, 2, 3]);
  });

  it('앞 작업이 실패해도 다음 작업은 돌고, 실패는 그 호출에만 돌아간다', async () => {
    const q = new SerialQueue();
    const failed = q.run(async () => {
      throw new Error('실패');
    });
    const ok = q.run(async () => 7);
    await expect(failed).rejects.toThrow('실패');
    await expect(ok).resolves.toBe(7);
  });
});

describe('CandidateBuffer', () => {
  it('기기별로 모았다가 꺼내면 비운다', () => {
    const b = new CandidateBuffer<string>();
    b.push('a', 'c1');
    b.push('a', 'c2');
    b.push('b', 'x');
    expect(b.take('a')).toEqual(['c1', 'c2']);
    expect(b.take('a')).toEqual([]);
    expect(b.size('b')).toBe(1);
    b.delete('b');
    expect(b.size('b')).toBe(0);
  });

  it('상대가 보낸 후보를 무한히 쌓지 않는다', () => {
    const b = new CandidateBuffer<number>(3, 2);
    expect([1, 2, 3, 4].map((n) => b.push('a', n))).toEqual([true, true, true, false]);
    expect(b.push('b', 1)).toBe(true);
    expect(b.push('c', 1)).toBe(false);
    b.clear();
    expect(b.push('c', 1)).toBe(true);
  });
});
