// 이 기기가 내주는 첨부를 지금 받고 있는 사람(transfer.rates의 uploads): 프사·이름·속도를 첨부 밑에 작게.
// 익명 채널의 첨부는 데몬이 받는 사람을 ANONYMOUS_USER로 가려 보내므로 '익명'과 기본 그림으로 그리고 프로필을 열지 않습니다.

import { Upload } from 'lucide-react';
import { useShallow } from 'zustand/react/shallow';
import { ANONYMOUS_USER } from '../api/types';
import { t } from '../i18n';
import { formatRate } from '../lib/format';
import { uploadsFor, userName } from '../lib/ratelimit';
import { useStore } from '../store/store';
import { Avatar } from './common';

/** 한 번에 보이는 줄 수(나머지는 '외 N명'). */
export const MAX_UPLOAD_ROWS = 5;

export function UploadPeers({ space, blob }: { space: string; blob: string }) {
  // 받는 사람이 없으면 늘 같은 빈 배열이라 1초마다 오는 알림에도 다시 그리지 않습니다.
  const rows = useStore(useShallow((s) => uploadsFor(s.rates?.uploads, space, blob)));
  const views = useStore((s) => s.views);
  const friends = useStore((s) => s.friends);
  const openModal = useStore((s) => s.openModal);
  if (rows.length === 0) return null;
  const shown = rows.slice(0, MAX_UPLOAD_ROWS);
  const more = rows.length - shown.length;
  const total = rows.reduce((n, r) => n + r.bps, 0);
  return (
    <div className="att-peers" aria-label={t('이 파일을 받고 있는 사람')}>
      <div className="att-peers-head" title={t('이 기기가 나눠 주는 이 파일을 지금 받고 있는 사람과 속도(최근 몇 초 평균)')}>
        <Upload size={11} /> {t('나눠 주는 중 · {n}명 · {rate}', { n: rows.length, rate: formatRate(total) })}
      </div>
      {shown.map((r, i) => {
        const anon = r.user === ANONYMOUS_USER;
        const name = userName(r.user, { views, friends }, space);
        const body = (
          <>
            <Avatar id={r.user} name={name} size={16} />
            <span className="att-peer-name">{name}</span>
            <span className="att-peer-rate">{formatRate(r.bps)}</span>
          </>
        );
        return anon ? (
          // 익명 줄은 모두 같은 자리 표시라 순서로 구분합니다.
          <div key={`anon-${i}`} className="att-peer anon">
            {body}
          </div>
        ) : (
          <button key={r.user} type="button" className="att-peer" title={t('{name} 프로필', { name })} onClick={() => openModal({ kind: 'friendProfile', user: r.user })}>
            {body}
          </button>
        );
      })}
      {more > 0 && <div className="att-peers-more">{t('외 {n}명', { n: more })}</div>}
    </div>
  );
}
