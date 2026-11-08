// @vitest-environment happy-dom
// 입력창의 동영상·음성 첨부: 붙이면 썸네일·길이를 미리 읽어 칩에 보이고, 보낼 때 message.send에 같이 싣습니다.
// 아직 읽는 중이면 기다렸다 보내고, 읽지 못한 파일은 힌트 없이 보냅니다. 휘발성 채널에서는 읽지 않습니다. (실제 디코딩은 lib/media.test.ts)

import { act } from 'react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import type { ApiClient, PickedFile } from '../api/client';
import type { ChannelView, SpaceView } from '../api/types';
import type { MediaHints } from '../lib/media';
import { useStore } from '../store/store';
import { cleanup, click, render } from '../test/dom';

const capture = vi.hoisted(() => vi.fn<(t: unknown, path: string, name: string) => Promise<MediaHints | null>>());
vi.mock('../lib/media', async (importOriginal) => ({
  ...(await importOriginal<typeof import('../lib/media')>()),
  captureMediaHints: capture,
}));

const { Composer } = await import('./Composer');

const SPACE = 'S'.repeat(22);
const CHANNEL = 'C'.repeat(22);
const HINTS: MediaHints = { thumbnail_base64: btoa('\xff\xd8\xff\xe0'), width: 1280, height: 720, duration_ms: 65_000 };

function setup(picked: PickedFile[]) {
  const call = vi.fn(async (method: string, _params?: unknown) => (method === 'message.send' ? { id: 'E'.repeat(44) } : {}));
  const transport = { pickFiles: vi.fn(async () => picked), onFileDrop: () => () => undefined };
  const view = { space: SPACE, my_permissions: ['send_messages', 'attach_files'], members: [], emojis: [] } as unknown as SpaceView;
  useStore.setState({
    client: { call, transport } as unknown as ApiClient,
    me: 'U'.repeat(44),
    views: { [SPACE]: view },
    channels: {},
    replyTo: {},
    modal: null,
    toasts: [],
  });
  return call;
}

afterEach(async () => {
  await cleanup();
  capture.mockReset();
  useStore.setState({ client: null, views: {}, me: null });
});

async function pressEnter(el: HTMLElement) {
  const ta = el.querySelector('textarea')!;
  await act(async () => {
    ta.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', bubbles: true }));
  });
}

const sent = (call: ReturnType<typeof setup>) => call.mock.calls.filter((c) => c[0] === 'message.send').map((c) => c[1] as unknown as { attachments?: unknown[] });

