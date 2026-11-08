// SPDX-License-Identifier: LicenseRef-YANGPA-Supporter
// 채널 콘텐츠 뷰어(X-Twitter-Viewer를 본뜸): 채널의 글·이미지·영상·오디오·파일을 한 화면에서 ↑/↓로 넘겨 봅니다.
// 불러온 메시지(store)만 쓰고, 끝에 가까워지면 이전 메시지를 더 읽습니다(loadOlder). 받지 않은 첨부는 '원본 받기'를 누를 때만
// 받습니다: 보이거나 다음 차례라는 이유로 blob.fetch·blob.status를 부르면 지금 보는 첨부가 멤버 기기에 드러납니다.
// 휘발성 채널은 메모리 blob: URL로만 그리고(useBlobUrl), 보던 위치는 어디에도 남기지 않으며, 캡처가 감지되면 뷰어도 가립니다.

import { Download, FileIcon, ImageOff, Maximize, MoveVertical, Music, Pin, PinOff, Play, Settings, X } from 'lucide-react';
import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState, type CSSProperties, type MouseEvent, type ReactNode } from 'react';
import { ANONYMOUS_USER, type AttachmentView, type ChannelId, type MemberView, type MessageView, type SpaceId } from '../api/types';
import { useBlobUrl, useThumbUrl } from '../hooks/media';
import { t } from '../i18n';
import { anonName, isAnonymousMessage, isMine } from '../lib/anon';
import { formatBytes, formatFull } from '../lib/format';
import { clamp, cx, displayName, excerpt } from '../lib/misc';
import { plainName } from '../lib/text';
import { buildViewerItems, imageFit, sanitizeViewer, viewerAvailable, VIEWER_LIMITS, type ViewerItem, type ViewerPrefs } from './viewer';
import { Field } from '../modals/Modal';
import { chKey, useStore } from '../store/store';
import { Attachment, useMediaSave, useSaveAttachment } from '../components/Attachment';
import { AudioWave } from './AudioWave';
import { CaptureShield } from '../components/CaptureShield';
import { Avatar, Spinner } from '../components/common';
import { MessageText } from '../components/MessageText';

/** 남은 항목이 이 수 이하면 이전 메시지를 더 읽습니다. */
const REFILL_AHEAD = 10;
/** 썸네일 줄에 그리는 앞뒤 항목 수(긴 채널에서 단추 수천 개를 그리지 않게, 먼 곳은 위치 막대로). */
const STRIP_SPAN = 30;
const EMPTY: MessageView[] = [];

function typeLabel(type: ViewerItem['type']): string {
  return type === 'text' ? t('글') : type === 'image' ? t('이미지') : type === 'video' ? t('영상') : type === 'audio' ? t('오디오') : t('첨부 파일');
}

/** 메시지 보기의 downloaded는 다시 읽을 때까지 그대로라서, 진행이 끝났으면 받은 것으로 봅니다(Attachment와 같음). */
function useDownloaded(att: AttachmentView): boolean {
  const progress = useStore((s) => s.blobProgress[att.blob]);
  return att.downloaded || (!!progress && progress.total > 0 && progress.have >= progress.total);
}

/** 라이브러리에서 고른 파일처럼 정해진 항목. 메시지마다 `space`·`channel`이 채워져 있어야 합니다(`library.list`). */
export interface ViewerList {
  items: ViewerItem[];
  /** 처음 볼 항목의 키. */
  start?: string;
}

type ViewerProps = { space: SpaceId; channel: ChannelId; list?: undefined; onClose: () => void } | { list: ViewerList; space?: undefined; channel?: undefined; onClose: () => void };

