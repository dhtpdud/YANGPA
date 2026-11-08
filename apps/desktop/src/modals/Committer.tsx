// 크루 설정 → 개요 → 확정 기기(소유자만 바꿈). 확정 기기는 멤버·기기·역할이 바뀔 때 키 그룹 갱신(KeyUpdate)을 내는
// 기기입니다. 후보는 데몬(crew.committer_candidates)이 소유자·관리자의 활성 기기로 좁혀 주고, 바꾸기 전에 확인을 받습니다.

import { Crown, Server } from 'lucide-react';
import { useCallback, useEffect, useState } from 'react';
import { ErrorCode, type CommitterCandidate } from '../api/types';
import { Spinner } from '../components/common';
import { t } from '../i18n';
import { displayName } from '../lib/misc';
import { useStore } from '../store/store';

/** 화면에 보일 기기 이름(이름이 없으면 ID 앞부분). */
export function deviceLabel(c: CommitterCandidate): string {
  return c.name.trim() || t('기기 {id}', { id: c.device.slice(0, 8) });
}

export function CommitterSection({ space }: { space: string }) {
  const view = useStore((s) => s.views[space]);
  const client = useStore((s) => s.client);
  const call = useStore((s) => s.call);
  const me = useStore((s) => s.me);
  const openModal = useStore((s) => s.openModal);
  const [list, setList] = useState<CommitterCandidate[] | null>(null);
  const [unsupported, setUnsupported] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [sel, setSel] = useState('');

  const load = useCallback(async () => {
    if (!client) return;
    try {
      const r = await client.call('crew.committer_candidates', { space });
      setError(null);
      setList(r);
      setSel(r.find((c) => c.current)?.device ?? '');
    } catch (e) {
      // 메서드를 모르는 예전 데몬일 때만 칸을 숨깁니다. 잠깐의 오류(연결 끊김 등)는 보여 주고 다시 읽게 합니다.
      if ((e as { code?: number }).code === ErrorCode.MethodNotFound) setUnsupported(true);
      else setError((e as Error).message || t('읽지 못했습니다'));
    }
  }, [client, space]);

  // 확정 기기가 바뀌면(space.updated → 보기 갱신) 다시 읽습니다.
  const committer = view?.committer;
  useEffect(() => {
    void load();
  }, [load, committer]);

  if (!view || unsupported) return null;
  const owner = view.owner === me;
  const member = (user: string) => view.members.find((m) => m.user === user);

  const choose = (c: CommitterCandidate) =>
    openModal({
      kind: 'confirm',
      title: t('확정 기기 바꾸기'),
      body:
        t(
          '{who}의 ‘{device}’을(를) 이 크루의 확정 기기로 정합니다. 확정 기기가 꺼져 있는 동안에는 멤버·기기·역할 변경이 다시 켜질 때까지 미뤄집니다. 새 확정 기기가 아직 들지 않은 비공개 채널은 그 기기가 그룹을 다시 세워 이어 갑니다.',
          { who: displayName(member(c.user)), device: deviceLabel(c) },
        ) + (c.online ? '' : ' ' + t('이 기기는 지금 연결돼 있지 않습니다.')),
      confirmLabel: t('정하기'),
      onConfirm: async () => {
        if (!client) return;
        const r = await call(client.call('crew.set_committer', { space, device: c.device }), { ok: t('확정 기기를 바꿨습니다') });
        if (r !== undefined) await load();
      },
    });

  const current = list?.find((c) => c.current);
  const picked = list?.find((c) => c.device === sel);
  return (
    <div className="card">
      <strong className="row gap-s">
        <Server size={15} /> {t('확정 기기')}
      </strong>
      <p className="muted small">
        {t(
          '멤버·기기·역할이 바뀔 때 크루의 그룹 키 변경을 확정하는 기기입니다. 늘 켜져 있는 기기(예: 헤드리스 시더)로 정하면 변경이 빨리 반영됩니다. 소유자와 관리자의 기기만 고를 수 있고, 소유자만 바꿀 수 있습니다.',
        )}
      </p>
      {error && !list ? (
        <div className="row gap-s">
          <span className="muted small">{t('확정 기기 후보를 읽지 못했습니다: {error}', { error })}</span>
          <button type="button" className="btn small" onClick={() => void load()}>
            {t('다시 읽기')}
          </button>
        </div>
      ) : !list ? (
        <Spinner />
      ) : (
        <div className="committer-list" role="radiogroup" aria-label={t('확정 기기')}>
          {list.map((c) => (
            <label key={c.device} className="committer-row">
              <input
                type="radio"
                name={`committer-${space}`}
                value={c.device}
                checked={sel === c.device}
                disabled={!owner}
                onChange={() => setSel(c.device)}
              />
              <span className={c.online ? 'dot on' : 'dot'} title={c.online ? t('연결됨') : t('연결 안 됨')} />
              <span className="grow">
                <span>{deviceLabel(c)}</span>
                <span className="muted small">
                  {' '}
                  · {displayName(member(c.user))}
                  {c.user === view.owner && (
                    <>
                      {' '}
                      <Crown size={11} />
                    </>
                  )}
                </span>
              </span>
              {c.mine && <span className="tag">{t('이 기기')}</span>}
              {c.current && <span className="tag ok">{t('지금 확정 기기')}</span>}
            </label>
          ))}
        </div>
      )}
      {owner && list && (
        <div className="row gap">
          <button type="button" className="btn small primary" disabled={!picked || picked.current} onClick={() => picked && choose(picked)}>
            {t('확정 기기로 정하기')}
          </button>
          {current && !current.online && <span className="muted small">{t('지금 확정 기기가 연결돼 있지 않습니다.')}</span>}
        </div>
      )}
    </div>
  );
}
