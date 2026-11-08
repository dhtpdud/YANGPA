// @vitest-environment happy-dom
// 첨부: blob.status는 받기를 눌렀거나 받는 중일 때만 부릅니다(그려질 때마다 멤버 기기에 보유 여부를 묻지 않게).
// 동영상·음성은 원본 없이 썸네일·길이를 보이고, 받은 첨부는 보관(고정)할 수 있습니다(내가 올린 것·휘발성 제외).

import { act } from 'react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { RpcError, type ApiClient } from '../api/client';
import { resetPinsForTest } from '../hooks/pins';
import type { AttachmentView, EmbedView, NetStatus } from '../api/types';
import { useStore } from '../store/store';
import { cleanup, click, render } from '../test/dom';
import { Attachment, Embed, VOLATILE_SAVE_WARNING, onWebviewSave, resetRequestedForTest } from './Attachment';

const att: AttachmentView = { blob: 'B'.repeat(64), name: '자료.zip', mime: 'application/zip', size: 50 * 1024 * 1024, downloaded: false };

function setup(seeders = 0) {
  const call = vi.fn(async (method: string) => {
    if (method === 'blob.status') return { have_chunks: 0, total_chunks: 0, seeders_online: seeders, size: att.size, downloading: false };
    return {};
  });
  useStore.setState({ client: { call } as unknown as ApiClient, blobProgress: {}, toasts: [] });
  return call;
}

const statusCalls = (call: ReturnType<typeof setup>) => call.mock.calls.filter((c) => c[0] === 'blob.status').length;

afterEach(async () => {
  await cleanup();
  resetRequestedForTest();
  resetPinsForTest();
  useStore.setState({ client: null, blobProgress: {}, modal: null, views: {}, net: null });
});

describe('Attachment 상태 조회', () => {
  it('그려지기만 해서는 blob.status를 부르지 않는다', async () => {
    const call = setup();
    const { el } = await render(<Attachment att={att} space="S" messageId="m1" msgKey="S/A" volatile={false} />);
    expect(el.textContent).toContain('아직 받지 않음');
    // 다시 그려도(가상 스크롤) 마찬가지입니다.
    await cleanup();
    await render(<Attachment att={att} space="S" messageId="m1" msgKey="S/A" volatile={false} />);
    expect(call).not.toHaveBeenCalled();
  });

  it('받기를 누르면 받기 요청 뒤 상태를 읽고, 시더가 없으면 알린다(다시 그려도 유지)', async () => {
    const call = setup(0);
    const { el } = await render(<Attachment att={att} space="S" messageId="m1" msgKey="S/A" volatile={false} />);
    await click(el.querySelector<HTMLButtonElement>('button[title="받기"]')!);
    expect(call.mock.calls[0]).toEqual(['blob.fetch', { space: 'S', blob: att.blob, priority: 200 }]);
    await vi.waitFor(() => expect(el.textContent).toContain('가진 온라인 기기 없음'));
    expect(statusCalls(call)).toBe(1);
    await cleanup();
    const again = await render(<Attachment att={att} space="S" messageId="m1" msgKey="S/A" volatile={false} />);
    await vi.waitFor(() => expect(again.el.textContent).toContain('가진 온라인 기기 없음'));
  });

  it('자동 받기 진행 알림이 오면 그동안만 상태를 읽는다', async () => {
    const call = setup(2);
    const { el } = await render(<Attachment att={att} space="S" messageId="m1" msgKey="S/A" volatile={false} />);
    expect(statusCalls(call)).toBe(0);
    await act(async () => useStore.setState({ blobProgress: { [att.blob]: { have: 3, total: 10 } } }));
    await vi.waitFor(() => expect(statusCalls(call)).toBe(1));
    expect(el.textContent).toContain('받는 중 30%');
  });

  it('진행 알림이 두 번 오면 % 옆에 받기 속도를 보인다', async () => {
    setup(1);
    const now = vi.spyOn(Date, 'now').mockReturnValue(1000);
    try {
      const { el } = await render(<Attachment att={att} space="S" messageId="m1" msgKey="S/A" volatile={false} />);
      await act(async () => useStore.setState({ blobProgress: { [att.blob]: { have: 1, total: 10 } } }));
      expect(el.textContent).toContain('받는 중 10%');
      expect(el.textContent).not.toContain('/s');
      now.mockReturnValue(3000);
      await act(async () => useStore.setState({ blobProgress: { [att.blob]: { have: 3, total: 10 } } }));
      // 50MiB / 10조각 = 조각당 5MiB, 2초에 2조각
      expect(el.textContent).toContain('받는 중 30% · 5.0 MiB/s');
    } finally {
      now.mockRestore();
    }
  });
});

