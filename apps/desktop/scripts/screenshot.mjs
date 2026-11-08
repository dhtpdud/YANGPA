// 빌드한 프런트엔드(dist)를 MOCK 백엔드로 띄워 주요 화면을 1440x900으로 찍습니다.
// 사용: npm run build && npm run screenshot
// Playwright는 전역 설치본을 씁니다(PLAYWRIGHT_MODULE로 경로 지정 가능). 브라우저를 내려받지 않고, 없으면 CHROME_PATH로 설치된 Chrome을 씁니다.
// 찍은 화면은 README가 그대로 씁니다.

import { createServer } from 'node:http';
import { readFile, mkdir } from 'node:fs/promises';
import { existsSync } from 'node:fs';
import { extname, isAbsolute, join, normalize, resolve, dirname } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, '..');
const dist = join(root, 'dist');
const outDir = join(root, 'screenshots');

async function loadPlaywright() {
  const candidates = [process.env.PLAYWRIGHT_MODULE, 'playwright', '/opt/node22/lib/node_modules/playwright/index.mjs'].filter(Boolean);
  for (const c of candidates) {
    try {
      return await import(isAbsolute(c) ? pathToFileURL(c).href : c);
    } catch {
      // 다음 후보
    }
  }
  throw new Error('playwright 모듈을 찾지 못했습니다(PLAYWRIGHT_MODULE로 경로를 지정하세요)');
}

const MIME = {
  '.html': 'text/html; charset=utf-8',
  '.js': 'text/javascript; charset=utf-8',
  '.css': 'text/css; charset=utf-8',
  '.svg': 'image/svg+xml',
  '.png': 'image/png',
  '.json': 'application/json',
};

function serve() {
  const server = createServer(async (req, res) => {
    try {
      const url = new URL(req.url ?? '/', 'http://localhost');
      let path = normalize(decodeURIComponent(url.pathname)).replace(/^([/\\])+/, '');
      if (path.includes('..')) throw new Error('bad path');
      let file = join(dist, path);
      if (!path || !existsSync(file)) file = join(dist, 'index.html');
      const body = await readFile(file);
      res.writeHead(200, { 'content-type': MIME[extname(file)] ?? 'application/octet-stream', 'cache-control': 'no-store' });
      res.end(body);
    } catch {
      res.writeHead(404);
      res.end();
    }
  });
  return new Promise((resolveServer) => server.listen(0, '127.0.0.1', () => resolveServer(server)));
}

