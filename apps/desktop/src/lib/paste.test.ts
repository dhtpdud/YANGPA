import { describe, expect, it, vi } from 'vitest';
import type { ApiClient } from '../api/client';
import { PASTE_PIECE, clipboardFiles, pastedName, uploadPaste } from './paste';

describe('클립보드 첨부', () => {
  it('스크린샷처럼 이름이 없으면 시각으로 이름을 짓는다', () => {
    const now = new Date(2026, 9, 24, 9, 5, 7);
    expect(pastedName({ name: 'image.png', type: 'image/png' }, now)).toBe('붙여넣기-20261024-090507.png');
    expect(pastedName({ name: '', type: 'image/jpeg' }, now)).toBe('붙여넣기-20261024-090507.jpg');
    expect(pastedName({ name: '보고서.pdf', type: 'application/pdf' }, now)).toBe('보고서.pdf');
  });

  it('글자가 함께 있으면 파일을 붙이지 않는다', () => {
    const file = new File(['x'], 'a.png');
    const data = (text: string) => ({ files: [file] as unknown as FileList, getData: () => text });
    expect(clipboardFiles(data(''))).toEqual([file]);
    expect(clipboardFiles(data('글'))).toEqual([]);
    expect(clipboardFiles(null)).toEqual([]);
  });

  it('조각으로 나눠 올리고 같은 ID로 이어 붙인다', async () => {
    const calls: Array<Record<string, unknown>> = [];
    const call = vi.fn(async (_m: string, p: Record<string, unknown>) => {
      calls.push(p);
      return { paste: 'P1', received: 0 };
    });
    const client = { call } as unknown as ApiClient;
    const size = PASTE_PIECE * 2 + 10;
    const picked = await uploadPaste(client, new Blob([new Uint8Array(size)]), 'a.bin');
    expect(picked).toEqual({ path: 'paste:P1', name: 'a.bin', size, paste: 'P1' });
    expect(calls.map((c) => [c.paste, c.size])).toEqual([
      [undefined, size],
      ['P1', undefined],
      ['P1', undefined],
    ]);
    await expect(uploadPaste(client, new Blob([]), 'e')).rejects.toThrow('빈 파일');
  });
});
