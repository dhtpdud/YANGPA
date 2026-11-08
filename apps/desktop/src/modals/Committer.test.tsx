// @vitest-environment happy-dom
// 크루 설정 → 확정 기기: 소유자만 고르고, 바꾸기 전에 확인을 받으며, 다른 멤버에게는 목록만 보입니다.

import { act } from 'react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { RpcError, type ApiClient } from '../api/client';
import { ErrorCode, type CommitterCandidate, type MemberView, type SpaceView } from '../api/types';
import { useStore } from '../store/store';
import { button, cleanup, click, render } from '../test/dom';
import { CommitterSection, deviceLabel } from './Committer';

const OWNER = 'O'.repeat(44);
const ADMIN = 'A'.repeat(44);
const SPACE = 'S'.repeat(22);
const MINE: CommitterCandidate = { user: OWNER, device: 'D'.repeat(44), name: '내 노트북', online: true, current: true, mine: true };
const SEEDER: CommitterCandidate = { user: ADMIN, device: 'E'.repeat(44), name: '', online: false, current: false, mine: false };

const member = (user: string, name: string): MemberView => ({
  user,
  profile: { version: 1, name, bio: '', avatar: null, banner: null, theme_color: null, status: '' },
  roles: [],
  online: true,
  automation: false,
});

function setup(me: string, opts: { unsupported?: boolean; failOnce?: boolean } = {}) {
  let list = [MINE, SEEDER];
  let fail = !!opts.failOnce;
  const call = vi.fn(async (method: string, params?: { device?: string }) => {
    if (opts.unsupported) throw new RpcError(ErrorCode.MethodNotFound, '없는 메서드');
    if (fail) {
      fail = false;
      throw new RpcError(ErrorCode.Internal, '데몬 연결이 끊겼습니다');
    }
    if (method === 'crew.committer_candidates') return list;
    if (method === 'crew.set_committer') {
      list = list.map((c) => ({ ...c, current: c.device === params?.device }));
      return {};
    }
    return {};
  });
  const view = { space: SPACE, owner: OWNER, committer: MINE.device, members: [member(OWNER, '세영'), member(ADMIN, '시더')] } as unknown as SpaceView;
  useStore.setState({ client: { call } as unknown as ApiClient, me, views: { [SPACE]: view }, modal: null, toasts: [] });
  return call;
}

afterEach(async () => {
  await cleanup();
  useStore.setState({ client: null, modal: null, views: {}, me: null });
});

describe('CommitterSection', () => {
  it('이름 없는 기기는 ID 앞부분으로 보인다', () => {
    expect(deviceLabel(MINE)).toBe('내 노트북');
    expect(deviceLabel(SEEDER)).toBe(`기기 ${'E'.repeat(8)}`);
  });

  it('소유자는 다른 기기를 골라 확인한 뒤 확정 기기로 정한다', async () => {
    const call = setup(OWNER);
    const { el } = await render(<CommitterSection space={SPACE} />);
    await vi.waitFor(() => expect(el.textContent).toContain('내 노트북'));
    expect(el.textContent).toContain('지금 확정 기기');
    expect(el.textContent).toContain('이 기기');
    expect(button(el, '확정 기기로 정하기').disabled).toBe(true);

    const radios = el.querySelectorAll<HTMLInputElement>('input[type="radio"]');
    expect(radios).toHaveLength(2);
    await click(radios[1]);
    expect(button(el, '확정 기기로 정하기').disabled).toBe(false);
    await click(button(el, '확정 기기로 정하기'));
    expect(call).not.toHaveBeenCalledWith('crew.set_committer', expect.anything());
    const m = useStore.getState().modal;
    expect(m).toMatchObject({ kind: 'confirm', title: '확정 기기 바꾸기' });
    if (m?.kind !== 'confirm') return;
    expect(m.body).toContain('시더');
    expect(m.body).toContain('연결돼 있지 않습니다');
    await act(async () => {
      await m.onConfirm();
    });
    expect(call).toHaveBeenCalledWith('crew.set_committer', { space: SPACE, device: SEEDER.device });
    await vi.waitFor(() => expect(el.querySelectorAll('.tag.ok')).toHaveLength(1));
    expect(el.querySelectorAll('.committer-row')[1].textContent).toContain('지금 확정 기기');
  });

  it('소유자가 아니면 목록만 보이고 바꿀 수 없다', async () => {
    const call = setup(ADMIN);
    const { el } = await render(<CommitterSection space={SPACE} />);
    await vi.waitFor(() => expect(el.textContent).toContain('내 노트북'));
    expect([...el.querySelectorAll<HTMLInputElement>('input[type="radio"]')].every((r) => r.disabled)).toBe(true);
    expect(el.querySelectorAll('button')).toHaveLength(0);
    expect(call).toHaveBeenCalledTimes(1);
  });

  it('잠깐의 오류는 숨기지 않고 알린 뒤 다시 읽을 수 있다', async () => {
    const call = setup(OWNER, { failOnce: true });
    const { el } = await render(<CommitterSection space={SPACE} />);
    await vi.waitFor(() => expect(el.textContent).toContain('데몬 연결이 끊겼습니다'));
    await click(button(el, '다시 읽기'));
    await vi.waitFor(() => expect(el.textContent).toContain('내 노트북'));
    expect(el.textContent).not.toContain('데몬 연결이 끊겼습니다');
    expect(call).toHaveBeenCalledTimes(2);
  });

  it('후보 목록을 모르는 데몬이면 그리지 않는다', async () => {
    setup(OWNER, { unsupported: true });
    const { el } = await render(<CommitterSection space={SPACE} />);
    await vi.waitFor(() => expect(el.textContent).toBe(''));
  });
});
