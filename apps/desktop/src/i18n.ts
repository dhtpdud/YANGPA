// 화면 문구 번역. 한국어 원문이 곧 키라(gettext 방식) 코드에는 원문이 그대로 남고, 카탈로그에 없으면 원문을 씁니다.
// 언어는 시작할 때 한 번 정하고(main.tsx) 바꾸면 화면을 다시 불러옵니다 — 모듈 최상위 상수도 t()를 그대로 쓰게.
// 그래서 main.tsx가 setLang 전에 불러오는 모듈(이 파일뿐)은 최상위에서 t()를 부르면 안 됩니다.
// 카탈로그 값이 {one, few, many, other} 객체면 `n` 자리 값으로 복수형을 고릅니다(Intl.PluralRules).

import { createElement, Fragment, type ReactNode } from 'react';

/** 지원 언어(코드, 그 언어로 쓴 이름). 첫째가 원문 언어. */
export const LANGS = [
  ['ko', '한국어'], // i18n-ignore
  ['en', 'English'],
  ['ja', '日本語'],
  ['zh-CN', '简体中文'],
  ['zh-TW', '繁體中文'],
  ['es', 'Español'],
  ['fr', 'Français'],
  ['de', 'Deutsch'],
  ['pt-BR', 'Português (Brasil)'],
  ['ru', 'Русский'],
  ['it', 'Italiano'],
  ['id', 'Bahasa Indonesia'],
  ['vi', 'Tiếng Việt'],
  ['tr', 'Türkçe'],
  ['hi', 'हिन्दी'],
] as const;

export type Lang = (typeof LANGS)[number][0];
export type Entry = string | Partial<Record<Intl.LDMLPluralRule, string>>;

const loaders = import.meta.glob<Record<string, Entry>>('./locales/*.json', { import: 'default' });

let lang: Lang = 'ko';
let catalog: Record<string, Entry> = {};
let rules = new Intl.PluralRules('ko');

export function isLang(v: unknown): v is Lang {
  return LANGS.some(([c]) => c === v);
}

/** OS 언어 목록에서 지원하는 첫 언어. 중국어는 지역·문자로 간체·번체를 가르고, 맞는 게 없으면 영어. */
export function detectLang(prefer: readonly string[]): Lang {
  for (const raw of prefer) {
    const tag = raw.toLowerCase();
    if (tag.startsWith('zh')) return /-(tw|hk|mo|hant)/.test(tag) ? 'zh-TW' : 'zh-CN';
    if (tag.startsWith('pt')) return 'pt-BR';
    const hit = LANGS.find(([c]) => c === tag.split('-')[0]);
    if (hit) return hit[0];
  }
  return 'en';
}

export async function setLang(next: Lang): Promise<void> {
  const load = loaders[`./locales/${next}.json`];
  catalog = next === 'ko' || !load ? {} : await load();
  lang = next;
  rules = new Intl.PluralRules(next);
  if (typeof document !== 'undefined') document.documentElement.lang = next;
}

export function currentLang(): Lang {
  return lang;
}

function pick(ko: string, n: unknown): string {
  const e = catalog[ko];
  if (e === undefined) return ko;
  if (typeof e === 'string') return e;
  return e[rules.select(Number(n))] ?? e.other ?? ko;
}

function fill(s: string, vars?: Record<string, string | number>): string {
  return vars ? s.replace(/\{(\w+)\}/g, (m, k: string) => (k in vars ? String(vars[k]) : m)) : s;
}

/** 원문 `ko`의 번역. `{이름}` 자리는 `vars`로 채웁니다. 첫 인자는 늘 글자 그대로(키 추출·검사가 읽음). */
export function t(ko: string, vars?: Record<string, string | number>): string {
  return fill(pick(ko, vars?.n), vars);
}

/** 같은 원문이 화면에 따라 다르게 옮겨져야 할 때의 t()(gettext 문맥처럼). 키는 `원문\u0004문맥`, 없으면 원문의 번역. */
export function tc(context: string, ko: string, vars?: Record<string, string | number>): string {
  const key = `${ko}\u0004${context}`;
  return fill(pick(key in catalog ? key : ko, vars?.n), vars);
}

/** 자리에 React 노드(굵게·링크 등)를 끼우는 t(). */
export function tn(ko: string, vars: Record<string, ReactNode>): ReactNode {
  const parts = pick(ko, vars.n).split(/\{(\w+)\}/);
  return createElement(Fragment, null, ...parts.map((p, i) => (i % 2 ? (p in vars ? vars[p] : `{${p}}`) : p)));
}

/** 밖에서 온 한국어 문구(데몬 오류 등)의 번역. 카탈로그에 없으면 undefined(한국어 화면이면 그대로). */
export function lookup(ko: string): string | undefined {
  if (lang === 'ko') return ko;
  const e = catalog[ko];
  return typeof e === 'string' ? e : e?.other;
}
