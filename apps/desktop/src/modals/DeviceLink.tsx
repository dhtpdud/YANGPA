// 내 기기끼리 잇기: 쓰던 기기가 띄운 QR·링크를 새 기기가 읽고, 양쪽의 같은 번호를 사람이 보고 쓰던 기기에서 허용합니다.
// 쓰던 기기 쪽은 설정 › 기기의 링크와 허용 창, 새 기기 쪽은 온보딩의 연결 화면입니다.

import { AlertTriangle, Camera, Copy, QrCode } from 'lucide-react';
import { useCallback, useEffect, useRef, useState } from 'react';
import type { ReadResult } from 'zxing-wasm/reader';
import zxingWasm from 'zxing-wasm/reader/zxing_reader.wasm?url';
import type { DeviceLinkOffer } from '../api/types';
import { QrImage, Spinner, copySecret } from '../components/common';
import { t } from '../i18n';
import { appLinkKind } from '../lib/links';
import { useStore } from '../store/store';
import { Field, Modal } from './Modal';

/** 설정 › 기기: 그 자리에서 띄우는 연결 링크. 허용·거절로 링크가 끝나면 `onDone`을 부릅니다. */
export function DeviceLinkSection({ onDone }: { onDone: () => void }) {
  const client = useStore((s) => s.client);
  const call = useStore((s) => s.call);
  const asking = useStore((s) => s.deviceLinkRequest !== null);
  const [offer, setOffer] = useState<DeviceLinkOffer | null>(null);

  const create = async () => {
    if (!client) return;
    const r = await call(client.call('device.link.create', {}));
    if (r) setOffer(r);
  };
  const cancel = async () => {
    setOffer(null);
    await client?.call('device.link.cancel', {}).catch(() => undefined);
  };

  useEffect(() => {
    if (!offer) return;
    const timer = setTimeout(() => setOffer(null), Math.max(0, offer.expires_at - Date.now()));
    return () => clearTimeout(timer);
  }, [offer]);

  const asked = useRef(false);
  useEffect(() => {
    if (asking) asked.current = true;
    else if (asked.current) {
      asked.current = false;
      setOffer(null);
      onDone();
    }
  }, [asking, onDone]);

  return (
    <>
      <h3>{t('새 기기 연결')}</h3>
      <p className="muted small">
        {t('새 기기에서 ‘내 다른 기기와 연결’을 고른 뒤 이 QR을 찍거나 링크를 넣으세요. 이 기기에서 허용해야만 들어옵니다. 내 기기를 모두 잃으면 계정을 다시 찾을 수 없으니, 쓸 기기를 미리 연결해 두세요.')}
      </p>
      {offer ? (
        <div className="card stack-s device-link-qr">
          <QrImage qr={offer.qr} label={t('기기 연결 QR 코드')} />
          <p className="muted small">{t('10분 동안, 한 기기만 쓸 수 있습니다.')}</p>
          <div className="row gap-s">
            <button type="button" className="btn small" onClick={() => void copySecret(offer.url)}>
              <Copy size={13} /> {t('링크 복사')}
            </button>
            <button type="button" className="btn small ghost" onClick={() => void cancel()}>
              {t('취소')}
            </button>
          </div>
        </div>
      ) : (
        <div>
          <button type="button" className="btn primary" onClick={() => void create()}>
            <QrCode size={15} /> {t('QR 띄우기')}
          </button>
        </div>
      )}
    </>
  );
}

/** 새 기기 허용 창. 닫으면 거절입니다(허용은 번호를 보고 누를 때만). */
export function DeviceLinkApproveModal() {
  const request = useStore((s) => s.deviceLinkRequest);
  const client = useStore((s) => s.client);
  const call = useStore((s) => s.call);
  const [busy, setBusy] = useState(false);
  const decide = useCallback(
    async (approve: boolean) => {
      if (!client) return;
      setBusy(true);
      await call(client.call('device.link.decide', { approve }), approve ? { ok: t('새 기기를 허용했습니다') } : undefined);
      setBusy(false);
      useStore.setState({ deviceLinkRequest: null });
    },
    [client, call],
  );
  // Modal은 onClose가 바뀌면 첫 칸으로 초점을 옮기므로 고정합니다.
  const reject = useCallback(() => void decide(false), [decide]);
  if (!request) return null;
  return (
    <Modal
      title={t('새 기기 연결 요청')}
      onClose={reject}
      size="sm"
      footer={
        <>
          <button type="button" className="btn ghost" disabled={busy} onClick={reject}>
            {t('거절')}
          </button>
          <button type="button" className="btn primary" disabled={busy} onClick={() => void decide(true)}>
            {busy && <Spinner size={14} />} {t('허용')}
          </button>
        </>
      }
    >
      <div className="stack-s">
        <p>{t('「{name}」 기기가 내 계정에 들어오려고 합니다. 허용하면 그 기기가 계정의 키를 받습니다.', { name: request.name })}</p>
        <div className="link-code">{request.code}</div>
        <p className="muted small">{t('새 기기 화면의 번호와 같을 때만 허용하세요.')}</p>
        {request.contested && (
          <div className="warn-box">
            <AlertTriangle size={16} />
            <div>{t('다른 기기도 이 링크를 썼습니다. 링크가 새어 나갔을 수 있으니, 내 새 기기가 맞는지 꼭 확인하세요.')}</div>
          </div>
        )}
      </div>
    </Modal>
  );
}

