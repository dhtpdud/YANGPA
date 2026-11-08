// @vitest-environment happy-dom
// 캡처 감지: 셸의 감지는 하나뿐이라, 채널 화면과 그 위의 콘텐츠 뷰어가 함께 지켜볼 때 뷰어를 닫아도 꺼지면 안 됩니다.

import { invoke } from '@tauri-apps/api/core';
import { describe, expect, it, vi } from 'vitest';
import { createTauriTransport } from './client';

vi.mock('@tauri-apps/api/core', () => ({ convertFileSrc: () => 'yangpa-blob://localhost/', invoke: vi.fn(async () => true) }));
vi.mock('@tauri-apps/api/event', () => ({ listen: vi.fn(async () => () => undefined) }));

const watchCalls = (enabled: boolean) => vi.mocked(invoke).mock.calls.filter(([cmd, args]) => cmd === 'set_capture_watch' && (args as { enabled: boolean }).enabled === enabled).length;

describe('watchCapture', () => {
  it('마지막 구독이 그만둘 때만 끈다', async () => {
    const tr = await createTauriTransport();
    const stopPane = tr.watchCapture!(() => undefined);
    const stopViewer = tr.watchCapture!(() => undefined);
    await vi.waitFor(() => expect(watchCalls(true)).toBe(2));
    stopViewer();
    stopViewer();
    expect(watchCalls(false)).toBe(0);
    stopPane();
    expect(watchCalls(false)).toBe(1);
  });
});
