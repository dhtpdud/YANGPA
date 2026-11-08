// 네트워크 탭의 Tor 브리지(`net.bridges`, 기본 꺼짐). Tor Browser의 브리지 설정처럼 켜기, 내 브리지(이모지 4개로 알아보기·
// 복사·지우기·QR), 내장 브리지 고르기·주소 직접 넣기, 브리지를 얻는 곳을 둡니다. Tor에 들어가지 못하면 데몬이 내장
// Snowflake를 스스로 켜고 알립니다(`auto`, store/apply.ts).

import { ChevronRight, Copy, Globe, Mail, QrCode, Send, Trash2 } from 'lucide-react';
import { useCallback, useEffect, useState } from 'react';
import type { BridgeKind, BridgesView } from '../api/types';
import { QrImage, Toggle, copyText } from '../components/common';
import { t } from '../i18n';
import { EMOJI_CATEGORIES } from '../lib/emojiData';
import { useStore } from '../store/store';

const LEARN_MORE = 'https://tb-manual.torproject.org/bridges/';
const TELEGRAM = 'https://t.me/GetBridgesBot';
const WEB = 'https://bridges.torproject.org/';
const EMAIL = 'bridges@torproject.org';

/** 스위치 아래 설명. */
export const BRIDGES_HELP = t(
  '브리지는 Tor가 막힌 곳에서 Tor에 들어가게 돕는 공개되지 않은 중계기입니다. 인터넷 사업자는 Tor를 쓴다는 것을 알기 어려워집니다. 브리지(Snowflake면 자원봉사 프록시)는 Tor 입구처럼 이 기기의 IP를 봅니다. 브리지 설정을 한 번도 바꾸지 않았다면 Tor에 들어가지 못할 때 Snowflake를 스스로 켭니다.',
);

const EMOJI_POOL = EMOJI_CATEGORIES.filter((c) => ['nature', 'food', 'activity'].includes(c.id)).flatMap((c) =>
  c.emojis.map((e) => e.char),
);

/** 브리지 줄을 알아보기 쉽게 하는 이모지 4개(줄의 FNV-1a 해시 바이트마다 하나). Tor Browser의 이모지와는 다릅니다. */
export function bridgeEmoji(line: string): string[] {
  let h = 0x811c9dc5;
  for (const b of new TextEncoder().encode(line)) h = Math.imul(h ^ b, 0x01000193) >>> 0;
  return [24, 16, 8, 0].map((s) => EMOJI_POOL[((h >>> s) & 0xff) % EMOJI_POOL.length]);
}

const BUILTIN_LABEL: Record<Exclude<BridgeKind, 'custom'>, string> = { snowflake: 'Snowflake', obfs4: 'obfs4' };

