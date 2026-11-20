// 본문 도우미: 링크 검사, 멘션 표기(<@사용자ID>) 바꾸기·찾기. 본문을 그리는 규칙(마크다운·링크·멘션·이모지)은 lib/markdown.ts에 있습니다.

/** base58 32바이트 식별자(대략 32~44자). */
const MENTION_RE = /<@([1-9A-HJ-NP-Za-km-z]{32,44})>/g;

/** 양방향 텍스트 제어 문자(CLI `one_line`과 같은 집합). 보이는 순서를 뒤집어 `사진‮gpj.exe`가 `사진exe.jpg`처럼 보입니다. */
const BIDI_CONTROLS = /[؜‎‏‪-‮⁦-⁩]/g;

/** 남이 정한 파일 이름에서 양방향 제어 문자를 뺍니다(보이는 이름·저장할 이름이 진짜 확장자를 숨기지 않게). */
export function plainName(name: string): string {
  return name.replace(BIDI_CONTROLS, '');
}

/** 링크가 http(s)인지 확인합니다(외부 열기 전 검사). */
export function isSafeHttpUrl(url: string): boolean {
  try {
    const u = new URL(url);
    return (u.protocol === 'http:' || u.protocol === 'https:') && !u.username && !u.password;
  } catch {
    return false;
  }
}

/**
 * 작성기에서 고른 표기(`@이름`·`#이름` → `<@ID>`·`<@&ID>`·`<#ID>`)를 꺾쇠 표기로 바꿉니다. 한 번에 훑어 긴 표기를 먼저
 * 맞추므로 접두어가 겹치거나 바꾼 표기 안의 ID가 다른 이름과 겹쳐도 다시 바꾸지 않습니다.
 */
export function encodeMentions(text: string, picked: Map<string, string>): string {
  const shown = [...picked.keys()].filter(Boolean).sort((a, b) => b.length - a.length);
  if (!shown.length) return text;
  const re = new RegExp(shown.map((s) => s.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')).join('|'), 'g');
  return text.replace(re, (s) => picked.get(s) ?? s);
}

/** 본문에 언급된 사용자 ID들. */
export function mentionedUsers(text: string): string[] {
  return [...text.matchAll(MENTION_RE)].map((m) => m[1]);
}
