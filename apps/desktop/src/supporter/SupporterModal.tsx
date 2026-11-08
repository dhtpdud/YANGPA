// SPDX-License-Identifier: LicenseRef-YANGPA-Supporter
// 후원자 확인: 라이브러리 탭·콘텐츠 뷰어는 GitHub Sponsors·Patreon 정기 후원자와 GitHub 3달러 이상 1회 후원자(30일) 전용입니다. 사용자 설정의 '후원' 탭과
// 잠긴 기능을 눌렀을 때 뜨는 창이 같은 패널을 씁니다. 로그인·확인은 데몬이 하고(supporter.*, GitHub는 Tor로), 끝나면
// supporter.updated가 와서 스토어의 상태가 바뀝니다(실패 까닭은 토스트).

import { Copy, Heart, Info, RefreshCw } from 'lucide-react';
import { useEffect, useState } from 'react';
import type { SupporterProvider } from '../api/types';
import { Spinner, copyText } from '../components/common';
import { t } from '../i18n';
import { formatFull, formatRelative } from '../lib/format';
import { cx } from '../lib/misc';
import { isAndroid } from '../lib/platform';
import { PROVIDER_NAMES, SPONSOR_URLS } from './supporter';
import { useStore } from '../store/store';
import { Modal } from '../modals/Modal';