async function main() {
  if (!existsSync(join(dist, 'index.html'))) throw new Error('dist가 없습니다. 먼저 npm run build');
  await mkdir(outDir, { recursive: true });
  const { chromium } = await loadPlaywright();
  const server = await serve();
  const port = server.address().port;
  const browser = await chromium.launch({
    executablePath: process.env.CHROME_PATH || undefined,
    args: ['--use-fake-ui-for-media-stream', '--use-fake-device-for-media-stream', '--autoplay-policy=no-user-gesture-required'],
  });
  const context = await browser.newContext({ viewport: { width: 1440, height: 900 }, deviceScaleFactor: 1, locale: 'ko-KR', timezoneId: 'Asia/Seoul' });
  await context.grantPermissions(['microphone', 'camera'], { origin: `http://127.0.0.1:${port}` });
  // 출시본에 없는 개발용 표시는 찍지 않습니다.
  const hideMockRibbon = () =>
    addEventListener('DOMContentLoaded', () => {
      const style = document.createElement('style');
      style.textContent = '.mock-ribbon{display:none}';
      document.head.append(style);
    });
  await context.addInitScript(hideMockRibbon);
  const page = await context.newPage();
  const errors = [];
  page.on('pageerror', (e) => errors.push(String(e)));
  page.on('console', (m) => {
    if (m.type() === 'error') errors.push(m.text());
  });
  const shotOf = async (p, name) => {
    // 마우스가 메시지 위에 있으면 도구 막대가 떠서 치웁니다.
    await p.mouse.move(700, 24);
    await p.waitForTimeout(400);
    await p.screenshot({ path: join(outDir, name) });
    console.log('저장:', name);
  };
  const shot = (name) => shotOf(page, name);

  await page.goto(`http://127.0.0.1:${port}/?quiet`);
  await page.waitForSelector('.rail');

  // 1) 크루 텍스트 채널(잡담, 2000개 메시지)
  await page.click('.rail-btn[title="게임 모임"]');
  await page.click('.ch-btn:has-text("잡담")');
  await page.waitForSelector('.msg');
  await shot('01-crew-text-channel.png');

  // 2) 휘발성 채널(입장·퇴장 + 60초 수명)
  await page.click('.ch-btn:has-text("비밀-작전")');
  await page.click('button:has-text("입장하기")');
  await page.waitForSelector('.msg .countdown');
  await page.fill('.composer textarea', '집결지는 북쪽 다리, 9시 정각 🧅');
  await page.keyboard.press('Enter');
  await page.waitForTimeout(500);
  await shot('02-volatile-channel.png');

  // 13) 휘발성 채널에서 스크린샷이 감지되면 채널 화면을 검게 가립니다(PrtScn keyup으로 흉내).
  await page.evaluate(() => window.dispatchEvent(new KeyboardEvent('keyup', { key: 'PrintScreen' })));
  await page.waitForSelector('.capture-shield');
  await shot('13-capture-shield.png');
  await page.click('.capture-shield button:has-text("확인")');

  // 3) 크루 설정 → 역할·권한
  await page.click('.sb-header');
  await page.click('.menu-item:has-text("크루 설정")');
  await page.click('.sn-item:has-text("역할·권한")');
  await page.click('.role-pick:has-text("운영진")');
  await shot('03-crew-settings-roles.png');
  await page.keyboard.press('Escape');

  // 4) P2P 전용 통화: 참가하면 민지의 화면공유가 저절로 켜집니다.
  await page.click('.ch-btn:has-text("일반 음성")');
  await page.click('button:has-text("통화 참가")');
  await page.waitForSelector('.voice-room');
  await page.waitForSelector('.vtile.has-video');
  await page.waitForTimeout(1200);
  // MOCK은 프레임을 실제로 암호화하지 않아 '시뮬레이션' 표시가 나오므로 찍지 않습니다.
  const hideE2ee = await page.addStyleTag({ content: '.vr-e2ee{display:none}' });
  await shot('04-voice-channel.png');
  await hideE2ee.evaluate((el) => el.remove());
  await page.click('.round-btn.danger');

  // 22) 일반 통화 채널: WebRTC 없이 소리만 Tor로
  await page.click('.ch-btn:has-text("게임 음성")');
  await page.click('button:has-text("통화 참가")');
  await page.waitForSelector('.voice-room');
  await page.waitForTimeout(800);
  await shot('22-tor-call.png');

  // 덤: 밝은 테마 + 포럼, 온보딩
  await page.click('.round-btn.danger');
  await page.evaluate(() => (document.documentElement.dataset.theme = 'light'));
  await page.click('.ch-btn:has-text("공략-게시판")');
  await page.waitForSelector('.post');
  await shot('05-forum-light.png');

  const page2 = await context.newPage();
  await page2.goto(`http://127.0.0.1:${port}/?quiet&onboarding`);
  await page2.waitForSelector('.ob-card');
  await page2.screenshot({ path: join(outDir, '06-onboarding.png') });
  console.log('저장: 06-onboarding.png');

  // 모더레이션·설정 화면(어두운 테마, 새 페이지)
  const page3 = await context.newPage();
  page3.on('pageerror', (e) => errors.push(String(e)));
  page3.on('console', (m) => {
    if (m.type() === 'error') errors.push(m.text());
  });
  await page3.goto(`http://127.0.0.1:${port}/?quiet`);
  await page3.waitForSelector('.rail');
  await page3.click('.rail-btn[title="게임 모임"]');
  await page3.click('.ch-btn:has-text("잡담")');
  await page3.waitForSelector('.msg');

  // 7) 크루 설정 → 모더레이션(한계 안내, 신고 목록, 밴 목록)
  await page3.click('.sb-header');
  await page3.click('.menu-item:has-text("크루 설정")');
  await page3.click('.sn-item:has-text("모더레이션")');
  await page3.waitForSelector('.report-row');
  await shotOf(page3, '07-crew-moderation.png');
  await page3.keyboard.press('Escape');

  // 8) 메시지 신고(운영진에게만 가며, 기본으로 내 화면에서 숨김)
  const target = page3.locator('.msg').filter({ has: page3.locator('.msg-author', { hasText: '민지' }) }).last();
  await target.hover();
  await target.locator('.qa[title="더 보기"]').click();
  await page3.click('.menu-item:has-text("신고하기")');
  await page3.fill('.modal textarea', '다른 크루원을 계속 비하합니다');
  await shotOf(page3, '08-report-message.png');
  await page3.keyboard.press('Escape');

  // 9) 차단한 사용자의 메시지는 접혀 보입니다.
  await page3.click('.rail-btn[title="러스트 스터디"]');
  await page3.click('.ch-btn:has-text("일반")');
  await page3.waitForSelector('.msg-blocked');
  await shotOf(page3, '09-blocked-messages.png');

  // 사용자 설정: 차단 목록, 네트워크·Tor(브리지), 음성·화면공유, 동기화(시더), AI 확인 대기
  await page3.click('.icon-btn[title="사용자 설정"]');
  for (const [tab, name] of [
    ['개인정보', '10-settings-blocked.png'],
    ['네트워크·Tor', '11-settings-network.png'],
    ['음성·화면공유', '12-settings-voice.png'],
    ['동기화', '16-settings-sync.png'],
    ['확인 대기', '17-ai-confirmations.png'],
  ]) {
    await page3.click(`.sn-item:has-text("${tab}")`);
    await shotOf(page3, name);
  }

  // 기기: 새 기기는 쓰던 기기의 QR로만, 양쪽 번호를 맞춰 본 사람이 허용
  await page3.click('.sn-item:has-text("기기")');
  await page3.click('button:has-text("QR 띄우기")');
  await page3.waitForSelector('.modal:has-text("새 기기 연결 요청")');
  await shotOf(page3, '15-device-link.png');
  await page3.click('.modal button:has-text("거절")');
  await page3.keyboard.press('Escape');

  // 익명 채널(화면에서만 가명)과 P2P 전용 채널(참가 전 IP 경고)
  await page3.click('.rail-btn[title="게임 모임"]');
  await page3.click('.ch-btn:has-text("익명-건의함")');
  await page3.waitForSelector('.msg');
  await shotOf(page3, '14-anonymous-channel.png');
  await page3.click('.ch-btn:has-text("직접-대화")');
  await page3.waitForSelector('button:text-is("참가")');
  await shotOf(page3, '19-p2p-channel-lock.png');

  // 신뢰 스위치: 사람마다 IP를 보여도 되는지 정함
  await page3.click('.mem-row:has-text("민지")');
  await page3.click('.modal-tabs button:has-text("설정")');
  await shotOf(page3, '20-trust-switch.png');

  // Android: 좁은 화면은 칸을 하나씩 보여 줍니다.
  const phone = await browser.newContext({
    viewport: { width: 390, height: 844 },
    deviceScaleFactor: 2,
    hasTouch: true,
    isMobile: true,
    userAgent: 'Mozilla/5.0 (Linux; Android 15) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0 Mobile Safari/537.36',
    locale: 'ko-KR',
    timezoneId: 'Asia/Seoul',
  });
  await phone.addInitScript(hideMockRibbon);
  const page4 = await phone.newPage();
  await page4.goto(`http://127.0.0.1:${port}/?quiet`);
  await page4.waitForSelector('.rail');
  await page4.click('.rail-btn[title="게임 모임"]');
  await page4.click('.ch-btn:has-text("비밀-작전")');
  await page4.click('button:has-text("입장하기")');
  await page4.waitForSelector('.msg .countdown');
  await page4.waitForTimeout(500);
  await page4.screenshot({ path: join(outDir, '21-android.png') });
  console.log('저장: 21-android.png');

  await browser.close();
  server.close();
  if (errors.length) {
    console.log('페이지 오류:');
    for (const e of errors) console.log(' -', e);
    process.exitCode = 1;
  }
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
