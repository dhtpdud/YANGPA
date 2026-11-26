// SPDX-License-Identifier: LicenseRef-YANGPA-Supporter
// 크루 구독자 역할 화면. 관리자: 역할 편집 안에서 경로를 고르고 이 기기에 크리에이터 계정을 연결합니다(subrole.creator_*,
// crew.set_subscriber_roles). 멤버: 크루 메뉴의 '구독 후원 확인' 창에서 구독한 계정을 증명합니다(subrole.github_begin 등).
// 확인·역할 지정은 데몬이 하고, 끝나면 subrole.updated(토스트 + subroleTick)가 옵니다.

import { Copy, Info, RefreshCw } from 'lucide-react';
import { useEffect, useState } from 'react';
import type { CreatorStatus, Role, SpaceId, SubscriptionProvider } from '../api/types';
import { EmojiText, Spinner, copyText } from '../components/common';
import { t } from '../i18n';
import { cx, hasPerm, roleColor } from '../lib/misc';
import { Modal } from '../modals/Modal';
import { useStore } from '../store/store';
import { SUBSCRIPTION_NAMES } from './subrole';

const ADMINISTRATOR = 0x8000_0000;

/** GitHub 기기 코드 로그인 안내(코드 + 복사). */
function DeviceCode({ code }: { code: string }) {
  return (
    <div className="stack">
      <div className="muted small">{t('브라우저에 열린 GitHub 페이지에 이 코드를 넣고 승인하세요.')}</div>
      <div className="row gap-s">
        <code className="mono">{code}</code>
        <button type="button" className="icon-btn" title={t('복사')} onClick={() => void copyText(code)}>
          <Copy size={14} />
        </button>
      </div>
    </div>
  );
}

function Waiting() {
  return (
    <div className="row gap-s muted small">
      <Spinner size={14} /> {t('브라우저에서 승인을 기다리는 중…')}
    </div>
  );
}

/** 역할 편집 안: 이 역할을 구독자 역할로 쓸 경로. 역할 관리 권한(`manage_roles`)이 있을 때만 보입니다. */
export function RoleSubscriberSection({ space, role, canManage }: { space: SpaceId; role: Role; canManage: boolean }) {
  const view = useStore((s) => s.views[space]);
  if (!view || view.kind !== 'crew' || !hasPerm(view, 'manage_roles')) return null;
  return <RoleSubscriberEdit space={space} role={role} canManage={canManage} />;
}

