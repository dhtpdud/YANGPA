// 포럼 채널: 대문 게시글(있으면 전체 내용) → 글 목록(정렬·방향, 태그 여러 개와 일부/모두 일치, 목록/갤러리 보기) → 글을 누르면
// 글타래(맨 위 게시글 + 댓글 + 입력창). 새 글은 팝업이 아니라 이 화면 안의 작성 페이지(ForumCompose)에서 쓰고, 올리면 그 글을 엽니다.
// 내 글은 같은 틀의 수정 페이지(ForumEdit)에서 고치고(수정 금지 채널이면 못 고침), 고친 글에는 '(수정됨) 일시'가 붙습니다.
// 목록은 새 글·글 변경·새 댓글 알림(forumTick)과 내가 바꾼 뒤 다시 읽습니다. 게시글 본문은 데몬이 글타래 첫 페이지에 함께 주는
// 게시글 메시지로 그립니다(MessageList의 게시글 행). 보기 설정(정렬·방향·보기·태그 맞춤)은 채널마다 이 기기에 기억합니다.

import { ArrowLeft, Film, Image as ImageIcon, LayoutGrid, List, Lock, MessageSquare, Paperclip, Pencil, Pin, Play, Plus, Star } from 'lucide-react';
import { useEffect, useMemo, useRef, useState, type MouseEvent } from 'react';
import { isRenderableMime } from '../api/client';
import {
  ANONYMOUS_USER,
  FORUM_PREVIEW_CHARS,
  type ChannelView,
  type EventId,
  type ForumPostView,
  type ForumSort,
  type ForumTag,
  type MemberView,
  type SortOrder,
  type SpaceView,
  type TagMatch,
} from '../api/types';
import { useBlobUrl, useThumbUrl } from '../hooks/media';
import { t } from '../i18n';
import { anonName, isAnonymousUser } from '../lib/anon';
import { formatFull, formatRelative } from '../lib/format';
import { cx, displayName, excerpt, rememberRecent } from '../lib/misc';
import { hasChannelPerm } from '../lib/perms';
import { DEFAULT_FORUM_VIEW, MAX_FORUM_VIEWS, chKey, useStore, type ForumViewPrefs } from '../store/store';
import { Avatar, Empty, Spinner } from './common';
import { Composer } from './Composer';
import { ForumCompose } from './ForumCompose';
import { ForumEdit } from './ForumEdit';
import { MessageItem } from './MessageItem';
import { MessageList } from './MessageList';

function PostTags({ ids, tags }: { ids: number[]; tags: ForumTag[] }) {
  return (
    <div className="post-tags">
      {ids.map((id) => {
        const tg = tags.find((x) => x.id === id);
        return tg ? (
          <span key={id} className="tag">
            {tg.emoji} {tg.name}
          </span>
        ) : null;
      })}
    </div>
  );
}

function FeaturedBadge() {
  return (
    <span className="featured-badge" title={t('대문 게시글')}>
      <Star size={11} /> {t('대문')}
    </span>
  );
}

/** 고친 게시글 표시: '(수정됨) 일시'. */
function EditedAt({ at }: { at?: number }) {
  return at ? (
    <span className="edited post-edited" title={t('{time}에 수정', { time: formatFull(at) })}>
      {t('(수정됨) {time}', { time: formatFull(at) })}
    </span>
  ) : null;
}

/** 글타래 맨 위 게시글의 제목·태그·고친 일시(본문·첨부·작성자는 게시글 메시지가 그 아래에 그림). `editedAt`은 게시글 메시지의 값이 더 새로우면 그것. */
export function ForumPostHeader({ post, tags, featured, editedAt }: { post: ForumPostView; tags: ForumTag[]; featured?: boolean; editedAt?: number }) {
  return (
    <div className="forum-post-head">
      <h2 className="forum-post-title">
        {post.pinned && <Pin size={16} className="pin" />}
        {post.locked && <Lock size={16} className="muted" />}
        <span>{post.title}</span>
        {featured && <FeaturedBadge />}
      </h2>
      {post.tags.length > 0 && <PostTags ids={post.tags} tags={tags} />}
      <EditedAt at={editedAt ?? post.edited_at} />
    </div>
  );
}