export function ContentViewer(props: ViewerProps) {
  const { list, onClose } = props;
  // 정해진 목록이면 채널 기록을 읽지 않고(이전 메시지 더 읽기 없음), 채널을 떠나도 닫지 않으며, 항목마다 공간·채널이 다릅니다.
  const key = list ? '' : chKey(props.space, props.channel);
  const st = useStore((s) => (list ? undefined : s.channels[key]));
  const v = useStore((s) => s.prefs.viewer);
  const setPrefs = useStore((s) => s.setPrefs);
  const loadOlder = useStore((s) => s.loadOlder);
  const jumpToMessage = useStore((s) => s.jumpToMessage);
  const me = useStore((s) => s.me);
  // 사용자 필터는 크루마다 다른 사람이라 설정 파일에 두지 않고 뷰어가 열려 있는 동안만 씁니다(정해진 목록에는 걸지 않음).
  const [authors, setAuthors] = useState<string[]>([]);
  const items = useMemo(() => list?.items ?? buildViewerItems(st?.messages ?? EMPTY, v, authors), [list, st?.messages, v, authors]);
  // 지금 항목은 키로 따라갑니다(이전 메시지를 더 읽거나 새 메시지가 오면 번호가 밀림). 키가 사라지면(만료·필터) 같은 번호에 머묾.
  const [pos, setPos] = useState<{ key: string | null; index: number }>({ key: list?.start ?? null, index: 0 });
  const found = pos.key ? items.findIndex((i) => i.key === pos.key) : -1;
  const index = found >= 0 ? found : clamp(pos.index, 0, Math.max(0, items.length - 1));
  const item: ViewerItem | undefined = items[index];
  const space = (list ? item?.msg.space : props.space) ?? '';
  const channel = (list ? item?.msg.channel : props.channel) ?? '';
  const view = useStore((s) => s.views[space]);
  const ch = view?.channels.find((c) => c.channel === channel);
  const selectedHere = useStore((s) => s.selected.space === space && s.selected.channel === channel);
  const volatile = !list && ch?.settings.security === 'volatile';
  const members = useMemo(() => new Map((view?.members ?? []).map((m) => [m.user, m])), [view?.members]);
  const [panel, setPanel] = useState(false);
  const [osd, setOsd] = useState<string | null>(null);
  const [stalled, setStalled] = useState(false);
  const [retry, setRetry] = useState(0);
  const rootRef = useRef<HTMLDivElement>(null);
  const stageRef = useRef<HTMLElement>(null);
  const stripRef = useRef<HTMLDivElement>(null);
  /** 이전 항목으로 돌아왔으면 끝부분부터 보입니다. */
  const atEnd = useRef(false);
  /** ↑/↓ 부드러운 스크롤이 가는 곳(연달아 누르면 거기서 이어 감). */
  const scrollTarget = useRef<number | null>(null);
  const osdTimer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);

  const reachedStart = !!list || !!st?.reachedStart;
  // 오래된순은 처음까지 다 읽은 뒤 보여 줍니다(읽는 동안 첫 항목이 계속 바뀌지 않게).
  const reading = v.order === 'oldest' && !reachedStart && !stalled;
  const fit = item?.type === 'image' ? imageFit(v, item.att?.width, item.att?.height) : '';

  // 채널을 떠나거나(선택 바뀜·퇴장·P2P 참여 그만둠·삭제) 메시지 목록이 안 보이게 되면 닫습니다.
  const available = !!list || (selectedHere && viewerAvailable(ch));
  useEffect(() => {
    if (!available) onClose();
  }, [available, onClose]);

  // 남은 항목이 적으면(오래된순이면 처음까지) 이전 메시지를 더 읽습니다. 읽어도 늘지 않으면(실패) 멈추고 다시 시도를 보입니다.
  const want = !!st?.loaded && !reachedStart && !st.loadingOlder && !stalled && (v.order === 'oldest' || items.length - 1 - index <= REFILL_AHEAD);
  useEffect(() => {
    if (!want) return;
    const before = useStore.getState().channels[key]?.messages.length ?? 0;
    void loadOlder(space, channel).then(() => {
      const after = useStore.getState().channels[key];
      if (after && !after.reachedStart && after.messages.length === before) setStalled(true);
    });
  }, [want, key, space, channel, loadOlder]);

  // 맞춘 이미지는 읽기 시작하는 곳(이전으로 왔으면 끝)으로 스크롤합니다. 높이에 맞추면 좌→우, 오른쪽→왼쪽 읽기면 우→좌.
  const place = useCallback(() => {
    const el = stageRef.current;
    if (!el) return;
    scrollTarget.current = null;
    el.scrollTop = fit === 'width' && atEnd.current ? el.scrollHeight : 0;
    el.scrollLeft = fit === 'height' && v.rtl !== atEnd.current ? el.scrollWidth : 0;
  }, [fit, v.rtl]);
  useLayoutEffect(place, [place, item?.key, v.fillPercent]);

  useEffect(() => {
    const prev = document.activeElement as HTMLElement | null;
    const root = rootRef.current;
    root?.focus({ preventScroll: true });
    return () => {
      clearTimeout(osdTimer.current);
      if (root && document.fullscreenElement === root) void document.exitFullscreen().catch(() => undefined);
      prev?.focus?.();
    };
  }, []);

  // 지금 항목이 가운데 오게 썸네일 줄만 옮깁니다(scrollIntoView는 뷰어 틀까지 밀어 올림).
  useEffect(() => {
    const strip = stripRef.current;
    const cur = strip?.querySelector<HTMLElement>('.vw-thumb.active');
    if (strip && cur) strip.scrollTo({ left: Math.max(0, cur.offsetLeft - (strip.clientWidth - cur.offsetWidth) / 2), behavior: 'smooth' });
  }, [index, items.length]);

  const go = (i: number, fromEnd = false) => {
    if (items.length === 0) return;
    const c = clamp(i, 0, items.length - 1);
    atEnd.current = fromEnd;
    setPos({ key: items[c].key, index: c });
  };

  const flash = (text: string) => {
    setOsd(text);
    clearTimeout(osdTimer.current);
    osdTimer.current = setTimeout(() => setOsd(null), 800);
  };

  const toggleFill = () => {
    setPrefs({ viewer: { ...v, fill: !v.fill } });
    flash(v.fill ? t('꽉차게 보기 끔') : t('꽉차게 보기 켬'));
  };

  const toggleFullscreen = () => {
    if (document.fullscreenElement) void document.exitFullscreen().catch(() => undefined);
    else void rootRef.current?.requestFullscreen().catch(() => undefined);
  };

  // 화면보다 큰 이미지는 ↑/↓가 먼저 스크롤하고(높이에 맞추면 옆으로), 끝에 닿으면 거짓을 돌려 이전·다음 항목으로 넘깁니다.
  const scrollImage = (dir: 1 | -1): boolean => {
    const el = stageRef.current;
    if (!el || !fit) return false;
    const across = fit === 'height';
    const step = across && v.rtl ? -dir : dir;
    const prop = across ? 'scrollLeft' : 'scrollTop';
    if (scrollTarget.current !== null) el[prop] = scrollTarget.current;
    const at = el[prop];
    const max = across ? el.scrollWidth - el.clientWidth : el.scrollHeight - el.clientHeight;
    if ((step < 0 ? at : max - at) < 2) return false;
    const value = clamp(at + step * v.scrollStep, 0, max);
    scrollTarget.current = value;
    el.scrollTo({ [across ? 'left' : 'top']: value, behavior: 'smooth' });
    return true;
  };

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.altKey || e.ctrlKey || e.metaKey || e.isComposing) return;
      // 위에 다른 창(휘발성 저장 경고 등)이 떠 있으면 그 창 몫입니다.
      const overlays = document.querySelectorAll('.overlay');
      if (overlays[overlays.length - 1] !== rootRef.current) return;
      const el = e.target instanceof HTMLElement ? e.target : null;
      const typing =
        !!el &&
        (el.tagName === 'TEXTAREA' ||
          el.tagName === 'SELECT' ||
          el.isContentEditable ||
          (el.tagName === 'INPUT' && !['checkbox', 'range'].includes((el as HTMLInputElement).type)));
      if (typing) {
        if (e.key !== 'Escape') return;
        setPanel(false);
      } else {
        // 단추에서 누른 Enter·Space는 그 단추를 누릅니다.
        if ((e.key === 'Enter' || e.key === ' ') && el?.closest('button, a')) return;
        const video = stageRef.current?.querySelector<HTMLMediaElement>('video, audio');
        // e.code도 보는 것은 한글 입력기가 켜져 있어도 F를 받기 위해서입니다.
        switch (e.code === 'KeyF' ? 'f' : e.key) {
          case 'ArrowUp':
          case 'ArrowDown': {
            const dir = e.key === 'ArrowUp' ? -1 : 1;
            if (!scrollImage(dir)) go(index + dir, dir < 0);
            break;
          }
          case 'ArrowLeft':
          case 'ArrowRight': {
            if (!video) return;
            const step = v.seek * (e.key === 'ArrowLeft' ? -1 : 1);
            video.currentTime = clamp(video.currentTime + step, 0, Number.isFinite(video.duration) ? video.duration : Infinity);
            flash(t('{n}초', { n: step > 0 ? `+${step}` : String(step) }));
            break;
          }
          case ' ':
            if (!video) return;
            if (video.paused) void video.play()?.catch(() => undefined);
            else video.pause();
            flash(video.paused ? t('일시 정지') : t('재생'));
            break;
          case 'f':
            toggleFill();
            break;
          case 'Enter':
            toggleFullscreen();
            break;
          case 'Escape':
            if (panel) setPanel(false);
            else if (!document.fullscreenElement) onClose();
            break;
          default:
            return;
        }
      }
      e.preventDefault();
      e.stopPropagation();
    };
    window.addEventListener('keydown', onKey, true);
    return () => window.removeEventListener('keydown', onKey, true);
  });

  // 마우스로 누른 막대 단추가 포커스를 쥐면 키를 누를 때 그 막대가 계속 보여서(:focus-visible) 포커스를 뷰어에 남깁니다.
  const keepFocus = (e: MouseEvent) => (e.target as HTMLElement).closest('button') && e.preventDefault();
  const refocus = () => rootRef.current?.focus({ preventScroll: true });

  const author = !item
    ? ''
    : isAnonymousMessage(item.msg)
      ? anonName(item.msg)
      : displayName(members.get(item.msg.author), isMine(item.msg, me) ? t('나') : t('알 수 없는 사용자'));
  // 익명 글은 누구의 사진도 찾지 않게 익명 자리로 그립니다(가명마다 바탕색만 다름).
  const authorAvatar = (size: number) =>
    item && (
      <Avatar
        id={isAnonymousMessage(item.msg) ? ANONYMOUS_USER : item.msg.author}
        name={author}
        size={size}
        color={members.get(item.msg.author)?.profile.theme_color}
      />
    );
  const sub = item ? [formatFull(item.msg.created_at), typeLabel(item.type), item.att && plainName(item.att.name)].filter(Boolean).join(' · ') : '';
  const from = Math.max(0, index - STRIP_SPAN);

  let status: ReactNode = null;
  if (reading) {
    status = (
      <p className="row gap-s">
        <Spinner size={16} /> {t('처음부터 읽는 중… 메시지 {n}개', { n: st?.messages.length ?? 0 })}
      </p>
    );
  } else if (items.length === 0) {
    if (st?.error) status = <p>{st.error}</p>;
    else if (stalled)
      status = (
        <>
          <p>{t('이전 메시지를 더 읽지 못했습니다')}</p>
          <button type="button" className="btn primary" onClick={() => setStalled(false)}>
            {t('다시 시도')}
          </button>
        </>
      );
    else if (!list && (!st?.loaded || !reachedStart)) status = <Spinner size={28} />;
    else
      status = (
        <>
          <p>{t('보여 줄 콘텐츠가 없습니다')}</p>
          <button type="button" className="btn primary" onClick={() => setPanel(true)}>
            {t('보기 설정')}
          </button>
        </>
      );
  }

  return (
    // 밝은 테마에서도 사진·영상이 잘 보이게 늘 어두운 테마로 그립니다.
    <div ref={rootRef} className={cx('overlay viewer', v.pinStrip && 'strip-pinned')} data-theme="dark" role="dialog" aria-modal aria-label={t('콘텐츠 뷰어')} tabIndex={-1}>
      <main
        ref={stageRef}
        className="vw-stage"
        data-fit={fit}
        style={{ '--vw-fill': `${v.fillPercent}%` } as CSSProperties}
        onWheel={() => (scrollTarget.current = null)}
        onPointerDown={() => (scrollTarget.current = null)}
        onDoubleClick={(e) => !(e.target as HTMLElement).closest('.vw-text, .vw-card, button, video') && toggleFullscreen()}
      >
        {item &&
          !reading &&
          (item.type === 'text' ? (
            <article className="vw-text">
              <div className="vw-text-head">
                {authorAvatar(28)}
                <strong>{author}</strong>
                <span className="muted">{formatFull(item.msg.created_at)}</span>
              </div>
              <MessageText text={item.msg.text} members={members} me={me} />
            </article>
          ) : item.type === 'image' || item.type === 'video' ? (
            <ViewerMedia
              key={`${item.key}:${retry}`}
              item={item}
              space={space}
              volatile={volatile || item.msg.volatile}
              onLoad={place}
              onRetry={() => setRetry((r) => r + 1)}
            />
          ) : (
            <ViewerCard key={item.key} item={item} space={space} channel={channel} volatile={volatile || item.msg.volatile} />
          ))}
      </main>
      {status && <div className="vw-status">{status}</div>}
      {osd && <div className="vw-osd">{osd}</div>}
      <header className="vw-bar vw-top" onMouseDown={keepFocus}>
        <div className="vw-meta">
          {item ? (
            <button
              type="button"
              className="vw-title"
              title={t('채팅에서 보기')}
              onClick={() => {
                // 포럼은 게시글을 열고 그 안에서 찾습니다(라이브러리 목록에는 포럼 첨부도 있음).
                void jumpToMessage(space, channel, item.msg.id, ch?.settings.kind === 'forum' ? (item.msg.thread ?? item.msg.id) : undefined);
                onClose();
              }}
            >
              {authorAvatar(20)}
              <span>{author}</span>
            </button>
          ) : (
            <span className="vw-title">#{ch?.settings.name}</span>
          )}
          <div className="vw-sub">{sub}</div>
          {item && item.type !== 'text' && item.msg.text.trim() && <div className="vw-caption">{excerpt(item.msg.text, 300)}</div>}
        </div>
        <span className="vw-counter">
          {items.length ? index + 1 : 0} / {items.length}
          {reachedStart ? '' : '+'}
          {st?.loadingOlder && <Spinner size={12} />}
          {stalled && items.length > 0 && (
            <button type="button" className="link-btn" title={t('이전 메시지를 더 읽지 못했습니다')} onClick={() => setStalled(false)}>
              {t('다시 시도')}
            </button>
          )}
        </span>
        <button type="button" className={cx('icon-btn', v.fill && 'active')} aria-pressed={v.fill} title={`${t('꽉차게 보기')} (F)`} onClick={toggleFill}>
          <MoveVertical size={18} />
        </button>
        <button type="button" className={cx('icon-btn', panel && 'active')} aria-pressed={panel} title={t('보기 설정')} onClick={() => setPanel(!panel)}>
          <Settings size={18} />
        </button>
        <button type="button" className="icon-btn" title={`${t('전체 화면으로 보기')} (Enter)`} onClick={toggleFullscreen}>
          <Maximize size={18} />
        </button>
        <button type="button" className="icon-btn" title={t('닫기')} onClick={onClose}>
          <X size={20} />
        </button>
      </header>
      <footer className="vw-bar vw-bottom" onMouseDown={keepFocus}>
        <div className="row gap-s">
          <input
            type="range"
            className="vw-slider"
            min={0}
            max={Math.max(0, items.length - 1)}
            value={index}
            aria-label={t('위치')}
            onChange={(e) => go(Number(e.target.value))}
            onPointerUp={refocus}
          />
          <button
            type="button"
            className={cx('icon-btn small', v.pinStrip && 'active')}
            aria-pressed={v.pinStrip}
            title={v.pinStrip ? t('미리보기 고정 풀기') : t('미리보기 고정')}
            onClick={() => setPrefs({ viewer: { ...v, pinStrip: !v.pinStrip } })}
          >
            {v.pinStrip ? <PinOff size={14} /> : <Pin size={14} />}
          </button>
        </div>
        <div ref={stripRef} className="vw-strip">
          {items.slice(from, index + STRIP_SPAN + 1).map((it, i) => (
            <Thumb key={it.key} item={it} volatile={volatile} active={from + i === index} onClick={() => go(from + i)} />
          ))}
        </div>
        <div className="vw-hint">{t('↑↓ 이전·다음 · ←→ 영상 {n}초 이동 · Space 재생·정지 · F 꽉차게 보기 · Enter 전체 화면 · Esc 닫기', { n: v.seek })}</div>
      </footer>
      {panel && (
        <ViewerSettings
          value={v}
          fixed={!!list}
          authors={authors}
          members={view?.members ?? []}
          messages={st?.messages ?? EMPTY}
          onSave={(next, picked) => {
            setPrefs({ viewer: next });
            setAuthors(picked);
            setPanel(false);
          }}
          onClose={() => setPanel(false)}
        />
      )}
      {volatile && <CaptureShield />}
    </div>
  );
}

