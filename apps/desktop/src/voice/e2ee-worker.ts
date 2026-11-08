// 프레임 암호화 워커. RTCRtpScriptTransform(rtctransform 이벤트)과, 그것이 없는 브라우저의
// createEncodedStreams(메인 스레드가 스트림을 넘겨줌) 두 경로를 모두 처리합니다.
// 키는 이 워커 메모리에만 두고, 받은 원본 키 바이트는 가져온 뒤 0으로 덮어씁니다.

import {
  IvSequence,
  KeyRing,
  clearPrefixLength,
  decryptFrame,
  encryptFrame,
  senderId,
  type MediaKind,
  type VideoFrameType,
} from './frame';

/** 변환 하나의 설정. `sender`는 프레임을 보낸 기기(암호화면 나, 복호화면 그 트랙의 출처)로 AAD에 묶입니다. */
export interface TransformOptions {
  operation: 'encode' | 'decode';
  kind: MediaKind;
  sender: string;
}

export type WorkerInMessage =
  | { type: 'setKey'; generation: number; key: ArrayBuffer }
  | { type: 'clearKeys' }
  /** 데이터 채널 패킷(신경망 코덱) 봉인·열기. `sender`는 보낸 기기(봉인이면 나). 결과는 같은 `id`의 'data'. */
  | { type: 'seal' | 'open'; id: number; sender: string; data: ArrayBuffer }
  | ({
      type: 'stream';
      readable: ReadableStream;
      writable: WritableStream;
    } & TransformOptions);

/** 봉인·열기 결과. 실패하면(키 없음, 인증 실패) data가 null. */
export interface WorkerData {
  type: 'data';
  id: number;
  data: ArrayBuffer | null;
}

/** 데이터 채널 패킷의 평문 첫 바이트(AAD에 묶임): 같은 키로 암호화한 음성·화면 프레임과 섞이지 않게. */
export const DATA_DOMAIN = 0xd7;

export interface WorkerStats {
  type: 'stats';
  encrypted: number;
  decrypted: number;
  dropped: number;
  /** 보낸 기기별로 푼 것(ok)과 풀지 못한 것(bad): 프레임과 봉인 패킷(Tor 통화·신경망 코덱) 모두. 상대 소리가 어디서 끊기는지 보입니다. */
  rx: Record<string, { ok: number; bad: number }>;
}

interface EncodedFrameLike {
  data: ArrayBuffer;
  type?: VideoFrameType;
}

interface TransformerLike {
  readable: ReadableStream;
  writable: WritableStream;
  options?: Partial<TransformOptions>;
}

interface WorkerScope {
  onmessage: ((e: MessageEvent<WorkerInMessage>) => void) | null;
  onrtctransform?: ((e: { transformer: TransformerLike }) => void) | null;
  postMessage(msg: WorkerStats | WorkerData, transfer?: Transferable[]): void;
}

const scope = globalThis as unknown as WorkerScope;
const ring = new KeyRing();
const ivs = new IvSequence();
const stats: WorkerStats = { type: 'stats', encrypted: 0, decrypted: 0, dropped: 0, rx: {} };
/** 키 설정을 받은 순서대로(파생이 비동기라 나중에 온 키가 먼저 끝나 앞의 키로 되돌아가지 않게). */
let keyChain: Promise<void> = Promise.resolve();

function countRx(sender: string, ok: boolean) {
  const r = (stats.rx[sender] ??= { ok: 0, bad: 0 });
  if (ok) r.ok++;
  else r.bad++;
}

function transformer({ operation, kind, sender }: TransformOptions) {
  const from = senderId(sender);
  return new TransformStream<EncodedFrameLike, EncodedFrameLike>({
    async transform(frame, controller) {
      const data = new Uint8Array(frame.data);
      const clearLen = clearPrefixLength(kind, frame.type);
      if (operation === 'encode') {
        const cur = ring.current();
        // 키가 아직 없으면 평문을 내보내지 않고 버립니다.
        if (!cur) {
          stats.dropped++;
          return;
        }
        const out = await encryptFrame(data, clearLen, cur.key, cur.generation, ivs.next(), from);
        frame.data = out.buffer as ArrayBuffer;
        stats.encrypted++;
        controller.enqueue(frame);
      } else {
        const out = await decryptFrame(data, clearLen, ring, from);
        countRx(sender, !!out);
        if (!out) {
          stats.dropped++;
          return;
        }
        frame.data = out.buffer as ArrayBuffer;
        stats.decrypted++;
        controller.enqueue(frame);
      }
    },
  });
}

async function sealOrOpen(op: 'seal' | 'open', id: number, sender: string, data: Uint8Array) {
  let out: Uint8Array | null = null;
  try {
    if (op === 'seal') {
      const cur = ring.current();
      if (cur) {
        const plain = new Uint8Array(data.length + 1);
        plain[0] = DATA_DOMAIN;
        plain.set(data, 1);
        out = await encryptFrame(plain, 1, cur.key, cur.generation, ivs.next(), senderId(sender));
      }
    } else {
      const plain = data[0] === DATA_DOMAIN ? await decryptFrame(data, 1, ring, senderId(sender)) : null;
      out = plain ? plain.slice(1) : null;
      countRx(sender, !!out);
    }
  } catch {
    out = null;
  }
  const buf = out ? (out.buffer.slice(out.byteOffset, out.byteOffset + out.byteLength) as ArrayBuffer) : null;
  scope.postMessage({ type: 'data', id, data: buf }, buf ? [buf] : []);
}

function pipe(readable: ReadableStream, writable: WritableStream, options: TransformOptions) {
  readable
    .pipeThrough(transformer(options))
    .pipeTo(writable)
    .catch(() => {
      // 통화 종료·트랙 교체로 스트림이 닫히면 여기로 옵니다.
    });
}

scope.onmessage = (e) => {
  const msg = e.data;
  switch (msg.type) {
    case 'setKey': {
      const raw = new Uint8Array(msg.key);
      // 잘못된 키(길이 등)는 버립니다. 키가 없으면 송신 프레임도 버리므로 평문이 나가지 않습니다.
      keyChain = keyChain.then(() =>
        ring
          .setKey(msg.generation, raw)
          .catch(() => undefined)
          .finally(() => raw.fill(0)),
      );
      break;
    }
    case 'clearKeys':
      keyChain = keyChain.then(() => ring.clear());
      break;
    case 'seal':
    case 'open':
      void sealOrOpen(msg.type, msg.id, msg.sender, new Uint8Array(msg.data));
      break;
    case 'stream':
      pipe(msg.readable, msg.writable, { operation: msg.operation, kind: msg.kind, sender: msg.sender });
      break;
  }
};

scope.onrtctransform = (e) => {
  const t = e.transformer;
  // 보낸 기기가 빠졌으면 빈 이름으로 묶어 어떤 프레임도 통과하지 않게 합니다.
  pipe(t.readable, t.writable, {
    operation: t.options?.operation ?? 'decode',
    kind: t.options?.kind ?? 'audio',
    sender: t.options?.sender ?? '',
  });
};

setInterval(() => scope.postMessage({ ...stats }), 2000);