/** 익명 글의 작성자 자리(ANONYMOUS_USER)는 멤버 목록에서 찾지 않습니다. */
function authorOf(p: ForumPostView, members: Map<string, MemberView>): { id: string; name: string } {
  const anon = !!p.anonymous || isAnonymousUser(p.author);
  return anon ? { id: ANONYMOUS_USER, name: anonName(p) } : { id: p.author, name: displayName(members.get(p.author)) };
}

/** 목록·갤러리의 미리보기 한 줄(마크다운을 뺀 평문). 데몬이 앞부분만 주므로 잘렸으면 닫히지 않은 서식도 닫힌 것으로 봅니다. */
export function previewText(p: Pick<ForumPostView, 'preview'>): string {
  const cut = Array.from(p.preview ?? '').length >= FORUM_PREVIEW_CHARS;
  const text = excerpt(p.preview ?? '', FORUM_PREVIEW_CHARS, { lenient: cut });
  return cut && text && !text.endsWith('…') ? `${text}…` : text;
}

function PostMeta({ post, members }: { post: ForumPostView; members: Map<string, MemberView> }) {
  const a = authorOf(post, members);
  return (
    <div className="post-meta">
      <Avatar id={a.id} name={a.name} size={18} />
      <span>{a.name}</span>
      <span className="dot">·</span>
      <span>{t('{time} 작성', { time: formatRelative(post.created_at) })}</span>
      {post.edited_at && (
        <>
          <span className="dot">·</span>
          <EditedAt at={post.edited_at} />
        </>
      )}
      {post.attachment_count > 0 && (
        <>
          <span className="dot">·</span>
          <span className="post-atts" title={t('첨부 {n}개', { n: post.attachment_count })}>
            <Paperclip size={12} /> {post.attachment_count}
          </span>
        </>
      )}
    </div>
  );
}

function PostTitle({ post }: { post: ForumPostView }) {
  return (
    <div className="post-title">
      {post.pinned && <Pin size={14} className="pin" />}
      {post.locked && <Lock size={14} className="muted" />}
      {post.featured && <FeaturedBadge />}
      <span className="post-title-text">{post.title}</span>
    </div>
  );
}

function PostRow({ post, tags, members, onOpen }: { post: ForumPostView; tags: ForumTag[]; members: Map<string, MemberView>; onOpen: () => void }) {
  const preview = previewText(post);
  return (
    <button type="button" className="post" onClick={onOpen}>
      <div className="post-main">
        <PostTitle post={post} />
        <PostTags ids={post.tags} tags={tags} />
        {preview && <div className="post-preview">{preview}</div>}
        <PostMeta post={post} members={members} />
      </div>
      <div className="post-side">
        <span className="post-replies">
          <MessageSquare size={14} /> {post.reply_count}
        </span>
        <span className="muted small">{formatRelative(post.last_activity)}</span>
      </div>
    </button>
  );
}

/** 갤러리 표지: 첫 이미지·영상의 썸네일. 썸네일이 없으면 이미 받은 이미지 원본, 그것도 없으면 아이콘이나 미리보기 글. */
function Cover({ space, post }: { space: string; post: ForumPostView }) {
  const c = post.cover;
  const isVideo = !!c && c.mime.startsWith('video/');
  const thumb = useThumbUrl(c?.thumbnail_base64, false);
  // 표지 때문에 받지 않은 원본을 받지는 않습니다.
  const direct = !!c && !thumb && c.mime.startsWith('image/') && c.downloaded && isRenderableMime(c.mime);
  const src = useBlobUrl(direct && c ? { space, blob: c.blob, keyFrom: post.id, mime: c.mime, volatile: false } : null);
  const url = thumb ?? src.url;
  if (url) {
    return (
      <div className="pc-cover">
        <img src={url} alt="" draggable={false} decoding="async" />
        {isVideo && (
          <span className="pc-play">
            <Play size={16} />
          </span>
        )}
      </div>
    );
  }
  if (c) return <div className="pc-cover empty">{isVideo ? <Film size={30} /> : <ImageIcon size={30} />}</div>;
  const preview = previewText(post);
  return <div className="pc-cover text">{preview || <MessageSquare size={30} />}</div>;
}

