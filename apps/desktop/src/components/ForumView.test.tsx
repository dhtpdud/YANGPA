// @vitest-environment happy-dom
// 포럼: 새 글 알림(forumTick)이면 글 목록을 다시 읽고, 글을 열면 글타래 맨 위에 게시글 본문을 한 번만 그린 뒤 댓글을 잇습니다.
// 새 글은 채널 화면 안의 작성 페이지에서 쓰고, 목록은 정렬·방향·보기·태그 맞춤을 채널마다 기억하며, 대문 글은 목록 위에 전체 내용을 그립니다.
// 가상 스크롤과 메시지 한 줄은 가짜로 바꿔 어떤 메시지가 어디에 그려지는지만 봅니다.

import { act } from 'react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import type { ApiClient, PickedFile } from '../api/client';
import { ANONYMOUS_USER, type ChannelView, type ForumPostView, type MessageView, type SpaceView } from '../api/types';
import { applyBatch } from '../store/apply';
import { chKey, useStore } from '../store/store';
import { cleanup, click, render } from '../test/dom';
import { ForumView, matchesTags } from './ForumView';
import { TAG_HINT } from './ForumCompose';

vi.mock('@tanstack/react-virtual', () => ({
  useVirtualizer: (o: { count: number }) => ({
    scrollToEnd: () => undefined,
    scrollToIndex: () => undefined,
    getVirtualItems: () => Array.from({ length: o.count }, (_, i) => ({ index: i, key: i, start: i * 40, size: 40, end: (i + 1) * 40, lane: 0 })),
    getTotalSize: () => o.count * 40,
    isAtEnd: () => true,
    measureElement: () => undefined,
  }),
}));

vi.mock('./MessageItem', () => ({
  MessageItem: ({ m, post }: { m: MessageView; post?: boolean }) => <div className={post ? 'm is-post' : 'm'}>{m.text}</div>,
}));

// 입력창만 가짜로(작성 페이지는 진짜 첨부 칩을 씁니다).
vi.mock('./Composer', async (importOriginal) => ({
  ...(await importOriginal<typeof import('./Composer')>()),
  Composer: () => <div className="composer" />,
}));

const OTHER = 'O'.repeat(32);
const ME = 'M'.repeat(32);
const THUMB = btoa('\xff\xd8\xff\xe0');

function makeSpace(perms: string[]): SpaceView {
  return {
    space: 'S',
    kind: 'crew',
    settings: { name: '크루' },
    members: [{ user: OTHER, profile: { name: '상대' }, roles: [], online: true, automation: false }],
    roles: [],
    channels: [],
    // 채널 보기는 기본 역할(@everyone)에 있습니다(없으면 채널 권한이 모두 거짓).
    my_permissions: ['view_channel', ...perms],
  } as unknown as SpaceView;
}

function makeChannel(extra: Partial<ChannelView> = {}, settings: Record<string, unknown> = {}): ChannelView {
  return {
    channel: 'F',
    settings: {
      name: '질문방',
      kind: 'forum',
      forum_tags: [
        { id: 1, name: '질문', emoji: '❓' },
        { id: 2, name: '버그', emoji: '🐞' },
      ],
      overwrites: [],
      ...settings,
    },
    unread: 0,
    ...extra,
  } as unknown as ChannelView;
}

function post(extra: Partial<ForumPostView> = {}): ForumPostView {
  return {
    id: 'p1',
    title: '첫 글 제목',
    author: OTHER,
    created_at: 1000,
    last_activity: 2000,
    reply_count: 1,
    tags: [1],
    pinned: false,
    locked: false,
    preview: '게시글 본문',
    attachment_count: 0,
    ...extra,
  };
}

const msg = (id: string, lamport: number, text: string, extra: Partial<MessageView> = {}) =>
  ({ id, lamport, author: OTHER, author_device: 'd', created_at: lamport * 1000, text, attachments: [], embeds: [], reactions: [], deleted: false, volatile: false, ...extra }) as MessageView;

interface Setup {
  perms?: string[];
  channel?: ChannelView;
  picked?: PickedFile[];
  getPost?: (id: string) => ForumPostView | undefined;
}

