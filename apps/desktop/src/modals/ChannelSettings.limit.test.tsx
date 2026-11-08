// @vitest-environment happy-dom
// 채널 설정의 첨부 용량 한도·수정·삭제 금지: 그 채널의 채널 관리 권한이 있으면 크루의 카테고리가 아닌 채널에서 정합니다.
// 한도는 입력란(비우면 무제한, 0은 첨부 금지, 그 밖에는 MiB)이고, 저장할 때 바뀐 경우에만 `attach_limit`(바이트, 무제한이면
// null)으로 보냅니다. 휘발성 채널은 만들 때 10MiB에서 시작하고, 그보다 크면 RAM 경고를 붙입니다. 저장해도 창은 닫지 않습니다.

import { act } from 'react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import type { ApiClient } from '../api/client';
import type { ChannelKind, ChannelView, SpaceKind, SpaceView } from '../api/types';
import { useStore } from '../store/store';
import { cleanup, click, render } from '../test/dom';
import { ATTACH_LIMIT_HINT, ChannelSettingsModal, VOLATILE_LIMIT_HINT, VOLATILE_RAM_WARNING } from './ChannelSettings';

const SPACE = 'S'.repeat(32);
const CH = 'C'.repeat(32);
const ME = 'M'.repeat(32);
const MIB = 1024 * 1024;

function channel(attach_limit?: number, kind: ChannelKind = 'text', security = 'normal', extra: Partial<ChannelView> = {}): ChannelView {
  return {
    channel: CH,
    settings: {
      name: '자료',
      kind,
      topic: '',
      parent: null,
      position: 0,
      security,
      join_mode: 'always',
      ttl: { mode: 'none' },
      history: 'private',
      link_previews: true,
      overwrites: [],
      forum_tags: [],
    },
    unread: 0,
    api_read_allowed: false,
    attach_limit,
    ...extra,
  } as unknown as ChannelView;
}

function setup(ch: ChannelView | null, perms = ['administrator'], kind: SpaceKind = 'crew') {
  const view = {
    space: SPACE,
    kind,
    settings: { name: '크루' },
    roles: [],
    channels: ch ? [ch] : [],
    members: [{ user: ME, profile: { name: '나' }, roles: [] }],
    my_permissions: perms,
  } as unknown as SpaceView;
  const call = vi.fn(async (method: string, _params?: unknown) =>
    method === 'space.get' ? view : method === 'channel.create' ? { channel: 'N'.repeat(32) } : {},
  );
  useStore.setState({ client: { call } as unknown as ApiClient, views: { [SPACE]: view }, me: ME, modal: null, toasts: [] });
  return call;
}

const updates = (call: ReturnType<typeof setup>) => call.mock.calls.filter((c) => c[0] === 'channel.update').map((c) => c[1] as Record<string, unknown>);
const creates = (call: ReturnType<typeof setup>) => call.mock.calls.filter((c) => c[0] === 'channel.create').map((c) => c[1] as Record<string, unknown>);
const section = (el: HTMLElement) => el.querySelector<HTMLElement>('section[aria-label="첨부 용량 한도"]');
const mib = (el: HTMLElement) => el.querySelector<HTMLInputElement>('input[aria-label="첨부 하나의 최대 크기(MiB)"]');
const save = (el: HTMLElement) => el.querySelector<HTMLButtonElement>('.modal-foot .btn.primary')!;
const toggle = (el: HTMLElement, label: string) => el.querySelector<HTMLButtonElement>(`button[role="switch"][aria-label="${label}"]`);

async function typeInto(i: HTMLInputElement, value: string) {
  await act(async () => {
    Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')!.set!.call(i, value);
    i.dispatchEvent(new Event('input', { bubbles: true }));
  });
}

async function open(onClose = () => undefined) {
  return (await render(<ChannelSettingsModal space={SPACE} channel={CH} onClose={onClose} />)).el;
}

afterEach(async () => {
  await cleanup();
  useStore.setState({ client: null, views: {}, me: null, modal: null });
});

