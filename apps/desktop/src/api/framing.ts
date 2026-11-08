// 로컬 API 프레이밍(docs/api.md "전송", crates/yangpa-rpc/src/framing.rs): `u32 BE 길이 | u8 종류 | 본문`.
// 종류 1 = JSON-RPC 메시지(UTF-8), 종류 2 = 바이너리(`u64 BE 요청 ID | 바이트`). 길이는 종류 1바이트 + 본문.
// 브라우저·Node 어디서나 쓰도록 Uint8Array만 다룹니다(Node 전송과 시험이 씀).

export const KIND_JSON = 1;
export const KIND_BINARY = 2;
/** 프레임 최대 크기(길이 필드가 가리키는 바이트 수). 넘으면 상대가 연결을 끊습니다. */
export const MAX_FRAME = 16 * 1024 * 1024;
const LEN_BYTES = 4;
const BIN_ID_BYTES = 8;

export type Frame = { kind: 'json'; text: string } | { kind: 'binary'; id: number; data: Uint8Array };

export class FrameError extends Error {
  constructor(message: string) {
    super(message);
    this.name = 'FrameError';
  }
}

const encoder = new TextEncoder();
const decoder = new TextDecoder('utf-8', { fatal: true });

/** JSON 값 하나를 프레임으로. */
export function encodeJsonFrame(value: unknown): Uint8Array {
  const body = encoder.encode(JSON.stringify(value));
  const len = 1 + body.length;
  if (len > MAX_FRAME) throw new FrameError(`프레임이 너무 큽니다: ${len}바이트`); // i18n-ignore: 시험·e2e 전용 전송 오류
  const out = new Uint8Array(LEN_BYTES + len);
  new DataView(out.buffer).setUint32(0, len, false);
  out[LEN_BYTES] = KIND_JSON;
  out.set(body, LEN_BYTES + 1);
  return out;
}

/** 바이너리 프레임(서버만 보내지만 시험용으로 둡니다). */
export function encodeBinaryFrame(id: number, data: Uint8Array): Uint8Array {
  const len = 1 + BIN_ID_BYTES + data.length;
  if (len > MAX_FRAME) throw new FrameError(`프레임이 너무 큽니다: ${len}바이트`); // i18n-ignore: 시험·e2e 전용 전송 오류
  const out = new Uint8Array(LEN_BYTES + len);
  const dv = new DataView(out.buffer);
  dv.setUint32(0, len, false);
  out[LEN_BYTES] = KIND_BINARY;
  dv.setBigUint64(LEN_BYTES + 1, BigInt(id), false);
  out.set(data, LEN_BYTES + 1 + BIN_ID_BYTES);
  return out;
}

/** 조각난 바이트 흐름에서 프레임을 꺼냅니다. 받은 만큼만 버퍼에 들고 있습니다. */
export class FrameDecoder {
  private buf = new Uint8Array(0);
  private readonly maxFrame: number;

  constructor(maxFrame = MAX_FRAME) {
    this.maxFrame = maxFrame;
  }

  /** 새 바이트를 넣고 완성된 프레임을 돌려줍니다. 형식이 틀리면 FrameError(연결을 끊어야 함). */
  push(chunk: Uint8Array): Frame[] {
    if (chunk.length) {
      const next = new Uint8Array(this.buf.length + chunk.length);
      next.set(this.buf, 0);
      next.set(chunk, this.buf.length);
      this.buf = next;
    }
    const out: Frame[] = [];
    let off = 0;
    for (;;) {
      if (this.buf.length - off < LEN_BYTES) break;
      const dv = new DataView(this.buf.buffer, this.buf.byteOffset + off);
      const len = dv.getUint32(0, false);
      if (len === 0) throw new FrameError('빈 프레임'); // i18n-ignore: 시험·e2e 전용 전송 오류
      if (len > this.maxFrame) throw new FrameError(`프레임이 너무 큽니다: ${len}바이트`); // i18n-ignore: 시험·e2e 전용 전송 오류
      if (this.buf.length - off < LEN_BYTES + len) break;
      const kind = this.buf[off + LEN_BYTES];
      const body = this.buf.subarray(off + LEN_BYTES + 1, off + LEN_BYTES + len);
      if (kind === KIND_JSON) {
        out.push({ kind: 'json', text: decoder.decode(body) });
      } else if (kind === KIND_BINARY) {
        if (body.length < BIN_ID_BYTES) throw new FrameError('바이너리 프레임이 너무 짧습니다'); // i18n-ignore: 시험·e2e 전용 전송 오류
        const id = new DataView(body.buffer, body.byteOffset).getBigUint64(0, false);
        // 복사해 두어야 버퍼를 줄여도 내용이 바뀌지 않습니다.
        out.push({ kind: 'binary', id: Number(id), data: body.slice(BIN_ID_BYTES) });
      } else {
        throw new FrameError(`알 수 없는 프레임 종류: ${kind}`); // i18n-ignore: 시험·e2e 전용 전송 오류
      }
      off += LEN_BYTES + len;
    }
    if (off > 0) this.buf = this.buf.slice(off);
    return out;
  }

  /** 아직 프레임이 되지 못한 바이트 수. */
  get pending(): number {
    return this.buf.length;
  }
}
