// 메시지 한 줄. 같은 작성자 연속 메시지는 묶어서(grouped) 머리글 없이 그립니다.
// 익명 글은 이름 '익명'과 기본 프사로 그리고, 누구인지 찾아볼 단서(프로필 창·역할 색·차단)를 주지 않습니다. 내 글인지는 `mine`으로 압니다.

import { Ban, Copy, CornerUpLeft, Eye, EyeOff, Flag, Flame, MoreHorizontal, Pencil, Pin, PinOff, Share2, SmilePlus, Trash2 } from 'lucide-react';
import { memo, useEffect, useRef, useState, type MouseEvent } from 'react';
import { ANONYMOUS_USER, type MemberView, type MessageView, type Role } from '../api/types';
import { t, tn } from '../i18n';
import { parseReactionEmoji } from '../lib/emoji';
import { QUICK_REACTIONS } from '../lib/emojiData';
import { formatCountdown, formatFull, formatStamp, formatTime, remainingSecs } from '../lib/format';
import { cx, displayName, excerpt, roleColor, topRole } from '../lib/misc';
import { useNow } from '../hooks/media';
import { loadPins, pinBlobs, usePinStore } from '../hooks/pins';
import { loadSharing, setSharing, useStopped } from '../hooks/sharing';
import { anonName, isAnonymousMessage, isMine } from '../lib/anon';
import { useStore } from '../store/store';
import { mentionsMe } from '../store/apply';
import { Attachment, Embed } from './Attachment';
import { Avatar, CustomEmoji, Popover, copyText } from './common';
import { MenuButton, openPersonMenu } from './menus';
import { EmojiPicker } from './EmojiPicker';
import { MessageText } from './MessageText';

/**
 * 메시지 메뉴의 파일 관리: 공유 중단·재개와 첨부 보관. 라이브러리와 같은 동작(hooks/sharing·pins)입니다.
 * 상태는 메뉴를 열 때 공간마다 한 번 읽습니다(메시지마다 묻지 않게).
 */
function FileItems({ space, channel, m, uploadedHere, onClose }: { space: string; channel: string; m: MessageView; uploadedHere: boolean; onClose: () => void }) {
  const client = useStore((s) => s.client);
  const stopped = useStopped(space, m.id);
  const pinStatus = usePinStore((s) => s.status);
  const pinIds = usePinStore((s) => s.ids);
  useEffect(() => {
    void loadSharing(space);
    if (client && usePinStore.getState().status === 'idle') void loadPins(client);
  }, [space, client]);
  const target = { space, channel, id: m.id };
  // 이 기기에서 올린 첨부는 데몬이 늘 남겨 보관할 것이 없습니다.
  const blobs = uploadedHere ? [] : m.attachments.map((a) => ({ space, blob: a.blob }));
  const allPinned = blobs.length > 0 && blobs.every((b) => pinIds.has(b.blob));
  const run = (fn: () => void) => () => {
    onClose();
    fn();
  };
  return (
    <>
      {stopped ? (
        <MenuButton icon={<Share2 size={15} />} label={t('공유 재개')} onClick={run(() => void setSharing([target], true))} />
      ) : (
        <MenuButton icon={<Ban size={15} />} label={t('공유 중단')} sub={m.attachments.length > 0 ? t('첨부도 함께') : undefined} onClick={run(() => void setSharing([target], false))} />
      )}
      {pinStatus !== 'unsupported' && blobs.length > 0 && (
        <MenuButton
          icon={allPinned ? <PinOff size={15} /> : <Pin size={15} />}
          label={allPinned ? t('첨부 보관 풀기') : t('첨부 보관하기')}
          sub={allPinned ? undefined : t('저장 공간 정리에서 뺌')}
          onClick={run(() => void pinBlobs(blobs, !allPinned))}
        />
      )}
    </>
  );
}