// 썸네일 대신 쓰는 짧은 JPEG 머리(형식 판별만).
const THUMB = btoa(String.fromCharCode(0xff, 0xd8, 0xff, 0xe0, 0, 16, 74, 70, 73, 70, 0));

describe('동영상·음성', () => {
  it('받지 않은 동영상은 보내는 쪽 썸네일과 재생 길이를 보인다', async () => {
    setup();
    const video: AttachmentView = { blob: 'V'.repeat(64), name: 'clip.webm', mime: 'video/webm', size: 3 * 1024 * 1024, width: 1920, height: 1080, duration_ms: 75_000, thumbnail_base64: THUMB, downloaded: false };
    const { el } = await render(<Attachment att={video} space="S" messageId="m1" msgKey="S/A" volatile={false} />);
    expect(el.querySelector('.att-thumb')).not.toBeNull();
    expect(el.querySelector('.att-duration')?.textContent).toContain('1:15');
    expect(el.textContent).toContain('원본 받기');
    expect(el.querySelector('video')).toBeNull();
  });

  it('미리 볼 수 없는 형식의 동영상도 파일 카드에 썸네일과 길이를 보인다', async () => {
    setup();
    const mov: AttachmentView = { blob: 'Q'.repeat(64), name: 'clip.mov', mime: 'video/quicktime', size: 1024, duration_ms: 5_000, thumbnail_base64: THUMB, downloaded: false };
    const { el } = await render(<Attachment att={mov} space="S" messageId="m1" msgKey="S/A" volatile={false} />);
    expect(el.querySelector('.att-file-thumb img')).not.toBeNull();
    expect(el.textContent).toContain('0:05');
  });

  it('음성은 길이를 보이고, 받은 뒤에는 재생기를 그린다', async () => {
    setup();
    const audio: AttachmentView = { blob: 'M'.repeat(64), name: '음성 메모.ogg', mime: 'audio/ogg', size: 200 * 1024, duration_ms: 90_000, downloaded: false };
    const { el } = await render(<Attachment att={audio} space="S" messageId="m1" msgKey="S/A" volatile={false} />);
    expect(el.textContent).toContain('1:30');
    expect(el.textContent).toContain('아직 받지 않음');
    expect(el.querySelector('audio')).toBeNull();
    await cleanup();
    useStore.setState({
      client: { call: vi.fn(async () => []), transport: { blobSrc: () => 'yangpa-blob://localhost/blob/x' } } as unknown as ApiClient,
    });
    const done = await render(<Attachment att={{ ...audio, downloaded: true }} space="S" messageId="m1" msgKey="S/A" volatile={false} />);
    expect(done.el.querySelector('audio')?.getAttribute('src')).toBe('yangpa-blob://localhost/blob/x');
  });
});