function setup(posts: () => ForumPostView[], o: Setup = {}) {
  const sp = makeSpace(o.perms ?? ['create_posts']);
  const ch = o.channel ?? makeChannel();
  const view = { ...sp, channels: [ch] };
  const call = vi.fn(async (method: string, params?: Record<string, unknown>) => {
    if (method === 'forum.list_posts') return posts();
    // 데몬은 글타래 첫 페이지에 게시글 자체(ID = 글 ID, 글타래 없음)를 맨 앞에 함께 줍니다.
    if (method === 'message.list') {
      const id = String(params?.thread);
      return { messages: [msg(id, 1, '게시글 본문'), msg('c1', 2, '첫 댓글', { thread: id })], gaps: [] };
    }
    if (method === 'forum.get_post') {
      const p = (o.getPost ?? ((id: string) => posts().find((x) => x.id === id)))(String(params?.post));
      if (!p) throw new Error('없음');
      return p;
    }
    if (method === 'forum.create_post') return { id: 'p9' };
    if (method === 'space.get') return view;
    return {};
  });
  const transport = {
    pickFiles: vi.fn(async () => o.picked ?? []),
    onFileDrop: () => () => undefined,
    savePrefs: vi.fn(async () => undefined),
    openExternal: vi.fn(async () => undefined),
  };
  useStore.setState({
    client: { call, transport } as unknown as ApiClient,
    views: { S: view },
    channels: {},
    forumTick: 0,
    me: ME,
    modal: null,
    toasts: [],
    prefs: { ...useStore.getState().prefs, forumView: {} },
  });
  return { call, transport, space: sp, channel: ch };
}

const calls = (call: ReturnType<typeof setup>['call'], method: string) =>
  call.mock.calls.filter(([m]) => m === method).map(([, p]) => p as Record<string, unknown>);

async function typeInto(input: HTMLInputElement | HTMLTextAreaElement, value: string) {
  await act(async () => {
    const proto = input instanceof HTMLTextAreaElement ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype;
    Object.getOwnPropertyDescriptor(proto, 'value')!.set!.call(input, value);
    input.dispatchEvent(new Event('input', { bubbles: true }));
  });
}

async function choose(select: HTMLSelectElement, value: string) {
  await act(async () => {
    Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype, 'value')!.set!.call(select, value);
    select.dispatchEvent(new Event('change', { bubbles: true }));
  });
}

function btn(root: Element, text: string): HTMLButtonElement {
  const b = [...root.querySelectorAll('button')].find((x) => x.textContent?.trim() === text || x.textContent?.trim().endsWith(` ${text}`));
  if (!b) throw new Error(`버튼 없음: ${text}`);
  return b;
}

afterEach(async () => {
  await cleanup();
  useStore.setState({ client: null, views: {}, channels: {}, forumTick: 0, modal: null });
});

