import { describe, expect, it } from 'vitest';
import { appLinkKind, channelLink, WEB_BASE } from './links';

describe('appLinkKind', () => {
  it('웹 초대 페이지 주소와 yangpa:// 링크를 같은 종류로 읽는다', () => {
    expect(appLinkKind(`${WEB_BASE}#invite/3yZe7d`)).toBe('invite');
    expect(appLinkKind(' yangpa://friend/3yZe7d ')).toBe('friend');
    expect(appLinkKind(' yangpa://device/3yZe7d ')).toBe('device');
    expect(appLinkKind(`${WEB_BASE}#device/3yZe7d`)).toBeNull();
    // 페이지를 옮겨도 예전 링크가 통하게 호스트는 따지지 않습니다(데몬과 같음).
    expect(appLinkKind('https://yangpa.example/#archive/3yZe7d')).toBe('archive');
  });

  it('다른 링크·잘린 링크·base58이 아닌 글자는 앱 링크가 아니다', () => {
    for (const url of [
      'https://docs.example/guide',
      'http://yangpa.example/#invite/3yZe7d',
      `${WEB_BASE}#invite/`,
      `${WEB_BASE}#invite/3yZe0d`,
      `${WEB_BASE}#space/3yZe7d`,
      'yangpa://invite/3yZe7d?x=1',
    ]) {
      expect(appLinkKind(url)).toBeNull();
    }
  });
});

describe('channelLink', () => {
  it('알림이 여는 채널만 읽고 다른 링크는 받지 않는다', () => {
    expect(channelLink('yangpa://open/3yZe7d/4zAf8e')).toEqual({ space: '3yZe7d', channel: '4zAf8e' });
    expect(channelLink('yangpa://invite/3yZe7d')).toBeNull();
    expect(channelLink('yangpa://open/3yZe7d')).toBeNull();
    expect(channelLink('yangpa://open/3yZe7d/../x')).toBeNull();
  });
});
