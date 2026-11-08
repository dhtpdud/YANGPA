// 사용자 설정 → 저장 공간: 받은 첨부가 차지하는 공간, 한도·최소 디스크 여유, 비우기, 보관(고정)한 첨부.
// 남에게서 받은 첨부만 한도를 넘으면 오래 안 본 것부터 지웁니다(다시 받을 수 있음). 내가 올린 첨부·이모지·프로필 이미지·썸네일은
// 지우지 않습니다. 값은 데몬(blob.cache_*)이 들고 있고 여기서는 보여 주고 바꾸기만 합니다.

import { HardDrive, PinOff, Trash2 } from 'lucide-react';
import { useCallback, useEffect, useState } from 'react';
import { ErrorCode, type BlobCacheView, type BlobPinView } from '../api/types';
import { Spinner } from '../components/common';
import { markPinned } from '../hooks/pins';
import { t } from '../i18n';
import { formatBytes, formatRelative } from '../lib/format';
import { useStore } from '../store/store';
import { Field } from './Modal';

const GIB = 1024 * 1024 * 1024;

/** GiB 입력값(소수 한 자리). */
export function toGib(bytes: number): string {
  return String(Math.round((bytes / GIB) * 10) / 10);
}

/** GiB 입력을 바이트로. 비었거나 잘못됐으면 null. */
export function parseGib(v: string): number | null {
  const text = v.trim().replace(',', '.');
  if (!text) return null;
  const n = Number(text);
  if (!Number.isFinite(n) || n < 0 || n > 1_000_000) return null;
  return Math.round(n * GIB);
}