describe('Composer 동영상·음성 힌트', () => {
  it('붙인 동영상의 썸네일·길이를 칩에 보이고 보낼 때 싣는다(다른 파일은 그대로)', async () => {
    const call = setup([
      { path: '/f/clip.mp4', name: 'clip.mp4' },
      { path: '/f/doc.pdf', name: 'doc.pdf' },
    ]);
    capture.mockImplementation(async (_t, path) => (path.endsWith('.mp4') ? HINTS : null));
    const { el } = await render(<Composer space={SPACE} channel={CHANNEL} />);
    await click(el.querySelector<HTMLButtonElement>('button[title="파일 첨부"]')!);
    await vi.waitFor(() => expect(el.querySelector('.chip-meta')?.textContent).toBe('1:05'));
    expect(el.querySelector<HTMLImageElement>('.chip-thumb')?.getAttribute('src')).toBe(`data:image/jpeg;base64,${HINTS.thumbnail_base64}`);
    // 이름이 동영상·음성인 파일만 미리보기를 엽니다.
    expect(capture).toHaveBeenCalledTimes(1);
    await pressEnter(el);
    await vi.waitFor(() => expect(sent(call)).toHaveLength(1));
    expect(sent(call)[0].attachments).toEqual([
      { path: '/f/clip.mp4', name: 'clip.mp4', ...HINTS },
      { path: '/f/doc.pdf', name: 'doc.pdf' },
    ]);
    // 보낸 뒤 칩을 비웁니다.
    await vi.waitFor(() => expect(el.querySelector('.file-chip')).toBeNull());
  });

  it('아직 읽는 중이면 기다렸다 보내고, 못 읽으면 힌트 없이 보낸다', async () => {
    const call = setup([{ path: '/f/voice.ogg', name: 'voice.ogg' }]);
    let finish: (h: MediaHints | null) => void = () => undefined;
    capture.mockImplementation(() => new Promise((r) => (finish = r)));
    const { el } = await render(<Composer space={SPACE} channel={CHANNEL} />);
    await click(el.querySelector<HTMLButtonElement>('button[title="파일 첨부"]')!);
    await vi.waitFor(() => expect(el.querySelector('.file-chip .spinner')).not.toBeNull());
    await pressEnter(el);
    expect(sent(call)).toHaveLength(0);
    await act(async () => finish(null));
    await vi.waitFor(() => expect(sent(call)).toHaveLength(1));
    expect(sent(call)[0].attachments).toEqual([{ path: '/f/voice.ogg', name: 'voice.ogg' }]);
  });

  it('힌트를 실어 보내지 못하면 다음에는 힌트 없이 보낸다(같은 값으로 계속 실패하지 않게)', async () => {
    const call = setup([{ path: '/f/song.aac', name: 'song.aac' }]);
    let first = true;
    call.mockImplementation(async (method: string) => {
      if (method !== 'message.send') return {};
      if (first) {
        first = false;
        throw new Error('첨부 정보가 올바르지 않습니다');
      }
      return { id: 'E'.repeat(44) };
    });
    capture.mockResolvedValue({ duration_ms: 3_000 });
    const { el } = await render(<Composer space={SPACE} channel={CHANNEL} />);
    await click(el.querySelector<HTMLButtonElement>('button[title="파일 첨부"]')!);
    await vi.waitFor(() => expect(el.querySelector('.chip-meta')?.textContent).toBe('0:03'));
    await pressEnter(el);
    await vi.waitFor(() => expect(sent(call)).toHaveLength(1));
    expect(sent(call)[0].attachments).toEqual([{ path: '/f/song.aac', name: 'song.aac', duration_ms: 3_000 }]);
    // 실패해도 입력은 남고, 다시 보내면 힌트 없이 갑니다.
    await vi.waitFor(() => expect(el.querySelector('.chip-meta')).toBeNull());
    expect(el.querySelector('.file-chip')).not.toBeNull();
    await pressEnter(el);
    await vi.waitFor(() => expect(sent(call)).toHaveLength(2));
    expect(sent(call)[1].attachments).toEqual([{ path: '/f/song.aac', name: 'song.aac' }]);
    expect(capture).toHaveBeenCalledTimes(1);
  });

  it('휘발성 채널에서는 미리보기를 열지 않는다', async () => {
    const call = setup([{ path: '/f/clip.mp4', name: 'clip.mp4' }]);
    const ch = { channel: CHANNEL, settings: { security: 'volatile', join_mode: 'always' }, joined: true } as unknown as ChannelView;
    const { el } = await render(<Composer space={SPACE} channel={CHANNEL} channelView={ch} />);
    await click(el.querySelector<HTMLButtonElement>('button[title="파일 첨부(메모리에만 보관)"]')!);
    await vi.waitFor(() => expect(el.querySelector('.file-chip')).not.toBeNull());
    expect(el.querySelector('.file-chip .spinner')).toBeNull();
    await pressEnter(el);
    await vi.waitFor(() => expect(sent(call)).toHaveLength(1));
    expect(capture).not.toHaveBeenCalled();
    expect(sent(call)[0].attachments).toEqual([{ path: '/f/clip.mp4', name: 'clip.mp4' }]);
  });
});

