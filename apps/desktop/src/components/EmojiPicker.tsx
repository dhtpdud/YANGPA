// 이모지 고르기: 이 공간의 커스텀 이모지, (공간 설정과 권한이 모두 있으면) 다른 공간 이모지, 유니코드 분류·검색.

import { Search } from 'lucide-react';
import { useMemo, useState } from 'react';
import type { EmojiView, SpaceView } from '../api/types';
import { t } from '../i18n';
import { formatEmoji } from '../lib/emoji';
import { EMOJI_CATEGORIES, searchUnicodeEmoji } from '../lib/emojiData';
import { canUseExternalEmoji, cx } from '../lib/misc';
import { useStore } from '../store/store';
import { CustomEmoji } from './common';

// 이모지 분류 이름(lib/emojiData.ts는 한국어 원문뿐이라 id로 번역을 고릅니다).
const CATEGORY_LABELS: Record<string, string> = {
  smileys: t('표정'),
  gestures: t('손짓·사람'),
  hearts: t('하트·기호'),
  nature: t('동물·자연'),
  food: t('음식'),
  activity: t('활동·사물'),
};

interface Group {
  id: string;
  label: string;
  custom?: EmojiView[];
  unicode?: string[];
}

export function EmojiPicker({ space, onPick }: { space?: string; onPick: (emoji: string) => void }) {
  const views = useStore((s) => s.views);
  const [q, setQ] = useState('');
  const [tab, setTab] = useState<string>('custom');
  const current: SpaceView | undefined = space ? views[space] : undefined;

  const groups = useMemo<Group[]>(() => {
    const out: Group[] = [];
    if (current && current.emojis.length) out.push({ id: 'custom', label: current.settings.name, custom: current.emojis });
    if (canUseExternalEmoji(current)) {
      const others = Object.values(views)
        .filter((v) => v.space !== space)
        .flatMap((v) => v.emojis)
        .filter((e, i, arr) => arr.findIndex((x) => x.id === e.id) === i);
      if (others.length) out.push({ id: 'external', label: t('다른 공간'), custom: others });
    }
    for (const c of EMOJI_CATEGORIES) out.push({ id: c.id, label: CATEGORY_LABELS[c.id] ?? c.label, unicode: c.emojis.map((e) => e.char) });
    return out;
  }, [views, current, space]);

  const query = q.trim().toLowerCase();
  const results = useMemo(() => {
    if (!query) return null;
    const custom = groups.flatMap((g) => g.custom ?? []).filter((e) => e.name.toLowerCase().includes(query));
    const uni = searchUnicodeEmoji(query, 60).map((e) => e.char);
    return { custom, uni };
  }, [groups, query]);

  const shown = results ? null : groups.find((g) => g.id === tab) ?? groups[0];

  return (
    <div className="emoji-picker" onMouseDown={(e) => e.stopPropagation()}>
      <div className="ep-search">
        <Search size={14} />
        <input autoFocus placeholder={t('이모지 찾기 (예: 하트, fire, yangpa)')} value={q} onChange={(e) => setQ(e.target.value)} />
      </div>
      {!results && (
        <div className="ep-tabs">
          {groups.map((g) => (
            <button key={g.id} type="button" className={cx('ep-tab', shown?.id === g.id && 'active')} onClick={() => setTab(g.id)} title={g.label}>
              {g.custom ? g.custom[0] ? <CustomEmoji id={g.custom[0].id} name={g.custom[0].name} size={18} /> : '★' : g.unicode?.[0]}
            </button>
          ))}
        </div>
      )}
      <div className="ep-body">
        {results ? (
          <>
            {results.custom.length > 0 && <div className="ep-label">{t('커스텀')}</div>}
            <div className="ep-grid">
              {results.custom.map((e) => (
                <button key={e.id} type="button" className="ep-item" title={`:${e.name}:`} onClick={() => onPick(formatEmoji(e.name, e.id, e.animated))}>
                  <CustomEmoji id={e.id} name={e.name} size={26} />
                </button>
              ))}
            </div>
            <div className="ep-label">{t('유니코드')}</div>
            <div className="ep-grid">
              {results.uni.map((c) => (
                <button key={c} type="button" className="ep-item uni" onClick={() => onPick(c)}>
                  {c}
                </button>
              ))}
            </div>
            {results.custom.length === 0 && results.uni.length === 0 && <div className="ep-none">{t('찾는 이모지가 없습니다')}</div>}
          </>
        ) : shown ? (
          <>
            <div className="ep-label">{shown.label}</div>
            <div className="ep-grid">
              {shown.custom?.map((e) => (
                <button key={e.id} type="button" className="ep-item" title={`:${e.name}:`} onClick={() => onPick(formatEmoji(e.name, e.id, e.animated))}>
                  <CustomEmoji id={e.id} name={e.name} size={26} />
                </button>
              ))}
              {shown.unicode?.map((c) => (
                <button key={c} type="button" className="ep-item uni" onClick={() => onPick(c)}>
                  {c}
                </button>
              ))}
            </div>
          </>
        ) : null}
      </div>
    </div>
  );
}
