// 크루 만들기·초대로 참가·1대1을 크루로 전환.

import { Info, Users } from 'lucide-react';
import { useState } from 'react';
import { t } from '../i18n';
import { useStore } from '../store/store';
import { Field, Modal } from './Modal';

export function CreateCrewModal({ onClose }: { onClose: () => void }) {
  const client = useStore((s) => s.client);
  const call = useStore((s) => s.call);
  const refreshSpaces = useStore((s) => s.refreshSpaces);
  const selectSpace = useStore((s) => s.selectSpace);
  const [name, setName] = useState('');
  const [desc, setDesc] = useState('');
  const [busy, setBusy] = useState(false);
  const submit = async () => {
    if (!client || !name.trim()) return;
    setBusy(true);
    const r = await call(client.call('crew.create', { name: name.trim(), description: desc.trim() || undefined }));
    setBusy(false);
    if (!r) return;
    await refreshSpaces();
    await selectSpace(r.space);
    onClose();
  };
  return (
    <Modal
      title={t('크루 만들기')}
      onClose={onClose}
      footer={
        <>
          <button type="button" className="btn ghost" onClick={onClose}>
            {t('취소')}
          </button>
          <button type="button" className="btn primary" disabled={busy || !name.trim()} onClick={() => void submit()}>
            {t('만들기')}
          </button>
        </>
      }
    >
      <p className="muted">{t('크루는 서버 없이 멤버 기기끼리 이어지는 모임입니다. 만든 사람이 소유자가 되고, 역할·채널은 나중에 바꿀 수 있습니다.')}</p>
      <Field label={t('크루 이름')}>
        <input
          className="input"
          value={name}
          maxLength={100}
          placeholder={t('예: 주말 레이드')}
          onChange={(e) => setName(e.target.value)}
          onKeyDown={(e) => e.key === 'Enter' && !e.nativeEvent.isComposing && void submit()}
        />
      </Field>
      <Field label={t('설명(선택)')}>
        <textarea className="input" rows={3} value={desc} maxLength={1000} onChange={(e) => setDesc(e.target.value)} />
      </Field>
    </Modal>
  );
}

/** `url`: 웹 초대 페이지에서 연 초대 링크(칸에 채워 두고, 참가는 사용자가 누름). */
export function JoinCrewModal({ url: initial, onClose }: { url?: string; onClose: () => void }) {
  const client = useStore((s) => s.client);
  const call = useStore((s) => s.call);
  const refreshSpaces = useStore((s) => s.refreshSpaces);
  const selectSpace = useStore((s) => s.selectSpace);
  const toast = useStore((s) => s.toast);
  const [url, setUrl] = useState(initial ?? '');
  const [busy, setBusy] = useState(false);
  const submit = async () => {
    if (!client || !url.trim()) return;
    setBusy(true);
    const r = await call(client.call('invite.join', { url: url.trim() }));
    setBusy(false);
    if (!r) return;
    await refreshSpaces();
    if (r.status === 'joined') await selectSpace(r.space);
    else toast('info', t('참가 요청을 보냈습니다. 크루의 확정 노드가 처리하면 들어가집니다.'));
    onClose();
  };
  return (
    <Modal
      title={t('초대 링크로 참가')}
      onClose={onClose}
      size="sm"
      footer={
        <>
          <button type="button" className="btn ghost" onClick={onClose}>
            {t('취소')}
          </button>
          <button type="button" className="btn primary" disabled={busy || !url.trim()} onClick={() => void submit()}>
            {t('참가')}
          </button>
        </>
      }
    >
      <Field label={t('초대 링크')} hint={t('받은 초대 링크(https://…#invite/… 또는 yangpa://invite/…)')}>
        <input className="input mono" value={url} placeholder={t('받은 초대 링크')} onChange={(e) => setUrl(e.target.value)} />
      </Field>
    </Modal>
  );
}

export function ConvertCrewModal({ space, onClose }: { space: string; onClose: () => void }) {
  const client = useStore((s) => s.client);
  const call = useStore((s) => s.call);
  const friends = useStore((s) => s.friends);
  const peer = friends.find((f) => f.space === space);
  const [name, setName] = useState(peer ? t('{name}와(과) 크루', { name: peer.profile.name }) : '');
  const [busy, setBusy] = useState(false);
  const submit = async () => {
    if (!client || !name.trim()) return;
    setBusy(true);
    const r = await call(client.call('crew.convert_request', { space, name: name.trim() }), { ok: t('전환 요청을 보냈습니다. 상대가 동의하면 크루가 됩니다.') });
    setBusy(false);
    if (r !== undefined) onClose();
  };
  return (
    <Modal
      title={t('1대1 대화를 크루로 전환')}
      onClose={onClose}
      footer={
        <>
          <button type="button" className="btn ghost" onClick={onClose}>
            {t('취소')}
          </button>
          <button type="button" className="btn primary" disabled={busy || !name.trim()} onClick={() => void submit()}>
            <Users size={15} /> {t('전환 요청')}
          </button>
        </>
      }
    >
      <div className="note">
        <Info size={16} />
        <div>
          <p>{t('두 사람 모두 동의해야 바뀝니다. 바뀌면 다른 사람을 초대하고 역할·채널을 만들 수 있습니다.')}</p>
          <p>{t('지금까지의 기록과 채널은 그대로 남고, 둘 다 관리자 역할을 받습니다. 되돌릴 수는 없습니다.')}</p>
        </div>
      </div>
      <Field label={t('크루 이름')}>
        <input className="input" value={name} maxLength={100} onChange={(e) => setName(e.target.value)} />
      </Field>
    </Modal>
  );
}
