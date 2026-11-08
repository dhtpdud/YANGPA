// SPDX-License-Identifier: LicenseRef-YANGPA-Supporter
// 콘텐츠 뷰어의 오디오 재생기: 파형을 누르거나 끌면 그 자리로 옮겨 재생합니다.
// 파형은 받은 첨부를 메모리에서 Web Audio로 풀어 막대 높이만 남깁니다(휘발성도 디스크에 쓰지 않음, 읽은 바이트는 지움).
// 풀지 못하거나 너무 크면 평평한 막대로 그리고, 끌어서 옮기기는 그대로 됩니다.

import { Pause, Play } from 'lucide-react';
import { useEffect, useMemo, useRef, useState, type PointerEvent } from 'react';
import type { AttachmentView } from '../api/types';
import { useBlobUrl } from '../hooks/media';
import { t } from '../i18n';
import { formatClock } from '../lib/media';
import { clamp } from '../lib/misc';
import { wavePeaks } from './viewer';
import { useStore } from '../store/store';

const BARS = 240;
/** shortcut: 풀면 PCM이 커지므로(3kHz로 낮춰도 1시간 스테레오 약 86MB) 이보다 큰 오디오는 파형 없이 막대로. 스트리밍 해석이 필요해지면 그때 바꿈. */
const WAVE_MAX_BYTES = 64 * 1024 * 1024;
const WAVE_MAX_MS = 60 * 60 * 1000;

/** 파형 모양만 필요하므로 OfflineAudioContext 최저 표본율(3kHz)로 풀어 메모리를 줄입니다. 원본 바이트는 사본을 넘겨 지울 수 있게. */
async function decodePeaks(bytes: Uint8Array): Promise<number[]> {
  const buf = await new OfflineAudioContext(1, 1, 3000).decodeAudioData(bytes.slice().buffer);
  return wavePeaks(
    Array.from({ length: buf.numberOfChannels }, (_, c) => buf.getChannelData(c)),
    BARS,
  );
}

export function AudioWave({ space, att, keyFrom, volatile }: { space: string; att: AttachmentView; keyFrom: string; volatile: boolean }) {
  const client = useStore((s) => s.client);
  const src = useBlobUrl({ space, blob: att.blob, keyFrom, mime: att.mime, volatile });
  const audioRef = useRef<HTMLAudioElement>(null);
  const dragging = useRef(false);
  const [peaks, setPeaks] = useState<number[] | null>(null);
  const [time, setTime] = useState(0);
  const [duration, setDuration] = useState((att.duration_ms ?? 0) / 1000);
  const [playing, setPlaying] = useState(false);

  useEffect(() => {
    if (!client || typeof OfflineAudioContext === 'undefined' || att.size > WAVE_MAX_BYTES || (att.duration_ms ?? 0) > WAVE_MAX_MS) return;
    let stale = false;
    client.transport
      .readBlob(space, att.blob, keyFrom, att.mime)
      .then(async (bytes) => {
        try {
          const p = await decodePeaks(bytes);
          if (!stale) setPeaks(p);
        } finally {
          bytes.fill(0);
        }
      })
      .catch(() => undefined);
    return () => {
      stale = true;
    };
  }, [client, space, att.blob, keyFrom, att.mime, att.size, att.duration_ms]);

  // 영상처럼 보이면 바로 틉니다.
  useEffect(() => {
    if (src.url) void audioRef.current?.play()?.catch(() => undefined);
  }, [src.url]);

  // 막대 하나를 폭 1의 칸 가운데 0.7만큼 그린 경로 하나(바탕·지난 부분이 같은 모양을 씀).
  const path = useMemo(
    () =>
      (peaks ?? new Array<number>(BARS).fill(0))
        .map((p, i) => {
          const h = Math.max(2, p * 48);
          return `M${i + 0.15} ${50 - h}h0.7v${2 * h}h-0.7z`;
        })
        .join(''),
    [peaks],
  );

  const seekAt = (e: PointerEvent<HTMLDivElement>) => {
    const a = audioRef.current;
    const r = e.currentTarget.getBoundingClientRect();
    if (!a || !duration || r.width <= 0) return;
    a.currentTime = clamp((e.clientX - r.left) / r.width, 0, 1) * duration;
    setTime(a.currentTime);
  };
  const toggle = () => {
    const a = audioRef.current;
    if (a?.paused) void a.play()?.catch(() => undefined);
    else a?.pause();
  };
  const pct = duration > 0 ? (time / duration) * 100 : 0;
  const wave = (
    <svg viewBox={`0 0 ${BARS} 100`} preserveAspectRatio="none" aria-hidden>
      <path d={path} />
    </svg>
  );

  if (src.error) return <p className="vw-audio-err">{src.error}</p>;
  return (
    <div className="vw-audio">
      <div className="row gap-s">
        <button type="button" className="icon-btn" title={playing ? t('일시 정지') : t('재생')} disabled={!src.url} onClick={toggle}>
          {playing ? <Pause size={20} /> : <Play size={20} />}
        </button>
        <span className="vw-audio-time">
          {formatClock(time * 1000)} / {formatClock(duration * 1000)}
        </span>
      </div>
      <div
        className="vw-wave"
        role="slider"
        aria-label={t('재생 위치')}
        aria-valuemin={0}
        aria-valuemax={Math.round(duration)}
        aria-valuenow={Math.round(time)}
        onPointerDown={(e) => {
          dragging.current = true;
          e.currentTarget.setPointerCapture?.(e.pointerId);
          seekAt(e);
          void audioRef.current?.play()?.catch(() => undefined);
        }}
        onPointerMove={(e) => dragging.current && seekAt(e)}
        onPointerUp={() => (dragging.current = false)}
        onPointerCancel={() => (dragging.current = false)}
      >
        {wave}
        <div className="vw-wave-done" style={{ clipPath: `inset(0 ${100 - pct}% 0 0)` }}>
          {wave}
        </div>
      </div>
      <audio
        ref={audioRef}
        src={src.url}
        preload="auto"
        onLoadedMetadata={(e) => Number.isFinite(e.currentTarget.duration) && setDuration(e.currentTarget.duration)}
        onTimeUpdate={(e) => !dragging.current && setTime(e.currentTarget.currentTime)}
        onPlay={() => setPlaying(true)}
        onPause={() => setPlaying(false)}
      />
    </div>
  );
}
