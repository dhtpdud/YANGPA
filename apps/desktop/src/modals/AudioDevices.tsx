// 사용자 설정 → 음성·화면공유 → 장치: 마이크·스피커 고르기, 음량, 마이크 테스트. 통화 중에 바꿔도 바로 걸립니다
// (voice/session.ts의 setMedia). 규칙과 기본값은 voice/devices.ts.

import { Mic, MicOff, Volume2 } from 'lucide-react';
import { useEffect, useRef, useState } from 'react';
import { t } from '../i18n';
import { cx } from '../lib/misc';
import { useStore } from '../store/store';
import {
  INPUT_VOLUME_RANGE,
  OUTPUT_VOLUME_RANGE,
  applySink,
  findDevice,
  isAliasDevice,
  listDevices,
  needsPermission,
  resolveDevice,
  unlockDevices,
  type DevicePick,
  type DevicePrefs,
} from '../voice/devices';
import { DEFAULT_MEDIA, micConstraints, type AudioPrefs } from '../voice/media';
import { Field } from './Modal';

export const DEVICES_NOTE = t(
  '통화에 쓸 마이크와 스피커입니다. 이 기기만의 설정이고, 통화 중에 바꿔도 바로 적용됩니다. 고른 장치를 빼 두면 시스템 기본 장치를 씁니다.',
);
export const MIC_TEST_NOTE = t('스피커로 들으면 소리가 되울릴 수 있어 이어폰을 권합니다. 테스트 소리는 아무에게도 보내지 않습니다.');

/** 크로뮴의 '기본값 - 스피커(…)' 같은 이름에서 실제 장치 이름만. */
function aliasTarget(label: string): string {
  const i = label.indexOf(' - ');
  return i >= 0 ? label.slice(i + 3) : label;
}

/**
 * 마이크·스피커 목록. 장치가 꽂히거나 빠지면 다시 읽고, 권한을 받은 뒤(이름이 보이게) `refresh`로도 다시 읽습니다.
 * 권한 전 목록(종류마다 이름 없는 장치 하나)이면 마이크를 한 번 잠깐 열어 권한을 받고 다시 읽습니다.
 */
function useDeviceList() {
  const [inputs, setInputs] = useState<MediaDeviceInfo[]>([]);
  const [outputs, setOutputs] = useState<MediaDeviceInfo[]>([]);
  const [tick, setTick] = useState(0);
  const unlockTried = useRef(false);
  useEffect(() => {
    let stale = false;
    void Promise.all([listDevices('audioinput'), listDevices('audiooutput')]).then(async ([i, o]) => {
      if (stale) return;
      setInputs(i);
      setOutputs(o);
      if (!unlockTried.current && needsPermission([...i, ...o])) {
        unlockTried.current = true;
        if ((await unlockDevices()) && !stale) setTick((n) => n + 1);
      }
    });
    const md = typeof navigator !== 'undefined' ? navigator.mediaDevices : undefined;
    const onChange = () => setTick((n) => n + 1);
    md?.addEventListener?.('devicechange', onChange);
    return () => {
      stale = true;
      md?.removeEventListener?.('devicechange', onChange);
    };
  }, [tick]);
  return { inputs, outputs, refresh: () => setTick((n) => n + 1) };
}

/** 장치 고르기 칸. 고른 장치가 지금 없으면 '(연결 안 됨)'으로 남겨 보여 줍니다. */
function DeviceSelect({
  label,
  devices,
  kind,
  value,
  onChange,
  disabled,
}: {
  label: string;
  devices: MediaDeviceInfo[];
  kind: 'audioinput' | 'audiooutput';
  value: DevicePick | null;
  onChange: (v: DevicePick | null) => void;
  disabled?: boolean;
}) {
  const current = resolveDevice(devices, kind, value);
  const real = devices.filter((d) => !isAliasDevice(d));
  const def = devices.find((d) => d.deviceId === 'default');
  const missing = value && !current;
  const nameOf = (d: MediaDeviceInfo, i: number) => d.label || (kind === 'audioinput' ? t('마이크 {n}', { n: i + 1 }) : t('스피커 {n}', { n: i + 1 }));
  return (
    <select
      className="select"
      aria-label={label}
      // 칸이 좁아 이름이 잘려도 마우스를 올리면 전체 이름이 보이게
      title={missing ? value.label : (real.find((d) => d.deviceId === current)?.label ?? def?.label ?? t('시스템 기본'))}
      disabled={disabled}
      value={missing ? '__missing__' : (current ?? '')}
      onChange={(e) => {
        const id = e.target.value;
        if (id === '__missing__') return;
        const i = real.findIndex((d) => d.deviceId === id);
        onChange(i < 0 ? null : { id, label: real[i].label });
      }}
    >
      <option value="">{def?.label ? t('시스템 기본 ({name})', { name: aliasTarget(def.label) }) : t('시스템 기본')}</option>
      {real.map((d, i) => (
        <option key={d.deviceId} value={d.deviceId}>
          {nameOf(d, i)}
        </option>
      ))}
      {missing && <option value="__missing__">{t('(연결 안 됨) {name}', { name: value.label || t('이름 없는 장치') })}</option>}
    </select>
  );
}

