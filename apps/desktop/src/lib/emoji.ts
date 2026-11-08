// 커스텀 이모지 표기: 정지 `<:이름:ID>`, 애니메이션 `<a:이름:ID>` (디스코드 양식, yangpa-core emoji.rs와 같은 규칙).

import type { EmojiLimits } from '../api/types';
import { t } from '../i18n';

export interface EmojiToken {
  animated: boolean;
  name: string;
  /** 10진수 문자열(64비트라 숫자로 바꾸지 않음). */
  id: string;
  /** 원문에서의 [start, end) 범위(UTF-16 단위). */
  start: number;
  end: number;
}

/** u64 최댓값(10진수). ID가 이보다 크면 무효. */
const U64_MAX = '18446744073709551615';

/** 이름 규칙: 2~32자, 글자·숫자·밑줄(한글 포함). */
export function validEmojiName(name: string): boolean {
  const chars = Array.from(name);
  return chars.length >= 2 && chars.length <= 32 && /^[\p{L}\p{N}_]+$/u.test(name);
}

function validU64(id: string): boolean {
  if (!/^[0-9]+$/.test(id)) return false;
  const trimmed = id.replace(/^0+(?=\d)/, '');
  if (trimmed.length < U64_MAX.length) return true;
  if (trimmed.length > U64_MAX.length) return false;
  return trimmed <= U64_MAX;
}

export function formatEmoji(name: string, id: string, animated: boolean): string {
  return animated ? `<a:${name}:${id}>` : `<:${name}:${id}>`;
}

/** 텍스트에서 커스텀 이모지 표기를 모두 찾습니다. */
export function parseEmojiTokens(text: string): EmojiToken[] {
  const out: EmojiToken[] = [];
  let i = 0;
  while (i < text.length) {
    const start = text.indexOf('<', i);
    if (start < 0) break;
    const end = text.indexOf('>', start);
    if (end < 0) break;
    const inner = text.slice(start + 1, end);
    let animated: boolean;
    let rest: string;
    if (inner.startsWith('a:')) {
      animated = true;
      rest = inner.slice(2);
    } else if (inner.startsWith(':')) {
      animated = false;
      rest = inner.slice(1);
    } else {
      i = start + 1;
      continue;
    }
    const sep = rest.indexOf(':');
    if (sep > 0) {
      const name = rest.slice(0, sep);
      const id = rest.slice(sep + 1);
      if (validEmojiName(name) && id.length > 0 && validU64(id)) {
        out.push({ animated, name, id, start, end: end + 1 });
        i = end + 1;
        continue;
      }
    }
    i = start + 1;
  }
  return out;
}

/** 반응 문자열이 커스텀 이모지면 그 토큰. */
export function parseReactionEmoji(emoji: string): EmojiToken | null {
  const toks = parseEmojiTokens(emoji);
  return toks.length === 1 && toks[0].start === 0 && toks[0].end === emoji.length ? toks[0] : null;
}

/** 메시지가 이모지만으로 이뤄졌는지(크게 표시). 공백은 무시, 최대 27개. */
export function isEmojiOnly(text: string): boolean {
  const t = text.trim();
  if (!t) return false;
  let rest = t;
  const custom = parseEmojiTokens(t);
  if (custom.length > 27) return false;
  for (let k = custom.length - 1; k >= 0; k--) {
    rest = rest.slice(0, custom[k].start) + ' ' + rest.slice(custom[k].end);
  }
  rest = rest.replace(/\s+/g, '');
  if (!rest) return custom.length > 0;
  // Emoji_Component에는 숫자·#·*도 들어 있어, 그림 문자가 하나는 있어야 합니다.
  return (
    /^(?:\p{Extended_Pictographic}|\p{Emoji_Component}|‍|️)+$/u.test(rest) &&
    /\p{Extended_Pictographic}/u.test(rest) &&
    Array.from(rest).length <= 54
  );
}

/** 커스텀 이모지 표기를 `:이름:`으로 바꾼 글(그림을 넣을 수 없는 곳: 확인 창·툴팁·이어 붙인 글). */
export function emojiPlain(text: string): string {
  const toks = parseEmojiTokens(text);
  let out = text;
  for (let k = toks.length - 1; k >= 0; k--) out = out.slice(0, toks[k].start) + `:${toks[k].name}:` + out.slice(toks[k].end);
  return out;
}

/**
 * 이름·별명에 쓸 수 없는 그림 문자(이모지)가 있는지. 뱃지와 헷갈리지 않게 막습니다. 데몬의 `yangpa_core::badge::has_emoji`와
 * 같은 범위입니다(©®™·화살표 같은 글자 기호는 둠).
 */
export function hasEmoji(s: string): boolean {
  for (const ch of s) {
    const c = ch.codePointAt(0)!;
    if (
      (c >= 0x1f000 && c <= 0x1faff) ||
      (c >= 0x1fc00 && c <= 0x1fffd) ||
      (c >= 0x2600 && c <= 0x27bf) ||
      (c >= 0x231a && c <= 0x231b) ||
      c === 0x2328 ||
      c === 0x23cf ||
      (c >= 0x23e9 && c <= 0x23fa) ||
      (c >= 0x2b05 && c <= 0x2b07) ||
      (c >= 0x2b1b && c <= 0x2b1c) ||
      c === 0x2b50 ||
      c === 0x2b55 ||
      c === 0x3030 ||
      c === 0x303d ||
      c === 0x3297 ||
      c === 0x3299 ||
      c === 0x20e3 ||
      c === 0xfe0f ||
      (c >= 0xe0020 && c <= 0xe007f)
    )
      return true;
  }
  return false;
}

/** 크루 이모지 수 상한(데몬 해소기와 같음). 크루 한도는 이 안에서만 정합니다. */
export const MAX_EMOJIS = 500;

export type EmojiLimitsInput = { ok: true; limits: EmojiLimits } | { ok: false; error: string };

/** 입력란(개수, KiB) → 크루 이모지 한도. 비운 칸은 그 한도 없음. 개수는 1~500, 크기는 1KiB 이상의 정수. */
export function parseEmojiLimits(count: string, kib: string): EmojiLimitsInput {
  const c = count.trim();
  const k = kib.trim();
  const n = Number(c);
  if (c && (!Number.isInteger(n) || n < 1 || n > MAX_EMOJIS)) return { ok: false, error: t('개수는 1~{max} 사이의 정수로 넣어 주세요', { max: MAX_EMOJIS }) };
  const b = Number(k);
  if (k && (!Number.isInteger(b) || b < 1)) return { ok: false, error: t('크기는 1 이상의 정수(KiB)로 넣어 주세요') };
  return { ok: true, limits: { max_count: c ? n : null, max_bytes: k ? b * 1024 : null } };
}
