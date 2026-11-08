// 영어 화면에서는 MOCK 샘플 데이터도 영어여야 README 영어 스크린샷에 한글이 섞이지 않습니다.

import { expect, it, vi } from 'vitest';

const HANGUL = /[ㄱ-ㅎㅏ-ㅣ가-힣]/;

/** 객체 안의 모든 문자열 중 한글이 든 것(Map·Set·클래스 인스턴스까지, 순환은 한 번만). */
function hangulStrings(root: unknown): string[] {
  const out = new Set<string>();
  const seen = new WeakSet<object>();
  const walk = (v: unknown) => {
    if (typeof v === 'string') {
      if (HANGUL.test(v)) out.add(v.length > 120 ? `${v.slice(0, 120)}…` : v);
      return;
    }
    if (!v || typeof v !== 'object' || seen.has(v)) return;
    seen.add(v);
    if (v instanceof Map) for (const [k, x] of v) (walk(k), walk(x));
    else if (v instanceof Set) for (const x of v) walk(x);
    else for (const x of Object.values(v)) walk(x);
  };
  walk(root);
  return [...out];
}

it('영어 화면의 샘플 데이터에 한글이 없음', async () => {
  // 샘플 문구는 모듈을 불러올 때 정해지므로 언어를 먼저 바꾸고 새로 불러옵니다.
  vi.resetModules();
  const i18n = await import('../i18n');
  await i18n.setLang('en');
  const data = await import('./mockData');
  const { createMockTransport } = await import('./mock');
  const t = createMockTransport({ quiet: true, seed: 7 });
  try {
    // SVG 첨부 그림 속 글자도 문자열이라 함께 봅니다.
    expect(hangulStrings([t.backend, data])).toEqual([]);
  } finally {
    t.backend.dispose();
    await i18n.setLang('ko');
  }
});
