// @vitest-environment happy-dom
// 기기 설정 탭: 폐기한 기기는 데몬 목록에 남아 있어도 내 기기 목록에 보이지 않습니다.

import { afterEach, describe, expect, it, vi } from 'vitest';
import type { ApiClient } from '../api/client';
import type { DeviceEntry, IdentityView } from '../api/types';
import { useStore } from '../store/store';
import { button, cleanup, click, render } from '../test/dom';
import { DevicesTab } from './UserSettings';

const device = (id: string, name: string, revoked_at: number | null) => ({ device: id, name, added_at: 1, revoked_at }) as DeviceEntry;

describe('기기 설정 탭', () => {
  afterEach(cleanup);

  it('폐기한 기기는 목록에서 빠지고 남은 기기만 보인다', async () => {
    const devices = [device('a', '이 PC', null), device('b', '노트북', null), device('c', '잃어버린 휴대폰', 2)];
    const client = { call: vi.fn(async () => devices) } as unknown as ApiClient;
    useStore.setState({ client, identity: { device: 'a', devices } as unknown as IdentityView });
    const { el } = await render(<DevicesTab />);
    expect(el.textContent).toContain('이 PC');
    expect(el.textContent).toContain('노트북');
    expect(el.textContent).not.toContain('잃어버린 휴대폰');
  });

  it('휴대폰은 휴대폰 아이콘으로 보인다', async () => {
    const devices = [device('a', '이 PC', null), { ...device('b', 'Android', null), mobile: true }];
    const client = { call: vi.fn(async () => devices) } as unknown as ApiClient;
    useStore.setState({ client, identity: { device: 'a', devices } as unknown as IdentityView });
    const { el } = await render(<DevicesTab />);
    const rows = [...el.querySelectorAll('.tr')];
    expect(rows[0].querySelector('.lucide-laptop')).not.toBeNull();
    expect(rows[1].querySelector('.lucide-smartphone')).not.toBeNull();
  });

  it('기기 이름을 바꾼다', async () => {
    const devices = [device('a', 'Windows', null)];
    const call = vi.fn(async () => devices);
    useStore.setState({ client: { call } as unknown as ApiClient, identity: { device: 'a', devices } as unknown as IdentityView });
    const { el } = await render(<DevicesTab />);
    await click(el.querySelector<HTMLElement>('[aria-label="이름 바꾸기"]')!);
    expect(el.querySelector<HTMLInputElement>('input')!.value).toBe('Windows');
    await click(button(el, '저장'));
    expect(call).toHaveBeenCalledWith('device.rename', { device: 'a', name: 'Windows' });
    expect(el.querySelector('input')).toBeNull();
  });
});
