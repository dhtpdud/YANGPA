import { describe, expect, it } from 'vitest';
import { DEFAULT_DEVICES, isAliasDevice, resolveDevice, sanitizeDevices } from './devices';
import { DEFAULT_MEDIA, micConstraints, sanitizeMedia } from './media';

const list = [
  { kind: 'audioinput' as const, deviceId: 'default', label: '기본값 - 헤드셋 마이크' },
  { kind: 'audioinput' as const, deviceId: 'communications', label: '통신 - 헤드셋 마이크' },
  { kind: 'audioinput' as const, deviceId: 'a1', label: '헤드셋 마이크' },
  { kind: 'audioinput' as const, deviceId: 'a2', label: '웹캠 마이크' },
  { kind: 'audiooutput' as const, deviceId: 'o1', label: '헤드셋 마이크' },
];

describe('장치 설정', () => {
  it('모르는 값은 기본값으로, 음량은 범위 안으로', () => {
    expect(sanitizeDevices(undefined)).toEqual(DEFAULT_DEVICES);
    expect(sanitizeDevices({ input: { id: 'a1', label: 'x' }, output: { id: 3 }, inputVolume: 999, outputVolume: -5 })).toEqual({
      input: { id: 'a1', label: 'x' },
      output: null,
      inputVolume: 200,
      outputVolume: 0,
    });
    // 예전 설정 파일(장치 칸 없음)도 읽습니다.
    expect(sanitizeMedia({ audio: { codec: 'opus' } }).devices).toEqual(DEFAULT_DEVICES);
  });

  it('ID로 찾고, 바뀌었으면 이름으로 찾고(종류가 같고 가짜 장치는 빼고), 없으면 기본', () => {
    expect(resolveDevice(list, 'audioinput', null)).toBeUndefined();
    expect(resolveDevice(list, 'audioinput', { id: 'a2', label: '아무거나' })).toBe('a2');
    expect(resolveDevice(list, 'audioinput', { id: 'gone', label: '헤드셋 마이크' })).toBe('a1');
    expect(resolveDevice(list, 'audiooutput', { id: 'gone', label: '헤드셋 마이크' })).toBe('o1');
    expect(resolveDevice(list, 'audioinput', { id: 'gone', label: '빼 둔 마이크' })).toBeUndefined();
    expect(isAliasDevice(list[0])).toBe(true);
    expect(isAliasDevice(list[2])).toBe(false);
  });

  it('고른 마이크는 정확히 그 장치로 엽니다', () => {
    expect(micConstraints(DEFAULT_MEDIA.audio).deviceId).toBeUndefined();
    expect(micConstraints(DEFAULT_MEDIA.audio, 'a1').deviceId).toEqual({ exact: 'a1' });
  });
});