/** 앱에 든 wasm을 씁니다(지정하지 않으면 CDN에서 받으려 합니다). 같은 객체여야 스캐너를 다시 열 때 모듈을 재사용합니다. */
const ZXING = { locateFile: () => zxingWasm };

async function loadReader() {
  const { prepareZXingModule, readBarcodes } = await import('zxing-wasm/reader');
  await prepareZXingModule({ overrides: ZXING, fireImmediately: true });
  return readBarcodes;
}

/** 한 프레임의 해독 결과. 위치만 나오고 못 읽었으면 QR은 보이는데 흔들리거나 초점이 안 맞은 것입니다. */
export function scanState(r?: Pick<ReadResult, 'isValid' | 'text'>) {
  if (!r) return 'find';
  if (!r.isValid) return 'seen';
  return appLinkKind(r.text) === 'device' ? 'device' : 'wrong';
}

type ScanView = { state: 'open' | 'find' | 'seen' | 'stuck' | 'wrong'; spot: string; size: number };

const corners = ({ topLeft: a, topRight: b, bottomRight: c, bottomLeft: d }: ReadResult['position']) => [a, b, c, d].map((p) => `${p.x},${p.y}`).join(' ');

/** 카메라로 기기 연결 QR을 읽습니다. 화면은 이 안에서만 쓰고 어디에도 남기지 않습니다. */
function QrScanner({ onFound }: { onFound: (url: string) => void }) {
  const video = useRef<HTMLVideoElement>(null);
  const [err, setErr] = useState<string | null>(null);
  const [view, setView] = useState<ScanView>({ state: 'open', spot: '', size: 1 });
  useEffect(() => {
    let stopped = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    let stream: MediaStream | undefined;
    const release = () => stream?.getTracks().forEach((track) => track.stop());
    const canvas = document.createElement('canvas');
    const ctx = canvas.getContext('2d', { willReadFrequently: true });
    void (async () => {
      try {
        // 링크가 길어 QR 칸이 촘촘하니 기본 해상도보다 크게 받습니다. 그래야 초점이 맞는 거리에서도 읽힙니다.
        const camera = { facingMode: 'environment', width: { ideal: 1920 } };
        const [read] = await Promise.all([
          loadReader(),
          navigator.mediaDevices.getUserMedia({ video: camera }).then((media) => {
            stream = media;
            // 해독기가 먼저 실패했거나 화면을 떠났으면 늦게 열린 카메라를 바로 끕니다.
            if (stopped) release();
          }),
        ]);
        const v = video.current;
        if (stopped || !v || !ctx || !stream) return release();
        v.srcObject = stream;
        await v.play();
        setView({ state: 'find', spot: '', size: 1 });
        let seenAt = 0;
        let seenSince = 0;
        while (!stopped) {
          const size = Math.min(v.videoWidth, v.videoHeight);
          if (size) {
            // 화면의 정사각형과 같은 곳만 읽어, 보이는 네모가 곧 읽는 범위입니다.
            if (canvas.width !== size) canvas.width = canvas.height = size;
            ctx.drawImage(v, (v.videoWidth - size) / 2, (v.videoHeight - size) / 2, size, size, 0, 0, size, size);
            const [r] = await read(ctx.getImageData(0, 0, size, size), { formats: ['QRCode'], returnErrors: true, maxNumberOfSymbols: 1 });
            if (stopped) return;
            const state = scanState(r);
            if (state === 'device') return onFound(r.text.trim());
            const now = Date.now();
            // 한두 프레임 놓쳤다고 안내가 깜빡이지 않게 잠시 붙듭니다.
            if (state !== 'find' || now - seenAt > 700) {
              if (state !== 'find') seenAt = now;
              seenSince = state === 'seen' ? seenSince || now : 0;
              setView({ state: state === 'seen' && now - seenSince > 3000 ? 'stuck' : state, spot: r ? corners(r.position) : '', size });
            }
          }
          await new Promise((next) => {
            timer = setTimeout(next, 100);
          });
        }
      } catch {
        if (!stopped) setErr(t('카메라를 열지 못했습니다. 링크를 붙여 넣으세요.'));
        stopped = true;
        release();
      }
    })();
    return () => {
      stopped = true;
      clearTimeout(timer);
      release();
    };
  }, [onFound]);
  if (err) return <div className="err">{err}</div>;
  const hint = {
    open: t('카메라를 여는 중입니다'),
    find: t('QR을 네모 안에 맞추세요'),
    seen: t('QR이 보입니다. 그대로 멈춰 주세요'),
    stuck: t('잘 읽히지 않습니다. 거리를 천천히 바꿔 초점을 맞춰 보세요'),
    wrong: t('기기 연결 QR이 아닙니다. 쓰던 기기의 설정 › 기기에서 띄운 QR을 찍으세요.'),
  }[view.state];
  return (
    <div className="qr-scan" data-state={view.state}>
      <video ref={video} muted playsInline aria-label={t('QR 코드 찍기')} />
      <svg viewBox={`0 0 ${view.size} ${view.size}`} aria-hidden="true">
        {view.spot && <polygon points={view.spot} />}
      </svg>
      <div className="qr-scan-hint" role="status">
        {hint}
      </div>
    </div>
  );
}