export function SupporterPanel() {
  const client = useStore((s) => s.client);
  const call = useStore((s) => s.call);
  const status = useStore((s) => s.supporter);
  const refreshSupporter = useStore((s) => s.refreshSupporter);
  const [code, setCode] = useState<string | null>(null);
  const [waiting, setWaiting] = useState<SupporterProvider | null>(null);
  useEffect(() => {
    void refreshSupporter();
  }, [refreshSupporter]);
  // 로그인이 끝나 상태가 다시 오면 기다림을 풉니다.
  useEffect(() => {
    setWaiting(null);
    setCode(null);
  }, [status]);

  // 내 다른 기기가 확인해 보내 준 결과 중 가장 오래 가는 것(옛 데몬은 `shared`가 없음).
  const fromOther = (status?.shared ?? []).filter((s) => s.active).sort((a, b) => b.valid_until - a.valid_until)[0];
  const open = (url: string) => void client?.transport.openExternal(url);
  const login = async (p: SupporterProvider) => {
    if (!client) return;
    if (p === 'github') {
      const r = await call(client.call('supporter.github_begin', {}));
      if (!r) return;
      setCode(r.user_code);
      setWaiting(p);
      open(r.verification_uri);
    } else {
      const r = await call(client.call('supporter.patreon_begin', {}));
      if (!r) return;
      setWaiting(p);
      open(r.url);
    }
  };
  const recheck = async () => {
    if (!client) return;
    const r = await call(client.call('supporter.refresh', {}));
    if (r) useStore.setState({ supporter: r });
  };
  const unlink = async (provider: SupporterProvider) => {
    if (!client) return;
    await call(client.call('supporter.unlink', { provider }));
  };

  return (
    <div className="stack">
      <h3>{t('후원')}</h3>
      <p className="muted small">
        {/* Play 결제 정책: 앱 안에서 다른 결제로 안내하지 않습니다(docs/android.md). */}
        {isAndroid()
          ? t('라이브러리 탭과 콘텐츠 뷰어는 후원자 전용입니다. 이미 후원한 계정이 있으면 아래에서 로그인해 확인하세요.')
          : t(
              '라이브러리 탭과 콘텐츠 뷰어는 후원자 전용입니다. GitHub Sponsors나 Patreon에서 월 정기 후원을 하거나 GitHub Sponsors에서 3달러 이상 1회 후원을 한 뒤, 후원한 계정으로 아래에서 확인하세요. 하루에 한 번 다시 확인하고, 구독이 끝나거나 1회 후원 뒤 30일이 지나면 다시 잠깁니다.',
            )}
      </p>
      {status?.grace_until && (
        <div className="warn-box">
          <Info size={16} />
          <div>{t('기존 사용자는 {date}까지 후원 없이 쓸 수 있습니다.', { date: formatFull(status.grace_until) })}</div>
        </div>
      )}
      {fromOther && (
        <div className="warn-box">
          <Info size={16} />
          <div>
            {t('내 다른 기기에서 확인한 후원으로 열려 있습니다({login}, {date}까지). 이 기기는 따로 로그인하지 않아도 됩니다.', {
              login: fromOther.login || '-',
              date: formatFull(fromOther.valid_until),
            })}
          </div>
        </div>
      )}
      {status === null ? (
        <Spinner />
      ) : status.providers.length === 0 ? (
        <p className="muted small">{t('이 빌드는 후원을 확인하지 않아 모든 기능이 열려 있습니다.')}</p>
      ) : (
        status.providers.map((p) => {
          const link = status.links.find((l) => l.provider === p);
          return (
            <div key={p} className="card stack">
              <div className="row between wrap gap">
                <strong>{PROVIDER_NAMES[p]}</strong>
                {link && (
                  <span className={cx('tag', link.active && 'ok')}>
                    {link.needs_login ? t('다시 로그인 필요') : link.active ? t('후원 확인됨') : t('유효한 후원 없음')}
                  </span>
                )}
              </div>
              {link && (
                <div className="muted small">
                  {t('{login} · 마지막 확인 {when}', { login: link.login || '-', when: formatRelative(link.checked_at) })}
                  {link.active && ` · ${t('{date}까지 유효', { date: formatFull(link.valid_until) })}`}
                </div>
              )}
              <div className="row gap-s wrap">
                {SPONSOR_URLS[p] && !isAndroid() && (
                  <button type="button" className="btn small" onClick={() => open(SPONSOR_URLS[p])}>
                    <Heart size={14} /> {t('후원하러 가기')}
                  </button>
                )}
                <button type="button" className="btn small primary" disabled={waiting === p} onClick={() => void login(p)}>
                  {link ? t('다시 로그인') : p === 'github' ? t('GitHub로 확인') : t('Patreon으로 확인')}
                </button>
                {link && !link.needs_login && (
                  <button type="button" className="btn small" onClick={() => void recheck()}>
                    <RefreshCw size={14} /> {t('지금 다시 확인')}
                  </button>
                )}
                {link && (
                  <button type="button" className="btn small ghost" onClick={() => void unlink(p)}>
                    {t('연결 해제')}
                  </button>
                )}
              </div>
              {waiting === p && code && (
                <div className="stack">
                  <div className="muted small">{t('브라우저에 열린 GitHub 페이지에 이 코드를 넣고 승인하세요.')}</div>
                  <div className="row gap-s">
                    <code className="mono">{code}</code>
                    <button type="button" className="icon-btn" title={t('복사')} onClick={() => void copyText(code)}>
                      <Copy size={14} />
                    </button>
                  </div>
                </div>
              )}
              {waiting === p && (
                <div className="row gap-s muted small">
                  <Spinner size={14} /> {t('브라우저에서 승인을 기다리는 중…')}
                </div>
              )}
            </div>
          );
        })
      )}
      <p className="muted small">
        {t(
          '로그인은 GitHub·Patreon 계정의 프로필과 후원 정보만 읽고, YANGPA 신원은 보내지 않습니다. 내 기기 중 한 대에서만 로그인하면 확인 결과가 내 다른 기기에도 전해집니다(Patreon은 한 계정을 기기 하나에만 연결할 수 있어, 여러 기기에서 로그인하면 먼저 연결한 기기가 끊깁니다).',
        )}
      </p>
    </div>
  );
}

export function SupporterModal({ onClose }: { onClose: () => void }) {
  const active = useStore((s) => s.supporter?.active);
  // 확인되면 창을 닫습니다(누른 기능은 다시 누르면 열림).
  useEffect(() => {
    if (active) onClose();
  }, [active, onClose]);
  return (
    <Modal title={t('후원자 전용 기능')} onClose={onClose}>
      <SupporterPanel />
    </Modal>
  );
}
