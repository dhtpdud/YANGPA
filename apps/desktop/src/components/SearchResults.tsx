// 검색 결과 패널: 한 쪽씩(오른쪽 위 입력칸으로 쪽 크기), 맨 아래 쪽 번호. 쪽은 데몬에서 비동기로 받고, 받는 동안 이전 쪽을 그대로 둡니다.

import { ChevronLeft, ChevronRight, X } from 'lucide-react';
import { useEffect, useRef, useState } from 'react';
import type { SpaceView } from '../api/types';
import { currentLang, t } from '../i18n';
import { cx } from '../lib/misc';
import { pageList } from '../lib/search';
import { SEARCH_PAGE_MAX, SEARCH_PAGE_MIN, useStore } from '../store/store';
import { Spinner } from './common';
import { MessageItem } from './MessageItem';

const num = (n: number) => n.toLocaleString(currentLang());

export function SearchResults({ space }: { space: SpaceView }) {
  const search = useStore((s) => s.search);
  const pageSize = useStore((s) => s.prefs.searchPageSize);
  const jumpToMessage = useStore((s) => s.jumpToMessage);
  const clearSearch = useStore((s) => s.clearSearch);
  const searchPage = useStore((s) => s.searchPage);
  const me = useStore((s) => s.me);
  const body = useRef<HTMLDivElement>(null);
  // 다른 쪽을 받으면 목록 맨 위부터 보이게 합니다.
  const shownOffset = search?.offset;
  const shownFirst = search?.results[0]?.id;
  useEffect(() => {
    if (body.current) body.current.scrollTop = 0;
  }, [shownOffset, shownFirst]);
  if (!search) return null;
  const members = new Map(space.members.map((m) => [m.user, m]));
  const pages = Math.max(1, Math.ceil(search.total / pageSize));
  const cur = Math.min(pages, Math.floor(search.offset / pageSize) + 1);
  const go = (page: number) => {
    if (page < 1 || page > pages || page === cur) return;
    void searchPage((page - 1) * pageSize);
  };
  const first = search.results.length ? search.offset + 1 : 0;
  const last = search.offset + search.results.length;
  const empty = !search.loading && !search.error && search.total === 0;
  return (
    <aside className="search-panel">
      <div className="sp-head">
        <span className="grow sp-title">
          {t('“{query}” 검색 결과', { query: search.query })}
          {!search.error && (search.total > 0 || !search.loading) && (
            <span className="sp-count">
              {' '}
              {t('{count}개', { count: num(search.total), n: search.total })}
              {search.tookMs !== undefined && (
                <span className="muted"> · {t('{sec}초', { sec: (search.tookMs / 1000).toFixed(search.tookMs < 1000 ? 3 : 1) })}</span>
              )}
            </span>
          )}
        </span>
        {search.loading && <Spinner size={14} />}
        <PageSizeInput />
        <button type="button" className="icon-btn small" title={t('검색 닫기')} onClick={clearSearch}>
          <X size={14} />
        </button>
      </div>
      {search.author && <div className="sp-sub muted">{t('{name}님이 쓴 글(익명 글 제외)', { name: search.author })}</div>}
      <div className={cx('sp-body', search.loading && search.results.length > 0 && 'sp-stale')} ref={body} aria-busy={search.loading}>
        {search.loading && search.results.length === 0 && <Spinner />}
        {search.error && <div className="muted pad">{search.error}</div>}
        {empty && <div className="muted pad">{t('결과가 없습니다. 휘발성 채널은 검색하지 않습니다.')}</div>}
        {search.results.map((m) => {
          const ch = m.channel ? space.channels.find((c) => c.channel === m.channel) : undefined;
          // 포럼은 게시글을 열고 그 안에서 찾습니다(댓글이면 그 글타래, 게시글이면 자기 자신).
          const post = ch?.settings.kind === 'forum' ? (m.thread ?? m.id) : undefined;
          const open = () => m.space && m.channel && void jumpToMessage(m.space, m.channel, m.id, post);
          // 메시지 안에 버튼(반응·첨부)이 있어 <button>으로 감싸면 대화형 요소가 겹칩니다. div + 키보드 처리로 대신합니다.
          return (
            <div
              key={m.id}
              role="button"
              tabIndex={0}
              className="sp-item"
              onClick={open}
              onKeyDown={(e) => {
                if (e.target !== e.currentTarget) return;
                if (e.key === 'Enter' || e.key === ' ') {
                  e.preventDefault();
                  open();
                }
              }}
            >
              {ch && <div className="sp-ch">#{ch.settings.name}</div>}
              <MessageItem
                m={m}
                grouped={false}
                space={m.space ?? space.space}
                channel={m.channel ?? ''}
                msgKey="search"
                members={members}
                roles={space.roles}
                me={me}
                volatile={false}
                canManage={false}
                canReact={false}
                isEditing={false}
                readOnly
              />
            </div>
          );
        })}
      </div>
      {search.total > 0 && (
        <nav className="sp-pager" aria-label={t('검색 결과 쪽')}>
          <div className="sp-range muted">
            {num(first)}–{num(last)} / {num(search.total)} · {t('{cur}/{pages}쪽', { cur: num(cur), pages: num(pages) })}
          </div>
          {pages > 1 && (
            <div className="sp-pages">
              <button type="button" className="icon-btn small" title={t('이전 쪽')} disabled={cur <= 1} onClick={() => go(cur - 1)}>
                <ChevronLeft size={14} />
              </button>
              {pageList(cur, pages).map((p, i) =>
                p === 'gap' ? (
                  <span key={`gap-${i}`} className="sp-gap">
                    …
                  </span>
                ) : (
                  <button
                    key={p}
                    type="button"
                    className={cx('sp-page', p === cur && 'active')}
                    aria-current={p === cur ? 'page' : undefined}
                    onClick={() => go(p)}
                  >
                    {num(p)}
                  </button>
                ),
              )}
              <button type="button" className="icon-btn small" title={t('다음 쪽')} disabled={cur >= pages} onClick={() => go(cur + 1)}>
                <ChevronRight size={14} />
              </button>
            </div>
          )}
        </nav>
      )}
    </aside>
  );
}

/** 쪽 크기 입력칸. 입력하는 동안은 그대로 두고, Enter나 칸을 벗어날 때 범위 안으로 맞춰 적용합니다. */
function PageSizeInput() {
  const size = useStore((s) => s.prefs.searchPageSize);
  const setSize = useStore((s) => s.setSearchPageSize);
  const [text, setText] = useState(String(size));
  useEffect(() => setText(String(size)), [size]);
  const commit = () => {
    const n = Number(text);
    if (text.trim() === '' || !Number.isFinite(n)) setText(String(size));
    else {
      const next = Math.min(SEARCH_PAGE_MAX, Math.max(SEARCH_PAGE_MIN, Math.round(n)));
      setText(String(next));
      void setSize(next);
    }
  };
  return (
    <label className="sp-size" title={t('한 쪽에 보일 결과 수({min}–{max})', { min: SEARCH_PAGE_MIN, max: SEARCH_PAGE_MAX })}>
      <span className="muted">{t('쪽당')}</span>
      <input
        type="number"
        inputMode="numeric"
        min={SEARCH_PAGE_MIN}
        max={SEARCH_PAGE_MAX}
        value={text}
        aria-label={t('쪽당 결과 수')}
        onChange={(e) => setText(e.target.value)}
        onBlur={commit}
        onKeyDown={(e) => {
          if (e.key === 'Enter') {
            e.preventDefault();
            commit();
          }
        }}
      />
    </label>
  );
}