/** 온보딩의 연결 화면. 허용되면 store가 첫 동기화 화면으로 넘어갑니다. */
export function DeviceLinkJoin() {
  const client = useStore((s) => s.client);
  const result = useStore((s) => s.deviceLinkResult);
  const passed = useStore((s) => s.deviceLinkUrl);
  const [url, setUrl] = useState('');
  const [scanning, setScanning] = useState(false);
  const [read, setRead] = useState(false);
  const [busy, setBusy] = useState(false);
  const [code, setCode] = useState<string | null>(null);
  const [err, setErr] = useState<string | null>(null);

  // OS가 넘긴 링크는 어느 사이트든 띄울 수 있으므로 칸에 채우기만 합니다.
  useEffect(() => {
    if (!passed) return;
    setUrl(passed);
    useStore.setState({ deviceLinkUrl: null });
  }, [passed]);

  useEffect(() => {
    if (!result) return;
    useStore.setState({ deviceLinkResult: null });
    if (result === 'approved') return;
    setCode(null);
    setErr(
      result === 'rejected'
        ? t('쓰던 기기에서 거절했습니다.')
        : result === 'expired'
          ? t('링크가 끝났습니다. 쓰던 기기에서 QR을 새로 띄우세요.')
          : t('쓰던 기기가 보낸 정보를 쓰지 못했습니다. QR을 새로 띄워 다시 해 보세요.'),
    );
  }, [result]);

  const join = useCallback(
    async (link: string) => {
      if (!client || !link.trim()) return;
      setScanning(false);
      setBusy(true);
      setErr(null);
      try {
        setCode((await client.call('device.link.join', { url: link.trim() })).code);
        setUrl('');
      } catch (e) {
        setErr((e as Error).message);
      } finally {
        setBusy(false);
      }
    },
    [client],
  );
  const scanned = useCallback(
    (link: string) => {
      setRead(true);
      void join(link).finally(() => setRead(false));
    },
    [join],
  );
  const abort = async () => {
    setCode(null);
    await client?.call('device.link.abort', {}).catch(() => undefined);
  };

  if (code) {
    return (
      <>
        <h2>{t('쓰던 기기에서 허용하세요')}</h2>
        <p className="muted">{t('쓰던 기기에 뜬 번호가 이 번호와 같은지 보고 허용을 누르세요.')}</p>
        <div className="link-code">{code}</div>
        <div className="row gap-s">
          <Spinner size={16} /> <span className="muted small">{t('허용을 기다리는 중입니다')}</span>
        </div>
        <button type="button" className="btn ghost" onClick={() => void abort()}>
          {t('그만두기')}
        </button>
      </>
    );
  }
  return (
    <>
      <h2>{t('내 다른 기기와 연결')}</h2>
      <p className="muted">{t('쓰던 기기의 설정 › 기기에서 QR을 띄운 뒤, 찍거나 링크를 붙여 넣으세요. 쓰던 기기가 켜져 있어야 합니다.')}</p>
      {read ? (
        <div className="row gap-s" role="status">
          <Spinner size={16} /> <span className="muted small">{t('QR을 읽었습니다. 쓰던 기기에 연결하는 중입니다')}</span>
        </div>
      ) : scanning ? (
        <QrScanner onFound={scanned} />
      ) : (
        <button type="button" className="btn large" disabled={busy} onClick={() => setScanning(true)}>
          <Camera size={18} /> {t('QR 찍기')}
        </button>
      )}
      <Field label={t('기기 연결 링크')}>
        <textarea className="input mono" rows={3} autoComplete="off" spellCheck={false} placeholder="yangpa://device/…" value={url} onChange={(e) => setUrl(e.target.value)} />
      </Field>
      {err && <div className="err">{err}</div>}
      <button type="button" className="btn primary large" disabled={busy || !url.trim()} onClick={() => void join(url)}>
        {busy && <Spinner size={16} />} {t('연결 요청')}
      </button>
    </>
  );
}
