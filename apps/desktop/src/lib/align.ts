// 입력창의 정렬 단추: 고른 줄(고른 게 없으면 커서가 있는 줄)을 `:::center`…`:::` 정렬 묶음으로 감쌉니다(lib/markdown.ts가 그림).
// 커서가 이미 정렬 묶음 안이면 새로 감싸지 않고 그 묶음의 정렬만 바꿉니다. 왼쪽은 기본 정렬이라 묶음을 걷어 냅니다.

import { ALIGN_CLOSE, ALIGN_OPEN, type Align } from './markdown';

export interface Edited {
  text: string;
  selStart: number;
  selEnd: number;
}

/** 줄마다 시작 위치. */
function lineStarts(text: string): number[] {
  const out = [0];
  for (let i = 0; i < text.length; i++) if (text[i] === '\n') out.push(i + 1);
  return out;
}

function lineAt(starts: number[], pos: number): number {
  let lo = 0;
  let hi = starts.length - 1;
  while (lo < hi) {
    const mid = (lo + hi + 1) >> 1;
    if (starts[mid] <= pos) lo = mid;
    else hi = mid - 1;
  }
  return lo;
}

/** 줄 [from, to]를 감싼 정렬 묶음의 (여는 줄, 닫는 줄). 코드 블록 안의 표시는 셈하지 않습니다. */
function enclosing(lines: string[], from: number, to: number): [number, number] | null {
  let open = -1;
  let fence = false;
  for (let i = 0; i < from; i++) {
    const l = lines[i];
    if (l.trimStart().startsWith('```')) fence = !fence;
    if (fence) continue;
    if (open < 0 && ALIGN_OPEN.test(l)) open = i;
    else if (open >= 0 && ALIGN_CLOSE.test(l)) open = -1;
  }
  if (open < 0 && !fence && ALIGN_OPEN.test(lines[from])) open = from;
  if (open < 0) return null;
  for (let i = Math.max(open + 1, from); i < lines.length; i++) {
    const l = lines[i];
    if (l.trimStart().startsWith('```')) fence = !fence;
    if (fence) continue;
    if (ALIGN_CLOSE.test(l)) return i >= to ? [open, i] : null;
    if (ALIGN_OPEN.test(l)) return null;
  }
  return null;
}

export function applyAlign(text: string, selStart: number, selEnd: number, align: Align): Edited {
  const starts = lineStarts(text);
  const lines = text.split('\n');
  const from = lineAt(starts, selStart);
  // 다음 줄 맨 앞까지 고른 것은 그 줄을 고르지 않은 것으로 봅니다.
  const to = Math.max(from, lineAt(starts, selEnd > selStart && text[selEnd - 1] === '\n' ? selEnd - 1 : selEnd));
  const shift = (pos: number, at: number, delta: number) => (pos >= at ? Math.max(at, pos + delta) : pos);

  const enc = enclosing(lines, from, to);
  if (enc) {
    const [open, close] = enc;
    if (align !== 'left') {
      const old = lines[open];
      lines[open] = `:::${align}`;
      const delta = lines[open].length - old.length;
      const at = starts[open] + old.length;
      return { text: lines.join('\n'), selStart: shift(selStart, at, delta), selEnd: shift(selEnd, at, delta) };
    }
    // 닫는 줄부터 지워야 앞의 위치가 그대로입니다.
    const closeAt = starts[close] - 1;
    const closeLen = lines[close].length + 1;
    const openLen = lines[open].length + 1;
    lines.splice(close, 1);
    lines.splice(open, 1);
    const fix = (p: number) => shift(shift(p, closeAt, -closeLen), starts[open], -openLen);
    return { text: lines.join('\n'), selStart: fix(selStart), selEnd: fix(selEnd) };
  }
  if (align === 'left') return { text, selStart, selEnd };
  const head = `:::${align}\n`;
  const body = lines.slice(from, to + 1).join('\n');
  const before = lines.slice(0, from).join('\n');
  const after = lines.slice(to + 1).join('\n');
  const out = (from > 0 ? before + '\n' : '') + head + body + '\n:::' + (to + 1 < lines.length ? '\n' + after : '');
  // 다음 줄 맨 앞에 있던 끝은 묶음 안 끝으로 당깁니다.
  const bodyEnd = starts[to] + lines[to].length;
  return { text: out, selStart: selStart + head.length, selEnd: Math.min(selEnd, bodyEnd) + head.length };
}
