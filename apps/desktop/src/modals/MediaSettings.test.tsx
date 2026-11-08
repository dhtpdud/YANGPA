// @vitest-environment happy-dom
// 사용자 설정 → 음성·화면공유: 고른 값을 앱 설정(prefs.media)에 저장하고, 비우면 자동(null)으로 돌립니다.

import { act } from 'react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { useStore } from '../store/store';
import { cleanup, click, render } from '../test/dom';
import { DEFAULT_MEDIA } from '../voice/media';
import { MediaTab } from './MediaSettings';

async function choose(select: HTMLSelectElement, value: string) {
  await act(async () => {
    Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype, 'value')!.set!.call(select, value);
    select.dispatchEvent(new Event('change', { bubbles: true }));
  });
}

async function typeAndBlur(input: HTMLInputElement, value: string) {
  await act(async () => {
    Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')!.set!.call(input, value);
    input.dispatchEvent(new Event('input', { bubbles: true }));
  });
  await act(async () => {
    input.dispatchEvent(new FocusEvent('focusout', { bubbles: true }));
  });
}

afterEach(async () => {
  await cleanup();
  useStore.setState({ prefs: { ...useStore.getState().prefs, media: DEFAULT_MEDIA } });
});

describe('음성·화면공유 설정', () => {
  it('코덱·해상도·프레임·비트레이트·스위치를 저장하고 기본값으로 되돌린다', async () => {
    useStore.setState({ client: null, prefs: { ...useStore.getState().prefs, media: DEFAULT_MEDIA } });
    const { el } = await render(<MediaTab />);
    const sel = (label: string) => el.querySelector<HTMLSelectElement>(`select[aria-label="${label}"]`)!;
    await choose(sel('비디오 코덱'), 'vp9');
    await choose(sel('출력 해상도'), '720');
    await choose(sel('최대 프레임'), '30');
    await choose(sel('패킷 길이'), '60');
    await typeAndBlur(el.querySelector<HTMLInputElement>('input[aria-label="화면 최대 비트레이트"]')!, '99999');
    await typeAndBlur(el.querySelector<HTMLInputElement>('input[aria-label="음성 최대 비트레이트"]')!, '64');
    await click(el.querySelector<HTMLButtonElement>('button[role="switch"][aria-label="스테레오"]')!);
    const m = useStore.getState().prefs.media;
    expect(m.screen).toMatchObject({ codec: 'vp9', resolution: 720, fps: 30, bitrateKbps: 20_000 });
    expect(m.audio).toMatchObject({ ptime: 60, bitrateKbps: 64, stereo: true });
    // 데스크톱 앱이 아니면(신경망 코덱 전송 없음) Mimi·DAC는 고를 수 없습니다.
    const opts = [...sel('오디오 코덱').options];
    expect(opts.map((o) => [o.value, o.disabled])).toEqual([
      ['opus', false],
      ['mimi', true],
      ['dac', true],
    ]);
    expect(el.textContent).toContain('데스크톱 앱에서만');
    await typeAndBlur(el.querySelector<HTMLInputElement>('input[aria-label="음성 최대 비트레이트"]')!, '');
    expect(useStore.getState().prefs.media.audio.bitrateKbps).toBeNull();
    await click([...el.querySelectorAll('button')].find((b) => b.textContent?.includes('기본값으로'))!);
    expect(useStore.getState().prefs.media).toEqual(DEFAULT_MEDIA);
  });

  it('모델 폴더 상태를 보여 주고, 모델이 있는 신경망 코덱만 고를 수 있다', async () => {
    const neural = {
      status: async () => ({
        dir: 'C:\models',
        codecs: [
          { codec: 'mimi', missing: [] },
          { codec: 'dac', missing: ['dac/decoder_model.onnx'] },
        ],
      }),
    };
    useStore.setState({ client: { transport: { neural } } as never, prefs: { ...useStore.getState().prefs, media: DEFAULT_MEDIA } });
    const { el } = await render(<MediaTab />);
    await vi.waitFor(() => expect(el.textContent).toContain('C:\models'));
    const sel = el.querySelector<HTMLSelectElement>('select[aria-label="오디오 코덱"]')!;
    expect([...sel.options].map((o) => [o.value, o.disabled])).toEqual([
      ['opus', false],
      ['mimi', false],
      ['dac', true],
    ]);
    expect(el.textContent).toContain('빠졌거나 바뀐 파일 dac/decoder_model.onnx');
    await choose(sel, 'mimi');
    expect(useStore.getState().prefs.media.audio.codec).toBe('mimi');
    expect(el.textContent).not.toContain('모델 폴더 열기');
    useStore.setState({ client: null });
  });

  it('ffmpeg 인코더: 이 기기에서 되는 것만 고를 수 있고, 고르면 세부 설정을 저장한다', async () => {
    const screen = {
      status: async () => ({
        dir: 'C:\app\ffmpeg',
        missing: [],
        encoders: [
          { id: 'aom-av1', available: true },
          { id: 'svt-av1', available: true },
          { id: 'nvenc-av1', available: false, error: 'No capable devices found' },
          { id: 'nvenc-h264', available: true },
          { id: 'nvenc-hevc', available: true },
        ],
      }),
    };
    useStore.setState({ client: { transport: { screen } } as never, prefs: { ...useStore.getState().prefs, media: DEFAULT_MEDIA } });
    const { el } = await render(<MediaTab />);
    await vi.waitFor(() => expect(el.textContent).toContain('No capable devices found'));
    const sel = (label: string) => el.querySelector<HTMLSelectElement>(`select[aria-label="${label}"]`)!;
    expect([...sel('화면 인코더').options].map((o) => [o.value, o.disabled])).toEqual([
      ['webrtc', false],
      ['aom-av1', false],
      ['svt-av1', false],
      ['nvenc-av1', true],
      ['nvenc-h264', false],
      ['nvenc-hevc', false],
    ]);
    // 웹뷰 인코더일 때는 ffmpeg 세부 설정이 없습니다.
    expect(el.querySelector('[aria-label="ffmpeg 인코더 설정"]')).toBeNull();
    await choose(sel('화면 인코더'), 'nvenc-h264');
    await click([...el.querySelectorAll('button')].find((b) => b.textContent?.includes('화질 고정'))!);
    await choose(sel('NVENC 프리셋'), '7');
    await choose(sel('H.264 프로파일'), 'main');
    await typeAndBlur(el.querySelector<HTMLInputElement>('input[aria-label="화질 값"]')!, '20');
    await typeAndBlur(el.querySelector<HTMLInputElement>('input[aria-label="키 프레임 간격"]')!, '99');
    const sc = useStore.getState().prefs.media.screen;
    expect(sc.encoder).toBe('nvenc-h264');
    expect(sc.native).toMatchObject({ rateControl: 'cq', cq: 20, keyintSec: 10 });
    expect(sc.native.nvenc).toMatchObject({ preset: 7, profile: 'main' });
    await choose(sel('화면 인코더'), 'svt-av1');
    expect(sel('SVT-AV1 프리셋')).not.toBeNull();
    expect(sel('NVENC 프리셋')).toBeNull();
    useStore.setState({ client: null });
  });
});
