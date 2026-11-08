// @vitest-environment happy-dom
// 설정 → 음성·화면공유 → 장치: 고른 마이크·스피커는 ID와 이름을 함께 저장하고, 빠진 장치는 '(연결 안 됨)'으로 남깁니다.

import { act } from 'react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { useStore } from '../store/store';
import { cleanup, render } from '../test/dom';
import { DEFAULT_MEDIA } from '../voice/media';
import { AudioDevicesSection, levelOf } from './AudioDevices';

const dev = (kind: MediaDeviceKind, deviceId: string, label: string) => ({ kind, deviceId, label, groupId: '' }) as MediaDeviceInfo;
const DEVICES = [
  dev('audioinput', 'default', '기본값 - 헤드셋 마이크'),
  dev('audioinput', 'mic-a', '헤드셋 마이크'),
  dev('audioinput', 'mic-b', '웹캠 마이크'),
  dev('audiooutput', 'default', '기본값 - 스피커'),
  dev('audiooutput', 'spk-a', '스피커'),
];

async function choose(select: HTMLSelectElement, value: string) {
  await act(async () => {
    Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype, 'value')!.set!.call(select, value);
    select.dispatchEvent(new Event('change', { bubbles: true }));
  });
}

beforeEach(() => {
  Object.defineProperty(navigator, 'mediaDevices', {
    configurable: true,
    value: { enumerateDevices: async () => DEVICES, addEventListener: () => {}, removeEventListener: () => {} },
  });
  useStore.setState({ client: null, prefs: { ...useStore.getState().prefs, media: DEFAULT_MEDIA } });
});

afterEach(async () => {
  await cleanup();
  useStore.setState({ prefs: { ...useStore.getState().prefs, media: DEFAULT_MEDIA } });
});

describe('마이크·스피커 장치 설정', () => {
  it('고른 마이크를 ID·이름으로 저장하고, 시스템 기본을 고르면 비운다', async () => {
    const { el } = await render(<AudioDevicesSection />);
    const mic = el.querySelector<HTMLSelectElement>('select[aria-label="마이크"]')!;
    await vi.waitFor(() => expect(mic.options.length).toBe(3));
    // '기본' 가짜 장치는 목록에 따로 넣지 않고 '시스템 기본' 칸에 이름만 붙입니다.
    expect([...mic.options].map((o) => o.textContent)).toEqual(['시스템 기본 (헤드셋 마이크)', '헤드셋 마이크', '웹캠 마이크']);
    await choose(mic, 'mic-b');
    expect(useStore.getState().prefs.media.devices.input).toEqual({ id: 'mic-b', label: '웹캠 마이크' });
    await choose(mic, '');
    expect(useStore.getState().prefs.media.devices.input).toBeNull();
  });

  it('권한 전 목록(이름 없는 장치 하나)이면 마이크를 잠깐 열어 모든 장치를 보여 준다', async () => {
    const stop = vi.fn();
    let granted = false;
    const getUserMedia = vi.fn(async () => {
      granted = true;
      return { getTracks: () => [{ stop }] } as unknown as MediaStream;
    });
    Object.defineProperty(navigator, 'mediaDevices', {
      configurable: true,
      value: {
        enumerateDevices: async () => (granted ? DEVICES : [dev('audioinput', '', ''), dev('audiooutput', '', '')]),
        getUserMedia,
        addEventListener: () => {},
        removeEventListener: () => {},
      },
    });
    const { el } = await render(<AudioDevicesSection />);
    const mic = el.querySelector<HTMLSelectElement>('select[aria-label="마이크"]')!;
    const spk = el.querySelector<HTMLSelectElement>('select[aria-label="스피커"]')!;
    await vi.waitFor(() => expect(mic.options.length).toBe(3));
    expect([...spk.options].map((o) => o.textContent)).toEqual(['시스템 기본 (스피커)', '스피커']);
    expect(getUserMedia).toHaveBeenCalledTimes(1);
    expect(stop).toHaveBeenCalled();
  });

  it('ID가 바뀐 장치는 이름으로 찾고, 빠진 장치는 (연결 안 됨)으로 보여 준다', async () => {
    const media = { ...DEFAULT_MEDIA, devices: { ...DEFAULT_MEDIA.devices, input: { id: 'old-id', label: '웹캠 마이크' }, output: { id: 'gone', label: 'USB 스피커' } } };
    useStore.setState({ prefs: { ...useStore.getState().prefs, media } });
    const { el } = await render(<AudioDevicesSection />);
    const mic = el.querySelector<HTMLSelectElement>('select[aria-label="마이크"]')!;
    const spk = el.querySelector<HTMLSelectElement>('select[aria-label="스피커"]')!;
    await vi.waitFor(() => expect(mic.value).toBe('mic-b'));
    expect(spk.value).toBe('__missing__');
    expect(spk.options[spk.options.length - 1].textContent).toBe('(연결 안 됨) USB 스피커');
  });

  it('음량 막대는 잠깐 멈춘 뒤 저장하고, 다른 칸을 되돌리지 않는다', async () => {
    const { el } = await render(<AudioDevicesSection />);
    const mic = el.querySelector<HTMLSelectElement>('select[aria-label="마이크"]')!;
    await vi.waitFor(() => expect(mic.options.length).toBe(3));
    const vol = el.querySelector<HTMLInputElement>('input[aria-label="마이크 음량"]')!;
    await act(async () => {
      Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')!.set!.call(vol, '150');
      vol.dispatchEvent(new Event('input', { bubbles: true }));
    });
    expect(useStore.getState().prefs.media.devices.inputVolume).toBe(100);
    await choose(mic, 'mic-a');
    await vi.waitFor(() => expect(useStore.getState().prefs.media.devices.inputVolume).toBe(150));
    expect(useStore.getState().prefs.media.devices.input).toEqual({ id: 'mic-a', label: '헤드셋 마이크' });
  });

  it('입력 크기 막대는 -60dB~0dB를 0~1로', () => {
    expect(levelOf(0)).toBe(0);
    expect(levelOf(1)).toBe(1);
    expect(levelOf(0.001)).toBe(0);
    expect(levelOf(0.0316)).toBeGreaterThan(0.4);
    expect(levelOf(0.0316)).toBeLessThan(0.6);
  });
});
