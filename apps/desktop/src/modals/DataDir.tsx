// 데이터 폴더(윈도 설치판·포터블): 지금 폴더·크기 보기, 다른 폴더로 바꾸기(옮길지 크기·예상 시간을 보고 고름).
// 옮기기·바꾸기는 앱의 Rust 쪽(src-tauri/src/datadir.rs)이 데몬을 끄고 한 뒤 앱을 다시 시작합니다.

import { FolderOpen } from 'lucide-react';
import { useEffect, useState } from 'react';
import type { DataDirInfo, DataMovePlan } from '../api/client';
import { Spinner } from '../components/common';
import { t } from '../i18n';
import { formatBytes, formatDuration } from '../lib/format';
import { useStore } from '../store/store';
import { Modal } from './Modal';

/** 예상 시간 문구. */
export function formatEta(secs: number): string {
  if (!Number.isFinite(secs) || secs < 1) return t('1초 안');
  return t('약 {time}', { time: formatDuration(Math.ceil(secs)) });
}

function usageText(p: { usage: { bytes: number; files: number } }): string {
  return t('{size}(파일 {n}개)', { size: formatBytes(p.usage.bytes), n: p.usage.files.toLocaleString() });
}

function etaText(p: DataMovePlan): string {
  const eta = formatEta(p.estimate_secs);
  return p.same_volume ? t('{eta}(같은 드라이브라 옮기기만 함)', { eta }) : t('{eta}(다른 드라이브로 복사한 뒤 원래 것을 지움)', { eta });
}

/** 옮기는 동안의 진행 막대. */
function useMoveProgress(active: boolean): { done: number; total: number } | null {
  const dataDir = useStore((s) => s.client?.transport?.dataDir);
  const [p, setP] = useState<{ done: number; total: number } | null>(null);
  useEffect(() => {
    if (!active || !dataDir) return;
    return dataDir.onProgress(setP);
  }, [active, dataDir]);
  return active ? p : null;
}

function ProgressBar({ p }: { p: { done: number; total: number } | null }) {
  const pct = p && p.total > 0 ? Math.min(100, Math.round((p.done / p.total) * 100)) : 0;
  return (
    <div className="progress" aria-label={t('데이터 옮기는 중')}>
      <div className="bar" style={{ width: `${pct}%` }} />
      <span>{p ? `${formatBytes(p.done)} / ${formatBytes(p.total)} (${pct}%)` : t('데몬을 끄는 중…')}</span>
    </div>
  );
}

/** 사용자 설정 → 저장 공간 맨 위: 데이터 폴더. */
export function DataDirSection() {
  const dataDir = useStore((s) => s.client?.transport?.dataDir);
  const openModal = useStore((s) => s.openModal);
  const [info, setInfo] = useState<DataDirInfo | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  useEffect(() => {
    if (!dataDir) return;
    let stale = false;
    void dataDir
      .info()
      .then((i) => !stale && setInfo(i))
      .catch((e: Error) => !stale && setError(e.message));
    return () => {
      stale = true;
    };
  }, [dataDir]);
  if (!dataDir) return null;
  const choose = async (target: string | null) => {
    setError(null);
    if (!target) return;
    setBusy(true);
    try {
      const plan = await dataDir.plan(target);
      openModal({ kind: 'dataMove', plan });
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setBusy(false);
    }
  };
  const notDefault = info?.changeable && info.default_dir && info.default_dir !== info.dir;
  return (
    <section className="card stack data-dir" aria-label={t('데이터 폴더')}>
      <strong>{t('데이터 폴더')}</strong>
      <code className="data-dir-path" title={info?.dir}>
        {info?.dir ?? t('확인 중…')}
      </code>
      {info && <div className="muted small">{t('대화·키·첨부·설정 등 {usage}', { usage: usageText(info) })}</div>}
      {info?.changeable ? (
        <>
          <p className="muted small">{t('모든 데이터를 이 폴더에 둡니다. 다른 폴더로 바꿀 때 지금 데이터를 옮길지 고를 수 있고, 바꾸면 앱이 다시 시작합니다.')}</p>
          {notDefault && <p className="muted small">{t('기본 폴더: {dir}', { dir: info.default_dir ?? '' })}</p>}
          <div className="row gap-s">
            <button type="button" className="btn small" disabled={busy} onClick={() => void dataDir.pickFolder().then(choose)}>
              {busy ? <Spinner size={12} /> : <FolderOpen size={14} />} {t('폴더 바꾸기')}
            </button>
            {notDefault && (
              <button type="button" className="btn small ghost" disabled={busy} onClick={() => void choose(info.default_dir)}>
                {t('기본 폴더로')}
              </button>
            )}
          </div>
        </>
      ) : (
        info && <p className="muted small">{t('이 판은 운영체제의 앱 데이터 경로를 씁니다(폴더 바꾸기는 Windows 앱에서만 됩니다).')}</p>
      )}
      {error && <p className="err small">{error}</p>}
    </section>
  );
}

