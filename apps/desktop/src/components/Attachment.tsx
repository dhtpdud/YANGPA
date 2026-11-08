// 첨부: 이미지·영상은 미리보기, 그 밖은 파일 카드. 원본이 없으면 보내는 쪽 썸네일 + 받기 버튼.
// 받는 중에는 blob.progress 알림과 blob.status(조각 수·온라인 시더 수)로 진행을 보여 주고, 진행 알림 사이 시간으로 어림한
// 받기 속도를 % 옆에 보입니다. blob.status는 받기를 눌렀거나
// 받는 중일 때만 부릅니다: 데몬이 연결된 멤버 기기마다 보유 여부를 물어보므로(seeders_online), 그려질 때마다 부르면 큰 크루에서
// 요청이 쏟아지고 내가 지금 어떤 첨부를 보는지도 멤버들에게 드러납니다.
// 휘발성 채널 첨부는 메모리 blob: URL로만 그리고(화면에서 빠지거나 만료되면 해제), 파일로 저장하는 것은 경고를 보이고
// 확인을 받은 뒤에만 합니다(저장한 파일은 수명이 지나도 남음, decisions.md).
// 동영상·음성은 원본 없이도 보내는 쪽 썸네일과 재생 길이를 보여 주고, 받은 첨부는 고정(blob.pin)해 저장 공간 정리에서 뺄 수
// 있습니다(내가 올린 것·휘발성은 고정 단추 없음).
// 이 기기가 내주는 첨부면 지금 받고 있는 사람과 속도를 밑에 작게 보이고(UploadPeers), 이 기기에 데이터가 없는 일반 채널
// 첨부는 같은 파일을 골라 다시 이을 수 있습니다(blob.relink, 해시가 정확히 같아야 함).

import { Download, FileIcon, Film, Flame, ImageOff, Link2, Music, Pin, PinOff, Play, Save } from 'lucide-react';
import { memo, useEffect, useRef, useState } from 'react';
import { isRenderableMime, toRpcError, type SaveRequest } from '../api/client';
import type { AttachmentView, BlobStatusView, EmbedView } from '../api/types';
import { formatBytes, formatRate } from '../lib/format';
import { t } from '../i18n';
import { formatClock } from '../lib/media';
import { cx } from '../lib/misc';
import { addSample, chunkRate, type RateSample } from '../lib/rate';
import { isSafeHttpUrl, plainName } from '../lib/text';
import { useBlobUrl, useThumbUrl } from '../hooks/media';
import { markPinned, usePinned } from '../hooks/pins';
import { findChannel, useStore } from '../store/store';
import { Spinner } from './common';
import { UploadPeers } from './UploadPeers';

const MAX_W = 420;
const MAX_H = 320;

/** 휘발성 첨부를 파일로 저장하기 전에 보이는 경고. */
export const VOLATILE_SAVE_WARNING = [
  t('이 첨부는 휘발성 채널에 있어 지금은 기기 메모리에만 있습니다. 파일로 저장하면:'),
  '',
  t('• 이 기기 디스크에 원본 그대로 남고, 메시지 수명이 끝나거나 채널을 나가도 지워지지 않습니다.'),
  t('• 보낸 사람과 다른 참가자는 저장한 사실을 알 수 없습니다. 지우는 것은 직접 해야 합니다.'),
  t('• 백업·동기화 폴더에 저장하면 그쪽에도 복사될 수 있습니다.'),
].join('\n');

/** ‘파일 다시 잇기’ 설명(단추 툴팁). 원본을 옮기거나 지워도 공유가 멈추지 않는다는 것도 함께 알립니다. */
export const RELINK_HINT = t(
  '이 기기에 이 첨부의 데이터가 없을 때(저장 공간 정리·새 기기·다시 설치) 같은 파일을 골라 다시 채웁니다. 파일 내용(해시)이 정확히 같아야 합니다. 올린 첨부는 이 기기의 암호화된 사본으로 나눠 주므로, 원본 파일을 옮기거나 지워도 그것만으로는 공유가 멈추지 않습니다.',
);

/** 받는 중이면 다시 물어보는 간격(진행 알림이 없는 동안 시더 수를 새로 보기 위해). */
const STATUS_POLL_MS = 3000;

