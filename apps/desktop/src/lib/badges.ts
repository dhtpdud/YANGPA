// 도전과제 뱃지의 이모지와 이름(데몬 yangpa-core `badge::Badge`와 같은 순서).

import type { Badge } from '../api/types';
import { t } from '../i18n';

/** 뱃지마다 이모지(번호 순). */
export const BADGE_EMOJI: Record<Badge, string> = {
  founder: '🧅',
  support3: '☕',
  support14: '🍕',
  support50: '💎',
  friends5: '👋',
  friends10: '🤝',
  friends20: '🎉',
  friends50: '🌟',
  friends100: '👑',
  crew10: '⛺',
  crew100: '🏠',
  crew1000: '🏰',
  crew10000: '🌆',
  seed1gb: '🌱',
  seed10gb: '🌿',
  seed100gb: '🌳',
  seed1tb: '🏔️',
  seed10tb: '🌍',
  seed100tb: '🪐',
  seed1pb: '🌌',
  voice1h: '📞',
  voice10h: '🎧',
  voice100h: '🎙️',
  voice1000h: '📻',
  voice10000h: '📡',
  screen1h: '💻',
  screen10h: '🖥️',
  screen100h: '📺',
  screen1000h: '🎬',
  screen10000h: '🏟️',
};

export const ALL_BADGES = Object.keys(BADGE_EMOJI) as Badge[];

/** 뱃지 이름(툴팁·설정). */
export function badgeLabel(b: Badge): string {
  if (b === 'founder') return t('원년멤버');
  const m = /^(support|friends|crew|seed|voice|screen)(\d+)(gb|tb|pb|h)?$/.exec(b);
  if (!m) return b;
  const n = Number(m[2]);
  switch (m[1]) {
    case 'support':
      return t('${n} 이상 후원', { n });
    case 'friends':
      return t('친구 {n}명 이상', { n });
    case 'crew':
      return t('멤버 {n}명 이상 크루의 소유자', { n: n.toLocaleString() });
    case 'voice':
      return t('음성 통화 {n}시간 이상', { n: n.toLocaleString() });
    case 'screen':
      return t('화면 공유 {n}시간 이상', { n: n.toLocaleString() });
    default:
      return t('시딩 {size} 이상', { size: `${n}${m[3].toUpperCase()}` });
  }
}