describe('보관(고정)', () => {
  const got: AttachmentView = { ...att, downloaded: true };

  function pinSetup(pins: string[] = []) {
    const call = vi.fn(async (method: string) => (method === 'blob.pins' ? pins.map((blob) => ({ space: 'S', blob, pinned_at: 1, size: 1, downloaded: true })) : {}));
    useStore.setState({ client: { call } as unknown as ApiClient, blobProgress: {}, toasts: [] });
    return call;
  }

  it('받은 첨부는 보관 단추로 고정하고 풀 수 있다(목록은 한 번만 읽음)', async () => {
    const call = pinSetup();
    const { el } = await render(<Attachment att={got} space="S" messageId="m1" msgKey="S/A" volatile={false} />);
    await vi.waitFor(() => expect(el.querySelector('button.att-pin')).not.toBeNull());
    const pin = el.querySelector<HTMLButtonElement>('button.att-pin')!;
    expect(pin.getAttribute('aria-pressed')).toBe('false');
    await click(pin);
    expect(call).toHaveBeenCalledWith('blob.pin', { space: 'S', blob: att.blob });
    await vi.waitFor(() => expect(el.querySelector('button.att-pin')?.getAttribute('aria-pressed')).toBe('true'));
    await click(el.querySelector<HTMLButtonElement>('button.att-pin')!);
    expect(call).toHaveBeenCalledWith('blob.unpin', { space: 'S', blob: att.blob });
    expect(call.mock.calls.filter((c) => c[0] === 'blob.pins')).toHaveLength(1);
    expect(call.mock.calls.some((c) => c[0] === 'blob.status')).toBe(false);
  });

  it('이미 보관한 첨부는 눌린 상태로 보인다', async () => {
    pinSetup([att.blob]);
    const { el } = await render(<Attachment att={got} space="S" messageId="m1" msgKey="S/A" volatile={false} />);
    await vi.waitFor(() => expect(el.querySelector('button.att-pin')?.getAttribute('aria-pressed')).toBe('true'));
  });

  it('내가 올린 첨부·휘발성·아직 받지 않은 첨부에는 보관 단추가 없다', async () => {
    const call = pinSetup();
    const a = await render(<Attachment att={got} space="S" messageId="m1" msgKey="S/A" volatile={false} own />);
    const b = await render(<Attachment att={got} space="S" messageId="m1" msgKey="S/A" volatile />);
    const c = await render(<Attachment att={att} space="S" messageId="m1" msgKey="S/A" volatile={false} />);
    for (const r of [a, b, c]) expect(r.el.querySelector('button.att-pin')).toBeNull();
    expect(call).not.toHaveBeenCalledWith('blob.pins', expect.anything());
    });
  });

describe('휘발성 첨부 저장', () => {
  const done: AttachmentView = { ...att, name: '비밀.bin', mime: 'application/octet-stream', downloaded: true };

  function setupSave() {
    const call = vi.fn(async (_method: string, _params?: unknown) => ({}));
    const saveDialog = vi.fn(async (name: string) => `/저장/${name}`);
    useStore.setState({ client: { call, transport: { saveDialog } } as unknown as ApiClient, blobProgress: {}, toasts: [], modal: null });
    return { call, saveDialog };
  }

  it('경고를 확인한 뒤에만 allow_volatile로 저장한다', async () => {
    const { call, saveDialog } = setupSave();
    const { el } = await render(<Attachment att={done} space="S" messageId="m1" msgKey="S/A" volatile />);
    await click(el.querySelector<HTMLButtonElement>('button[title="저장(경고 확인 뒤)"]')!);
    // 확인 전에는 저장 위치도 묻지 않습니다.
    expect(saveDialog).not.toHaveBeenCalled();
    expect(call).not.toHaveBeenCalled();
    const modal = useStore.getState().modal;
    expect(modal?.kind).toBe('confirm');
    if (modal?.kind !== 'confirm') return;
    expect(modal.body).toBe(VOLATILE_SAVE_WARNING);
    expect(modal.body).toContain('지워지지 않습니다');
    expect(modal.danger).toBe(true);
    await act(async () => {
      await modal.onConfirm();
    });
    expect(saveDialog).toHaveBeenCalledWith('비밀.bin');
    expect(call).toHaveBeenCalledWith('blob.save', { space: 'S', blob: done.blob, key_from: 'm1', path: '/저장/비밀.bin', allow_volatile: true });
  });

  it('일반 첨부는 확인 없이 저장하고 allow_volatile을 보내지 않는다', async () => {
    const { call } = setupSave();
    const { el } = await render(<Attachment att={done} space="S" messageId="m1" msgKey="S/A" volatile={false} />);
    await click(el.querySelector<HTMLButtonElement>('button[title="저장"]')!);
    expect(useStore.getState().modal).toBeNull();
    await vi.waitFor(() => expect(call).toHaveBeenCalledWith('blob.save', { space: 'S', blob: done.blob, key_from: 'm1', path: '/저장/비밀.bin' }));
  });
});

