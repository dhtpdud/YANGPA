// @vitest-environment happy-dom
// 알림 설정 탭: 켜고 끄기, 크루 알림 범위, 내용 미리보기가 UI 설정 키로 저장됩니다.

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { useStore } from '../store/store';
import { button, cleanup, click, render } from '../test/dom';
import { NotifyTab } from './UserSettings';

const setPrefs = vi.fn((patch: object) => useStore.setState({ prefs: { ...useStore.getState().prefs, ...patch } }));
const original = useStore.getState().setPrefs;

beforeEach(() => {
  useStore.setState({ prefs: { ...useStore.getState().prefs, notify: true, notifyCrew: 'all', notifyPreview: true }, setPrefs });
});

afterEach(async () => {
  await cleanup();
  setPrefs.mockClear();
  useStore.setState({ setPrefs: original });
});

describe('알림 설정 탭', () => {
  it('세 가지 설정을 각각의 키로 바꾼다', async () => {
    const { el } = await render(<NotifyTab />);
    await click(button(el, '나를 멘션한 메시지만'));
    expect(setPrefs).toHaveBeenLastCalledWith({ notifyCrew: 'mentions' });
    expect(button(el, '나를 멘션한 메시지만').getAttribute('aria-checked')).toBe('true');
    await click(el.querySelector<HTMLButtonElement>('[aria-label="내용 미리보기"]')!);
    expect(setPrefs).toHaveBeenLastCalledWith({ notifyPreview: false });
    await click(el.querySelector<HTMLButtonElement>('[aria-label="OS 알림"]')!);
    expect(setPrefs).toHaveBeenLastCalledWith({ notify: false });
  });

  it('알림을 끄면 나머지 설정은 잠긴다', async () => {
    useStore.setState({ prefs: { ...useStore.getState().prefs, notify: false } });
    const { el } = await render(<NotifyTab />);
    expect(button(el, '끄기').disabled).toBe(true);
    expect(el.querySelector<HTMLButtonElement>('[aria-label="내용 미리보기"]')!.disabled).toBe(true);
  });
});