/**
 * 이번 실행에서 받기를 누른 첨부(공간/블롭). 가상 스크롤로 줄이 다시 그려져도 상태 표시(시더 없음 등)를 이어 가려고
 * 메모리에만 둡니다(블롭 ID뿐, 내용 없음).
 */
const requested = new Set<string>();
const MAX_REQUESTED = 256;

function markRequested(key: string) {
  requested.delete(key);
  requested.add(key);
  if (requested.size > MAX_REQUESTED) requested.delete(requested.values().next().value as string);
}

/**
 * 받는 중인 블롭의 속도 표본(블롭 ID → 시각·조각 수). 가상 스크롤로 줄이 다시 그려져도 이어 가려고 메모리에만 둡니다
 * (내용 없음, 디스크·localStorage에 두지 않음).
 */
const rates = new Map<string, RateSample[]>();
const MAX_RATES = 64;
/** 받는 중인 줄만 이 간격으로 다시 그려 멈추면 속도가 내려가게 합니다. */
const RATE_TICK_MS = 1000;

/** 시험용: 기억한 받기 요청과 속도 표본을 지웁니다. */
export function resetRequestedForTest() {
  requested.clear();
  rates.clear();
}

/**
 * 받기 속도(바이트/초). blob.progress로만 표본을 적습니다(blob.status 조각 수는 몇 초 늦게 와서 시간이 틀어짐).
 * 조각 하나의 평문 크기는 크기/조각 수로 어림합니다(마지막 짧은 조각·암호 꼬리는 무시할 만함).
 */
function useTransferRate(blob: string, progress: { have: number; total: number } | undefined, sizeBytes: number, active: boolean): number | null {
  const [now, setNow] = useState(() => Date.now());
  const have = progress?.have;
  useEffect(() => {
    if (have === undefined) return;
    const at = Date.now();
    const next = addSample(rates.get(blob) ?? [], at, have);
    rates.delete(blob);
    rates.set(blob, next);
    if (rates.size > MAX_RATES) rates.delete(rates.keys().next().value as string);
    setNow(at);
  }, [blob, have]);
  useEffect(() => {
    if (!active) return;
    const timer = setInterval(() => setNow(Date.now()), RATE_TICK_MS);
    return () => clearInterval(timer);
  }, [active]);
  const r = chunkRate(rates.get(blob) ?? [], now);
  if (r === null || !progress || progress.total <= 0 || sizeBytes <= 0) return null;
  return (r * sizeBytes) / progress.total;
}

/**
 * 아직 받지 않은 첨부의 상태(blob.status). `enabled`일 때만(받기를 눌렀거나 받는 중) 읽고, 진행 알림(blob.progress)이
 * 끝나면 한 번 더 읽고, 받는 중이면 몇 초마다 다시 읽습니다.
 */
function useBlobStatus(space: string, blob: string, enabled: boolean): [BlobStatusView | null, () => void] {
  const client = useStore((s) => s.client);
  const done = useStore((s) => {
    const p = s.blobProgress[blob];
    return !!p && p.have >= p.total;
  });
  const [status, setStatus] = useState<BlobStatusView | null>(null);
  const [tick, setTick] = useState(0);
  useEffect(() => {
    if (!client || !enabled) return;
    let stale = false;
    client
      .call('blob.status', { space, blob })
      .then((r) => !stale && setStatus(r))
      .catch(() => undefined);
    return () => {
      stale = true;
    };
  }, [client, space, blob, enabled, tick, done]);
  const polling = enabled && !!status?.downloading;
  useEffect(() => {
    if (!polling) return;
    const timer = setInterval(() => setTick((x) => x + 1), STATUS_POLL_MS);
    return () => clearInterval(timer);
  }, [polling]);
  return [status, () => setTick((x) => x + 1)];
}

/**
 * 파일 다시 잇기(blob.relink): 파일을 고르면 데몬이 첨부의 키로 다시 암호화해 같은 블롭인지 봅니다. 성공하면 진행 상태를 다
 * 받은 것으로 맞추고(데몬도 blob.progress를 보냄), 실패하면 데몬이 준 까닭을 알림과 첨부 옆에 보입니다.
 */
