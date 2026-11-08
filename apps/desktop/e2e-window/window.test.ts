// 실제 Tauri 창 연기 시험(리눅스): tauri-driver → WebKitWebDriver가 빌드한 앱(yangpa-desktop)을 실제 창으로 띄우고,
// 앱은 옆에 있는 실제 데몬(yangpad, 워크스페이스 빌드)을 스스로 띄워 붙습니다. 화면에서 신원을 만들고
// 메인 화면이 그려지는지, 그 신원이 실제 데몬에 생겼는지 봅니다.
//
// 준비: `npm run test:window:build`(프런트엔드 + 데몬 + `custom-protocol,webdriver` 기능을 켠 앱),
//       tauri-driver(`cargo install tauri-driver --locked`), WebKitWebDriver(`apt install webkit2gtk-driver`), 화면(X·Wayland,
//       없으면 `xvfb-run -a npm run test:window`). 하나라도 없으면 이유를 알리고 건너뜁니다. CI처럼 꼭 돌려야 하면
//       YANGPA_WINDOW_REQUIRED=1(없으면 실패).
// 데이터·소켓·웹뷰 저장소와 XDG 설정·데이터·캐시 경로(앱의 UI 설정 파일 포함)는 모두 임시 폴더에 두므로 개발자의
// 실제 앱 설정을 읽거나 고치지 않습니다. 끝나면 정해진 시간 안에 세션·데몬·드라이버를 끄고, 그래도 남은 프로세스는
// 시험 경로를 물려받은 것만 찾아 끈 뒤 폴더를 지웁니다(cleanup.ts).

