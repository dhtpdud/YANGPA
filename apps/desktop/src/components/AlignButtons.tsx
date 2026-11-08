// 글쓰기 칸의 정렬 단추(왼쪽·가운데·오른쪽). 고른 줄을 `:::center`…`:::`로 감싸거나 풀고(lib/align.ts), 커서·선택을 그대로 둡니다.

import { AlignCenter, AlignLeft, AlignRight } from 'lucide-react';
import type { RefObject } from 'react';
import { t } from '../i18n';
import { applyAlign } from '../lib/align';
import type { Align } from '../lib/markdown';
import { cx } from '../lib/misc';

const BUTTONS: Array<{ align: Align; title: string; Icon: typeof AlignLeft }> = [
  { align: 'left', title: t('왼쪽 정렬'), Icon: AlignLeft },
  { align: 'center', title: t('가운데 정렬'), Icon: AlignCenter },
  { align: 'right', title: t('오른쪽 정렬'), Icon: AlignRight },
];

export function AlignButtons({
  textarea,
  text,
  onChange,
  maxLength,
  disabled,
  size = 18,
  className,
}: {
  textarea: RefObject<HTMLTextAreaElement | null>;
  text: string;
  onChange: (text: string) => void;
  maxLength?: number;
  disabled?: boolean;
  size?: number;
  className?: string;
}) {
  const apply = (align: Align) => {
    const el = textarea.current;
    const r = applyAlign(text, el?.selectionStart ?? text.length, el?.selectionEnd ?? text.length, align);
    if (r.text === text || (maxLength !== undefined && r.text.length > maxLength)) return;
    onChange(r.text);
    // 값이 바뀐 뒤(다음 그리기)에 커서·선택을 되돌립니다.
    requestAnimationFrame(() => {
      el?.focus();
      el?.setSelectionRange(r.selStart, r.selEnd);
    });
  };
  return (
    <div className={cx('align-btns', className)} role="group" aria-label={t('정렬')}>
      {BUTTONS.map(({ align, title, Icon }) => (
        <button
          key={align}
          type="button"
          className="icon-btn small"
          title={title}
          aria-label={title}
          disabled={disabled}
          // 누르는 동안 글쓰기 칸이 포커스·선택을 잃지 않게 합니다.
          onMouseDown={(e) => e.preventDefault()}
          onClick={() => apply(align)}
        >
          <Icon size={size} />
        </button>
      ))}
    </div>
  );
}
