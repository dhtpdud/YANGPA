// 크루·채널·사람별 속도 제한(이 기기만의 설정): 범위 하나를 고치는 창(크루 메뉴·채널 오른쪽 클릭·프로필에서 엶)과
// 사용자 설정 → 동기화의 전체 목록. 전체 제한과 여러 범위가 함께 걸리면 데몬이 가장 낮은 값을 따릅니다.

import { Gauge, Pencil, Trash2 } from 'lucide-react';
import { useEffect, useState } from 'react';
import type { RateScope } from '../api/types';
import { Spinner } from '../components/common';
import { currentLang, t } from '../i18n';
import { formatLimit } from '../lib/format';
import { cx } from '../lib/misc';
import { RATE_UNITS, describeScope, findLimit, limitToInput, parseLimitInput, scopeKey, type RateUnit } from '../lib/ratelimit';
import { useStore, type Modal as ModalState } from '../store/store';
import { Field, Modal } from './Modal';

/** 창과 목록에 함께 보이는 설명. */
export const RATE_LIMIT_NOTE = t(
  '이 기기에만 저장되는 설정입니다(다른 사람에게 알리지 않음). 전체 제한(사용자 설정 → 동기화)과 크루·채널·사람별 제한이 함께 걸리면 가장 낮은 값을 따릅니다.',
);

/** 범위마다 무엇이 느려지는지. */
export function scopeHelp(scope: RateScope): string {
  switch (scope.kind) {
    case 'crew':
      return t('올리기: 이 기기가 이 크루(1대1 대화 포함)의 메시지·첨부를 다른 사람에게 내줄 때. 받기: 이 크루의 첨부를 받을 때.');
    case 'channel':
      return t('올리기: 이 기기가 이 채널의 메시지·첨부를 다른 사람에게 내줄 때. 받기: 이 채널의 첨부를 받을 때.');
    case 'user':
      return t('올리기: 이 기기가 이 사람에게 내줄 때(모든 크루). 받기: 이 사람에게서 첨부 조각을 받을 때.');
  }
}

function LimitField({
  label,
  text,
  unit,
  error,
  onText,
  onUnit,
}: {
  label: string;
  text: string;
  unit: RateUnit;
  error?: string;
  onText: (v: string) => void;
  onUnit: (u: RateUnit) => void;
}) {
  return (
    <Field label={label} hint={error ? <span className="err">{error}</span> : t('비우면 제한 없음, 0이면 막음')}>
      <div className="row gap-s">
        <input
          className={cx('input', error && 'invalid')}
          type="text"
          inputMode="decimal"
          placeholder={t('제한 없음')}
          aria-label={label}
          aria-invalid={!!error}
          value={text}
          onChange={(e) => onText(e.target.value)}
        />
        <select className="select rl-unit" aria-label={t('{label} 단위', { label })} value={unit} onChange={(e) => onUnit(e.target.value as RateUnit)}>
          {(Object.keys(RATE_UNITS) as RateUnit[]).map((u) => (
            <option key={u} value={u}>
              {u}/s
            </option>
          ))}
        </select>
      </div>
    </Field>
  );
}