function Thumb({ item, volatile, active, onClick }: { item: ViewerItem; volatile: boolean; active: boolean; onClick: () => void }) {
  const thumb = useThumbUrl(item.att?.thumbnail_base64, volatile);
  const label = item.att ? plainName(item.att.name) : excerpt(item.msg.text, 40);
  return (
    <button type="button" className={cx('vw-thumb', active && 'active')} title={label} aria-current={active} onClick={onClick}>
      {thumb ? (
        <img src={thumb} alt="" decoding="async" draggable={false} />
      ) : item.type === 'text' ? (
        <span className="vw-thumb-text">{label}</span>
      ) : item.type === 'audio' || item.type === 'file' ? (
        <span className="vw-thumb-file">
          {item.type === 'audio' ? <Music size={16} /> : <FileIcon size={16} />}
          <span>{label}</span>
        </span>
      ) : (
        <ImageOff size={18} />
      )}
      {item.type === 'video' && (
        <span className="vw-badge">
          <Play size={10} fill="currentColor" />
        </span>
      )}
    </button>
  );
}

/** 오디오·파일: 채팅의 첨부 카드 그대로(받기·저장·보관·다시 잇기·받는 사람), 받은 오디오는 그 위에 파형 재생기. */
function ViewerCard({ item, space, channel, volatile }: { item: ViewerItem; space: SpaceId; channel: ChannelId; volatile: boolean }) {
  const att = item.att!;
  const myDevice = useStore((s) => s.myDevice);
  const downloaded = useDownloaded(att);
  return (
    <div className="vw-card">
      {item.type === 'audio' && downloaded && <AudioWave space={space} att={att} keyFrom={item.msg.id} volatile={volatile} />}
      <Attachment
        att={att}
        space={space}
        channel={channel}
        messageId={item.msg.id}
        msgKey={chKey(space, channel)}
        volatile={volatile}
        own={!!myDevice && item.msg.author_device === myDevice}
        noPlayer
      />
    </div>
  );
}

