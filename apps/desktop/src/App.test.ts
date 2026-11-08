// 시작 화면: Tor가 준비될 때까지 진행을 보이고, 준비되면 연다(친구 연결은 친구마다 '연결 중').

import { describe, expect, it } from 'vitest';
import type { NetStatus } from './api/types';
import { startupProgress } from './App';

const net = (p: Partial<NetStatus>): NetStatus => ({
  peer_id: 'p',
  mode: 'tor',
  listen_addrs: [],
  relay_addrs: [],
  nat: 'disabled',
  connections: 0,
  external_addrs: [],
  peers: 0,
  ...p,
});

describe('시작 화면 진행', () => {
  it('Tor 준비 중이면 그 진행률', () => {
    expect(startupProgress(net({ bootstrap_progress: 40, connecting: ['u'] }))).toEqual({ pct: 40, text: 'Tor 회로 만드는 중… 40%' });
  });
  it('Tor가 준비되면 친구 연결을 기다리지 않고 연다', () => {
    expect(startupProgress(net({ bootstrap_progress: 100, connecting: ['u'] }))).toBeNull();
    expect(startupProgress(net({ mode: 'normal' }))).toBeNull();
  });
  it('오프라인 모드는 Tor를 기다리지 않는다', () => {
    expect(startupProgress(net({ offline: true }))).toBeNull();
  });
  it('상태를 아직 못 읽었거나 Arti가 시작 전이면 기다린다', () => {
    expect(startupProgress(null)?.pct).toBe(0);
    expect(startupProgress(net({}))?.pct).toBe(0);
  });
});