describe('파일 다시 잇기', () => {
  const view = (security: 'normal' | 'volatile') =>
    ({ space: 'S', kind: 'crew', settings: { name: '크루' }, members: [], channels: [{ channel: 'C', settings: { name: '잡담', security } }] }) as never;

  function relinkSetup(o: { security?: 'normal' | 'volatile'; fail?: RpcError; picked?: string[] } = {}) {
    const call = vi.fn(async (method: string, _params?: unknown) => {
      if (method === 'blob.relink') {
        if (o.fail) throw o.fail;
        return { have_chunks: 4, total_chunks: 4, seeders_online: 0, size: att.size, downloading: false, pinned: true };
      }
      return {};
    });
    const pickFiles = vi.fn(async () => (o.picked ?? ['/새 위치/자료.zip']).map((path) => ({ path, name: path.split('/').pop()! })));
    useStore.setState({
      client: { call, transport: { pickFiles } } as unknown as ApiClient,
      views: { S: view(o.security ?? 'normal') },
      blobProgress: {},
      toasts: [],
    });
    return { call, pickFiles };
  }

  const relinkBtn = (el: HTMLElement) => el.querySelector<HTMLButtonElement>('button[aria-label="파일 다시 잇기"]');

  it('같은 파일을 고르면 blob.relink를 부르고 받은 첨부가 된다', async () => {
    const { call } = relinkSetup();
    const { el } = await render(<Attachment att={att} space="S" channel="C" messageId="m1" msgKey="S/A" volatile={false} />);
    expect(relinkBtn(el)?.title).toContain('해시');
    await click(relinkBtn(el)!);
    await vi.waitFor(() => expect(call).toHaveBeenCalledWith('blob.relink', { space: 'S', channel: 'C', message: 'm1', blob: att.blob, path: '/새 위치/자료.zip' }));
    await vi.waitFor(() => expect(relinkBtn(el)).toBeNull());
    expect(el.querySelector('button[title="저장"]')).not.toBeNull();
    expect(useStore.getState().blobProgress[att.blob]).toEqual({ have: 4, total: 4 });
    expect(useStore.getState().toasts.at(-1)).toMatchObject({ kind: 'success', text: expect.stringContaining('다시 이었습니다') });
  });

  it('내용이 다르면 데몬이 준 까닭을 첨부 옆과 알림에 보인다', async () => {
    const msg = '파일 내용이 첨부와 달라 다시 이을 수 없습니다(해시가 다름)';
    const { call } = relinkSetup({ fail: new RpcError(1005, msg) });
    const { el } = await render(<Attachment att={att} space="S" channel="C" messageId="m1" msgKey="S/A" volatile={false} />);
    await click(relinkBtn(el)!);
    await vi.waitFor(() => expect(el.querySelector('.att-file-err')?.textContent).toBe(msg));
    expect(call.mock.calls.filter((c) => c[0] === 'blob.relink')).toHaveLength(1);
    expect(relinkBtn(el)).not.toBeNull();
    expect(useStore.getState().toasts.at(-1)).toMatchObject({ kind: 'error', text: msg });
  });

  it('파일을 고르지 않으면 부르지 않는다', async () => {
    const { call } = relinkSetup({ picked: [] });
    const { el } = await render(<Attachment att={att} space="S" channel="C" messageId="m1" msgKey="S/A" volatile={false} />);
    await click(relinkBtn(el)!);
    expect(call).not.toHaveBeenCalled();
  });

  it('휘발성 채널·휘발성 메시지·채널을 모를 때·이미 받은 첨부에는 없다', async () => {
    relinkSetup({ security: 'volatile' });
    let r = await render(<Attachment att={att} space="S" channel="C" messageId="m1" msgKey="S/A" volatile={false} />);
    expect(relinkBtn(r.el)).toBeNull();
    await cleanup();
    relinkSetup();
    r = await render(<Attachment att={att} space="S" channel="C" messageId="m1" msgKey="S/A" volatile />);
    expect(relinkBtn(r.el)).toBeNull();
    await cleanup();
    r = await render(<Attachment att={att} space="S" messageId="m1" msgKey="S/A" volatile={false} />);
    expect(relinkBtn(r.el)).toBeNull();
    await cleanup();
    r = await render(<Attachment att={{ ...att, downloaded: true }} space="S" channel="C" messageId="m1" msgKey="S/A" volatile={false} />);
    expect(relinkBtn(r.el)).toBeNull();
  });

  it('사진은 원본 받기 옆에 글자 단추로 보인다', async () => {
    relinkSetup();
    const img: AttachmentView = { ...att, name: '사진.png', mime: 'image/png', width: 100, height: 80 };
    const { el } = await render(<Attachment att={img} space="S" channel="C" messageId="m1" msgKey="S/A" volatile={false} />);
    expect([...el.querySelectorAll('button.att-relink')].map((b) => b.textContent?.trim())).toEqual(['파일 다시 잇기']);
  });
});