/** 이미지·영상 하나. 받지 않았으면 썸네일과 '원본 받기'(누를 때만 받음), 받았으면 원본. */
function ViewerMedia({
  item,
  space,
  volatile,
  onLoad,
  onRetry,
}: {
  item: ViewerItem;
  space: SpaceId;
  volatile: boolean;
  onLoad: () => void;
  onRetry: () => void;
}) {
  const att = item.att!;
  const client = useStore((s) => s.client);
  const call = useStore((s) => s.call);
  const progress = useStore((s) => s.blobProgress[att.blob]);
  const [asked, setAsked] = useState(false);
  const [broken, setBroken] = useState(false);
  // 메시지 보기의 downloaded는 다시 읽을 때까지 그대로라서, 진행이 끝났으면 받은 것으로 봅니다(Attachment와 같음).
  const downloaded = att.downloaded || (!!progress && progress.total > 0 && progress.have >= progress.total);
  const name = plainName(att.name);
  const src = useBlobUrl(downloaded ? { space, blob: att.blob, keyFrom: item.msg.id, mime: att.mime, volatile } : null);
  const thumb = useThumbUrl(att.thumbnail_base64, volatile);
  // 뷰어에서 "다른 이름으로 저장"해도 앱의 저장으로 잇습니다(휘발성은 경고 뒤).
  useMediaSave(src.url, useSaveAttachment(space, att.blob, item.msg.id, name, volatile));
  const videoRef = useRef<HTMLVideoElement>(null);
  useEffect(() => {
    const el = videoRef.current;
    if (!el) return;
    // 소리 있는 자동 재생이 막히면 소리 없이 다시 틉니다.
    el.play()?.catch(() => {
      el.muted = true;
      void el.play()?.catch(() => undefined);
    });
  }, [src.url]);

  if (!downloaded) {
    const fetching = asked || (!!progress && progress.have < progress.total);
    const percent = progress && progress.total > 0 ? Math.round((progress.have / progress.total) * 100) : 0;
    const fetchBlob = async () => {
      if (!client) return;
      if ((await call(client.call('blob.fetch', { space, blob: att.blob, priority: 200 }))) !== undefined) setAsked(true);
    };
    return (
      <div className="vw-pending">
        {thumb ? <img className="vw-placeholder" src={thumb} alt="" draggable={false} /> : <ImageOff size={40} />}
        <div className="vw-pending-box">
          {fetching ? (
            <span>{t('받는 중 {percent}%{speed}', { percent, speed: '' })}</span>
          ) : (
            <button type="button" className="btn primary" onClick={() => void fetchBlob()}>
              <Download size={14} /> {t('원본 받기 · {size}', { size: formatBytes(att.size) })}
            </button>
          )}
        </div>
      </div>
    );
  }
  if (src.error || broken) {
    return (
      <div className="vw-pending">
        <ImageOff size={40} />
        <p>{src.error ?? t('불러오지 못했습니다')}</p>
        <button type="button" className="btn" onClick={onRetry}>
          {t('다시 시도')}
        </button>
      </div>
    );
  }
  if (!src.url) return <Spinner size={32} />;
  if (item.type === 'video') {
    return (
      <video
        ref={videoRef}
        className="vw-video"
        src={src.url}
        poster={thumb}
        controls
        playsInline
        disablePictureInPicture={volatile}
        onError={() => setBroken(true)}
      />
    );
  }
  return (
    <div className="vw-frame">
      <img
        className="vw-full"
        src={src.url}
        alt={name}
        width={att.width}
        height={att.height}
        decoding="async"
        draggable={false}
        onLoad={onLoad}
        onError={() => setBroken(true)}
      />
    </div>
  );
}

