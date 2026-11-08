// 송·수신기에 프레임 암호화 변환을 붙입니다.
// createEncodedStreams(연결을 encodedInsertableStreams로 만듦)가 있으면 그것을, 없으면 RTCRtpScriptTransform을 씁니다.
// 크로뮴(WebView2 15x)의 RTCRtpScriptTransform은 협상 도중에 단 변환을 때때로 적용하지 않아(워커에는 붙었다고 오는데
// 프레임이 지나가지 않음) 암호문이 그대로 디코더에 들어가거나 평문이 나갑니다('삐익' 소리·깨진 화면). encodedInsertableStreams
// 연결은 모든 송·수신기 프레임이 스크립트를 거치고, 스트림을 받지 않은 프레임은 내보내지 않으므로 평문이 새지 않습니다.

import type { MediaKind } from './frame';
import type { TransformOptions, WorkerInMessage } from './e2ee-worker';

interface EncodedStreams {
  readable: ReadableStream;
  writable: WritableStream;
}

type WithEncodedStreams = { createEncodedStreams?: () => EncodedStreams };

export type TransformMode = 'script-transform' | 'encoded-streams' | 'none';

export function detectTransformMode(): TransformMode {
  const proto = (globalThis.RTCRtpSender?.prototype ?? {}) as WithEncodedStreams;
  if (typeof proto.createEncodedStreams === 'function') return 'encoded-streams';
  if (typeof globalThis.RTCRtpScriptTransform === 'function') return 'script-transform';
  return 'none';
}

/** 프레임 암호화 워커를 만듭니다(Vite가 별도 청크로 묶음). */
export function createE2eeWorker(): Worker {
  return new Worker(new URL('./e2ee-worker.ts', import.meta.url), { type: 'module', name: 'yangpa-e2ee' });
}

export function postKey(worker: Worker, generation: number, key: Uint8Array) {
  // 복사본을 넘기고(전송 후 원본 버퍼는 분리됨) 호출한 쪽 사본은 지웁니다.
  const copy = key.slice();
  const msg: WorkerInMessage = { type: 'setKey', generation, key: copy.buffer };
  worker.postMessage(msg, [copy.buffer]);
  key.fill(0);
}

/** 송신기에 암호화를 붙입니다. `me`는 내 기기 ID(프레임 AAD에 묶임). */
export function attachSender(
  worker: Worker,
  mode: TransformMode,
  sender: RTCRtpSender,
  kind: MediaKind,
  me: string,
): boolean {
  const options: TransformOptions = { operation: 'encode', kind, sender: me };
  if (mode === 'script-transform') {
    sender.transform = new RTCRtpScriptTransform(worker, options);
    return true;
  }
  if (mode === 'encoded-streams') {
    const s = (sender as unknown as WithEncodedStreams).createEncodedStreams?.();
    if (!s) return false;
    const msg: WorkerInMessage = { type: 'stream', ...options, readable: s.readable, writable: s.writable };
    worker.postMessage(msg, [s.readable as unknown as Transferable, s.writable as unknown as Transferable]);
    return true;
  }
  return false;
}

/** 수신기에 복호화를 붙입니다. `origin`은 이 트랙을 보냈다고 알려진 기기 ID로, 다른 기기가 암호화한 프레임은 버립니다. */
export function attachReceiver(
  worker: Worker,
  mode: TransformMode,
  receiver: RTCRtpReceiver,
  kind: MediaKind,
  origin: string,
): boolean {
  const options: TransformOptions = { operation: 'decode', kind, sender: origin };
  if (mode === 'script-transform') {
    receiver.transform = new RTCRtpScriptTransform(worker, options);
    return true;
  }
  if (mode === 'encoded-streams') {
    const s = (receiver as unknown as WithEncodedStreams).createEncodedStreams?.();
    if (!s) return false;
    const msg: WorkerInMessage = { type: 'stream', ...options, readable: s.readable, writable: s.writable };
    worker.postMessage(msg, [s.readable as unknown as Transferable, s.writable as unknown as Transferable]);
    return true;
  }
  return false;
}
