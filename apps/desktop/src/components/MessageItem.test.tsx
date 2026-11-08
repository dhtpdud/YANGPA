// @vitest-environment happy-dom
// 메시지 한 줄: 차단한 사람의 메시지는 접어 두고 ‘보기’로만 펼칩니다.
// 첨부의 보관 단추는 이 기기가 올린 첨부에만 숨깁니다(내 다른 기기가 올린 것은 여기서 보관할 수 있음).
// 메뉴의 공유 중단·재개는 그 메시지의 지금 상태를 따르고, 첨부가 있으면 보관도 합니다.
// 익명 글은 '익명'과 기본 프사로만 그리고, 수정·삭제는 `mine`으로 정합니다.

import { afterEach, describe, expect, it, vi } from 'vitest';
import type { ApiClient } from '../api/client';
import { ANONYMOUS_USER, type MemberView, type MessageView } from '../api/types';
import { resetPinsForTest } from '../hooks/pins';
import { resetSharingForTest } from '../hooks/sharing';
import { useStore } from '../store/store';
import { button, cleanup, click, render } from '../test/dom';
import { MessageItem, type MessageItemProps } from './MessageItem';

const ME = 'M'.repeat(32);
const OTHER = 'O'.repeat(32);

const member: MemberView = {
  user: OTHER,
  profile: { version: 1, name: '상대', bio: '', avatar: null, banner: null, theme_color: null, status: '' },
  roles: [],
  online: true,
  automation: false,
};

function msg(extra: Partial<MessageView> = {}): MessageView {
  return {
    id: 'm1',
    author: OTHER,
    author_device: 'd',
    created_at: 1_700_000_000_000,
    lamport: 1,
    text: '평범한 본문',
    attachments: [],
    embeds: [],
    reactions: [],
    deleted: false,
    volatile: false,
    ...extra,
  };
}

function props(m: MessageView): MessageItemProps {
  return {
    m,
    grouped: false,
    space: 'S',
    channel: 'A',
    msgKey: 'S/A',
    members: new Map([[OTHER, member]]),
    roles: [],
    me: ME,
    volatile: false,
    canManage: false,
    canReact: true,
    isEditing: false,
  };
}

afterEach(async () => {
  await cleanup();
  resetPinsForTest();
  useStore.setState({ blocked: null, blockedDevices: null, views: {}, myDevice: null, client: null });
});

