// 가상 스크롤 메시지 목록(@tanstack/react-virtual).
// - 화면에 보이는 행(+여유분)만 DOM에 둡니다. 행 높이는 그린 뒤 재서(measureElement) 맞춥니다.
// - anchorTo 'end' + followOnAppend: 맨 아래에 있으면 새 메시지를 따라가고, 위에 이전 페이지를 붙여도 보던 위치를 유지합니다.
// - 이 앱에서 보낸 메시지는 보던 위치와 상관없이 맨 아래로 내립니다(store.sent).
// - 위쪽 끝 근처로 스크롤하면 keyset 페이지(message.list before)를 더 읽습니다.
// - 포럼 글타래는 데몬이 맨 앞 페이지에 게시글 자체를 함께 주므로, 그 메시지를 맨 위 게시글(제목·태그·본문)로 그립니다.

import { useVirtualizer } from '@tanstack/react-virtual';
import { ArrowDown, CloudOff, Hash, Flame, RefreshCw } from 'lucide-react';
import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState, type ReactNode } from 'react';
import type { ChannelView, GapView, MessageView } from '../api/types';
import { t } from '../i18n';
import { formatDay } from '../lib/format';
import { gapKey, buildRows, type Row } from '../lib/rows';
import { cx, hasPerm } from '../lib/misc';
import { chKey, useStore } from '../store/store';
import { Spinner } from './common';
import { MessageItem } from './MessageItem';

const EMPTY_MSGS: MessageView[] = [];
/** 보낸 뒤 이 시간 안에 들어온 내 메시지만 따라 내립니다(600ms 재조회 대비 여유). */
const SENT_FOLLOW_MS = 5000;
const EMPTY_GAPS: GapView[] = [];

function estimate(row: Row | undefined): number {
  if (!row) return 40;
  switch (row.kind) {
    case 'start':
      return 140;
    case 'loading':
      return 48;
    case 'day':
      return 40;
    case 'gap':
      return 64;
    case 'post':
    case 'message': {
      const m = row.message;
      let h = row.kind === 'post' ? 150 : row.grouped ? 26 : 50;
      if (m.reply_to) h += 22;
      h += Math.max(0, Math.ceil(m.text.length / 110) - 1) * 22;
      if (m.attachments.length) h += m.attachments.length * 230;
      if (m.embeds.length) h += 140;
      if (m.reactions.length) h += 30;
      return h;
    }
  }
}

