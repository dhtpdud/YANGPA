// @vitest-environment happy-dom
// 휘발성 채널 캡처 감지: 스크린샷·녹화가 감지되면 채널 화면을 검게 가리고 금지 안내를 띄웁니다.

import { act } from 'react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import type { ApiClient, CaptureEvent } from '../api/client';
import { useStore } from '../store/store';
import { button, cleanup, click, render } from '../test/dom';
import { CAPTURE_BLOCKED_TEXT, CaptureShield } from './CaptureShield';

function setup() {
  let emit: (e: CaptureEvent) => void = () => undefined;
  const stop = vi.fn();
  const watchCapture = vi.fn((h: (e: CaptureEvent) => void) => {
    emit = h;
    return stop;
  });
  useStore.setState({
    client: { transport: { watchCapture } } as unknown as ApiClient,
  });
  return {
    send: (e: CaptureEvent) => act(async () => emit(e)),
    watchCapture,
    stop,
  };
}

afterEach(async () => {
  await cleanup();
  useStore.setState({ client: null });
});

describe('캡처 감지 가림막', () => {
  it('감지 전에는 아무것도 가리지 않고, 화면을 떠나면 감지를 끈다', async () => {
    const { watchCapture, stop } = setup();
    const r = await render(<CaptureShield />);
    expect(watchCapture).toHaveBeenCalledTimes(1);
    expect(r.el.querySelector('.capture-shield')).toBeNull();
    await r.unmount();
    expect(stop).toHaveBeenCalledTimes(1);
  });

  it('스크린샷이 감지되면 검게 가리고 안내를 띄우며, 확인을 누르면 다시 보인다', async () => {
    const { send } = setup();
    const { el } = await render(<CaptureShield />);
    await send({ kind: 'screenshot', tool: '캡처 도구' });
    const shield = el.querySelector('.capture-shield');
    expect(shield?.textContent).toContain(CAPTURE_BLOCKED_TEXT);
    expect(shield?.textContent).toContain('스크린샷(캡처 도구)이 감지되어');
    await click(button(el, '확인'));
    expect(el.querySelector('.capture-shield')).toBeNull();
  });

  it('앱 창에서 PrtScn을 누르면(keyup) 가린다', async () => {
    setup();
    const { el } = await render(<CaptureShield />);
    await act(async () => {
      window.dispatchEvent(new KeyboardEvent('keyup', { key: 'PrintScreen' }));
    });
    expect(el.querySelector('.capture-shield')?.textContent).toContain(CAPTURE_BLOCKED_TEXT);
  });

  it('녹화 프로그램이 켜져 있는 동안은 닫을 수 없고, 꺼지면 걷힌다', async () => {
    const { send } = setup();
    const { el } = await render(<CaptureShield />);
    await send({ kind: 'recording', tools: ['OBS Studio'] });
    const shield = el.querySelector('.capture-shield');
    expect(shield?.textContent).toContain(CAPTURE_BLOCKED_TEXT);
    expect(shield?.textContent).toContain('녹화 프로그램(OBS Studio)이 켜져 있는 동안');
    expect(shield?.querySelector('button')).toBeNull();
    await send({ kind: 'recording', tools: [] });
    expect(el.querySelector('.capture-shield')).toBeNull();
  });
});