describe('포럼 글 목록', () => {
  it('새 글 알림(forumTick)이 오면 다른 채널을 다녀오지 않아도 목록을 다시 읽는다', async () => {
    let list: ForumPostView[] = [];
    const { call, space, channel } = setup(() => list);
    const { el } = await render(<ForumView space={space} channel={channel} />);
    await vi.waitFor(() => expect(el.textContent).toContain('아직 글이 없습니다'));
    list = [post()];
    await act(async () => useStore.setState({ forumTick: useStore.getState().forumTick + 1 }));
    await vi.waitFor(() => expect(el.querySelector('.post-title-text')?.textContent).toBe('첫 글 제목'));
    expect(calls(call, 'forum.list_posts')).toHaveLength(2);
  });

  it("익명 글의 글쓴이는 '익명'으로 보인다", async () => {
    const { space, channel } = setup(() => [post({ author: ANONYMOUS_USER, anonymous: true })]);
    const { el } = await render(<ForumView space={space} channel={channel} />);
    await vi.waitFor(() => expect(el.querySelector('.post-meta')?.textContent).toContain('익명'));
    expect(el.textContent).not.toContain('상대');
  });

  it('미리보기는 마크다운을 뺀 평문(스포일러는 가림)이고, 대문 글에는 표시가 붙는다', async () => {
    const long = 'ㄱ'.repeat(200);
    const { space, channel } = setup(() => [
      post({ preview: '**굵게** ||비밀|| [링크](https://a.example) `코드`', featured: true, attachment_count: 2 }),
      post({ id: 'p2', title: '긴 글', preview: long, featured: undefined }),
    ]);
    const { el } = await render(<ForumView space={space} channel={channel} />);
    await vi.waitFor(() => expect(el.querySelectorAll('button.post')).toHaveLength(2));
    const [a, b] = [...el.querySelectorAll('button.post')];
    expect(a.querySelector('.post-preview')?.textContent).toBe('굵게 (스포일러) 링크 코드');
    expect(a.textContent).not.toContain('비밀');
    expect(a.querySelector('.featured-badge')?.textContent).toContain('대문');
    expect(a.querySelector('.post-atts')?.textContent?.trim()).toBe('2');
    // 데몬이 앞 200자만 주면 잘린 표시를 붙입니다.
    expect(b.querySelector('.post-preview')?.textContent).toBe(`${long}…`);
    expect(b.querySelector('.featured-badge')).toBeNull();
    expect(b.querySelector('.post-atts')).toBeNull();
  });

  it('정렬·방향·보기·태그 맞춤을 보내고 채널마다 기억한다(고른 태그는 기억하지 않음)', async () => {
    const { call, transport, space, channel } = setup(() => [post({ cover: { blob: 'b', name: 'a.jpg', mime: 'image/jpeg', size: 4, thumbnail_base64: THUMB, downloaded: false }, attachment_count: 1 })]);
    const r = await render(<ForumView space={space} channel={channel} />);
    await vi.waitFor(() => expect(r.el.querySelector('button.post')).not.toBeNull());
    expect(calls(call, 'forum.list_posts')[0]).toEqual({ space: 'S', channel: 'F', sort: 'activity', order: 'desc' });

    const [sort, order] = [...r.el.querySelectorAll<HTMLSelectElement>('.forum-controls select')];
    await choose(sort, 'created');
    await choose(order, 'asc');
    await click(btn(r.el, '갤러리'));
    await click(btn(r.el.querySelector('.forum-tags')!, '질문'));
    await click(btn(r.el.querySelector('.forum-tags')!, '버그'));
    await click(btn(r.el, '모두 일치'));
    await vi.waitFor(() =>
      expect(calls(call, 'forum.list_posts').at(-1)).toEqual({ space: 'S', channel: 'F', sort: 'created', order: 'asc', tags: [1, 2], tag_match: 'all' }),
    );
    // 갤러리: 첫 이미지의 썸네일이 표지.
    const card = r.el.querySelector('.post-grid .post-card')!;
    expect(card.querySelector<HTMLImageElement>('.pc-cover img')?.getAttribute('src')).toBe(`data:image/jpeg;base64,${THUMB}`);
    expect(r.el.querySelector('button.post')).toBeNull();
    expect(useStore.getState().prefs.forumView[chKey('S', 'F')]).toEqual({ sort: 'created', order: 'asc', view: 'gallery', match: 'all' });
    expect(transport.savePrefs).toHaveBeenCalled();

    // 다시 들어오면 보기 설정은 그대로, 태그 고르기는 처음부터.
    await r.unmount();
    call.mockClear();
    const again = await render(<ForumView space={space} channel={channel} />);
    await vi.waitFor(() => expect(again.el.querySelector('.post-card')).not.toBeNull());
    expect(calls(call, 'forum.list_posts')[0]).toEqual({ space: 'S', channel: 'F', sort: 'created', order: 'asc' });
    expect(again.el.querySelector<HTMLSelectElement>('.forum-controls select')?.value).toBe('created');
    expect(btn(again.el.querySelector('.forum-tags')!, '전체').classList.contains('active')).toBe(true);
  });

  it('휘발성 채널의 보기 설정은 설정 파일에 남기지 않는다', async () => {
    const { call, transport, space, channel } = setup(() => [post()], { channel: makeChannel({}, { security: 'volatile' }) });
    const { el } = await render(<ForumView space={space} channel={channel} />);
    await vi.waitFor(() => expect(el.querySelector('button.post')).not.toBeNull());
    await click(btn(el, '갤러리'));
    await vi.waitFor(() => expect(el.querySelector('.post-card')).not.toBeNull());
    expect(useStore.getState().prefs.forumView).toEqual({});
    expect(transport.savePrefs).not.toHaveBeenCalled();
    expect(calls(call, 'forum.list_posts')).toHaveLength(1);
  });

  it('태그가 하나뿐이면 일부/모두 일치를 보이지 않는다', async () => {
    const { space, channel } = setup(() => [], { channel: makeChannel({}, { forum_tags: [{ id: 1, name: '질문', emoji: '❓' }] }) });
    const { el } = await render(<ForumView space={space} channel={channel} />);
    await vi.waitFor(() => expect(el.textContent).toContain('아직 글이 없습니다'));
    expect(el.querySelector('.tag-match')).toBeNull();
  });
});

