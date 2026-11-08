// 왼쪽 끝 레일: 홈(친구·1대1), 안 읽은 1대1, 크루 목록, 크루 만들기·참가, 지금 전체 속도와 네트워크 상태.

import { Compass, Library, Plus, Shield } from 'lucide-react';
import { useState } from 'react';
import type { SpaceId, SpaceSummary } from '../api/types';
import { t } from '../i18n';
import { formatRate } from '../lib/format';
import { colorFromId, cx, initials } from '../lib/misc';
import { isMuted } from '../lib/notify';
import { useStore } from '../store/store';
import { useImageBlobSrc } from './common';
import { openPersonMenu } from './menus';
import { SpaceMenu } from './Sidebar';

/** 크루 메뉴는 열린 동안만 그 크루 보기를 구독합니다(보기를 아직 못 읽었으면 띄우지 않음). */
function RailMenu({ space, anchor, onClose }: { space: SpaceId; anchor: DOMRect; onClose: () => void }) {
  const view = useStore((s) => s.views[space]);
  return view ? <SpaceMenu space={view} anchor={anchor} onClose={onClose} /> : null;
}

/** 크루는 오른쪽 클릭으로 크루 메뉴를, 1대1은 상대 사람 메뉴를 엽니다. */
function RailItem({ s, active, label, peer }: { s: SpaceSummary; active: boolean; label: string; peer?: string }) {
  const selectSpace = useStore((st) => st.selectSpace);
  const icon = useImageBlobSrc(s.space, s.kind === 'crew' ? s.icon : null);
  // 알림을 끈 크루는 흐리게 그리고 안 읽음 표시를 하지 않습니다(멘션 수는 그대로).
  const muted = useStore((st) => isMuted(st.prefs.notifyRules, s.space));
  const [menu, setMenu] = useState<DOMRect | null>(null);
  return (
    <div className={cx('rail-item', active && 'active', s.unread > 0 && !muted && 'unread', muted && 'muted')}>
      <span className="rail-pill" />
      <button
        type="button"
        className={cx('rail-btn', s.kind === 'space' && 'dm', s.dormant && 'dormant')}
        title={label + (s.dormant ? ' ' + t('(휴면)') : '') + (muted ? ' ' + t('(알림 끔)') : '')}
        style={active ? undefined : { ['--hue' as string]: colorFromId(s.space) }}
        onClick={() => void selectSpace(s.space)}
        onContextMenu={(e) => {
          if (s.kind !== 'crew') return peer && openPersonMenu(e, peer, label);
          e.preventDefault();
          setMenu(new DOMRect(e.clientX, e.clientY, 0, 0));
        }}
      >
        {icon.src ? <img className="rail-icon" src={icon.src} alt="" decoding="async" draggable={false} onError={icon.onError} /> : initials(label)}
      </button>
      {s.mention > 0 && <span className="rail-badge">{s.mention > 99 ? '99+' : s.mention}</span>}
      {menu && <RailMenu space={s.space} anchor={menu} onClose={() => setMenu(null)} />}
    </div>
  );
}

/**
 * 지금 전체 올리기(↑ 초록)·받기(↓ 빨강) 속도. 데몬이 1초마다(바뀌었을 때만) 알려 주는 최근 몇 초 평균이고,
 * 이 컴포넌트만 다시 그려지게 숫자 둘만 구독합니다. 속도를 모르는 옛 데몬이면 그리지 않습니다.
 */
export function TransferSpeed() {
  const up = useStore((s) => s.rates?.up_bps);
  const down = useStore((s) => s.rates?.down_bps);
  if (up === undefined || down === undefined) return null;
  const upText = formatRate(up);
  const downText = formatRate(down);
  return (
    <div
      className="rail-rates"
      role="status"
      aria-label={t('올리기 {up}, 받기 {down}', { up: upText, down: downText })}
      title={[t('지금 전체 속도(최근 몇 초 평균, 통화 제외)'), t('↑ 올리기 {up}', { up: upText }), t('↓ 받기 {down}', { down: downText })].join('\n')}
    >
      <span className="rate-up">↑ {upText}</span>
      <span className="rate-down">↓ {downText}</span>
    </div>
  );
}

