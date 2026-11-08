import { describe, expect, it } from 'vitest';
import type { VoiceParticipant } from '../api/types';
import { TOR_CALL_NOTE, isLinkUp, isPeerConnected, isTorCall, needsTorScreen } from './policy';

describe('통화 길', () => {
  it('직접 연결로 띄운 데몬은 WebRTC', () => {
    expect(isTorCall({ mode: 'normal' })).toBe(false);
    expect(isTorCall(null)).toBe(false);
  });

  it('Tor로 도는 기기의 일반 채널 통화는 Tor 통화', () => {
    expect(isTorCall({ mode: 'tor' })).toBe(true);
    expect(TOR_CALL_NOTE).toContain('IP');
  });

  it('참가한 P2P 전용 채널은 Tor여도 직접 통화한다', () => {
    expect(isTorCall({ mode: 'tor' }, true)).toBe(false);
  });
});

describe('Tor 비상용 화면 경고', () => {
  const p = (device: string, extra: Partial<VoiceParticipant> = {}): VoiceParticipant => ({
    user: `u-${device}`,
    device,
    muted: false,
    deafened: false,
    screen: false,
    ...extra,
  });
  const me = p('A', { mine: true });

  it('Tor로 잇는 상대가 있거나 아직 아무도 없으면 경고한다', () => {
    expect(needsTorScreen({ topology: 'tor', participants: [me] })).toBe(true);
    expect(needsTorScreen({ topology: 'tor', participants: [me, p('B', { direct: true }), p('C')] })).toBe(true);
  });

  it('모두 서로 신뢰한 상대이거나 메시 통화면 경고하지 않는다', () => {
    expect(needsTorScreen({ topology: 'tor', participants: [me, p('B', { direct: true })] })).toBe(false);
    expect(needsTorScreen({ topology: 'mesh', participants: [me, p('B')] })).toBe(false);
  });
});

describe('이어진 상태', () => {
  it('Tor 통화의 상대(tor)도 WebRTC connected처럼 이어진 것으로 본다', () => {
    expect(isLinkUp('connected')).toBe(true);
    expect(isLinkUp('tor')).toBe(true);
    expect(isLinkUp('connecting')).toBe(false);
    expect(isLinkUp(undefined)).toBe(false);
  });

  it('화면 표시: 신호 전·ICE 중은 연결 중, Tor는 첫 소리가 와야 연결됨', () => {
    const p = { device: 'D', muted: false };
    expect(isPeerConnected({ connection: {} }, p)).toBe(false);
    expect(isPeerConnected({ connection: { D: 'connecting' } }, p)).toBe(false);
    expect(isPeerConnected({ connection: { D: 'connected' } }, p)).toBe(true);
    expect(isPeerConnected({ connection: { D: 'tor' } }, p)).toBe(false);
    expect(isPeerConnected({ connection: { D: 'tor' }, heard: ['E'] }, p)).toBe(false);
    expect(isPeerConnected({ connection: { D: 'tor' }, heard: ['D'] }, p)).toBe(true);
    // 음소거한 Tor 상대는 소리로 확인할 수 없어 링크만 봅니다.
    expect(isPeerConnected({ connection: { D: 'tor' } }, { ...p, muted: true })).toBe(true);
  });
});
