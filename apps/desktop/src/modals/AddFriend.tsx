// 연락처 교환: 내 연락처 링크 만들기(1회용·만료) + 받은 링크 붙여 넣기 + 내 링크 목록.
// 링크를 붙여 넣으면 링크 주인의 승인 없이 바로 1대1 대화가 열리고, 링크가 만료될 때까지 서로 임시 연락처로 남습니다.

import { Clipboard, Copy, Link2, Trash2 } from 'lucide-react';
import { useEffect, useState } from 'react';
import type { FriendLinkView } from '../api/types';
import { NoAddressWarning, copyText } from '../components/common';
import { t } from '../i18n';
import { formatFull, formatRelative } from '../lib/format';
import { linkLacksAddress } from '../lib/misc';
import { useStore } from '../store/store';
import { Modal } from './Modal';

const EXPIRY: Array<[string, number | undefined]> = [
  [t('1시간'), 3600],
  [t('1일'), 86_400],
  [t('7일'), 7 * 86_400],
  [t('만료 없음'), undefined],
];

/** `url`: 웹 초대 페이지에서 연 연락처 링크(받은 링크 칸에 채움). */
export function AddFriendPanel({ url }: { url?: string }) {
  const client = useStore((s) => s.client);
  const call = useStore((s) => s.call);
  const toast = useStore((s) => s.toast);
  const openModal = useStore((s) => s.openModal);
  const refreshFriends = useStore((s) => s.refreshFriends);
  const refreshSpaces = useStore((s) => s.refreshSpaces);
  const [expiry, setExpiry] = useState(1);
  const [single, setSingle] = useState(true);
  const [created, setCreated] = useState<{ url: string; expires_at?: number; noAddress: boolean } | null>(null);
  const [paste, setPaste] = useState(url ?? '');
  const [links, setLinks] = useState<FriendLinkView[]>([]);
  const [busy, setBusy] = useState(false);

  const loadLinks = async () => {
    if (!client) return;
    const l = await client.call('friend.link.list', {}).catch(() => null);
    if (l) setLinks(l);
  };
  useEffect(() => {
    if (!client) return;
    let stale = false;
    client
      .call('friend.link.list', {})
      .then((l) => !stale && setLinks(l))
      .catch(() => undefined);
    return () => {
      stale = true;
    };
  }, [client]);

  const create = async () => {
    if (!client) return;
    setBusy(true);
    const r = await call(client.call('friend.link.create', { expires_in_secs: EXPIRY[expiry][1], single_use: single }));
    const st = r ? await client.call('net.status', {}).catch(() => null) : null;
    setBusy(false);
    if (r) {
      setCreated({ url: r.url, expires_at: r.expires_at, noAddress: linkLacksAddress(st) });
      void loadLinks();
    }
  };

  const add = async () => {
    if (!client || !paste.trim()) return;
    setBusy(true);
    const r = await call(client.call('friend.add', { url: paste.trim() }));
    setBusy(false);
    if (r) {
      setPaste('');
      toast('success', t('연락처 링크로 대화를 시작합니다. 상대에게 닿으면 임시 연락처에 나타납니다.'));
      await Promise.all([refreshFriends(), refreshSpaces()]);
    }
  };

  // 폐기하면 그 링크로 만난 임시 연락처도 만료되므로 확인받습니다.
  const revoke = (l: FriendLinkView) =>
    openModal({
      kind: 'confirm',
      title: t('연락처 링크 폐기'),
      body:
        l.uses > 0
          ? t('이 링크로는 더 대화를 시작할 수 없고, 이 링크로 만나 아직 친구가 되지 않은 임시 연락처({n}명)도 만료됩니다. 만료된 대화는 읽기만 할 수 있습니다.', { n: l.uses })
          : t('이 링크로는 더 대화를 시작할 수 없고, 이 링크로 만나 아직 친구가 되지 않은 임시 연락처도 만료됩니다. 만료된 대화는 읽기만 할 수 있습니다.'),
      danger: true,
      confirmLabel: t('폐기'),
      onConfirm: async () => {
        if (!client) return;
        await call(client.call('friend.link.revoke', { token_id: l.token_id }), { ok: t('링크를 폐기했습니다') });
        void loadLinks();
        await Promise.all([refreshFriends(), refreshSpaces()]);
      },
    });

  return (
    <div className="add-friend">
      <section className="card">
        <h3>
          <Link2 size={18} /> {t('내 연락처 링크 만들기')}
        </h3>
        <p className="muted small">
          {t('링크를 메신저 밖(문자·이메일)으로 보내세요. 링크를 붙여 넣은 사람과 바로 1대1 대화가 열리고, 링크가 만료될 때까지 서로 임시 연락처로 남습니다. 그동안 한쪽이 친구 추가를 보내고 상대가 수락하면 친구가 됩니다.')}
        </p>
        <div className="row gap">
          <div className="seg">
            {EXPIRY.map(([label], i) => (
              <button key={label} type="button" className={i === expiry ? 'active' : ''} onClick={() => setExpiry(i)}>
                {label}
              </button>
            ))}
          </div>
          <label className="check">
            <input type="checkbox" checked={single} onChange={(e) => setSingle(e.target.checked)} /> {t('한 번만 쓰기')}
          </label>
          <button type="button" className="btn primary" disabled={busy} onClick={() => void create()}>
            {t('링크 만들기')}
          </button>
        </div>
        {created && (
          <div className="link-box">
            <code className="mono">{created.url}</code>
            <button
              type="button"
              className="btn small"
              onClick={async () => {
                if (await copyText(created.url)) toast('success', t('링크를 복사했습니다'));
              }}
            >
              <Copy size={14} /> {t('복사')}
            </button>
            <span className="muted small">{created.expires_at ? t('{time}까지', { time: formatFull(created.expires_at) }) : t('만료 없음')}</span>
          </div>
        )}
        {created?.noAddress && <NoAddressWarning />}
      </section>
      <section className="card">
        <h3>
          <Clipboard size={18} /> {t('받은 연락처 링크로 대화 시작')}
        </h3>
        <div className="row gap">
          <input
            className="input mono grow"
            value={paste}
            placeholder={t('받은 연락처 링크')}
            onChange={(e) => setPaste(e.target.value)}
            onKeyDown={(e) => e.key === 'Enter' && void add()}
          />
          <button
            type="button"
            className="btn"
            title={t('클립보드에서 붙여넣기')}
            onClick={async () => {
              try {
                setPaste((await navigator.clipboard.readText()).trim().slice(0, 2000));
              } catch {
                toast('warn', t('클립보드를 읽을 수 없습니다. 직접 붙여 넣으세요.'));
              }
            }}
          >
            {t('붙여넣기')}
          </button>
          <button type="button" className="btn primary" disabled={busy || !paste.trim()} onClick={() => void add()}>
            {t('대화 시작')}
          </button>
        </div>
      </section>
      <section className="card">
        <h3>{t('내 연락처 링크')}</h3>
        {links.length === 0 && <p className="muted small">{t('만든 링크가 없습니다.')}</p>}
        {links.map((l) => (
          <div key={l.token_id} className="list-row">
            <span className="mono small">{l.token_id}</span>
            <span className="muted small">
              {t('{time} 만듦', { time: formatRelative(l.created_at) })} · {l.single_use ? t('1회용') : t('여러 번')} · {t('사용 {n}회', { n: l.uses })} ·{' '}
              {l.expires_at ? t('{time} 만료', { time: formatFull(l.expires_at) }) : t('만료 없음')}
            </span>
            <button
              type="button"
              className="icon-btn"
              title={t('폐기')}
              onClick={() => revoke(l)}
            >
              <Trash2 size={16} />
            </button>
          </div>
        ))}
      </section>
    </div>
  );
}

export function AddFriendModal({ url, onClose }: { url?: string; onClose: () => void }) {
  return (
    <Modal title={t('연락처 교환')} onClose={onClose} size="lg">
      <AddFriendPanel url={url} />
    </Modal>
  );
}

