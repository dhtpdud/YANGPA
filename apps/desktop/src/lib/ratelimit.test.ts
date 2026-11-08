// 범위별 속도 제한 입력·표시와 업로드 줄 고르기. 비우면 제한 없음(null)이고 0은 그 방향을 막습니다(올리지·받지 않음).

import { describe, expect, it } from 'vitest';
import { ANONYMOUS_USER, type FriendView, type SpaceView, type UploadRate } from '../api/types';
import { describeScope, findLimit, limitToInput, parseLimitInput, scopeKey, uploadsFor, userName, withLimit } from './ratelimit';

const U1 = 'A'.repeat(32);
const U2 = 'B'.repeat(32);

const views = {
  S: {
    space: 'S',
    kind: 'crew',
    settings: { name: '게임 모임' },
    members: [{ user: U1, profile: { name: '민지' }, nickname: '민지짱', roles: [] }],
    channels: [{ channel: 'C', settings: { name: '잡담' } }],
  },
} as unknown as Record<string, SpaceView>;
const friends = [{ user: U2, space: 'D', profile: { name: '하준' } }] as unknown as FriendView[];
const ctx = { views, friends };

describe('속도 제한 입력', () => {
  it('비우면 제한 없음, 0·음수·글자는 오류, 단위를 곱해 바이트로', () => {
    expect(parseLimitInput('', 'KiB')).toEqual({ ok: true, bps: null });
    expect(parseLimitInput('   ', 'MiB')).toEqual({ ok: true, bps: null });
    expect(parseLimitInput('0', 'KiB')).toEqual({ ok: true, bps: 0 });
    expect(parseLimitInput('0.0', 'MiB')).toEqual({ ok: true, bps: 0 });
    expect(parseLimitInput('-1', 'KiB')).toEqual({ ok: false, error: '0 이상의 숫자를 넣으세요' });
    expect(parseLimitInput('빠르게', 'KiB').ok).toBe(false);
    expect(parseLimitInput('512', 'KiB')).toEqual({ ok: true, bps: 512 * 1024 });
    expect(parseLimitInput(' 1.5 ', 'MiB')).toEqual({ ok: true, bps: 1.5 * 1024 * 1024 });
  });

  it('저장된 값은 1MiB/s 이상이면 MiB, 아니면 KiB로 채운다', () => {
    expect(limitToInput(null)).toEqual({ text: '', unit: 'KiB' });
    expect(limitToInput(undefined)).toEqual({ text: '', unit: 'KiB' });
    expect(limitToInput(512 * 1024)).toEqual({ text: '512', unit: 'KiB' });
    expect(limitToInput(2.5 * 1024 * 1024)).toEqual({ text: '2.5', unit: 'MiB' });
    expect(limitToInput(1000)).toEqual({ text: '0.98', unit: 'KiB' });
  });
});

describe('범위', () => {
  it('키로 찾고, 바꾸고, 둘 다 null이면 뺀다', () => {
    const crew = { kind: 'crew', space: 'S' } as const;
    const user = { kind: 'user', user: U1 } as const;
    expect(scopeKey(crew)).not.toBe(scopeKey({ kind: 'channel', space: 'S', channel: 'C' }));
    let list = withLimit([], crew, 100, null);
    list = withLimit(list, user, null, 200);
    expect(findLimit(list, { kind: 'crew', space: 'S' })).toEqual({ scope: crew, up_bps: 100, down_bps: null });
    list = withLimit(list, crew, 300, 400);
    expect(list).toHaveLength(2);
    expect(findLimit(list, crew)?.up_bps).toBe(300);
    list = withLimit(list, crew, null, null);
    expect(list).toEqual([{ scope: user, up_bps: null, down_bps: 200 }]);
    expect(findLimit(null, crew)).toBeUndefined();
  });

  it('크루 이름, #채널 · 크루, 사람 이름(별명 → 친구 프로필 → 알 수 없음)', () => {
    expect(describeScope({ kind: 'crew', space: 'S' }, ctx)).toEqual({ kind: '크루', label: '게임 모임' });
    expect(describeScope({ kind: 'channel', space: 'S', channel: 'C' }, ctx)).toEqual({ kind: '채널', label: '#잡담 · 게임 모임' });
    expect(describeScope({ kind: 'crew', space: 'D' }, ctx).label).toBe('1:1 하준');
    expect(describeScope({ kind: 'user', user: U1 }, ctx)).toEqual({ kind: '사람', label: '민지짱' });
    expect(describeScope({ kind: 'user', user: U2 }, ctx).label).toBe('하준');
    expect(describeScope({ kind: 'user', user: 'Z'.repeat(32) }, ctx).label).toBe('알 수 없는 사용자');
    expect(userName(ANONYMOUS_USER, ctx, 'S')).toBe('익명');
  });
});

describe('이 첨부를 받는 사람', () => {
  it('공간·블롭이 맞는 줄만 빠른 순으로, 없으면 늘 같은 빈 배열', () => {
    const r = (user: string, bps: number, blob = 'X', space = 'S'): UploadRate => ({ space, blob, user, bps });
    const ups = [r(U1, 10), r(U2, 30), r(U1, 99, 'Y'), r(U2, 99, 'X', 'T')];
    expect(uploadsFor(ups, 'S', 'X').map((u) => u.bps)).toEqual([30, 10]);
    const a = uploadsFor(ups, 'S', 'Q');
    expect(a).toEqual([]);
    expect(uploadsFor(undefined, 'S', 'X')).toBe(a);
    expect(uploadsFor([], 'S', 'X')).toBe(a);
  });
});