describe('MessageItem 차단 표시', () => {
  it('차단하지 않은 사람의 메시지는 그대로 보인다', async () => {
    useStore.setState({ blocked: [] });
    const { el } = await render(<MessageItem {...props(msg())} />);
    expect(el.textContent).toContain('평범한 본문');
    expect(el.textContent).not.toContain('차단한 사용자의 메시지');
  });

  it('차단 목록에 있으면 접어 두고, 보기를 누르면 펼친다', async () => {
    useStore.setState({ blocked: [{ user: OTHER, profile: null, blocked_at: 0 }] });
    const { el } = await render(<MessageItem {...props(msg())} />);
    expect(el.textContent).toContain('차단한 사용자의 메시지입니다');
    expect(el.textContent).not.toContain('평범한 본문');
    // 접힌 동안에는 반응·답장 같은 동작 버튼도 없습니다.
    expect(el.querySelector('.msg-actions')).toBeNull();
    await click(button(el, '보기'));
    expect(el.textContent).toContain('평범한 본문');
  });

  it('그 글을 쓴 기기만 차단해도 접고, 같은 사람의 다른 기기 글은 그대로 보인다', async () => {
    useStore.setState({ blocked: [], blockedDevices: [{ user: OTHER, device: 'd', name: '', profile: null, blocked_at: 0 }] });
    const { el } = await render(<MessageItem {...props(msg())} />);
    expect(el.textContent).toContain('차단한 사용자의 메시지입니다');
    const other = await render(<MessageItem {...props(msg({ id: 'm9', author_device: 'e', text: '다른 기기 글' }))} />);
    expect(other.el.textContent).toContain('다른 기기 글');
  });

  it('차단 목록을 모르는 데몬이면 메시지의 author_blocked를 따른다', async () => {
    useStore.setState({ blocked: null });
    const { el } = await render(<MessageItem {...props(msg({ author_blocked: true }))} />);
    expect(el.textContent).toContain('차단한 사용자의 메시지입니다');
  });

  it('목록에서 차단을 풀면 예전 표시(author_blocked)가 남아 있어도 펼친다', async () => {
    useStore.setState({ blocked: [] });
    const { el } = await render(<MessageItem {...props(msg({ author_blocked: true }))} />);
    expect(el.textContent).toContain('평범한 본문');
  });

  it('차단한 사람의 메시지에 단 남의 답장은 머리글에 원문 발췌를 보여 주지 않는다', async () => {
    const original = msg({ id: 'm0', text: '차단한 사람의 원문' });
    const reply = msg({ id: 'm2', author: ME, text: '답장 본문', reply_to: 'm0' });
    useStore.setState({ blocked: [{ user: OTHER, profile: null, blocked_at: 0 }] });
    const { el } = await render(<MessageItem {...props(reply)} replied={original} />);
    expect(el.textContent).toContain('답장 본문');
    expect(el.textContent).toContain('차단한 사용자의 메시지');
    expect(el.textContent).not.toContain('차단한 사람의 원문');
  });

  it('차단 목록을 모르는 데몬이면 답장 원문의 author_blocked를 따르고, 차단하지 않았으면 발췌를 보여 준다', async () => {
    const reply = msg({ id: 'm2', author: ME, text: '답장 본문', reply_to: 'm0' });
    useStore.setState({ blocked: null });
    const first = await render(<MessageItem {...props(reply)} replied={msg({ id: 'm0', text: '원문 발췌', author_blocked: true })} />);
    expect(first.el.textContent).not.toContain('원문 발췌');
    await cleanup();
    useStore.setState({ blocked: [] });
    const second = await render(<MessageItem {...props(reply)} replied={msg({ id: 'm0', text: '원문 발췌' })} />);
    expect(second.el.textContent).toContain('원문 발췌');
  });
});

describe('MessageItem 첨부 보관 단추', () => {
  const att = { blob: 'B'.repeat(64), name: '자료.zip', mime: 'application/zip', size: 1, downloaded: true };

  function setup() {
    const call = vi.fn(async (method: string) => (method === 'blob.pins' ? [] : {}));
    useStore.setState({ blocked: [], myDevice: 'this-device', client: { call } as unknown as ApiClient });
  }

  it('내 다른 기기가 올린 첨부는 이 기기에서 보관할 수 있고, 이 기기가 올린 첨부에는 단추가 없다', async () => {
    setup();
    const other = await render(<MessageItem {...props(msg({ author: ME, author_device: 'other-device', attachments: [att] }))} />);
    const here = await render(<MessageItem {...props(msg({ id: 'm2', author: ME, author_device: 'this-device', attachments: [att] }))} />);
    // 보관 목록을 다 읽어 다른 기기 쪽에 단추가 그려진 뒤에 비교합니다.
    await vi.waitFor(() => expect(other.el.querySelector('button.att-pin')).not.toBeNull());
    expect(here.el.querySelector('.att-file')).not.toBeNull();
    expect(here.el.querySelector('button.att-pin')).toBeNull();
  });
});