/// <reference types="node" />
import { spawn, type ChildProcess } from 'node:child_process';
import { accessSync, constants, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, statSync, writeFileSync } from 'node:fs';
import { createServer } from 'node:net';
import { tmpdir } from 'node:os';
import { delimiter, dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { afterAll, describe, expect, it } from 'vitest';
import { ApiClient } from '../src/api/client';
import { connectNodeTransport, type NodeTransport } from '../src/api/nodeTransport';
import { budget, reapWithEnv, runSteps, type Step } from './cleanup';
import { notYet, WebDriver, WebDriverError } from './webdriver';

const REQUIRED = process.env.YANGPA_WINDOW_REQUIRED === '1';
/** 화면 단계 하나의 최대 대기. 데몬 첫 시작(키 만들기)까지 감안합니다. */
const STEP_MS = 90_000;
const POLL_MS = 150;
const NAME = '창 시험';
/** 뒷정리 단계별 최대 대기: 세션 닫기, 데몬에 붙기, 데몬이 끝나기, 드라이버가 끝나기. */
const CLOSE_MS = 10_000;
const CONNECT_MS = 5_000;
const SHUTDOWN_MS = 15_000;
const DRIVER_EXIT_MS = 5_000;
/** 위 단계 뒤 남은 프로세스(닫는 중인 데몬·웹 프로세스)가 스스로 끝나기를 기다리는 시간. */
const REAP_GRACE_MS = 10_000;
/** 앱 식별자(Tauri `app_config_dir`의 마지막 이름). */
const APP_ID = (
  JSON.parse(readFileSync(fileURLToPath(new URL('../src-tauri/tauri.conf.json', import.meta.url)), 'utf8')) as { identifier: string }
).identifier;

function executable(p: string): boolean {
  try {
    accessSync(p, constants.X_OK);
    return statSync(p).isFile();
  } catch {
    return false;
  }
}

/** 환경 변수로 준 경로 또는 PATH에서 찾기. */
function which(cmd: string, envName: string): string | null {
  const explicit = process.env[envName];
  if (explicit) return executable(explicit) ? explicit : null;
  for (const dir of (process.env.PATH ?? '').split(delimiter)) {
    if (dir && executable(join(dir, cmd))) return join(dir, cmd);
  }
  return null;
}

/** 시험할 앱: YANGPA_WINDOW_APP → CARGO_TARGET_DIR·워크스페이스 target의 debug 빌드. */
function findApp(): string | null {
  const explicit = process.env.YANGPA_WINDOW_APP;
  if (explicit) return executable(explicit) ? explicit : null;
  const root = fileURLToPath(new URL('../../../', import.meta.url));
  for (const d of [process.env.CARGO_TARGET_DIR, join(root, 'target')]) {
    if (d && executable(join(d, 'debug', 'yangpa-desktop'))) return join(d, 'debug', 'yangpa-desktop');
  }
  return null;
}

const APP = findApp();
const DRIVER = which('tauri-driver', 'TAURI_DRIVER');
const NATIVE = which('WebKitWebDriver', 'WEBKIT_WEBDRIVER');

/** 돌릴 수 없는 이유들(비면 돌림). */
function missing(): string[] {
  if (process.platform !== 'linux') {
    return ['리눅스에서만 돌립니다(WebKitWebDriver). macOS·Windows 창은 자동 시험하지 않습니다(CONTRIBUTING.md)'];
  }
  const out: string[] = [];
  if (!process.env.DISPLAY && !process.env.WAYLAND_DISPLAY) out.push('화면이 없습니다: `xvfb-run -a npm run test:window`로 실행하세요');
  if (!DRIVER) out.push('tauri-driver가 없습니다: `cargo install tauri-driver --locked`(또는 TAURI_DRIVER=경로)');
  if (!NATIVE) out.push('WebKitWebDriver가 없습니다: `sudo apt install webkit2gtk-driver`(또는 WEBKIT_WEBDRIVER=경로)');
  if (!APP) out.push('앱 빌드가 없습니다: `npm run test:window:build`(또는 YANGPA_WINDOW_APP=경로)');
  else if (!executable(join(dirname(APP), 'yangpad'))) out.push('앱 옆에 데몬(yangpad)이 없습니다: `npm run test:window:build`');
  return out;
}

const MISSING = missing();
const WHY = `\n- ${MISSING.join('\n- ')}`;
const SKIP_MSG = `[창 시험] 실제 Tauri 창 시험을 건너뜁니다:${WHY}`;
if (MISSING.length > 0 && REQUIRED) throw new Error(`[창 시험] YANGPA_WINDOW_REQUIRED=1인데 실제 Tauri 창 시험을 돌릴 수 없습니다:${WHY}`);

/** 운영체제가 고른 빈 포트(고정 포트를 쓰지 않음). */
function freePort(): Promise<number> {
  return new Promise((resolve, reject) => {
    const srv = createServer();
    srv.unref();
    srv.on('error', reject);
    srv.listen(0, '127.0.0.1', () => {
      const addr = srv.address();
      srv.close(() => (addr && typeof addr === 'object' ? resolve(addr.port) : reject(new Error('포트를 얻지 못함'))));
    });
  });
}

/** 값이 나올 때까지 짧게 다시 봅니다. 요소가 아직 없는 것만 "아직"으로 보고 다른 오류는 바로 던집니다. */
async function waitFor<T>(what: string, f: () => Promise<T | undefined | null | false>, timeoutMs = STEP_MS): Promise<T> {
  const deadline = Date.now() + timeoutMs;
  let last: unknown;
  for (;;) {
    try {
      const v = await f();
      if (v !== undefined && v !== null && v !== false) return v;
    } catch (e) {
      if (!notYet(e)) throw e;
      last = e;
    }
    if (Date.now() >= deadline) throw new Error(`시간 초과: ${what}${last ? ` (마지막: ${(last as Error).message})` : ''}`);
    await new Promise((r) => setTimeout(r, POLL_MS));
  }
}

describe('실제 Tauri 창(리눅스)', () => {
  // 임시 경로는 실제로 돌 때만 만듭니다(건너뛰면 아무것도 남기지 않음).
  let root = '';
  let sockDir = '';
  let dataDir = '';
  let endpoint = '';
  let driver: ChildProcess | null = null;
  const driverLog: string[] = [];
  let wd: WebDriver | null = null;
  let api: { transport: NodeTransport; client: ApiClient } | null = null;

  /** 앱이 띄운 데몬에 소유자 토큰으로 붙습니다. */
  async function daemonClient(timeoutMs = STEP_MS) {
    if (api && !api.transport.closed) return api.client;
    const tokenPath = join(dataDir, 'owner.token');
    const transport = await waitFor(
      '데몬 API',
      async () => {
        if (!existsSync(tokenPath)) return null;
        const token = readFileSync(tokenPath, 'utf8').trim();
        return token ? connectNodeTransport({ endpoint, token }).catch(() => null) : null;
      },
      timeoutMs,
    );
    api = { transport, client: new ApiClient(transport) };
    return api.client;
  }

  /** 드라이버에 SIGTERM(자기 WebKitWebDriver를 끄고 끝남)을 보내고 끝나기를 기다립니다. */
  async function stopDriver() {
    const d = driver;
    if (!d || d.exitCode !== null || d.signalCode !== null) return;
    const gone = new Promise<void>((resolve) => d.once('exit', () => resolve()));
    d.kill('SIGTERM');
    await gone;
  }

  // 단계마다 최대 시간을 두어 걸린 세션·죽은 데몬이 있어도 훅 제한 시간 안에 드라이버를 끄고 폴더를 지웁니다.
  const teardown: Step[] = [
    { name: 'WebDriver 세션 닫기', ms: CLOSE_MS + 1_000, run: () => wd?.close(CLOSE_MS) },
    {
      // 앱이 띄운 데몬은 앱과 따로 돌므로(앱을 닫아도 시딩) 직접 끕니다.
      name: '데몬 끄기',
      ms: CONNECT_MS + SHUTDOWN_MS + 2_000,
      run: async () => {
        if (!existsSync(join(dataDir, 'owner.token'))) return;
        const c = await daemonClient(CONNECT_MS);
        await c.call('system.shutdown', {}).catch(() => undefined);
        await waitFor('데몬 종료', async () => !existsSync(endpoint), SHUTDOWN_MS);
      },
    },
    { name: 'tauri-driver 끄기', ms: DRIVER_EXIT_MS, run: stopDriver },
    {
      // 위에서 끄지 못한 것(답 없는 데몬, 드라이버가 남긴 WebKitWebDriver·앱·웹 프로세스)은 시험 경로를 물려받은
      // 것만 골라 끕니다.
      name: '남은 프로세스 끄기',
      ms: REAP_GRACE_MS + 2_000,
      run: async () => {
        api?.transport.close();
        const killed = await reapWithEnv('YANGPA_DATA_DIR', dataDir, REAP_GRACE_MS);
        if (killed.length > 0) console.warn(`[창 시험] 뒷정리에서 끝나지 않은 프로세스를 강제로 껐습니다: ${killed.join(', ')}`);
      },
    },
    {
      name: '임시 폴더 지우기',
      ms: 5_000,
      run: () => {
        rmSync(root, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 });
        if (sockDir !== root) rmSync(sockDir, { recursive: true, force: true });
      },
    },
  ];

  afterAll(async () => {
    if (!root) return;
    const problems = await runSteps(teardown);
    if (problems.length > 0) console.warn(`[창 시험] 뒷정리 문제:\n- ${problems.join('\n- ')}`);
  }, budget(teardown) + 10_000);

  it('신원을 화면에서 만들고 메인 화면이 그려진다', async (ctx) => {
    if (MISSING.length > 0) {
      // 테스트 안에서 알려야 보고서에 그대로 보입니다(파일을 읽을 때 낸 출력은 건너뛴 파일에서 보이지 않음).
      console.warn(SKIP_MSG);
      ctx.skip(MISSING.join(' / '));
    }
    root = mkdtempSync(join(tmpdir(), 'yangpa-window-'));
    // 유닉스 소켓 경로 한도(108바이트) 안에 들게, 임시 경로가 길면 소켓만 /tmp 아래에 둡니다.
    sockDir = root.length > 80 ? mkdtempSync('/tmp/ypw-') : root;
    dataDir = join(root, 'data');
    endpoint = join(sockDir, 'd.sock');
    const port = await freePort();
    let nativePort = await freePort();
    while (nativePort === port) nativePort = await freePort();
    // 앱의 설정·데이터·캐시 경로(Tauri `app_config_dir` 등, WebKitGTK 기본 경로)도 시험 폴더로 돌립니다. 그러지 않으면
    // 앱이 개발자의 실제 UI 설정(ui-prefs.json)을 읽고 고칩니다.
    const xdg = {
      XDG_CONFIG_HOME: join(root, 'xdg', 'config'),
      XDG_DATA_HOME: join(root, 'xdg', 'data'),
      XDG_CACHE_HOME: join(root, 'xdg', 'cache'),
      XDG_STATE_HOME: join(root, 'xdg', 'state'),
    };
    for (const d of Object.values(xdg)) mkdirSync(d, { recursive: true, mode: 0o700 });
    // 시험 폴더의 UI 설정을 미리 심어, 앱이 이 파일을 읽고(밝은 테마·OS 알림 끔, OS 언어와 상관없이 한국어 화면) 같은 파일에 쓰는지 아래에서 봅니다.
    const prefsDir = join(xdg.XDG_CONFIG_HOME, APP_ID);
    const prefsFile = join(prefsDir, 'ui-prefs.json');
    mkdirSync(prefsDir, { recursive: true });
    writeFileSync(prefsFile, JSON.stringify({ theme: 'light', notify: false, lang: 'ko' }));
    // 앱과 데몬이 시험 경로만 쓰게 바깥 설정을 지우고 넣습니다(드라이버 → WebKitWebDriver → 앱 → 데몬으로 이어짐).
    const env: NodeJS.ProcessEnv = {
      ...process.env,
      ...xdg,
      YANGPA_DATA_DIR: dataDir,
      YANGPA_ENDPOINT: endpoint,
      YANGPA_LOG: 'info',
      // 앱이 띄우는 데몬도 직접 연결로 띄웁니다(창 시험이 Tor 망에 나가지 않게).
      YANGPA_DIRECT: '1',
    };
    for (const k of ['YANGPA_DAEMON_BIN', 'YANGPA_NO_SPAWN', 'YANGPA_LOG_FILE', 'YANGPA_TOKEN']) delete env[k];
    driver = spawn(DRIVER!, ['--port', String(port), '--native-port', String(nativePort), '--native-driver', NATIVE!], {
      env,
      stdio: ['ignore', 'pipe', 'pipe'],
    });
    const keep = (b: Buffer) => {
      for (const line of b.toString('utf8').split('\n')) if (line.trim()) driverLog.push(line);
      if (driverLog.length > 200) driverLog.splice(0, driverLog.length - 200);
    };
    driver.stdout?.on('data', keep);
    driver.stderr?.on('data', keep);
    const base = `http://127.0.0.1:${port}`;

    try {
      await waitFor('tauri-driver 준비', async () => {
        if (driver?.exitCode !== null) throw new Error(`tauri-driver가 끝났습니다(코드 ${driver?.exitCode})`);
        return WebDriver.ready(base);
      }, 30_000);
      wd = await WebDriver.newSession(base, { 'tauri:options': { application: APP } });
      const w = wd;

      // 1. 앱이 데몬을 띄워 붙고(신원 없음) 처음 화면이 나옴. 데몬 없이 도는 MOCK이면 안 됨.
      await waitFor('처음 화면', () => w.find('.onboarding .ob-actions .btn.primary'));
      expect(await w.findAll('.mock-ribbon')).toHaveLength(0);
      await w.click(await w.find('.onboarding .ob-actions .btn.primary'));

      // 2. 이름 입력 → 신원 만들기.
      const nameInput = await waitFor('이름 칸', () => w.find('.ob-card input.input'));
      await w.type(nameInput, NAME);
      const createBtn = await w.find('.ob-card > button.btn.primary');
      await waitFor('신원 만들기 버튼', () => w.enabled(createBtn), 10_000);
      await w.click(createBtn);

      // 3. 메인 화면: 레일·사이드바가 그려지고 연결 오류·MOCK 표시가 없음.
      await waitFor('메인 화면', () => w.find('.app .layout nav.rail'));
      await w.find('.app .layout .sidebar');
      expect(await w.findAll('.onboarding')).toHaveLength(0);
      expect(await w.findAll('.daemon-banner.error')).toHaveLength(0);
      expect(await w.findAll('.mock-ribbon')).toHaveLength(0);
      expect(await w.title()).toBe('YANGPA');

      // 4. 화면에서 만든 신원이 실제 데몬에 있음(앱이 띄운 그 데몬).
      const c = await daemonClient();
      const me = await c.call('identity.get', {});
      expect(me.profile.name).toBe(NAME);

      // 5. UI 설정은 시험 폴더의 파일에서 읽고 거기에 씀: 심어 둔 밝은 테마로 그리고, 설정 화면에서 어두운 테마로 바꾸면
      //    같은 파일에 저장하며, 심어 둔 다른 값(OS 알림 끔)은 남아 있어야 합니다.
      await waitFor('심어 둔 밝은 테마', async () => (await w.execute<string>('return document.documentElement.dataset.theme || "";')) === 'light');
      await w.click(await w.find('button.icon-btn[title="사용자 설정"]'));
      await waitFor('사용자 설정 창', () => w.find('.settings-modal .settings-nav'));
      await w.execute(
        "const b = [...document.querySelectorAll('.settings-nav .sn-item')].find((x) => x.textContent.includes('모양·언어')); b && b.click();",
      );
      await w.click(await waitFor('테마 고르기', () => w.find('.theme-card.dark')));
      const saved = await waitFor('시험 폴더의 UI 설정 저장', async () => {
        const p = JSON.parse(readFileSync(prefsFile, 'utf8')) as Record<string, unknown>;
        return p.theme === 'dark' ? p : null;
      });
      expect(saved).toMatchObject({ theme: 'dark', notify: false });
      expect(saved).not.toHaveProperty('autoLowPower');
    } catch (e) {
      // 화면 단계(제목)와 드라이버 로그만 보탭니다(화면 내용·스크린샷은 남기지 않음).
      const step = wd
        ? await wd.execute<string>("const h = document.querySelector('h1, h2'); return h ? h.textContent : document.body.className;").catch(() => '?')
        : '세션 없음';
      const why = e instanceof WebDriverError && e.error === 'session not created' ? '\n(webdriver 기능 없이 빌드한 앱이면 창을 자동화할 수 없습니다: npm run test:window:build)' : '';
      throw new Error(`${(e as Error).message}${why}\n화면 단계: ${step}\n── tauri-driver ──\n${driverLog.slice(-40).join('\n')}`);
    }
  });
});
