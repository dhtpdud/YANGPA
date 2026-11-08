// @vitest-environment happy-dom
// 사이드바 커밋 대기 표시: 이 기기가 확정 기기면 진행 중으로, 아니면 확정 기기를 기다리는 것으로 보입니다. 옛 키로
// 보낸 글이 있으면(키 회전 대기) 그 경고가 먼저 보입니다.
// 채널 줄: P2P 전용 채널은 이름 앞에 ‘P2P 전용 채널’ 표시를 붙입니다. 익명 채널은 ‘익명 채널’ 표시를 붙이고,
// 음성 참가자를 '익명 N'으로(참가자 목록이 없으면 수만큼 '익명') 보입니다.

import { afterEach, describe, expect, it } from 'vitest';
import { ANONYMOUS_USER, type ChannelView, type SpaceView } from '../api/types';
import { useStore } from '../store/store';
import { cleanup, render } from '../test/dom';
import { ChannelRow, CommitNote } from './Sidebar';

const view = (pending: boolean, committer: string) => ({ space: 'S', kind: 'crew', pending_commit: pending, committer }) as unknown as SpaceView;

afterEach(async () => {
  await cleanup();
  useStore.setState({ myDevice: null, voiceRosters: {} });
});

describe('커밋 대기 표시', () => {
  it('대기가 없으면 아무것도 그리지 않는다', async () => {
    useStore.setState({ myDevice: 'd1' });
    const { el } = await render(<CommitNote space={view(false, 'd1')} />);
    expect(el.textContent).toBe('');
  });

  it('이 기기가 확정 기기면 진행 중으로 보인다', async () => {
    useStore.setState({ myDevice: 'd1' });
    const { el } = await render(<CommitNote space={view(true, 'd1')} />);
    expect(el.textContent).toContain('키 그룹에 반영하는 중');
    expect(el.querySelector('.sb-note')?.getAttribute('title')).toContain('확정 기기');
  });

  it('다른 기기가 확정 기기면 기다린다고만 보이고 돌아가는 표시가 없다', async () => {
    useStore.setState({ myDevice: 'd1' });
    const { el } = await render(<CommitNote space={view(true, 'd2')} />);
    expect(el.textContent).toContain('확정 기기가 접속하면');
    expect(el.querySelector('.sb-note')?.children.length).toBe(0);
  });

  it('옛 키로 보낸 글이 있으면 키 회전 대기 경고가 커밋 대기보다 먼저 보인다', async () => {
    useStore.setState({ myDevice: 'd1' });
    const space = { ...view(true, 'd1'), key_rotation_pending: true } as SpaceView;
    const { el } = await render(<CommitNote space={space} />);
    expect(el.textContent).toContain('키 회전 대기 중');
    expect(el.querySelector('.sb-note')?.getAttribute('title')).toContain('엄격한 키 회전');
  });
});

describe('채널 줄의 P2P 전용 표시', () => {
  const space = { space: 'S', kind: 'crew', members: [], my_permissions: [] } as unknown as SpaceView;
  const row = (p2p: boolean, kind: 'text' | 'voice' = 'text') =>
    ({
      channel: `C-${kind}`,
      unread: 0,
      api_read_allowed: false,
      p2p,
      p2p_joined: false,
      settings: { name: '직접', kind, topic: '', security: 'normal', join_mode: 'always', ttl: { mode: 'none' }, overwrites: [] },
    }) as unknown as ChannelView;

  it('P2P 전용 채널은 이름 앞에 표시가 있고, 아닌 채널에는 없다', async () => {
    const { el } = await render(<ChannelRow space={space} c={row(true)} active={false} />);
    const icon = el.querySelector('[aria-label="P2P 전용 채널"]');
    expect(icon).not.toBeNull();
    expect(icon?.getAttribute('title')).toBe('P2P 전용 채널');
    // 이름 앞에 옵니다.
    expect(icon?.nextElementSibling?.classList.contains('ch-name')).toBe(true);
    await cleanup();
    const plain = await render(<ChannelRow space={space} c={row(false)} active={false} />);
    expect(plain.el.querySelector('[aria-label="P2P 전용 채널"]')).toBeNull();
  });

  it('통화 채널에도 붙는다', async () => {
    const { el } = await render(<ChannelRow space={space} c={row(true, 'voice')} active={false} />);
    expect(el.querySelector('[aria-label="P2P 전용 채널"]')).not.toBeNull();
  });
});

