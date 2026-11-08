// 크루 검색어 풀이: 디스코드처럼 `from:이름`을 쓰면 그 사람이 쓴 글만 찾습니다(이름의 빈칸은 _). 나머지 글자는 본문 검색어.

import type { MemberView, UserId } from '../api/types';
import { displayName } from './misc';

const FROM = /(^|\s)from:(\S*)/i;

/** 검색창에 넣는 그 사람의 이름 토큰(빈칸은 _). */
export function searchToken(m: MemberView): string {
  return displayName(m).replace(/\s+/g, '_');
}

const norm = (s: string) => s.trim().replace(/\s+/g, '_').toLowerCase();

export interface ParsedSearch {
  /** 본문 검색어(from: 토큰을 뺀 것). */
  query: string;
  author?: UserId;
  /** 찾은 사람의 화면 이름. */
  authorName?: string;
  /** from:에 적었지만 멤버 중에 없는 이름. */
  unknown?: string;
}

export function parseSearch(q: string, members: MemberView[]): ParsedSearch {
  const hit = FROM.exec(q);
  if (!hit) return { query: q.trim() };
  const query = (q.slice(0, hit.index) + hit[1] + q.slice(hit.index + hit[0].length)).replace(/\s+/g, ' ').trim();
  const want = norm(hit[2]);
  if (!want) return { query };
  const m =
    members.find((x) => norm(searchToken(x)) === want) ??
    members.find((x) => norm(x.profile.name) === want || (!!x.nickname && norm(x.nickname) === want));
  return m ? { query, author: m.user, authorName: displayName(m) } : { query, unknown: hit[2] };
}

/** 입력 끝이 `from:…`이면 이름이 맞는 멤버(8명까지). 아니면 null. */
export function authorSuggestions(q: string, members: MemberView[]): MemberView[] | null {
  const hit = /(^|\s)from:(\S*)$/i.exec(q);
  if (!hit) return null;
  const part = norm(hit[2]);
  return members.filter((m) => norm(searchToken(m)).includes(part)).slice(0, 8);
}

/** 입력 끝의 `from:…`을 고른 사람으로 바꿉니다. */
export function applyAuthor(q: string, m: MemberView): string {
  return q.replace(/(^|\s)from:\S*$/i, (_all, pre: string) => `${pre}from:${searchToken(m)} `);
}

/** 검색어 낱말(데몬 `parse_terms`와 같은 규칙, 모의 백엔드용): 빈칸으로 나누고 큰따옴표로 묶은 것은 구절, 소문자·빈칸 한 칸. */
export function searchTerms(q: string): string[] {
  const raw: string[] = [];
  let cur = '';
  let quoted = false;
  for (const c of q.normalize('NFKC')) {
    if (c === '"' || c === '“' || c === '”') {
      raw.push(cur);
      cur = '';
      quoted = !quoted;
    } else if (/\s/.test(c) && !quoted) {
      raw.push(cur);
      cur = '';
    } else cur += c;
  }
  raw.push(cur);
  const terms = [...new Set(raw.map((t) => t.trim().toLowerCase().replace(/\s+/g, ' ')).filter(Boolean))];
  terms.sort((a, b) => b.length - a.length);
  return terms.filter((t, i) => !terms.slice(0, i).some((o) => o.includes(t)));
}

/** 검색 결과 아래 쪽 번호 줄: 첫 쪽·끝 쪽과 지금 쪽 둘레를 보이고 사이는 'gap'. `cur`는 1부터. */
export function pageList(cur: number, pages: number, around = 2): (number | 'gap')[] {
  if (pages <= around * 2 + 5) return Array.from({ length: pages }, (_, i) => i + 1);
  let lo = Math.max(2, cur - around);
  let hi = Math.min(pages - 1, cur + around);
  // 양 끝 가까이면 반대쪽으로 늘려 칸 수를 고르게 둡니다.
  if (cur - around <= 2) hi = around * 2 + 3;
  if (cur + around >= pages - 1) lo = pages - around * 2 - 2;
  // 빈칸이 한 쪽만 가리면 그 쪽 번호를 그대로 보입니다.
  if (lo === 3) lo = 2;
  if (hi === pages - 2) hi = pages - 1;
  const out: (number | 'gap')[] = [1];
  if (lo > 2) out.push('gap');
  for (let i = lo; i <= hi; i++) out.push(i);
  if (hi < pages - 1) out.push('gap');
  out.push(pages);
  return out;
}