describe('포럼 새 글 작성 페이지', () => {
  it('팝업이 아니라 채널 화면 안에서 쓰고, 태그·첨부·대문 지정을 실어 올린 뒤 그 글을 연다', async () => {
    const { call, space, channel } = setup(() => [], {
      perms: ['create_posts', 'attach_files', 'manage_channels'],
      picked: [{ path: '/f/a.png', name: 'a.png' }],
    });
    const { el } = await render(<ForumView space={space} channel={channel} />);
    await vi.waitFor(() => expect(el.textContent).toContain('아직 글이 없습니다'));
    await click(btn(el, '새 글'));
    const page = el.querySelector('.forum-compose')!;
    expect(page).not.toBeNull();
    expect(useStore.getState().modal).toBeNull();
    expect(el.querySelector('.forum-list')).toBeNull();
    expect(page.textContent).toContain('마크다운');

    const submit = btn(page, '올리기');
    expect(submit.disabled).toBe(true);
    await typeInto(page.querySelector<HTMLInputElement>('input.input')!, '  새 질문 ');
    await click(btn(page, '질문'));
    await typeInto(page.querySelector('textarea')!, '**굵게** 본문');
    await click(btn(page, '파일 첨부'));
    await vi.waitFor(() => expect(page.querySelector('.file-chip')?.textContent).toContain('a.png'));
    await click(page.querySelector<HTMLInputElement>('.fc-featured input')!);
    expect(submit.disabled).toBe(false);
    await click(submit);

    await vi.waitFor(() => expect(calls(call, 'forum.create_post')).toHaveLength(1));
    expect(calls(call, 'forum.create_post')[0]).toEqual({
      space: 'S',
      channel: 'F',
      title: '새 질문',
      text: '**굵게** 본문',
      tags: [1],
      attachments: [{ path: '/f/a.png', name: 'a.png' }],
      featured: true,
    });
    await vi.waitFor(() => expect(el.querySelector('.forum-thread .ft-title')?.textContent).toBe('새 질문'));
    expect(call).toHaveBeenCalledWith('message.list', expect.objectContaining({ thread: 'p9' }));
  });

  it('본문 없이 첨부만 있어도 올릴 수 있다(대문 지정은 보내지 않음)', async () => {
    const { call, space, channel } = setup(() => [], { perms: ['create_posts', 'attach_files'], picked: [{ path: '/f/v.mp4', name: 'v.mp4' }] });
    const { el } = await render(<ForumView space={space} channel={channel} />);
    await vi.waitFor(() => expect(el.textContent).toContain('아직 글이 없습니다'));
    await click(btn(el, '새 글'));
    const page = el.querySelector('.forum-compose')!;
    await typeInto(page.querySelector<HTMLInputElement>('input.input')!, '영상');
    expect(btn(page, '올리기').disabled).toBe(true);
    await click(btn(page, '파일 첨부'));
    await vi.waitFor(() => expect(btn(page, '올리기').disabled).toBe(false));
    await click(btn(page, '올리기'));
    await vi.waitFor(() => expect(calls(call, 'forum.create_post')).toHaveLength(1));
    const sent = calls(call, 'forum.create_post')[0];
    expect(sent.text).toBe('');
    expect((sent.attachments as { name: string }[]).map((a) => a.name)).toEqual(['v.mp4']);
    expect(sent).not.toHaveProperty('featured');
  });

  it('권한이 없으면 대문 지정은 보이지 않고 첨부는 막힌다', async () => {
    const { space, channel } = setup(() => [], { perms: ['create_posts'] });
    const { el } = await render(<ForumView space={space} channel={channel} />);
    await vi.waitFor(() => expect(el.textContent).toContain('아직 글이 없습니다'));
    await click(btn(el, '새 글'));
    const page = el.querySelector('.forum-compose')!;
    expect(page.querySelector('.fc-featured')).toBeNull();
    expect(btn(page, '파일 첨부').disabled).toBe(true);
  });

  it('태그는 5개까지 고른다', async () => {
    const forum_tags = Array.from({ length: 7 }, (_, i) => ({ id: i + 1, name: `태그${i + 1}` }));
    const { space, channel } = setup(() => [], { channel: makeChannel({}, { forum_tags }) });
    const { el } = await render(<ForumView space={space} channel={channel} />);
    await vi.waitFor(() => expect(el.textContent).toContain('아직 글이 없습니다'));
    await click(btn(el, '새 글'));
    const page = el.querySelector('.forum-compose')!;
    for (let i = 1; i <= 6; i++) await click(btn(page, `태그${i}`));
    expect(page.querySelectorAll('.chip.active')).toHaveLength(5);
    expect(btn(page, '태그6').disabled).toBe(true);
    expect(page.textContent).toContain('태그 (5/5)');
  });

  it('쓰던 내용이 있으면 취소할 때 확인을 받는다', async () => {
    const { space, channel } = setup(() => []);
    const { el } = await render(<ForumView space={space} channel={channel} />);
    await vi.waitFor(() => expect(el.textContent).toContain('아직 글이 없습니다'));
    await click(btn(el, '새 글'));
    const page = el.querySelector('.forum-compose')!;
    await typeInto(page.querySelector('textarea')!, '쓰다 만 글');
    await click(btn(page, '취소'));
    const modal = useStore.getState().modal;
    expect(modal?.kind).toBe('confirm');
    expect(el.querySelector('.forum-compose')).not.toBeNull();
    await act(async () => (modal as { onConfirm: () => void }).onConfirm());
    expect(el.querySelector('.forum-compose')).toBeNull();
    expect(el.querySelector('.forum-list')).not.toBeNull();
  });
});

