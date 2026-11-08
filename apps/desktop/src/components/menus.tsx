// 오른쪽 클릭 메뉴 조각: 메뉴 줄, 구분선, 크루·채널 알림(끄기·수준), 사람 메뉴(프로필·신뢰 스위치·ID 복사).
// 하위 메뉴는 팝오버를 겹치지 않고 같은 메뉴 안에 펼칩니다(겹친 팝오버는 바깥 클릭으로 부모가 먼저 닫힘).

import { Bell, BellOff, Check, ChevronRight, Handshake, IdCard, SlidersHorizontal, UserRound } from 'lucide-react';
import { useEffect, useState, type MouseEvent, type ReactNode } from 'react';
import { create } from 'zustand';
import type { ChannelId, SpaceId, UserId } from '../api/types';
import { t } from '../i18n';
import { isAnonymousUser } from '../lib/anon';
import { cx } from '../lib/misc';
import { LEVEL_LABELS, MUTE_CHOICES, muteLabel, muteUntil, ruleKey, withRule, type NotifyRule } from '../lib/notify';
import { canTrustUser } from '../lib/trust';
import { useStore, type NotifyCrew } from '../store/store';
import { Popover, copyText } from './common';

export function MenuButton({
  icon,
  label,
  sub,
  onClick,
  danger,
  disabled,
  checked,
  expanded,
}: {
  icon?: ReactNode;
  label: string;
  /** 줄 아래 작은 설명(지금 값). */
  sub?: string;
  onClick: () => void;
  danger?: boolean;
  disabled?: boolean;
  /** 켜고 끄는 줄이면 지금 값(스위치 모양). */
  checked?: boolean;
  /** 펼치는 줄이면 펼쳤는지. */
  expanded?: boolean;
}) {
  return (
    <button
      type="button"
      className={cx('menu-item', danger && 'danger', disabled && 'disabled')}
      disabled={disabled}
      role={checked === undefined ? undefined : 'menuitemcheckbox'}
      aria-checked={checked}
      aria-expanded={expanded}
      onClick={onClick}
    >
      {icon}
      <span className="menu-label">
        {label}
        {sub && <span className="menu-sub">{sub}</span>}
      </span>
      {expanded !== undefined && <ChevronRight size={14} className={cx('menu-caret', expanded && 'open')} />}
      {checked !== undefined && (
        <span className={cx('toggle', 'mini', checked && 'on')} aria-hidden>
          <span className="knob" />
        </span>
      )}
    </button>
  );
}

export const MenuSep = () => <div className="menu-sep" role="separator" />;

/** ID를 복사하고 알립니다. */
export async function copyId(id: string, what: string) {
  if (await copyText(id)) useStore.getState().toast('success', t('{what} ID를 복사했습니다', { what }));
}

/**
 * 크루(채널을 주면 그 채널)의 알림 끄기와 알림 수준. 이 기기 설정(`notifyRules`)이고 OS 알림에만 씁니다.
 * 휘발성 채널은 ID를 디스크에 남기지 않으므로 부르지 않습니다(크루 규칙은 그 채널에도 적용).
 */
export function NotifyItems({ space, channel, onClose }: { space: SpaceId; channel?: ChannelId; onClose: () => void }) {
  const rules = useStore((s) => s.prefs.notifyRules);
  const base = useStore((s) => s.prefs.notifyCrew);
  const setPrefs = useStore((s) => s.setPrefs);
  const [open, setOpen] = useState<'mute' | 'level' | null>(null);
  const key = ruleKey(space, channel);
  const rule = rules[key] ?? {};
  const now = Date.now();
  const muted = (rule.muteUntil ?? 0) > now;
  // 이 줄이 수준을 고르지 않으면 따르는 값.
  const inherited: NotifyCrew = (channel && rules[ruleKey(space)]?.level) || base;
  const set = (patch: NotifyRule) => {
    setPrefs({ notifyRules: withRule(useStore.getState().prefs.notifyRules, key, patch) });
    onClose();
  };
  const toggle = (which: 'mute' | 'level') => setOpen(open === which ? null : which);
  const levels: Array<NotifyCrew | undefined> = [undefined, 'all', 'mentions', 'off'];
  const inheritText = channel
    ? t('크루 설정 따르기 ({level})', { level: LEVEL_LABELS[inherited] })
    : t('기본값 따르기 ({level})', { level: LEVEL_LABELS[inherited] });
  return (
    <>
      {muted ? (
        <MenuButton icon={<Bell size={15} />} label={channel ? t('채널 알림 켜기') : t('크루 알림 켜기')} sub={muteLabel(rule.muteUntil ?? 0, now)} onClick={() => set({ muteUntil: undefined })} />
      ) : (
        <MenuButton icon={<BellOff size={15} />} label={channel ? t('채널 알림 끄기') : t('크루 알림 끄기')} expanded={open === 'mute'} onClick={() => toggle('mute')} />
      )}
      {open === 'mute' && (
        <div className="menu-group">
          {MUTE_CHOICES.map(([ms, label]) => (
            <button key={ms} type="button" className="menu-item" onClick={() => set({ muteUntil: muteUntil(ms) })}>
              {label}
            </button>
          ))}
        </div>
      )}
      <MenuButton
        icon={<SlidersHorizontal size={15} />}
        label={t('알림 설정')}
        sub={LEVEL_LABELS[rule.level ?? inherited]}
        expanded={open === 'level'}
        onClick={() => toggle('level')}
      />
      {open === 'level' && (
        <div className="menu-group" role="radiogroup" aria-label={channel ? t('채널 알림 설정') : t('크루 알림 설정')}>
          {levels.map((l) => (
            <button key={l ?? 'inherit'} type="button" role="radio" aria-checked={rule.level === l} className="menu-item" onClick={() => set({ level: l })}>
              <span className="menu-label">{l ? LEVEL_LABELS[l] : inheritText}</span>
              {rule.level === l && <Check size={14} />}
            </button>
          ))}
        </div>
      )}
    </>
  );
}