function useRelink(space: string, channel: string | undefined, messageId: string, blob: string) {
  const client = useStore((s) => s.client);
  const toast = useStore((s) => s.toast);
  const [busy, setBusy] = useState(false);
  const [done, setDone] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const run = async () => {
    if (!client || !channel || busy) return;
    const picked = await client.transport.pickFiles({ multiple: false });
    const file = picked[0];
    if (!file) return;
    setBusy(true);
    setError(null);
    try {
      const st = await client.call('blob.relink', { space, channel, message: messageId, blob, path: file.path });
      if (st.total_chunks > 0) {
        useStore.setState((s) => ({ blobProgress: { ...s.blobProgress, [blob]: { have: st.total_chunks, total: st.total_chunks } } }));
      }
      setDone(true);
      toast('success', t('파일을 다시 이었습니다. 이 기기가 다시 이 첨부를 나눠 줍니다.'));
    } catch (e) {
      const msg = toRpcError(e).message;
      setError(msg);
      toast('error', msg);
    } finally {
      setBusy(false);
    }
  };
  return { run, busy, done, error };
}

function RelinkButton({ busy, onClick, icon }: { busy: boolean; onClick: () => void; icon?: boolean }) {
  return icon ? (
    <button type="button" className="icon-btn att-relink" aria-label={t('파일 다시 잇기')} title={[t('파일 다시 잇기'), RELINK_HINT].join('\n')} disabled={busy} onClick={onClick}>
      {busy ? <Spinner /> : <Link2 size={18} />}
    </button>
  ) : (
    <button type="button" className="link-btn att-relink" title={RELINK_HINT} disabled={busy} onClick={onClick}>
      {busy ? <Spinner size={12} /> : <Link2 size={12} />} {t('파일 다시 잇기')}
    </button>
  );
}

function fit(w?: number, h?: number): { width: number; height: number } {
  if (!w || !h) return { width: 320, height: 220 };
  const s = Math.min(1, MAX_W / w, MAX_H / h);
  return { width: Math.max(48, Math.round(w * s)), height: Math.max(48, Math.round(h * s)) };
}

/** 받은 첨부 고정(저장 공간 정리에서 뺌). 고정을 모르는 데몬이면 그리지 않습니다. */
function PinButton({ space, blob, enabled }: { space: string; blob: string; enabled: boolean }) {
  const client = useStore((s) => s.client);
  const call = useStore((s) => s.call);
  const pinned = usePinned(blob, enabled);
  if (!enabled || pinned === null) return null;
  const toggle = async () => {
    if (!client) return;
    const r = await call(client.call(pinned ? 'blob.unpin' : 'blob.pin', { space, blob }), {
      ok: pinned ? t('보관을 풀었습니다(저장 공간이 모자라면 정리될 수 있음)') : t('보관합니다(저장 공간 정리에서 뺌)'),
    });
    if (r !== undefined) markPinned(blob, !pinned);
  };
  return (
    <button
      type="button"
      className={cx('icon-btn small att-pin', pinned && 'active')}
      title={pinned ? t('보관 풀기') : t('보관(저장 공간 정리에서 빼기)')}
      aria-pressed={pinned}
      onClick={() => void toggle()}
    >
      {pinned ? <PinOff size={14} /> : <Pin size={14} />}
    </button>
  );
}

/** 다 받은 첨부를 고른 곳·이름으로 저장합니다(휘발성은 경고를 확인한 뒤에만). 첨부 카드·크게 보기 창·라이브러리가 함께 씁니다. */
export async function saveAttachment(space: string, blob: string, keyFrom: string, name: string, volatile: boolean, chosen?: string) {
  const { client, call, openModal } = useStore.getState();
  if (!client) return;
  const write = async () => {
    // 웹뷰가 이미 저장 창을 띄워 고른 경로면 다시 묻지 않습니다(onWebviewSave).
    const path = chosen ?? (await client.transport.saveDialog(name));
    if (!path) return;
    const params = { space, blob, key_from: keyFrom, path };
    // 데몬도 휘발성 첨부는 이 확인 없이는 저장하지 않습니다(1010).
    const saved = client.call('blob.save', volatile ? { ...params, allow_volatile: true } : params).then(() => client.transport.finishSave?.(path));
    await call(saved, { ok: t('저장했습니다') });
  };
  if (!volatile) {
    await write();
    return;
  }
  openModal({
    kind: 'confirm',
    title: t('휘발성 첨부를 파일로 저장'),
    body: VOLATILE_SAVE_WARNING,
    danger: true,
    confirmLabel: t('그래도 저장'),
    onConfirm: write,
  });
}

