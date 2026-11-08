// 앱 타입(types.ts)과 데몬 메서드 표(crates/yangpa-rpc)가 어긋나지 않게 지키는 시험.
// methods.generated.json은 `cargo test -p yangpa-rpc --test desktop_contract`가 메서드 표에서 만듭니다.

import { describe, expect, it } from 'vitest';
import { PERMISSION_LABELS } from '../lib/misc';
import generated from './methods.generated.json';
import { METHOD_NAMES, NOTIFICATION_TYPES } from './methods';
import { PERMISSION_BITS } from './types';

type Spec = (typeof generated.methods)[number];

/**
 * 데몬 쪽 구현이 함께 들어오는 중인 계약. 앱이 먼저 타입을 갖고 데몬이 뒤따를 때만 여기 적습니다(생성 파일에 아직 없어도
 * 되지만, 들어오면 적은 범위·공간 규칙과 같아야 함). 모더레이션·확정 기기 지정은 모두 데몬에 들어와 비었습니다.
 */
const PENDING_METHODS: Record<string, { scope: string; space: string; destructive: boolean }> = {};
const PENDING_NOTIFICATIONS: string[] = [];
const PENDING_PERMISSIONS: Array<[string, number]> = [];

const byName = new Map<string, Spec>(generated.methods.map((m) => [m.name, m]));

describe('API 계약(methods.generated.json)', () => {
  it('데몬의 모든 메서드를 types.ts가 안다', () => {
    const known = new Set<string>(METHOD_NAMES);
    const missing = generated.methods.map((m) => m.name).filter((n) => !known.has(n));
    expect(missing, 'types.ts의 Methods에 없는 데몬 메서드').toEqual([]);
  });

  it('types.ts에는 데몬에 없는 메서드가 없다(합의된 계약 제외)', () => {
    const extra = METHOD_NAMES.filter((n) => !byName.has(n) && !(n in PENDING_METHODS));
    expect(extra, '데몬 메서드 표에 없는 메서드').toEqual([]);
  });

  it('합의된 계약 메서드가 데몬에 들어오면 범위·공간 규칙이 같다', () => {
    for (const [name, want] of Object.entries(PENDING_METHODS)) {
      expect(METHOD_NAMES as string[], `${name}이(가) types.ts에 있어야 함`).toContain(name);
      const have = byName.get(name);
      if (!have) continue;
      expect({ scope: have.scope, space: have.space, destructive: have.destructive !== 'no' }, name).toEqual(want);
    }
  });

  it('메서드 이름이 겹치지 않고 표 형식이 맞다', () => {
    expect(new Set(generated.methods.map((m) => m.name)).size).toBe(generated.methods.length);
    for (const m of generated.methods) {
      expect(['public', 'read', 'write', 'admin', 'settings', 'owner'], m.name).toContain(m.scope);
      expect(['no', 'always', 'when_others'], m.name).toContain(m.destructive);
    }
  });

  it('알림 종류가 같다', () => {
    const daemon = new Set(generated.notifications);
    const app = new Set<string>(NOTIFICATION_TYPES);
    expect([...daemon].filter((t) => !app.has(t)), '앱이 모르는 알림').toEqual([]);
    expect(
      [...app].filter((t) => !daemon.has(t) && !PENDING_NOTIFICATIONS.includes(t)),
      '데몬이 보내지 않는 알림',
    ).toEqual([]);
  });

  it('권한 이름·비트가 같고 모두 화면 이름이 있다', () => {
    const app = new Map(PERMISSION_BITS.map(([n, b]) => [n, b]));
    for (const p of generated.permissions) expect(app.get(p.name), p.name).toBe(p.bit);
    const daemon = new Set(generated.permissions.map((p) => p.name));
    for (const [n, b] of PERMISSION_BITS) {
      if (daemon.has(n)) continue;
      expect(PENDING_PERMISSIONS, `데몬에 없는 권한 ${n}`).toContainEqual([n, b]);
    }
    for (const [n] of PERMISSION_BITS) expect(PERMISSION_LABELS[n], `${n} 화면 이름`).toBeTruthy();
  });
});