describe('채널 첨부 용량 한도', () => {
  it('기본은 무제한(빈 입력란)이고, 그대로 저장하면 아무것도 보내지 않고 창도 닫지 않는다', async () => {
    const call = setup(channel());
    const onClose = vi.fn();
    const el = await open(onClose);
    expect(mib(el)!.value).toBe('');
    expect(section(el)!.textContent).toContain(ATTACH_LIMIT_HINT);
    await click(save(el));
    expect(updates(call)).toEqual([]);
    expect(onClose).not.toHaveBeenCalled();
  });

  it('MiB를 넣어 저장하면 바이트로 보내고 창은 그대로 둔다', async () => {
    const call = setup(channel());
    const onClose = vi.fn();
    const el = await open(onClose);
    await typeInto(mib(el)!, '8');
    await click(save(el));
    await vi.waitFor(() => expect(updates(call)).toEqual([{ space: SPACE, channel: CH, attach_limit: 8 * MIB }]));
    expect(onClose).not.toHaveBeenCalled();
    expect(mib(el)).not.toBeNull();
  });

  it('0은 첨부 금지로 보낸다', async () => {
    const call = setup(channel());
    const el = await open();
    await typeInto(mib(el)!, '0');
    expect(section(el)!.textContent).toContain('첨부 금지');
    await click(save(el));
    await vi.waitFor(() => expect(updates(call)).toEqual([{ space: SPACE, channel: CH, attach_limit: 0 }]));
  });

  it('정해 둔 한도를 보여 주고, 비우면 null(무제한)을 보낸다', async () => {
    const call = setup(channel(8 * MIB));
    const el = await open();
    expect(mib(el)!.value).toBe('8');
    await typeInto(mib(el)!, '');
    await click(save(el));
    await vi.waitFor(() => expect(updates(call)).toEqual([{ space: SPACE, channel: CH, attach_limit: null }]));
  });

  it('0과 1MiB 사이는 저장할 수 없다', async () => {
    setup(channel(8 * MIB));
    const el = await open();
    await typeInto(mib(el)!, '0.5');
    expect(section(el)!.textContent).toContain('1MiB 이상');
    expect(save(el).disabled).toBe(true);
  });

  it('휘발성 채널은 10MiB보다 크거나 무제한이면 RAM 경고를 붙인다', async () => {
    setup(channel(10 * MIB, 'text', 'volatile'));
    const el = await open();
    expect(section(el)!.textContent).toContain(VOLATILE_LIMIT_HINT.trim());
    expect(section(el)!.querySelector('.ram-warning')).toBeNull();
    await typeInto(mib(el)!, '11');
    expect(section(el)!.querySelector('.ram-warning')?.textContent).toBe(VOLATILE_RAM_WARNING);
    await typeInto(mib(el)!, '');
    expect(section(el)!.querySelector('.ram-warning')).not.toBeNull();
  });

  it('휘발성 채널을 만들면 10MiB에서 시작해 함께 보낸다', async () => {
    const call = setup(null);
    const el = (await render(<ChannelSettingsModal space={SPACE} onClose={() => undefined} />)).el;
    expect(mib(el)!.value).toBe('');
    await click(el.querySelector<HTMLButtonElement>('.sec-card.volatile')!);
    expect(mib(el)!.value).toBe('10');
    await typeInto(el.querySelector<HTMLInputElement>('.modal-body input.input')!, '잠깐');
    await click(save(el));
    await vi.waitFor(() => expect(creates(call)).toHaveLength(1));
    expect(creates(call)[0]).toMatchObject({ security: 'volatile', attach_limit: 10 * MIB });
  });

  it('채널 관리 권한이 없거나, 카테고리거나, 크루가 아니면 보이지 않는다', async () => {
    setup(channel(), []);
    expect(section(await open())).toBeNull();
    await cleanup();
    setup(channel(undefined, 'category'));
    expect(section(await open())).toBeNull();
    await cleanup();
    setup(channel(), ['administrator'], 'space');
    expect(section(await open())).toBeNull();
  });
});

describe('채널 수정·삭제 금지', () => {
  it('켜서 저장하면 바뀐 것만 보낸다', async () => {
    const call = setup(channel(undefined, 'text', 'normal', { no_delete: true }));
    const el = await open();
    expect(toggle(el, '삭제 금지')!.getAttribute('aria-checked')).toBe('true');
    expect(toggle(el, '수정 금지')!.getAttribute('aria-checked')).toBe('false');
    await click(toggle(el, '수정 금지')!);
    await click(save(el));
    await vi.waitFor(() => expect(updates(call)).toEqual([{ space: SPACE, channel: CH, no_edit: true }]));
  });

  it('채널 관리 권한이 없으면 보이지 않는다', async () => {
    setup(channel(), []);
    const el = await open();
    expect(toggle(el, '수정 금지')).toBeNull();
    expect(toggle(el, '삭제 금지')).toBeNull();
  });
});

describe('설정 창 위의 확인 창', () => {
  it('채널 삭제 확인 창은 설정 창 위에 겹치고, 취소하면 설정 창(고치던 값 그대로)으로 돌아간다', async () => {
    const { ModalRoot } = await import('./Modal');
    setup(channel());
    useStore.getState().openModal({ kind: 'channelSettings', space: SPACE, channel: CH });
    const { el } = await render(<ModalRoot />);
    await typeInto(mib(el)!, '8');
    const del = [...el.querySelectorAll<HTMLButtonElement>('.modal-foot button')].find((b) => b.textContent?.includes('채널 삭제'))!;
    await click(del);
    expect(el.querySelectorAll('.modal')).toHaveLength(2);
    expect(mib(el)!.value).toBe('8');
    const cancel = [...el.querySelectorAll<HTMLButtonElement>('.modal-sm .modal-foot button')].find((b) => b.textContent?.trim() === '취소')!;
    await click(cancel);
    expect(el.querySelectorAll('.modal')).toHaveLength(1);
    expect(useStore.getState().modal?.kind).toBe('channelSettings');
    expect(mib(el)!.value).toBe('8');
  });
});