export function StorageTab() {
  const client = useStore((s) => s.client);
  const call = useStore((s) => s.call);
  const toast = useStore((s) => s.toast);
  const openModal = useStore((s) => s.openModal);
  const spaces = useStore((s) => s.spaces);
  const [view, setView] = useState<BlobCacheView | null>(null);
  const [pins, setPins] = useState<BlobPinView[]>([]);
  const [unsupported, setUnsupported] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [limit, setLimit] = useState('');
  const [minFree, setMinFree] = useState('');

  const show = useCallback((v: BlobCacheView) => {
    setView(v);
    setLimit(toGib(v.limit_bytes));
    setMinFree(toGib(v.min_free_bytes));
  }, []);

  const load = useCallback(async () => {
    if (!client) return;
    try {
      show(await client.call('blob.cache_status', {}));
      setPins(await client.call('blob.pins', {}));
      setError(null);
    } catch (e) {
      // 메서드를 모르는 예전 데몬일 때만 '지원하지 않음'으로 봅니다. 잠깐의 오류는 보여 주고 다시 읽게 합니다.
      if ((e as { code?: number }).code === ErrorCode.MethodNotFound) setUnsupported(true);
      else setError((e as Error).message || t('읽지 못했습니다'));
    }
  }, [client, show]);

  useEffect(() => {
    void load();
  }, [load]);

  if (unsupported) return <p className="muted">{t('이 데몬은 저장 공간 정리를 지원하지 않습니다.')}</p>;
  if (error && !view)
    return (
      <div className="stack">
        <p className="muted">{t('저장 공간 정보를 읽지 못했습니다: {error}', { error })}</p>
        <button type="button" className="btn small" onClick={() => void load()}>
          {t('다시 읽기')}
        </button>
      </div>
    );
  if (!view) return <Spinner />;

  const limitBytes = parseGib(limit);
  const minFreeBytes = parseGib(minFree);
  const dirty = (limitBytes !== null && limitBytes !== view.limit_bytes) || (minFreeBytes !== null && minFreeBytes !== view.min_free_bytes);
  const used = view.limit_bytes > 0 ? Math.min(100, Math.round((view.cache_bytes / view.limit_bytes) * 100)) : 100;
  const custom = view.limit_bytes !== view.default_limit_bytes || view.min_free_bytes !== view.default_min_free_bytes;
  const spaceName = (id: string) => spaces.find((s) => s.space === id)?.name ?? t('알 수 없는 공간');

  const apply = async () => {
    if (!client || !dirty) return;
    const r = await call(
      client.call('blob.cache_set_limit', {
        limit_bytes: limitBytes ?? undefined,
        min_free_bytes: minFreeBytes ?? undefined,
      }),
      { ok: t('저장 공간 한도를 바꿨습니다') },
    );
    if (r) show(r);
  };

  const reset = async () => {
    if (!client) return;
    const r = await call(client.call('blob.cache_set_limit', { reset: true }), { ok: t('기본값으로 되돌렸습니다') });
    if (r) show(r);
  };

  const clear = () =>
    openModal({
      kind: 'confirm',
      title: t('받은 첨부 비우기'),
      body: t(
        '남에게서 받은 첨부의 원본을 이 기기에서 지웁니다. 썸네일은 남고, 가진 기기가 온라인이면 다시 받을 수 있습니다. 보관한 첨부·내가 올린 첨부·이모지·프로필 이미지는 지우지 않습니다.',
      ),
      danger: true,
      confirmLabel: t('비우기'),
      onConfirm: async () => {
        if (!client) return;
        const r = await call(client.call('blob.cache_clear', {}));
        if (!r) return;
        toast(
          'success',
          r.evicted_count > 0
            ? t('첨부 {n}개({size})를 비웠습니다', { n: r.evicted_count, size: formatBytes(r.freed_bytes) })
            : t('비울 첨부가 없습니다'),
        );
        await load();
      },
    });

  const unpin = async (p: BlobPinView) => {
    if (!client) return;
    const r = await call(client.call('blob.unpin', { space: p.space, blob: p.blob }), { ok: t('보관을 풀었습니다') });
    if (r === undefined) return;
    markPinned(p.blob, false);
    setPins((list) => list.filter((x) => x.blob !== p.blob));
    void client.call('blob.cache_status', {}).then(setView, () => undefined);
  };

  return (
    <div className="stack">
      <h3>{t('저장 공간')}</h3>
      <p className="muted small">
        {t(
          '남에게서 받은 첨부는 한도를 넘거나 디스크 여유 공간이 모자라면 오래 안 본 것부터 원본을 지웁니다. 지운 첨부도 썸네일은 남고, 가진 기기가 온라인이면 다시 받을 수 있습니다. 내가 올린 첨부·이모지·프로필 이미지는 지우지 않고, 보관한 첨부도 남깁니다.',
        )}
      </p>
      <div className="card">
        <div className="row between">
          <strong className="row gap-s">
            <HardDrive size={16} /> {t('받은 첨부')}
          </strong>
          <span className="mono small">
            {formatBytes(view.cache_bytes)} / {formatBytes(view.limit_bytes)}
          </span>
        </div>
        <div className="storage-meter" role="meter" aria-valuenow={used} aria-valuemin={0} aria-valuemax={100} aria-label={t('받은 첨부 사용량')}>
          <div className={used >= 100 ? 'bar over' : 'bar'} style={{ width: `${used}%` }} />
        </div>
        <div className="kv">
          <span>{t('받은 첨부')}</span>
          <span>{t('{n}개 · {size}', { n: view.cache_count, size: formatBytes(view.cache_bytes) })}</span>
          <span>{t('보관한 첨부')}</span>
          <span>{t('{n}개 · {size}', { n: view.pinned_count, size: formatBytes(view.pinned_bytes) })}</span>
          <span>{t('늘 남기는 것')}</span>
          <span>
            {t('{n}개 · {size} (내가 올린 첨부·이모지·프로필 이미지)', { n: view.kept_count, size: formatBytes(view.kept_bytes) })}
          </span>
          <span>{t('디스크 여유 공간')}</span>
          <span>{view.disk_free_bytes != null ? formatBytes(view.disk_free_bytes) : t('알 수 없음')}</span>
          <span>{t('이번 실행에서 정리')}</span>
          <span>{t('{n}개 · {size}', { n: view.evicted_count, size: formatBytes(view.evicted_bytes) })}</span>
        </div>
      </div>
      <div className="grid2">
        <Field
          label={t('받은 첨부 한도(GiB)')}
          hint={t('기본 {size}. 0이면 받은 첨부를 남기지 않습니다(볼 때마다 다시 받음).', { size: formatBytes(view.default_limit_bytes) })}
        >
          <input className="input" type="number" min={0} step={0.5} value={limit} aria-label={t('받은 첨부 한도')} onChange={(e) => setLimit(e.target.value)} />
        </Field>
        <Field
          label={t('남겨 둘 디스크 여유 공간(GiB)')}
          hint={t('기본 {size}. 이보다 적으면 한도 아래여도 정리합니다.', { size: formatBytes(view.default_min_free_bytes) })}
        >
          <input className="input" type="number" min={0} step={0.5} value={minFree} aria-label={t('최소 여유 공간')} onChange={(e) => setMinFree(e.target.value)} />
        </Field>
      </div>
      <div className="row gap">
        <button type="button" className="btn primary small" disabled={!dirty} onClick={() => void apply()}>
          {t('적용')}
        </button>
        <button type="button" className="btn small" disabled={!custom} onClick={() => void reset()}>
          {t('기본값으로')}
        </button>
        <span className="grow" />
        <button type="button" className="btn danger-outline small" onClick={clear}>
          <Trash2 size={14} /> {t('받은 첨부 비우기')}
        </button>
      </div>
      <h3>{t('보관한 첨부')}</h3>
      {pins.length === 0 ? (
        <p className="muted small">{t('보관한 첨부가 없습니다. 메시지의 첨부에서 압정 단추를 누르면 정리에서 빠집니다.')}</p>
      ) : (
        <div>
          {pins.map((p) => (
            <div key={p.blob} className="list-row">
              <span className="mono small" title={p.blob}>
                {p.blob.slice(0, 10)}…
              </span>
              <span className="muted small">
                {spaceName(p.space)} · {p.size ? formatBytes(p.size) : t('크기 모름')}
                {p.downloaded ? '' : ' · ' + t('받는 중')} · {formatRelative(p.pinned_at)}
              </span>
              <button type="button" className="btn ghost small" onClick={() => void unpin(p)}>
                <PinOff size={14} /> {t('보관 풀기')}
              </button>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}
