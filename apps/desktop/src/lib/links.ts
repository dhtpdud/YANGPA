// 친구·초대·보관 노드·기기 연결 링크. 데몬은 웹 초대 페이지 주소(`${WEB_BASE}#invite/…`)로 만들고 `yangpa://invite/…`도 받습니다.

/** 웹 초대 페이지(web/index.html). crates/yangpa-core/src/links.rs의 WEB_BASE와 같게 둡니다. */
export const WEB_BASE = 'https://dhtpdud.github.io/YANGPA/';

export type AppLinkKind = 'friend' | 'invite' | 'archive' | 'device';

// 데몬처럼 https는 호스트를 따지지 않습니다. 링크 내용이 스스로 검증되므로 페이지를 옮겨도 예전 링크가 통합니다.
const APP_LINK = /^(?:yangpa:\/\/|https:\/\/[^#\s]*#)(friend|invite|archive)\/[1-9A-HJ-NP-Za-km-z]+$/;

// 기기 연결 링크는 내 기기끼리만 건네므로 웹 페이지를 거치지 않습니다.
const DEVICE_LINK = /^yangpa:\/\/device\/[1-9A-HJ-NP-Za-km-z]+$/;

const CHANNEL_LINK = /^yangpa:\/\/open\/([1-9A-HJ-NP-Za-km-z]+)\/([1-9A-HJ-NP-Za-km-z]+)$/;

/** Android 알림을 눌러 온 링크의 채널(`yangpa://open/<공간>/<채널>`, src-tauri/src/notify.rs). 아니면 null. */
export function channelLink(url: string): { space: string; channel: string } | null {
  const m = CHANNEL_LINK.exec(url.trim());
  return m ? { space: m[1], channel: m[2] } : null;
}

/** 앱 링크의 종류. 앱 링크가 아니면 null. */
export function appLinkKind(url: string): AppLinkKind | null {
  const link = url.trim();
  if (DEVICE_LINK.test(link)) return 'device';
  return (APP_LINK.exec(link)?.[1] as AppLinkKind | undefined) ?? null;
}