function PostCard({
  space,
  post,
  tags,
  members,
  onOpen,
}: {
  space: string;
  post: ForumPostView;
  tags: ForumTag[];
  members: Map<string, MemberView>;
  onOpen: () => void;
}) {
  return (
    <button type="button" className="post-card" onClick={onOpen}>
      <Cover space={space} post={post} />
      <div className="pc-body">
        <PostTitle post={post} />
        <PostTags ids={post.tags} tags={tags} />
        <PostMeta post={post} members={members} />
        <div className="pc-foot">
          <span className="post-replies">
            <MessageSquare size={14} /> {post.reply_count}
          </span>
          <span className="muted small">{formatRelative(post.last_activity)}</span>
        </div>
      </div>
    </button>
  );
}

/** 대문 글의 첫 메시지를 찾을 때 더 읽을 이전 페이지 상한(댓글이 아주 많은 글). */
const MAX_HEAD_PAGES = 30;

/**
 * 게시글 자체(글타래의 첫 메시지: 본문·첨부). 데몬은 글타래의 맨 앞 페이지에만 게시글을 넣어 주므로, 글타래를 스토어로 읽고
 * (글을 열면 그대로 씀, 고침·반응 알림도 반영됨) 게시글이 나올 때까지 이전 페이지를 더 읽습니다.
 */
function usePostHead(space: string, channel: string, post: EventId) {
  const key = chKey(space, channel, post);
  const st = useStore((s) => s.channels[key]);
  const loadLatest = useStore((s) => s.loadLatest);
  const loadOlder = useStore((s) => s.loadOlder);
  const head = st?.messages.find((m) => m.id === post);
  const len = st?.messages.length ?? 0;
  const tried = useRef({ len: -1, pages: 0 });
  const [stuck, setStuck] = useState(false);
  useEffect(() => {
    if (head) return;
    if (!st) {
      void loadLatest(space, channel, post);
      return;
    }
    if (!st.loaded || st.loadingOlder || st.reachedStart || st.error) return;
    // 같은 길이에서 다시 부르지 않습니다(이전 페이지 읽기가 실패해도 되풀이하지 않게).
    if (tried.current.len === len || tried.current.pages >= MAX_HEAD_PAGES) {
      setStuck(true);
      return;
    }
    tried.current = { len, pages: tried.current.pages + 1 };
    void loadOlder(space, channel, post);
  }, [head, st, len, space, channel, post, loadLatest, loadOlder]);
  const loading = !head && !stuck && (!st || !st.loaded || st.loadingOlder || (!st.reachedStart && !st.error));
  return { key, head, loading };
}

/** 대문: 포럼에 들어오면 맨 위에 보이는 게시글 전체(제목·태그·작성자·본문·첨부). 누르면 그 글을 엽니다. */
/** 데몬의 태그 거르기와 같은 규칙: 일부 일치는 고른 태그 중 하나라도, 모두 일치는 글의 태그가 고른 태그와 꼭 같아야 합니다. */
export function matchesTags(postTags: readonly number[], picked: readonly number[], match: TagMatch): boolean {
  if (picked.length === 0) return true;
  return match === 'all'
    ? picked.every((x) => postTags.includes(x)) && postTags.every((x) => picked.includes(x))
    : picked.some((x) => postTags.includes(x));
}

