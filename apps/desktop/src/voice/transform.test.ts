// 변환 방식 고르기: 크로뮴의 RTCRtpScriptTransform이 협상 도중 단 변환을 때때로 적용하지 않아 createEncodedStreams를 먼저 씁니다.

import { afterEach, describe, expect, it, vi } from 'vitest';
import { detectTransformMode } from './transform';

afterEach(() => {
  vi.unstubAllGlobals();
});

describe('detectTransformMode', () => {
  it('둘 다 있으면 encoded-streams, 하나만 있으면 그것, 없으면 none', () => {
    const sender = (encoded: boolean) => ({ prototype: encoded ? { createEncodedStreams() {} } : {} });
    vi.stubGlobal('RTCRtpSender', sender(true));
    vi.stubGlobal('RTCRtpScriptTransform', class {});
    expect(detectTransformMode()).toBe('encoded-streams');
    vi.stubGlobal('RTCRtpSender', sender(false));
    expect(detectTransformMode()).toBe('script-transform');
    vi.stubGlobal('RTCRtpScriptTransform', undefined);
    expect(detectTransformMode()).toBe('none');
  });
});
