// 로컬 API 프레이밍(u32 BE 길이 | u8 종류 | 본문). 데몬(crates/yangpa-rpc/src/framing.rs)과 같은 바이트여야 합니다.

import { describe, expect, it } from 'vitest';
import { FrameDecoder, FrameError, KIND_BINARY, KIND_JSON, encodeBinaryFrame, encodeJsonFrame } from './framing';

describe('프레이밍', () => {
  it('JSON 프레임의 바이트 배치', () => {
    const f = encodeJsonFrame({ a: 1 });
    // 본문 `{"a":1}` 7바이트 + 종류 1바이트 = 길이 8
    expect([...f.slice(0, 5)]).toEqual([0, 0, 0, 8, KIND_JSON]);
    expect(new TextDecoder().decode(f.slice(5))).toBe('{"a":1}');
  });

  it('바이너리 프레임은 u64 BE 요청 ID 뒤에 바이트', () => {
    const f = encodeBinaryFrame(258, new Uint8Array([9, 8]));
    expect([...f]).toEqual([0, 0, 0, 11, KIND_BINARY, 0, 0, 0, 0, 0, 0, 1, 2, 9, 8]);
  });

  it('한 바이트씩 들어와도, 여러 프레임이 한꺼번에 와도 순서대로 꺼낸다', () => {
    const bytes = new Uint8Array([...encodeJsonFrame({ x: '한글' }), ...encodeBinaryFrame(7, new Uint8Array([1, 2, 3])), ...encodeJsonFrame([1])]);
    const d = new FrameDecoder();
    const got = [];
    for (const b of bytes) got.push(...d.push(new Uint8Array([b])));
    expect(got).toEqual([
      { kind: 'json', text: '{"x":"한글"}' },
      { kind: 'binary', id: 7, data: new Uint8Array([1, 2, 3]) },
      { kind: 'json', text: '[1]' },
    ]);
    expect(d.pending).toBe(0);
    const all = new FrameDecoder().push(bytes);
    expect(all).toEqual(got);
  });

  it('빈 프레임·모르는 종류·너무 큰 길이·깨진 UTF-8은 오류(연결을 끊어야 함)', () => {
    expect(() => new FrameDecoder().push(new Uint8Array([0, 0, 0, 0]))).toThrow(FrameError);
    expect(() => new FrameDecoder().push(new Uint8Array([0, 0, 0, 1, 9]))).toThrow(FrameError);
    expect(() => new FrameDecoder(16).push(new Uint8Array([0, 0, 0, 17]))).toThrow(FrameError);
    expect(() => new FrameDecoder().push(new Uint8Array([0, 0, 0, 2, KIND_JSON, 0xff]))).toThrow();
    expect(() => new FrameDecoder().push(new Uint8Array([0, 0, 0, 3, KIND_BINARY, 0, 0]))).toThrow(FrameError);
  });

  it('보내는 쪽도 최대 크기를 넘기지 않는다', () => {
    expect(() => encodeBinaryFrame(1, new Uint8Array(16 * 1024 * 1024))).toThrow(FrameError);
  });
});
