// 휘발성 채널에서 스크린샷·화면 녹화가 감지되면 채널 화면을 검게 가리고 금지 안내를 띄웁니다.
// 감지는 윈도 앱(src-tauri/src/capture.rs)과, 앱 창에서 누른 PrtScn 키입니다. 찍힌 화면에서 창 내용을 빼는 일은
// 창 캡처 차단(set_content_protection)이 합니다.

import { EyeOff } from 'lucide-react';
import { useEffect, useState } from 'react';
import { t } from '../i18n';
import { useStore } from '../store/store';

export const CAPTURE_BLOCKED_TEXT = t('해당 채널에서는 스크린샷이나 녹화가 금지되어 있어요.');

export function CaptureShield() {
  const client = useStore((s) => s.client);
  const [recording, setRecording] = useState<string[]>([]);
  const [shot, setShot] = useState<{ tool: string | null } | null>(null);

  useEffect(() => {
    // 윈도는 PrtScn의 keydown을 앱에 주지 않아 keyup만 옵니다. 입력창이 이벤트를 멈춰도 받도록 캡처 단계에서 봅니다.
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'PrintScreen') setShot({ tool: null });
    };
    window.addEventListener('keyup', onKey, true);
    const stop = client?.transport.watchCapture?.((ev) => {
      if (ev.kind === 'screenshot') setShot({ tool: ev.tool });
      else setRecording(ev.tools);
    });
    return () => {
      window.removeEventListener('keyup', onKey, true);
      stop?.();
    };
  }, [client]);

  if (!shot && recording.length === 0) return null;
  const detail =
    recording.length > 0
      ? t('녹화 프로그램({tools})이 켜져 있는 동안 화면을 가립니다.', { tools: recording.join(', ') })
      : shot?.tool
        ? t('스크린샷({tool})이 감지되어 화면을 가렸습니다.', { tool: shot.tool })
        : t('스크린샷이 감지되어 화면을 가렸습니다.');
  return (
    <div className="capture-shield" role="alertdialog" aria-modal="true" aria-labelledby="capture-shield-title">
      <EyeOff size={44} />
      <h2 id="capture-shield-title">{CAPTURE_BLOCKED_TEXT}</h2>
      <p>{detail}</p>
      {recording.length === 0 && (
        <button type="button" className="btn primary" autoFocus onClick={() => setShot(null)}>
          {t('확인')}
        </button>
      )}
    </div>
  );
}
