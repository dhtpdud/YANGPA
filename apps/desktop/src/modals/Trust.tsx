// 신뢰 스위치: 상대 프로필의 ‘연결’ 구역, 크루 설정의 ‘이 크루 신뢰’, 켤 때마다 띄우는 IP 노출 경고.
// 신뢰 = 그 사람(크루 멤버)에게 이 기기의 IP를 보여도 된다는 동의입니다. 앱은 IP 주소를 어디에도 보이지 않고 연결 방식만 알립니다.

import { AlertTriangle, Handshake, Network } from 'lucide-react';
import { useCallback, useEffect, useRef, useState } from 'react';
import { RpcError } from '../api/client';
import { ErrorCode, type SpaceId, type TrustView, type UserId } from '../api/types';
import { Spinner, Toggle } from '../components/common';
import { t } from '../i18n';
import { canTrustUser, connectionLabel, TRUST_WARNING_DETAIL, trustWarningLead, trustWarningPoints, type TrustTarget } from '../lib/trust';
import { useStore } from '../store/store';
import { Field, Modal } from './Modal';

/** 프로필이 열린 동안 연결 방식을 다시 읽는 간격(연결은 상대가 켜고 끄거나 접속이 바뀌면 달라짐). */
export const TRUST_POLL_MS = 5000;

/** 신뢰 스위치를 켤 때의 경고. 확인해야만 켜고, ‘다시 보지 않기’를 고르고 확인하면 다음부터 건너뜁니다. */
export function TrustWarningModal({
  target,
  onConfirm,
  onCancel,
  onClose,
}: {
  target: TrustTarget;
  onConfirm: (skip: boolean) => Promise<void>;
  onCancel: () => void;
  onClose: () => void;
}) {
  const [skip, setSkip] = useState(false);
  const [busy, setBusy] = useState(false);
  // 보내는 중에 Esc·바깥 누르기로 취소되지 않게. Modal의 onClose가 바뀌면 포커스를 다시 잡으므로 ref로 보고 함수는 고정합니다.
  const busyRef = useRef(false);
  const cancel = useCallback(() => {
    if (busyRef.current) return;
    onCancel();
    onClose();
  }, [onCancel, onClose]);
  const confirm = async () => {
    busyRef.current = true;
    setBusy(true);
    try {
      await onConfirm(skip);
    } finally {
      busyRef.current = false;
      setBusy(false);
      onClose();
    }
  };
  return (
    <Modal
      title={
        <span className="row gap-s">
          <AlertTriangle size={18} /> {t('IP 노출 경고')}
        </span>
      }
      onClose={cancel}
      size="sm"
      className="trust-warning"
      footer={
        <>
          <button type="button" className="btn ghost" disabled={busy} onClick={cancel}>
            {t('취소')}
          </button>
          <button type="button" className="btn danger" disabled={busy} onClick={() => void confirm()}>
            {busy && <Spinner size={14} />} {t('신뢰 켜기')}
          </button>
        </>
      }
    >
      <div className="warn-box" role="alert">
        <AlertTriangle size={16} />
        <div>
          <strong>{trustWarningLead(target)}</strong>
          <div>{TRUST_WARNING_DETAIL}</div>
        </div>
      </div>
      <ul className="trust-points">
        {trustWarningPoints(target).map((p) => (
          <li key={p}>{p}</li>
        ))}
      </ul>
      <label className="check">
        <input type="checkbox" checked={skip} onChange={(e) => setSkip(e.target.checked)} /> {t('다시 보지 않기')}
      </label>
    </Modal>
  );
}

/**
 * 상대 프로필의 ‘연결’: 사람 신뢰 스위치와 지금 연결 방식. 열린 동안 몇 초마다 다시 읽습니다.
 * 신뢰를 켤 수 없는 사람(함께 쓰는 공간이 없음, 만료된 연락처뿐)에게는 숨기되, 이미 켜 둔 신뢰는 끌 수 있게 남깁니다.
 */
