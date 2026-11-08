// 신뢰 스위치(사람·크루마다 이 기기의 IP를 보여도 되는지) 화면 도우미.
// 앱은 이 기기나 상대의 IP 주소를 어디에도 보이지 않고, 연결 방식만 말로 알립니다.

import type { ConnectionMode, FriendView, SpaceId, SpaceView, UserId } from '../api/types';
import { t } from '../i18n';

/** 신뢰 스위치를 켜거나 끌 대상. `name`은 경고 창·알림 문구에만 씁니다. */
export type TrustTarget = { type: 'user'; user: UserId; name: string } | { type: 'space'; space: SpaceId; name: string };

const CONNECTION_LABELS: Record<ConnectionMode, string> = {
  p2p: t('P2P 직접 연결(서로 IP가 보임)'),
  one_way_me_exposed: t('단방향: 상대에게 내 IP가 보임(상대는 숨음)'),
  one_way_peer_exposed: t('단방향: 내게 상대 IP가 보임(나는 숨음)'),
  tor: t('Tor(서로 숨음)'),
  offline: t('오프라인'),
};

/** 연결 방식을 화면 문구로. 새 데몬이 모르는 값을 보내도 그대로 드러내지 않습니다. */
export function connectionLabel(mode: string): string {
  return Object.prototype.hasOwnProperty.call(CONNECTION_LABELS, mode) ? CONNECTION_LABELS[mode as ConnectionMode] : t('알 수 없음');
}

/**
 * 이 사람에게 신뢰를 켤 수 있는지(`trust.set_user`가 받아들이는지): 내가 아니고, 나와 함께 멤버인 공간이 있어야 합니다.
 * 데몬처럼 만료된 연락처의 1대1 공간(읽기만 남음)은 세지 않습니다.
 */
export function canTrustUser(me: UserId | null, user: UserId, views: Record<SpaceId, SpaceView>, friends: FriendView[]): boolean {
  if (!me || user === me) return false;
  const closed = new Set(friends.filter((f) => f.kind === 'expired').map((f) => f.space));
  for (const v of Object.values(views)) {
    if (closed.has(v.space)) continue;
    const members = v.members ?? [];
    if (members.some((m) => m.user === me) && members.some((m) => m.user === user)) return true;
  }
  // 보기를 아직 읽지 못했어도 친구·임시 연락처면 1대1 공간을 함께 씁니다.
  return friends.some((f) => f.user === user && f.kind !== 'expired');
}

/** 경고 창 첫 줄: 누가 이 기기의 IP를 보게 되는지. */
export function trustWarningLead(target: TrustTarget): string {
  return target.type === 'user'
    ? t('{name} 님을 신뢰하면 그 사람이 이 기기의 IP 주소를 볼 수 있습니다.', { name: target.name })
    : t('「{name}」 크루를 신뢰하면 이 크루의 멤버 모두가 이 기기의 IP 주소를 볼 수 있습니다.', { name: target.name });
}

/** IP로 무엇을 알 수 있는지. 앱이 보이지 않는다고 안전한 것은 아님을 숨기지 않습니다. */
export const TRUST_WARNING_DETAIL = t(
  'IP 주소로는 대략의 위치와 인터넷 회사(ISP)를 알 수 있습니다. 양파 앱은 IP 주소를 어디에도 보여 주지 않지만, 상대가 다른 도구를 쓰면 알아낼 수 있습니다.',
);

/** 경고 창의 나머지 안내. */
export function trustWarningPoints(target: TrustTarget): string[] {
  return [
    t('상대도 나를 신뢰하면 P2P로 직접 연결하고, 나만 신뢰하면 상대가 Tor 출구를 거쳐 숨은 채로 이 기기에 연결합니다(단방향).'),
    t('서로 신뢰하면 일반 채널의 통화도 Tor 대신 WebRTC로 직접 연결합니다. 이때 상대와 STUN 서버(설정의 네트워크·Tor)에 IP가 보입니다.'),
    ...(target.type === 'space' ? [t('나중에 이 크루에 들어오는 멤버에게도 보입니다.')] : []),
    t('이 기기에만 적용됩니다. 다른 기기는 그 기기에서 따로 켜야 합니다.'),
    t('언제든 끌 수 있지만, 이미 보인 IP 주소는 다시 숨길 수 없습니다.'),
  ];
}
