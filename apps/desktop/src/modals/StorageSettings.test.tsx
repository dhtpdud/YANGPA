// @vitest-environment happy-dom
// 사용자 설정 → 저장 공간: 사용량·한도를 보여 주고, 한도는 GiB로 받아 바이트로 보내며, 비우기는 확인 뒤에만, 보관 풀기는 목록에서 뺍니다.

import { act } from 'react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { RpcError, type ApiClient } from '../api/client';
import { ErrorCode, type BlobCacheView, type BlobPinView } from '../api/types';
import { usePinStore, resetPinsForTest } from '../hooks/pins';
import { useStore } from '../store/store';
import { button, cleanup, click, render } from '../test/dom';
import { StorageTab, parseGib, toGib } from './StorageSettings';

const GIB = 1024 ** 3;
const VIEW: BlobCacheView = {
  cache_bytes: GIB,
  cache_count: 12,
  pinned_bytes: 5 * 1024 * 1024,
  pinned_count: 1,
  kept_bytes: 3 * 1024 * 1024,
  kept_count: 4,
  limit_bytes: 2 * GIB,
  min_free_bytes: GIB,
  default_limit_bytes: 2 * GIB,
  default_min_free_bytes: GIB,
  disk_free_bytes: 50 * GIB,
  evicted_count: 0,
  evicted_bytes: 0,
};
const PIN: BlobPinView = { space: 'S'.repeat(22), blob: 'B'.repeat(44), pinned_at: Date.now() - 60_000, size: 5 * 1024 * 1024, downloaded: true };

function setup(opts: { unsupported?: boolean; failOnce?: boolean } = {}) {
  let view = { ...VIEW };
  let fail = !!opts.failOnce;
  const call = vi.fn(async (method: string, params?: Record<string, unknown>) => {
    if (opts.unsupported) throw new RpcError(ErrorCode.MethodNotFound, '없는 메서드');
    if (fail) {
      fail = false;
      throw new RpcError(ErrorCode.Internal, '데몬 연결이 끊겼습니다');
    }
    switch (method) {
      case 'blob.cache_status':
        return view;
      case 'blob.pins':
        return [PIN];
      case 'blob.cache_set_limit':
        view = params?.reset
          ? { ...view, limit_bytes: view.default_limit_bytes, min_free_bytes: view.default_min_free_bytes }
          : {
              ...view,
              limit_bytes: (params?.limit_bytes as number | undefined) ?? view.limit_bytes,
              min_free_bytes: (params?.min_free_bytes as number | undefined) ?? view.min_free_bytes,
            };
        return view;
      case 'blob.cache_clear':
        return { evicted_count: 12, freed_bytes: GIB };
      default:
        return {};
    }
  });
  useStore.setState({ client: { call } as unknown as ApiClient, modal: null, toasts: [], spaces: [] });
  return call;
}

afterEach(async () => {
  await cleanup();
  resetPinsForTest();
  useStore.setState({ client: null, modal: null, toasts: [] });
});

/** React가 다루는 입력의 값을 바꿉니다. */
async function typeInto(input: HTMLInputElement, value: string) {
  await act(async () => {
    const set = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')!.set!;
    set.call(input, value);
    input.dispatchEvent(new Event('input', { bubbles: true }));
  });
}

describe('GiB 변환', () => {
  it('입력 문자열을 바이트로, 바이트를 입력 문자열로', () => {
    expect(parseGib('1.5')).toBe(1.5 * GIB);
    expect(parseGib('2,5')).toBe(2.5 * GIB);
    expect(parseGib('0')).toBe(0);
    expect(parseGib('')).toBeNull();
    expect(parseGib('-1')).toBeNull();
    expect(parseGib('abc')).toBeNull();
    expect(toGib(2 * GIB)).toBe('2');
    expect(toGib(1.25 * GIB)).toBe('1.3');
  });
});

describe('StorageTab', () => {
  it('사용량과 한도를 보여 주고, 바꾼 한도를 바이트로 보낸다', async () => {
    const call = setup();
    const { el } = await render(<StorageTab />);
    await vi.waitFor(() => expect(el.textContent).toContain('받은 첨부'));
    expect(el.textContent).toContain('12개');
    expect(el.textContent).toContain('1.0 GiB / 2.0 GiB');
    expect(el.querySelector('[role="meter"]')?.getAttribute('aria-valuenow')).toBe('50');
    expect(button(el, '적용').disabled).toBe(true);
    expect(button(el, '기본값으로').disabled).toBe(true);

    await typeInto(el.querySelector<HTMLInputElement>('input[aria-label="받은 첨부 한도"]')!, '0.5');
    expect(button(el, '적용').disabled).toBe(false);
    await click(button(el, '적용'));
    expect(call).toHaveBeenCalledWith('blob.cache_set_limit', { limit_bytes: 0.5 * GIB, min_free_bytes: GIB });
    await vi.waitFor(() => expect(button(el, '기본값으로').disabled).toBe(false));
    expect(el.querySelector('.storage-meter .bar')?.className).toContain('over');

    await click(button(el, '기본값으로'));
    expect(call).toHaveBeenCalledWith('blob.cache_set_limit', { reset: true });
    await vi.waitFor(() => expect(button(el, '기본값으로').disabled).toBe(true));
  });

  it('비우기는 확인한 뒤에만 부르고 결과를 알린다', async () => {
    const call = setup();
    const { el } = await render(<StorageTab />);
    await vi.waitFor(() => expect(el.textContent).toContain('받은 첨부 비우기'));
    await click(button(el, '받은 첨부 비우기'));
    expect(call).not.toHaveBeenCalledWith('blob.cache_clear', expect.anything());
    const m = useStore.getState().modal;
    expect(m).toMatchObject({ kind: 'confirm', danger: true });
    if (m?.kind !== 'confirm') return;
    await act(async () => {
      await m.onConfirm();
    });
    expect(call).toHaveBeenCalledWith('blob.cache_clear', {});
    expect(useStore.getState().toasts.map((t) => t.text).join()).toContain('12개');
  });

  it('보관한 첨부를 풀면 목록과 고정 표시에서 뺀다', async () => {
    const call = setup();
    usePinStore.setState({ status: 'ready', ids: new Set([PIN.blob]) });
    const { el } = await render(<StorageTab />);
    await vi.waitFor(() => expect(el.textContent).toContain('보관 풀기'));
    await click(button(el, '보관 풀기'));
    expect(call).toHaveBeenCalledWith('blob.unpin', { space: PIN.space, blob: PIN.blob });
    await vi.waitFor(() => expect(el.textContent).toContain('보관한 첨부가 없습니다'));
    expect(usePinStore.getState().ids.has(PIN.blob)).toBe(false);
  });

  it('잠깐의 오류는 지원하지 않음으로 보지 않고, 알린 뒤 다시 읽을 수 있다', async () => {
    setup({ failOnce: true });
    const { el } = await render(<StorageTab />);
    await vi.waitFor(() => expect(el.textContent).toContain('데몬 연결이 끊겼습니다'));
    expect(el.textContent).not.toContain('지원하지 않습니다');
    await click(button(el, '다시 읽기'));
    await vi.waitFor(() => expect(el.textContent).toContain('받은 첨부'));
    expect(el.textContent).not.toContain('데몬 연결이 끊겼습니다');
  });

  it('저장 공간 정리를 모르는 데몬이면 알리기만 한다', async () => {
    setup({ unsupported: true });
    const { el } = await render(<StorageTab />);
    await vi.waitFor(() => expect(el.textContent).toContain('지원하지 않습니다'));
    expect(el.querySelectorAll('button')).toHaveLength(0);
  });
});
