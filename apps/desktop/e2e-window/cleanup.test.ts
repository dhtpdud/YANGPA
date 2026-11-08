// 창 시험 뒷정리 도구 시험(도구·화면 없이 돕니다). 걸린 단계가 뒤 단계를 막지 않는지, 남은 프로세스를 환경 변수로
// 찾아 끄되 다른 프로세스는 건드리지 않는지 봅니다.

/// <reference types="node" />
import { spawn, type ChildProcess } from 'node:child_process';
import { randomUUID } from 'node:crypto';
import { afterEach, describe, expect, it } from 'vitest';
import { budget, killWithEnv, pidsWithEnv, reapWithEnv, runSteps, within } from './cleanup';

const MARK = 'YANGPA_WINDOW_CLEANUP_TEST';

/** 조건이 참이 될 때까지 짧게 다시 봅니다(고정 대기 없음). */
async function eventually(what: string, f: () => boolean, timeoutMs = 20_000): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (!f()) {
    if (Date.now() >= deadline) throw new Error(`시간 초과: ${what}`);
    await new Promise((r) => setTimeout(r, 20));
  }
}

function exited(child: ChildProcess): Promise<NodeJS.Signals | null> {
  if (child.exitCode !== null || child.signalCode !== null) return Promise.resolve(child.signalCode);
  return new Promise((resolve) => child.once('exit', (_code, signal) => resolve(signal)));
}

describe('창 시험 뒷정리', () => {
  it('within은 끝남·실패·시간 초과를 가린다', async () => {
    expect(await within(async () => 1, 1_000)).toBe('ok');
    expect(await within(() => 1, 1_000)).toBe('ok');
    expect(await within(async () => Promise.reject(new Error('x')), 1_000)).toBe('failed');
    expect(
      await within(() => {
        throw new Error('동기 오류');
      }, 1_000),
    ).toBe('failed');
    expect(await within(() => new Promise(() => undefined), 30)).toBe('timeout');
  });

  it('걸리거나 실패한 단계가 있어도 뒤 단계(드라이버 끄기·폴더 지우기)는 돈다', async () => {
    const ran: string[] = [];
    const steps = [
      // 답 없는 WebDriver 세션 닫기, 죽은 데몬에 붙기를 흉내 냄
      { name: '세션 닫기', ms: 40, run: () => new Promise(() => undefined) },
      { name: '데몬 끄기', ms: 40, run: async () => Promise.reject(new Error('연결 안 됨')) },
      { name: '드라이버 끄기', ms: 40, run: () => ran.push('드라이버') },
      { name: '폴더 지우기', ms: 40, run: () => ran.push('폴더') },
    ];
    const started = Date.now();
    const problems = await runSteps(steps);
    expect(ran).toEqual(['드라이버', '폴더']);
    expect(problems).toEqual(['세션 닫기: 40ms 안에 끝나지 않음', '데몬 끄기: 실패']);
    expect(budget(steps)).toBe(160);
    // 걸린 단계를 제 시간만큼만 기다림(느린 러너를 감안해 넉넉히)
    expect(Date.now() - started).toBeLessThan(10_000);
  });

  describe.skipIf(process.platform !== 'linux')('환경 변수로 남은 프로세스 끄기(리눅스)', () => {
    const children: ChildProcess[] = [];
    afterEach(() => {
      for (const c of children.splice(0)) if (c.exitCode === null && c.signalCode === null) c.kill('SIGKILL');
    });

    /**
     * 오래 도는 Node 프로세스. `stubborn`이면 SIGTERM을 무시합니다(앱이 띄운 뒤 답하지 않는 데몬 흉내).
     * 처리기를 단 뒤 "ready"를 찍으므로 그것을 받은 뒤에야 신호를 보냅니다.
     */
    async function child(value: string, stubborn = true): Promise<ChildProcess> {
      const code = `${stubborn ? "process.on('SIGTERM', () => {});" : ''} setInterval(() => {}, 1000); console.log('ready');`;
      const c = spawn(process.execPath, ['-e', code], { env: { ...process.env, [MARK]: value }, stdio: ['ignore', 'pipe', 'ignore'] });
      children.push(c);
      await new Promise<void>((resolve, reject) => {
        let out = '';
        c.stdout?.on('data', (b: Buffer) => {
          out += b.toString('utf8');
          if (out.includes('ready')) resolve();
        });
        c.once('exit', () => reject(new Error('시험용 프로세스가 준비 전에 끝남')));
      });
      return c;
    }
    const stubborn = (value: string) => child(value);

    it('표시가 같은 프로세스만 SIGKILL로 끈다', async () => {
      const mine = randomUUID();
      const other = randomUUID();
      const target = await stubborn(mine);
      const bystander = await stubborn(other);
      // exec 전에는 부모 환경이 보이므로 새 환경이 보일 때까지 기다립니다.
      await eventually('대상 프로세스 환경', () => pidsWithEnv(MARK, mine).includes(target.pid!));
      await eventually('다른 프로세스 환경', () => pidsWithEnv(MARK, other).includes(bystander.pid!));
      // 값의 앞부분만 같은 것은 다른 표시입니다.
      expect(pidsWithEnv(MARK, mine.slice(0, 8))).toEqual([]);
      expect(pidsWithEnv(MARK, mine)).not.toContain(process.pid);

      expect(killWithEnv(MARK, mine)).toEqual([`${target.pid}(node)`]);
      expect(await exited(target)).toBe('SIGKILL');
      expect(bystander.exitCode).toBeNull();
      expect(bystander.signalCode).toBeNull();
      expect(pidsWithEnv(MARK, other)).toEqual([bystander.pid]);
    });

    it('스스로 끝나는 프로세스는 기다리고, 끝나지 않는 것만 강제로 끈다', async () => {
      const mark = randomUUID();
      const polite = await child(mark, false);
      const stuck = await stubborn(mark);
      await eventually('두 프로세스 환경', () => {
        const pids = pidsWithEnv(MARK, mark);
        return pids.includes(polite.pid!) && pids.includes(stuck.pid!);
      });
      // 정상 종료 요청(데몬의 system.shutdown 흉내)을 받은 쪽은 스스로 끝나고, 다른 쪽은 SIGTERM을 무시합니다.
      polite.kill('SIGTERM');
      stuck.kill('SIGTERM');
      expect(await reapWithEnv(MARK, mark, 300)).toEqual([`${stuck.pid}(node)`]);
      expect(await exited(polite)).toBe('SIGTERM');
      expect(await exited(stuck)).toBe('SIGKILL');
      // 남은 것이 없으면 기다리지 않고 바로 끝남
      expect(await reapWithEnv(MARK, mark, 60_000)).toEqual([]);
    });
  });
});