export interface MessageItemProps {
  m: MessageView;
  grouped: boolean;
  space: string;
  channel: string;
  msgKey: string;
  members: Map<string, MemberView>;
  roles: Role[];
  me: string | null;
  replied?: MessageView | null;
  volatile: boolean;
  canManage: boolean;
  canReact: boolean;
  isEditing: boolean;
  onJump?: (id: string) => void;
  /** 검색 결과처럼 보기만 하는 곳: 답장·수정·삭제 같은 동작 버튼을 그리지 않습니다. */
  readOnly?: boolean;
  /** 포럼 글타래 맨 위의 게시글(댓글이 곧 답글이라 답장 단추를 그리지 않음). */
  post?: boolean;
  /** 수정 금지 채널: 수정 단추를 그리지 않습니다. */
  noEdit?: boolean;
  /** 삭제 금지 채널: 운영진(canManage)이 아니면 삭제 단추를 그리지 않습니다. */
  noDelete?: boolean;
  /** 수정을 글 안이 아니라 따로 할 때(포럼 게시글의 수정 페이지). */
  onEdit?: () => void;
}

function Countdown({ expiresAt }: { expiresAt: number }) {
  const now = useNow();
  const left = remainingSecs(expiresAt, now);
  return (
    <span className={cx('countdown', left <= 10 && 'soon')} title={t('{time}에 모든 기기에서 지워집니다', { time: formatFull(expiresAt) })}>
      <Flame size={11} />
      {formatCountdown(left)}
    </span>
  );
}

export function ReactionChip({ emoji, count, me, onClick }: { emoji: string; count: number; me: boolean; onClick?: () => void }) {
  const custom = parseReactionEmoji(emoji);
  return (
    <button type="button" className={cx('reaction', me && 'me')} onClick={onClick} title={custom ? `:${custom.name}:` : emoji}>
      {custom ? <CustomEmoji id={custom.id} name={custom.name} size={16} /> : <span className="r-uni">{emoji}</span>}
      <span className="r-count">{count}</span>
    </button>
  );
}

function EditBox({ m, space, channel }: { m: MessageView; space: string; channel: string }) {
  const [text, setText] = useState(m.text);
  const edit = useStore((s) => s.edit);
  const setEditing = useStore((s) => s.setEditing);
  const ref = useRef<HTMLTextAreaElement>(null);
  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    el.focus();
    el.setSelectionRange(el.value.length, el.value.length);
  }, []);
  const save = () => {
    const body = text.trim();
    if (!body || body === m.text) {
      setEditing(null);
      return;
    }
    void edit(space, channel, m.id, body);
  };
  return (
    <div className="edit-box">
      <textarea
        ref={ref}
        value={text}
        maxLength={4000}
        rows={Math.min(8, text.split('\n').length + 1)}
        spellCheck={!m.volatile}
        autoComplete={m.volatile ? 'off' : undefined}
        onChange={(e) => setText(e.target.value)}
        onKeyDown={(e) => {
          if (e.nativeEvent.isComposing) return;
          if (e.key === 'Escape') setEditing(null);
          if (e.key === 'Enter' && !e.shiftKey) {
            e.preventDefault();
            save();
          }
        }}
      />
      <div className="edit-hint">
        {tn('Esc로 {cancel} · Enter로 {save}', {
          cancel: (
            <button type="button" onClick={() => setEditing(null)}>
              {t('취소')}
            </button>
          ),
          save: (
            <button type="button" onClick={save}>
              {t('저장')}
            </button>
          ),
        })}
        <span className="muted"> · {t('이미 받아 간 사본에는 이전 내용이 남을 수 있습니다')}</span>
      </div>
    </div>
  );
}

/** 답장 머리글에 쓸 원문 글쓴이 이름(익명 글이면 가명). */
function authorName(m: MessageView, members: Map<string, MemberView>): string {
  return isAnonymousMessage(m) ? anonName(m) : displayName(members.get(m.author));
}

