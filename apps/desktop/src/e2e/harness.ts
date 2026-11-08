/// <reference types="node" />
// 실제 데몬(yangpad) 여러 개를 임시 데이터 경로에 띄워 앱의 API 클라이언트로 조작하는 시험 도구.
// - 127.0.0.1 TCP에서만 듣고 mDNS를 끕니다. 먼저 띄운 데몬의 주소를 시작 옵션 `--bootstrap`으로 넘겨 서로 잇습니다
//   (고정 포트 없음, 실행 중에 상시 노드를 더하는 API는 없음).
// - 주기 따라잡기를 짧게(2초) 둡니다. 시험망에는 DHT 위치 기록이 없어 직접 전파가 닿지 않는 쌍이 있기 때문입니다.
// - 기다릴 때는 고정 sleep 대신 조건이 될 때까지 짧게 다시 보는 eventually를 씁니다.
// - 데몬 로그는 실패했을 때만 보여 줍니다(데몬 로그에는 메시지 내용·키가 없음).

import { spawn, type ChildProcess } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { ApiClient, RpcError } from '../api/client';
import { connectNodeTransport, type NodeTransport } from '../api/nodeTransport';
import type { Notification } from '../api/types';

/** 조건 하나를 기다리는 기본 최대 시간(느린 CI를 감안해 넉넉히). */
export const TIMEOUT_MS = 120_000;
const POLL_MS = 100;

const EXE = process.platform === 'win32' ? 'yangpad.exe' : 'yangpad';

/**
 * yangpad 실행 파일 찾기: `YANGPA_YANGPAD` → `CARGO_TARGET_DIR` → 작업 공간 target(debug, release).
 * 없으면 null(시험을 건너뜀).
 */
export function findYangpad(): string | null {
  const explicit = process.env.YANGPA_YANGPAD;
  if (explicit) return existsSync(explicit) ? explicit : null;
  const root = fileURLToPath(new URL('../../../../', import.meta.url));
  const dirs = [process.env.CARGO_TARGET_DIR, join(root, 'target')].filter((d): d is string => !!d);
  for (const d of dirs) {
    for (const profile of ['debug', 'release']) {
      const p = join(d, profile, EXE);
      if (existsSync(p)) return p;
    }
  }
  return null;
}

/** 조건이 값을 낼 때까지 기다립니다. 조건 안의 RpcError는 "아직"으로 봅니다(마지막 오류를 시간 초과 메시지에 붙임). */
export async function eventually<T>(what: string, f: () => Promise<T | undefined | null | false>, timeoutMs = TIMEOUT_MS): Promise<T> {
  const deadline = Date.now() + timeoutMs;
  let last: unknown;
  for (;;) {
    try {
      const v = await f();
      if (v !== undefined && v !== null && v !== false) return v;
    } catch (e) {
      if (!(e instanceof RpcError)) throw e;
      last = e;
    }
    if (Date.now() >= deadline) {
      throw new Error(`시간 초과: ${what}${last ? ` (마지막 오류: ${(last as Error).message})` : ''}`);
    }
    await new Promise((r) => setTimeout(r, POLL_MS));
  }
}

export class Daemon {
  readonly name: string;
  readonly dir: string;
  readonly endpoint: string;
  readonly events: Notification[] = [];
  private proc: ChildProcess;
  private logs: string[] = [];
  private exited: number | null = null;
  private transport: NodeTransport | null = null;
  client!: ApiClient;

  private constructor(name: string, dir: string, endpoint: string, proc: ChildProcess) {
    this.name = name;
    this.dir = dir;
    this.endpoint = endpoint;
    this.proc = proc;
    const keep = (chunk: Buffer) => {
      for (const line of chunk.toString('utf8').split('\n')) if (line.trim()) this.logs.push(line);
      if (this.logs.length > 400) this.logs.splice(0, this.logs.length - 400);
    };
    proc.stdout?.on('data', keep);
    proc.stderr?.on('data', keep);
    proc.on('exit', (code, signal) => {
      this.exited = code ?? (signal ? -1 : 0);
    });
  }

