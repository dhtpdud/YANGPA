// 화면공유 소리: 데스크톱 앱이 잡은 PCM(src-tauri/src/screen/audio.rs)을 오디오 트랙으로 만듭니다. 메시 통화에서 화면을 보는
// 사람에게만, 마이크와 따로 된 오디오 m-line으로 보냅니다(session.ts). 이 앱이 내는 소리(통화 목소리)는 잡지 않습니다.

import type { ScreenTransport } from '../api/client';

declare const MediaStreamTrackGenerator:
  | { new (init: { kind: 'audio' }): MediaStreamTrack & { writable: WritableStream<AudioData> } }
  | undefined;

const SAMPLE_RATE = 48_000;
const CHANNELS = 2;
const FRAME_BYTES = 4 * CHANNELS;

export interface ScreenAudio {
  track: MediaStreamTrack;
  /** 보내는 음량(%, 100이면 그대로). */
  setVolume(percent: number): void;
  stop(): void;
}

/** f32 PCM 바이트(4바이트 경계에서 시작하는 새 버퍼)에 음량을 곱합니다. 키워서 넘친 값은 ±1로 자릅니다. */
export function applyGain(data: Uint8Array, gain: number): void {
  if (gain === 1) return;
  const f = new Float32Array(data.buffer, data.byteOffset, data.byteLength / 4);
  for (let i = 0; i < f.length; i++) f[i] = Math.max(-1, Math.min(1, f[i] * gain));
}

/** PCM 메시지 하나 → AudioData(f32 인터리브). 빈 조각이면 null. `timestamp`는 마이크로초, `gain`은 곱할 음량(1이면 그대로). */
export function pcmToAudioData(pcm: Uint8Array, timestamp: number, gain = 1): AudioData | null {
  const frames = Math.floor(pcm.byteLength / FRAME_BYTES);
  if (frames === 0) return null;
  // slice: 메시지 머리 1바이트 뒤라 4바이트 경계가 아님(f32로 읽게 새 버퍼에).
  const data = pcm.slice(0, frames * FRAME_BYTES);
  applyGain(data, gain);
  return new AudioData({ format: 'f32', sampleRate: SAMPLE_RATE, numberOfFrames: frames, numberOfChannels: CHANNELS, timestamp, data });
}

/**
 * 화면 소리를 잡기 시작합니다. 이 웹뷰가 오디오 트랙을 만들 수 없거나 데스크톱 앱이 아니면 null. 열지 못하면 던집니다.
 * `onEnded`는 스스로 끝났을 때(까닭). `volume`은 보내는 음량(%).
 */
export async function startScreenAudio(
  t: ScreenTransport,
  window: string | null,
  onEnded: (reason: string) => void,
  volume = 100,
): Promise<ScreenAudio | null> {
  if (!t.audioStart || typeof MediaStreamTrackGenerator !== 'function' || typeof AudioData !== 'function') return null;
  const gen = new MediaStreamTrackGenerator({ kind: 'audio' });
  const writer = gen.writable.getWriter();
  let ts = 0;
  let gain = volume / 100;
  let id: number | null = null;
  let stopped = false;
  const stop = () => {
    if (stopped) return;
    stopped = true;
    if (id != null) void t.audioStop?.(id).catch(() => undefined);
    void writer.close().catch(() => undefined);
    gen.stop();
  };
  try {
    id = await t.audioStart(window, (msg) => {
      if (stopped) return;
      if (msg[0] === 3) {
        stop();
        onEnded(new TextDecoder().decode(msg.subarray(1)));
        return;
      }
      if (msg[0] !== 1) return;
      const data = pcmToAudioData(msg.subarray(1), ts, gain);
      if (!data) return;
      ts += Math.round((data.numberOfFrames * 1e6) / SAMPLE_RATE);
      // 보내는 쪽이 밀리면 버립니다(지연이 쌓이지 않게).
      if ((writer.desiredSize ?? 1) <= 0) data.close();
      else void writer.write(data).catch(() => data.close());
    });
  } catch (e) {
    stop();
    throw e;
  }
  if (stopped) return null;
  return {
    track: gen,
    setVolume: (p) => {
      gain = p / 100;
    },
    stop,
  };
}