describe('MessageItem 메뉴의 파일 관리', () => {
  const att = { blob: 'B'.repeat(64), name: '자료.zip', mime: 'application/zip', size: 1, downloaded: false };

  function setup(excluded: string[]) {
    const call = vi.fn(async (method: string) => {
      if (method === 'blob.pins') return [];
      if (method === 'seed.get') return { excluded_events: excluded };
      return {};
    });
    useStore.setState({ blocked: [], myDevice: 'this-device', modal: null, client: { call } as unknown as ApiClient });
    return call;
  }

  async function openMenu(el: HTMLElement) {
    await click(el.querySelector<HTMLButtonElement>('button[title="더 보기"]')!);
    await vi.waitFor(() => expect(document.body.querySelector('.popover.menu')).not.toBeNull());
    return () => [...document.body.querySelectorAll<HTMLButtonElement>('.popover.menu .menu-item')];
  }

  afterEach(() => {
    resetSharingForTest();
    useStore.setState({ modal: null });
  });

  it('공유 중단 뒤엔 공유 재개가 보이고, 첨부 보관은 blob.pin', async () => {
    const call = setup([]);
    const { el } = await render(<MessageItem {...props(msg({ attachments: [att] }))} />);
    let items = await openMenu(el);
    await vi.waitFor(() => expect(call).toHaveBeenCalledWith('seed.get', { space: 'S' }));
    await click(items().find((b) => b.textContent?.includes('공유 중단'))!);
    await vi.waitFor(() => expect(call).toHaveBeenCalledWith('seed.exclude_message', { space: 'S', channel: 'A', id: 'm1', excluded: true }));
    items = await openMenu(el);
    expect(items().some((b) => b.textContent?.includes('공유 재개'))).toBe(true);
    await click(items().find((b) => b.textContent?.includes('첨부 보관하기'))!);
    await vi.waitFor(() => expect(call).toHaveBeenCalledWith('blob.pin', { space: 'S', blob: att.blob }));
  });

  it('이미 멈춘 메시지는 공유 재개만 보임', async () => {
    const call = setup(['m1']);
    const { el } = await render(<MessageItem {...props(msg({ attachments: [att] }))} />);
    const items = await openMenu(el);
    await vi.waitFor(() => expect(items().some((b) => b.textContent?.includes('공유 재개'))).toBe(true));
    expect(items().some((b) => b.textContent?.includes('공유 중단'))).toBe(false);
    await click(items().find((b) => b.textContent?.includes('공유 재개'))!);
    await vi.waitFor(() => expect(call).toHaveBeenCalledWith('seed.exclude_message', { space: 'S', channel: 'A', id: 'm1', excluded: false }));
  });
});

