// 입력창: 글(마크다운)·첨부·이모지·멘션 자동완성·답장. 한글 조합 중(isComposing)에는 Enter로 보내지 않습니다.
// 동영상·음성을 붙이면 웹뷰에서 썸네일·크기·길이를 미리 읽어(hooks/attachments.ts) message.send에 같이 싣습니다. 못 읽으면 그냥 보냅니다.
// 휘발성 채널에 첨부를 고르면 무엇을 지켜 주고 무엇은 못 지키는지 알립니다. 참가 전 P2P 전용 채널에서는 쓸 수 없습니다.

import { AlertTriangle, CornerUpLeft, FileIcon, Flame, Paperclip, SendHorizontal, SmilePlus, X } from 'lucide-react';
import { useEffect, useMemo, useRef, useState } from 'react';
import type { ChannelView, EmojiView, MemberView } from '../api/types';
import { NO_ATTACH_TEXT, usePendingFiles, type PendingFiles } from '../hooks/attachments';
import { anonName, isAnonymousMessage, isMine } from '../lib/anon';
import { isP2pLocked } from '../lib/channel';
import { t, tn } from '../i18n';
import { formatEmoji } from '../lib/emoji';
import { searchUnicodeEmoji } from '../lib/emojiData';
import { formatClock } from '../lib/media';
import { cx, displayName, excerpt, hasPerm } from '../lib/misc';
import { clipboardFiles } from '../lib/paste';
import { isTouch } from '../lib/platform';
import { hasChannelPerm } from '../lib/perms';
import { encodeMentions } from '../lib/text';
import { chKey, useStore } from '../store/store';
import { AlignButtons } from './AlignButtons';
import { Avatar, CustomEmoji, Popover, Spinner } from './common';
import { EmojiPicker } from './EmojiPicker';
import { TypingIndicator } from './TypingIndicator';

export const MAX_LEN = 4000;

/** 입력하는 동안 '쓰는 중' 알림을 보내는 간격(데몬은 3초에 한 번으로 줄이고, 받는 쪽은 6초 동안 보여 줌). */
export const TYPING_EVERY_MS = 2500;

/** 휘발성 채널 첨부 안내(보장과 한계). 한도는 데몬 설정(기본값)입니다. */
export const VOLATILE_ATTACH_NOTE = t(
  '휘발성 첨부는 각 기기의 메모리에만 두고 메시지와 함께 지웁니다(디스크·임시 파일에 쓰지 않음, 메모리 예산은 기기마다 기본 1GiB). 다만 화면 캡처, 운영체제(스왑·최대 절전 파일), 받은 사람이 직접 저장한 파일은 지울 수 없습니다.',
);

/** 모두 멘션(`mention_many` 권한이 있을 때만 후보에 보임). */
type Broadcast = { broadcast: 'everyone' | 'here'; label: string };
const BROADCASTS: Broadcast[] = [
  { broadcast: 'everyone', label: t('이 채널을 볼 수 있는 모두에게 알림') },
  { broadcast: 'here', label: t('지금 접속한 사람에게만 알림') },
];

type Suggest =
  | { kind: 'mention'; start: number; items: Array<MemberView | Broadcast> }
  | { kind: 'emoji'; start: number; items: Array<{ key: string; insert: string; custom?: EmojiView; char?: string; label: string }> };