describe('포럼 대문', () => {
  it('대문 글은 목록 위에 전체 내용을 그리고, 채널 관리 권한이 있으면 내릴 수 있다', async () => {
    const featured = post({ featured: true, title: '안내' });
    const { call, space, channel } = setup(() => [featured, post({ id: 'p2', title: '둘째 글' })], {
      perms: ['create_posts', 'manage_channels'],
      channel: makeChannel({ featured_post: 'p1' }),
    });
    const { el } = await render(<ForumView space={space} channel={channel} />);
    await vi.waitFor(() => expect(el.querySelector('.forum-featured .m.is-post')?.textContent).toBe('게시글 본문'));
    const box = el.querySelector('.forum-featured')!;
    expect(box.querySelector('.ff-title')?.textContent).toBe('안내');
    expect(box.querySelector('.post-tags')?.textContent).toContain('질문');
    expect(box.querySelector('.ff-foot')?.textContent).toContain('댓글 1개');
    // 대문이 목록보다 먼저.
    expect(el.querySelector('.forum-list')?.firstElementChild).toBe(box);
    expect(call).toHaveBeenCalledWith('forum.get_post', { space: 'S', channel: 'F', post: 'p1' });
    expect(call).toHaveBeenCalledWith('message.list', expect.objectContaining({ thread: 'p1' }));

    await click(btn(box, '대문 해제'));
    await vi.waitFor(() => expect(calls(call, 'forum.set_featured')).toEqual([{ space: 'S', channel: 'F', post: null }]));
    // 버튼을 눌러도 글은 열리지 않습니다.
    expect(el.querySelector('.forum-thread')).toBeNull();
  });

  it('대문 본문을 누르면 그 글을 열고, 권한이 없으면 내리기 버튼이 없다', async () => {
    const { space, channel } = setup(() => [post()], { channel: makeChannel({ featured_post: 'p1' }) });
    const { el } = await render(<ForumView space={space} channel={channel} />);
    await vi.waitFor(() => expect(el.querySelector('.forum-featured .m.is-post')).not.toBeNull());
    expect(el.querySelector('.forum-featured')?.textContent).not.toContain('대문 해제');
    await click(el.querySelector<HTMLElement>('.forum-featured .ff-body .m')!);
    await vi.waitFor(() => expect(el.querySelector('.forum-thread')).not.toBeNull());
    expect(el.querySelector('.ft-title')?.textContent).toBe('첫 글 제목');
    expect(el.querySelector('.ft-actions')).toBeNull();
  });

  it('대문 글을 읽지 못하면(지워짐) 대문 자리를 그리지 않는다', async () => {
    const { space, channel } = setup(() => [post({ id: 'p2' })], { channel: makeChannel({ featured_post: 'p1' }) });
    const { el } = await render(<ForumView space={space} channel={channel} />);
    await vi.waitFor(() => expect(el.querySelector('button.post')).not.toBeNull());
    await vi.waitFor(() => expect(el.querySelector('.forum-featured')).toBeNull());
  });

  it('글 화면에서 대문으로 지정한다', async () => {
    const { call, space, channel } = setup(() => [post()], { perms: ['create_posts', 'manage_channels'] });
    const { el } = await render(<ForumView space={space} channel={channel} />);
    await vi.waitFor(() => expect(el.querySelector('button.post')).not.toBeNull());
    await click(el.querySelector<HTMLButtonElement>('button.post')!);
    await vi.waitFor(() => expect(el.querySelector('.forum-thread')).not.toBeNull());
    await click(btn(el.querySelector('.ft-actions')!, '대문으로 지정'));
    await vi.waitFor(() => expect(calls(call, 'forum.set_featured')).toEqual([{ space: 'S', channel: 'F', post: 'p1' }]));
    // 관리 권한(manage_messages)이 없으니 고정·잠금은 없습니다.
    expect(el.querySelector('.ft-actions')?.textContent).not.toContain('고정');
  });
});

