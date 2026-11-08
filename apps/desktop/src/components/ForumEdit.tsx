// 포럼 게시글 수정 페이지(작성 페이지와 같은 틀, 포럼 채널 화면 안). 제목·태그·본문을 고칩니다. 첨부는 고칠 수 없어 보기만 합니다
// (수정 이벤트는 본문만 바꿈). 저장하면 본문은 늘 `message.edit`로 보내 '(수정됨) 일시'가 정해지게 하고, 제목·태그가 바뀌었으면
// `forum.update_post`로 함께 보냅니다. 본문은 글타래에 읽어 둔 게시글 메시지에서 가져옵니다(없으면 읽음).

import { ArrowLeft, Paperclip } from 'lucide-react';
import { useEffect, useRef, useState } from 'react';
import { FORUM_PREVIEW_CHARS, type ChannelView, type ForumPostView, type SpaceView } from '../api/types';
import { t } from '../i18n';
import { cx } from '../lib/misc';
import { hasChannelPerm } from '../lib/perms';
import { plainName } from '../lib/text';
import { Field } from '../modals/Modal';
import { chKey, useStore } from '../store/store';
import { Spinner } from './common';
import { AlignButtons } from './AlignButtons';
import { MAX_LEN } from './Composer';
import { MARKDOWN_EDIT_HINT, MAX_POST_TAGS, MAX_TITLE, TAG_HINT } from './ForumCompose';

const sameTags = (a: number[], b: number[]) => a.length === b.length && a.every((x) => b.includes(x));

