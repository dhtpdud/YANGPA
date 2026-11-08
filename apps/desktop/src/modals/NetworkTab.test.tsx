// @vitest-environment happy-dom
// 네트워크 탭: 앱은 늘 Tor로 돌아 Tor 모드·토르망 스위치가 없고, 모든 연결이 Tor로 오간다고만 알립니다. 회로를 만드는 중이면
// 진행률을, 직접 연결로 띄운 데몬이면 IP가 보인다는 경고를 보입니다. IP 숨기기·저전력·상시 노드 설정은 없어졌고, 앱은 이 기기의
// IP 주소(듣는 주소·중계 주소)를 보이지 않습니다. STUN 서버 목록과 Tor 파일 회로 수(기본 0, 0에서 올릴 때만 경고)를
// 여기서 고칩니다. TURN 설정은 없습니다.

import { act } from 'react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import type { ApiClient } from '../api/client';
import type { NetStatus, StunServers } from '../api/types';
import { useStore } from '../store/store';
import { button, cleanup, click, render } from '../test/dom';
import { BRIDGES_HELP, bridgeEmoji } from './Bridges';
import { CHANNEL_TOPICS_HELP, CHANNEL_TOPICS_WARNING } from './ChannelTopics';
import { FILE_CIRCUITS_HELP, FILE_CIRCUITS_WARNING } from './FileCircuits';
import { DIRECT_MODE_WARNING, DirectModeWarning, STUN_HELP } from './StunServers';
import { NetworkTab } from './UserSettings';

const LISTEN = '/ip4/192.168.0.23/tcp/4001';
const RELAY = '/ip4/203.0.113.7/tcp/4001/p2p/12D3KooWRelay/p2p-circuit/p2p/12D3KooWTest';
const EXTERNAL = '/ip4/198.51.100.9/udp/4001/quic-v1';

const NET: NetStatus = {
  peer_id: '12D3KooWTest',
  mode: 'tor',
  listen_addrs: [LISTEN],
  relay_addrs: [RELAY],
  nat: 'disabled',
  connections: 1,
  bootstrap_progress: 100,
  external_addrs: [EXTERNAL],
  onion_addr: '/onion3/abcdefghijklmnopqrstuvwxyz234567abcdefghijklmnopqrstuvwx:4001',
  peers: 1,
};

const DEFAULT_STUN = ['stun.l.google.com:19302', 'stun1.l.google.com:19302', 'stun.cloudflare.com:3478'];

function setup(patch: Partial<NetStatus>) {
  const net = { ...NET, ...patch };
  let stun: StunServers = { servers: DEFAULT_STUN, default: true };
  let fileCircuits = 0;
  let channelTopics = false;
  let bridges: { enabled: boolean; kind: string; custom: string[] } = { enabled: false, kind: 'snowflake', custom: [] };
  const call = vi.fn(async (method: string, params?: unknown) => {
    if (method === 'net.status') return net;
    if (method === 'net.file_circuits') return { enabled: fileCircuits > 0, count: fileCircuits };
    if (method === 'net.set_file_circuits') fileCircuits = (params as { count: number }).count;
    if (method === 'net.bridges') return { ...bridges, available: true };
    if (method === 'net.set_bridges') bridges = params as typeof bridges;
    if (method === 'net.channel_topics') return { enabled: channelTopics };
    if (method === 'net.set_channel_topics') channelTopics = (params as { enabled: boolean }).enabled;
    if (method === 'net.peers') return [];
    if (method === 'net.stun_servers') return stun;
    if (method === 'net.set_stun_servers') {
      const servers = (params as { servers: string[] | null }).servers;
      stun = servers === null ? { servers: DEFAULT_STUN, default: true } : { servers, default: false };
    }
    return {};
  });
  useStore.setState({ client: { call } as unknown as ApiClient, net, modal: null, toasts: [], voice: null });
  return call;
}

const toggle = (el: HTMLElement, label: string) => el.querySelector<HTMLButtonElement>(`button[aria-label="${label}"]`);

/** React가 다루는 입력의 값을 바꿉니다. */
async function typeInto(input: HTMLInputElement | HTMLTextAreaElement, value: string) {
  await act(async () => {
    const proto = input instanceof HTMLTextAreaElement ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype;
    const set = Object.getOwnPropertyDescriptor(proto, 'value')!.set!;
    set.call(input, value);
    input.dispatchEvent(new Event('input', { bubbles: true }));
  });
}

const stunBox = (el: HTMLElement) => el.querySelector<HTMLTextAreaElement>('textarea[aria-label="STUN 서버"]')!;

afterEach(async () => {
  await cleanup();
  useStore.setState({ client: null, net: null, modal: null });
});

