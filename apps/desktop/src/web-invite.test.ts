// 웹 초대 페이지(web/index.html) 번역 검사: 앱과 같은 언어, 모든 언어에 같은 키, HTML 원문 = ko 값, 언어 고르기 = 앱.

import { readFileSync } from 'node:fs';
import { describe, expect, it } from 'vitest';
import { detectLang, LANGS } from './i18n';

const html = readFileSync(new URL('../../../web/index.html', import.meta.url), 'utf8');
const block = html.match(/\/\* i18n:start[^]*?\*\/([^]*?)\/\* i18n:end \*\//)?.[1] ?? '';
const { L, pickLang } = new Function(`${block}\nreturn { L, pickLang };`)() as {
  L: Record<string, Record<string, string>>;
  pickLang: (prefer: readonly string[]) => string;
};

describe('웹 초대 페이지', () => {
  it('앱과 같은 언어, 모든 언어에 같은 키', () => {
    expect(Object.keys(L).sort()).toEqual(LANGS.map(([c]) => c).sort());
    const keys = Object.keys(L.ko).sort();
    for (const [lang, dict] of Object.entries(L)) {
      expect(Object.keys(dict).sort(), lang).toEqual(keys);
      for (const [k, v] of Object.entries(dict)) expect(v.trim(), `${lang}.${k}`).not.toBe('');
    }
  });

  it('HTML 원문이 ko 값과 같음', () => {
    const tagged = [...html.matchAll(/data-t="(\w+)"[^>]*>([^<]*)</g)];
    expect(tagged.length).toBeGreaterThan(10);
    for (const [, key, text] of tagged) expect(text, key).toBe(L.ko[key]);
    expect(html.match(/<title>([^<]*)<\/title>/)?.[1]).toBe(L.ko.page);
  });

  it('언어 고르기가 앱과 같음', () => {
    const cases = [['ko-KR'], ['zh-Hant-TW'], ['zh-HK'], ['zh-CN'], ['pt-PT'], ['nl-NL', 'de-AT'], ['nl-NL'], ['EN-us'], ['constructor'], []];
    for (const c of cases) expect(pickLang(c), c.join(',')).toBe(detectLang(c));
  });
});
