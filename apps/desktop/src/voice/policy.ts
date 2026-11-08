// 통화가 어느 길로 가는지(화면 안내용). WebRTC는 참여자에게 IP가 보이므로, Tor로 도는 기기의 일반 채널(P2P 전용이 아닌
// 채널) 통화는 소리를 Tor로 나르고(데몬이 `topology: 'tor'`로 정함, tor.ts), 서로 신뢰한 상대와만 WebRTC로 잇습니다
// (참가자의 `direct`). 참가한 P2P 전용 채널은 직접 연결(WebRTC)입니다. 화면의 안내와 store가 같은 판단을 쓰도록 한곳에 둡니다.

import type { NetStatus, Topology, VoiceParticipant } from '../api/types';
import { t } from '../i18n';

/** Tor 통화 안내. */
export const TOR_CALL_NOTE = t(
  'Tor 통화: Tor로 나릅니다(참여자에게 IP가 보이지 않음). 말이 0.5~1.5초쯤 늦게 닿고, 화면공유는 비상용(낮은 화질·초당 2프레임)입니다. 서로 신뢰한 상대와는 WebRTC로 바로 이어(서로 IP가 보임) 늦지 않고 화면도 제 화질입니다.',
);

/**
 * 화면공유 전에 Tor 비상용 경고를 받을지: Tor 통화에 Tor로 잇는 상대가 있거나, 아직 서로 신뢰한 상대가 없음. 모두 서로
 * 신뢰한 상대면 메시 통화처럼 공유합니다.
 */
export function needsTorScreen(voice: { topology: Topology; participants: VoiceParticipant[] }): boolean {
  if (voice.topology !== 'tor') return false;
  const others = voice.participants.filter((p) => !p.mine);
  return others.length === 0 || others.some((p) => !p.direct);
}

/**
 * 이 채널의 통화가 Tor 통화인지. `direct`는 이 기기가 참가한 P2P 전용 채널인지. 직접 연결로 띄운 데몬(상시 시더·시험)은
 * WebRTC로 통화합니다.
 */
export function isTorCall(net: Pick<NetStatus, 'mode'> | null, direct = false): boolean {
  return net?.mode === 'tor' && !direct;
}

/** 상대와 이어진 상태인지: WebRTC 'connected', 또는 Tor 통화('tor', WebRTC 연결 없이 데몬의 Tor 연결로 감). */
export function isLinkUp(state: string | undefined): boolean {
  return state === 'connected' || state === 'tor';
}

/**
 * 화면에 '연결됨'으로 보일지. 'tor'는 상대가 들어오자마자 붙는 값(WebRTC 없이 Tor로 보낸다는 뜻)이라, 이번 참가 동안 그
 * 상대의 소리가 한 번이라도 도착해야(`heard`) 연결된 것으로 봅니다. 음소거한 Tor 상대는 보내는 소리가 없어 확인할 수
 * 없으므로 링크만 보고, 화면은 '연결 확인 전'을 따로 붙입니다. 신호를 주고받기 전(항목 없음)은 연결 중입니다.
 */
export function isPeerConnected(v: { connection: Record<string, string>; heard?: string[] }, p: { device: string; muted: boolean }): boolean {
  const state = v.connection[p.device];
  if (state === 'tor') return p.muted || !!v.heard?.includes(p.device);
  return state === 'connected';
}
