// 포럼 태그 편집(채널 설정·채널 만들기): 태그마다 글자 입력 하나(이모지도 이름 안에), 더하기·이름 바꾸기·지우기.
// 저장은 채널 설정의 저장 단추가 목록 전체를 channel.update(forum_tags)로 보냅니다. 예전에 따로 붙은 이모지는 보이기만 합니다.

import { Plus, X } from 'lucide-react';
import { useState } from 'react';
import type { ForumTag } from '../api/types';
import { MAX_FORUM_TAGS, MAX_TAG_CHARS, newTagId, tagNameError } from '../lib/forumTags';
import { t } from '../i18n';
import { cx } from '../lib/misc';

export function ForumTagsEditor({ tags, onChange }: { tags: ForumTag[]; onChange: (tags: ForumTag[]) => void }) {
  const [draft, setDraft] = useState('');
  const [addError, setAddError] = useState<string | null>(null);
  const full = tags.length >= MAX_FORUM_TAGS;
  const add = () => {
    const name = draft.trim();
    const err = full ? t('포럼 태그는 {n}개까지입니다', { n: MAX_FORUM_TAGS }) : tagNameError(name, tags);
    if (err) {
      setAddError(err);
      return;
    }
    onChange([...tags, { id: newTagId(tags), name }]);
    setDraft('');
    setAddError(null);
  };
  return (
    <div className="ft-editor">
      {tags.length === 0 && <div className="muted small">{t('태그가 없습니다. 글쓴이가 고를 태그를 더하세요.')}</div>}
      {tags.map((tag, i) => {
        // 입력 중 앞뒤 공백은 저장할 때 다듬으므로 다듬은 이름으로 검사합니다.
        const err = tagNameError(tag.name.trim(), tags.filter((_, j) => j !== i));
        return (
          <div key={tag.id} className="ft-row">
            {tag.emoji && (
              <span className="ft-emoji" title={t('예전에 따로 붙은 이모지(여기서는 고치지 않음)')}>
                {tag.emoji}
              </span>
            )}
            <input
              className={cx('input', err && 'invalid')}
              aria-label={t('태그 이름 {n}', { n: i + 1 })}
              aria-invalid={!!err}
              value={tag.name}
              maxLength={MAX_TAG_CHARS * 2}
              onChange={(e) => onChange(tags.map((x) => (x.id === tag.id ? { ...x, name: e.target.value } : x)))}
            />
            <button
              type="button"
              className="icon-btn small danger"
              title={t('태그 지우기')}
              aria-label={tag.name ? t('{name} 태그 지우기', { name: tag.name }) : t('이름 없는 태그 지우기')}
              onClick={() => onChange(tags.filter((x) => x.id !== tag.id))}
            >
              <X size={14} />
            </button>
            {err && <span className="err ft-err">{err}</span>}
          </div>
        );
      })}
      <div className="ft-row ft-add">
        <input
          className={cx('input', addError && 'invalid')}
          aria-label={t('새 태그 이름')}
          placeholder={full ? t('{n}개까지 둘 수 있습니다', { n: MAX_FORUM_TAGS }) : t('새 태그(이모지도 바로 쓸 수 있음)')}
          value={draft}
          disabled={full}
          maxLength={MAX_TAG_CHARS * 2}
          onChange={(e) => {
            setDraft(e.target.value);
            setAddError(null);
          }}
          onKeyDown={(e) => {
            if (e.nativeEvent.isComposing) return;
            if (e.key === 'Enter') {
              e.preventDefault();
              add();
            }
          }}
        />
        <button type="button" className="btn small" disabled={full || !draft.trim()} onClick={add}>
          <Plus size={14} /> {t('태그 추가')}
        </button>
        {addError && <span className="err ft-err">{addError}</span>}
      </div>
      <div className="field-hint">
        {t('{n}/{max}개', { n: tags.length, max: MAX_FORUM_TAGS })} ·{' '}
        {t('이름은 {n}자까지(이모지 포함). 태그를 지우면 그 태그가 붙은 글에서는 보이지 않습니다.', { n: MAX_TAG_CHARS })}
      </div>
    </div>
  );
}
