import { describe, expect, it } from 'vitest';
import { ANONYMOUS_USER, type SpaceView } from '../api/types';
import { anonCallEnded, anonName, anonParticipantLabel, canSetAnonymous, isAnonymousMessage, isMine, participantUsers } from './anon';

const ME = 'M'.repeat(32);

describe('익명 글 표시 규칙', () => {
  it('ANONYMOUS_USER는 32바이트 0의 base58이다', () => {
    expect(ANONYMOUS_USER).toBe('1'.repeat(32));
  });

  it('익명 글은 anonymous 표시나 가린 작성자로 알아본다', () => {
    expect(isAnonymousMessage({ author: ANONYMOUS_USER })).toBe(true);
    expect(isAnonymousMessage({ author: ME, anonymous: true })).toBe(true);
    expect(isAnonymousMessage({ author: ME })).toBe(false);
  });

  it('내 글인지는 mine으로 알고, mine이 없는 옛 데몬이면 작성자로 비교한다', () => {
    expect(isMine({ author: ANONYMOUS_USER, mine: true }, ME)).toBe(true);
    expect(isMine({ author: ANONYMOUS_USER }, ME)).toBe(false);
    expect(isMine({ author: ME }, ME)).toBe(true);
    expect(isMine({ author: 'O'.repeat(32) }, ME)).toBe(false);
    // 내 ID를 모를 때 익명 자리를 내 글로 잘못 보지 않습니다.
    expect(isMine({ author: ANONYMOUS_USER }, ANONYMOUS_USER)).toBe(false);
  });

  it('익명 스위치는 크루의 카테고리가 아닌 채널에서 채널 관리 권한이 있을 때만', () => {
    const view = (kind: string, perms: string[]) => ({ kind, my_permissions: perms }) as unknown as SpaceView;
    expect(canSetAnonymous(view('crew', ['manage_channels']), 'text')).toBe(true);
    expect(canSetAnonymous(view('crew', ['administrator']), 'voice')).toBe(true);
    expect(canSetAnonymous(view('crew', ['administrator']), 'category')).toBe(false);
    expect(canSetAnonymous(view('crew', ['send_messages']), 'text')).toBe(false);
    expect(canSetAnonymous(view('space', ['administrator']), 'text')).toBe(false);
    expect(canSetAnonymous(view('self_space', ['administrator']), 'text')).toBe(false);
  });
});

describe('익명 통화 참가자', () => {
  it("번호로 '익명 N', 내 줄은 '(나)'를 붙이고 익명이 아니면 null", () => {
    expect(anonParticipantLabel({ user: ANONYMOUS_USER, anon: 2 })).toBe('익명 2');
    expect(anonParticipantLabel({ user: ANONYMOUS_USER, anon: 1, mine: true })).toBe('익명 1 (나)');
    expect(anonParticipantLabel({ user: ANONYMOUS_USER, anon: 3 }, true)).toBe('익명 3 (나)');
    expect(anonParticipantLabel({ user: ANONYMOUS_USER })).toBe('익명');
    expect(anonParticipantLabel({ user: ME, mine: true })).toBeNull();
  });

  it('데몬이 붙인 가명이 있으면 번호 대신 가명을 쓴다', () => {
    expect(anonParticipantLabel({ user: ANONYMOUS_USER, anon: 2, anon_name: '용감한 수달' })).toBe('용감한 수달');
    expect(anonParticipantLabel({ user: ANONYMOUS_USER, anon: 1, anon_name: '졸린 판다', mine: true })).toBe('졸린 판다 (나)');
    expect(anonName({ anon_name: '졸린 판다' })).toBe('졸린 판다');
    expect(anonName({})).toBe('익명');
  });

  it('채널 보기의 참가자 수는 익명 참가자를 하나로 합치지 않는다', () => {
    const a = { user: ANONYMOUS_USER };
    expect(participantUsers([a, a, a])).toEqual([ANONYMOUS_USER, ANONYMOUS_USER, ANONYMOUS_USER]);
    // 실제 사용자는 기기가 여럿이어도 한 번만 셉니다.
    expect(participantUsers([{ user: 'u1' }, { user: 'u1' }, { user: 'u2' }])).toEqual(['u1', 'u2']);
  });

  it('익명으로 들어간 통화는 채널이 익명이 아니게 되거나, 번호 없는 참가자 목록이 오면 끝난다', () => {
    const anonCall = { anonymous: true };
    expect(anonCallEnded(anonCall, { channel: { anonymous: false } })).toBe(true);
    expect(anonCallEnded(anonCall, { channel: {} })).toBe(true);
    expect(anonCallEnded(anonCall, { channel: { anonymous: true } })).toBe(false);
    expect(anonCallEnded(anonCall, { roster: [{ anon: 1 }, { anon: 2 }] })).toBe(false);
    expect(anonCallEnded(anonCall, { roster: [{}, {}] })).toBe(true);
    // 빈 목록만으로는 판단하지 않습니다(참가 직전에 온 옛 목록일 수 있음).
    expect(anonCallEnded(anonCall, { roster: [] })).toBe(false);
    // 익명이 아닌 통화는 상관없습니다.
    expect(anonCallEnded({ anonymous: false }, { channel: { anonymous: false } })).toBe(false);
    expect(anonCallEnded(null, { channel: {} })).toBe(false);
  });
});
