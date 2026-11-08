// ffmpeg 인코더로 화면공유할 때 공유할 모니터·창 고르기. 웹뷰 기본 선택 창과 같은 모양(창·전체 화면 탭, 미리보기, 창 아이콘)
// 이지만 목록은 앱의 Rust 쪽 것이라 앱이 잡을 수 있는 화면만 나옵니다.

import { AppWindow, Monitor } from 'lucide-react';
import { useCallback, useEffect, useState } from 'react';
import type { ScreenSource } from '../api/client';
import { t } from '../i18n';
import { cx } from '../lib/misc';
import { useStore } from '../store/store';
import { Modal } from './Modal';

/** 웹뷰 선택 창처럼 열려 있는 동안 미리보기를 새로 찍는 간격. */
const REFRESH_MS = 2000;

type Tab = 'window' | 'monitor';
const key = (s: ScreenSource) => `${s.kind}:${s.id}`;

/** 닫거나 다른 창으로 바뀌면 store가 취소(`onCancel`)를 알립니다. */
export function ScreenSourceModal({ onPick, onClose }: { onPick: (s: ScreenSource) => void; onClose: () => void }) {
  const transport = useStore((s) => s.client?.transport?.screen);
  const [sources, setSources] = useState<ScreenSource[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [tab, setTab] = useState<Tab>('window');
  const [picked, setPicked] = useState<string | null>(null);

  const load = useCallback(() => {
    if (!transport) return;
    void transport
      .sources()
      .then((list) => {
        setSources(list);
        setError(null);
      })
      .catch((e: Error) => setError(e.message));
  }, [transport]);
  useEffect(() => {
    load();
    const timer = setInterval(load, REFRESH_MS);
    return () => clearInterval(timer);
  }, [load]);

  const shown = sources?.filter((s) => s.kind === tab) ?? [];
  // 고른 창이 닫혀 목록에서 빠지면 공유 버튼도 꺼집니다.
  const selected = sources?.find((s) => key(s) === picked) ?? null;
  const tabButton = (id: Tab, label: string) => (
    <button type="button" role="tab" aria-selected={tab === id} className={cx(tab === id && 'active')} onClick={() => setTab(id)}>
      {label}
    </button>
  );
  return (
    <Modal
      title={t('공유할 화면 고르기')}
      onClose={onClose}
      size="lg"
      footer={
        <>
          <button type="button" className="btn ghost" onClick={onClose}>
            {t('취소')}
          </button>
          <button type="button" className="btn primary" disabled={!selected} onClick={() => selected && onPick(selected)}>
            {t('공유')}
          </button>
        </>
      }
    >
      <div className="screen-tabs" role="tablist">
        {tabButton('window', t('창'))}
        {tabButton('monitor', t('전체 화면'))}
      </div>
      <div className="screen-grid" role="listbox" aria-label={tab === 'window' ? t('창') : t('전체 화면')}>
        {error && <p className="err small">{error}</p>}
        {!sources && !error && <p className="muted small">{t('목록을 읽는 중…')}</p>}
        {sources && shown.length === 0 && (
          <p className="muted small">{tab === 'window' ? t('공유할 수 있는 창이 없습니다.') : t('공유할 수 있는 화면이 없습니다.')}</p>
        )}
        {shown.map((s) => (
          <button
            key={key(s)}
            type="button"
            role="option"
            aria-selected={picked === key(s)}
            className={cx('screen-card', picked === key(s) && 'active')}
            title={`${s.title} (${s.width}×${s.height})`}
            onClick={() => setPicked(key(s))}
            onDoubleClick={() => onPick(s)}
          >
            <span className="screen-thumb">{s.thumbnail && <img src={s.thumbnail} alt="" draggable={false} />}</span>
            <span className="screen-name">
              {s.icon ? <img src={s.icon} alt="" width={16} height={16} /> : s.kind === 'monitor' ? <Monitor size={16} /> : <AppWindow size={16} />}
              <span className="ellipsis">{s.title}</span>
            </span>
          </button>
        ))}
      </div>
    </Modal>
  );
}
