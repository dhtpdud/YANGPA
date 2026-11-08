// 처음 실행: 새 신원 만들기, 또는 쓰던 내 기기와 연결(DeviceLink.tsx).

import { ArrowLeft, Laptop, Sparkles } from 'lucide-react';
import { useEffect, useState } from 'react';
import flameLogo from '../assets/flame.png';
import { Spinner } from '../components/common';
import { t } from '../i18n';
import { hasEmoji } from '../lib/emoji';
import { useStore } from '../store/store';
import { DeviceLinkJoin } from './DeviceLink';
import { Field } from './Modal';

type Step = 'welcome' | 'create' | 'join';

export function Onboarding() {
  const client = useStore((s) => s.client);
  const loadAll = useStore((s) => s.loadAll);
  const toast = useStore((s) => s.toast);
  const passedLink = useStore((s) => s.deviceLinkUrl !== null);
  const [step, setStep] = useState<Step>('welcome');
  const [name, setName] = useState('');
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);

  useEffect(() => {
    if (passedLink) setStep('join');
  }, [passedLink]);

  const create = async () => {
    if (!client || !name.trim() || hasEmoji(name)) return;
    setBusy(true);
    setErr(null);
    try {
      await client.call('identity.create', { name: name.trim() });
      await loadAll();
      toast('success', t('환영합니다! 연락처 링크를 만들어 보내고 대화를 시작해 보세요.'));
    } catch (e) {
      setErr((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  const back = () => {
    // 연결을 기다리던 중이면 쓰던 기기에 묻는 것도 그만둡니다.
    if (step === 'join') void client?.call('device.link.abort', {}).catch(() => undefined);
    setStep('welcome');
  };

  return (
    <div className="onboarding">
      <div className="ob-card">
        {step !== 'welcome' && (
          <button type="button" className="icon-btn ob-back" onClick={back} title={t('처음으로')}>
            <ArrowLeft size={18} />
          </button>
        )}
        {step === 'welcome' && (
          <>
            <img className="ob-logo" src={flameLogo} alt="" />
            <h1>YANGPA</h1>
            <p className="muted">{t('서버 없이 기기끼리 직접 잇는 메신저입니다. 계정은 이 기기에서 만든 키로만 존재합니다.')}</p>
            <div className="ob-actions">
              <button type="button" className="btn primary large" onClick={() => setStep('create')}>
                <Sparkles size={18} /> {t('새로 시작하기')}
              </button>
              <button type="button" className="btn large" onClick={() => setStep('join')}>
                <Laptop size={18} /> {t('내 다른 기기와 연결')}
              </button>
            </div>
            <ul className="ob-points">
              <li>{t('전화번호·이메일이 필요 없습니다')}</li>
              <li>{t('메시지는 종단간 암호화되어 친구·크루 기기에만 갑니다')}</li>
              <li>{t('휘발성 채널은 디스크에 아무것도 남기지 않습니다')}</li>
            </ul>
          </>
        )}
        {step === 'create' && (
          <>
            <h2>{t('이름 정하기')}</h2>
            <p className="muted">{t('친구와 크루에게 보이는 이름입니다. 나중에 바꿀 수 있습니다.')}</p>
            <Field label={t('이름')} hint={hasEmoji(name) ? t('이름에 이모지를 넣을 수 없습니다') : undefined}>
              <input
                className="input"
                autoFocus
                value={name}
                maxLength={64}
                onChange={(e) => setName(e.target.value)}
                onKeyDown={(e) => e.key === 'Enter' && !e.nativeEvent.isComposing && void create()}
              />
            </Field>
            <p className="muted small">{t('계정은 내 기기에만 있습니다. 기기를 모두 잃으면 다시 찾을 수 없으니, 쓸 기기를 미리 연결해 두세요.')}</p>
            {err && <div className="err">{err}</div>}
            <button type="button" className="btn primary large" disabled={busy || !name.trim() || hasEmoji(name)} onClick={() => void create()}>
              {busy && <Spinner size={16} />} {t('신원 만들기')}
            </button>
          </>
        )}
        {step === 'join' && <DeviceLinkJoin />}
      </div>
    </div>
  );
}
