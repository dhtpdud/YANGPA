// 포럼 새 글 작성 페이지(팝업이 아니라 포럼 채널 화면 안). 제목·태그(채널 설정에서 관리자가 만든 태그 중 5개까지 고름)·
// 본문(마크다운)·첨부(이미지·영상·파일, 붙여넣기 포함)와, 그 채널의 채널 관리 권한이 있으면 ‘대문으로 지정’. 첨부 고르기·동영상
// 썸네일 힌트는 입력창과 같은 hooks/attachments.ts를 씁니다.

import { ArrowLeft, Paperclip, Star } from 'lucide-react';
import { useEffect, useRef, useState } from 'react';
import { ANONYMOUS_USER, FORUM_PREVIEW_CHARS, type ChannelView, type ForumPostView, type SpaceView } from '../api/types';
import { MAX_FILES, NO_ATTACH_TEXT, usePendingFiles } from '../hooks/attachments';
import { t, tc } from '../i18n';
import { cx } from '../lib/misc';
import { clipboardFiles } from '../lib/paste';
import { hasChannelPerm } from '../lib/perms';
import { Field } from '../modals/Modal';
import { useStore } from '../store/store';
import { AnonIcon, Spinner } from './common';
import { AlignButtons } from './AlignButtons';
import { FileChips, MAX_LEN } from './Composer';

/** 채널 관리 권한이 있는 사람에게 보이는 태그 안내. */
export const TAG_HINT = t('태그는 채널 설정의 「태그」에서 만들고 고칩니다. 글쓴이는 그 목록에서 고릅니다.');

/** 게시글 태그 상한(데몬 MAX_POST_TAGS). */
export const MAX_POST_TAGS = 5;
export const MAX_TITLE = 100;

export const MARKDOWN_HINT = t(
  '마크다운을 쓸 수 있습니다: **굵게** *기울임* __밑줄__ ~~취소선~~ ||스포일러|| `코드` ```코드 블록``` > 인용 - 목록 # 제목 [글](https://주소), 정렬 단추(:::center … :::). Ctrl+Enter로 올리기',
);
/** 수정 페이지용(끝의 동작만 다름). 번역된 문구를 고쳐 쓰지 않으려고 따로 둡니다. */
export const MARKDOWN_EDIT_HINT = t(
  '마크다운을 쓸 수 있습니다: **굵게** *기울임* __밑줄__ ~~취소선~~ ||스포일러|| `코드` ```코드 블록``` > 인용 - 목록 # 제목 [글](https://주소), 정렬 단추(:::center … :::). Ctrl+Enter로 저장',
);