describe('네트워크 탭(Tor 전용)', () => {
  it('Tor 모드·토르망 스위치 없이 Tor 안내와 IP가 보이는 경우를 모두 알리고 어니언 주소를 보인다', async () => {
    setup({});
    const { el } = await render(<NetworkTab />);
    expect(toggle(el, 'Tor 모드')).toBeNull();
    expect(toggle(el, '토르망')).toBeNull();
    expect(el.textContent).not.toContain('토르망');
    expect(el.textContent).not.toContain('일반 모드');
    expect(el.textContent).toContain('연결은 기본으로 Tor로 오가서');
    expect(el.textContent).not.toContain('모든 연결은 Tor로 오갑니다');
    // README "앱이 바깥과 통신하는 곳" 표의 IP가 보이는 경우와 같아야 합니다.
    const ipCases = [...el.querySelectorAll('.tor-ip li')].map((li) => li.textContent ?? '');
    for (const [i, word] of ['Snowflake', '기기가 2대 이상', 'P2P 전용 채널', '신뢰 스위치', '크루 STUN', 'Patreon'].entries()) {
      expect(ipCases[i]).toContain(word);
    }
    expect(ipCases).toHaveLength(6);
    expect(el.textContent).toContain('어니언 주소: /onion3/');
    expect(el.textContent).not.toContain(DIRECT_MODE_WARNING);
    expect(el.textContent).not.toContain('Tor 회로 만드는 중');
  });

  it('회로를 만드는 중이면 진행률을 보인다', async () => {
    setup({ bootstrap_progress: 40, onion_addr: undefined });
    const { el } = await render(<NetworkTab />);
    expect(el.textContent).toContain('Tor 회로 만드는 중… 40%');
  });

  it('직접 연결로 띄운 데몬이면 IP가 보인다고 경고한다', async () => {
    setup({ mode: 'normal', nat: 'private', bootstrap_progress: undefined, onion_addr: undefined });
    const { el } = await render(<NetworkTab />);
    expect(el.textContent).toContain(DIRECT_MODE_WARNING);
    expect(el.textContent).toContain('직접 연결(Tor 없음)');
  });

  it('이 기기의 IP 주소(듣는 주소·중계 주소·외부 주소)를 어디에도 보이지 않는다', async () => {
    for (const mode of ['tor', 'normal'] as const) {
      setup({ mode });
      const { el, unmount } = await render(<NetworkTab />);
      await vi.waitFor(() => expect(stunBox(el).value).toContain('stun.cloudflare.com:3478'));
      for (const ip of ['192.168.0.23', '203.0.113.7', '198.51.100.9', '/ip4/', '/ip6/']) expect(el.textContent).not.toContain(ip);
      expect(el.textContent).not.toContain('듣는 주소');
      expect(el.textContent).not.toContain('중계 주소');
      await unmount();
    }
  });

  it('IP 숨기기·저전력·상시 노드 설정은 없다', async () => {
    const call = setup({});
    const { el } = await render(<NetworkTab />);
    expect(toggle(el, 'IP 숨기기')).toBeNull();
    expect(toggle(el, '저전력')).toBeNull();
    expect(toggle(el, '창을 내리면 저전력')).toBeNull();
    expect(toggle(el, 'IPFS 공개 노드')).toBeNull();
    expect(el.textContent).not.toContain('상시 노드');
    expect(el.textContent).not.toContain('IP 숨기기');
    expect(el.querySelector('textarea[aria-label="TURN 서버"]')).toBeNull();
    for (const m of ['net.bootstrap_list', 'net.ice_servers', 'net.set_tor', 'net.set_tor_open', 'net.turn']) {
      expect(call).not.toHaveBeenCalledWith(m, expect.anything());
    }
  });

  it('STUN 서버를 입력 칸 하나에 한 줄씩 보이고, 고쳐 저장하거나 기본 목록으로 되돌린다', async () => {
    const call = setup({});
    const { el } = await render(<NetworkTab />);
    await vi.waitFor(() => expect(stunBox(el).value).toBe(DEFAULT_STUN.join('\n')));
    expect(el.textContent).toContain(STUN_HELP);
    expect(el.textContent).toContain('기본 목록을 쓰는 중');
    expect(button(el, '기본 목록으로').disabled).toBe(true);
    // 따로 된 추가 칸·추가 버튼·빼기 버튼은 없습니다.
    expect(el.querySelector('input[placeholder="stun.example.org:3478"]')).toBeNull();
    expect(el.querySelector('button[aria-label="STUN 서버 빼기"]')).toBeNull();
    expect(button(el, '저장').disabled).toBe(true);

    await typeInto(stunBox(el), ' stun1.l.google.com:19302 \n\nstun.example.org:3479, stun.cloudflare.com:3478');
    await click(button(el, '저장'));
    expect(call).toHaveBeenCalledWith('net.set_stun_servers', {
      servers: ['stun1.l.google.com:19302', 'stun.example.org:3479', 'stun.cloudflare.com:3478'],
    });
    await vi.waitFor(() => expect(stunBox(el).value).toBe('stun1.l.google.com:19302\nstun.example.org:3479\nstun.cloudflare.com:3478'));
    expect(el.textContent).not.toContain('기본 목록을 쓰는 중');

    await click(button(el, '기본 목록으로'));
    expect(call).toHaveBeenCalledWith('net.set_stun_servers', { servers: null });
    await vi.waitFor(() => expect(el.textContent).toContain('기본 목록을 쓰는 중'));
    expect(stunBox(el).value).toBe(DEFAULT_STUN.join('\n'));
  });

  it('입력 칸을 비우고 저장하면 STUN을 쓰지 않는다', async () => {
    const call = setup({});
    const { el } = await render(<NetworkTab />);
    await vi.waitFor(() => expect(stunBox(el).value).not.toBe(''));
    await typeInto(stunBox(el), '  ');
    await click(button(el, '저장'));
    expect(call).toHaveBeenCalledWith('net.set_stun_servers', { servers: [] });
  });

  it('STUN 설명은 P2P 연결에만 쓰고 Tor 연결에는 쓰지 않는다고 말한다', () => {
    expect(STUN_HELP).toContain('P2P 연결');
    expect(STUN_HELP).toContain('Tor 연결에는 쓰지 않습니다');
  });

  it('신뢰 스위치를 켜면 그 사람·크루 멤버에게 IP가 보인다고 알리고, 건너뛴 IP 노출 경고를 다시 켤 수 있다', async () => {
    setup({});
    const savePrefs = vi.fn(async (_p: Record<string, unknown>) => undefined);
    const client = useStore.getState().client!;
    useStore.setState({
      client: { ...client, transport: { savePrefs } } as unknown as ApiClient,
      prefs: { ...useStore.getState().prefs, skipTrustWarning: true },
    });
    const { el } = await render(<NetworkTab />);
    expect(el.textContent).toContain('신뢰 스위치를 켰을 때: 신뢰한 사람·크루 멤버');
    const sw = toggle(el, '신뢰 스위치 IP 노출 경고')!;
    expect(sw.getAttribute('aria-checked')).toBe('false');
    await click(sw);
    expect(useStore.getState().prefs.skipTrustWarning).toBe(false);
    expect(savePrefs).toHaveBeenLastCalledWith(expect.objectContaining({ skipTrustWarning: false }));
    expect(toggle(el, '신뢰 스위치 IP 노출 경고')!.getAttribute('aria-checked')).toBe('true');
  });

  it('Tor 파일 회로는 기본 0개이고, 0에서 올릴 때만 익명성 경고를 거쳐 저장하며 바꾸거나 끄는 것은 바로 저장한다', async () => {
    const call = setup({});
    const { el } = await render(<NetworkTab />);
    const pick = (n: number) => el.querySelector<HTMLButtonElement>(`[aria-label="Tor 파일 회로 수"] button:nth-child(${n + 1})`)!;
    await vi.waitFor(() => expect(pick(0).disabled).toBe(false));
    expect(el.textContent).toContain(FILE_CIRCUITS_HELP);
    expect(el.querySelectorAll('[aria-label="Tor 파일 회로 수"] button')).toHaveLength(5);
    expect(pick(0).getAttribute('aria-checked')).toBe('true');

    await click(pick(2));
    const modal = useStore.getState().modal;
    expect(modal?.kind).toBe('confirm');
    if (modal?.kind !== 'confirm') return;
    expect(modal.body).toBe(FILE_CIRCUITS_WARNING);
    expect(call).not.toHaveBeenCalledWith('net.set_file_circuits', expect.anything());
    await act(async () => {
      await modal.onConfirm();
    });
    expect(call).toHaveBeenCalledWith('net.set_file_circuits', { enabled: true, count: 2 });
    await vi.waitFor(() => expect(pick(2).getAttribute('aria-checked')).toBe('true'));

    // 이미 켜 둔 뒤 수를 바꾸거나 0으로 끄는 것은 경고 없이 바로.
    useStore.setState({ modal: null });
    await click(pick(4));
    expect(useStore.getState().modal).toBeNull();
    expect(call).toHaveBeenCalledWith('net.set_file_circuits', { enabled: true, count: 4 });
    await vi.waitFor(() => expect(pick(4).getAttribute('aria-checked')).toBe('true'));
    await click(pick(0));
    expect(useStore.getState().modal).toBeNull();
    expect(call).toHaveBeenCalledWith('net.set_file_circuits', { enabled: false, count: 0 });
    await vi.waitFor(() => expect(pick(0).getAttribute('aria-checked')).toBe('true'));
  });

  it('브리지는 기본 꺼짐이고, 켜기·내장 브리지 고르기·주소 넣기·지우기를 데몬에 맡긴다', async () => {
    const line = 'webtunnel [2001:db8::1]:443 0123456789ABCDEF0123456789ABCDEF01234567 url=https://example.com/abc ver=0.0.1';
    const call = setup({});
    const { el } = await render(<NetworkTab />);
    const sw = () => toggle(el, '브리지 사용')!;
    await vi.waitFor(() => expect(sw()).not.toBeNull());
    expect(el.textContent).toContain(BRIDGES_HELP);
    expect(el.textContent).toContain('내장 브리지: Snowflake');
    expect(sw().getAttribute('aria-checked')).toBe('false');

    await click(sw());
    expect(call).toHaveBeenCalledWith('net.set_bridges', { enabled: true, kind: 'snowflake', custom: [] });
    await vi.waitFor(() => expect(sw().getAttribute('aria-checked')).toBe('true'));

    await click(button(el, '내장 브리지 중 하나 선택'));
    await click(button(el, 'obfs4'));
    await click(button(el, '이 브리지 사용'));
    expect(call).toHaveBeenCalledWith('net.set_bridges', { enabled: true, kind: 'obfs4', custom: [] });

    await click(button(el, '이미 알고 있는 브리지 주소 입력'));
    await typeInto(el.querySelector<HTMLTextAreaElement>('textarea[aria-label="브리지 주소"]')!, `\n  ${line}\n\n`);
    await click(button(el, '저장하고 켜기'));
    expect(call).toHaveBeenCalledWith('net.set_bridges', { enabled: true, kind: 'custom', custom: [line] });
    await vi.waitFor(() => expect(el.textContent).toContain(line));
    expect(el.textContent).toContain(bridgeEmoji(line).join(' '));

    // 쓰던 직접 줄을 다 지우면 끕니다(직접 넣은 브리지가 없으면 켤 수 없음).
    await click(toggle(el, '브리지 지우기')!);
    expect(call).toHaveBeenCalledWith('net.set_bridges', { enabled: false, kind: 'custom', custom: [] });
  });

  it('채널 묶음 토픽은 기본 꺼짐이고, 켤 때만 확인을 거쳐 저장하며 끄는 것은 바로 저장한다', async () => {
    const call = setup({});
    const { el } = await render(<NetworkTab />);
    const sw = () => el.querySelector<HTMLButtonElement>('[aria-label="채널 묶음 토픽"]')!;
    await vi.waitFor(() => expect(sw().disabled).toBe(false));
    expect(el.textContent).toContain(CHANNEL_TOPICS_HELP);
    expect(sw().getAttribute('aria-checked')).toBe('false');

    await click(sw());
    const modal = useStore.getState().modal;
    expect(modal?.kind).toBe('confirm');
    if (modal?.kind !== 'confirm') return;
    expect(modal.body).toBe(CHANNEL_TOPICS_WARNING);
    expect(call).not.toHaveBeenCalledWith('net.set_channel_topics', expect.anything());
    await act(async () => {
      await modal.onConfirm();
    });
    expect(call).toHaveBeenCalledWith('net.set_channel_topics', { enabled: true });
    await vi.waitFor(() => expect(sw().getAttribute('aria-checked')).toBe('true'));

    useStore.setState({ modal: null });
    await click(sw());
    expect(useStore.getState().modal).toBeNull();
    expect(call).toHaveBeenCalledWith('net.set_channel_topics', { enabled: false });
    await vi.waitFor(() => expect(sw().getAttribute('aria-checked')).toBe('false'));
  });

  it('파일 회로 경고는 가드 찾기 위험과 상대도 켜야 쓰인다는 것을 말한다', () => {
    expect(FILE_CIRCUITS_WARNING).toContain('가드');
    expect(FILE_CIRCUITS_WARNING).toContain('vanguards');
    expect(FILE_CIRCUITS_WARNING).toContain('상대도 이 설정을 1 이상으로 두었을 때만');
  });

  it('직접 연결 경고는 IP 노출과 앱이 띄운 데몬은 늘 Tor라는 것을 말한다', async () => {
    const r = await render(<DirectModeWarning />);
    expect(r.el.textContent).toBe(DIRECT_MODE_WARNING);
    expect(DIRECT_MODE_WARNING).toContain('IP 주소가 보입니다');
    expect(DIRECT_MODE_WARNING).toContain('늘 Tor');
  });
});