export function Composer({
  space,
  channel,
  thread,
  channelView,
  placeholder,
}: {
  space: string;
  channel: string;
  thread?: string;
  channelView?: ChannelView;
  placeholder?: string;
}) {
  const key = chKey(space, channel, thread);
  const view = useStore((s) => s.views[space]);
  const client = useStore((s) => s.client);
  const me = useStore((s) => s.me);
  const replyTo = useStore((s) => s.replyTo[key]);
  const setReply = useStore((s) => s.setReply);
  const setEditing = useStore((s) => s.setEditing);
  const send = useStore((s) => s.send);
  const toast = useStore((s) => s.toast);
  const lastMine = useStore((s) => {
    const list = s.channels[key]?.messages;
    if (!list) return undefined;
    // 익명 글도 내 글이면 고칠 수 있게 `mine`으로 찾습니다.
    for (let i = list.length - 1; i >= 0 && i >= list.length - 50; i--) if (isMine(list[i], s.me) && !list[i].deleted) return list[i];
    return undefined;
  });

  const [text, setText] = useState('');
  const [sending, setSending] = useState(false);
  const [picker, setPicker] = useState<DOMRect | null>(null);
  const [suggest, setSuggest] = useState<Suggest | null>(null);
  const [sel, setSel] = useState(0);
  const [pasteWarn, setPasteWarn] = useState(false);
  const mentionMap = useRef(new Map<string, string>());
  const ref = useRef<HTMLTextAreaElement>(null);
  const typingAt = useRef(0);

  const volatile = channelView?.settings.security === 'volatile';
  const pending = usePendingFiles(client, volatile, undefined, channelView?.attach_limit);
  const { files, clear: clearFiles } = pending;
  const canSend = hasPerm(view, 'send_messages');
  // 첨부 한도 0은 첨부 금지 채널입니다.
  const attachBanned = channelView?.attach_limit === 0;
  const canAttach = hasPerm(view, 'attach_files') && !attachBanned;
  const mustJoin = channelView?.settings.join_mode === 'join_leave' && !channelView.joined;
  // 만료된 연락처와의 1대1 대화는 읽기만 합니다(데몬도 보내기를 거절).
  const expired = useStore((s) => s.friends.some((f) => f.space === space && f.kind === 'expired'));
  // 화면은 잠금 화면을 대신 보이지만, 입력창이 다른 경로로 그려져도 IP 노출 동의 없이 보내지 않게 여기서도 막습니다.
  const p2pLocked = isP2pLocked(channelView);
  const disabled = !canSend || mustJoin || expired || p2pLocked;
  const members = view?.members ?? [];

  // 채널을 바꾸면 입력 초기화(휘발성 채널 입력이 다른 채널로 새지 않게)
  useEffect(() => {
    setText('');
    clearFiles();
    setSuggest(null);
    setPasteWarn(false);
    mentionMap.current.clear();
    typingAt.current = 0;
    // 화상 키보드가 저절로 올라와 메시지를 가리지 않게 합니다.
    if (!isTouch()) ref.current?.focus();
  }, [key, clearFiles]);

  /** 글타래가 아닌 채널 입력창에서 글을 쓰는 동안 몇 초마다 알립니다(못 보내도 다음 입력 때 다시). */
  const noteTyping = (value: string) => {
    if (!client || thread || disabled || !value.trim()) return;
    const now = Date.now();
    if (now - typingAt.current < TYPING_EVERY_MS) return;
    typingAt.current = now;
    client.call('channel.typing', { space, channel }).catch(() => {});
  };

  // 답장을 누르면 바로 쓸 수 있게 입력창 끝에 커서를 둡니다.
  const focusReq = useStore((s) => (s.composerFocus?.key === key ? s.composerFocus.n : 0));
  useEffect(() => {
    const el = ref.current;
    if (!focusReq || disabled || !el) return;
    el.focus();
    el.setSelectionRange(el.value.length, el.value.length);
  }, [focusReq, disabled]);

  const addFilesRef = useRef(pending.add);
  addFilesRef.current = pending.add;

  // 파일 끌어 놓기(Tauri)
  useEffect(() => {
    if (!client || disabled || !canAttach) return;
    return client.transport.onFileDrop((paths) => {
      addFilesRef.current(paths.map((p) => ({ path: p, name: p.split(/[\\/]/).pop() ?? p })));
    });
  }, [client, disabled, canAttach]);

  // 높이 자동 조절
  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    el.style.height = 'auto';
    el.style.height = `${Math.min(el.scrollHeight, Math.round(window.innerHeight * 0.4))}px`;
  }, [text]);

  const emojis = useMemo(() => view?.emojis ?? [], [view?.emojis]);

  const updateSuggest = (value: string, caret: number) => {
    const before = value.slice(0, caret);
    const mention = /(^|\s)@([\p{L}\p{N}_.-]{0,32})$/u.exec(before);
    if (mention) {
      const q = mention[2].toLowerCase();
      const broadcasts = view?.kind === 'crew' && hasChannelPerm(view, channelView, me, 'mention_many') ? BROADCASTS.filter((b) => b.broadcast.startsWith(q)) : [];
      const people = members
        .filter((m) => m.user !== me && displayName(m).toLowerCase().includes(q))
        .sort((a, b) => Number(b.online) - Number(a.online))
        .slice(0, 8);
      const items = [...broadcasts, ...people];
      setSuggest(items.length ? { kind: 'mention', start: caret - mention[2].length - 1, items } : null);
      setSel(0);
      return;
    }
    const emo = /(^|\s):([\p{L}\p{N}_]{2,32})$/u.exec(before);
    if (emo) {
      const q = emo[2].toLowerCase();
      const custom = emojis
        .filter((e) => e.name.toLowerCase().includes(q))
        .slice(0, 6)
        .map((e) => ({ key: e.id, insert: formatEmoji(e.name, e.id, e.animated), custom: e, label: `:${e.name}:` }));
      const uni = searchUnicodeEmoji(q, 8 - custom.length).map((e) => ({ key: e.char, insert: e.char, char: e.char, label: e.keywords.split(' ')[0] ?? '' }));
      const items = [...custom, ...uni];
      setSuggest(items.length ? { kind: 'emoji', start: caret - emo[2].length - 1, items } : null);
      setSel(0);
      return;
    }
    setSuggest(null);
  };

  const insertAt = (start: number, insert: string) => {
    const el = ref.current;
    const caret = el?.selectionStart ?? text.length;
    const next = text.slice(0, start) + insert + text.slice(caret);
    setText(next.slice(0, MAX_LEN));
    setSuggest(null);
    requestAnimationFrame(() => {
      if (!el) return;
      const pos = start + insert.length;
      el.focus();
      el.setSelectionRange(pos, pos);
    });
  };

  const accept = (i: number) => {
    if (!suggest) return;
    if (suggest.kind === 'mention') {
      const m = suggest.items[i];
      if (!m) return;
      if ('broadcast' in m) return insertAt(suggest.start, `@${m.broadcast} `);
      const name = displayName(m).replace(/\s+/g, '_');
      mentionMap.current.set(name, m.user);
      insertAt(suggest.start, `@${name} `);
    } else {
      const e = suggest.items[i];
      if (e) insertAt(suggest.start, `${e.insert} `);
    }
  };

  const submit = async () => {
    if (sending || disabled) return;
    const body = encodeMentions(text.trim(), mentionMap.current);
    if (!body && files.length === 0) return;
    if (body.length > MAX_LEN) return;
    setSending(true);
    const { inputs, hinted } = await pending.collect();
    const ok = await send(space, channel, body, inputs, { replyTo: replyTo?.id, thread });
    setSending(false);
    if (!ok && hinted) pending.dropHints();
    if (ok) {
      setText('');
      clearFiles();
      setReply(key, undefined);
      mentionMap.current.clear();
      typingAt.current = 0;
    }
  };

  const attach = async () => {
    if (!client) return;
    const picked = await client.transport.pickFiles({ multiple: true });
    if (picked.length) pending.add(picked);
  };

  if (mustJoin) return null;

  const replyAnon = !!replyTo && isAnonymousMessage(replyTo);
  const replyAuthor = replyTo && !replyAnon ? members.find((m) => m.user === replyTo.author) : undefined;

  return (
    <div className={cx('composer', volatile && 'volatile')}>
      {suggest && (
        <div className="suggest">
          <div className="suggest-title">{suggest.kind === 'mention' ? t('멤버') : t('이모지')}</div>
          {suggest.kind === 'mention'
            ? suggest.items.map((m, i) =>
                'broadcast' in m ? (
                  <button key={m.broadcast} type="button" className={cx('suggest-item', i === sel && 'active')} onMouseDown={(e) => e.preventDefault()} onClick={() => accept(i)}>
                    <span className="suggest-at">@</span>
                    <span>@{m.broadcast}</span>
                    <span className="muted small">{m.label}</span>
                  </button>
                ) : (
                  <button key={m.user} type="button" className={cx('suggest-item', i === sel && 'active')} onMouseDown={(e) => e.preventDefault()} onClick={() => accept(i)}>
                    <Avatar id={m.user} name={displayName(m)} size={22} online={m.online} />
                    <span>{displayName(m)}</span>
                    {m.automation && <span className="tag bot">AI</span>}
                  </button>
                ),
              )
            : suggest.items.map((e, i) => (
                <button key={e.key} type="button" className={cx('suggest-item', i === sel && 'active')} onMouseDown={(ev) => ev.preventDefault()} onClick={() => accept(i)}>
                  {e.custom ? <CustomEmoji id={e.custom.id} name={e.custom.name} size={20} /> : <span className="uni">{e.char}</span>}
                  <span>{e.label}</span>
                </button>
              ))}
        </div>
      )}
      {replyTo && (
        <div className="reply-bar">
          <CornerUpLeft size={14} />
          <div className="reply-bar-body">
            <span>
              {replyAnon ? (
                tn('{name} 글에 답장', { name: <strong>{anonName(replyTo)}</strong> })
              ) : (
                tn('{name} 님에게 답장', { name: <strong>{displayName(replyAuthor)}</strong> })
              )}
            </span>
            {/* 메시지 목록의 답장 머리글과 같은 발췌 */}
            <span className="reply-text muted">{replyTo.deleted ? t('삭제된 메시지') : excerpt(replyTo.text) || t('첨부 파일')}</span>
          </div>
          <button type="button" className="icon-btn small" title={t('답장 취소')} onClick={() => setReply(key, undefined)}>
            <X size={14} />
          </button>
        </div>
      )}
      {pasteWarn && volatile && (
        <div className="paste-warn">
          <AlertTriangle size={14} />
          {t('붙여넣은 원본은 클립보드(와 클립보드 기록 앱)에 그대로 남아 있습니다. 휘발성 채널은 클립보드를 지우지 못합니다.')}
          <button type="button" className="icon-btn small" onClick={() => setPasteWarn(false)}>
            <X size={14} />
          </button>
        </div>
      )}
      {volatile && files.length > 0 && (
        <div className="paste-warn" role="note">
          <Flame size={14} />
          {VOLATILE_ATTACH_NOTE}
        </div>
      )}
      <FileChips files={pending} />
      <div className="composer-box">
        <button type="button" className="icon-btn" title={canAttach ? (volatile ? t('파일 첨부(메모리에만 보관)') : t('파일 첨부')) : attachBanned ? NO_ATTACH_TEXT : t('파일 첨부 권한이 없습니다')} disabled={disabled || !canAttach} onClick={() => void attach()}>
          <Paperclip size={20} />
        </button>
        <textarea
          ref={ref}
          value={text}
          rows={1}
          maxLength={MAX_LEN}
          disabled={disabled}
          // 휘발성 글은 맞춤법 검사기(Android는 키보드 앱의 검사 서비스)·자동 완성 기록으로 보내지 않습니다.
          spellCheck={!volatile}
          autoComplete={volatile ? 'off' : undefined}
          placeholder={
            expired
              ? t('만료된 연락처에는 메시지를 보낼 수 없습니다')
              : p2pLocked
                ? t('P2P 전용 채널에 참가해야 메시지를 보낼 수 있습니다')
                : disabled
                  ? t('이 채널에 메시지를 보낼 권한이 없습니다')
                  : placeholder
          }
          onChange={(e) => {
            setText(e.target.value);
            updateSuggest(e.target.value, e.target.selectionStart);
            noteTyping(e.target.value);
          }}
          onPaste={(e) => {
            // 클립보드의 이미지·파일은 첨부로(글자가 함께 있으면 글자만 붙임).
            const pasted = canAttach ? clipboardFiles(e.clipboardData) : [];
            if (pasted.length) {
              e.preventDefault();
              void pending.paste(pasted).then((err) => err && toast('error', err));
            }
            if (volatile) setPasteWarn(true);
          }}
          onKeyDown={(e) => {
            if (e.nativeEvent.isComposing) return;
            if (suggest) {
              if (e.key === 'ArrowDown') {
                e.preventDefault();
                setSel((sel + 1) % suggest.items.length);
                return;
              }
              if (e.key === 'ArrowUp') {
                e.preventDefault();
                setSel((sel - 1 + suggest.items.length) % suggest.items.length);
                return;
              }
              if (e.key === 'Enter' || e.key === 'Tab') {
                e.preventDefault();
                accept(sel);
                return;
              }
              if (e.key === 'Escape') {
                setSuggest(null);
                return;
              }
            }
            // 화상 키보드는 Enter가 줄바꿈이고 보내기는 단추로 합니다.
            if (e.key === 'Enter' && !e.shiftKey && !isTouch()) {
              e.preventDefault();
              void submit();
            } else if (e.key === 'ArrowUp' && !text && lastMine) {
              e.preventDefault();
              setEditing({ key, id: lastMine.id });
            } else if (e.key === 'Escape' && replyTo) {
              setReply(key, undefined);
            }
          }}
        />
        <AlignButtons textarea={ref} text={text} onChange={setText} maxLength={MAX_LEN} disabled={disabled} />
        <button type="button" className="icon-btn" title={t('이모지')} disabled={disabled} onClick={(e) => setPicker(e.currentTarget.getBoundingClientRect())}>
          <SmilePlus size={20} />
        </button>
        {isTouch() && (
          // 누를 때 입력창의 포커스를 뺏으면 화상 키보드가 내려갑니다.
          <button type="button" className="icon-btn send" title={t('메시지 보내기')} aria-label={t('메시지 보내기')} disabled={disabled} onMouseDown={(e) => e.preventDefault()} onClick={() => void submit()}>
            <SendHorizontal size={20} />
          </button>
        )}
      </div>
      {!thread && <TypingIndicator space={space} channel={channel} />}
      {text.length > MAX_LEN - 500 && <div className="char-count">{MAX_LEN - text.length}</div>}
      {picker && (
        <Popover anchor={picker} onClose={() => setPicker(null)}>
          <EmojiPicker
            space={space}
            onPick={(emoji) => {
              setPicker(null);
              const el = ref.current;
              const caret = el?.selectionStart ?? text.length;
              insertAt(caret, emoji);
            }}
          />
        </Popover>
      )}
    </div>
  );
}

