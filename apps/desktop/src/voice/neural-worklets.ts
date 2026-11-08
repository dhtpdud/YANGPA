// 신경망 코덱용 AudioWorklet(24kHz 컨텍스트): 마이크를 프레임(1920샘플, 80ms)으로 모아 보내는 'yangpa-capture'와,
// 풀어 낸 소리를 지터 버퍼에 모았다가 트는 'yangpa-player'. Vite가 `?worker&url`로 따로 묶습니다.

// 모듈로 두어 아래 선언이 앱 전체 전역 타입에 섞이지 않게 합니다(실행 때는 워클릿 전역의 것을 씀).
export {};

declare class AudioWorkletProcessor {
  readonly port: MessagePort;
  constructor(options?: { processorOptions?: unknown });
}
declare function registerProcessor(name: string, ctor: new (options?: { processorOptions?: unknown }) => AudioWorkletProcessor): void;

/** 마이크를 프레임으로 모아 메인 스레드로 넘깁니다. */
class CaptureProcessor extends AudioWorkletProcessor {
  private buf: Float32Array;
  private fill = 0;

  constructor(options?: { processorOptions?: unknown }) {
    super(options);
    const frame = (options?.processorOptions as { frame?: number } | undefined)?.frame ?? 1920;
    this.buf = new Float32Array(frame);
  }

  process(inputs: Float32Array[][]): boolean {
    const ch = inputs[0]?.[0];
    if (!ch) return true;
    let i = 0;
    while (i < ch.length) {
      const n = Math.min(ch.length - i, this.buf.length - this.fill);
      this.buf.set(ch.subarray(i, i + n), this.fill);
      this.fill += n;
      i += n;
      if (this.fill === this.buf.length) {
        const out = this.buf;
        // 넘기면 원래 버퍼는 길이가 0이 되므로 길이를 먼저 잡아 둡니다.
        this.buf = new Float32Array(out.length);
        this.fill = 0;
        this.port.postMessage(out, [out.buffer]);
      }
    }
    return true;
  }
}

/**
 * 지터 버퍼 재생기. 시작·끊김 뒤에는 `target`만큼 모일 때까지 기다렸다가 틀고, `max`를 넘게 쌓이면(밀린 패킷이 한꺼번에
 * 옴) 오래된 소리를 버려 지연이 늘지 않게 합니다.
 */
class PlayerProcessor extends AudioWorkletProcessor {
  private queue: Float32Array[] = [];
  private offset = 0;
  private buffered = 0;
  private playing = false;
  private readonly target: number;
  private readonly max: number;

  constructor(options?: { processorOptions?: unknown }) {
    super(options);
    const o = (options?.processorOptions ?? {}) as { target?: number; max?: number };
    this.target = o.target ?? 2880;
    this.max = o.max ?? 14400;
    this.port.onmessage = (e: MessageEvent<Float32Array>) => {
      if (!(e.data instanceof Float32Array) || e.data.length === 0) return;
      this.queue.push(e.data);
      this.buffered += e.data.length;
      while (this.buffered > this.max && this.queue.length > 1) {
        const old = this.queue.shift()!;
        this.buffered -= old.length - this.offset;
        this.offset = 0;
      }
    };
  }

  process(_inputs: Float32Array[][], outputs: Float32Array[][]): boolean {
    const out = outputs[0]?.[0];
    if (!out) return true;
    if (!this.playing) {
      if (this.buffered < this.target) {
        out.fill(0);
        return true;
      }
      this.playing = true;
    }
    let i = 0;
    while (i < out.length) {
      const head = this.queue[0];
      if (!head) {
        // 끊김: 남은 자리는 무음으로 두고 다시 모일 때까지 기다립니다.
        out.fill(0, i);
        this.playing = false;
        break;
      }
      const n = Math.min(out.length - i, head.length - this.offset);
      if (n <= 0) {
        // 빈 조각(있어선 안 됨)에서 멈추지 않게 넘깁니다.
        this.queue.shift();
        this.offset = 0;
        continue;
      }
      out.set(head.subarray(this.offset, this.offset + n), i);
      i += n;
      this.offset += n;
      this.buffered -= n;
      if (this.offset === head.length) {
        this.queue.shift();
        this.offset = 0;
      }
    }
    for (let c = 1; c < outputs[0].length; c++) outputs[0][c].set(out);
    return true;
  }
}

registerProcessor('yangpa-capture', CaptureProcessor);
registerProcessor('yangpa-player', PlayerProcessor);
