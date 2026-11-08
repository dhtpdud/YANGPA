// @vitest-environment happy-dom
// 이 기기가 내주는 첨부를 지금 받는 사람: 프사·이름·속도를 빠른 순으로 5줄까지, 나머지는 '외 N명'. 익명은 '익명'과 기본 그림이고
// 프로필을 열지 않습니다.

import { act } from 'react';
import { afterEach, describe, expect, it } from 'vitest';
import { ANONYMOUS_USER, type SpaceView, type UploadRate } from '../api/types';
import { useStore } from '../store/store';
import { cleanup, click, render } from '../test/dom';
import { UploadPeers } from './UploadPeers';

const BLOB = 'B'.repeat(64);
// '1'×32는 ANONYMOUS_USER라 글자로 만듭니다.
const user = (i: number) => 'abcdefgh'[i].repeat(32);

const view = {
  space: 'S',
  kind: 'crew',
  settings: { name: '크루' },
  members: [1, 2, 3, 4, 5, 6, 7].map((i) => ({ user: user(i), profile: { name: `사람${i}` }, roles: [] })),
  channels: [],
} as unknown as SpaceView;

const up = (u: string, bps: number, extra: Partial<UploadRate> = {}): UploadRate => ({ space: 'S', blob: BLOB, user: u, bps, ...extra });

function setup(uploads: UploadRate[]) {
  useStore.setState({ views: { S: view }, friends: [], modal: null, rates: { up_bps: 1, down_bps: 0, uploads } });
}

afterEach(async () => {
  await cleanup();
  useStore.setState({ rates: null, views: {}, modal: null });
});

const names = (el: HTMLElement) => [...el.querySelectorAll('.att-peer-name')].map((x) => x.textContent);

describe('받고 있는 사람', () => {
  it('이 공간·이 블롭만, 빠른 순으로 이름·속도를 보이고 누르면 프로필을 연다', async () => {
    setup([up(user(1), 1024), up(user(2), 4096), up(user(3), 9999, { blob: 'X'.repeat(64) }), up(user(4), 9999, { space: 'T' })]);
    const { el } = await render(<UploadPeers space="S" blob={BLOB} />);
    expect(names(el)).toEqual(['사람2', '사람1']);
    expect([...el.querySelectorAll('.att-peer-rate')].map((x) => x.textContent)).toEqual(['4.0 KiB/s', '1.0 KiB/s']);
    expect(el.querySelector('.att-peers-head')?.textContent).toContain('나눠 주는 중 · 2명 · 5.0 KiB/s');
    expect(el.querySelectorAll('button.att-peer .avatar')).toHaveLength(2);
    await click(el.querySelector<HTMLButtonElement>('button.att-peer')!);
    expect(useStore.getState().modal).toEqual({ kind: 'friendProfile', user: user(2) });
  });

  it('익명은 익명으로 그리고 단추가 아니다', async () => {
    setup([up(ANONYMOUS_USER, 10), up(ANONYMOUS_USER, 20)]);
    const { el } = await render(<UploadPeers space="S" blob={BLOB} />);
    expect(names(el)).toEqual(['익명', '익명']);
    expect(el.querySelectorAll('button.att-peer')).toHaveLength(0);
    expect(el.querySelectorAll('.att-peer.anon')).toHaveLength(2);
    // 기본 그림(누구의 사진도 찾지 않음).
    expect(el.querySelectorAll('.att-peer .avatar.anon')).toHaveLength(2);
  });

  it('5줄까지 보이고 나머지는 외 N명', async () => {
    setup([1, 2, 3, 4, 5, 6, 7].map((i) => up(user(i), i * 100)));
    const { el } = await render(<UploadPeers space="S" blob={BLOB} />);
    expect(el.querySelectorAll('.att-peer')).toHaveLength(5);
    expect(names(el)[0]).toBe('사람7');
    expect(el.querySelector('.att-peers-more')?.textContent).toBe('외 2명');
    expect(el.querySelector('.att-peers-head')?.textContent).toContain('7명');
  });

  it('받는 사람이 없으면 그리지 않고, 알림이 오면 나타난다', async () => {
    setup([]);
    const { el } = await render(<UploadPeers space="S" blob={BLOB} />);
    expect(el.querySelector('.att-peers')).toBeNull();
    await act(async () => useStore.setState({ rates: { up_bps: 5, down_bps: 0, uploads: [up(user(1), 5)] } }));
    expect(names(el)).toEqual(['사람1']);
  });
});
