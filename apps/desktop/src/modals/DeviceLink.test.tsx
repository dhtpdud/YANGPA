// @vitest-environment happy-dom
// 기기 연결: 새 기기 화면은 번호를 보이고 결과에 따라 되돌아가며, 쓰던 기기의 허용 창은 닫으면 거절입니다.

import { act } from 'react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import type { ApiClient } from '../api/client';
import { useStore } from '../store/store';
import { button, cleanup, click, render } from '../test/dom';
import { DeviceLinkApproveModal, DeviceLinkJoin, scanState } from './DeviceLink';

const LINK = 'yangpa://device/abc123';

vi.mock('zxing-wasm/reader', () => ({ prepareZXingModule: () => Promise.reject(new Error('wasm')) }));

function setup(reply: (method: string) => unknown) {
  const calls: Array<[string, unknown]> = [];
  const client = {
    call: vi.fn(async (method: string, params: unknown) => {
      calls.push([method, params]);
      return reply(method);
    }),
  } as unknown as ApiClient;
  useStore.setState({ client, toasts: [], deviceLinkRequest: null, deviceLinkResult: null, deviceLinkUrl: null });
  return calls;
}

async function paste(el: HTMLElement, url: string) {
  const input = el.querySelector('textarea') as HTMLTextAreaElement;
  const set = Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, 'value')!.set!;
  await act(async () => {
    set.call(input, url);
    input.dispatchEvent(new Event('input', { bubbles: true }));
  });
}

describe('QR 스캐너 안내', () => {
  afterEach(cleanup);

  it('못 읽어도 위치가 나오면 보인다고 하고, 기기 연결이 아닌 QR은 넘기지 않는다', () => {
    expect(scanState(undefined)).toBe('find');
    expect(scanState({ isValid: false, text: '' })).toBe('seen');
    expect(scanState({ isValid: true, text: 'https://example.com' })).toBe('wrong');
    expect(scanState({ isValid: true, text: 'yangpa://friend/abc' })).toBe('wrong');
    expect(scanState({ isValid: true, text: LINK })).toBe('device');
  });

  it('해독기를 못 불러오면 붙여 넣기를 안내하고, 늦게 열린 카메라도 끈다', async () => {
    setup(() => ({}));
    let open = (_: MediaStream) => {};
    const getUserMedia = () => new Promise<MediaStream>((done) => (open = done));
    Object.defineProperty(navigator, 'mediaDevices', { configurable: true, value: { getUserMedia } });
    const { el } = await render(<DeviceLinkJoin />);
    await click(button(el, 'QR 찍기'));
    await act(() => vi.waitFor(() => expect(el.textContent).toContain('카메라를 열지 못했습니다')));

    const stop = vi.fn();
    await act(async () => open({ getTracks: () => [{ stop }] } as unknown as MediaStream));
    expect(stop).toHaveBeenCalledOnce();
  });
});

describe('새 기기 연결 화면', () => {
  afterEach(cleanup);

  it('링크를 넣으면 번호를 보이고, 거절되면 까닭과 함께 다시 넣게 한다', async () => {
    const calls = setup(() => ({ code: '123456' }));
    const { el } = await render(<DeviceLinkJoin />);
    await paste(el, ` ${LINK} `);
    await click(button(el, '연결 요청'));
    expect(calls).toEqual([['device.link.join', { url: LINK }]]);
    expect(el.textContent).toContain('123456');

    await act(async () => useStore.setState({ deviceLinkResult: 'rejected' }));
    expect(el.textContent).not.toContain('123456');
    expect(el.textContent).toContain('쓰던 기기에서 거절했습니다');
    expect(useStore.getState().deviceLinkResult).toBeNull();
  });

  it('OS가 넘긴 링크는 칸에 채우기만 하고 보내지 않는다', async () => {
    const calls = setup(() => ({}));
    useStore.setState({ deviceLinkUrl: LINK });
    const { el } = await render(<DeviceLinkJoin />);
    expect((el.querySelector('textarea') as HTMLTextAreaElement).value).toBe(LINK);
    expect(calls).toEqual([]);
    expect(useStore.getState().deviceLinkUrl).toBeNull();
  });

  it('그만두면 쓰던 기기에 묻는 것도 그만둔다', async () => {
    const calls = setup(() => ({ code: '123456' }));
    const { el } = await render(<DeviceLinkJoin />);
    await paste(el, LINK);
    await click(button(el, '연결 요청'));
    await click(button(el, '그만두기'));
    expect(calls.at(-1)).toEqual(['device.link.abort', {}]);
    expect(el.querySelector('textarea')).not.toBeNull();
  });
});

describe('새 기기 허용 창', () => {
  afterEach(cleanup);
  const request = { name: '새 휴대폰', code: '123456', contested: false };

  it('허용하면 번호를 본 판단을 보내고 창을 닫는다', async () => {
    const calls = setup(() => ({}));
    useStore.setState({ deviceLinkRequest: request });
    const { el } = await render(<DeviceLinkApproveModal />);
    expect(el.textContent).toContain('123456');
    expect(el.textContent).not.toContain('다른 기기도');
    await click(button(el, '허용'));
    expect(calls).toEqual([['device.link.decide', { approve: true }]]);
    expect(useStore.getState().deviceLinkRequest).toBeNull();
  });

  it('다른 기기도 링크를 썼으면 경고하고, 거절은 허용으로 보내지 않는다', async () => {
    const calls = setup(() => ({}));
    useStore.setState({ deviceLinkRequest: { ...request, contested: true } });
    const { el } = await render(<DeviceLinkApproveModal />);
    expect(el.textContent).toContain('다른 기기도 이 링크를 썼습니다');
    await click(button(el, '거절'));
    expect(calls).toEqual([['device.link.decide', { approve: false }]]);
  });
});