export function useSaveAttachment(space: string, blob: string, keyFrom: string, name: string, volatile: boolean) {
  return (chosen?: string) => saveAttachment(space, blob, keyFrom, name, volatile, chosen);
}

// 웹뷰 자체 저장(우클릭 "다른 이름으로 저장", 플레이어 다운로드)은 셸이 취소하고 주소만 알려 줍니다(lib.rs `on_download`).
// 웹뷰가 쓰면 휘발성 경고를 건너뛰고, 링크 저장은 Tor를 거치지 않기 때문입니다. 지금 그린 첨부의 주소면 같은 일을 앱의 저장으로
// 합니다(웹뷰가 정한 경로가 있으면 그곳, 없으면 저장 창에서 고른 곳, 휘발성은 경고 뒤). 같은 첨부를 여러 곳(목록·크게 보기·
// 콘텐츠 뷰어)에 그려도 저장은 같고, 한 곳이 화면에서 빠져도 남은 곳으로 이어지게 주소마다 목록으로 둡니다.
const mediaSaves = new Map<string, Array<(path?: string) => Promise<void>>>();

function urlKey(url: string): string {
  try {
    return new URL(url).href;
  } catch {
    return url;
  }
}

/** 그린 첨부 미디어의 주소를 저장과 묶습니다(웹뷰 자체 저장을 앱의 저장으로 잇게). */
export function useMediaSave(url: string | undefined, save: (path?: string) => Promise<void>) {
  const latest = useRef(save);
  latest.current = save;
  useEffect(() => {
    if (!url) return;
    const key = urlKey(url);
    const run = (path?: string) => latest.current(path);
    mediaSaves.set(key, [...(mediaSaves.get(key) ?? []), run]);
    return () => {
      const rest = (mediaSaves.get(key) ?? []).filter((f) => f !== run);
      if (rest.length) mediaSaves.set(key, rest);
      else mediaSaves.delete(key);
    };
  }, [url]);
}

/** 웹뷰가 받으려던 주소. 그린 첨부면 앱의 저장으로 잇고, 아니면(링크 저장) 막았다고 알립니다. */
export function onWebviewSave({ url, path }: SaveRequest, notify: (text: string) => void) {
  const save = mediaSaves.get(urlKey(url))?.at(-1);
  if (save) void save(path ?? undefined);
  else notify(t('링크를 직접 저장하면 Tor를 거치지 않아 막았습니다. 링크를 열어 브라우저에서 저장하세요'));
}