describe('포럼 글타래', () => {
  it('글을 열면 맨 위에 제목·태그·본문을 한 번만 그리고 그 아래에 댓글을 그린다', async () => {
    const { call, space, channel } = setup(() => [post()]);
    const { el } = await render(<ForumView space={space} channel={channel} />);
    await vi.waitFor(() => expect(el.querySelector('button.post')).not.toBeNull());
    await click(el.querySelector<HTMLButtonElement>('button.post')!);
    await vi.waitFor(() => expect(el.querySelector('.forum-post')).not.toBeNull());
    expect(call).toHaveBeenCalledWith('message.list', expect.objectContaining({ space: 'S', channel: 'F', thread: 'p1' }));

    const head = el.querySelector('.forum-post')!;
    expect(head.querySelector('.forum-post-title')?.textContent).toBe('첫 글 제목');
    expect(head.querySelector('.post-tags')?.textContent).toContain('질문');
    expect(head.querySelector('.m.is-post')?.textContent).toBe('게시글 본문');
    // 본문은 한 번만, 댓글은 게시글 밖에(글타래의 시작 표시 없이).
    expect([...el.querySelectorAll('.m')].map((x) => x.textContent)).toEqual(['게시글 본문', '첫 댓글']);
    expect(head.textContent).not.toContain('첫 댓글');
    expect(el.textContent).not.toContain('글타래의 시작');
  });

  it('글을 고치면(글타래 없는 message.updated) 열어 둔 글타래의 본문도 바뀐다', async () => {
    const { space, channel } = setup(() => [post()]);
    const { el } = await render(<ForumView space={space} channel={channel} />);
    await vi.waitFor(() => expect(el.querySelector('button.post')).not.toBeNull());
    await click(el.querySelector<HTMLButtonElement>('button.post')!);
    await vi.waitFor(() => expect(el.querySelector('.m.is-post')?.textContent).toBe('게시글 본문'));
    await act(async () => {
      const { patch } = applyBatch(useStore.getState(), [
        { type: 'message.updated', space: 'S', channel: 'F', message: msg('p1', 1, '고친 본문', { edited_at: 5000 }) },
      ]);
      useStore.setState(patch);
    });
    expect(el.querySelector('.m.is-post')?.textContent).toBe('고친 본문');
    expect([...el.querySelectorAll('.m')]).toHaveLength(2);
  });
});

