// 포럼 태그 규칙(순수 함수). 데몬 검사(channel.create·channel.update의 forum_tags)와 같은 한도를 앱에서 먼저 알립니다:
// 20개까지, 이름 1~100자·앞뒤 공백·제어 문자 금지, ID 중복 금지. 이름이 같은 태그도 앱에서 막습니다(고르기 헷갈림).
// 태그는 디스코드처럼 글자 하나뿐이고 이모지는 이름 안에 바로 씁니다(100자에 이모지도 셈). 예전에 따로 붙은 `emoji`는
// 그대로 두고 보이기만 합니다(앱에서 고치지 않음).
// 새 태그 ID는 지금 목록에 없는 무작위 u32입니다. 최대값+1로 하면 지운 태그의 ID를 다시 써서, 그 태그가 붙어 있던 옛 글에
// 새 태그가 갑자기 보이게 됩니다(지운 ID를 가리키는 글은 그 태그를 그냥 보이지 않음).

import type { ForumTag } from '../api/types';
import { t } from '../i18n';

/** 채널 하나의 태그 상한(데몬 MAX_FORUM_TAGS). */
export const MAX_FORUM_TAGS = 20;
/** 태그 이름·이모지 길이 상한(글자, 데몬 MAX_NAME_CHARS). */
export const MAX_TAG_CHARS = 100;

const CONTROL = /\p{Cc}/u;

const chars = (s: string) => Array.from(s).length;

/** 태그 이름 오류(없으면 null). `others`는 이 태그를 뺀 나머지(이름 중복 검사). */
export function tagNameError(name: string, others: readonly ForumTag[] = []): string | null {
  if (!name.trim()) return t('태그 이름을 넣으세요');
  if (name !== name.trim()) return t('태그 이름 앞뒤에 공백을 둘 수 없습니다');
  if (chars(name) > MAX_TAG_CHARS) return t('태그 이름은 {n}자까지입니다', { n: MAX_TAG_CHARS });
  if (CONTROL.test(name)) return t('태그 이름에 제어 문자를 쓸 수 없습니다');
  const key = name.toLocaleLowerCase();
  if (others.some((o) => o.name.toLocaleLowerCase() === key)) return t('같은 이름의 태그가 이미 있습니다');
  return null;
}

/** 태그 이모지 오류(비워 두면 없음). */
export function tagEmojiError(emoji: string | null | undefined): string | null {
  if (!emoji) return null;
  if (chars(emoji) > MAX_TAG_CHARS) return t('태그 이모지는 {n}자까지입니다', { n: MAX_TAG_CHARS });
  if (CONTROL.test(emoji)) return t('태그 이모지에 제어 문자를 쓸 수 없습니다');
  return null;
}

/** 목록 전체의 첫 오류(없으면 null). */
export function tagsError(tags: readonly ForumTag[]): string | null {
  if (tags.length > MAX_FORUM_TAGS) return t('포럼 태그는 {n}개까지입니다', { n: MAX_FORUM_TAGS });
  for (let i = 0; i < tags.length; i++) {
    const tag = tags[i];
    if (!Number.isInteger(tag.id) || tag.id < 0 || tag.id > 0xffff_ffff) return t('태그 ID가 잘못됐습니다');
    if (tags.slice(0, i).some((x) => x.id === tag.id)) return t('태그 ID가 겹칩니다');
    const e = tagNameError(tag.name, tags.slice(0, i)) ?? tagEmojiError(tag.emoji);
    if (e) return t('「{name}」: {error}', { name: tag.name.trim() || t('이름 없음'), error: e });
  }
  return null;
}

/** 무작위 u32. 웹뷰에서는 crypto, 없으면 Math.random. */
function randomU32(): number {
  const c = (globalThis as { crypto?: Crypto }).crypto;
  if (c?.getRandomValues) return c.getRandomValues(new Uint32Array(1))[0];
  return Math.floor(Math.random() * 0x1_0000_0000);
}

/** 지금 목록에 없는 새 태그 ID(무작위 u32, 0은 피함). */
export function newTagId(existing: readonly ForumTag[], rand: () => number = randomU32): number {
  const used = new Set(existing.map((t) => t.id));
  for (;;) {
    const id = rand() >>> 0;
    if (id !== 0 && !used.has(id)) return id;
  }
}

/** 보낼 모양: 이름 앞뒤 공백을 다듬고, 예전부터 있던 `emoji`만 그대로 둡니다(새 태그는 늘 없음). */
export function cleanTag(t: ForumTag): ForumTag {
  return t.emoji ? { id: t.id, name: t.name.trim(), emoji: t.emoji } : { id: t.id, name: t.name.trim() };
}

/** 두 목록이 같은지(순서·ID·이름·이모지). */
export function sameTags(a: readonly ForumTag[], b: readonly ForumTag[]): boolean {
  if (a.length !== b.length) return false;
  return a.every((t, i) => t.id === b[i].id && t.name === b[i].name && (t.emoji || '') === (b[i].emoji || ''));
}
