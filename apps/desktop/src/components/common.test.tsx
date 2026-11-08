// @vitest-environment happy-dom
// 아바타: 프로필 이미지는 그 사람이 멤버인 공간을 통해 블롭 프로토콜로 읽고, 못 읽으면 머리글자로 돌아갑니다.

import { act } from 'react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import type { ApiClient } from '../api/client';
import type { FlairView, FriendView } from '../api/types';
import { ANONYMOUS_USER } from '../api/types';
import { useStore } from '../store/store';
import { cleanup, render } from '../test/dom';
import { AUTO_IMAGE_MIME, Avatar, BannerBack, NO_KEY_EVENT, resetImageFetchForTest } from './common';

const profile = { version: 1, name: '가나 다라', bio: '', avatar: 'BLOB1', banner: null, theme_color: null, status: '' };

const call = vi.fn(async () => ({}));

function fakeClient(): ApiClient {
  return {
    call,
    transport: { blobSrc: (space: string, blob: string, key: string, mime: string) => `yangpa-blob://localhost/blob/${space}/${blob}/${key}?mime=${mime}` },
  } as unknown as ApiClient;
}

afterEach(async () => {
  await cleanup();
  call.mockClear();
  resetImageFetchForTest();
  useStore.setState({ client: null, friends: [], views: {}, identity: null, spaces: [], blobProgress: {}, flair: {} });
});

describe('Avatar', () => {
  it('이미지가 없으면 머리글자를 그린다', async () => {
    useStore.setState({ client: fakeClient(), friends: [] });
    const { el } = await render(<Avatar id="U1" name="가나 다라" />);
    expect(el.querySelector('img')).toBeNull();
    expect(el.textContent).toBe('가다');
  });

  it('친구의 프로필 이미지는 1:1 공간을 통해 읽고, 오류가 나면 머리글자로 돌아간다', async () => {
    const friend: FriendView = { user: 'U1', profile, space: 'SP1', online: true, safety_number_verified: false, kind: 'friend', proposed: false };
    useStore.setState({ client: fakeClient(), friends: [friend] });
    const { el } = await render(<Avatar id="U1" name="가나 다라" />);
    const img = el.querySelector('img')!;
    expect(img.getAttribute('src')).toBe(`yangpa-blob://localhost/blob/SP1/BLOB1/${NO_KEY_EVENT}?mime=${AUTO_IMAGE_MIME}`);
    // 메인 스레드에서 디코딩하지 않게
    expect(img.getAttribute('decoding')).toBe('async');
    await act(async () => {
      img.dispatchEvent(new Event('error'));
    });
    expect(el.querySelector('img')).toBeNull();
    expect(el.textContent).toBe('가다');
  });

  it('못 읽은 이미지는 낮은 우선순위로 한 번만 받아 달라고 한다', async () => {
    const member = { user: 'U2', profile: { ...profile, avatar: 'BLOB2' } };
    useStore.setState({ client: fakeClient(), views: { CR: { space: 'CR', members: [member] } as never } });
    const first = await render(<Avatar id="U2" name="크루 멤버" />);
    const second = await render(<Avatar id="U2" name="크루 멤버" />);
    for (const r of [first, second]) {
      await act(async () => {
        r.el.querySelector('img')!.dispatchEvent(new Event('error'));
      });
    }
    expect(call).toHaveBeenCalledTimes(1);
    expect(call).toHaveBeenCalledWith('blob.fetch', { space: 'CR', blob: 'BLOB2', priority: 64 });
  });

  it('받는 중인 이미지는 다 받은 뒤에 다시 시도한다', async () => {
    const friend: FriendView = { user: 'U1', profile, space: 'SP1', online: true, safety_number_verified: false, kind: 'friend', proposed: false };
    useStore.setState({ client: fakeClient(), friends: [friend], blobProgress: { BLOB1: { have: 1, total: 3 } } });
    const { el } = await render(<Avatar id="U1" name="가나 다라" />);
    expect(el.querySelector('img')).toBeNull();
    await act(async () => useStore.setState({ blobProgress: { BLOB1: { have: 3, total: 3 } } }));
    expect(el.querySelector('img')).not.toBeNull();
  });
});

describe('후원자 표시', () => {
  const later = () => Date.now() + 60_000;
  const flair = (user: string, extra: Partial<FlairView>): FlairView => ({ user, banner: false, halo_allowed: false, until: later(), ...extra });
  const friend: FriendView = { user: 'U1', profile: { ...profile, banner: 'BAN1' }, space: 'SP1', online: true, safety_number_verified: false, kind: 'friend', proposed: false };

  it('카드 배너는 월 14달러 이상 후원자만, 기한이 지나면 내린다', async () => {
    useStore.setState({ client: fakeClient(), friends: [friend] });
    const { el } = await render(<BannerBack user="U1" banner="BAN1" space="SP1" />);
    expect(el.querySelector('img')).toBeNull();
    await act(async () => useStore.setState({ flair: { U1: flair('U1', { banner: true }) } }));
    expect(el.querySelector('img.banner-back')!.getAttribute('src')).toContain('/SP1/BAN1/');
    await act(async () => useStore.setState({ flair: { U1: flair('U1', { banner: true, until: Date.now() - 1 }) } }));
    expect(el.querySelector('img')).toBeNull();
  });

  it('후광은 고른 두 색으로 그리고, 익명 자리에는 그리지 않는다', async () => {
    useStore.setState({ client: fakeClient(), flair: { U1: flair('U1', { halo_allowed: true, halo: [0x112233, 0x000000] }), [ANONYMOUS_USER]: flair(ANONYMOUS_USER, { halo_allowed: true, halo: [1, 2] }) } });
    const { el } = await render(<Avatar id="U1" name="가나" />);
    const av = el.querySelector<HTMLElement>('.avatar')!;
    expect(av.classList.contains('halo')).toBe(true);
    expect(av.style.getPropertyValue('--halo-a')).toBe('#112233');
    // 검정도 색으로 씁니다.
    expect(av.style.getPropertyValue('--halo-b')).toBe('#000000');
    const anon = await render(<Avatar id={ANONYMOUS_USER} name="익명" />);
    expect(anon.el.querySelector('.avatar')!.classList.contains('halo')).toBe(false);
  });
});