export const Attachment = memo(function Attachment({
  att,
  space,
  channel,
  messageId,
  msgKey,
  volatile,
  own = false,
  noPlayer = false,
}: {
  att: AttachmentView;
  space: string;
  /** 첨부가 달린 메시지의 채널(파일 다시 잇기에 씀). 모르면 다시 잇기 단추를 그리지 않습니다. */
  channel?: string;
  messageId: string;
  msgKey: string;
  volatile: boolean;
  /** 이 기기에서 올린 첨부(데몬이 늘 남기므로 고정 단추 없음). 내 다른 기기에서 보낸 것은 해당 없음. */
  own?: boolean;
  /** 음성 첨부의 재생기를 그리지 않음(콘텐츠 뷰어가 파형 재생기를 따로 그림). */
  noPlayer?: boolean;
}) {
  const client = useStore((s) => s.client);
  const progress = useStore((s) => s.blobProgress[att.blob]);
  const call = useStore((s) => s.call);
  const openModal = useStore((s) => s.openModal);
  const name = plainName(att.name);
  const isImage = att.mime.startsWith('image/');
  const isVideo = att.mime.startsWith('video/');
  const isAudio = att.mime.startsWith('audio/');
  const media = (isImage || isVideo) && isRenderableMime(att.mime);
  const playable = isAudio && isRenderableMime(att.mime);
  const reqKey = `${space}/${att.blob}`;
  const [asked, setAsked] = useState(() => requested.has(reqKey));
  const relink = useRelink(space, channel, messageId, att.blob);
  const active = !!progress && progress.have < progress.total;
  const [status, refreshStatus] = useBlobStatus(space, att.blob, !att.downloaded && !relink.done && (asked || active));
  const have = progress?.have ?? status?.have_chunks ?? 0;
  const total = progress?.total ?? status?.total_chunks ?? 0;
  // 메시지 보기의 downloaded는 다시 읽을 때까지 그대로라서, 진행이 끝났거나 다시 이었으면 받은 것으로 봅니다.
  const downloaded = att.downloaded || relink.done || (total > 0 && have >= total);
  const fetching = !downloaded && (!!status?.downloading || active);
  const percent = total > 0 ? Math.round((have / total) * 100) : 0;
  const noSeeders = !downloaded && !fetching && status?.seeders_online === 0;
  const bps = useTransferRate(att.blob, progress, status?.size || att.size, fetching);
  const speed = fetching && bps !== null ? ` · ${formatRate(Math.round(bps))}` : '';
  /** 파일 카드 크기 줄 뒤에 붙는 받기 상태. */
  const fileNote = downloaded
    ? ''
    : ' · ' + (fetching ? t('받는 중 {percent}%{speed}', { percent, speed }) : noSeeders ? t('가진 온라인 기기 없음') : t('아직 받지 않음'));
  const src = useBlobUrl((media || (playable && !noPlayer)) && downloaded ? { space, blob: att.blob, keyFrom: messageId, mime: att.mime, volatile } : null);
  const canPin = downloaded && !volatile && !own;
  // 휘발성 채널의 첨부는 디스크에 두지 않으므로 다시 이을 수 없습니다(데몬도 거절). 채널 설정도 함께 봅니다(검색 결과 등).
  const channelVolatile = useStore((s) => findChannel(s.views, space, channel)?.settings.security === 'volatile');
  const canRelink = !!channel && !volatile && !channelVolatile && !downloaded && !fetching;
  const duration = (isVideo || isAudio) && att.duration_ms ? formatClock(att.duration_ms) : null;
  const thumb = useThumbUrl(att.thumbnail_base64, volatile);

  const fetchBlob = async () => {
    if (!client) return;
    const r = await call(client.call('blob.fetch', { space, blob: att.blob, priority: 200 }));
    if (r === undefined) return;
    markRequested(reqKey);
    if (asked) refreshStatus();
    else setAsked(true);
  };

  const save = useSaveAttachment(space, att.blob, messageId, name, volatile);
  useMediaSave(downloaded ? src.url : undefined, save);

  if (media) {
    const box = fit(att.width, att.height);
    const ready = downloaded && src.url;
    return (
      <div className="att-media" style={{ width: box.width }}>
        <div className="att-frame" style={{ width: box.width, height: box.height }}>
          {ready && isImage && (
            <img
              src={src.url}
              alt={name}
              decoding="async"
              draggable={false}
              onClick={() =>
                openModal({
                  kind: 'image',
                  att: { space, blob: att.blob, keyFrom: messageId, mime: att.mime, name, volatile, msgKey, messageId, thumbnail: att.thumbnail_base64 },
                })
              }
            />
          )}
          {ready && isVideo && <video src={src.url} controls preload="metadata" playsInline disablePictureInPicture={volatile} />}
          {!ready && (
            <>
              {thumb ? <img className="att-thumb" src={thumb} alt="" decoding="async" draggable={false} /> : <div className="att-placeholder"><ImageOff size={28} /></div>}
              <div className="att-overlay">
                {src.loading ? (
                  <Spinner size={22} />
                ) : downloaded ? (
                  src.error && <span className="att-err">{src.error}</span>
                ) : fetching ? (
                  <div className="att-progress" role="progressbar" aria-valuenow={percent} aria-valuemin={0} aria-valuemax={100}>
                    <div className="bar" style={{ width: `${percent}%` }} />
                    <span>
                      {status
                        ? t('받는 중 {percent}%{speed} · 시더 {n}', { percent, speed, n: status.seeders_online })
                        : t('받는 중 {percent}%{speed}', { percent, speed })}
                    </span>
                  </div>
                ) : (
                  <div className="att-fetch">
                    <button type="button" className="btn small" disabled={relink.busy} onClick={() => void fetchBlob()}>
                      <Download size={14} /> {t('원본 받기 · {size}', { size: formatBytes(att.size) })}
                    </button>
                    {noSeeders && <span className="att-note">{t('지금 이 파일을 가진 온라인 기기가 없습니다')}</span>}
                    {canRelink && <RelinkButton busy={relink.busy} onClick={() => void relink.run()} />}
                    {canRelink && relink.error && (
                      <span className="att-note err" role="alert">
                        {relink.error}
                      </span>
                    )}
                  </div>
                )}
              </div>
            </>
          )}
          {volatile && (
            <span className="att-volatile" title={t('휘발성: 메모리에서만 보여 주고 수명이 지나면 지워집니다')}>
              <Flame size={12} />
            </span>
          )}
          {duration && !ready && (
            <span className="att-duration" title={t('재생 길이')}>
              <Play size={10} fill="currentColor" /> {duration}
            </span>
          )}
        </div>
        {(duration || canPin) && (
          <div className="att-meta row between">
            <span>{duration ? `${duration} · ${formatBytes(att.size)}` : formatBytes(att.size)}</span>
            <PinButton space={space} blob={att.blob} enabled={canPin} />
          </div>
        )}
        <UploadPeers space={space} blob={att.blob} />
      </div>
    );
  }

  if (isAudio) {
    const ready = downloaded && playable && src.url;
    return (
      <div className="att-file att-audio">
        <Music size={28} className="att-file-icon" />
        <div className="att-file-info">
          <div className="att-file-name" title={name}>
            {name}
          </div>
          <div className="att-file-size">
            {duration ? `${duration} · ` : ''}
            {formatBytes(att.size)}
            {fileNote}
          </div>
          {ready && <audio className="att-audio-player" src={src.url} controls preload="metadata" />}
          {fetching && (
            <div className="att-file-progress" role="progressbar" aria-valuenow={percent} aria-valuemin={0} aria-valuemax={100}>
              <div className="bar" style={{ width: `${percent}%` }} />
            </div>
          )}
          {canRelink && relink.error && (
            <div className="att-file-err" role="alert">
              {relink.error}
            </div>
          )}
          <UploadPeers space={space} blob={att.blob} />
        </div>
        {canRelink && <RelinkButton icon busy={relink.busy} onClick={() => void relink.run()} />}
        <PinButton space={space} blob={att.blob} enabled={canPin} />
        {downloaded ? (
          <button type="button" className="icon-btn" title={volatile ? t('저장(경고 확인 뒤)') : t('저장')} onClick={() => void save()}>
            <Save size={18} />
          </button>
        ) : (
          <button type="button" className="icon-btn" title={t('받기')} disabled={fetching} onClick={() => void fetchBlob()}>
            {fetching ? <Spinner /> : <Download size={18} />}
          </button>
        )}
      </div>
    );
  }

  return (
    <div className="att-file">
      {isVideo && thumb ? (
        <span className="att-file-thumb">
          <img src={thumb} alt="" decoding="async" draggable={false} />
        </span>
      ) : isVideo ? (
        <Film size={30} className="att-file-icon" />
      ) : (
        <FileIcon size={30} className="att-file-icon" />
      )}
      <div className="att-file-info">
        <div className="att-file-name" title={name}>
          {name}
        </div>
        <div className="att-file-size">
          {duration ? `${duration} · ` : ''}
          {formatBytes(att.size)}
          {fileNote}
        </div>
        {fetching && (
          <div className="att-file-progress" role="progressbar" aria-valuenow={percent} aria-valuemin={0} aria-valuemax={100}>
            <div className="bar" style={{ width: `${percent}%` }} />
          </div>
        )}
        {canRelink && relink.error && (
          <div className="att-file-err" role="alert">
            {relink.error}
          </div>
        )}
        <UploadPeers space={space} blob={att.blob} />
      </div>
      {canRelink && <RelinkButton icon busy={relink.busy} onClick={() => void relink.run()} />}
      <PinButton space={space} blob={att.blob} enabled={canPin} />
      {downloaded ? (
        <button type="button" className="icon-btn" title={volatile ? t('저장(경고 확인 뒤)') : t('저장')} onClick={() => void save()}>
          <Save size={18} />
        </button>
      ) : (
        <button type="button" className="icon-btn" title={t('받기')} disabled={fetching} onClick={() => void fetchBlob()}>
          {fetching ? <Spinner /> : <Download size={18} />}
        </button>
      )}
    </div>
  );
});