describe('Composer 클립보드 첨부', () => {
  function paste(el: HTMLElement, files: File[], text = '') {
    const ta = el.querySelector('textarea')!;
    const ev = new Event('paste', { bubbles: true, cancelable: true });
    Object.defineProperty(ev, 'clipboardData', { value: { files, getData: () => text } });
    return act(async () => {
      ta.dispatchEvent(ev);
    });
  }

  it('붙여넣은 이미지를 데몬에 올려 첨부로 보내고, 보낸 뒤 RAM의 붙여넣기를 지운다', async () => {
    const call = setup([]);
    call.mockImplementation(async (method: string) =>
      method === 'message.send' ? { id: 'E'.repeat(44) } : method === 'blob.paste' ? { paste: 'P1', received: 3 } : {},
    );
    const { el } = await render(<Composer space={SPACE} channel={CHANNEL} />);
    await paste(el, [new File([new Uint8Array([1, 2, 3])], 'image.png', { type: 'image/png' })]);
    await vi.waitFor(() => expect(el.querySelector('.file-chip span')?.textContent).toMatch(/^붙여넣기-\d{8}-\d{6}\.png$/));
    expect(call.mock.calls.find((c) => c[0] === 'blob.paste')?.[1]).toEqual({ size: 3, data_base64: 'AQID' });
    await pressEnter(el);
    await vi.waitFor(() => expect(sent(call)).toHaveLength(1));
    expect(sent(call)[0].attachments).toEqual([{ path: '', paste: 'P1', name: expect.stringMatching(/\.png$/) }]);
    await vi.waitFor(() => expect(call.mock.calls.some((c) => c[0] === 'blob.paste_discard')).toBe(true));
  });

  it('채널 첨부 한도를 넘는 붙여넣기는 올리지 않고 알린다', async () => {
    const call = setup([]);
    const ch = { channel: CHANNEL, settings: { security: 'normal' }, attach_limit: 2 } as unknown as ChannelView;
    const { el } = await render(<Composer space={SPACE} channel={CHANNEL} channelView={ch} />);
    await paste(el, [new File([new Uint8Array([1, 2, 3])], 'image.png', { type: 'image/png' })]);
    await vi.waitFor(() => expect(useStore.getState().toasts.some((t) => t.text.includes('2 B까지'))).toBe(true));
    expect(call.mock.calls.some((c) => c[0] === 'blob.paste')).toBe(false);
    expect(el.querySelector('.file-chip')).toBeNull();
  });

  it('글자가 함께 있으면 글자만 붙인다', async () => {
    const call = setup([]);
    const { el } = await render(<Composer space={SPACE} channel={CHANNEL} />);
    await paste(el, [new File(['x'], 'image.png', { type: 'image/png' })], '복사한 글');
    expect(call.mock.calls.some((c) => c[0] === 'blob.paste')).toBe(false);
    expect(el.querySelector('.file-chip')).toBeNull();
  });
});

describe('Composer 정렬 단추', () => {
  it('커서가 있는 줄을 가운데로 감싸 보내고, 왼쪽을 누르면 되돌린다', async () => {
    const call = setup([]);
    const { el } = await render(<Composer space={SPACE} channel={CHANNEL} />);
    const ta = el.querySelector('textarea')!;
    await act(async () => {
      Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, 'value')!.set!.call(ta, '안녕');
      ta.dispatchEvent(new Event('input', { bubbles: true }));
    });
    ta.setSelectionRange(1, 1);
    await click(el.querySelector<HTMLElement>('button[title="가운데 정렬"]')!);
    expect(ta.value).toBe(':::center\n안녕\n:::');
    await click(el.querySelector<HTMLElement>('button[title="오른쪽 정렬"]')!);
    expect(ta.value).toBe(':::right\n안녕\n:::');
    ta.setSelectionRange(10, 10);
    await click(el.querySelector<HTMLElement>('button[title="왼쪽 정렬"]')!);
    expect(ta.value).toBe('안녕');
    await click(el.querySelector<HTMLElement>('button[title="가운데 정렬"]')!);
    await pressEnter(el);
    await vi.waitFor(() => expect(call.mock.calls.some((c) => c[0] === 'message.send')).toBe(true));
    expect((call.mock.calls.find((c) => c[0] === 'message.send')![1] as { text: string }).text).toBe(':::center\n안녕\n:::');
  });
});

describe('Composer 모두 멘션', () => {
  async function typeIn(el: HTMLElement, value: string) {
    const ta = el.querySelector('textarea')!;
    await act(async () => {
      Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, 'value')!.set!.call(ta, value);
      ta.setSelectionRange(value.length, value.length);
      ta.dispatchEvent(new Event('input', { bubbles: true }));
    });
    return ta;
  }
  const withPerms = (perms: string[]) =>
    useStore.setState({ views: { [SPACE]: { space: SPACE, kind: 'crew', my_permissions: perms, members: [], emojis: [] } as unknown as SpaceView } });

  it('많이 멘션하기 권한이 있으면 @everyone·@here를 후보로 보이고 고르면 그대로 넣는다', async () => {
    setup([]);
    withPerms(['send_messages', 'mention_many']);
    const { el } = await render(<Composer space={SPACE} channel={CHANNEL} />);
    const ta = await typeIn(el, '공지 @e');
    expect(el.querySelector('.suggest')?.textContent).toContain('@everyone');
    expect(el.querySelector('.suggest')?.textContent).not.toContain('@here');
    await click([...el.querySelectorAll<HTMLElement>('.suggest-item')].find((b) => b.textContent?.includes('@everyone'))!);
    expect(ta.value).toBe('공지 @everyone ');
  });

  it('권한이 없으면 후보에 없다', async () => {
    setup([]);
    withPerms(['send_messages']);
    const { el } = await render(<Composer space={SPACE} channel={CHANNEL} />);
    await typeIn(el, '@');
    expect(el.querySelector('.suggest')).toBeNull();
  });
});
