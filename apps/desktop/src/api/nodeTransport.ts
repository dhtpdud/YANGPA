/// <reference types="node" />
// Node용 전송: 데몬의 로컬 API(유닉스 소켓·윈도 named pipe)에 직접 붙습니다.
// 앱 번들에는 들어가지 않고(웹뷰는 Tauri 전송을 씀), 실제 데몬을 띄우는 종단 간 시험과 스크립트가 씁니다.
// 연결하면 auth.hello → events.subscribe 순서로 Tauri 쪽(src-tauri/src/daemon.rs)과 같은 일을 합니다.

import { connect, type Socket } from 'node:net';
import { RpcError, type DaemonState, type PickedFile, type Transport } from './client';
import { FrameDecoder, encodeJsonFrame } from './framing';
import { ErrorCode, type Notification } from './types';

export interface NodeTransportOptions {
  /** 유닉스 소켓 경로 또는 `\\.\pipe\…` */
  endpoint: string;
  /** 소유자 토큰(`<데이터 경로>/owner.token`) 또는 발급한 API 토큰 */
  token: string;
  /** 요청 하나의 응답 상한(밀리초) */
  timeoutMs?: number;
  /** 알림 구독 주제(비우면 전부) */
  topics?: string[];
}

interface Pending {
  resolve: (v: unknown) => void;
  reject: (e: unknown) => void;
  timer: ReturnType<typeof setTimeout>;
}

export interface NodeTransport extends Transport {
  readonly kind: 'node';
  /** 연결을 닫습니다(기다리던 요청은 모두 실패). */
  close(): void;
  readonly closed: boolean;
}

function openSocket(endpoint: string): Promise<Socket> {
  return new Promise((resolve, reject) => {
    const s = connect({ path: endpoint });
    const onError = (e: Error) => {
      s.destroy();
      reject(e);
    };
    s.once('error', onError);
    s.once('connect', () => {
      s.off('error', onError);
      resolve(s);
    });
  });
}

/** 데몬에 붙고 인증·구독까지 마친 전송을 돌려줍니다. */
export async function connectNodeTransport(opts: NodeTransportOptions): Promise<NodeTransport> {
  const socket = await openSocket(opts.endpoint);
  const timeoutMs = opts.timeoutMs ?? 120_000;
  const decoder = new FrameDecoder();
  const pending = new Map<number, Pending>();
  const handlers = new Set<(batch: Notification[]) => void>();
  const stateHandlers = new Set<(s: DaemonState) => void>();
  let nextId = 1;
  let closed = false;
  let closeReason = '연결이 닫혔습니다'; // i18n-ignore: 시험·e2e 전용 Node 전송 오류

  const failAll = (reason: string) => {
    for (const [id, p] of pending) {
      clearTimeout(p.timer);
      p.reject(new RpcError(ErrorCode.DaemonUnavailable, reason));
      pending.delete(id);
    }
  };

  const shutdown = (reason: string) => {
    if (closed) return;
    closed = true;
    closeReason = reason;
    failAll(reason);
    socket.destroy();
    for (const h of stateHandlers) h({ state: 'error', message: reason });
  };

  socket.on('data', (chunk: Buffer) => {
    let frames;
    try {
      frames = decoder.push(new Uint8Array(chunk.buffer, chunk.byteOffset, chunk.length));
    } catch (e) {
      shutdown(`잘못된 프레임: ${(e as Error).message}`); // i18n-ignore: 시험·e2e 전용 Node 전송 오류
      return;
    }
    for (const f of frames) {
      if (f.kind === 'binary') {
        const p = pending.get(f.id);
        if (p) {
          pending.delete(f.id);
          clearTimeout(p.timer);
          p.resolve(f.data);
        }
        continue;
      }
      let msg: { id?: unknown; method?: unknown; params?: unknown; result?: unknown; error?: { code: number; message: string; data?: unknown } };
      try {
        msg = JSON.parse(f.text);
      } catch {
        shutdown('JSON이 아닌 응답'); // i18n-ignore: 시험·e2e 전용 Node 전송 오류
        return;
      }
      if (msg.method === 'events' && msg.id === undefined) {
        const batch = (msg.params as { batch?: Notification[] } | undefined)?.batch;
        if (Array.isArray(batch)) for (const h of handlers) h(batch);
        continue;
      }
      if (typeof msg.id !== 'number') continue;
      const p = pending.get(msg.id);
      if (!p) continue;
      pending.delete(msg.id);
      clearTimeout(p.timer);
      if (msg.error) p.reject(new RpcError(msg.error.code, msg.error.message, msg.error.data));
      else p.resolve(msg.result);
    }
  });
  socket.on('close', () => shutdown(closeReason));
  socket.on('error', (e) => shutdown(`소켓 오류: ${e.message}`)); // i18n-ignore: 시험·e2e 전용 Node 전송 오류

  const call = (method: string, params: unknown): Promise<unknown> => {
    if (closed) return Promise.reject(new RpcError(ErrorCode.DaemonUnavailable, closeReason));
    const id = nextId++;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        pending.delete(id);
        reject(new RpcError(ErrorCode.Internal, `${method} 응답 시간 초과`)); // i18n-ignore: 시험·e2e 전용 Node 전송 오류
      }, timeoutMs);
      pending.set(id, { resolve, reject, timer });
      socket.write(encodeJsonFrame({ jsonrpc: '2.0', id, method, params: params ?? {} }));
    });
  };

  try {
    await call('auth.hello', { token: opts.token });
    await call('events.subscribe', opts.topics?.length ? { topics: opts.topics } : {});
  } catch (e) {
    shutdown('인증 실패'); // i18n-ignore: 시험·e2e 전용 Node 전송 오류
    throw e;
  }

  const unsupported = (what: string) => () => {
    throw new RpcError(ErrorCode.Internal, `Node 전송은 ${what}을(를) 지원하지 않습니다`); // i18n-ignore: 시험·e2e 전용 Node 전송 오류
  };

  const t: NodeTransport = {
    kind: 'node',
    get closed() {
      return closed;
    },
    close: () => shutdown('클라이언트가 닫음'), // i18n-ignore: 시험·e2e 전용 Node 전송 오류
    call(method, params) {
      if (method === 'auth.hello' || method.startsWith('events.')) {
        return Promise.reject(new RpcError(ErrorCode.InvalidRequest, '이 메서드는 전송이 직접 관리합니다')); // i18n-ignore: 시험·e2e 전용 Node 전송 오류
      }
      return call(method, params);
    },
    subscribe(handler) {
      handlers.add(handler);
      return () => handlers.delete(handler);
    },
    onDaemonState(handler) {
      stateHandlers.add(handler);
      handler(closed ? { state: 'error', message: closeReason } : { state: 'connected' });
      return () => stateHandlers.delete(handler);
    },
    async retryDaemon() {},
    blobSrc: unsupported('블롭 주소'), // i18n-ignore: 시험·e2e 전용 Node 전송 오류
    async readBlob(space, blob, keyFrom) {
      const data = await call('blob.read', { space, blob, key_from: keyFrom, binary: true });
      if (data instanceof Uint8Array) return data;
      throw new RpcError(ErrorCode.Internal, '바이너리 응답이 아닙니다'); // i18n-ignore: 시험·e2e 전용 Node 전송 오류
    },
    async pickFiles(): Promise<PickedFile[]> {
      return [];
    },
    async saveDialog() {
      return null;
    },
    async setContentProtection() {
      return false;
    },
    async openExternal() {},
    async loadPrefs() {
      return {};
    },
    async savePrefs() {},
    onFileDrop() {
      return () => undefined;
    },
  };
  return t;
}
