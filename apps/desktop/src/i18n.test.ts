// 번역 함수와 카탈로그 검사: 모든 키가 모든 언어에 있고, 자리 이름이 원문과 같고, 화면 문구가 t() 밖에 없음.

import { afterEach, describe, expect, it } from 'vitest';
import { detectLang, LANGS, lookup, setLang, t, tc, type Entry } from './i18n';
import { scanApp, scanRust, slots } from './i18n.scan';

const app = scanApp();
const keys = new Set([...app.keys.keys(), ...scanRust()]);
const catalogs = import.meta.glob<Record<string, Entry>>('./locales/*.json', { eager: true, import: 'default' });

afterEach(() => setLang('ko'));

describe('t()', () => {
  it('카탈로그가 없으면 원문에 자리를 채움', () => {
    expect(t('{n}명', { n: 3 })).toBe('3명');
    expect(t('{a}와 {b}', { a: '가' })).toBe('가와 {b}');
    expect(lookup('없는 문구')).toBe('없는 문구');
    expect(tc('문맥', '{n}개', { n: 2 })).toBe('2개');
  });

  it('복수형을 n으로 고름', async () => {
    await setLang('en');
    const cat = catalogs['./locales/en.json'] ?? {};
    const [key, entry] = Object.entries(cat).find(([, v]) => typeof v === 'object') ?? [];
    if (!key || typeof entry !== 'object') return;
    expect(t(key, { n: 1 })).toBe(entry.one?.replace('{n}', '1') ?? entry.other?.replace('{n}', '1'));
    expect(t(key, { n: 5 })).toBe(entry.other?.replace('{n}', '5'));
  });

  it('OS 언어 고르기', () => {
    expect(detectLang(['ko-KR'])).toBe('ko');
    expect(detectLang(['zh-Hant-TW'])).toBe('zh-TW');
    expect(detectLang(['zh-CN'])).toBe('zh-CN');
    expect(detectLang(['pt-PT'])).toBe('pt-BR');
    expect(detectLang(['nl-NL', 'de-AT'])).toBe('de');
    expect(detectLang(['nl-NL'])).toBe('en');
  });
});

describe('카탈로그', () => {
  it('화면 문구는 모두 t()를 거침', () => {
    // I18N_FILES=modals/Crew.tsx,lib/misc.ts 처럼 주면 그 파일만 봅니다.
    const only = process.env.I18N_FILES?.split(',');
    const raw = only ? app.raw.filter((o) => only.includes(o.file)) : app.raw;
    expect(raw, 't() 밖 한글(화면에 안 뜨는 것이면 줄에 i18n-ignore)').toEqual([]);
  });

  for (const [lang] of LANGS.slice(1)) {
    it(`${lang}: 키가 빠지거나 남지 않고 자리 이름이 같음`, () => {
      const cat = catalogs[`./locales/${lang}.json`] ?? {};
      const missing = [...keys].filter((k) => !(k in cat));
      const stale = Object.keys(cat).filter((k) => !keys.has(k));
      const badSlots = Object.entries(cat).flatMap(([k, v]) =>
        (typeof v === 'string' ? [v] : Object.values(v)).filter((s) => s !== undefined && slots(s).join() !== slots(k).join()).map(() => k),
      );
      expect({ missing, stale, badSlots }).toEqual({ missing: [], stale: [], badSlots: [] });
    });
  }
});
