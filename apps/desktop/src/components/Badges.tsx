// 도전과제 뱃지: 프로필 이름 앞뒤·아래의 뱃지와 설정의 고르기. 이름 앞뒤에는 이모지 그대로, 모은 뱃지 목록·설정에서는
// 테두리 있는 칩으로 그립니다.

import { useEffect, useState } from 'react';
import type { Badge, BadgeView, ShownBadge } from '../api/types';
import { t } from '../i18n';
import { ALL_BADGES, BADGE_EMOJI, badgeLabel } from '../lib/badges';
import { useStore } from '../store/store';

export function BadgeIcon({ badge, dim, plain }: { badge: Badge; dim?: boolean; plain?: boolean }) {
  const label = badgeLabel(badge);
  return (
    <span className={plain ? 'badge-emoji' : dim ? 'badge-icon dim' : 'badge-icon'} title={label} role="img" aria-label={label}>
      {BADGE_EMOJI[badge]}
    </span>
  );
}

/** 그 사람의 뱃지(`badge.get`). 뱃지를 모르는 데몬이면 null. 바뀌면(`badges.updated`) 다시 읽습니다. */
export function useBadges(user: string): [BadgeView | null, (v: BadgeView) => void] {
  const client = useStore((s) => s.client);
  const tick = useStore((s) => s.badgesTick);
  const [view, setView] = useState<BadgeView | null>(null);
  useEffect(() => {
    if (!client) return;
    let stale = false;
    client
      .call('badge.get', { user })
      .then((v) => !stale && setView(v))
      .catch(() => undefined);
    return () => {
      stale = true;
    };
  }, [client, user, tick]);
  return [view, setView];
}

/** 이름 앞이나 뒤에 고른 뱃지(이모지 그대로). */
export function ShownBadges({ view, after }: { view: BadgeView | null; after: boolean }) {
  return (
    <>
      {view?.shown
        .filter((s) => s.after === after)
        .map((s) => (
          <BadgeIcon key={s.badge} badge={s.badge} plain />
        ))}
    </>
  );
}

type Place = 'none' | 'before' | 'after';

/** 설정: 모든 뱃지(모으지 않은 것은 흐리게)와, 모은 것 중 두 개까지 이름 앞뒤에 보이기. */
export function BadgePicker() {
  const me = useStore((s) => s.me)!;
  const client = useStore((s) => s.client);
  const call = useStore((s) => s.call);
  const [view, setView] = useBadges(me);
  if (!view) return null;
  const placeOf = (b: Badge): Place => {
    const s = view.shown.find((x) => x.badge === b);
    return s ? (s.after ? 'after' : 'before') : 'none';
  };
  const full = view.shown.length >= 2;
  const change = async (b: Badge, place: Place) => {
    if (!client) return;
    const rest = view.shown.filter((x) => x.badge !== b);
    const shown: ShownBadge[] = place === 'none' ? rest : [...rest, { badge: b, after: place === 'after' }];
    const r = await call(client.call('badge.set_shown', { shown }));
    if (r) setView(r);
  };
  return (
    <div className="badge-picker">
      {ALL_BADGES.map((b) => {
        const earned = view.earned.includes(b);
        const place = placeOf(b);
        return (
          <div key={b} className={earned ? 'badge-row' : 'badge-row locked'}>
            <BadgeIcon badge={b} dim={!earned} />
            <span className="grow">{badgeLabel(b)}</span>
            {earned ? (
              <select className="input small" value={place} aria-label={t('{name} 뱃지 위치', { name: badgeLabel(b) })} onChange={(e) => void change(b, e.target.value as Place)}>
                <option value="none">{t('보이지 않음')}</option>
                <option value="before" disabled={full && place === 'none'}>
                  {t('이름 앞')}
                </option>
                <option value="after" disabled={full && place === 'none'}>
                  {t('이름 뒤')}
                </option>
              </select>
            ) : (
              <span className="muted small">{t('아직 모으지 않음')}</span>
            )}
          </div>
        );
      })}
    </div>
  );
}