function RoleSubscriberEdit({ space, role, canManage }: { space: SpaceId; role: Role; canManage: boolean }) {
  const view = useStore((s) => s.views[space])!;
  const myDevice = useStore((s) => s.myDevice);
  const client = useStore((s) => s.client);
  const call = useStore((s) => s.call);
  const tick = useStore((s) => s.subroleTick);
  const [status, setStatus] = useState<CreatorStatus | null>(null);
  const [busy, setBusy] = useState(false);
  const [syncing, setSyncing] = useState(false);
  const [waiting, setWaiting] = useState<SubscriptionProvider | null>(null);
  const [code, setCode] = useState<string | null>(null);
  const [sellapp, setSellapp] = useState<{ store: string; key: string } | null>(null);
  useEffect(() => {
    if (!client) return;
    void call(client.call('subrole.creator_status', {})).then((r) => r && setStatus(r));
    // 연결이 끝나면(subrole.updated) 기다림을 풉니다.
    setWaiting(null);
    setCode(null);
  }, [client, call, tick]);

  const all = view.subscriber_roles ?? [];
  const mine = all.find((s) => s.role === role.id);
  const admin = (role.permissions & ADMINISTRATOR) !== 0;
  if (status && status.providers.length === 0) return null;

  // 목록 전체를 보냅니다(경로가 빈 항목은 데몬이 뺌). 바뀐 항목은 이 기기가 확인을 맡습니다.
  const setProviders = async (providers: SubscriptionProvider[]) => {
    if (!client) return;
    const roles = all.filter((s) => s.role !== role.id).map((s) => ({ role: s.role, providers: s.providers }));
    if (providers.length) roles.push({ role: role.id, providers });
    setBusy(true);
    await call(client.call('crew.set_subscriber_roles', { space, roles }), { ok: t('구독자 역할을 바꿨습니다') });
    setBusy(false);
  };
  const toggle = (p: SubscriptionProvider, on: boolean) => {
    const cur = mine?.providers ?? [];
    void setProviders(on ? [...cur, p] : cur.filter((x) => x !== p));
  };
  const connect = async (p: SubscriptionProvider) => {
    if (!client) return;
    if (p === 'github') {
      const r = await call(client.call('subrole.creator_github_begin', {}));
      if (!r) return;
      setCode(r.user_code);
      setWaiting(p);
      void client.transport.openExternal(r.verification_uri);
    } else if (p === 'patreon') {
      const r = await call(client.call('subrole.creator_patreon_begin', {}));
      if (!r) return;
      setWaiting(p);
      void client.transport.openExternal(r.url);
    } else {
      setSellapp({ store: '', key: '' });
    }
  };
  const saveSellapp = async () => {
    if (!client || !sellapp) return;
    setBusy(true);
    const r = await call(client.call('subrole.creator_sellapp', { store: sellapp.store.trim(), key: sellapp.key.trim() }));
    setBusy(false);
    if (r) setSellapp(null);
  };
  const unlink = (p: SubscriptionProvider) => client && void call(client.call('subrole.creator_unlink', { provider: p }));
  // 하루 한 번 맞추기를 기다리지 않고 이 기기가 확인하는 이 크루의 역할을 지금 맞춥니다.
  const syncNow = async () => {
    if (!client) return;
    setSyncing(true);
    await call(client.call('crew.sync_subscriber_roles', { space }), { ok: t('구독자 역할을 업데이트했습니다') });
    setSyncing(false);
  };

  return (
    <div className="perm-group">
      <div className="perm-title">{t('구독자 역할')}</div>
      <span className="field-hint">
        {t(
          '고른 경로에서 내 크리에이터 계정을 정기 구독하는 멤버가 크루 메뉴의 ‘구독 후원 확인’으로 증명하면 이 역할을 자동으로 주고, 구독이 끝나면 뺍니다. 확인은 설정한 기기가 하루에 한 번 하므로 그 기기가 켜져 있어야 합니다. 손으로 준 역할은 빼지 않습니다.',
        )}
      </span>
      {admin ? (
        <span className="muted small">{t('관리자 권한이 있는 역할은 구독자 역할로 쓸 수 없습니다.')}</span>
      ) : status === null ? (
        <Spinner size={14} />
      ) : (
        <>
          {mine && (
            <div className="warn-box small">
              <Info size={14} />
              <div>
                {mine.holder === myDevice
                  ? t('이 기기가 구독을 확인하고 역할을 주고 뺍니다.')
                  : t('다른 관리자 기기가 구독을 확인합니다. 경로를 바꾸면 이 기기가 맡습니다.')}
              </div>
            </div>
          )}
          {mine?.holder === myDevice && (
            <div className="row gap-s wrap">
              <button type="button" className="btn small" disabled={syncing} onClick={() => void syncNow()}>
                {syncing ? <Spinner size={12} /> : <RefreshCw size={13} />} {t('구독자 정보 지금 업데이트하기')}
              </button>
              <span className="muted small">{t('구독을 증명한 멤버를 지금 다시 확인해 역할을 주고 뺍니다.')}</span>
            </div>
          )}
          {status.providers.map((p) => {
            const link = status.links.find((l) => l.provider === p);
            const on = !!mine?.providers.includes(p);
            const usable = !!link && !link.needs_login;
            return (
              <div key={p} className="stack">
                <div className="perm-row">
                  <span>
                    {SUBSCRIPTION_NAMES[p]}{' '}
                    <span className="muted small">
                      {link ? (link.needs_login ? t('다시 연결 필요') : link.name || t('연결됨')) : t('이 기기에 연결 안 됨')}
                    </span>
                  </span>
                  <span className="row gap-s">
                    <button type="button" className="btn small" disabled={waiting === p} onClick={() => void connect(p)}>
                      {link ? t('다시 연결') : t('연결')}
                    </button>
                    {link && (
                      <button type="button" className="btn small ghost" onClick={() => unlink(p)}>
                        {t('연결 해제')}
                      </button>
                    )}
                    <input
                      type="checkbox"
                      aria-label={t('{provider} 구독자에게 주기', { provider: SUBSCRIPTION_NAMES[p] })}
                      checked={on}
                      disabled={busy || !canManage || (!on && !usable)}
                      onChange={(e) => toggle(p, e.target.checked)}
                    />
                  </span>
                </div>
                {waiting === p && code && p === 'github' && <DeviceCode code={code} />}
                {waiting === p && <Waiting />}
                {p === 'sellapp' && sellapp && (
                  <div className="row gap-s wrap">
                    <input
                      className="input"
                      aria-label={t('SellApp 상점')}
                      // 상점 슬러그: mystore.sell.app의 mystore(X-STORE 헤더).
                      placeholder="mystore (mystore.sell.app)"
                      value={sellapp.store}
                      onChange={(e) => setSellapp({ ...sellapp, store: e.target.value })}
                    />
                    <input
                      className="input grow"
                      type="password"
                      autoComplete="off"
                      aria-label={t('SellApp API 키')}
                      placeholder={t('SellApp API 키')}
                      value={sellapp.key}
                      onChange={(e) => setSellapp({ ...sellapp, key: e.target.value })}
                    />
                    <button type="button" className="btn small primary" disabled={busy || !sellapp.store.trim() || !sellapp.key.trim()} onClick={() => void saveSellapp()}>
                      {t('저장')}
                    </button>
                    <button type="button" className="btn small ghost" onClick={() => setSellapp(null)}>
                      {t('취소')}
                    </button>
                  </div>
                )}
              </div>
            );
          })}
          <span className="field-hint">
            {t('크리에이터 계정의 토큰·키는 이 기기에만 저장되고, 이 기기에서 구독자 역할을 쓰는 모든 크루가 함께 씁니다.')}
          </span>
        </>
      )}
    </div>
  );
}