/** 범위 하나의 속도 제한 창. 저장하면 그 범위의 제한을 통째로 바꿉니다(둘 다 비우면 지움). */
export function RateLimitModal({ scope, onClose }: { scope: RateScope; onClose: () => void }) {
  const views = useStore((s) => s.views);
  const friends = useStore((s) => s.friends);
  const limits = useStore((s) => s.rateLimits);
  const unsupported = useStore((s) => s.rateLimitsUnsupported);
  const refresh = useStore((s) => s.refreshRateLimits);
  const setRateLimit = useStore((s) => s.setRateLimit);
  const current = findLimit(limits, scope);
  const [up, setUp] = useState(() => limitToInput(current?.up_bps));
  const [down, setDown] = useState(() => limitToInput(current?.down_bps));
  const [touched, setTouched] = useState(false);
  const [busy, setBusy] = useState(false);
  const [tried, setTried] = useState(false);
  useEffect(() => {
    void refresh();
  }, [refresh]);
  // 목록을 늦게 읽었으면 아직 고치지 않은 칸만 지금 값으로 채웁니다.
  const loadedKey = current ? `${current.up_bps}|${current.down_bps}` : 'none';
  useEffect(() => {
    if (touched) return;
    setUp(limitToInput(current?.up_bps));
    setDown(limitToInput(current?.down_bps));
  }, [loadedKey]);
  const { kind, label } = describeScope(scope, { views, friends });
  const upParsed = parseLimitInput(up.text, up.unit);
  const downParsed = parseLimitInput(down.text, down.unit);
  // 입력 중에는 잘못된 값을 바로 알리고, 저장을 누르면 막습니다.
  const upError = !upParsed.ok && (tried || up.text.trim() !== '') ? upParsed.error : undefined;
  const downError = !downParsed.ok && (tried || down.text.trim() !== '') ? downParsed.error : undefined;
  const save = async (clear = false) => {
    setTried(true);
    if (!clear && (!upParsed.ok || !downParsed.ok)) return;
    setBusy(true);
    const ok = clear
      ? await setRateLimit(scope, null, null)
      : await setRateLimit(scope, upParsed.ok ? upParsed.bps : null, downParsed.ok ? downParsed.bps : null);
    setBusy(false);
    if (ok) onClose();
  };
  return (
    <Modal
      title={
        <>
          <Gauge size={18} /> {t('속도 제한')}
        </>
      }
      onClose={onClose}
      size="sm"
      className="rate-limit-modal"
      footer={
        <>
          {current && (
            <button type="button" className="btn ghost rl-clear" disabled={busy || unsupported} onClick={() => void save(true)}>
              {t('제한 지우기')}
            </button>
          )}
          <span className="grow" />
          <button type="button" className="btn ghost" onClick={onClose}>
            {t('취소')}
          </button>
          <button type="button" className="btn primary" disabled={busy || unsupported} onClick={() => void save()}>
            {busy && <Spinner size={14} />} {t('저장')}
          </button>
        </>
      }
    >
      <div className="stack">
        <div className="rl-scope">
          <span className="tag">{kind}</span>
          <strong className="rl-scope-name">{label}</strong>
        </div>
        {unsupported ? (
          <p className="muted">{t('연결된 데몬이 크루·채널·사람별 속도 제한을 지원하지 않습니다.')}</p>
        ) : (
          <>
            <div className="grid2">
              <LimitField
                label={t('올리기 제한')}
                text={up.text}
                unit={up.unit}
                error={upError}
                onText={(text) => {
                  setTouched(true);
                  setUp({ ...up, text });
                }}
                onUnit={(unit) => {
                  setTouched(true);
                  setUp({ ...up, unit });
                }}
              />
              <LimitField
                label={t('받기 제한')}
                text={down.text}
                unit={down.unit}
                error={downError}
                onText={(text) => {
                  setTouched(true);
                  setDown({ ...down, text });
                }}
                onUnit={(unit) => {
                  setTouched(true);
                  setDown({ ...down, unit });
                }}
              />
            </div>
            <p className="muted small">{scopeHelp(scope)}</p>
          </>
        )}
        <p className="rl-note small">{RATE_LIMIT_NOTE}</p>
      </div>
    </Modal>
  );
}

/** 한 방향의 제한 표시. */
function dirText(arrow: string, bps: number | null): string {
  return arrow + ' ' + formatLimit(bps, t('제한 없음'));
}

/** 사용자 설정 → 동기화: 범위별 제한 전체(이름·값·고치기·지우기). `back`은 고치기 창을 닫으면 돌아올 창. */
export function ScopedLimitsSection({ back }: { back?: ModalState }) {
  const views = useStore((s) => s.views);
  const friends = useStore((s) => s.friends);
  const limits = useStore((s) => s.rateLimits);
  const unsupported = useStore((s) => s.rateLimitsUnsupported);
  const refresh = useStore((s) => s.refreshRateLimits);
  const setRateLimit = useStore((s) => s.setRateLimit);
  const openModal = useStore((s) => s.openModal);
  useEffect(() => {
    void refresh();
  }, [refresh]);
  const rows = (limits ?? [])
    .map((l) => ({ l, ...describeScope(l.scope, { views, friends }) }))
    .sort((a, b) => a.kind.localeCompare(b.kind, currentLang()) || a.label.localeCompare(b.label, currentLang()));
  return (
    <section className="stack" aria-label={t('범위별 속도 제한')}>
      <h3>{t('크루·채널·사람별 속도 제한')}</h3>
      <p className="muted small">
        {RATE_LIMIT_NOTE} {t('크루 메뉴(크루 이름 누르기), 채널 오른쪽 클릭, 사람 프로필의 ‘속도 제한’에서 더합니다. 크루를 나가면 그 크루·채널의 제한은 지워집니다.')}
      </p>
      {unsupported ? (
        <p className="muted">{t('연결된 데몬이 범위별 속도 제한을 지원하지 않습니다.')}</p>
      ) : limits === null ? (
        <Spinner />
      ) : rows.length === 0 ? (
        <p className="muted">{t('범위별 속도 제한이 없습니다.')}</p>
      ) : (
        <div className="table rl-table">
          {rows.map(({ l, kind, label }) => (
            <div key={scopeKey(l.scope)} className="tr">
              <span className="tag">{kind}</span>
              <div className="grow rl-name" title={label}>
                <strong>{label}</strong>
                <div className="muted small rl-values">
                  <span className="rate-up">{dirText('↑', l.up_bps)}</span> · <span className="rate-down">{dirText('↓', l.down_bps)}</span>
                </div>
              </div>
              <button
                type="button"
                className="icon-btn small"
                title={t('고치기')}
                aria-label={t('{label} 속도 제한 고치기', { label })}
                onClick={() => openModal({ kind: 'rateLimit', scope: l.scope, back })}
              >
                <Pencil size={14} />
              </button>
              <button
                type="button"
                className="icon-btn small danger"
                title={t('지우기')}
                aria-label={t('{label} 속도 제한 지우기', { label })}
                onClick={() => void setRateLimit(l.scope, null, null)}
              >
                <Trash2 size={14} />
              </button>
            </div>
          ))}
        </div>
      )}
    </section>
  );
}