describe('작성 페이지의 태그', () => {
  it('채널에 있는 태그 중에서만 고르고, 작성 페이지에서는 새 태그를 만들지 않는다', async () => {
    const { call, space, channel } = setup(() => [], { perms: ['create_posts', 'manage_channels'] });
    const { el } = await render(<ForumView space={space} channel={channel} />);
    await vi.waitFor(() => expect(el.textContent).toContain('아직 글이 없습니다'));
    await click(btn(el, '새 글'));
    const page = el.querySelector<HTMLElement>('.forum-compose')!;
    expect(page.querySelector('.fc-new-tag')).toBeNull();
    expect(page.querySelector('input[aria-label="새 태그 이름"]')).toBeNull();
    // 관리자에게는 태그를 어디서 만드는지 알려 줍니다.
    expect(page.textContent).toContain(TAG_HINT);
    await click(btn(page, '❓ 질문'));
    expect(btn(page, '❓ 질문').getAttribute('aria-pressed')).toBe('true');
    expect(page.textContent).toContain('태그 (1/5)');
    expect(calls(call, 'channel.update')).toHaveLength(0);
  });

  it('권한이 없으면 안내 없이 고르기만, 태그도 없으면 태그 칸 자체가 없다', async () => {
    const a = setup(() => [], { perms: ['create_posts'] });
    let r = await render(<ForumView space={a.space} channel={a.channel} />);
    await vi.waitFor(() => expect(r.el.textContent).toContain('아직 글이 없습니다'));
    await click(btn(r.el, '새 글'));
    expect(r.el.querySelector('.forum-compose')?.textContent).toContain('태그 (0/5)');
    expect(r.el.querySelector('.forum-compose')?.textContent).not.toContain(TAG_HINT);
    await cleanup();
    const b = setup(() => [], { perms: ['create_posts'], channel: makeChannel({}, { forum_tags: [] }) });
    r = await render(<ForumView space={b.space} channel={b.channel} />);
    await vi.waitFor(() => expect(r.el.textContent).toContain('아직 글이 없습니다'));
    await click(btn(r.el, '새 글'));
    expect(r.el.querySelector('.forum-compose')?.textContent).not.toContain('태그');
    await cleanup();
    // 태그가 없고 관리 권한이 있으면 채널 설정에서 만들라는 안내만 보입니다.
    const c = setup(() => [], { perms: ['create_posts', 'manage_channels'], channel: makeChannel({}, { forum_tags: [] }) });
    r = await render(<ForumView space={c.space} channel={c.channel} />);
    await vi.waitFor(() => expect(r.el.textContent).toContain('아직 글이 없습니다'));
    await click(btn(r.el, '새 글'));
    expect(r.el.querySelector('.forum-compose')?.textContent).toContain(TAG_HINT);
  });
});