/** 음량 막대. 끄는 동안 값이 바뀔 때마다 저장하지 않게, 잠깐 멈추면 저장합니다. */
export function VolumeSlider({
  label,
  value,
  range,
  onChange,
}: {
  label: string;
  value: number;
  range: { min: number; max: number };
  onChange: (v: number) => void;
}) {
  const [v, setV] = useState(value);
  const latest = useRef(onChange);
  latest.current = onChange;
  useEffect(() => setV(value), [value]);
  useEffect(() => {
    if (v === value) return;
    const timer = setTimeout(() => latest.current(v), 200);
    return () => clearTimeout(timer);
  }, [v, value]);
  return (
    <div className="row gap-s volume-slider">
      <input
        type="range"
        className="grow"
        aria-label={label}
        min={range.min}
        max={range.max}
        step={1}
        value={v}
        onChange={(e) => setV(Number(e.target.value))}
      />
      <span className="small volume-value">{v}%</span>
    </div>
  );
}

/** 0~1 음량 막대 값: RMS를 dB로(-60dB 이하는 0). */
export function levelOf(rms: number): number {
  if (rms <= 0) return 0;
  const db = 20 * Math.log10(rms);
  return Math.min(1, Math.max(0, (db + 60) / 60));
}

interface TestGraph {
  stream: MediaStream;
  ctx: AudioContext;
  micGain: GainNode;
  outGain: GainNode;
  analyser: AnalyserNode;
}

/**
 * 마이크 테스트: 고른 마이크를 열어(통화와 같은 마이크 처리) 음량을 걸고, 막대로 크기를 보이며, 켜 두면 고른 스피커로
 * 되들려줍니다. 화면을 닫거나 장치를 바꾸면 멈춥니다.
 */
function MicTest({ devices, audio, inputs, onOpened }: { devices: DevicePrefs; audio: AudioPrefs; inputs: MediaDeviceInfo[]; onOpened: () => void }) {
  const [running, setRunning] = useState(false);
  const [loopback, setLoopback] = useState(true);
  const [level, setLevel] = useState(0);
  const [error, setError] = useState<string | null>(null);
  const graph = useRef<TestGraph | null>(null);
  const inputId = resolveDevice(inputs, 'audioinput', devices.input);

  const stop = () => {
    const g = graph.current;
    graph.current = null;
    if (g) {
      for (const track of g.stream.getTracks()) track.stop();
      void g.ctx.close().catch(() => undefined);
    }
    setRunning(false);
    setLevel(0);
  };

  const start = async () => {
    setError(null);
    let stream: MediaStream;
    try {
      stream = await navigator.mediaDevices.getUserMedia({ audio: micConstraints(audio, inputId), video: false });
    } catch (e) {
      setError(e instanceof Error && e.name === 'NotAllowedError' ? t('마이크 권한이 없습니다.') : t('마이크를 열지 못했습니다: {message}', { message: (e as Error).message }));
      return;
    }
    try {
      const ctx = new AudioContext({ latencyHint: 'interactive' });
      const src = ctx.createMediaStreamSource(stream);
      const micGain = ctx.createGain();
      const outGain = ctx.createGain();
      const analyser = ctx.createAnalyser();
      analyser.fftSize = 1024;
      src.connect(micGain);
      micGain.connect(analyser);
      micGain.connect(outGain);
      outGain.connect(ctx.destination);
      graph.current = { stream, ctx, micGain, outGain, analyser };
      if (ctx.state === 'suspended') void ctx.resume().catch(() => undefined);
      setRunning(true);
      // 권한을 받으면 장치 이름이 보이므로 목록을 다시 읽습니다.
      onOpened();
    } catch (e) {
      for (const track of stream.getTracks()) track.stop();
      setError(t('오디오 장치를 열지 못했습니다: {message}', { message: (e as Error).message }));
    }
  };

  // 음량·되들려주기·스피커는 테스트 중에도 바로 바꿉니다.
  useEffect(() => {
    const g = graph.current;
    if (!g) return;
    g.micGain.gain.value = devices.inputVolume / 100;
    g.outGain.gain.value = loopback ? devices.outputVolume / 100 : 0;
  }, [running, loopback, devices.inputVolume, devices.outputVolume]);
  useEffect(() => {
    if (!running) return;
    void findDevice('audiooutput', devices.output).then((id) => {
      if (graph.current) applySink(graph.current.ctx, id ?? '');
    });
  }, [running, devices.output]);

  // 마이크·처리 설정이 바뀌면 다시 열어야 해서 멈춥니다.
  const micKey = JSON.stringify([inputId ?? '', micConstraints(audio)]);
  const lastKey = useRef(micKey);
  useEffect(() => {
    if (lastKey.current !== micKey && graph.current) stop();
    lastKey.current = micKey;
  }, [micKey]);

  useEffect(() => {
    if (!running) return;
    const buf = new Float32Array(1024);
    let raf = 0;
    let shown = 0;
    const tick = () => {
      const g = graph.current;
      if (!g) return;
      g.analyser.getFloatTimeDomainData(buf);
      let sum = 0;
      for (const x of buf) sum += x * x;
      const target = levelOf(Math.sqrt(sum / buf.length));
      // 막대가 너무 떨리지 않게 내려갈 때만 천천히.
      shown = target > shown ? target : shown * 0.85 + target * 0.15;
      setLevel(shown);
      raf = requestAnimationFrame(tick);
    };
    raf = requestAnimationFrame(tick);
    return () => cancelAnimationFrame(raf);
  }, [running]);

  useEffect(() => stop, []);

  return (
    <div className="card stack mic-test" aria-label={t('마이크 테스트')}>
      <div className="row between">
        <strong>{t('마이크 테스트')}</strong>
        <button type="button" className={cx('btn small', running ? 'danger' : 'primary')} onClick={() => (running ? stop() : void start())}>
          {running ? <MicOff size={14} /> : <Mic size={14} />} {running ? t('테스트 멈추기') : t('테스트 시작')}
        </button>
      </div>
      <div className="mic-meter" role="meter" aria-label={t('마이크 입력 크기')} aria-valuemin={0} aria-valuemax={100} aria-valuenow={Math.round(level * 100)}>
        <div className={cx('mic-meter-fill', level > 0.92 && 'clip')} style={{ width: `${Math.round(level * 100)}%` }} />
      </div>
      <label className="row gap-s small">
        <input type="checkbox" checked={loopback} onChange={(e) => setLoopback(e.target.checked)} />
        <Volume2 size={14} /> {t('내 목소리 들어보기(고른 스피커로)')}
      </label>
      <p className="muted small">
        {running ? t('말해 보세요. 막대가 움직이지 않으면 다른 마이크를 고르거나 마이크 음량을 올려 보세요.') : MIC_TEST_NOTE}
      </p>
      {error && <p className="err small">{error}</p>}
    </div>
  );
}

