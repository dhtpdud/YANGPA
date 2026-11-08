// 크루 STUN 서버(`SpaceView.crew_servers`, `crew.set_servers`)와 이 기기가 그 서버를 쓸지(`use_crew_servers`,
// `crew.use_servers`). 고치는 칸은 크루 관리 권한이 있어 목록이 오는 사람에게만, 쓰기 스위치는 내 역할로 쓸 수 있는 크루 서버가
// 있을 때(또는 이미 켜 두었을 때)만 보입니다.

import { Server } from 'lucide-react';
import { useState } from 'react';
import type { CrewServers, SpaceId } from '../api/types';
import { Toggle } from '../components/common';
import { t } from '../i18n';
import { useStore } from '../store/store';
import { Field } from './Modal';
import { MAX_STUN_SERVERS, parseServerList } from './StunServers';

/** 멤버가 크루 서버를 쓰려면 무엇이 필요한지. */
export const CREW_SERVERS_MEMBER_NOTE =
  t('멤버는 역할 권한 ‘크루 STUN 사용’이 있어야 쓸 수 있고, 각자 크루 설정에서 ‘이 크루의 STUN 서버 쓰기’를 켜야 씁니다.');

/** 쓰기 스위치 설명. */
export const CREW_SERVERS_USE_HINT =
  t('켜면 크루의 서버가 이 기기의 공인 IP를 봅니다. 내 STUN 서버를 먼저 쓰고, 크루 서버는 이 크루의 P2P 전용 채널·통화와 이 크루 멤버와의 신뢰 연결에만 씁니다. 이 기기에만 적용됩니다.');

const errorText = (e: unknown) => (e instanceof Error ? e.message : String(e));

/** 크루 설정(개요)의 ‘이 크루의 STUN 서버 쓰기’. */
export function CrewServersUseField({ space }: { space: SpaceId }) {
  const view = useStore((s) => s.views[space]);
  const client = useStore((s) => s.client);
  const call = useStore((s) => s.call);
  const refreshSpace = useStore((s) => s.refreshSpace);
  const [busy, setBusy] = useState(false);
  // 크루가 서버를 지웠거나 역할을 잃어도 켜 둔 것은 끌 수 있게 남깁니다.
  if (!view || view.kind !== 'crew' || !(view.crew_stun || view.use_crew_servers)) return null;
  return (
    <Field
      label={
        <span className="row gap-s">
          <Server size={13} /> {t('이 크루의 STUN 서버 쓰기')}
        </span>
      }
      hint={
        CREW_SERVERS_USE_HINT +
        ' ' +
        (view.crew_stun ? t('내 역할로 쓸 수 있는 크루 서버: {list}.', { list: 'STUN' }) : t('지금 내 역할로 쓸 수 있는 크루 서버가 없습니다.'))
      }
    >
      <Toggle
        checked={!!view.use_crew_servers}
        disabled={busy}
        label={t('이 크루의 STUN 서버 쓰기')}
        onChange={async (enabled) => {
          if (!client) return;
          setBusy(true);
          const r = await call(client.call('crew.use_servers', { space, enabled }), {
            ok: enabled ? t('이 크루의 STUN 서버를 씁니다') : t('이 크루의 STUN 서버를 쓰지 않습니다'),
          });
          setBusy(false);
          if (r === undefined) return;
          // 스위치가 바로 따라오게 보기에 먼저 반영하고 데몬 값으로 다시 맞춥니다.
          const v = useStore.getState().views[space];
          if (v) useStore.setState({ views: { ...useStore.getState().views, [space]: { ...v, use_crew_servers: enabled } } });
          refreshSpace(space);
        }}
      />
    </Field>
  );
}

/** 크루 관리 권한이 있는 사람의 크루 STUN 서버 칸. */
export function CrewServersSection({ space }: { space: SpaceId }) {
  const servers = useStore((s) => s.views[space]?.crew_servers);
  if (!servers) return null;
  // 저장된 목록이 바뀌면(내 저장·다른 관리자) 입력 칸을 새로 채웁니다.
  return <CrewServersEditor key={JSON.stringify(servers)} space={space} servers={servers} />;
}

function CrewServersEditor({ space, servers }: { space: SpaceId; servers: CrewServers }) {
  const client = useStore((s) => s.client);
  const toast = useStore((s) => s.toast);
  const refreshSpace = useStore((s) => s.refreshSpace);
  const [stun, setStun] = useState(servers.stun.join('\n'));
  const [saving, setSaving] = useState(false);
  const stunLines = parseServerList(stun);
  const changed = stunLines.join('\n') !== servers.stun.join('\n');

  // 줄 형식은 데몬이 봅니다(net.set_stun_servers와 같은 꼴).
  const save = async () => {
    if (!client) return;
    setSaving(true);
    try {
      await client.call('crew.set_servers', { space, stun: stunLines });
      toast('success', t('크루 STUN 서버를 저장했습니다'));
      refreshSpace(space);
    } catch (e) {
      toast('error', errorText(e));
    } finally {
      setSaving(false);
    }
  };

  return (
    <div className="card stack">
      <strong>{t('크루 STUN 서버')}</strong>
      <div className="muted small">
        {t('이 크루 멤버에게 STUN 서버를 나눠 줍니다. 멤버의 이 크루 P2P 전용 채널·통화와 이 크루 멤버끼리의 신뢰 연결에만 쓰이고, 멤버 자신의 서버를 먼저 씁니다.')}
      </div>
      <Field label={t('크루 STUN 서버')} hint={t('한 줄에 하나씩 host:port로 적습니다(예: stun.example.org:3478, 포트를 빼면 3478). {n}개까지.', { n: MAX_STUN_SERVERS })}>
        <textarea
          className="input mono"
          aria-label={t('크루 STUN 서버')}
          rows={3}
          value={stun}
          placeholder="stun.example.org:3478"
          spellCheck={false}
          onChange={(e) => setStun(e.target.value)}
        />
      </Field>
      <div className="muted small">{CREW_SERVERS_MEMBER_NOTE}</div>
      <div className="row gap">
        <button type="button" className="btn small primary" disabled={!changed || saving} onClick={() => void save()}>
          {t('저장')}
        </button>
      </div>
    </div>
  );
}