export function TrustSection({ user, name }: { user: UserId; name: string }) {
  const client = useStore((s) => s.client);
  const setTrust = useStore((s) => s.setTrust);
  const eligible = useStore((s) => canTrustUser(s.me, user, s.views, s.friends));
  const views = useStore((s) => s.views);
  const spaces = useStore((s) => s.spaces);
  const [view, setView] = useState<TrustView | null>(null);
  const [busy, setBusy] = useState(false);
  // 바꾸면 바로 다시 읽고 주기도 새로 셉니다.
  const [nonce, setNonce] = useState(0);

  useEffect(() => {
    if (!client) return;
    let stale = false;
    let unsupported = false;
    const tick = async () => {
      if (unsupported) return;
      try {
        const v = await client.call('trust.get', { user });
        if (!stale) setView(v);
      } catch (e) {
        // 신뢰 스위치 전 데몬이면 구역을 숨긴 채 다시 묻지 않습니다.
        if (e instanceof RpcError && e.code === ErrorCode.MethodNotFound) unsupported = true;
      }
    };
    void tick();
    const id = setInterval(() => void tick(), TRUST_POLL_MS);
    return () => {
      stale = true;
      clearInterval(id);
    };
  }, [client, user, nonce]);

  if (!view || view.user !== user || (!eligible && !view.trusted)) return null;

  const toggle = async (enabled: boolean) => {
    setBusy(true);
    const ok = await setTrust({ type: 'user', user, name }, enabled);
    setBusy(false);
    if (!ok) return;
    setView((v) => (v ? { ...v, trusted: enabled } : v));
    setNonce((n) => n + 1);
  };
  const crewName = (s: SpaceId) => views[s]?.settings.name ?? spaces.find((x) => x.space === s)?.name ?? t('알 수 없는 크루');
  const via = view.via_crews.map(crewName);

  return (
    <div className="pf-section trust-section">
      <div className="pf-label">
        <Network size={12} /> {t('연결')}
      </div>
      <div className="row between gap">
        <div className="grow">
          <div className="row gap-s strong">
            <Handshake size={14} /> {t('신뢰')}
          </div>
          <div className="muted small">
            {eligible
              ? t('켜면 이 사람이 이 기기의 IP 주소를 볼 수 있습니다. 서로 신뢰하면 P2P로 직접, 한쪽만 신뢰하면 단방향으로 연결합니다.')
              : t('이제 함께 쓰는 공간이 없어 끄면 다시 켤 수 없습니다.')}
          </div>
        </div>
        <Toggle checked={view.trusted} onChange={(v) => void toggle(v)} disabled={busy} label={t('신뢰')} />
      </div>
      <div className="kv">
        <span>{t('연결 방식')}</span>
        <span className="trust-connection">{connectionLabel(view.connection)}</span>
      </div>
      {via.length > 0 && (
        <div className="warn-box small" role="note">
          <AlertTriangle size={14} />
          <div>
            {t('크루 신뢰로 이 사람에게 이미 이 기기의 IP가 보입니다: {crews}. 이 스위치를 꺼도 그 크루의 신뢰를 끄기 전까지는 계속 보입니다.', {
              crews: via.map((n) => `「${n}」`).join(', '),
            })}
          </div>
        </div>
      )}
    </div>
  );
}

/** 크루 설정(개요)의 ‘이 크루 신뢰’(`SpaceView.trusted`, `trust.set_space`). 켤 때는 경고를 거칩니다. */
export function CrewTrustField({ space }: { space: SpaceId }) {
  const view = useStore((s) => s.views[space]);
  const setTrust = useStore((s) => s.setTrust);
  const [busy, setBusy] = useState(false);
  // 신뢰 스위치 전 데몬은 trusted를 보내지 않습니다(켜도 거절되므로 숨김).
  if (!view || view.kind !== 'crew' || typeof view.trusted !== 'boolean') return null;
  return (
    <Field
      label={
        <span className="row gap-s">
          <Handshake size={13} /> {t('이 크루 신뢰')}
        </span>
      }
      hint={t('켜면 이 크루의 멤버가 이 기기의 IP 주소를 볼 수 있습니다. 멤버도 나를 신뢰하면 P2P로 직접, 한쪽만 신뢰하면 단방향으로 연결합니다. 이 기기에만 적용됩니다.')}
    >
      <Toggle
        checked={view.trusted}
        disabled={busy}
        label={t('이 크루 신뢰')}
        onChange={async (enabled) => {
          setBusy(true);
          await setTrust({ type: 'space', space, name: view.settings.name }, enabled);
          setBusy(false);
        }}
      />
    </Field>
  );
}
