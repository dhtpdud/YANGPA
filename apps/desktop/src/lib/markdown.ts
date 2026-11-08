// 메시지·포럼 글 마크다운(디스코드 수준): **굵게** *기울임*·_기울임_ __밑줄__ ~~취소선~~ ||스포일러|| `코드` ```코드 블록```,
// `> ` 인용·`>>> ` 뒤 전부 인용, `# `~`### ` 제목, `-# ` 작은 글씨, `- `·`* `·`1. ` 목록, [글](https://…) 가린 링크, `\` 이스케이프,
// `:::center`·`:::right`·`:::left` 줄과 `:::` 줄 사이 정렬(마크다운에 없는 표시라 우리 앱만 그림, 겹쳐 쓰지 않음).
// HTML로 바꾸지 않고 노드 트리를 돌려주므로 React가 그대로 이스케이프해 그립니다. 짝이 없거나 모르는 표기, http(s)가 아닌 링크는
// 글자 그대로 남습니다. 코드 안은 해석하지 않고, 글 조각 안의 링크·멘션(<@ID>)·커스텀 이모지는 예전 tokenize와 같습니다.
// `@everyone`·`@here`는 데몬 messages::broadcast_mention과 같은 경계(앞뒤가 글자·숫자·_가 아님)로 찾습니다.

import { t } from '../i18n';
import { parseReactionEmoji } from './emoji';
import { isSafeHttpUrl } from './text';

export type WrapKind = 'bold' | 'italic' | 'underline' | 'strike' | 'spoiler';

export type Inline =
  | { t: 'text'; v: string }
  | { t: 'emoji'; name: string; id: string; animated: boolean }
  | { t: 'mention'; user: string }
  | { t: 'everyone'; v: 'everyone' | 'here' }
  | { t: 'url'; href: string }
  | { t: 'link'; href: string; children: Inline[] }
  | { t: 'code'; v: string }
  | { t: WrapKind; children: Inline[] };

export interface ListItem {
  children: Inline[];
  /** 들여 쓴 하위 목록. */
  sub: Block[];
}

export type Block =
  /** 문단(줄바꿈은 그대로 둡니다). */
  | { t: 'p'; children: Inline[] }
  | { t: 'heading'; level: 1 | 2 | 3; children: Inline[] }
  | { t: 'subtext'; children: Inline[] }
  | { t: 'quote'; children: Block[] }
  | { t: 'list'; ordered: boolean; start: number; items: ListItem[] }
  | { t: 'codeblock'; lang: string; v: string }
  | { t: 'align'; align: Align; children: Block[] };

export type Align = 'left' | 'center' | 'right';

export interface MarkdownOptions {
  /**
   * 잘린 글(포럼 미리보기)용: 끝까지 닫히지 않은 코드 블록·인라인 코드와 두 글자 표시(** __ ~~ ||)를 글 끝에서 닫힌 것으로 봅니다.
   * 한 글자 표시(* _)는 2*3 같은 글자일 때가 많아 그대로 둡니다.
   */
  lenient?: boolean;
}

/** 본문 상한(렌더링 보호). 넘는 부분은 잘라서 표시합니다. */
export const MAX_RENDER_CHARS = 8000;
/** 서식 중첩 상한(이보다 깊으면 글자 그대로). */
const MAX_DEPTH = 12;
const MAX_LIST_DEPTH = 3;
/** 스포일러를 평문으로 바꿀 때(미리보기에 내용이 새지 않게). */
export const SPOILER_TEXT = t('(스포일러)');

export function parseMarkdown(input: string, opts: MarkdownOptions = {}): Block[] {
  const text = input.length > MAX_RENDER_CHARS ? input.slice(0, MAX_RENDER_CHARS) + '…' : input;
  const o: Ctx = { links: true, lenient: !!opts.lenient };
  return blocks(splitItems(text, o.lenient), false, o);
}

/** 미리보기용 평문: 서식 표시를 빼고, 커스텀 이모지는 `:이름:`, 멘션은 `@…`, 스포일러는 가립니다. */
export function stripMarkdown(input: string, opts: MarkdownOptions = {}): string {
  return parseMarkdown(input, opts).map(blockText).join('\n');
}

// ───────────── 블록 ─────────────

interface Ctx {
  /** 링크·멘션을 만들지(가린 링크의 글 안에서는 만들지 않음). */
  links: boolean;
  lenient: boolean;
}