  /** 데몬을 띄우고 인증·구독까지 마칩니다. `bootstrap`은 시작하자마자 연결할 다른 데몬의 주소입니다. */
  static async start(bin: string, root: string, name: string, bootstrap: string[] = []): Promise<Daemon> {
    const dir = join(root, name);
    mkdirSync(dir, { recursive: true });
    // 윈도는 파일 경로 소켓 대신 named pipe(시험마다 다른 이름).
    const endpoint = process.platform === 'win32' ? `\\\\.\\pipe\\yangpa-e2e-${randomBytes(8).toString('hex')}` : join(dir, 'yangpad.sock');
    // 바깥 환경의 데몬 설정이 끼어들지 않게 지웁니다.
    const env = { ...process.env };
    for (const k of ['YANGPA_LOG_FILE', 'YANGPA_ENDPOINT', 'YANGPA_DATA_DIR', 'YANGPA_TOKEN', 'YANGPA_DORMANT_AFTER']) delete env[k];
    const proc = spawn(
      bin,
      [
        '--data-dir',
        dir,
        '--endpoint',
        endpoint,
        '--listen',
        '/ip4/127.0.0.1/tcp/0',
        '--no-mdns',
        // 시험은 같은 기계의 직접 연결 노드끼리만 연결합니다(Tor·인터넷에 나가지 않게). P2P 쪽 스웜도 루프백으로 열어
        // STUN 서버·공유기(UPnP)에 묻지 않습니다.
        '--direct',
        '--p2p-listen',
        '/ip4/127.0.0.1/tcp/0',
        '--key-storage',
        'file',
        '--device-name',
        name,
        // 127.0.0.1에서는 Kademlia가 서버가 되지 못해 위치 기록(DHT)으로 서로를 찾지 못합니다. 가입 전에 맺은 연결은
        // 인증되지 않은 채 남으므로, 다른 멤버의 이벤트는 인증된 피어(초대해 준 노드)와의 주기 따라잡기로 받습니다.
        '--sync-interval',
        '2s',
        // 실패했을 때 원인이 보이게 친구 요청 경로와 연결 실패는 자세히 남깁니다(내용·키는 로그에 없음).
        '--log',
        'warn,yangpa_daemon=info,yangpa_daemon::friends=debug,yangpa_net::task=debug',
        ...bootstrap.flatMap((a) => ['--bootstrap', a]),
      ],
      { stdio: ['ignore', 'pipe', 'pipe'], env },
    );
    const d = new Daemon(name, dir, endpoint, proc);
    try {
      const tokenPath = join(dir, 'owner.token');
      d.transport = await eventually(
        `${name} 데몬 시작`,
        async () => {
          if (d.exited !== null) throw new Error(`${name} 데몬이 끝났습니다(코드 ${d.exited})\n${d.logTail()}`);
          if (!existsSync(tokenPath)) return undefined;
          const token = readFileSync(tokenPath, 'utf8').trim();
          if (!token) return undefined;
          try {
            return await connectNodeTransport({ endpoint, token });
          } catch {
            return undefined;
          }
        },
        30_000,
      );
    } catch (e) {
      d.kill();
      throw e;
    }
    d.client = new ApiClient(d.transport);
    d.client.subscribe((batch) => d.events.push(...batch));
    return d;
  }

  logTail(n = 60): string {
    return this.logs.slice(-n).join('\n');
  }

  /** 지금까지·앞으로 받은 알림 중 조건에 맞는 것을 기다립니다. */
  async waitEvent<T>(what: string, pick: (n: Notification) => T | undefined, timeoutMs = TIMEOUT_MS): Promise<T> {
    let seen = 0;
    return eventually(
      `${this.name}: ${what}`,
      async () => {
        for (; seen < this.events.length; seen++) {
          const v = pick(this.events[seen]);
          if (v !== undefined) return v;
        }
        return undefined;
      },
      timeoutMs,
    );
  }

  /** 다른 데몬이 이 데몬에 붙을 수 있는 주소(`/p2p/<id>` 포함). */
  async dialAddrs(): Promise<string[]> {
    const st = await eventually(`${this.name} 수신 주소`, async () => {
      const s = await this.client.call('net.status', {});
      return s.listen_addrs.length > 0 ? s : undefined;
    });
    return st.listen_addrs.map((a) => (a.includes('/p2p/') ? a : `${a}/p2p/${st.peer_id}`));
  }

  private kill() {
    if (this.exited === null) this.proc.kill('SIGKILL');
  }

  /** system.shutdown으로 끄고 끝나기를 기다립니다. 늦으면 강제로 끝냅니다. */
  async stop(): Promise<void> {
    if (this.exited === null) {
      await this.client?.call('system.shutdown', {}).catch(() => undefined);
      const done = await new Promise<boolean>((resolve) => {
        if (this.exited !== null) return resolve(true);
        const t = setTimeout(() => resolve(false), 15_000);
        this.proc.once('exit', () => {
          clearTimeout(t);
          resolve(true);
        });
      });
      if (!done) this.kill();
    }
    this.transport?.close();
  }
}

/** 데몬 묶음. 끝나면 모두 끄고 임시 경로를 지웁니다. */
export class Cluster {
  readonly root: string;
  readonly nodes: Daemon[] = [];
  private readonly bin: string;

  constructor(bin: string) {
    this.bin = bin;
    // 유닉스 소켓 경로 길이 제한(macOS 104바이트) 안에 들게 짧은 이름을 씁니다.
    this.root = mkdtempSync(join(tmpdir(), 'yp-'));
  }

  /** 데몬을 하나 띄웁니다. 먼저 띄운 데몬들의 주소를 부트스트랩으로 넘기므로 하나씩 차례로 부릅니다. */
  async start(name: string): Promise<Daemon> {
    const boot = (await Promise.all(this.nodes.map((n) => n.dialAddrs()))).flat();
    const d = await Daemon.start(this.bin, this.root, name, boot);
    this.nodes.push(d);
    return d;
  }

  /** 모든 데몬이 서로 연결될 때까지 기다립니다(mDNS 없이 127.0.0.1에서, 시작 때 건 부트스트랩 연결). */
  async mesh(): Promise<void> {
    const ids = await Promise.all(this.nodes.map(async (n) => (await n.client.call('net.status', {})).peer_id));
    for (const [i, n] of this.nodes.entries()) {
      const want = ids.filter((_, j) => j !== i);
      await eventually(`${n.name}: 다른 데몬과 모두 연결`, async () => {
        const peers = (await n.client.call('net.peers', {})).map((p) => p.peer_id);
        return want.every((id) => peers.includes(id));
      });
    }
  }

  logs(): string {
    return this.nodes.map((n) => `── ${n.name} ──\n${n.logTail(200)}`).join('\n');
  }

  async stop(): Promise<void> {
    await Promise.all(this.nodes.map((n) => n.stop()));
    rmSync(this.root, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 });
  }
}