/** 보기 설정(필터·정렬·이미지 맞춤). 저장하면 모든 채널의 뷰어에 같게 걸립니다(사용자 필터만 이 뷰어에). */
function ViewerSettings({
  value,
  fixed,
  authors,
  members,
  messages,
  onSave,
  onClose,
}: {
  value: ViewerPrefs;
  /** 라이브러리에서 고른 정해진 목록(필터를 걸지 않음). */
  fixed?: boolean;
  authors: string[];
  members: MemberView[];
  messages: MessageView[];
  onSave: (v: ViewerPrefs, authors: string[]) => void;
  onClose: () => void;
}) {
  const [picked, setPicked] = useState(authors);
  const [q, setQ] = useState('');
  const byId = new Map(members.map((m) => [m.user, m]));
  const toggle = (id: string) => setPicked(picked.includes(id) ? picked.filter((x) => x !== id) : [...picked, id]);
  // 빈칸이면 불러온 글을 쓴 사람, 이름을 넣으면 멤버 전체에서 찾습니다(큰 크루에서 다 그리지 않게 30명까지).
  const want = q.trim().toLowerCase();
  const candidates = (
    want
      ? members.filter((m) => displayName(m).toLowerCase().includes(want))
      : [...new Set(messages.filter((m) => !isAnonymousMessage(m)).map((m) => m.author))].flatMap((id) => byId.get(id) ?? [])
  ).slice(0, 30);
  const check = (name: 'text' | 'images' | 'videos' | 'audio' | 'files' | 'fill' | 'rtl', label: string) => (
    <label className="check">
      <input type="checkbox" name={name} defaultChecked={value[name]} /> {label}
    </label>
  );
  const number = (name: keyof typeof VIEWER_LIMITS, label: string) => (
    <Field label={label}>
      <input className="input" type="number" name={name} min={VIEWER_LIMITS[name][0]} max={VIEWER_LIMITS[name][1]} defaultValue={value[name]} />
    </Field>
  );
  return (
    <form
      className="vw-panel"
      onSubmit={(e) => {
        e.preventDefault();
        const f = new FormData(e.currentTarget);
        onSave(
          sanitizeViewer({
            order: f.get('order'),
            text: f.has('text'),
            images: f.has('images'),
            videos: f.has('videos'),
            audio: f.has('audio'),
            files: f.has('files'),
            include: String(f.get('include') ?? ''),
            exclude: String(f.get('exclude') ?? ''),
            fill: f.has('fill'),
            fit: f.get('fit'),
            fillPercent: f.get('fillPercent'),
            rtl: f.has('rtl'),
            scrollStep: f.get('scrollStep'),
            seek: f.get('seek'),
          }),
          picked,
        );
      }}
    >
      <h2>{t('보기 설정')}</h2>
      {fixed && <p className="muted small">{t('라이브러리에서 고른 파일을 볼 때는 종류·단어·순서 필터를 쓰지 않습니다(채널 뷰어에만 걸림).')}</p>}
      <div className="row gap">
        {check('text', t('글'))}
        {check('images', t('이미지'))}
        {check('videos', t('영상'))}
        {check('audio', t('오디오'))}
        {check('files', t('첨부 파일'))}
      </div>
      {/* 안에 입력이 여럿이라 Field(label)로 감싸지 않습니다. 정해진 목록(라이브러리)에는 걸지 않습니다. */}
      {!fixed && (
      <div className="field">
        <span className="field-label">{t('사용자')}</span>
        {picked.length > 0 && (
          <div className="chips">
            {picked.map((id) => (
              <button key={id} type="button" className="chip active" title={t('빼기')} onClick={() => toggle(id)}>
                <Avatar id={id} name={displayName(byId.get(id), t('알 수 없는 사용자'))} size={18} color={byId.get(id)?.profile.theme_color} />
                {displayName(byId.get(id), t('알 수 없는 사용자'))} <X size={12} />
              </button>
            ))}
          </div>
        )}
        <input
          className="input"
          type="search"
          placeholder={t('이름으로 찾기')}
          aria-label={t('이름으로 찾기')}
          value={q}
          onChange={(e) => setQ(e.target.value)}
          // 이름을 찾다 누른 Enter가 설정을 저장하고 닫지 않게.
          onKeyDown={(e) => e.key === 'Enter' && e.preventDefault()}
        />
        <div className="vw-people">
          {candidates.map((m) => (
            <label key={m.user} className="check">
              <input type="checkbox" checked={picked.includes(m.user)} onChange={() => toggle(m.user)} />
              <Avatar id={m.user} name={displayName(m)} size={22} color={m.profile.theme_color} />
              {displayName(m)}
            </label>
          ))}
        </div>
        <span className="field-hint">{t('고른 사람이 쓴 것만 보입니다(비우면 모두, 익명 글은 빠짐). 뷰어를 닫으면 풀립니다.')}</span>
      </div>
      )}
      <Field label={t('포함할 단어')} hint={t('쉼표나 줄바꿈으로 나눕니다. 글이나 파일 이름에 하나라도 들어간 메시지만 보입니다(비우면 모두).')}>
        <textarea className="input" name="include" rows={2} defaultValue={value.include.join(', ')} />
      </Field>
      <Field label={t('제외할 단어')} hint={t('글이나 파일 이름에 하나라도 들어간 메시지는 뺍니다. 포함할 단어보다 먼저 봅니다.')}>
        <textarea className="input" name="exclude" rows={2} defaultValue={value.exclude.join(', ')} />
      </Field>
      <Field label={t('정렬 순서')} hint={t('오래된순은 채널 기록을 처음까지 모두 읽은 뒤 보여 주므로 처음 열 때 오래 걸릴 수 있습니다.')}>
        <select className="select" name="order" defaultValue={value.order}>
          <option value="newest">{t('최신순')}</option>
          <option value="oldest">{t('오래된순')}</option>
        </select>
      </Field>
      {check('fill', t('꽉차게 보기'))}
      <Field
        label={t('채워서 보기')}
        hint={t('기본은 가로가 긴 이미지를 높이에, 세로가 긴 이미지를 폭에 맞춥니다. 높이 맞춤은 가로형 만화책, 폭 맞춤은 세로형 웹툰에 알맞습니다.')}
      >
        <select className="select" name="fit" defaultValue={value.fit}>
          <option value="auto">{t('꽉차게 보기 (기본)')}</option>
          <option value="height">{t('꽉차게 보기 (높이 맞춤)')}</option>
          <option value="width">{t('꽉차게 보기 (폭 맞춤)')}</option>
        </select>
      </Field>
      {number('fillPercent', t('꽉차게 보기 크기(%)'))}
      {check('rtl', t('오른쪽→왼쪽으로 읽기'))}
      {number('scrollStep', t('↑↓ 스크롤 길이(px)'))}
      {number('seek', t('←→ 영상 이동(초)'))}
      <div className="row gap-s vw-panel-actions">
        <button type="submit" className="btn primary">
          {t('저장')}
        </button>
        <button type="button" className="btn ghost" onClick={onClose}>
          {t('닫기')}
        </button>
      </div>
    </form>
  );
}
