import { describe, expect, it } from 'vitest';
import type { SpaceView } from '../api/types';
import { byLevelDesc, canEditRole, canUseExternalEmoji, linkLacksAddress, rankOf, rememberRecent, roleLevel } from './misc';

function space(allow: boolean, perms: string[]): SpaceView {
  return {
    space: 's1',
    kind: 'crew',
    settings: { name: '크루', description: '', icon: null, allow_external_emoji: allow },
    owner: 'u1',
    committer: 'd1',
    members: [],
    roles: [],
    channels: [],
    emojis: [],
    my_permissions: perms,
    pending_commit: false,
    dormant: false,
    trusted: false,
  };
}

describe('다른 공간 이모지 사용 가능 여부', () => {
  it('공간 설정과 역할 권한이 둘 다 있어야 한다(동기화 규칙과 같음)', () => {
    expect(canUseExternalEmoji(space(true, ['use_external_emoji']))).toBe(true);
    expect(canUseExternalEmoji(space(false, ['use_external_emoji']))).toBe(false);
    expect(canUseExternalEmoji(space(true, ['send_messages']))).toBe(false);
  });

  it('설정이 꺼져 있으면 관리자도 못 쓰고, 공간을 모르면 쓰지 않는다', () => {
    expect(canUseExternalEmoji(space(false, ['administrator']))).toBe(false);
    expect(canUseExternalEmoji(space(true, ['administrator']))).toBe(true);
    expect(canUseExternalEmoji(undefined)).toBe(false);
  });
});

describe('최근 값 기억', () => {
  it('상한을 넘으면 가장 오래된 키부터 지우고, 다시 넣은 키는 최근으로 옮긴다', () => {
    let m: Record<string, string> = {};
    for (let i = 0; i < 5; i++) m = rememberRecent(m, `s${i}`, `c${i}`, 3);
    expect(Object.keys(m)).toEqual(['s2', 's3', 's4']);
    m = rememberRecent(m, 's2', 'x', 3);
    expect(Object.entries(m)).toEqual([
      ['s3', 'c3'],
      ['s4', 'c4'],
      ['s2', 'x'],
    ]);
    m = rememberRecent(m, 's9', 'y', 3);
    expect(Object.keys(m)).toEqual(['s4', 's2', 's9']);
  });
});

describe('링크에 찾아올 주소가 있는지', () => {
  it('직접 연결 데몬에 중계 주소도 onion도 없을 때만 경고하고, 상태를 모르면 경고하지 않는다', () => {
    expect(linkLacksAddress({ mode: 'normal', relay_addrs: [] })).toBe(true);
    expect(linkLacksAddress({ mode: 'normal', relay_addrs: ['/ip4/1.2.3.4/tcp/4001/p2p/R/p2p-circuit/p2p/M'] })).toBe(false);
    expect(linkLacksAddress({ mode: 'normal', relay_addrs: [], onion_addr: '/onion3/abc:4001' })).toBe(false);
    // Tor로 돌면 onion 주소는 상태에 아직 없어도 링크에 실립니다.
    expect(linkLacksAddress({ mode: 'tor', relay_addrs: [] })).toBe(false);
    expect(linkLacksAddress(null)).toBe(false);
  });
});

describe('역할 순위(관리자 역할은 일반 역할보다 위)', () => {
  const ADMIN = 2 ** 31;
  const adm = { id: 'r-adm', name: '관리자', color: 0, rank: 1, permissions: ADMIN, automation: false };
  const reg = { id: 'r-reg', name: '고인물', color: 0, rank: 20, permissions: 0, automation: false };
  const head = { id: 'r-head', name: '총관리자', color: 0, rank: 5, permissions: ADMIN + 1, automation: false };
  const view: SpaceView = {
    ...space(true, ['administrator']),
    roles: [reg, adm, head],
    members: [{ user: 'u2', roles: ['r-adm'] } as unknown as SpaceView['members'][number]],
  };

  it('관리자 역할은 rank가 낮아도 일반 역할보다 높습니다', () => {
    expect(roleLevel(adm)).toBeGreaterThan(roleLevel(reg));
    expect(roleLevel(head)).toBeGreaterThan(roleLevel(adm));
    expect([reg, adm, head].sort(byLevelDesc).map((r) => r.id)).toEqual(['r-head', 'r-adm', 'r-reg']);
  });

  it('맨 아래 순위 관리자도 일반 역할은 고치고, 더 높은 관리자 역할은 못 고칩니다', () => {
    expect(rankOf(view, 'u2')).toBe(roleLevel(adm));
    expect(canEditRole(view, 'u2', reg)).toBe(true);
    expect(canEditRole(view, 'u2', head)).toBe(false);
    expect(canEditRole(view, 'u2', adm)).toBe(false);
  });
});
