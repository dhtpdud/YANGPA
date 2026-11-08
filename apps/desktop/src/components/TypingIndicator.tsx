// 디스코드처럼 입력창 아래에 '누가 입력 중…'을 보여 줍니다. 목록은 channel.typing 알림으로 채워지고(store/apply.ts),
// 몇 초 동안 새 알림이 없거나 그 사람이 글을 올리면 빠집니다.

import { t } from '../i18n';
import type { TypingEntry } from '../store/apply';
import { chKey, useStore } from '../store/store';
import { userName } from '../lib/ratelimit';

const EMPTY: TypingEntry[] = [];

/** 이름 목록을 한 줄로: 1~3명은 이름을, 그보다 많으면 '여러 명'. */
export function typingText(names: string[]): string {
  if (names.length === 0) return '';
  if (names.length > 3) return t('여러 명이 입력 중…');
  return t('{names}이 입력 중…', { names: names.map((name) => t('{name}님', { name })).join(', '), n: names.length });
}

export function TypingIndicator({ space, channel }: { space: string; channel: string }) {
  const list = useStore((s) => s.typing[chKey(space, channel)] ?? EMPTY);
  const views = useStore((s) => s.views);
  const friends = useStore((s) => s.friends);
  if (list.length === 0) return null;
  const names = list.map((e) => e.anonName || userName(e.user, { views, friends }, space));
  return (
    <div className="typing-indicator" aria-live="polite">
      <span className="typing-dots" aria-hidden>
        <i />
        <i />
        <i />
      </span>
      <span className="typing-text">{typingText(names)}</span>
    </div>
  );
}
