// 네트워크 탭의 채널 묶음 토픽(`net.channel_topics`, 기본 꺼짐). 켜면 큰 크루(활성 기기 8대 초과)에서 크루 토픽 대신
// 최근 7일에 연 채널의 묶음 토픽과 멘션·답장 알림 토픽만 받아 트래픽을 줄입니다. 그 밖의 채널 새 글은 열 때 받아 오므로
// 켤 때만 확인을 받습니다. 끄는 것은 바로 저장합니다.

import { useCallback, useEffect, useState } from 'react';
import { Toggle } from '../components/common';
import { t } from '../i18n';
import { useStore } from '../store/store';

/** 스위치 아래 설명. */
export const CHANNEL_TOPICS_HELP = t(
  '큰 크루에서 최근 7일 안에 연 채널과 나를 멘션·답장한 메시지만 바로 받습니다. 다른 채널의 새 메시지는 그 채널을 열 때 받아 옵니다.',
);

/** 켤 때 띄우는 확인. */
export const CHANNEL_TOPICS_WARNING = t(
  '수천 명이 넘는 크루에서 받는 양이 크게 줄어듭니다. 대신 최근 7일 안에 열지 않은 채널의 새 메시지는 실시간으로 오지 않고, 그 채널을 열 때 받아 옵니다(멘션·답장은 그대로 바로 옵니다). 공개 채널만 해당하고, 비공개·휘발성·P2P 전용 채널은 예전처럼 받습니다. 활성 기기가 8대 이하인 크루는 바뀌지 않습니다.',
);

export function ChannelTopicsSection() {
  const client = useStore((s) => s.client);
  const call = useStore((s) => s.call);
  const openModal = useStore((s) => s.openModal);
  const [enabled, setEnabled] = useState<boolean | null>(null);
  const refresh = useCallback(async () => {
    if (!client) return;
    const r = await client.call('net.channel_topics', {}).catch(() => null);
    setEnabled(r ? r.enabled : null);
  }, [client]);
  useEffect(() => {
    void refresh();
  }, [refresh]);

  const save = async (v: boolean) => {
    if (!client) return;
    await call(client.call('net.set_channel_topics', { enabled: v }), {
      ok: v ? t('채널 묶음 토픽을 켰습니다') : t('채널 묶음 토픽을 껐습니다'),
    });
    await refresh();
  };
  const change = (v: boolean) => {
    if (enabled === null || v === enabled) return;
    if (!v) return void save(false);
    openModal({
      kind: 'confirm',
      title: t('채널 묶음 토픽 켜기'),
      body: CHANNEL_TOPICS_WARNING,
      confirmLabel: t('켜기'),
      onConfirm: () => save(true),
    });
  };

  return (
    <div className="card row between">
      <div>
        <strong>{t('큰 크루에서 연 채널만 바로 받기')}</strong>
        <div className="muted small">{CHANNEL_TOPICS_HELP}</div>
      </div>
      <Toggle checked={!!enabled} disabled={enabled === null} label={t('채널 묶음 토픽')} onChange={change} />
    </div>
  );
}