export function MessageList({
  space,
  channel,
  thread,
  channelView,
  postHeader,
  onEditPost,
}: {
  space: string;
  channel: string;
  thread?: string;
  channelView?: ChannelView;
  /** 포럼 글타래: 게시글 본문 위에 그릴 제목·태그. */
  postHeader?: ReactNode;
  /** 포럼 글타래: 게시글의 '수정'을 누르면 여는 수정 페이지(없으면 글 안에서 고침). */
  onEditPost?: () => void;
}) {
  const key = chKey(space, channel, thread);
  const state = useStore((s) => s.channels[key]);
  const view = useStore((s) => s.views[space]);
  const me = useStore((s) => s.me);
  const editing = useStore((s) => s.editing);
  const loadOlder = useStore((s) => s.loadOlder);
  const fillGap = useStore((s) => s.fillGap);
  const toast = useStore((s) => s.toast);
  const volatile = channelView?.settings.security === 'volatile';

  const messages = state?.messages ?? EMPTY_MSGS;
  const gaps = state?.gaps ?? EMPTY_GAPS;
  const reachedStart = !!state?.reachedStart;
  const loadingOlder = !!state?.loadingOlder;
  const rows = useMemo(
    () => buildRows({ messages, gaps, reachedStart, loadingOlder, post: thread }),
    [messages, gaps, reachedStart, loadingOlder, thread],
  );
  const members = useMemo(() => new Map((view?.members ?? []).map((m) => [m.user, m])), [view?.members]);
  const roles = view?.roles ?? [];
  const byId = useMemo(() => {
    const map = new Map<string, MessageView>();
    for (const m of messages) map.set(m.id, m);
    return map;
  }, [messages]);
  // 만료된 연락처와의 1대1 대화는 읽기만 합니다(답장·반응·고치기 단추를 감춤).
  const expired = useStore((s) => s.friends.some((f) => f.space === space && f.kind === 'expired'));
  const canManage = hasPerm(view, 'manage_messages') && !expired;
  const canReact = hasPerm(view, 'add_reactions') && !expired;

  const parentRef = useRef<HTMLDivElement>(null);
  const rowsRef = useRef(rows);
  rowsRef.current = rows;
  // 행이 바뀔 때만 새 함수 → 가상화가 측정 표를 다시 만듭니다(스크롤 중에는 그대로).
  const getItemKey = useCallback((i: number) => rows[i]?.key ?? i, [rows]);
  const estimateSize = useCallback((i: number) => estimate(rows[i]), [rows]);

  const virtualizer = useVirtualizer({
    count: rows.length,
    getScrollElement: () => parentRef.current,
    estimateSize,
    getItemKey,
    overscan: 10,
    anchorTo: 'end',
    followOnAppend: true,
    scrollEndThreshold: 48,
  });

  // 처음 열면 맨 아래로
  const initialised = useRef(false);
  useLayoutEffect(() => {
    if (initialised.current || !state?.loaded || rows.length === 0) return;
    initialised.current = true;
    virtualizer.scrollToIndex(rows.length - 1, { align: 'end' });
  }, [state?.loaded, rows.length, virtualizer]);

  // 맨 아래에 붙어 있었는지(스크롤할 때만 갱신). 입력창이 커져 목록 칸이 줄어도 스크롤 사건은 없으므로 줄기 전 상태가 남습니다.
  const pinned = useRef(true);
  const onScroll = () => {
    const el = parentRef.current;
    if (!el) return;
    pinned.current = el.scrollHeight - el.clientHeight - el.scrollTop <= 48;
    if (el.scrollTop < 900 && initialised.current) void loadOlder(space, channel, thread);
  };

  // 답장 띠·여러 줄 입력·첨부로 입력창이 커지면 목록 칸이 줄어 끝에서 밀려납니다. 맨 아래에 있었으면 다시 붙여서
  // 마지막 메시지가 가려지지 않고 새 메시지도 계속 따라가게 합니다.
  useEffect(() => {
    const el = parentRef.current;
    if (!el || !state?.loaded || typeof ResizeObserver === 'undefined') return;
    let height = el.clientHeight;
    const ro = new ResizeObserver(() => {
      if (el.clientHeight === height) return;
      height = el.clientHeight;
      if (pinned.current) virtualizer.scrollToEnd();
    });
    ro.observe(el);
    return () => ro.disconnect();
  }, [state?.loaded, virtualizer]);

  // 화면을 다 채우지 못하면 이전 페이지를 더 읽습니다.
  useEffect(() => {
    const el = parentRef.current;
    if (!el || !state?.loaded || reachedStart || loadingOlder) return;
    if (virtualizer.getTotalSize() < el.clientHeight + 200) void loadOlder(space, channel, thread);
  }, [state?.loaded, reachedStart, loadingOlder, rows.length, virtualizer, loadOlder, space, channel, thread]);

  const lastCopyWarn = useRef(0);
  const onCopy = () => {
    if (!volatile) return;
    const now = Date.now();
    if (now - lastCopyWarn.current < 15000) return;
    lastCopyWarn.current = now;
    toast('warn', t('복사한 내용은 클립보드(와 클립보드 기록 앱)에 남습니다. 휘발성 보장 밖입니다.'));
  };

  // 찾아 간 메시지를 잠깐 밝힙니다.
  const [flash, setFlash] = useState<string | null>(null);
  useEffect(() => {
    if (!flash) return;
    const timer = setTimeout(() => setFlash(null), 2000);
    return () => clearTimeout(timer);
  }, [flash]);

  const jumpTo = useCallback(
    (id: string) => {
      const idx = rowsRef.current.findIndex((r) => (r.kind === 'message' || r.kind === 'post') && r.message.id === id);
      if (idx >= 0) {
        virtualizer.scrollToIndex(idx, { align: 'center' });
        setFlash(id);
      }
    },
    [virtualizer],
  );

  // 다른 화면(신고 목록)에서 고른 메시지를 찾아 보여 줍니다. 아직 없으면 이전 페이지를 더 읽습니다(최대 20쪽).
  const jump = useStore((s) => (s.jump?.key === key ? s.jump : null));
  const jumpTries = useRef(0);
  useEffect(() => {
    if (!jump || !state?.loaded) return;
    const idx = rows.findIndex((r) => (r.kind === 'message' || r.kind === 'post') && r.message.id === jump.id);
    if (idx >= 0) {
      virtualizer.scrollToIndex(idx, { align: 'center' });
      setFlash(jump.id);
      jumpTries.current = 0;
      useStore.setState({ jump: null });
    } else if (reachedStart || jumpTries.current >= 20) {
      jumpTries.current = 0;
      useStore.setState({ jump: null });
      toast('info', t('이 기기에 그 메시지가 없습니다(지워졌거나 아직 받지 못함)'));
    } else if (!loadingOlder) {
      jumpTries.current += 1;
      void loadOlder(space, channel, thread);
    }
  }, [jump, rows, state?.loaded, reachedStart, loadingOlder, virtualizer, loadOlder, space, channel, thread, toast]);

  // 이 앱에서 보낸 메시지는 보던 위치와 상관없이 맨 아래로 내립니다. followOnAppend는 이미 끝(48px 이내)일 때만 따라가서,
  // 위로 올려 둔 채 보내거나 입력창이 커져(답장 띠·여러 줄·첨부) 끝에서 밀려나 있으면 보낸 메시지가 아래에 가려졌습니다.
  // 내 메시지 알림과 message.send 응답 중 무엇이 먼저 올지 모르므로 신호를 받았을 때와 그 메시지가 들어왔을 때 내립니다.
  const sent = useStore((s) => (s.sent?.key === key ? s.sent : null));
  const sentDone = useRef<string | null>(null);
  useLayoutEffect(() => {
    if (!sent || !state?.loaded || sentDone.current === sent.id) return;
    if (Date.now() - sent.at > SENT_FOLLOW_MS) {
      sentDone.current = sent.id;
      return;
    }
    virtualizer.scrollToEnd();
    if (byId.has(sent.id)) sentDone.current = sent.id;
  }, [sent, byId, state?.loaded, virtualizer]);

  if (!state?.loaded) {
    return (
      <div className="msg-scroll loading">
        <Spinner size={28} />
      </div>
    );
  }
  if (state.error && messages.length === 0) {
    return (
      <div className="msg-scroll loading">
        <div className="load-error">
          <CloudOff size={28} />
          <div>{t('메시지를 읽지 못했습니다: {message}', { message: state.error })}</div>
        </div>
      </div>
    );
  }

  const items = virtualizer.getVirtualItems();
  // 가상화가 스크롤·측정마다 다시 그리므로 렌더 중에 계산해도 항상 최신입니다.
  const atEnd = virtualizer.isAtEnd(120);
  const editingId = editing?.key === key ? editing.id : null;

  const item = (m: MessageView, grouped: boolean, post?: boolean) => (
    <MessageItem
      m={m}
      grouped={grouped}
      space={space}
      channel={channel}
      msgKey={key}
      members={members}
      roles={roles}
      me={me}
      replied={m.reply_to ? byId.get(m.reply_to) ?? null : undefined}
      volatile={volatile}
      canManage={canManage}
      canReact={canReact}
      isEditing={editingId === m.id}
      readOnly={expired}
      onJump={jumpTo}
      post={post}
      noEdit={!!channelView?.no_edit}
      noDelete={!!channelView?.no_delete}
      onEdit={post ? onEditPost : undefined}
    />
  );

  const renderRow = (row: Row) => {
    switch (row.kind) {
      case 'start':
        return (
          <div className="ch-start">
            <div className={volatile ? 'ch-start-icon volatile' : 'ch-start-icon'}>{volatile ? <Flame size={34} /> : <Hash size={34} />}</div>
            <h2>{thread ? t('글타래의 시작') : t('#{name}에 오신 것을 환영합니다', { name: channelView?.settings.name ?? '' })}</h2>
            <p>
              {volatile
                ? t('이 채널의 메시지는 디스크에 저장되지 않고, 수명이 지나면 모든 참가자 기기에서 지워집니다.')
                : t('이 채널의 맨 처음입니다. 기록은 참가자 기기와 시더가 나눠 들고 있습니다.')}
            </p>
          </div>
        );
      case 'loading':
        return (
          <div className="row-loading">
            <Spinner /> {t('이전 메시지를 불러오는 중…')}
          </div>
        );
      case 'day':
        return (
          <div className="day-sep">
            <span>{formatDay(row.at)}</span>
          </div>
        );
      case 'gap': {
        const filling = state.filling.includes(gapKey(row.gap));
        return (
          <div className="gap-row">
            <CloudOff size={18} />
            <div className="gap-text">
              <strong>{t('받지 않은 기록')}</strong>
              <span>{t('이 사이에 아직 받지 못한 메시지가 있습니다. 이 구간을 가진 시더가 온라인이면 받아 올 수 있습니다.')}</span>
            </div>
            <button type="button" className="btn small" disabled={filling} onClick={() => void fillGap(space, channel, row.gap)}>
              {filling ? <Spinner size={14} /> : <RefreshCw size={14} />} {filling ? t('받는 중') : t('기록 받기')}
            </button>
          </div>
        );
      }
      case 'post':
        return (
          <div className="forum-post">
            {postHeader}
            {item(row.message, false, true)}
            <div className="forum-post-sep">{t('댓글')}</div>
          </div>
        );
      case 'message':
        return item(row.message, row.grouped);
    }
  };

  return (
    <div className="msg-list-wrap">
      <div className="msg-scroll" ref={parentRef} onScroll={onScroll} onCopy={onCopy}>
        <div className="msg-inner" style={{ height: virtualizer.getTotalSize() }}>
          {items.map((vi) => {
            const row = rows[vi.index];
            if (!row) return null;
            return (
              <div
                key={vi.key}
                data-index={vi.index}
                ref={virtualizer.measureElement}
                className={cx('vrow', flash && (row.kind === 'message' || row.kind === 'post') && row.message.id === flash && 'flash')}
                style={{ transform: `translateY(${vi.start}px)` }}
              >
                {renderRow(row)}
              </div>
            );
          })}
        </div>
      </div>
      {!atEnd && (
        <button type="button" className="jump-bottom" onClick={() => virtualizer.scrollToIndex(rows.length - 1, { align: 'end', behavior: 'smooth' })}>
          {t('최근 메시지로')} <ArrowDown size={14} />
        </button>
      )}
    </div>
  );
}