/** 코드 블록을 떼어 낸 줄 목록. 코드 블록은 앞뒤 줄바꿈 하나씩을 먹습니다. */
type Item = { code: false; v: string } | { code: true; lang: string; v: string };

const FENCE_LANG = /^([a-zA-Z0-9_+-]{1,20})\n/;

/** 이스케이프되지 않은 ``` 위치. */
function findFence(text: string, from: number): number {
  let i = text.indexOf('```', from);
  while (i > 0 && text[i - 1] === '\\') i = text.indexOf('```', i + 1);
  return i;
}

function splitItems(text: string, lenient: boolean): Item[] {
  const out: Item[] = [];
  const lines = (s: string) => {
    for (const v of s.split('\n')) out.push({ code: false, v });
  };
  let pos = 0;
  let scan = 0;
  for (;;) {
    const open = findFence(text, scan);
    if (open < 0) break;
    const close = findFence(text, open + 3);
    if (close < 0 && !lenient) break;
    const end = close < 0 ? text.length : close;
    let body = text.slice(open + 3, end);
    let lang = '';
    const m = FENCE_LANG.exec(body);
    if (m) {
      lang = m[1];
      body = body.slice(m[0].length);
    } else if (body.startsWith('\n')) body = body.slice(1);
    if (body.endsWith('\n')) body = body.slice(0, -1);
    if (!body) {
      // 빈 코드 블록은 글자 그대로 둡니다.
      scan = end + 3;
      continue;
    }
    const before = text.slice(pos, open);
    if (before) lines(before.endsWith('\n') ? before.slice(0, -1) : before);
    out.push({ code: true, lang, v: body });
    pos = Math.min(text.length, end + 3);
    if (text[pos] === '\n') pos++;
    scan = pos;
  }
  if (pos < text.length) lines(text.slice(pos));
  return out;
}