describe('MessageItem 익명 글', () => {
  // 익명 자리를 멤버 목록에서 찾으면 이 이름이 드러납니다(찾지 않아야 함).
  const decoy: MemberView = { ...member, user: ANONYMOUS_USER, profile: { ...member.profile, name: '드러난 이름' } };
  const anonProps = (m: MessageView): MessageItemProps => ({ ...props(m), members: new Map([[OTHER, member], [ANONYMOUS_USER, decoy]]) });
  const anonMsg = (extra: Partial<MessageView> = {}) => msg({ author: ANONYMOUS_USER, author_device: ANONYMOUS_USER, anonymous: true, text: '익명 본문', ...extra });

  function setup() {
    useStore.setState({ blocked: [], views: { S: { kind: 'crew', members: [] } as never } });
  }

  async function openMenu(el: HTMLElement) {
    await click(el.querySelector<HTMLButtonElement>('button[title="더 보기"]')!);
    return document.body.querySelector('.popover.menu')!;
  }

  it('익명 글에 가명이 있으면 가명으로 그린다(실제 이름은 찾지 않음)', async () => {
    setup();
    const { el } = await render(<MessageItem {...anonProps(anonMsg({ anon_name: '용감한 수달' }))} />);
    expect(el.querySelector('.msg-author')?.textContent).toBe('용감한 수달');
    expect(el.textContent).not.toContain('드러난 이름');
    expect(el.querySelector('.avatar.anon')).not.toBeNull();
  });

  it("남의 익명 글은 '익명'으로 그리고 프로필을 열 단추·차단 메뉴가 없으며, 수정은 못 하고 신고만 된다", async () => {
    setup();
    const { el } = await render(<MessageItem {...anonProps(anonMsg())} />);
    expect(el.querySelector('.msg-author')?.textContent).toBe('익명');
    expect(el.textContent).not.toContain('드러난 이름');
    expect(el.querySelector('button.msg-author')).toBeNull();
    expect(el.querySelector('button.avatar-btn')).toBeNull();
    expect(el.querySelector('.avatar.anon')).not.toBeNull();
    expect(el.querySelector('button[title="수정"]')).toBeNull();
    expect(el.querySelector('button[title="삭제"]')).toBeNull();
    const menu = await openMenu(el);
    expect(menu.textContent).toContain('신고하기');
    expect(menu.textContent).not.toContain('이 사람 차단');
  });

  it('가려진 내 글(mine)은 수정·삭제할 수 있고 신고 메뉴가 없다', async () => {
    setup();
    const { el } = await render(<MessageItem {...anonProps(anonMsg({ mine: true }))} />);
    expect(el.querySelector('.msg-author')?.textContent).toBe('익명');
    expect(el.querySelector('button[title="수정"]')).not.toBeNull();
    expect(el.querySelector('button[title="삭제"]')).not.toBeNull();
    const menu = await openMenu(el);
    expect(menu.textContent).not.toContain('신고하기');
  });

  it('익명 글에 단 답장 머리글과 익명 자리 멘션도 익명으로 보인다', async () => {
    setup();
    const reply = msg({ id: 'm2', text: `<@${ANONYMOUS_USER}> 동의합니다`, reply_to: 'm0' });
    const { el } = await render(<MessageItem {...anonProps(reply)} replied={anonMsg({ id: 'm0', text: '원문' })} />);
    expect(el.querySelector('.reply-name')?.textContent).toBe('익명');
    expect(el.querySelector('.reply-text')?.textContent).toBe('원문');
    expect(el.querySelector('.msg-text')?.textContent).toContain('@익명');
    expect(el.querySelector('.msg-text button.mention')).toBeNull();
    expect(el.textContent).not.toContain('드러난 이름');
  });

  it('익명이 켜지기 전 글을 뒤늦게 가린 경우(anonymous만 있음)도 실제 이름을 찾지 않는다', async () => {
    setup();
    const { el } = await render(<MessageItem {...anonProps(msg({ anonymous: true }))} />);
    expect(el.querySelector('.msg-author')?.textContent).toBe('익명');
    expect(el.textContent).not.toContain('상대');
    expect(el.querySelector('.avatar.anon')).not.toBeNull();
  });
});

describe('MessageItem 수정·삭제 금지 채널', () => {
  const mine = () => msg({ author: ME });
  const has = (el: Element, title: string) => !!el.querySelector(`.msg-actions button[title="${title}"]`);

  it('수정 금지면 내 글에도 수정 단추가 없고, 삭제 금지면 운영진만 삭제 단추가 있다', async () => {
    const open = await render(<MessageItem {...props(mine())} />);
    expect(has(open.el, '수정') && has(open.el, '삭제')).toBe(true);
    await cleanup();
    const banned = await render(<MessageItem {...props(mine())} noEdit noDelete />);
    expect(has(banned.el, '수정')).toBe(false);
    expect(has(banned.el, '삭제')).toBe(false);
    await cleanup();
    const moderator = await render(<MessageItem {...props(msg())} canManage noDelete />);
    expect(has(moderator.el, '삭제')).toBe(true);
  });

  it('게시글의 수정은 따로 연 수정 페이지로 간다', async () => {
    const onEdit = vi.fn();
    const { el } = await render(<MessageItem {...props(mine())} post onEdit={onEdit} />);
    await click(el.querySelector<HTMLButtonElement>('.msg-actions button[title="수정"]')!);
    expect(onEdit).toHaveBeenCalledOnce();
    expect(useStore.getState().editing).toBeNull();
  });
});