describe('포럼 글 수정 페이지', () => {
  const openMine = async (o: Setup = {}) => {
    const s = setup(() => [post({ mine: true, author: ME })], o);
    const { el } = await render(<ForumView space={s.space} channel={s.channel} />);
    await vi.waitFor(() => expect(el.querySelector('button.post')).not.toBeNull());
    await click(el.querySelector<HTMLButtonElement>('button.post')!);
    await vi.waitFor(() => expect(el.querySelector('.m.is-post')?.textContent).toBe('게시글 본문'));
    return { ...s, el };
  };

  it('내 글은 작성 페이지와 같은 틀에서 제목·태그·본문을 고치고, 본문은 늘 보내 수정 일시를 남긴다', async () => {
    const { call, el } = await openMine();
    await click(btn(el.querySelector('.ft-actions')!, '수정'));
    await vi.waitFor(() => expect(el.querySelector('.forum-edit')).not.toBeNull());
    const form = el.querySelector('.forum-edit')!;
    expect(form.querySelector('.ft-title')?.textContent).toBe('글 수정');
    const title = form.querySelector<HTMLInputElement>('input.input')!;
    expect(title.value).toBe('첫 글 제목');
    expect(form.querySelector<HTMLTextAreaElement>('textarea')!.value).toBe('게시글 본문');
    await typeInto(title, '고친 제목');
    await click(btn(form, '버그'));
    await click(btn(el, '저장'));
    await vi.waitFor(() => expect(calls(call, 'forum.update_post')).toEqual([{ space: 'S', channel: 'F', post: 'p1', title: '고친 제목', tags: [1, 2] }]));
    expect(calls(call, 'message.edit')).toEqual([{ space: 'S', channel: 'F', id: 'p1', text: '게시글 본문' }]);
    // 저장하면 글로 돌아가고, 제목 아래에 '(수정됨) 일시'가 붙습니다.
    await vi.waitFor(() => expect(el.querySelector('.forum-thread')).not.toBeNull());
    expect(el.querySelector('.forum-post-title')?.textContent).toBe('고친 제목');
    expect(el.querySelector('.forum-post-head .post-edited')?.textContent).toMatch(/^\(수정됨\) /);
  });

  it('본문만 고치면 forum.update_post는 보내지 않는다', async () => {
    const { call, el } = await openMine();
    await click(btn(el.querySelector('.ft-actions')!, '수정'));
    await vi.waitFor(() => expect(el.querySelector('.forum-edit textarea')).not.toBeNull());
    await typeInto(el.querySelector<HTMLTextAreaElement>('.forum-edit textarea')!, '새 본문');
    await click(btn(el, '저장'));
    await vi.waitFor(() => expect(calls(call, 'message.edit')).toEqual([{ space: 'S', channel: 'F', id: 'p1', text: '새 본문' }]));
    expect(calls(call, 'forum.update_post')).toEqual([]);
  });

  it('남의 글이거나 수정 금지 채널이면 수정 버튼이 없다', async () => {
    const s = setup(() => [post()]);
    const { el } = await render(<ForumView space={s.space} channel={s.channel} />);
    await vi.waitFor(() => expect(el.querySelector('button.post')).not.toBeNull());
    await click(el.querySelector<HTMLButtonElement>('button.post')!);
    await vi.waitFor(() => expect(el.querySelector('.forum-thread')).not.toBeNull());
    expect(el.querySelector('.ft-actions')).toBeNull();
    await cleanup();
    const { el: el2 } = await openMine({ channel: makeChannel({ no_edit: true }) });
    expect(el2.querySelector('.ft-actions')).toBeNull();
  });

  it('고친 글은 목록에도 (수정됨) 일시를 보인다', async () => {
    const s = setup(() => [post({ edited_at: 3000 }), post({ id: 'p2', title: '둘째' })]);
    const { el } = await render(<ForumView space={s.space} channel={s.channel} />);
    await vi.waitFor(() => expect(el.querySelectorAll('button.post')).toHaveLength(2));
    const [first, second] = [...el.querySelectorAll('button.post')];
    expect(first.querySelector('.post-edited')?.textContent).toMatch(/^\(수정됨\) /);
    expect(second.querySelector('.post-edited')).toBeNull();
  });
});

describe('태그 맞춤', () => {
  it('일부 일치는 하나라도, 모두 일치는 글의 태그가 고른 태그와 꼭 같아야 하고, 고른 태그가 없으면 모두 맞는다', () => {
    expect(matchesTags([1, 2], [1], 'any')).toBe(true);
    expect(matchesTags([1, 2], [1], 'all')).toBe(false);
    expect(matchesTags([2, 1], [1, 2], 'all')).toBe(true);
    expect(matchesTags([1], [1, 2], 'all')).toBe(false);
    expect(matchesTags([3], [1], 'any')).toBe(false);
    expect(matchesTags([3], [], 'all')).toBe(true);
  });

  it('모두 일치에 맞지 않는 대문 글은 목록 위에 그리지 않는다', async () => {
    const { space, channel } = setup(() => [post({ featured: true, tags: [1, 2] }), post({ id: 'p2', title: '질문만', tags: [1] })], {
      channel: makeChannel({ featured_post: 'p1' }),
    });
    const { el } = await render(<ForumView space={space} channel={channel} />);
    await vi.waitFor(() => expect(el.querySelector('.forum-featured')).not.toBeNull());
    await click(btn(el.querySelector('.forum-tags')!, '질문'));
    expect(el.querySelector('.forum-featured')).not.toBeNull();
    await click(btn(el, '모두 일치'));
    await vi.waitFor(() => expect(el.querySelector('.forum-featured')).toBeNull());
  });
});