export function AudioDevicesSection() {
  const media = useStore((s) => s.prefs.media ?? DEFAULT_MEDIA);
  const setPrefs = useStore((s) => s.setPrefs);
  const { inputs, outputs, refresh } = useDeviceList();
  const dev = media.devices;
  const setDevices = (patch: Partial<DevicePrefs>) => {
    // 저장 직전의 값에 겹칩니다(음량 막대가 늦게 저장해도 다른 칸을 되돌리지 않게).
    const cur = useStore.getState().prefs.media ?? DEFAULT_MEDIA;
    setPrefs({ media: { ...cur, devices: { ...cur.devices, ...patch } } });
  };
  const canPickOutput = typeof HTMLMediaElement !== 'undefined' && 'setSinkId' in HTMLMediaElement.prototype;
  const noLabels = inputs.length > 0 && inputs.every((d) => !d.label);
  return (
    <>
      <h3>{t('장치')}</h3>
      <p className="muted small">{DEVICES_NOTE}</p>
      <div className="grid2 audio-devices">
        <Field label={t('마이크(입력 장치)')}>
          <DeviceSelect label={t('마이크')} devices={inputs} kind="audioinput" value={dev.input} onChange={(input) => setDevices({ input })} />
        </Field>
        <Field label={t('스피커(출력 장치)')} hint={canPickOutput ? undefined : t('이 웹뷰는 출력 장치를 고를 수 없어 시스템 기본 장치로 틉니다.')}>
          <DeviceSelect label={t('스피커')} devices={outputs} kind="audiooutput" value={dev.output} disabled={!canPickOutput} onChange={(output) => setDevices({ output })} />
        </Field>
        <Field label={t('마이크 음량')} hint={t('100%가 그대로입니다. 상대에게 작게 들리면 올리세요.')}>
          <VolumeSlider label={t('마이크 음량')} value={dev.inputVolume} range={INPUT_VOLUME_RANGE} onChange={(inputVolume) => setDevices({ inputVolume })} />
        </Field>
        <Field label={t('스피커 음량')} hint={t('통화에서 받는 소리 전체의 크기입니다.')}>
          <VolumeSlider label={t('스피커 음량')} value={dev.outputVolume} range={OUTPUT_VOLUME_RANGE} onChange={(outputVolume) => setDevices({ outputVolume })} />
        </Field>
      </div>
      {noLabels && <p className="muted small">{t('장치 이름은 마이크 권한을 받은 뒤에 보입니다. 아래 마이크 테스트를 한 번 시작해 보세요.')}</p>}
      <MicTest devices={dev} audio={media.audio} inputs={inputs} onOpened={refresh} />
    </>
  );
}