function FeaturedPost({
  space,
  channel,
  post: postId,
  members,
  canFeature,
  picked,
  match,
  onOpen,
  onUnfeature,
}: {
  space: SpaceView;
  channel: ChannelView;
  post: EventId;
  members: Map<string, MemberView>;
  canFeature: boolean;
  /** 고른 태그와 맞춤 방식. 대문 글도 목록처럼 맞지 않으면 숨깁니다. */
  picked: readonly number[];
  match: TagMatch;
  onOpen: (p: ForumPostView) => void;
  onUnfeature: () => void;
}) {
  const client = useStore((s) => s.client);
  const forumTick = useStore((s) => s.forumTick);
  const me = useStore((s) => s.me);
  const [meta, setMeta] = useState<ForumPostView | 'gone' | null>(null);
  useEffect(() => {
    if (!client) return;
    let stale = false;
    client
      .call('forum.get_post', { space: space.space, channel: channel.channel, post: postId })
      .then((p) => !stale && setMeta(p))
      .catch(() => !stale && setMeta('gone'));
    return () => {
      stale = true;
    };
  }, [client, space.space, channel.channel, postId, forumTick]);
  const { key, head, loading } = usePostHead(space.space, channel.channel, postId);

  if (meta === 'gone' || head?.deleted) return null;
  if (meta && !matchesTags(meta.tags, picked, match)) return null;
  if (!meta) {
    return (
      <section className="forum-featured loading">
        <Spinner />
      </section>
    );
  }
  const open = () => onOpen(meta);
  // 본문 안의 링크·멘션·이미지·스포일러를 누르거나 글자를 고르는 중이면 글을 열지 않습니다.
  const onClick = (e: MouseEvent) => {
    if ((e.target as HTMLElement).closest('a, button, input, textarea, select, video, audio, img, .spoiler, [role="button"]')) return;
    if (window.getSelection?.()?.toString()) return;
    open();
  };
  const preview = previewText(meta);
  return (
    <section className="forum-featured" onClick={onClick} aria-label={t('대문 게시글')}>
      <div className="ff-top">
        <FeaturedBadge />
        {canFeature && (
          <button type="button" className="btn small" onClick={onUnfeature}>
            {t('대문 해제')}
          </button>
        )}
      </div>
      <h2 className="forum-post-title ff-title">
        {meta.pinned && <Pin size={16} className="pin" />}
        {meta.locked && <Lock size={16} className="muted" />}
        <button type="button" className="ff-open" onClick={open}>
          {meta.title}
        </button>
      </h2>
      {meta.tags.length > 0 && <PostTags ids={meta.tags} tags={channel.settings.forum_tags} />}
      <div className="ff-body">
        {head ? (
          <MessageItem
            m={head}
            grouped={false}
            space={space.space}
            channel={channel.channel}
            msgKey={key}
            members={members}
            roles={space.roles}
            me={me}
            volatile={false}
            canManage={false}
            canReact={false}
            isEditing={false}
            readOnly
            post
          />
        ) : (
          <div className="ff-preview">
            {preview && <p>{preview}</p>}
            {loading && <Spinner />}
          </div>
        )}
      </div>
      <div className="ff-foot">
        <button type="button" className="link-btn" onClick={open}>
          <MessageSquare size={14} /> {t('댓글 {n}개 · 글 열기', { n: meta.reply_count })}
        </button>
      </div>
    </section>
  );
}

type Mode = { kind: 'list' } | { kind: 'compose' } | { kind: 'post'; post: ForumPostView } | { kind: 'edit'; post: ForumPostView };

