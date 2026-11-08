// 크루 설정 → 시더 탭의 보관 전용 노드 목록·지정·해제. 보관 노드는 멤버가 아니라 내용을 읽을 수 없고
// 암호문만 받아 두었다가 현재 멤버에게만 내줍니다(docs/protocol.md "보관 전용 노드").
// 지정 링크는 1회용 비밀이라 화면 상태에만 두고 웹뷰 저장소에는 쓰지 않습니다.

import { Archive, Plus, Trash2 } from 'lucide-react';
import { useCallback, useEffect, useState } from 'react';
import type { ArchiveView } from '../api/types';
import { ChannelIcon, Spinner, isPrivateChannel } from '../components/common';
import { t } from '../i18n';
import { formatFull } from '../lib/format';
import { cx, hasPerm } from '../lib/misc';
import { useStore } from '../store/store';
import { Field } from './Modal';

export function ArchiveNodes({ space }: { space: string }) {
  const view = useStore((s) => s.views[space])!;
  const client = useStore((s) => s.client);
  const call = useStore((s) => s.call);
  const [list, setList] = useState<ArchiveView[] | null>(null);
  const [adding, setAdding] = useState(false);
  const [url, setUrl] = useState('');
  const [name, setName] = useState('');
  const [picked, setPicked] = useState<string[]>([]);
  const [busy, setBusy] = useState(false);
  const canManage = hasPerm(view, 'manage_space');

  const load = useCallback(() => {
    if (!client) return;
    client
      .call('archive.list', { space })
      .then(setList)
      .catch(() => setList([]));
  }, [client, space]);
  useEffect(() => {
    load();
    const timer = setInterval(load, 5000);
    return () => clearInterval(timer);
  }, [load]);

  const privateChannels = view.channels.filter(
    (c) => isPrivateChannel(c) && c.settings.security !== 'volatile' && c.settings.kind !== 'category' && c.settings.kind !== 'voice',
  );
  const channelName = (id: string) => view.channels.find((c) => c.channel === id)?.settings.name ?? '?';

  const designate = async () => {
    if (!client || !url.trim()) return;
    setBusy(true);
    const r = await call(
      client.call('archive.designate', {
        space,
        url: url.trim(),
        private_channels: picked.length > 0 ? picked : undefined,
        name: name.trim() || undefined,
      }),
      { ok: t('보관 노드를 지정했습니다.') },
    );
    setBusy(false);
    if (!r) return;
    setUrl('');
    setName('');
    setPicked([]);
    setAdding(false);
    load();
  };

  const revoke = async (a: ArchiveView) => {
    if (!client) return;
    const r = await call(client.call('archive.revoke', { space, device: a.device }), { ok: t('보관 노드 지정을 해제했습니다.') });
    if (r !== undefined) load();
  };

  return (
    <div className="stack">
      <h3>{t('보관 전용 노드')}</h3>
      <p className="muted small">
        {t(
          '보관 노드는 복호화 키 없이 암호화된 기록과 첨부만 받아 두었다가, 모두가 꺼져 있을 때 현재 멤버에게 내주는 기기입니다. 멤버가 아니라 내용을 읽을 수 없지만, 누가 멤버인지·언제 얼마나 말했는지 같은 메타데이터는 봅니다. 공개 채널은 늘 보관하고 비공개 채널은 고른 것만 보관합니다.',
        )}
      </p>
      <div className="table">
        {list === null && <Spinner />}
        {list?.length === 0 && <div className="muted small">{t('지정된 보관 노드가 없습니다.')}</div>}
        {list?.map((a) => (
          <div key={a.device} className="tr">
            <Archive size={16} />
            <span className="grow">
              <strong>{a.name}</strong>
              <span className="muted small">
                {' '}
                · {t('{when} 지정', { when: formatFull(a.granted_at) })}
                {a.private_channels.length > 0 &&
                  ' · ' +
                    t('비공개 채널 {list}', { n: a.private_channels.length, list: a.private_channels.map((c) => `#${channelName(c)}`).join(', ') })}
              </span>
            </span>
            <span className={cx('seed-count', !a.online && 'zero')}>{a.online ? t('온라인') : t('오프라인')}</span>
            {canManage && (
              <button type="button" className="btn small danger-outline" title={t('지정 해제')} onClick={() => void revoke(a)}>
                <Trash2 size={13} /> {t('해제')}
              </button>
            )}
          </div>
        ))}
      </div>
      {canManage && !adding && (
        <button type="button" className="btn small" onClick={() => setAdding(true)}>
          <Plus size={13} /> {t('보관 노드 지정')}
        </button>
      )}
      {canManage && adding && (
        <div className="card stack">
          <Field label={t('지정 링크')} hint={t('보관 노드에서 yangpa archive offer로 만든 1회용 링크')}>
            <input
              className="input"
              value={url}
              autoFocus
              spellCheck={false}
              autoComplete="off"
              placeholder="yangpa://archive/…"
              onChange={(e) => setUrl(e.target.value)}
            />
          </Field>
          <Field label={t('이름(선택)')}>
            <input className="input" value={name} maxLength={64} placeholder={t('링크에 적힌 이름')} onChange={(e) => setName(e.target.value)} />
          </Field>
          {privateChannels.length > 0 && (
            // Field는 <label>이라 체크박스 label을 안에 넣지 않습니다.
            <div className="field">
              <span className="field-label">{t('함께 보관할 비공개 채널')}</span>
              <div className="stack">
                {privateChannels.map((c) => (
                  <label key={c.channel} className="check small">
                    <input
                      type="checkbox"
                      checked={picked.includes(c.channel)}
                      onChange={(e) =>
                        setPicked(e.target.checked ? [...picked, c.channel] : picked.filter((x) => x !== c.channel))
                      }
                    />
                    <ChannelIcon channel={c} size={14} /> {c.settings.name}
                  </label>
                ))}
              </div>
            </div>
          )}
          <div className="row">
            <button type="button" className="btn ghost small" onClick={() => setAdding(false)}>
              {t('취소')}
            </button>
            <button type="button" className="btn primary small" disabled={busy || !url.trim()} onClick={() => void designate()}>
              {busy && <Spinner size={14} />} {t('지정')}
            </button>
          </div>
        </div>
      )}
    </div>
  );
}