/** 폴더 바꾸기 확인: 크기·예상 시간을 보여 주고 옮길지 고릅니다. */
export function DataMoveModal({ plan, onClose }: { plan: DataMovePlan; onClose: () => void }) {
  const dataDir = useStore((s) => s.client?.transport?.dataDir);
  const [running, setRunning] = useState<'move' | 'switch' | null>(null);
  const [done, setDone] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const progress = useMoveProgress(running === 'move');
  const canMove = plan.target === 'missing' || plan.target === 'empty';
  const canSwitch = plan.target !== 'other';
  const run = async (mode: 'move' | 'switch') => {
    if (!dataDir) return;
    setError(null);
    setRunning(mode);
    try {
      await dataDir.change(plan.to, mode);
      setDone(true);
    } catch (e) {
      setError((e as Error).message);
      setRunning(null);
    }
  };
  const stopped = !!error || done;
  return (
    <Modal
      title={t('데이터 폴더 바꾸기')}
      onClose={running || stopped ? () => undefined : onClose}
      size="md"
      footer={
        error ? (
          <button type="button" className="btn primary" onClick={() => void dataDir?.restart()}>
            {t('앱 다시 시작')}
          </button>
        ) : running || done ? null : (
          <>
            <button type="button" className="btn ghost" onClick={onClose}>
              {t('취소')}
            </button>
            {canSwitch && (
              <button type="button" className="btn" onClick={() => void run('switch')}>
                {plan.target === 'yangpa' ? t('옮기지 않고 그 폴더의 데이터 쓰기') : t('옮기지 않고 새 폴더에서 새로 시작')}
              </button>
            )}
            {canMove && (
              <button type="button" className="btn primary" onClick={() => void run('move')}>
                {t('데이터 옮기기')}
              </button>
            )}
          </>
        )
      }
    >
      <div className="stack data-move">
        <dl className="kv">
          <dt>{t('지금 폴더')}</dt>
          <dd className="mono">{plan.from}</dd>
          <dt>{t('새 폴더')}</dt>
          <dd className="mono">{plan.to}</dd>
          <dt>{t('옮길 데이터')}</dt>
          <dd>{usageText(plan)}</dd>
          <dt>{t('예상 시간')}</dt>
          <dd>{etaText(plan)}</dd>
        </dl>
        {plan.target === 'yangpa' && (
          <p className="warn small">
            {t('새 폴더에 이미 양파 데이터가 있어 옮길 수 없습니다. 그 데이터를 쓰거나(지금 데이터는 지금 폴더에 그대로 남음) 다른 폴더를 고르세요.')}
          </p>
        )}
        {plan.target === 'other' && <p className="err small">{t('새 폴더에 다른 파일이 있습니다. 빈 폴더를 골라 주세요.')}</p>}
        {canMove && (
          <p className="muted small">
            {t('옮기면 지금 데이터가 새 폴더로 가고 지금 폴더에서는 지워집니다. 옮기지 않으면 지금 데이터는 지금 폴더에 그대로 남고, 새 폴더에서 새 기기로 시작합니다. 어느 쪽이든 끝나면 앱이 다시 시작합니다.')}
          </p>
        )}
        {running === 'move' && !error && <ProgressBar p={progress} />}
        {running === 'switch' && !error && !done && <p className="muted small">{t('바꾸는 중…')}</p>}
        {done && <p className="small">{t('다 됐습니다. 앱을 다시 시작합니다…')}</p>}
        {error && (
          <p className="err small">
            {error} {t('데몬이 꺼져 있으니 앱을 다시 시작해 주세요(데이터 폴더는 그대로입니다).')}
          </p>
        )}
      </div>
    </Modal>
  );
}