export function BridgesSection() {
  const client = useStore((s) => s.client);
  const call = useStore((s) => s.call);
  const toast = useStore((s) => s.toast);
  const [v, setV] = useState<BridgesView | null>(null);
  const [mode, setMode] = useState<'builtin' | 'custom' | null>(null);
  const [pick, setPick] = useState<Exclude<BridgeKind, 'custom'>>('snowflake');
  const [text, setText] = useState('');
  const [qr, setQr] = useState(false);
  const refresh = useCallback(async () => {
    if (!client) return;
    setV(await client.call('net.bridges', {}).catch(() => null));
  }, [client]);
  useEffect(() => {
    void refresh();
  }, [refresh]);

  if (!v) return null;
  const save = async (enabled: boolean, kind: BridgeKind, custom: string[], ok: string) => {
    if (!client) return false;
    const done = (await call(client.call('net.set_bridges', { enabled, kind, custom }), { ok })) !== undefined;
    await refresh();
    return done;
  };
  const toggle = (on: boolean) =>
    void save(on, v.kind, v.custom, on ? t('브리지를 켰습니다. Tor에 다시 연결합니다.') : t('브리지를 껐습니다'));
  const applyBuiltin = async () => {
    if (await save(true, pick, v.custom, t('{name} 브리지를 켰습니다', { name: BUILTIN_LABEL[pick] }))) setMode(null);
  };
  const applyCustom = async () => {
    const lines = text.split('\n').map((l) => l.trim()).filter(Boolean);
    if (await save(true, 'custom', lines, t('브리지 주소를 저장하고 켰습니다'))) {
      setMode(null);
      setText('');
    }
  };
  const remove = (line: string) => {
    const rest = v.custom.filter((l) => l !== line);
    // 쓰던 줄을 다 지우면 끕니다(직접 넣은 브리지가 없으면 켤 수 없음).
    void save(v.enabled && (v.kind !== 'custom' || rest.length > 0), v.kind, rest, t('브리지를 지웠습니다'));
  };
  const copy = async (s: string) => {
    if (await copyText(s)) toast('success', t('복사했습니다'));
    else toast('error', t('복사하지 못했습니다'));
  };
  const open = (url: string) => void client?.transport.openExternal(url);

  return (
    <div className="card stack bridges">
      <div className="row between">
        <div>
          <strong>{t('브리지')}</strong>
          <div className="muted small">
            {BRIDGES_HELP}{' '}
            <button type="button" className="link-btn" onClick={() => open(LEARN_MORE)}>
              {t('더 알아보기')}
            </button>
          </div>
        </div>
        <Toggle checked={v.enabled} disabled={!v.available && !v.enabled} label={t('브리지 사용')} onChange={toggle} />
      </div>
      {!v.available && <div className="small warn">{t('이 설치본에는 브리지 전송 프로그램(lyrebird)이 없어 브리지를 쓸 수 없습니다.')}</div>}
      {v.enabled && v.auto && <div className="small warn">{t('Tor에 바로 들어가지 못해 Snowflake 브리지를 자동으로 켰습니다.')}</div>}

      <div className="stack">
        <div className="small strong">{t('내 브리지')}</div>
        {v.kind !== 'custom' && (
          <div className="row">
            <span className="grow">{t('내장 브리지: {name}', { name: BUILTIN_LABEL[v.kind] })}</span>
            {v.enabled && <span className="tag">{t('사용 중')}</span>}
          </div>
        )}
        {v.custom.map((line) => (
          <div key={line} className="row bridge-line">
            <span className="bridge-emoji" aria-hidden>
              {bridgeEmoji(line).join(' ')}
            </span>
            <span className="mono small grow" title={line}>
              {line}
            </span>
            {v.enabled && v.kind === 'custom' && <span className="tag">{t('사용 중')}</span>}
            <button type="button" className="btn small icon" aria-label={t('주소 복사')} onClick={() => void copy(line)}>
              <Copy size={13} />
            </button>
            <button type="button" className="btn small icon" aria-label={t('브리지 지우기')} onClick={() => remove(line)}>
              <Trash2 size={13} />
            </button>
          </div>
        ))}
        {v.custom.length > 0 && (
          <div className="card row between">
            <div>
              <strong>{t('다른 사람의 연결 돕기')}</strong>
              <div className="muted small">{t('신뢰할 수 있는 연락처와 브리지를 공유하세요.')}</div>
            </div>
            <div className="row">
              <button type="button" className="btn small" onClick={() => void copy(v.custom.join('\n'))}>
                {t('주소 복사')}
              </button>
              {v.qr && (
                <button type="button" className="btn small icon" aria-label={t('QR 코드')} aria-pressed={qr} onClick={() => setQr(!qr)}>
                  <QrCode size={13} />
                </button>
              )}
            </div>
          </div>
        )}
        {qr && v.qr && <QrImage qr={v.qr} label={t('브리지 주소 QR 코드')} />}
      </div>

      <div className="stack">
        <div className="small strong">{t('브리지 교체')}</div>
        <button type="button" className="btn row between" onClick={() => setMode(mode === 'builtin' ? null : 'builtin')}>
          {t('내장 브리지 중 하나 선택')} <ChevronRight size={14} />
        </button>
        {mode === 'builtin' && (
          <div className="card stack">
            <div className="seg" role="radiogroup" aria-label={t('내장 브리지')}>
              {(['snowflake', 'obfs4'] as const).map((k) => (
                <button key={k} type="button" role="radio" aria-checked={pick === k} className={pick === k ? 'active' : ''} onClick={() => setPick(k)}>
                  {BUILTIN_LABEL[k]}
                </button>
              ))}
            </div>
            <div className="muted small">
              {pick === 'snowflake'
                ? t('자원봉사자의 브라우저를 거쳐 들어갑니다. 대부분의 검열을 넘지만 느릴 수 있습니다.')
                : t('Tor 트래픽을 무작위 데이터처럼 보이게 합니다. 빠르지만 내장 주소는 막혔을 수 있습니다.')}
            </div>
            <button type="button" className="btn primary" onClick={() => void applyBuiltin()}>
              {t('이 브리지 사용')}
            </button>
          </div>
        )}
        <button type="button" className="btn row between" onClick={() => setMode(mode === 'custom' ? null : 'custom')}>
          {t('이미 알고 있는 브리지 주소 입력')} <ChevronRight size={14} />
        </button>
        {mode === 'custom' && (
          <div className="card stack">
            <textarea
              className="input mono small"
              rows={4}
              value={text}
              placeholder="obfs4 192.0.2.1:443 … cert=… iat-mode=0"
              aria-label={t('브리지 주소')}
              onChange={(e) => setText(e.target.value)}
            />
            <div className="muted small">{t('한 줄에 하나씩 넣으세요. obfs4·WebTunnel·Snowflake 브리지를 쓸 수 있고 32줄까지 넣을 수 있습니다.')}</div>
            <button type="button" className="btn primary" disabled={!text.trim()} onClick={() => void applyCustom()}>
              {t('저장하고 켜기')}
            </button>
          </div>
        )}
      </div>

      <div className="stack">
        <div className="small strong">{t('브리지 찾기')}</div>
        <button type="button" className="btn row between" onClick={() => open(TELEGRAM)}>
          <span>
            <Send size={13} /> {t('텔레그램')}
            <span className="muted small"> · {t('GetBridgesBot에 메시지 보내기')}</span>
          </span>
          <ChevronRight size={14} />
        </button>
        <button type="button" className="btn row between" onClick={() => open(WEB)}>
          <span>
            <Globe size={13} /> {t('웹')}
            <span className="muted small"> · bridges.torproject.org</span>
          </span>
          <ChevronRight size={14} />
        </button>
        <button type="button" className="btn row between" onClick={() => void copy(EMAIL)}>
          <span>
            <Mail size={13} /> {t('Gmail 또는 Riseup 이메일')}
            <span className="muted small"> · {t('{email}로 이메일 보내기(주소 복사)', { email: EMAIL })}</span>
          </span>
          <Copy size={13} />
        </button>
      </div>
    </div>
  );
}