export function ForumView({ space, channel }: { space: SpaceView; channel: ChannelView }) {
  const client = useStore((s) => s.client);
  const me = useStore((s) => s.me);
  const forumTick = useStore((s) => s.forumTick);
  const loadLatest = useStore((s) => s.loadLatest);
  const call = useStore((s) => s.call);
  const refreshSpace = useStore((s) => s.refreshSpace);
  const setPrefs = useStore((s) => s.setPrefs);
  const fkey = chKey(space.space, channel.channel);
  const saved = useStore((s) => s.prefs.forumView?.[fkey]);
  const [mode, setMode] = useState<Mode>({ kind: 'list' });
  const [picked, setPicked] = useState<number[]>([]);
  const [posts, setPosts] = useState<ForumPostView[] | null>(null);
  // 휘발성 채널의 보기 설정은 설정 파일에 남기지 않습니다(마지막 채널과 같은 규칙).
  const volatile = channel.settings.security === 'volatile';
  const [local, setLocal] = useState<ForumViewPrefs>(DEFAULT_FORUM_VIEW);
  const pv: ForumViewPrefs = volatile ? local : { ...DEFAULT_FORUM_VIEW, ...saved };
  // 내가 고정·잠금·대문을 바꾼 뒤 알림을 기다리지 않고 다시 읽습니다.
  const [reload, setReload] = useState(0);
  const tags = channel.settings.forum_tags;
  // 채널에서 지운 태그는 고른 목록에서도 뺍니다.
  const selected = picked.filter((id) => tags.some((x) => x.id === id));
  const tagKey = selected.join(',');
  const members = useMemo(() => new Map(space.members.map((m) => [m.user, m])), [space.members]);
  const canPost = hasChannelPerm(space, channel, me, 'create_posts');
  const canManage = hasChannelPerm(space, channel, me, 'manage_messages');
  const canFeature = hasChannelPerm(space, channel, me, 'manage_channels');
  const featured = channel.featured_post;

  const setView = (patch: Partial<ForumViewPrefs>) => {
    const next = { ...pv, ...patch };
    if (volatile) setLocal(next);
    else setPrefs({ forumView: rememberRecent(useStore.getState().prefs.forumView ?? {}, fkey, next, MAX_FORUM_VIEWS) });
  };

  useEffect(() => {
    if (!client) return;
    let stale = false;
    const ids = tagKey ? tagKey.split(',').map(Number) : [];
    client
      .call('forum.list_posts', {
        space: space.space,
        channel: channel.channel,
        sort: pv.sort,
        order: pv.order,
        ...(ids.length ? { tags: ids, tag_match: pv.match } : {}),
      })
      .then((p) => !stale && setPosts(p))
      .catch(() => !stale && setPosts([]));
    return () => {
      stale = true;
    };
  }, [client, space.space, channel.channel, pv.sort, pv.order, pv.match, tagKey, forumTick, reload, space]);

  // 검색 결과에서 고른 게시글·댓글: 그 게시글을 열고, 댓글이면 글타래 안에서 찾게 넘깁니다.
  const jump = useStore((s) => (s.jump?.key === fkey && s.jump.post ? s.jump : null));
  useEffect(() => {
    if (!jump?.post || !client) return;
    let stale = false;
    const go = (p: ForumPostView) => {
      if (stale) return;
      setMode({ kind: 'post', post: p });
      useStore.setState({ jump: jump.id === p.id ? null : { key: chKey(space.space, channel.channel, p.id), id: jump.id } });
    };
    client
      .call('forum.get_post', { space: space.space, channel: channel.channel, post: jump.post })
      .then(go)
      .catch(() => {
        if (stale) return;
        useStore.setState({ jump: null });
        useStore.getState().toast('info', t('이 기기에 그 게시글이 없습니다(지워졌거나 아직 받지 못함)'));
      });
    return () => {
      stale = true;
    };
  }, [jump, client, space.space, channel.channel]);

  const openPost = mode.kind === 'post' ? mode.post : null;
  // 게시글 메시지의 고친 시각(수정 알림으로 목록보다 먼저 바뀜).
  const headEdited = useStore((s) =>
    openPost ? s.channels[chKey(space.space, channel.channel, openPost.id)]?.messages.find((m) => m.id === openPost.id)?.edited_at : undefined,
  );
  useEffect(() => {
    if (!openPost) return;
    const loaded = useStore.getState().channels[chKey(space.space, channel.channel, openPost.id)]?.loaded;
    if (!loaded) void loadLatest(space.space, channel.channel, openPost.id);
  }, [openPost, loadLatest, space.space, channel.channel]);

  const setFeatured = async (post: EventId | null) => {
    if (!client) return;
    const r = await call(client.call('forum.set_featured', { space: space.space, channel: channel.channel, post }), {
      ok: post ? t('대문으로 지정했습니다') : t('대문에서 내렸습니다'),
    });
    if (r === undefined) return;
    // space.updated 알림을 기다리지 않고 채널 보기(featured_post)와 목록 표시를 다시 읽습니다.
    refreshSpace(space.space);
    setReload((n) => n + 1);
  };

  if (mode.kind === 'compose') {
    return <ForumCompose space={space} channel={channel} onCancel={() => setMode({ kind: 'list' })} onPosted={(post) => setMode({ kind: 'post', post })} />;
  }

  if (mode.kind === 'edit') {
    return (
      <ForumEdit
        space={space}
        channel={channel}
        post={mode.post}
        onCancel={() => setMode({ kind: 'post', post: mode.post })}
        onSaved={(post) => {
          setMode({ kind: 'post', post });
          setReload((n) => n + 1);
        }}
      />
    );
  }

  if (mode.kind === 'post') {
    // 방금 고친 글은 목록을 다시 읽기 전까지 고친 값(mode.post)을 씁니다.
    const listed = posts?.find((p) => p.id === mode.post.id);
    const post = listed && (listed.edited_at ?? 0) >= (mode.post.edited_at ?? 0) ? listed : mode.post;
    // 내 글이고 수정 금지 채널이 아니면 수정 페이지를 엽니다(익명 글도 내 글이면 mine).
    const canEdit = !!post.mine && !channel.no_edit && !!client;
    const editPost = canEdit ? () => setMode({ kind: 'edit', post }) : undefined;
    const isFeatured = featured === post.id;
    const update = async (patch: { pinned?: boolean; locked?: boolean }) => {
      if (!client) return;
      const r = await call(client.call('forum.update_post', { space: space.space, channel: channel.channel, post: post.id, ...patch }));
      if (r !== undefined) setReload((n) => n + 1);
    };
    return (
      <div className="forum-thread">
        <div className="ft-head">
          <button type="button" className="icon-btn" title={t('목록으로')} onClick={() => setMode({ kind: 'list' })}>
            <ArrowLeft size={18} />
          </button>
          <div className="ft-title">
            {post.pinned && <Pin size={14} />}
            {post.locked && <Lock size={14} />}
            <span>{post.title}</span>
          </div>
          {client && (canManage || canFeature || canEdit) && (
            <div className="ft-actions">
              {editPost && (
                <button type="button" className="btn small" onClick={editPost}>
                  <Pencil size={13} /> {t('수정')}
                </button>
              )}
              {canFeature && (
                <button type="button" className={cx('btn small', isFeatured && 'active')} onClick={() => void setFeatured(isFeatured ? null : post.id)}>
                  <Star size={13} /> {isFeatured ? t('대문 해제') : t('대문으로 지정')}
                </button>
              )}
              {canManage && (
                <>
                  <button type="button" className="btn small" onClick={() => void update({ pinned: !post.pinned })}>
                    <Pin size={13} /> {post.pinned ? t('고정 해제') : t('고정')}
                  </button>
                  <button type="button" className="btn small" onClick={() => void update({ locked: !post.locked })}>
                    <Lock size={13} /> {post.locked ? t('잠금 해제') : t('잠금')}
                  </button>
                </>
              )}
            </div>
          )}
        </div>
        <MessageList
          key={post.id}
          space={space.space}
          channel={channel.channel}
          thread={post.id}
          channelView={channel}
          postHeader={<ForumPostHeader post={post} tags={tags} featured={isFeatured} editedAt={headEdited} />}
          onEditPost={editPost}
        />
        {post.locked && !canManage ? (
          <div className="locked-note">
            <Lock size={14} /> {t('잠긴 글입니다')}
          </div>
        ) : (
          <Composer space={space.space} channel={channel.channel} thread={post.id} channelView={channel} placeholder={t('「{title}」에 답글', { title: post.title })} />
        )}
      </div>
    );
  }

  const open = (p: ForumPostView) => setMode({ kind: 'post', post: p });
  const toggleTag = (id: number) => setPicked(selected.includes(id) ? selected.filter((x) => x !== id) : [...selected, id]);

  return (
    <div className="forum">
      <div className="forum-bar">
        <div className="forum-controls">
          <select className="select small" value={pv.sort} title={t('정렬 기준')} aria-label={t('정렬 기준')} onChange={(e) => setView({ sort: e.target.value as ForumSort })}>
            <option value="activity">{t('업데이트 시간')}</option>
            <option value="created">{t('최초 작성 시간')}</option>
          </select>
          <select className="select small" value={pv.order} title={t('정렬 방향')} aria-label={t('정렬 방향')} onChange={(e) => setView({ order: e.target.value as SortOrder })}>
            <option value="desc">{t('내림차순(최신 먼저)')}</option>
            <option value="asc">{t('오름차순(오래된 것 먼저)')}</option>
          </select>
          <div className="seg" role="group" aria-label={t('보기 형식')}>
            <button type="button" className={cx(pv.view === 'list' && 'active')} aria-pressed={pv.view === 'list'} onClick={() => setView({ view: 'list' })}>
              <List size={14} /> {t('목록')}
            </button>
            <button type="button" className={cx(pv.view === 'gallery' && 'active')} aria-pressed={pv.view === 'gallery'} onClick={() => setView({ view: 'gallery' })}>
              <LayoutGrid size={14} /> {t('갤러리')}
            </button>
          </div>
          <span className="grow" />
          {canPost && (
            <button type="button" className="btn primary small" onClick={() => setMode({ kind: 'compose' })}>
              <Plus size={14} /> {t('새 글')}
            </button>
          )}
        </div>
        {tags.length > 0 && (
          <div className="forum-tags">
            <button type="button" className={cx('chip', selected.length === 0 && 'active')} onClick={() => setPicked([])}>
              {t('전체')}
            </button>
            {tags.map((tg) => (
              <button key={tg.id} type="button" className={cx('chip', selected.includes(tg.id) && 'active')} aria-pressed={selected.includes(tg.id)} onClick={() => toggleTag(tg.id)}>
                {tg.emoji} {tg.name}
              </button>
            ))}
            {tags.length > 1 && (
              <div className="seg tag-match" role="group" aria-label={t('태그 매칭')} title={t('태그를 여러 개 골랐을 때')}>
                <button type="button" className={cx(pv.match === 'any' && 'active')} aria-pressed={pv.match === 'any'} onClick={() => setView({ match: 'any' })}>
                  {t('일부 일치')}
                </button>
                <button type="button" className={cx(pv.match === 'all' && 'active')} aria-pressed={pv.match === 'all'} onClick={() => setView({ match: 'all' })}>
                  {t('모두 일치')}
                </button>
              </div>
            )}
          </div>
        )}
      </div>
      <div className="forum-list">
        {featured && (
          <FeaturedPost
            key={featured}
            space={space}
            channel={channel}
            post={featured}
            members={members}
            canFeature={canFeature && !!client}
            picked={selected}
            match={pv.match}
            onOpen={open}
            onUnfeature={() => void setFeatured(null)}
          />
        )}
        {posts === null && (
          <div className="center pad">
            <Spinner />
          </div>
        )}
        {posts?.length === 0 && <Empty icon={<MessageSquare size={36} />} title={selected.length ? t('고른 태그에 맞는 글이 없습니다') : t('아직 글이 없습니다')} />}
        {posts && posts.length > 0 && pv.view === 'gallery' && (
          <div className="post-grid">
            {posts.map((p) => (
              <PostCard key={p.id} space={space.space} post={p} tags={tags} members={members} onOpen={() => open(p)} />
            ))}
          </div>
        )}
        {posts && pv.view === 'list' && posts.map((p) => <PostRow key={p.id} post={p} tags={tags} members={members} onOpen={() => open(p)} />)}
      </div>
    </div>
  );
}
