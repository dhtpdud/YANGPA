/// <reference types="node" />
// 창 시험 뒷정리 도구. 걸린 단계(답 없는 WebDriver, 죽은 데몬)가 있어도 정해진 시간 안에 끝내고, 시험이 띄운
// 프로세스 사슬(tauri-driver → WebKitWebDriver → 앱·웹 프로세스 → 데몬)을 하나도 남기지 않게 합니다.
// 뒷정리가 훅 제한 시간을 넘으면 vitest가 작업자를 내려 나머지 정리가 돌지 못하기 때문입니다.

import { readdirSync, readFileSync } from 'node:fs';

export type Outcome = 'ok' | 'failed' | 'timeout';

/** `f`를 돌려 `ms` 안에 끝나는지 봅니다. 늦으면 기다리지 않고 'timeout'(돌던 일 자체를 멈추지는 못함). */
export async function within(f: () => unknown, ms: number): Promise<Outcome> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  const late = new Promise<Outcome>((resolve) => {
    timer = setTimeout(() => resolve('timeout'), ms);
  });
  const run = Promise.resolve()
    .then(f)
    .then<Outcome, Outcome>(
      () => 'ok',
      () => 'failed',
    );
  try {
    return await Promise.race([run, late]);
  } finally {
    clearTimeout(timer);
  }
}

export interface Step {
  name: string;
  /** 이 단계를 기다리는 최대 시간. */
  ms: number;
  run: () => unknown;
}

/** 모든 단계의 최대 시간 합(훅 제한 시간은 이보다 넉넉해야 함). */
export function budget(steps: readonly Step[]): number {
  return steps.reduce((sum, s) => sum + s.ms, 0);
}

/** 단계를 차례로 돌립니다. 앞 단계가 실패하거나 늦어도 뒤 단계는 돕니다. 문제가 난 단계를 돌려줍니다. */
export async function runSteps(steps: readonly Step[]): Promise<string[]> {
  const problems: string[] = [];
  for (const s of steps) {
    const r = await within(s.run, s.ms);
    if (r !== 'ok') problems.push(`${s.name}: ${r === 'timeout' ? `${s.ms}ms 안에 끝나지 않음` : '실패'}`);
  }
  return problems;
}

/**
 * 환경 변수 `name=value`를 시작할 때 받은 프로세스의 PID(리눅스 `/proc`, 자기 자신 제외).
 * 시험이 띄운 사슬은 모두 시험 임시 경로(`YANGPA_DATA_DIR`)를 물려받으므로, 데몬이 API로 꺼지지 않거나 드라이버가
 * 자식을 남겨도 이것으로 찾습니다. 다른 사용자의 프로세스는 읽을 수 없어 건너뜁니다.
 */
export function pidsWithEnv(name: string, value: string): number[] {
  if (process.platform !== 'linux') return [];
  const needle = Buffer.from(`\0${name}=${value}\0`);
  let entries: string[];
  try {
    entries = readdirSync('/proc');
  } catch {
    return [];
  }
  const out: number[] = [];
  for (const e of entries) {
    if (!/^\d+$/.test(e) || Number(e) === process.pid) continue;
    try {
      // 첫 항목도 같은 모양으로 찾게 앞에 구분자를 붙입니다.
      if (Buffer.concat([Buffer.from([0]), readFileSync(`/proc/${e}/environ`)]).includes(needle)) out.push(Number(e));
    } catch {
      // 이미 끝났거나 읽을 권한이 없음
    }
  }
  return out;
}

/** 프로세스 이름(`/proc/<PID>/comm`, 진단용). 모르면 '?'. */
function procName(pid: number): string {
  try {
    return readFileSync(`/proc/${pid}/comm`, 'utf8').trim() || '?';
  } catch {
    return '?';
  }
}

/** `pidsWithEnv`로 찾은 프로세스를 SIGKILL로 끄고 끈 것을 "PID(이름)"으로 돌려줍니다. */
export function killWithEnv(name: string, value: string): string[] {
  const killed: string[] = [];
  for (const pid of pidsWithEnv(name, value)) {
    const comm = procName(pid);
    try {
      process.kill(pid, 'SIGKILL');
      killed.push(`${pid}(${comm})`);
    } catch {
      // 그사이 끝남
    }
  }
  return killed;
}

/**
 * 표시가 같은 프로세스가 스스로 끝나기를 `graceMs`까지 기다린 뒤(데몬은 소켓을 지운 뒤에도 잠시 닫는 중일 수 있음)
 * 남은 것만 SIGKILL로 끄고 그 목록을 돌려줍니다. 끝났지만 거두지 않은 좀비는 환경을 읽을 수 없어 세지 않습니다.
 */
export async function reapWithEnv(name: string, value: string, graceMs: number): Promise<string[]> {
  const deadline = Date.now() + graceMs;
  while (pidsWithEnv(name, value).length > 0 && Date.now() < deadline) {
    await new Promise((r) => setTimeout(r, 50));
  }
  return killWithEnv(name, value);
}
