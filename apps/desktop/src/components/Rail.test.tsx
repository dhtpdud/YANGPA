// @vitest-environment happy-dom
// 왼쪽 레일 맨 아래 Tor 표시 바로 위의 전체 전송 속도: ↑ 올리기(초록), ↓ 받기(빨강). transfer.rates를 받기 전에는 그리지 않습니다.

import { act } from 'react';
import { afterEach, describe, expect, it } from 'vitest';
import { useStore } from '../store/store';
import { cleanup, render } from '../test/dom';
import { TransferSpeed } from './Rail';

afterEach(async () => {
  await cleanup();
  useStore.setState({ rates: null });
});

describe('전체 전송 속도', () => {
  it('값이 없으면 그리지 않는다', async () => {
    useStore.setState({ rates: null });
    const { el } = await render(<TransferSpeed />);
    expect(el.querySelector('.rail-rates')).toBeNull();
  });

  it('올리기는 ↑ 초록, 받기는 ↓ 빨강으로 보이고 알림이 오면 바뀐다', async () => {
    useStore.setState({ rates: { up_bps: 1536, down_bps: 3 * 1024 * 1024 } });
    const { el } = await render(<TransferSpeed />);
    expect(el.querySelector('.rate-up')?.textContent).toBe('↑ 1.5 KiB/s');
    expect(el.querySelector('.rate-down')?.textContent).toBe('↓ 3.0 MiB/s');
    expect(el.querySelector('[role="status"]')?.getAttribute('aria-label')).toBe('올리기 1.5 KiB/s, 받기 3.0 MiB/s');
    await act(async () => useStore.setState({ rates: { up_bps: 0, down_bps: 512 } }));
    expect(el.querySelector('.rate-up')?.textContent).toBe('↑ 0 B/s');
    expect(el.querySelector('.rate-down')?.textContent).toBe('↓ 512 B/s');
  });
});