const QUOTE_REST = /^ *>>> (.*)$/;
const QUOTE = /^ *> (.*)$/;
const HEADING = /^(#{1,3}) +(\S.*)$/;
const SUBTEXT = /^-# +(\S.*)$/;
/** 정렬 여는 줄·닫는 줄(입력창의 정렬 단추가 쓰는 꼴, lib/align.ts). */
export const ALIGN_OPEN = /^ *:::(left|center|right) *$/;
export const ALIGN_CLOSE = /^ *::: *$/;
// 번호는 세 자리까지(‘2024. 10. 23.’ 같은 날짜가 목록이 되지 않게).
const LIST = /^( *)(?:[-*]|(\d{1,3})\.) +(\S.*)$/;

function blocks(items: Item[], inQuote: boolean, o: Ctx, inAlign = false): Block[] {
  const out: Block[] = [];
  let para: string[] = [];
  const flush = (more: boolean) => {
    if (!para.length) return;
    let v = para.join('\n');
    // 줄 끝 줄바꿈 하나는 화면에 빈 줄로 보이지 않으므로, 뒤에 블록이 오면 하나 더 둡니다.
    if (more && v.endsWith('\n')) v += '\n';
    out.push({ t: 'p', children: parseInline(v, o, 0) });
    para = [];
  };
  for (let i = 0; i < items.length; i++) {
    const it = items[i];
    if (it.code) {
      flush(true);
      out.push({ t: 'codeblock', lang: it.lang, v: it.v });
      continue;
    }
    const line = it.v;
    let m: RegExpExecArray | null;
    // 정렬은 한 겹만(안의 :::left 등은 글자). 닫는 줄이 없으면 글자 그대로(잘린 미리보기는 끝에서 닫음).
    if (!inAlign && (m = ALIGN_OPEN.exec(line))) {
      let close = i + 1;
      while (close < items.length && (items[close].code || !ALIGN_CLOSE.test(items[close].v))) close++;
      if (close < items.length || o.lenient) {
        flush(true);
        out.push({ t: 'align', align: m[1] as Align, children: blocks(items.slice(i + 1, close), inQuote, o, true) });
        i = close;
        continue;
      }
    }
    // 인용은 한 겹만(디스코드처럼 인용 안의 >는 글자).
    if (!inQuote && (m = QUOTE_REST.exec(line))) {
      flush(true);
      out.push({ t: 'quote', children: blocks([{ code: false, v: m[1] }, ...items.slice(i + 1)], true, o, inAlign) });
      return out;
    }
    if (!inQuote && QUOTE.test(line)) {
      flush(true);
      const inner: Item[] = [];
      for (; i < items.length; i++) {
        const x = items[i];
        const q = x.code ? null : QUOTE.exec(x.v);
        if (!q) break;
        inner.push({ code: false, v: q[1] });
      }
      i--;
      out.push({ t: 'quote', children: blocks(inner, true, o, inAlign) });
      continue;
    }
    if ((m = HEADING.exec(line))) {
      flush(true);
      out.push({ t: 'heading', level: m[1].length as 1 | 2 | 3, children: parseInline(m[2], o, 0) });
      continue;
    }
    if ((m = SUBTEXT.exec(line))) {
      flush(true);
      out.push({ t: 'subtext', children: parseInline(m[1], o, 0) });
      continue;
    }
    if (LIST.test(line)) {
      flush(true);
      const li: LineItem[] = [];
      for (; i < items.length; i++) {
        const x = items[i];
        const l = x.code ? null : LIST.exec(x.v);
        if (!l) break;
        li.push({ indent: l[1].length, ordered: l[2] !== undefined, num: l[2] !== undefined ? Number(l[2]) : 1, text: l[3] });
      }
      i--;
      out.push(...lists(li, 0, li.length, 0, o));
      continue;
    }
    para.push(line);
  }
  flush(false);
  return out;
}

interface LineItem {
  indent: number;
  ordered: boolean;
  num: number;
  text: string;
}

/** 목록 줄들을 들여쓰기로 묶습니다(더 들여 쓴 줄은 바로 앞 항목의 하위 목록). 종류가 바뀌면 새 목록입니다. */
function lists(li: LineItem[], start: number, end: number, depth: number, o: Ctx): Block[] {
  const out: Block[] = [];
  let cur: Extract<Block, { t: 'list' }> | null = null;
  let i = start;
  while (i < end) {
    const it = li[i];
    let j = i + 1;
    if (depth < MAX_LIST_DEPTH - 1) while (j < end && li[j].indent > it.indent) j++;
    if (!cur || cur.ordered !== it.ordered) {
      cur = { t: 'list', ordered: it.ordered, start: it.num, items: [] };
      out.push(cur);
    }
    cur.items.push({ children: parseInline(it.text, o, 0), sub: j > i + 1 ? lists(li, i + 1, j, depth + 1, o) : [] });
    i = j;
  }
  return out;
}

// ───────────── 인라인 ─────────────

/** `\`로 글자 그대로 쓸 수 있는 문자(ASCII 문장 부호). 윈도 경로의 `\U` 같은 것은 그대로 둡니다. */
const PUNCT = /[!-/:-@[-`{-~]/;
const MENTION = /^<@([1-9A-HJ-NP-Za-km-z]{32,44})>$/;
const AUTOLINK = /^<(https?:\/\/[^\s<>]+)>$/;
const BROADCAST_AT = /@(everyone|here)(?![\p{L}\p{N}_])/uy;
const isNameChar = (c: string | undefined) => c !== undefined && /[\p{L}\p{N}_]/u.test(c);
/** lib/text.ts의 링크 규칙과 같음(이 자리에서만 맞춤). */
const URL_AT = /https?:\/\/[^\s<>"'`]+[^\s<>"'`.,;:!?)\]}]/y;
const LINK_AT = /\[([^[\]\n]{1,256})\]\(\s*<?(https?:\/\/(?:[^\s<>()]|\([^\s<>()]*\))+?)>?\s*\)/y;
/** 글 속 도메인 모양. 라벨은 유니코드 글자도 봅니다(한글·키릴 문자로 비슷하게 쓴 주소). */
const DOMAIN_IN_TEXT = /(?:[\p{L}\p{N}-]+\.)+[a-z]{2,}/giu;

/**
 * 가린 링크의 글이 실제 주소가 아닌 곳을 가리키는 것처럼 보이는지: 주소 모양이거나, 글 속 도메인이 실제 호스트(또는 그 상위
 * 도메인)가 아님(`[www.paypal.com](https://evil.example)`). 같은 곳이면(`[github.com/a](https://github.com/a)`) 그대로 가립니다.
 */
function misleadingLinkText(text: string, href: string): boolean {
  if (/https?:\/\//i.test(text)) return true;
  const host = new URL(href).hostname.toLowerCase();
  return [...text.toLowerCase().matchAll(DOMAIN_IN_TEXT)].some(([d]) => {
    const shown = d.replace(/^www\./, '');
    return host !== shown && !host.endsWith(`.${shown}`);
  });
}
/** 꺾쇠 표기(멘션·이모지·링크)를 찾을 때 볼 최대 길이. */
const MAX_ANGLE = 2048;

type Delim = '**' | '__' | '~~' | '||' | '*' | '_';
const NONE = -1;
const ABORT = -2;

const isWord = (c: string | undefined) => c !== undefined && /\w/.test(c);
const isSpace = (c: string | undefined) => c === undefined || /\s/.test(c);

function runLength(src: string, i: number, ch: string): number {
  let n = 0;
  while (src[i + n] === ch) n++;
  return n;
}

/** i의 백틱 묶음으로 여는 인라인 코드(같은 줄, 같은 개수로 닫음). */
function matchCode(src: string, i: number, lenient: boolean): { v: string; end: number } | null {
  const n = runLength(src, i, '`');
  if (n > 2) return null;
  let j = i + n;
  while (j < src.length && src[j] !== '\n') {
    if (src[j] === '`') {
      const m = runLength(src, j, '`');
      if (m === n) {
        let v = src.slice(i + n, j);
        if (!v) return null;
        // ``␣`x`␣`` 처럼 가장자리 백틱을 쓰려고 띄운 한 칸은 뺍니다.
        if (n === 2 && v.length > 2 && v.startsWith(' ') && v.endsWith(' ')) v = v.slice(1, -1);
        return { v, end: j + n };
      }
      j += m;
      continue;
    }
    j++;
  }
  return lenient && j >= src.length && j > i + n ? { v: src.slice(i + n), end: src.length } : null;
}

/** i의 꺾쇠 표기: 멘션·커스텀 이모지·<https://…>. */
function matchAngle(src: string, i: number, links: boolean): { node: Inline; end: number } | null {
  const close = src.indexOf('>', i);
  if (close < 0 || close - i > MAX_ANGLE) return null;
  const tok = src.slice(i, close + 1);
  const end = close + 1;
  const e = parseReactionEmoji(tok);
  if (e) return { node: { t: 'emoji', name: e.name, id: e.id, animated: e.animated }, end };
  // 가린 링크 글 안에서는 멘션 단추·링크를 만들지 않습니다(링크 안의 단추).
  if (!links) return null;
  const mm = MENTION.exec(tok);
  if (mm) return { node: { t: 'mention', user: mm[1] }, end };
  const um = AUTOLINK.exec(tok);
  if (um) return { node: { t: 'url', href: um[1] }, end };
  return null;
}

/**
 * from부터 닫는 표시를 찾습니다. 이스케이프·인라인 코드·꺾쇠 표기 안은 건너뜁니다. 끝까지 없으면 NONE,
 * 이 자리에서는 닫을 수 없으면(앞이 공백인 *) ABORT.
 */
function findClose(src: string, from: number, d: Delim, lenient: boolean): number {
  let j = from;
  while (j < src.length) {
    const c = src[j];
    if (c === '\\' && j + 1 < src.length && PUNCT.test(src[j + 1])) {
      j += 2;
      continue;
    }
    if (c === '`') {
      const r = matchCode(src, j, false);
      j = r ? r.end : j + runLength(src, j, '`');
      continue;
    }
    if (c === '<') {
      const r = matchAngle(src, j, true);
      if (r) {
        j = r.end;
        continue;
      }
    }
    if (d === '*' || d === '_') {
      // 한 글자 표시 안의 두 글자 표시(**·__)는 안쪽 서식이라 건너뜁니다.
      if (src.startsWith(d + d, j)) {
        j += 2;
        continue;
      }
      if (c === d) {
        if (d === '*') return isSpace(src[j - 1]) ? ABORT : j;
        // _는 낱말 안(snake_case)에서는 닫지 않습니다.
        if (!isWord(src[j + 1])) return j;
      }
    } else if (src.startsWith(d, j)) {
      // ***·___처럼 같은 글자가 이어지면 한 칸 뒤를 닫는 표시로 봅니다(**_*x*_**).
      if ((d === '**' || d === '__') && src[j + 2] === d[0]) {
        j++;
        continue;
      }
      return j;
    }
    j++;
  }
  return lenient && d.length === 2 ? src.length : NONE;
}

const WRAP: Record<Delim, WrapKind> = {
  '**': 'bold',
  __: 'underline',
  '~~': 'strike',
  '||': 'spoiler',
  '*': 'italic',
  _: 'italic',
};

function parseInline(src: string, o: Ctx, depth: number): Inline[] {
  const out: Inline[] = [];
  let buf = '';
  const push = (n: Inline) => {
    if (buf) out.push({ t: 'text', v: buf });
    buf = '';
    out.push(n);
  };
  // 끝까지 닫는 표시가 없던 시작 위치(그 뒤에서 다시 찾지 않음: 긴 글에서 같은 표시가 많아도 느려지지 않게).
  const failed = new Map<Delim, number>();
  let i = 0;

  const wrap = (d: Delim): boolean => {
    const from = i + d.length;
    if (from >= src.length || (failed.get(d) ?? Infinity) <= from) return false;
    const close = findClose(src, from, d, o.lenient);
    if (close === NONE) {
      failed.set(d, from);
      return false;
    }
    if (close === ABORT || close <= from) return false;
    push({ t: WRAP[d], children: parseInline(src.slice(from, close), o, depth + 1) });
    i = Math.min(src.length, close + d.length);
    return true;
  };

  while (i < src.length) {
    const c = src[i];
    const next = src[i + 1];
    if (c === '\\' && next !== undefined && PUNCT.test(next)) {
      buf += next;
      i += 2;
      continue;
    }
    if (c === '`') {
      const r = matchCode(src, i, o.lenient);
      if (r) {
        push({ t: 'code', v: r.v });
        i = r.end;
      } else {
        // 짝이 없는 백틱 묶음은 통째로 글자(안쪽 백틱과 짝짓지 않게).
        const n = runLength(src, i, '`');
        buf += src.slice(i, i + n);
        i += n;
      }
      continue;
    }
    if (c === '<') {
      const r = matchAngle(src, i, o.links);
      if (r) {
        push(r.node);
        i = r.end;
        continue;
      }
    }
    if (o.links && c === '@' && !isNameChar(src[i - 1])) {
      BROADCAST_AT.lastIndex = i;
      const m = BROADCAST_AT.exec(src);
      if (m) {
        push({ t: 'everyone', v: m[1] as 'everyone' | 'here' });
        i += m[0].length;
        continue;
      }
    }
    if (o.links && c === 'h' && !isWord(src[i - 1])) {
      URL_AT.lastIndex = i;
      const m = URL_AT.exec(src);
      if (m) {
        push({ t: 'url', href: m[0] });
        i += m[0].length;
        continue;
      }
    }
    if (o.links && c === '[') {
      LINK_AT.lastIndex = i;
      const m = LINK_AT.exec(src);
      // 글이 다른 주소처럼 보이면(속이기) 가리지 않고 글자 그대로 둡니다. 그 안의 주소는 그대로 링크가 됩니다.
      if (m && isSafeHttpUrl(m[2]) && !misleadingLinkText(m[1], m[2]) && depth < MAX_DEPTH) {
        push({ t: 'link', href: m[2], children: parseInline(m[1], { ...o, links: false }, depth + 1) });
        i += m[0].length;
        continue;
      }
    }
    if (depth < MAX_DEPTH) {
      if (c === '|' && next === '|' && wrap('||')) continue;
      if (c === '~' && next === '~' && wrap('~~')) continue;
      if (c === '*') {
        if (next === '*') {
          if (wrap('**')) continue;
        } else if (!isSpace(next) && wrap('*')) continue;
      }
      if (c === '_') {
        if (next === '_') {
          if (wrap('__')) continue;
        } else if (!isWord(src[i - 1]) && next !== undefined && wrap('_')) continue;
      }
    }
    buf += c;
    i++;
  }
  if (buf) out.push({ t: 'text', v: buf });
  return out;
}

// ───────────── 평문 ─────────────

function blockText(b: Block): string {
  switch (b.t) {
    case 'codeblock':
      return b.v;
    case 'quote':
    case 'align':
      return b.children.map(blockText).join('\n');
    case 'list':
      return b.items.map((it) => [inlineText(it.children), ...it.sub.map(blockText)].join('\n')).join('\n');
    default:
      return inlineText(b.children);
  }
}

function inlineText(ns: Inline[]): string {
  let s = '';
  for (const n of ns) {
    switch (n.t) {
      case 'text':
      case 'code':
        s += n.v;
        break;
      case 'emoji':
        s += `:${n.name}:`;
        break;
      case 'mention':
        s += '@…';
        break;
      case 'everyone':
        s += `@${n.v}`;
        break;
      case 'url':
        s += n.href;
        break;
      case 'spoiler':
        s += SPOILER_TEXT;
        break;
      default:
        s += inlineText(n.children);
    }
  }
  return s;
}