/** 고른 첨부 칩(동영상·음성은 썸네일·길이, 읽는 중이면 돌림표). */
export function FileChips({ files }: { files: Pick<PendingFiles, 'files' | 'hints' | 'remove'> & { pasting?: number } }) {
  const pasting = files.pasting ?? 0;
  if (files.files.length === 0 && pasting === 0) return null;
  return (
    <div className="file-chips">
      {files.files.map((f, i) => {
        const h = files.hints[f.path];
        return (
          <div key={`${f.path}-${i}`} className="file-chip">
            {h && h !== 'pending' && h.thumbnail_base64 ? (
              <img className="chip-thumb" src={`data:image/jpeg;base64,${h.thumbnail_base64}`} alt="" draggable={false} />
            ) : (
              <FileIcon size={16} />
            )}
            <span title={f.path}>{f.name}</span>
            {h === 'pending' && <Spinner size={12} />}
            {h && h !== 'pending' && h.duration_ms ? <span className="chip-meta">{formatClock(h.duration_ms)}</span> : null}
            <button type="button" className="icon-btn small" title={t('빼기')} onClick={() => files.remove(i)}>
              <X size={13} />
            </button>
          </div>
        );
      })}
      {pasting > 0 && (
        <div className="file-chip">
          <Spinner size={12} />
          <span>{pasting > 1 ? t('붙여넣는 중 ({n})…', { n: pasting }) : t('붙여넣는 중…')}</span>
        </div>
      )}
    </div>
  );
}