// ───────────── 사람 메뉴 ─────────────

interface PersonTarget {
  at: DOMRect;
  user: UserId;
  name: string;
}

/** 목록 줄마다 메뉴를 두지 않고 앱에 하나만 띄웁니다(큰 크루의 멤버 목록에서도 줄마다 구독하지 않게). */
const usePersonMenu = create<{ open: PersonTarget | null }>(() => ({ open: null }));

/** 사람을 오른쪽 클릭하면 그 자리에 사람 메뉴를 엽니다. 익명 자리는 누구인지 모르므로 열지 않습니다. */
export function openPersonMenu(e: MouseEvent, user: UserId, name: string) {
  if (isAnonymousUser(user)) return;
  e.preventDefault();
  usePersonMenu.setState({ open: { at: new DOMRect(e.clientX, e.clientY, 0, 0), user, name } });
}

export function PersonMenu() {
  const open = usePersonMenu((s) => s.open);
  if (!open) return null;
  return <PersonMenuBody key={`${open.user}/${open.at.x}/${open.at.y}`} {...open} onClose={() => usePersonMenu.setState({ open: null })} />;
}

/** 프로필, 신뢰 스위치(이 사람에게 이 기기의 IP를 보여도 되는지), 사용자 ID 복사. */
function PersonMenuBody({ at, user, name, onClose }: PersonTarget & { onClose: () => void }) {
  const client = useStore((s) => s.client);
  const openModal = useStore((s) => s.openModal);
  const setTrust = useStore((s) => s.setTrust);
  // 연 순간에 한 번만 봅니다(멤버가 많은 크루마다 다 훑으므로 바뀔 때마다 다시 세지 않게).
  const [eligible] = useState(() => {
    const s = useStore.getState();
    return canTrustUser(s.me, user, s.views, s.friends);
  });
  const isMe = useStore((s) => s.me === user);
  const [trusted, setTrusted] = useState<boolean | null>(null);
  useEffect(() => {
    if (!client || isMe) return;
    let stale = false;
    // 신뢰 스위치 전 데몬이거나 실패하면 줄을 숨긴 채로 둡니다.
    client
      .call('trust.get', { user })
      .then((v) => !stale && setTrusted(v.trusted))
      .catch(() => undefined);
    return () => {
      stale = true;
    };
  }, [client, user, isMe]);
  return (
    <Popover anchor={at} onClose={onClose} placement="bottom-start" className="menu wide">
      <MenuButton
        icon={<UserRound size={15} />}
        label={t('프로필 보기')}
        onClick={() => {
          onClose();
          openModal({ kind: 'friendProfile', user });
        }}
      />
      {trusted !== null && (eligible || trusted) && (
        <MenuButton
          icon={<Handshake size={15} />}
          label={t('신뢰')}
          sub={trusted ? t('이 사람에게 이 기기의 IP가 보임') : t('꺼짐: 이 기기의 IP를 숨김')}
          checked={trusted}
          onClick={() => {
            // 켤 때 뜨는 경고 창과 Esc를 함께 받지 않게 메뉴부터 닫습니다.
            onClose();
            void setTrust({ type: 'user', user, name }, !trusted);
          }}
        />
      )}
      <MenuSep />
      <MenuButton
        icon={<IdCard size={15} />}
        label={t('사용자 ID 복사하기')}
        onClick={() => {
          onClose();
          void copyId(user, t('사용자'));
        }}
      />
    </Popover>
  );
}