describe('채널 줄의 새 글 힌트', () => {
  const space = { space: 'S', kind: 'crew', members: [], my_permissions: [] } as unknown as SpaceView;
  const row = (unread: number, has_new: boolean) =>
    ({
      channel: 'C',
      unread,
      has_new,
      api_read_allowed: false,
      p2p: false,
      p2p_joined: false,
      settings: { name: '잡담', kind: 'text', topic: '', security: 'normal', join_mode: 'always', ttl: { mode: 'none' }, overwrites: [] },
    }) as unknown as ChannelView;

  it('받지 않은 새 글이 있으면 점을, 안 읽은 수가 있으면 수만 보이고, 연 채널에는 없다', async () => {
    const { el } = await render(<ChannelRow space={space} c={row(0, true)} active={false} />);
    expect(el.querySelector('[aria-label="새 글 있음"]')).not.toBeNull();
    expect(el.querySelector('.ch-row')?.classList.contains('unread')).toBe(true);
    await cleanup();
    const counted = await render(<ChannelRow space={space} c={row(3, true)} active={false} />);
    expect(counted.el.querySelector('[aria-label="새 글 있음"]')).toBeNull();
    expect(counted.el.querySelector('.ch-badge')?.textContent).toBe('3');
    await cleanup();
    const open = await render(<ChannelRow space={space} c={row(0, true)} active />);
    expect(open.el.querySelector('[aria-label="새 글 있음"]')).toBeNull();
  });
});

describe('채널 줄의 익명 표시', () => {
  const space = { space: 'S', kind: 'crew', members: [], my_permissions: [] } as unknown as SpaceView;
  const voice = {
    channel: 'V',
    unread: 0,
    api_read_allowed: false,
    p2p: false,
    p2p_joined: false,
    anonymous: true,
    participants: [ANONYMOUS_USER, ANONYMOUS_USER, ANONYMOUS_USER],
    settings: { name: '수다', kind: 'voice', topic: '', security: 'normal', join_mode: 'always', ttl: { mode: 'none' }, overwrites: [] },
  } as unknown as ChannelView;
  const names = (el: HTMLElement) => [...el.querySelectorAll('.vp-name')].map((x) => x.textContent);

  it('익명 채널 표시를 붙이고, 같은 자리 표시인 참가자를 하나로 합치지 않는다', async () => {
    const { el } = await render(<ChannelRow space={space} c={voice} active={false} />);
    expect(el.querySelector('[aria-label="익명 채널"]')).not.toBeNull();
    expect(names(el)).toEqual(['익명', '익명', '익명']);
  });

  it('참가자 목록이 있으면 번호로 보이고 음소거 표시도 그 줄에 붙는다', async () => {
    const p = (n: number, muted = false) => ({ user: ANONYMOUS_USER, device: `d${n}`, muted, deafened: false, screen: false, anon: n });
    useStore.setState({ voiceRosters: { 'S/V': { participants: [p(1), p(2, true), p(3)], generation: 1, topology: 'mesh' } } });
    const { el } = await render(<ChannelRow space={space} c={voice} active={false} />);
    expect(names(el)).toEqual(['익명 1', '익명 2', '익명 3']);
    expect(el.querySelectorAll('.vp-row')[1].querySelector('.muted')).not.toBeNull();
    expect(el.querySelectorAll('.vp-row')[0].querySelector('.muted')).toBeNull();
  });
});