// 웹뷰 자체 저장(우클릭 "다른 이름으로 저장", 플레이어 다운로드)은 셸이 취소하고 주소만 알려 줍니다. 그린 첨부면 앱의
// 저장으로 이어 고른 곳·이름에 쓰고(휘발성은 경고 뒤), 링크 저장은 Tor를 거치지 않으므로 막았다고 알립니다.
describe('웹뷰 자체 저장', () => {
  const voice: AttachmentView = { blob: 'M'.repeat(64), name: '음성‮gpj.ogg', mime: 'audio/ogg', size: 1024, downloaded: true };

  function setupTransport() {
    const call = vi.fn(async (_method: string, _params?: unknown) => ({}));
    const saveDialog = vi.fn(async (name: string) => `/저장/${name}`);
    const transport = {
      saveDialog,
      blobSrc: (space: string, blob: string, keyFrom: string) => `yangpa-blob://localhost/blob/${space}/${blob}/${keyFrom}`,
      readBlob: async () => new Uint8Array([1, 2, 3]),
    };
    useStore.setState({ client: { call, transport } as unknown as ApiClient, blobProgress: {}, toasts: [], modal: null });
    return { call, saveDialog };
  }

  it('그린 첨부의 주소면 앱의 저장 창으로 이어 정리한 이름으로 저장한다', async () => {
    const { call, saveDialog } = setupTransport();
    const { el } = await render(<Attachment att={voice} space="S" messageId="m1" msgKey="S/A" volatile={false} />);
    const url = el.querySelector('audio')!.getAttribute('src')!;
    const notify = vi.fn();
    await act(async () => onWebviewSave({ url }, notify));
    await vi.waitFor(() => expect(call).toHaveBeenCalledWith('blob.save', { space: 'S', blob: voice.blob, key_from: 'm1', path: '/저장/음성gpj.ogg' }));
    expect(saveDialog).toHaveBeenCalledWith('음성gpj.ogg');
    expect(notify).not.toHaveBeenCalled();
  });

  it('웹뷰가 이미 저장 창에서 고른 경로(Windows)면 저장 창을 다시 띄우지 않는다', async () => {
    const { call, saveDialog } = setupTransport();
    const { el } = await render(<Attachment att={voice} space="S" messageId="m1" msgKey="S/A" volatile={false} />);
    const url = el.querySelector('audio')!.getAttribute('src')!;
    await act(async () => onWebviewSave({ url, path: 'E:/받기/고른 이름.ogg' }, vi.fn()));
    await vi.waitFor(() => expect(call).toHaveBeenCalledWith('blob.save', { space: 'S', blob: voice.blob, key_from: 'm1', path: 'E:/받기/고른 이름.ogg' }));
    expect(saveDialog).not.toHaveBeenCalled();
  });

  it('휘발성 첨부는 경고를 확인하기 전에는 저장 위치도 묻지 않는다', async () => {
    const { call, saveDialog } = setupTransport();
    const { el } = await render(<Attachment att={voice} space="S" messageId="m1" msgKey="S/A" volatile />);
    await vi.waitFor(() => expect(el.querySelector('audio')).not.toBeNull());
    await act(async () => onWebviewSave({ url: el.querySelector('audio')!.getAttribute('src')!, path: 'E:/x.ogg' }, vi.fn()));
    expect(useStore.getState().modal?.kind).toBe('confirm');
    expect(saveDialog).not.toHaveBeenCalled();
    expect(call).not.toHaveBeenCalled();
  });

  it('그린 첨부가 아닌 주소(링크 저장)는 저장하지 않고 알린다', async () => {
    const { call, saveDialog } = setupTransport();
    const notify = vi.fn();
    onWebviewSave({ url: 'https://example.com/file.zip', path: 'E:/file.zip' }, notify);
    expect(notify).toHaveBeenCalledOnce();
    expect(saveDialog).not.toHaveBeenCalled();
    expect(call).not.toHaveBeenCalled();
  });

  it('화면에서 빠진 첨부의 주소는 더는 잇지 않는다', async () => {
    setupTransport();
    const { el } = await render(<Attachment att={voice} space="S" messageId="m1" msgKey="S/A" volatile={false} />);
    const url = el.querySelector('audio')!.getAttribute('src')!;
    await cleanup();
    const notify = vi.fn();
    onWebviewSave({ url }, notify);
    expect(notify).toHaveBeenCalledOnce();
  });

  it('같은 첨부를 두 곳(목록·콘텐츠 뷰어)에 그렸다가 한 곳이 빠져도 남은 곳으로 잇는다', async () => {
    const { call } = setupTransport();
    const { el } = await render(<Attachment att={voice} space="S" messageId="m1" msgKey="S/A" volatile={false} />);
    const viewer = await render(<Attachment att={voice} space="S" messageId="m1" msgKey="S/A" volatile={false} />);
    const url = el.querySelector('audio')!.getAttribute('src')!;
    await viewer.unmount();
    const notify = vi.fn();
    await act(async () => onWebviewSave({ url, path: 'E:/x.ogg' }, notify));
    await vi.waitFor(() => expect(call).toHaveBeenCalledWith('blob.save', { space: 'S', blob: voice.blob, key_from: 'm1', path: 'E:/x.ogg' }));
    expect(notify).not.toHaveBeenCalled();
  });
});