/** 데몬이 만드는 플레이어 주소(`blobs::preview`). 수정된 클라이언트가 실은 다른 주소는 플레이어로 그리지 않습니다. */
const PLAYER_PREFIX = 'https://www.youtube-nocookie.com/embed/';

/** 링크 미리보기. 플레이어는 누르기 전에는 불러오지 않습니다(외부 서버 접속 = IP 노출). */
export const Embed = memo(function Embed({ embed, volatile }: { embed: EmbedView; volatile: boolean }) {
  const [playing, setPlaying] = useState(false);
  const client = useStore((s) => s.client);
  const openAppLink = useStore((s) => s.openAppLink);
  // 연결 방식을 아직 모르면(시작 직후, net.status 실패) Tor로 봅니다: 웹뷰의 iframe은 Tor를 거치지 않습니다.
  const tor = useStore((s) => s.net?.mode !== 'normal');
  const thumb = useThumbUrl(embed.thumbnail_base64, volatile);
  const player = embed.player_url?.startsWith(PLAYER_PREFIX) && isSafeHttpUrl(embed.player_url) ? embed.player_url : null;
  const open = () => {
    // 예전 판이 받아 둔 초대 링크 미리보기도 브라우저 대신 앱 창으로.
    if (isSafeHttpUrl(embed.url) && !openAppLink(embed.url)) void client?.transport.openExternal(embed.url);
  };
  let host = '';
  try {
    host = player ? new URL(player).host : '';
  } catch {
    host = '';
  }
  return (
    <div className="embed">
      {embed.site_name && <div className="embed-site">{embed.site_name}</div>}
      {embed.title && (
        // 제목·사이트 이름은 보낸 사람이 정하므로, 실제로 열 주소를 함께 보입니다(가린 링크와 같음).
        <button type="button" className="embed-title" title={embed.url} onClick={open}>
          {embed.title}
        </button>
      )}
      {embed.description && <div className="embed-desc">{embed.description}</div>}
      {player ? (
        playing ? (
          <iframe
            className="embed-player"
            src={player}
            title={embed.title || t('플레이어')}
            sandbox="allow-scripts allow-same-origin allow-presentation"
            allow="autoplay; encrypted-media; picture-in-picture; fullscreen"
            referrerPolicy="no-referrer"
            allowFullScreen
          />
        ) : (
          <button
            type="button"
            className="embed-thumb player"
            disabled={tor}
            onClick={() => setPlaying(true)}
            title={tor ? t('모든 연결이 Tor로 오가 외부 플레이어를 불러오지 않습니다(누르면 그 서버에 IP가 보이므로)') : t('누르면 {host}에서 불러옵니다(상대 서버에 IP가 보입니다)', { host })}
          >
            {thumb ? <img src={thumb} alt="" decoding="async" draggable={false} /> : <div className="att-placeholder" />}
            <span className="play">
              <Play size={26} fill="currentColor" />
            </span>
            <span className="embed-note">{tor ? t('Tor: 외부 플레이어 꺼짐') : t('눌러서 불러오기 · {host}', { host })}</span>
          </button>
        )
      ) : thumb ? (
        <div className="embed-thumb">
          <img src={thumb} alt="" decoding="async" draggable={false} />
        </div>
      ) : null}
    </div>
  );
});
