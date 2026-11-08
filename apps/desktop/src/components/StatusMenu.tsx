// 내 표시 상태 메뉴(왼쪽 아래 내 카드를 누르면): 프로필 편집, 표시 상태(온라인·자리 비움·숨기·오프라인과 기간),
// 마지막 동기화 시각 숨기기. 하위 목록은 옆으로 펼치되 같은 팝오버 안에 그려 바깥 클릭으로 닫히지 않게 합니다.

import { Check, ChevronRight, Clock, Pencil } from 'lucide-react';
import { useState } from 'react';
import type { PresenceMode, PresenceView } from '../api/types';
import { t } from '../i18n';
import { formatFull } from '../lib/format';
import { cx } from '../lib/misc';
import { useStore } from '../store/store';
import { Popover } from './common';
import { MenuButton, MenuSep } from './menus';

export const PRESENCE_LABEL: Record<PresenceMode, string> = {
  online: t('온라인'),
  away: t('자리 비움'),
  invisible: t('숨기'),
  offline: t('오프라인'),
};

const PRESENCE_SUB: Partial<Record<PresenceMode, string>> = {
  invisible: t('동기화·대화는 그대로, 다른 사람에게는 오프라인으로 보여요'),
  offline: t('Tor 연결과 동기화를 끊고 이 기기에서만 써요'),
};

const MODES: PresenceMode[] = ['online', 'away', 'invisible', 'offline'];

/** 기간(초). null은 무기한. */
const DURATIONS: Array<[number | null, string]> = [
  [15 * 60, t('15분')],
  [3600, t('1시간')],
  [8 * 3600, t('8시간')],
  [24 * 3600, t('24시간')],
  [3 * 86_400, t('3일 동안')],
  [null, t('무기한')],
];

export type Dot = 'on' | 'away' | 'hidden' | 'off';

/** 내 점: 자리 비움(직접 고름·입력 없음)은 노랑, 숨기는 빈 고리, 오프라인은 회색. */
export function myDot(p: PresenceView | null): Dot {
  if (!p || p.mode === 'online') return p?.idle ? 'away' : 'on';
  return p.mode === 'away' ? 'away' : p.mode === 'invisible' ? 'hidden' : 'off';
}

/** 지금 상태 문구(온라인인데 입력이 없으면 자리 비움). */
export function myStatusText(p: PresenceView | null): string {
  return p?.mode === 'online' && p.idle ? t('자리 비움') : PRESENCE_LABEL[p?.mode ?? 'online'];
}

const modeDot = (m: PresenceMode): Dot => (m === 'online' ? 'on' : m === 'away' ? 'away' : m === 'invisible' ? 'hidden' : 'off');

export function StatusDot({ dot }: { dot: Dot }) {
  return <span className={cx('status-dot', dot)} aria-hidden />;
}

export function StatusMenu({ anchor, onClose }: { anchor: DOMRect; onClose: () => void }) {
  const presence = useStore((s) => s.presence);
  const setPresence = useStore((s) => s.setPresence);
  const openModal = useStore((s) => s.openModal);
  const toast = useStore((s) => s.toast);
  const leaveVoice = useStore((s) => s.leaveVoice);
  const [statusOpen, setStatusOpen] = useState(false);
  const [durOpen, setDurOpen] = useState<PresenceMode | null>(null);
  const mode = presence?.mode ?? 'online';

  const choose = async (m: PresenceMode, secs: number | null) => {
    onClose();
    // 오프라인으로 들어가거나 나오면 데몬이 망을 끄거나 켜고 다시 뜹니다(잠시 연결 중으로 보임).
    if ((m === 'offline') !== (mode === 'offline')) {
      toast('info', m === 'offline' ? t('Tor 연결과 동기화를 끊고 오프라인으로 다시 시작합니다') : t('다시 연결합니다'));
    }
    // 통화의 직접 연결(WebRTC)은 웹뷰에 있어 데몬을 다시 띄워도 남으므로 먼저 나옵니다.
    if (m === 'offline' && useStore.getState().voice) await leaveVoice();
    await setPresence({ mode: m, ...(secs !== null && { duration_secs: secs }) });
  };

  return (
    <Popover anchor={anchor} onClose={onClose} placement="top-start" className="menu wide status-menu">
      <MenuButton
        icon={<Pencil size={15} />}
        label={t('프로필 편집')}
        onClick={() => {
          onClose();
          openModal({ kind: 'userSettings', tab: 'profile' });
        }}
      />
      <MenuSep />
      <div
        className="flyout-host"
        onMouseEnter={() => setStatusOpen(true)}
        onMouseLeave={() => {
          setStatusOpen(false);
          setDurOpen(null);
        }}
      >
        <button type="button" className="menu-item" aria-haspopup="menu" aria-expanded={statusOpen} onClick={() => setStatusOpen(!statusOpen)}>
          <StatusDot dot={myDot(presence)} />
          <span className="menu-label">
            {myStatusText(presence)}
            {presence?.until !== undefined && <span className="menu-sub">{t('{time}까지', { time: formatFull(presence.until) })}</span>}
          </span>
          <ChevronRight size={14} />
        </button>
        {statusOpen && (
          <div className="flyout" role="menu" aria-label={t('표시 상태')}>
            <div className="flyout-body">
              {MODES.map((m) => (
                <div key={m} className="flyout-host" onMouseEnter={() => setDurOpen(m === 'online' ? null : m)}>
                  <button
                    type="button"
                    role="menuitemradio"
                    aria-checked={mode === m}
                    className="menu-item"
                    onClick={() => (m === 'online' ? void choose(m, null) : setDurOpen(durOpen === m ? null : m))}
                  >
                    <StatusDot dot={modeDot(m)} />
                    <span className="menu-label">
                      {PRESENCE_LABEL[m]}
                      {PRESENCE_SUB[m] && <span className="menu-sub">{PRESENCE_SUB[m]}</span>}
                    </span>
                    {mode === m && <Check size={14} />}
                    {m !== 'online' && <ChevronRight size={14} />}
                  </button>
                  {durOpen === m && (
                    <div className="flyout" role="menu" aria-label={t('기간')}>
                      <div className="flyout-body">
                        {DURATIONS.map(([secs, label]) => (
                          <button key={label} type="button" role="menuitem" className="menu-item" onClick={() => void choose(m, secs)}>
                            {label}
                          </button>
                        ))}
                      </div>
                    </div>
                  )}
                </div>
              ))}
            </div>
          </div>
        )}
      </div>
      <MenuSep />
      <MenuButton
        icon={<Clock size={15} />}
        label={t('마지막 동기화 시각 숨기기')}
        sub={t('다른 사람 프로필 카드에 나와 마지막으로 동기화한 시각이 보이지 않아요')}
        checked={!!presence?.hide_last_seen}
        onClick={() => void setPresence({ hide_last_seen: !presence?.hide_last_seen })}
      />
    </Popover>
  );
}