// 플레이어 iframe은 웹뷰가 Tor 없이 불러오므로, 직접 연결임을 알 때만, 데몬이 만드는 주소만 엽니다.
describe('링크 미리보기 플레이어', () => {
  const yt = 'https://www.youtube-nocookie.com/embed/abcdefghijk';
  const embed = (player_url: string): EmbedView => ({ url: 'https://example.com/v', title: '영상', description: '', site_name: 'YouTube', player_url });
  const playButton = (el: HTMLElement) => el.querySelector<HTMLButtonElement>('button.embed-thumb.player');
  const direct = () => useStore.setState({ net: { mode: 'normal' } as NetStatus });

  it('제목은 보낸 사람이 정하므로 실제로 열 주소를 툴팁으로 보인다', async () => {
    const { el } = await render(<Embed embed={embed(yt)} volatile={false} />);
    expect(el.querySelector<HTMLButtonElement>('button.embed-title')?.title).toBe('https://example.com/v');
  });

  it('연결 방식을 아직 모르면 Tor로 보고 플레이어를 끈다', async () => {
    const { el } = await render(<Embed embed={embed(yt)} volatile={false} />);
    expect(playButton(el)?.disabled).toBe(true);
  });

  it('직접 연결이어도 데몬이 만들지 않는 주소는 플레이어로 그리지 않는다', async () => {
    direct();
    const { el } = await render(<Embed embed={embed('https://track.example/v/1')} volatile={false} />);
    expect(playButton(el)).toBeNull();
    expect(el.querySelector('iframe')).toBeNull();
  });

  // 누르면 시험 환경이 iframe 주소로 실제 접속하므로 버튼이 켜졌는지만 봅니다.
  it('직접 연결이면 플레이어를 누를 수 있다', async () => {
    direct();
    const { el } = await render(<Embed embed={embed(yt)} volatile={false} />);
    expect(playButton(el)?.disabled).toBe(false);
    expect(playButton(el)?.title).toContain('www.youtube-nocookie.com');
  });
});
