// 네트워크 탭의 Tor 파일 회로 수(`net.file_circuits`, 상대 하나에 더 여는 회로 0~4개). 기본은 0(끔)이고, 0에서 1 이상으로
// 올릴 때만 익명성 경고를 띄웁니다(사용자 결정 2026-10-22·10-24). 줄이거나 0으로 끄는 것은 경고 없이 바로 저장합니다.

import { useCallback, useEffect, useState } from 'react';
import { t } from '../i18n';
import { useStore } from '../store/store';

/** 고를 수 있는 가장 큰 수(데몬 MAX_CIRCUITS). */
export const MAX_FILE_CIRCUITS = 4;

/** 고르기 아래 설명. */
export const FILE_CIRCUITS_HELP = t(
  'Tor로 큰 첨부를 주고받을 때 상대마다 파일 전용 Tor 회로를 이만큼 더 열어 조각을 나눠 받습니다. 0이면 쓰지 않고, 상대도 1 이상일 때만 쓰입니다.',
);

/** 0에서 올릴 때 띄우는 경고. */
export const FILE_CIRCUITS_WARNING = t(
  'Tor로 큰 첨부를 받을 때 더 빨라질 수 있습니다. 대신 상대가 이 기기에서 받을 때마다 이 기기의 onion 서비스가 Tor 회로를 더 자주 만들어, 이 기기의 Tor 입구(가드)를 찾아내려는 공격에 조금 더 드러납니다. 회로 수가 많을수록 더 그렇고, vanguards가 이 위험을 줄이지만 없애지는 못합니다. 상대도 이 설정을 1 이상으로 두었을 때만 쓰이고, 0으로 두면 바로 예전처럼 주 연결로만 주고받습니다.',
);

export function FileCircuitsSection() {
  const client = useStore((s) => s.client);
  const call = useStore((s) => s.call);
  const openModal = useStore((s) => s.openModal);
  const [count, setCount] = useState<number | null>(null);
  const refresh = useCallback(async () => {
    if (!client) return;
    const r = await client.call('net.file_circuits', {}).catch(() => null);
    // 개수를 모르는 예전 데몬이면 켬 = 4개.
    setCount(r ? (r.count ?? (r.enabled ? MAX_FILE_CIRCUITS : 0)) : null);
  }, [client]);
  useEffect(() => {
    void refresh();
  }, [refresh]);

  const save = async (n: number) => {
    if (!client) return;
    await call(client.call('net.set_file_circuits', { enabled: n > 0, count: n }), {
      ok: n > 0 ? t('Tor 파일 회로를 {n}개로 정했습니다', { n }) : t('Tor 파일 회로를 껐습니다'),
    });
    await refresh();
  };
  const change = (n: number) => {
    if (count === null || n === count) return;
    if (count > 0 || n === 0) return void save(n);
    openModal({
      kind: 'confirm',
      title: t('Tor 파일 회로 {n}개 켜기', { n }),
      body: FILE_CIRCUITS_WARNING,
      danger: true,
      confirmLabel: t('켜기'),
      onConfirm: () => save(n),
    });
  };

  return (
    <div className="card row between">
      <div>
        <strong>{t('Tor 파일 회로')}</strong>
        <div className="muted small">{FILE_CIRCUITS_HELP}</div>
      </div>
      <div className="seg" role="radiogroup" aria-label={t('Tor 파일 회로 수')}>
        {Array.from({ length: MAX_FILE_CIRCUITS + 1 }, (_, n) => (
          <button
            key={n}
            type="button"
            role="radio"
            aria-checked={count === n}
            className={count === n ? 'active' : ''}
            disabled={count === null}
            onClick={() => change(n)}
          >
            {n}
          </button>
        ))}
      </div>
    </div>
  );
}