export const MessageItem = memo(function MessageItem(p: MessageItemProps) {
  const { m, grouped, space, channel, members, roles, me, volatile } = p;
  const anon = isAnonymousMessage(m);
  // 익명 자리(ANONYMOUS_USER)를 멤버 목록에서 찾지 않습니다.
  const author = anon ? undefined : members.get(m.author);
  const role = topRole(author, roles);
  const mine = isMine(m, me);
  const name = anon ? anonName(m) : displayName(author, mine ? t('나') : t('알 수 없는 사용자'));
  const setReply = useStore((s) => s.setReply);
  const setEditing = useStore((s) => s.setEditing);
  const remove = useStore((s) => s.remove);
  const toggleReaction = useStore((s) => s.toggleReaction);
  const openModal = useStore((s) => s.openModal);
  const toast = useStore((s) => s.toast);
  const client = useStore((s) => s.client);
  const blockUser = useStore((s) => s.blockUser);
  const unblockUser = useStore((s) => s.unblockUser);
  // 차단 목록을 읽을 수 있는 데몬인지(모더레이션 전 데몬이면 null).
  const canBlock = useStore((s) => s.blocked !== null);
  const blockedNow = useStore((s) => !!s.blocked?.some((b) => b.user === m.author));
  // 그 글을 쓴 기기만 차단했어도 접습니다(차단 메뉴는 사람 차단 기준).
  const deviceBlocked = useStore((s) => !!s.blockedDevices?.some((b) => b.device === m.author_device));
  const repliedAuthor = p.replied?.author;
  const repliedDevice = p.replied?.author_device;
  const repliedBlockedNow = useStore(
    (s) => (!!repliedAuthor && !!s.blocked?.some((b) => b.user === repliedAuthor)) || (!!repliedDevice && !!s.blockedDevices?.some((b) => b.device === repliedDevice)),
  );
  const isCrew = useStore((s) => s.views[space]?.kind === 'crew');
  const [picker, setPicker] = useState<DOMRect | null>(null);
  const [menu, setMenu] = useState<DOMRect | null>(null);
  const [reveal, setReveal] = useState(false);
  // 이 기기에서 올린 첨부만 데몬이 늘 남깁니다. 내 다른 기기에서 보낸 첨부는 여기서는 받은 첨부라 고정할 수 있어야 함.
  const myDevice = useStore((s) => s.myDevice);
  const uploadedHere = !!myDevice && m.author_device === myDevice;
  // 차단 목록을 아는 동안은 그것이 기준입니다(차단·해제 직후 다시 읽기 전의 메시지 표시보다 새것).
  const blocked = !mine && !anon && (canBlock ? blockedNow || deviceBlocked : !!m.author_blocked);
  const hidden = blocked && !reveal;
  // 답장 머리글의 원문 발췌도 같은 기준으로 가립니다(차단한 사람의 글이 남의 답장을 통해 보이지 않게).
  const repliedAnon = !!p.replied && isAnonymousMessage(p.replied);
  const repliedBlocked = !!p.replied && !repliedAnon && !isMine(p.replied, me) && (canBlock ? repliedBlockedNow : !!p.replied.author_blocked);
  const mention = !m.deleted && mentionsMe(m, me);
  const react = (emoji: string) => {
    setPicker(null);
    void toggleReaction(space, channel, m, emoji);
  };
  const showGroupedHeader = !grouped || !!m.reply_to;
  const openProfile = () => openModal({ kind: 'friendProfile', user: m.author });
  const personMenu = (e: MouseEvent) => openPersonMenu(e, m.author, name);

  return (
    <div
      className={cx('msg', grouped && !m.reply_to && 'grouped', mention && 'mentioned', p.isEditing && 'editing', m.volatile && 'is-volatile', p.post && 'is-post')}
      data-id={m.id}
    >
      {m.reply_to && (
        <div className="msg-reply" onClick={() => p.replied && p.onJump?.(p.replied.id)}>
          <span className="reply-spine" />
          {p.replied ? (
            <>
              <Avatar id={repliedAnon ? ANONYMOUS_USER : p.replied.author} name={authorName(p.replied, members)} size={16} />
              <span className="reply-name" style={{ color: repliedAnon ? undefined : roleColor(topRole(members.get(p.replied.author), roles)?.color ?? 0) }}>
                {authorName(p.replied, members)}
              </span>
              <span className={cx('reply-text', repliedBlocked && 'muted')}>
                {p.replied.deleted ? t('삭제된 메시지') : repliedBlocked ? t('차단한 사용자의 메시지') : excerpt(p.replied.text) || t('첨부 파일')}
              </span>
            </>
          ) : (
            <span className="reply-text muted">{t('원본 메시지를 불러오지 않았습니다')}</span>
          )}
        </div>
      )}
      <div className="msg-row">
        <div className="msg-gutter">
          {showGroupedHeader ? (
            anon ? (
              <span className="avatar-btn">
                {/* 뒤늦게 가린 글(anonymous만 있음)도 실제 작성자의 프사를 찾지 않게 익명 자리로 그립니다. */}
                <Avatar id={ANONYMOUS_USER} name={name} size={40} />
              </span>
            ) : (
              <button type="button" className="avatar-btn" onClick={openProfile} onContextMenu={personMenu}>
                <Avatar id={m.author} name={name} size={40} color={author?.profile.theme_color} />
              </button>
            )
          ) : (
            <span className="msg-hover-time" title={formatFull(m.created_at)}>
              {formatTime(m.created_at)}
            </span>
          )}
        </div>
        <div className="msg-main">
          {showGroupedHeader && (
            <div className="msg-head">
              {anon ? (
                <span className="msg-author anon">{name}</span>
              ) : (
                <button type="button" className="msg-author" style={{ color: roleColor(role?.color ?? 0) }} onClick={openProfile} onContextMenu={personMenu}>
                  {name}
                </button>
              )}
              {author?.automation && <span className="tag bot">AI</span>}
              <span className="msg-time" title={formatFull(m.created_at)}>
                {formatStamp(m.created_at)}
              </span>
              {m.expires_at && !m.deleted && <Countdown expiresAt={m.expires_at} />}
            </div>
          )}
          {p.isEditing ? (
            <EditBox m={m} space={space} channel={channel} />
          ) : m.deleted ? (
            <div className="msg-deleted">{t('삭제된 메시지입니다')}</div>
          ) : hidden ? (
            <div className="msg-blocked">
              <EyeOff size={14} /> {t('차단한 사용자의 메시지입니다')}
              <button type="button" className="link-btn" onClick={() => setReveal(true)}>
                {t('보기')}
              </button>
            </div>
          ) : (
            <>
              {m.text && (
                <div className="msg-body">
                  <MessageText text={m.text} members={members} me={me} />
                  {/* 게시글은 제목 아래(ForumPostHeader)에 고친 일시를 따로 보입니다. */}
                  {m.edited_at && !p.post && (
                    <span className="edited" title={t('{time}에 수정', { time: formatFull(m.edited_at) })}>
                      {t('(수정됨)')}
                    </span>
                  )}
                  {grouped && !m.reply_to && m.expires_at && <Countdown expiresAt={m.expires_at} />}
                </div>
              )}
              {m.attachments.length > 0 && (
                <div className="msg-atts">
                  {m.attachments.map((a) => (
                    <Attachment
                      key={a.blob}
                      att={a}
                      space={space}
                      channel={channel || undefined}
                      messageId={m.id}
                      msgKey={p.msgKey}
                      volatile={volatile || m.volatile}
                      own={uploadedHere}
                    />
                  ))}
                </div>
              )}
              {m.embeds.length > 0 && (
                <div className="msg-embeds">
                  {m.embeds.map((e, i) => (
                    <Embed key={i} embed={e} volatile={volatile || m.volatile} />
                  ))}
                </div>
              )}
              {m.reactions.length > 0 && (
                <div className="reactions">
                  {m.reactions.map((r) => (
                    <ReactionChip key={r.emoji} {...r} onClick={p.canReact ? () => react(r.emoji) : undefined} />
                  ))}
                  {p.canReact && (
                    <button type="button" className="reaction add" title={t('반응 추가')} onClick={(e) => setPicker(e.currentTarget.getBoundingClientRect())}>
                      <SmilePlus size={15} />
                    </button>
                  )}
                </div>
              )}
            </>
          )}
        </div>
      </div>
      {!m.deleted && !p.isEditing && !p.readOnly && !hidden && (
        <div className={cx('msg-actions', (picker || menu) && 'open')}>
          {p.canReact &&
            QUICK_REACTIONS.slice(0, 3).map((e) => (
              <button key={e} type="button" className="qa uni" title={t('{emoji} 반응', { emoji: e })} onClick={() => react(e)}>
                {e}
              </button>
            ))}
          {p.canReact && (
            <button type="button" className="qa" title={t('반응 추가')} onClick={(e) => setPicker(e.currentTarget.getBoundingClientRect())}>
              <SmilePlus size={17} />
            </button>
          )}
          {!p.post && (
            <button type="button" className="qa" title={t('답장')} onClick={() => setReply(p.msgKey, m)}>
              <CornerUpLeft size={17} />
            </button>
          )}
          {mine && !p.noEdit && (
            <button type="button" className="qa" title={t('수정')} onClick={() => (p.onEdit ? p.onEdit() : setEditing({ key: p.msgKey, id: m.id }))}>
              <Pencil size={16} />
            </button>
          )}
          {((mine && !p.noDelete) || p.canManage) && (
            <button type="button" className="qa danger" title={t('삭제')} onClick={() => remove(space, channel, m)}>
              <Trash2 size={16} />
            </button>
          )}
          <button type="button" className="qa" title={t('더 보기')} onClick={(e) => setMenu(e.currentTarget.getBoundingClientRect())}>
            <MoreHorizontal size={17} />
          </button>
        </div>
      )}
      {picker && (
        <Popover anchor={picker} onClose={() => setPicker(null)}>
          <EmojiPicker space={space} onPick={react} />
        </Popover>
      )}
      {menu && (
        <Popover anchor={menu} onClose={() => setMenu(null)} className="menu">
          <button
            type="button"
            className="menu-item"
            onClick={async () => {
              setMenu(null);
              if (await copyText(m.text)) {
                toast(volatile ? 'warn' : 'success', volatile ? t('복사했습니다. 클립보드에 남은 내용은 휘발성 보장 밖입니다.') : t('복사했습니다'));
              }
            }}
          >
            <Copy size={15} /> {t('본문 복사')}
          </button>
          <button
            type="button"
            className="menu-item"
            onClick={async () => {
              setMenu(null);
              if (await copyText(m.id)) toast('success', t('메시지 ID를 복사했습니다'));
            }}
          >
            <Copy size={15} /> {t('메시지 ID 복사')}
          </button>
          {!volatile && client && <FileItems space={space} channel={channel} m={m} uploadedHere={uploadedHere} onClose={() => setMenu(null)} />}
          {blocked && reveal && (
            <button
              type="button"
              className="menu-item"
              onClick={() => {
                setMenu(null);
                setReveal(false);
              }}
            >
              <Eye size={15} /> {t('다시 접기')}
            </button>
          )}
          {!mine && isCrew && (
            <button
              type="button"
              className="menu-item danger"
              title={t('크루 운영진에게만 알립니다')}
              onClick={() => {
                setMenu(null);
                openModal({ kind: 'report', space, channel, message: m });
              }}
            >
              <Flag size={15} /> {t('신고하기')}
            </button>
          )}
          {!mine && !anon && canBlock && (
            <button
              type="button"
              className="menu-item danger"
              onClick={() => {
                setMenu(null);
                if (blockedNow) void unblockUser(m.author);
                else blockUser(m.author, name);
              }}
            >
              <Ban size={15} /> {blockedNow ? t('차단 풀기') : t('이 사람 차단')}
            </button>
          )}
        </Popover>
      )}
    </div>
  );
});