/** 크루 메뉴의 '구독 후원 확인': 구독한 계정을 증명해 구독자 역할을 받습니다. */
export function SubroleModal({ space, onClose }: { space: SpaceId; onClose: () => void }) {
  const view = useStore((s) => s.views[space]);
  const me = useStore((s) => s.me);
  const client = useStore((s) => s.client);
  const call = useStore((s) => s.call);
  const tick = useStore((s) => s.subroleTick);
  const [waiting, setWaiting] = useState<SubscriptionProvider | null>(null);
  const [inBrowser, setInBrowser] = useState(false);
  // 이 기기의 Patreon 크리에이터 연결로 확인했으면(주소 없이 옴) '다른 계정' 선택지를 보입니다.
  const [patreonDirect, setPatreonDirect] = useState(false);
  const [key, setKey] = useState('');
  useEffect(() => setWaiting(null), [tick]);

  const subs = view?.subscriber_roles ?? [];
  const providers = (['github', 'patreon', 'sellapp'] as const).filter((p) => subs.some((s) => s.providers.includes(p)));
  const myRoles = view?.members.find((m) => m.user === me)?.roles ?? [];

  const prove = async (p: SubscriptionProvider, login = false) => {
    if (!client) return;
    // 결과(subrole.updated)가 호출 답보다 먼저 올 수 있어 기다림을 먼저 켜고, 호출이 실패하면 풉니다.
    setWaiting(p);
    if (p === 'sellapp') {
      if (!(await call(client.call('subrole.sellapp', { space, key: key.trim() })))) setWaiting(null);
      return;
    }
    const r =
      p === 'github'
        ? await call(client.call('subrole.github_begin', { space }))
        : await call(client.call('subrole.patreon_begin', { space, login }));
    if (!r) {
      setWaiting(null);
      return;
    }
    setInBrowser(!!r.url);
    if (r.url) void client.transport.openExternal(r.url);
    else setPatreonDirect(true);
  };

  return (
    <Modal title={t('구독 후원 확인')} onClose={onClose}>
      <div className="stack">
        <p className="muted small">
          {t('이 크루는 아래 경로의 정기 구독자에게 역할을 줍니다. 구독한 계정으로 확인하면 크루 관리자의 기기가 확인해 역할을 주고, 구독이 끝나면 뺍니다.')}
        </p>
        {subs.length === 0 ? (
          <p className="muted small">{t('이 크루에는 구독자 역할이 없습니다.')}</p>
        ) : (
          <div className="row gap-s wrap">
            {subs.map((s) => {
              const role = view?.roles.find((r) => r.id === s.role);
              if (!role) return null;
              const has = myRoles.includes(s.role);
              return (
                <span key={s.role} className={cx('tag', has && 'ok')} style={{ color: roleColor(role.color) }}>
                  <EmojiText text={role.name} /> · {s.providers.map((p) => SUBSCRIPTION_NAMES[p]).join(', ')}
                  {has && ` · ${t('받음')}`}
                </span>
              );
            })}
          </div>
        )}
        {providers.map((p) => (
          <div key={p} className="card stack">
            <strong>{SUBSCRIPTION_NAMES[p]}</strong>
            {p === 'sellapp' ? (
              <div className="row gap-s">
                <input
                  className="input grow"
                  autoComplete="off"
                  aria-label={t('라이선스 키')}
                  placeholder={t('구독 상품의 라이선스 키')}
                  value={key}
                  onChange={(e) => setKey(e.target.value)}
                />
                <button type="button" className="btn small primary" disabled={waiting === p || !key.trim()} onClick={() => void prove(p)}>
                  {t('확인')}
                </button>
              </div>
            ) : (
              <div className="row gap-s">
                <button type="button" className="btn small primary" disabled={waiting === p} onClick={() => void prove(p)}>
                  {p === 'github' ? t('GitHub로 확인') : t('Patreon으로 확인')}
                </button>
                {p === 'patreon' && patreonDirect && (
                  <button type="button" className="btn small" disabled={waiting === p} onClick={() => void prove(p, true)}>
                    {t('다른 Patreon 계정으로 확인')}
                  </button>
                )}
              </div>
            )}
            {waiting === p && (p !== 'sellapp' && inBrowser ? <Waiting /> : <Spinner size={14} />)}
            {p === 'patreon' && patreonDirect && (
              <p className="muted small">
                {t(
                  '‘Patreon으로 확인’은 이 기기에 크리에이터로 연결한 Patreon 계정을 씁니다. 다른 계정으로 확인할 때 그 크리에이터 계정으로 로그인하면 이 기기의 크리에이터 연결이 끊길 수 있습니다.',
                )}
              </p>
            )}
          </div>
        ))}
        {providers.some((p) => p !== 'sellapp') && (
          <p className="muted small">
            {t(
              'GitHub 로그인은 두 번 이어질 수 있습니다. 첫 번째는 이 기기가 받는 로그인으로, 계정 확인보다 넓은 권한이 열리는 계정이면 관리자 기기에는 아무것도 보내지 않고 멈춥니다. Patreon은 이 기기가 직접 연결해 내 후원 목록을 읽으므로 Patreon이 내 IP를 봅니다(로그인 토큰은 이 기기를 떠나지 않음).',
            )}
          </p>
        )}
        <p className="muted small">
          {t(
            '확인할 때 이 크루에서 나만 쓸 수 있는 증명을 만들어 역할을 주는 관리자 기기로 보냅니다. GitHub는 한 번 쓰는 로그인 코드를, Patreon·SellApp은 후원 번호나 라이선스 키로 만든 값만 보냅니다(번호·키 자체는 보내지 않음). 그 기기는 내 GitHub 계정이나, 자기 Patreon·SellApp 후원자 가운데 누가 나인지 알게 됩니다. YANGPA 신원은 GitHub·Patreon·SellApp에 보내지 않습니다.',
          )}
        </p>
      </div>
    </Modal>
  );
}