export function Rail() {
  const spaces = useStore((s) => s.spaces);
  const selected = useStore((s) => s.selected);
  const requests = useStore((s) => s.requests);
  const friends = useStore((s) => s.friends);
  const net = useStore((s) => s.net);
  const selectHome = useStore((s) => s.selectHome);
  const selectLibrary = useStore((s) => s.selectLibrary);
  const library = useStore((s) => s.library);
  const openModal = useStore((s) => s.openModal);
  const home = selected.space === 'home' && !library;
  const dms = spaces.filter((s) => s.kind === 'space' && (s.unread > 0 || s.space === selected.space));
  const crews = spaces.filter((s) => s.kind === 'crew');
  const dmPeer = (s: SpaceSummary) => friends.find((f) => f.space === s.space);
  const homeBadge = requests.length + spaces.filter((s) => s.kind === 'space').reduce((n, s) => n + s.mention, 0);
  // 회로를 만드는 동안은 진행률을(net.status 알림으로 옴), 다 되면 Tor 표시를 보입니다.
  const torProgress = net?.mode === 'tor' && net.bootstrap_progress !== undefined && net.bootstrap_progress < 100 ? net.bootstrap_progress : null;

  return (
    <nav className="rail" aria-label={t('공간')}>
      <div className={cx('rail-item', home && 'active')}>
        <span className="rail-pill" />
        <button type="button" className="rail-btn home" title={t('홈: 친구와 1대1 대화')} onClick={selectHome}>
          <svg viewBox="0 0 32 32" width="28" height="28" aria-hidden>
            <path d="M16 3c1.5 3 5 4.5 5 4.5C26 10 28 14 28 18.5 28 25 22.6 29 16 29S4 25 4 18.5C4 14 6 10 11 7.5c0 0 3.5-1.5 5-4.5z" fill="currentColor" opacity=".9" />
            <path d="M16 9c-3 2.5-5 5.6-5 9.5 0 3.6 2.2 6.5 5 7.5 2.8-1 5-3.9 5-7.5 0-3.9-2-7-5-9.5z" fill="var(--rail-bg)" opacity=".55" />
            <path d="M16 14c-1.2 1.2-2 2.6-2 4.5s.8 3.4 2 4.2c1.2-.8 2-2.3 2-4.2S17.2 15.2 16 14z" fill="currentColor" />
          </svg>
        </button>
        {homeBadge > 0 && <span className="rail-badge">{homeBadge}</span>}
      </div>
      <div className={cx('rail-item', library && 'active')}>
        <span className="rail-pill" />
        <button type="button" className="rail-btn library" title={t('라이브러리: 주고받은 파일')} aria-pressed={library} onClick={selectLibrary}>
          <Library size={22} />
        </button>
      </div>
      {dms.map((s) => (
        <RailItem key={s.space} s={s} active={selected.space === s.space} label={dmPeer(s)?.profile.name ?? s.name} peer={dmPeer(s)?.user} />
      ))}
      <div className="rail-sep" />
      <div className="rail-scroll">
        {crews.map((s) => (
          <RailItem key={s.space} s={s} active={selected.space === s.space} label={s.name} />
        ))}
        <div className="rail-item">
          <button type="button" className="rail-btn action" title={t('크루 만들기')} onClick={() => openModal({ kind: 'createCrew' })}>
            <Plus size={22} />
          </button>
        </div>
        <div className="rail-item">
          <button type="button" className="rail-btn action" title={t('초대 링크로 크루 참가')} onClick={() => openModal({ kind: 'joinCrew' })}>
            <Compass size={22} />
          </button>
        </div>
      </div>
      <div className="rail-foot">
        <TransferSpeed />
        {net?.mode === 'tor' ? (
          <button
            type="button"
            className="rail-net tor"
            title={torProgress !== null ? t('Tor 연결 중 {n}%', { n: torProgress }) : t('모든 연결은 Tor로 오갑니다') + (net?.onion_addr ? ` · ${net.onion_addr}` : '')}
            onClick={() => openModal({ kind: 'userSettings', tab: 'network' })}
          >
            <Shield size={16} />
            {torProgress !== null ? `${torProgress}%` : 'Tor'}
          </button>
        ) : (
          <button
            type="button"
            className={cx('rail-net', (net?.connections ?? 0) > 0 ? 'ok' : 'bad')}
            // 직접 연결은 데몬을 그렇게 띄웠을 때뿐입니다(상시 시더·중계 서버·시험).
            title={net ? t('직접 연결(Tor 없음) · 연결 {n}개 · NAT {nat}', { n: net.connections, nat: net.nat }) : t('네트워크 상태를 아직 모릅니다')}
            onClick={() => openModal({ kind: 'userSettings', tab: 'network' })}
          >
            <span className="net-dot" />
            {net?.connections ?? 0}
          </button>
        )}
      </div>
    </nav>
  );
}