export function ForumCompose({
  space,
  channel,
  onCancel,
  onPosted,
}: {
  space: SpaceView;
  channel: ChannelView;
  onCancel: () => void;
  /** 올린 글(목록을 다시 읽기 전까지 쓸 임시 목록 항목). */
  onPosted: (post: ForumPostView) => void;
}) {
  const client = useStore((s) => s.client);
  const me = useStore((s) => s.me);
  const createForumPost = useStore((s) => s.createForumPost);
  const openModal = useStore((s) => s.openModal);
  const toast = useStore((s) => s.toast);
  const [title, setTitle] = useState('');
  const [text, setText] = useState('');
  const [tags, setTags] = useState<number[]>([]);
  const [featured, setFeatured] = useState(false);
  const [busy, setBusy] = useState(false);
  const volatile = channel.settings.security === 'volatile';
  const files = usePendingFiles(client, volatile, undefined, channel.attach_limit);
  // 첨부 한도 0은 첨부 금지 채널입니다.
  const attachBanned = channel.attach_limit === 0;
  const canAttach = hasChannelPerm(space, channel, me, 'attach_files') && !attachBanned;
  const canFeature = hasChannelPerm(space, channel, me, 'manage_channels');
  // 태그는 채널 설정에서 관리자가 미리 만들어 둔 것 중에서만 고릅니다.
  const forumTags = channel.settings.forum_tags;
  const titleRef = useRef<HTMLInputElement>(null);
  const textRef = useRef<HTMLTextAreaElement>(null);

  useEffect(() => titleRef.current?.focus(), []);

  // 파일 끌어 놓기(Tauri). 이 페이지가 떠 있는 동안에만 받습니다.
  const addRef = useRef(files.add);
  addRef.current = files.add;
  useEffect(() => {
    if (!client || !canAttach) return;
    return client.transport.onFileDrop((paths) => addRef.current(paths.map((p) => ({ path: p, name: p.split(/[\\/]/).pop() ?? p }))));
  }, [client, canAttach]);

  const body = text.trim();
  const ready = !!title.trim() && (!!body || files.files.length > 0) && body.length <= MAX_LEN;
  const dirty = !!title.trim() || !!body || files.files.length > 0;

  const cancel = () => {
    if (!dirty) {
      onCancel();
      return;
    }
    openModal({
      kind: 'confirm',
      title: t('작성 취소'),
      body: t('쓰던 글을 버릴까요? 지금까지 쓴 제목·내용·첨부는 남지 않습니다.'),
      danger: true,
      confirmLabel: t('버리기'),
      onConfirm: onCancel,
    });
  };

  const submit = async () => {
    if (!ready || busy) return;
    setBusy(true);
    const { inputs, hinted } = await files.collect();
    const asFeatured = canFeature && featured;
    const id = await createForumPost(space.space, channel.channel, { title: title.trim(), text: body, tags, attachments: inputs, featured: asFeatured });
    setBusy(false);
    if (!id) {
      if (hinted) files.dropHints();
      return;
    }
    const now = Date.now();
    onPosted({
      id,
      title: title.trim(),
      tags,
      author: channel.anonymous ? ANONYMOUS_USER : (me ?? ''),
      created_at: now,
      last_activity: now,
      reply_count: 0,
      pinned: false,
      locked: false,
      mine: true,
      // 데몬처럼 글자(코드 포인트) 단위로 앞부분만.
      preview: Array.from(body).slice(0, FORUM_PREVIEW_CHARS).join(''),
      attachment_count: inputs.length,
      ...(channel.anonymous ? { anonymous: true } : {}),
      ...(asFeatured ? { featured: true } : {}),
    });
  };

  const attach = async () => {
    if (!client) return;
    const picked = await client.transport.pickFiles({ multiple: true });
    if (picked.length) files.add(picked);
  };

  const toggleTag = (id: number) => setTags(tags.includes(id) ? tags.filter((x) => x !== id) : [...tags, id].slice(0, MAX_POST_TAGS));

  return (
    <div className="forum-compose">
      <div className="ft-head">
        <button type="button" className="icon-btn" title={t('목록으로')} onClick={cancel}>
          <ArrowLeft size={18} />
        </button>
        <div className="ft-title">
          <span>{t('새 글')}</span>
        </div>
      </div>
      <div className="fc-scroll">
        <div className="fc-form">
          {channel.anonymous && (
            <div className="note">
              <AnonIcon size={16} />
              <span>{t("익명 포럼입니다. 이 글과 답글의 글쓴이는 다른 사람에게 '익명'으로 보입니다.")}</span>
            </div>
          )}
          <Field label={t('제목')}>
            <input
              ref={titleRef}
              className="input"
              value={title}
              maxLength={MAX_TITLE}
              onChange={(e) => setTitle(e.target.value)}
              placeholder={t('무엇에 관한 글인가요?')}
            />
          </Field>
          {forumTags.length > 0 ? (
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
          ) : (
            canFeature && (
              <div className="field">
                <span className="field-label">{t('태그')}</span>
                <span className="field-hint">{TAG_HINT}</span>
              </div>
            )
          )}
          {/* 정렬 단추가 있어 <label>(Field)로 감싸지 않습니다(이름표를 누르면 첫 단추가 눌림). */}
          <div className="field">
            <div className="fc-text-head">
              <span className="field-label">{t('내용')}</span>
              <AlignButtons textarea={textRef} text={text} onChange={setText} maxLength={MAX_LEN} />
            </div>
            <textarea
              ref={textRef}
              aria-label={t('내용')}
              className="input fc-text"
              rows={12}
              value={text}
              maxLength={MAX_LEN}
              placeholder={t('내용을 쓰세요')}
              onChange={(e) => setText(e.target.value)}
              onPaste={(e) => {
                const pasted = canAttach ? clipboardFiles(e.clipboardData) : [];
                if (!pasted.length) return;
                e.preventDefault();
                void files.paste(pasted).then((err) => err && toast('error', err));
              }}
              onKeyDown={(e) => {
                if (e.nativeEvent.isComposing) return;
                if (e.key === 'Enter' && (e.ctrlKey || e.metaKey)) {
                  e.preventDefault();
                  void submit();
                }
              }}
            />
            <span className="field-hint">{MARKDOWN_HINT}</span>
          </div>
          {text.length > MAX_LEN - 500 && <div className="field-hint warn">{t('{n}자 남음', { n: MAX_LEN - text.length })}</div>}
          <div className="field">
            <span className="field-label">{t('첨부')}</span>
            <div className="fc-attach">
              <button
                type="button"
                className="btn small"
                disabled={!canAttach || files.files.length >= MAX_FILES}
                title={canAttach ? undefined : attachBanned ? NO_ATTACH_TEXT : t('이 채널에 파일을 첨부할 권한이 없습니다')}
                onClick={() => void attach()}
              >
                <Paperclip size={14} /> {t('파일 첨부')}
              </button>
              <span className="field-hint">{t('이미지·영상·파일 {n}개까지. 첫 이미지·영상이 갤러리 보기의 표지가 됩니다.', { n: MAX_FILES })}</span>
            </div>
            <FileChips files={files} />
          </div>
          {canFeature && (
            <label className="check fc-featured">
              <input type="checkbox" checked={featured} onChange={(e) => setFeatured(e.target.checked)} />
              <Star size={14} />
              <span>
                {t('대문으로 지정')}
                <span className="field-hint"> · {t('포럼에 들어오면 이 글 전체가 맨 위에 보입니다(지금 대문이 있으면 바뀝니다)')}</span>
              </span>
            </label>
          )}
        </div>
      </div>
      <div className="fc-foot">
        <button type="button" className="btn ghost" onClick={cancel}>
          {t('취소')}
        </button>
        <button type="button" className="btn primary" disabled={!ready || busy} onClick={() => void submit()}>
          {busy && <Spinner size={14} />} {tc('게시글 올리기 단추', '올리기')}
        </button>
      </div>
    </div>
  );
}
