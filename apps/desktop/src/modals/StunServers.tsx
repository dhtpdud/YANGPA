// 네트워크 탭의 STUN 서버 목록(`net.stun_servers`). P2P 연결이 공유기 밖 주소를 알아낼 때만 쓰고 Tor 연결에는 쓰지
// 않습니다. 입력 칸 하나에 한 줄에 하나씩 적고 저장하거나 기본 목록으로 되돌립니다(`net.set_stun_servers`, 사람만 바꿈).
// 직접 연결로 띄운 데몬의 경고도 여기 둡니다.

import { AlertTriangle, RotateCcw } from 'lucide-react';
import { useCallback, useEffect, useState } from 'react';
import type { StunServers } from '../api/types';
import { t } from '../i18n';
import { useStore } from '../store/store';
import { Field } from './Modal';

/** 데몬이 Tor 없이 직접 연결로 떠 있을 때(상시 시더·중계 서버·시험용으로 띄운 경우만) 보이는 경고. */
export const DIRECT_MODE_WARNING = t(
  '이 데몬은 Tor 없이 직접 연결로 떠 있어(상시 시더·중계 서버·시험용) 연결되는 상대에게 내 IP 주소가 보입니다. 앱이 띄운 데몬은 늘 Tor로 돕니다.',
);

export function DirectModeWarning() {
  return (
    <div className="warn-box">
      <AlertTriangle size={16} />
      <div>{DIRECT_MODE_WARNING}</div>
    </div>
  );
}

/** STUN 목록 설명 한 줄. */
export const STUN_HELP = t('P2P 연결(내 기기끼리·신뢰한 상대·P2P 전용 채널)이 공유기 밖 주소를 알아낼 때만 씁니다. Tor 연결에는 쓰지 않습니다.');

/** 데몬 한도(`stun::MAX_STUN_SERVERS`)와 같게 둡니다. */
export const MAX_STUN_SERVERS = 8;

/** 입력 칸 글을 서버 목록으로: 줄바꿈·쉼표로 나누고 빈 줄은 뺍니다(형식 검사는 데몬이 함). */
export function parseServerList(text: string): string[] {
  return text
    .split(/[\n,]/)
    .map((x) => x.trim())
    .filter(Boolean);
}

export function StunServersSection() {
  const client = useStore((s) => s.client);
  const call = useStore((s) => s.call);
  const [list, setList] = useState<StunServers | null>(null);
  const [draft, setDraft] = useState('');
  const refresh = useCallback(async () => {
    if (!client) return;
    const l = await client.call('net.stun_servers', {}).catch(() => null);
    setList(l);
    if (l) setDraft(l.servers.join('\n'));
  }, [client]);
  useEffect(() => {
    void refresh();
  }, [refresh]);

  // 형식 검사(stun: 떼기, 기본 포트, TURN·비밀번호 거절)는 데몬이 하고, 거절 이유는 알림으로 보입니다.
  const save = async (servers: string[] | null, ok: string) => {
    if (!client) return;
    await call(client.call('net.set_stun_servers', { servers }), { ok });
    await refresh();
  };
  const parsed = parseServerList(draft);
  const changed = !!list && parsed.join('\n') !== list.servers.join('\n');

  return (
    <>
      <h3>{t('STUN 서버')}</h3>
      <Field
        label={t('STUN 서버')}
        hint={t('{help} 한 줄에 하나씩 host:port로 적습니다(예: stun.example.org:3478, 포트를 빼면 3478). {n}개까지, 비우면 STUN을 쓰지 않습니다. 고른 서버는 이 기기의 공인 IP를 봅니다.', {
          help: STUN_HELP,
          n: MAX_STUN_SERVERS,
        })}
      >
        <textarea
          className="input mono"
          aria-label={t('STUN 서버')}
          rows={4}
          value={draft}
          disabled={!list}
          placeholder="stun.example.org:3478"
          spellCheck={false}
          onChange={(e) => setDraft(e.target.value)}
        />
      </Field>
      <div className="row gap">
        <button type="button" className="btn small primary" disabled={!changed} onClick={() => void save(parsed, t('STUN 서버를 저장했습니다'))}>
          {t('저장')}
        </button>
        <button
          type="button"
          className="btn small"
          disabled={!list || list.default}
          onClick={() => void save(null, t('STUN 서버를 기본 목록으로 되돌렸습니다'))}
        >
          <RotateCcw size={13} /> {t('기본 목록으로')}
        </button>
        {list?.default && <span className="muted small">{t('기본 목록을 쓰는 중')}</span>}
      </div>
    </>
  );
}