export function ForumEdit({
  space,
  channel,
  post,
  onCancel,
  onSaved,
}: {
  space: SpaceView;
  channel: ChannelView;
  post: ForumPostView;
  onCancel: () => void;
  /** 고친 글(목록을 다시 읽기 전까지 쓸 임시 목록 항목). */
  onSaved: (post: ForumPostView) => void;
}) {
  const client = useStore((s) => s.client);
  const me = useStore((s) => s.me);
  const call = useStore((s) => s.call);
  const openModal = useStore((s) => s.openModal);
  const loadLatest = useStore((s) => s.loadLatest);
  const key = chKey(space.space, channel.channel, post.id);
  const loaded = useStore((s) => !!s.channels[key]?.loaded);
  const head = useStore((s) => s.channels[key]?.messages.find((m) => m.id === post.id));
  const forumTags = channel.settings.forum_tags;
  // 채널에서 지운 태그는 처음부터 빼 둡니다(데몬이 모르는 태그를 거절하므로).
  const tags0 = post.tags.filter((id) => forumTags.some((x) => x.id === id));
  const [title, setTitle] = useState(post.title);
  const [tags, setTags] = useState<number[]>(tags0);
  const [text, setText] = useState<string | null>(head ? head.text : null);
  const [busy, setBusy] = useState(false);
  const canFeature = hasChannelPerm(space, channel, me, 'manage_channels');
  const titleRef = useRef<HTMLInputElement>(null);
  const textRef = useRef<HTMLTextAreaElement>(null);

  useEffect(() => titleRef.current?.focus(), []);
  useEffect(() => {
    if (!loaded) void loadLatest(space.space, channel.channel, post.id);
  }, [loaded, loadLatest, space.space, channel.channel, post.id]);
  // 게시글 메시지를 늦게 읽었으면 그때 본문을 채웁니다(이미 고치기 시작했으면 그대로).
  useEffect(() => {
    if (head && text === null) setText(head.text);
  }, [head, text]);

  const body = (text ?? '').trim();
  const attachments = head?.attachments ?? [];
  const changed = title.trim() !== post.title || !sameTags(tags, tags0) || body !== (head?.text ?? '').trim();
  const ready = !!head && text !== null && !!title.trim() && (!!body || attachments.length > 0) && body.length <= MAX_LEN;

  const cancel = () => {
    if (!changed) {
      onCancel();
      return;
    }
    openModal({
      kind: 'confirm',
      title: t('수정 취소'),
      body: t('고친 내용을 버릴까요? 게시글은 고치기 전 그대로 남습니다.'),
      danger: true,
      confirmLabel: t('버리기'),
      onConfirm: onCancel,
    });
  };

  const submit = async () => {
    if (!ready || busy || !client) return;
    setBusy(true);
    const ref = { space: space.space, channel: channel.channel };
    // 본문은 그대로여도 보내 '(수정됨) 일시'를 남깁니다(제목·태그 변경은 수정 시각을 남기지 않음). 첨부만 있는 글의 빈 본문은 보낼 수 없습니다.
    if (body) {
      const r = await call(client.call('message.edit', { ...ref, id: post.id, text: body }));
      if (r === undefined) {
        setBusy(false);
        return;
      }
    }
    const newTitle = title.trim();
    const patch = { ...(newTitle !== post.title ? { title: newTitle } : {}), ...(!sameTags(tags, tags0) ? { tags } : {}) };
    if (Object.keys(patch).length > 0) {
      const r = await call(client.call('forum.update_post', { ...ref, post: post.id, ...patch }));
      if (r === undefined) {
        setBusy(false);
        return;
      }
    }
    setBusy(false);
    useStore.getState().toast('success', t('게시글을 고쳤습니다'));
    onSaved({
      ...post,
      title: newTitle,
      tags,
      ...(body ? { edited_at: Date.now(), preview: Array.from(body).slice(0, FORUM_PREVIEW_CHARS).join('') } : {}),
    });
  };

  const toggleTag = (id: number) => setTags(tags.includes(id) ? tags.filter((x) => x !== id) : [...tags, id].slice(0, MAX_POST_TAGS));

  return (
    <div className="forum-compose forum-edit">
      <div className="ft-head">
        <button type="button" className="icon-btn" title={t('글로 돌아가기')} onClick={cancel}>
          <ArrowLeft size={18} />
        </button>
        <div className="ft-title">
          <span>{t('글 수정')}</span>
        </div>
      </div>
      <div className="fc-scroll">
        <div className="fc-form">
          <Field label={t('제목')}>
            <input ref={titleRef} className="input" value={title} maxLength={MAX_TITLE} onChange={(e) => setTitle(e.target.value)} placeholder={t('무엇에 관한 글인가요?')} />
          </Field>
          {forumTags.length > 0 && (
            <div className="field">
              <span className="field-label">
                {t('태그 ({count}/{max})', { count: tags.length, max: MAX_POST_TAGS })}
              </span>
              <div className="chips">
                {forumTags.map((tg) => {
                  const on = tags.includes(tg.id);
                  return (
                    <button
                      key={tg.id}
                      type="button"
                      className={cx('chip', on && 'active')}
                      aria-pressed={on}
                      disabled={!on && tags.length >= MAX_POST_TAGS}
                      onClick={() => toggleTag(tg.id)}
                    >
                      {tg.emoji} {tg.name}
                    </button>
                  );
                })}
              </div>
              {canFeature && <span className="field-hint">{TAG_HINT}</span>}
            </div>
          )}
          {/* 정렬 단추가 있어 <label>(Field)로 감싸지 않습니다(이름표를 누르면 첫 단추가 눌림). */}
          <div className="field">
            <div className="fc-text-head">
              <span className="field-label">{t('내용')}</span>
              {text !== null && <AlignButtons textarea={textRef} text={text} onChange={setText} maxLength={MAX_LEN} />}
            </div>
            {text === null ? (
              <div className="center pad">
                <Spinner />
              </div>
            ) : (
              <textarea
                ref={textRef}
                aria-label={t('내용')}
                className="input fc-text"
                rows={12}
                value={text}
                maxLength={MAX_LEN}
                placeholder={t('내용을 쓰세요')}
                onChange={(e) => setText(e.target.value)}
                onKeyDown={(e) => {
                  if (e.nativeEvent.isComposing) return;
                  if (e.key === 'Enter' && (e.ctrlKey || e.metaKey)) {
                    e.preventDefault();
                    void submit();
                  }
                }}
              />
            )}
            <span className="field-hint">{MARKDOWN_EDIT_HINT}</span>
          </div>
          {(text ?? '').length > MAX_LEN - 500 && <div className="field-hint warn">{t('{n}자 남음', { n: MAX_LEN - (text ?? '').length })}</div>}
          {attachments.length > 0 && (
            <div className="field">
              <span className="field-label">{t('첨부')}</span>
              <div className="file-chips">
                {attachments.map((a) => (
                  <span key={a.blob} className="file-chip" title={plainName(a.name)}>
                    <Paperclip size={12} /> {plainName(a.name)}
                  </span>
                ))}
              </div>
              <span className="field-hint">{t('첨부는 고칠 수 없습니다. 바꾸려면 새 글로 올리세요.')}</span>
            </div>
          )}
        </div>
      </div>
      <div className="fc-foot">
        <button type="button" className="btn ghost" onClick={cancel}>
          {t('취소')}
        </button>
        <button type="button" className="btn primary" disabled={!ready || busy} onClick={() => void submit()}>
          {busy && <Spinner size={14} />} {t('저장')}
        </button>
      </div>
    </div>
  );
}
