// 마이크·스피커 장치와 음량(이 기기만, 앱 설정 파일 `prefs.media.devices`).
//
// 웹뷰가 시크릿 모드라 장치 ID가 앱을 다시 켤 때마다 바뀔 수 있습니다. 그래서 고른 장치는 ID와 이름을 함께 두고, ID로 못
// 찾으면 이름으로 찾습니다. 둘 다 없으면(빼 둔 장치) 시스템 기본 장치를 씁니다.

export interface DevicePick {
  id: string;
  label: string;
}

export interface DevicePrefs {
  /** 마이크(null이면 시스템 기본). */
  input: DevicePick | null;
  /** 스피커(null이면 시스템 기본). */
  output: DevicePick | null;
  /** 마이크 음량(%). 100이면 그대로, 넘으면 키움. */
  inputVolume: number;
  /** 스피커 음량(%). 받는 소리 전체에 걸림. */
  outputVolume: number;
}

export const DEFAULT_DEVICES: DevicePrefs = { input: null, output: null, inputVolume: 100, outputVolume: 100 };
export const INPUT_VOLUME_RANGE = { min: 0, max: 200 } as const;
export const OUTPUT_VOLUME_RANGE = { min: 0, max: 100 } as const;

export type AudioDeviceKind = 'audioinput' | 'audiooutput';

const clamp = (v: unknown, r: { min: number; max: number }, def: number): number =>
  typeof v === 'number' && Number.isFinite(v) ? Math.min(r.max, Math.max(r.min, Math.round(v))) : def;

function pick(v: unknown): DevicePick | null {
  if (!v || typeof v !== 'object') return null;
  const { id, label } = v as { id?: unknown; label?: unknown };
  if (typeof id !== 'string' || typeof label !== 'string' || id.length > 512 || label.length > 256) return null;
  return { id, label };
}

/** 설정 파일에서 읽은 값 → 쓸 수 있는 값(모르는 값은 기본값). */
export function sanitizeDevices(raw: unknown): DevicePrefs {
  const r = (raw && typeof raw === 'object' ? raw : {}) as Record<string, unknown>;
  return {
    input: pick(r.input),
    output: pick(r.output),
    inputVolume: clamp(r.inputVolume, INPUT_VOLUME_RANGE, DEFAULT_DEVICES.inputVolume),
    outputVolume: clamp(r.outputVolume, OUTPUT_VOLUME_RANGE, DEFAULT_DEVICES.outputVolume),
  };
}

/** 크로뮴이 목록 맨 앞에 넣는 '기본'·'통신' 가짜 장치(설정의 '시스템 기본'과 겹침). */
export function isAliasDevice(d: Pick<MediaDeviceInfo, 'deviceId'>): boolean {
  return d.deviceId === 'default' || d.deviceId === 'communications';
}

/** 고른 장치의 지금 ID. ID가 그대로 있으면 그것, 없으면 같은 이름, 둘 다 없거나 기본이면 undefined. */
export function resolveDevice(devices: readonly Pick<MediaDeviceInfo, 'deviceId' | 'label' | 'kind'>[], kind: AudioDeviceKind, want: DevicePick | null): string | undefined {
  if (!want) return undefined;
  const same = devices.filter((d) => d.kind === kind);
  if (same.some((d) => d.deviceId === want.id)) return want.id;
  return same.find((d) => d.label && d.label === want.label && !isAliasDevice(d))?.deviceId;
}

/** 이 종류의 장치 목록. 장치를 못 읽는 환경이면 빈 목록. */
export async function listDevices(kind: AudioDeviceKind): Promise<MediaDeviceInfo[]> {
  try {
    const all = await navigator.mediaDevices?.enumerateDevices?.();
    return (all ?? []).filter((d) => d.kind === kind);
  } catch {
    return [];
  }
}

/** 목록이 마이크 권한 전 모습인지: 크로뮴은 권한 전에는 종류마다 이름 없는 장치 하나만 줍니다. */
export function needsPermission(devices: readonly Pick<MediaDeviceInfo, 'label'>[]): boolean {
  return devices.some((d) => !d.label);
}

/**
 * 마이크를 잠깐 열었다 닫아 장치 권한을 받습니다(앱은 웹뷰의 마이크 요청을 바로 허용함). 그 뒤 목록에 모든 마이크·스피커와
 * 이름이 나옵니다. 마이크가 없거나 열지 못하면 false.
 */
export async function unlockDevices(): Promise<boolean> {
  try {
    const s = await navigator.mediaDevices?.getUserMedia?.({ audio: true, video: false });
    if (!s) return false;
    for (const t of s.getTracks()) t.stop();
    return true;
  } catch {
    return false;
  }
}

/** 고른 장치의 지금 ID(목록을 새로 읽음). */
export async function findDevice(kind: AudioDeviceKind, want: DevicePick | null): Promise<string | undefined> {
  if (!want) return undefined;
  return resolveDevice(await listDevices(kind), kind, want);
}

type SinkTarget = { setSinkId?: (id: string) => Promise<void> };

/** 소리 요소·오디오 컨텍스트의 출력 장치를 바꿉니다(''면 기본). 못 바꾸는 웹뷰면 그대로 둡니다. */
export function applySink(target: unknown, sinkId: string): void {
  const t = target as SinkTarget;
  if (typeof t?.setSinkId !== 'function') return;
  void t.setSinkId(sinkId).catch(() => undefined);
}

/**
 * 마이크 음량을 거는 Web Audio 줄(마이크 → 증폭 → 보낼 트랙). 100%면 만들지 않고 마이크 트랙을 그대로 보냅니다(연결이 하나
 * 줄어 지연·실패할 곳이 적음).
 */
export interface MicGain {
  track: MediaStreamTrack;
  setVolume(percent: number): void;
  close(): void;
}

export function createMicGain(raw: MediaStreamTrack, percent: number): MicGain | null {
  try {
    const ctx = new AudioContext({ latencyHint: 'interactive' });
    const src = ctx.createMediaStreamSource(new MediaStream([raw]));
    const gain = ctx.createGain();
    gain.gain.value = percent / 100;
    const dest = ctx.createMediaStreamDestination();
    src.connect(gain);
    gain.connect(dest);
    const track = dest.stream.getAudioTracks()[0];
    if (!track) {
      void ctx.close().catch(() => undefined);
      return null;
    }
    if (ctx.state === 'suspended') void ctx.resume().catch(() => undefined);
    return {
      track,
      setVolume: (p) => {
        gain.gain.value = p / 100;
      },
      close: () => {
        track.stop();
        src.disconnect();
        void ctx.close().catch(() => undefined);
      },
    };
  } catch {
    return null;
  }
}
