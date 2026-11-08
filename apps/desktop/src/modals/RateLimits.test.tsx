// @vitest-environment happy-dom
// 범위별 속도 제한 창: 비운 칸은 제한 없음(null)으로 보내고, 0은 앱에서 막아 부르지 않습니다. 단위(KiB/s·MiB/s)를 곱해 바이트로.
// 사용자 설정의 목록은 크루 이름, #채널 · 크루, 사람 이름으로 보이고 지우기는 두 방향 모두 null로 보냅니다.

import { act } from 'react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { RpcError, type ApiClient } from '../api/client';
import { ErrorCode, type RateLimitView, type RateScope, type SpaceView } from '../api/types';
import { useStore } from '../store/store';
import { button, cleanup, click, render } from '../test/dom';
import { RATE_LIMIT_NOTE, RateLimitModal, ScopedLimitsSection } from './RateLimits';

const U = 'U'.repeat(32);
const view = {
  space: 'S',
  kind: 'crew',
  settings: { name: '게임 모임' },
  members: [{ user: U, profile: { name: '민지' }, roles: [] }],
  channels: [{ channel: 'C', settings: { name: '잡담' } }],
} as unknown as SpaceView;

const KiB = 1024;
const MiB = 1024 * 1024;

function setup(initial: RateLimitView[] = [], o: { unsupported?: boolean } = {}) {
  let limits = [...initial];
  const call = vi.fn(async (method: string, params?: unknown) => {
    if (o.unsupported) throw new RpcError(ErrorCode.MethodNotFound, '없는 메서드');
    if (method === 'rate.limits') return limits;
    if (method === 'rate.set_limit') {
      const p = params as { scope: RateScope; up_bps: number | null; down_bps: number | null };
      limits = limits.filter((l) => JSON.stringify(l.scope) !== JSON.stringify(p.scope));
      if (p.up_bps !== null || p.down_bps !== null) limits.push({ scope: p.scope, up_bps: p.up_bps, down_bps: p.down_bps });
      return {};
    }
    return {};
  });
  useStore.setState({
    client: { call } as unknown as ApiClient,
    views: { S: view },
    friends: [],
    rateLimits: null,
    rateLimitsUnsupported: false,
    modal: null,
    toasts: [],
  });
  return call;
}

const sets = (call: ReturnType<typeof setup>) => call.mock.calls.filter((c) => c[0] === 'rate.set_limit').map((c) => c[1]);
const input = (el: HTMLElement, label: string) => el.querySelector<HTMLInputElement>(`input[aria-label="${label}"]`)!;
const unit = (el: HTMLElement, label: string) => el.querySelector<HTMLSelectElement>(`select[aria-label="${label} 단위"]`)!;

async function typeInto(i: HTMLInputElement, value: string) {
  await act(async () => {
    Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')!.set!.call(i, value);
    i.dispatchEvent(new Event('input', { bubbles: true }));
  });
}

async function choose(s: HTMLSelectElement, value: string) {
  await act(async () => {
    Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype, 'value')!.set!.call(s, value);
    s.dispatchEvent(new Event('change', { bubbles: true }));
  });
}

afterEach(async () => {
  await cleanup();
  useStore.setState({ client: null, views: {}, rateLimits: null, rateLimitsUnsupported: false, modal: null });
});

describe('속도 제한 창', () => {
  it('범위 이름과 설명을 보이고, 단위를 곱해 저장하며 비운 칸은 null', async () => {
    const call = setup();
    const onClose = vi.fn();
    const scope: RateScope = { kind: 'channel', space: 'S', channel: 'C' };
    const { el } = await render(<RateLimitModal scope={scope} onClose={onClose} />);
    expect(el.textContent).toContain('#잡담 · 게임 모임');
    expect(el.textContent).toContain(RATE_LIMIT_NOTE);
    expect(input(el, '올리기 제한').placeholder).toBe('제한 없음');
    await typeInto(input(el, '올리기 제한'), '1.5');
    await choose(unit(el, '올리기 제한'), 'MiB');
    await click(button(el, '저장'));
    await vi.waitFor(() => expect(onClose).toHaveBeenCalled());
    expect(sets(call)).toEqual([{ scope, up_bps: 1.5 * MiB, down_bps: null }]);
    expect(useStore.getState().toasts.at(-1)?.text).toBe('속도 제한을 저장했습니다');
  });

  it('둘 다 비우고 저장하면 둘 다 null로 보낸다', async () => {
    const call = setup();
    const scope: RateScope = { kind: 'crew', space: 'S' };
    const { el } = await render(<RateLimitModal scope={scope} onClose={() => undefined} />);
    await click(button(el, '저장'));
    await vi.waitFor(() => expect(sets(call)).toEqual([{ scope, up_bps: null, down_bps: null }]));
  });

  it('0은 막음으로 보내고, 음수는 칸 밑에 알리고 부르지 않는다', async () => {
    const call = setup();
    const onClose = vi.fn();
    const { el } = await render(<RateLimitModal scope={{ kind: 'user', user: U }} onClose={onClose} />);
    await typeInto(input(el, '받기 제한'), '-1');
    expect(el.querySelector('.err')?.textContent).toBe('0 이상의 숫자를 넣으세요');
    expect(input(el, '받기 제한').getAttribute('aria-invalid')).toBe('true');
    await click(button(el, '저장'));
    expect(sets(call)).toHaveLength(0);
    expect(onClose).not.toHaveBeenCalled();
    await typeInto(input(el, '받기 제한'), '0');
    expect(el.querySelector('.err')).toBeNull();
    await click(button(el, '저장'));
    await vi.waitFor(() => expect(sets(call)).toEqual([{ scope: { kind: 'user', user: U }, up_bps: null, down_bps: 0 }]));
  });

  it('있던 값을 채우고, 제한 지우기는 두 방향 모두 null로 보낸다', async () => {
    const scope: RateScope = { kind: 'user', user: U };
    const call = setup([{ scope, up_bps: 2 * MiB, down_bps: 512 * KiB }]);
    const { el } = await render(<RateLimitModal scope={scope} onClose={() => undefined} />);
    await vi.waitFor(() => expect(input(el, '올리기 제한').value).toBe('2'));
    expect(el.textContent).toContain('민지');
    expect(unit(el, '올리기 제한').value).toBe('MiB');
    expect(input(el, '받기 제한').value).toBe('512');
    expect(unit(el, '받기 제한').value).toBe('KiB');
    await click(button(el, '제한 지우기'));
    await vi.waitFor(() => expect(sets(call)).toEqual([{ scope, up_bps: null, down_bps: null }]));
    expect(useStore.getState().toasts.at(-1)?.text).toBe('속도 제한을 지웠습니다');
  });

  it('제한이 없으면 제한 지우기가 없다', async () => {
    setup();
    const { el } = await render(<RateLimitModal scope={{ kind: 'crew', space: 'S' }} onClose={() => undefined} />);
    await vi.waitFor(() => expect(useStore.getState().rateLimits).toEqual([]));
    expect([...el.querySelectorAll('button')].some((b) => b.textContent?.includes('제한 지우기'))).toBe(false);
  });

  it('옛 데몬이면 지원하지 않는다고 알리고 저장을 막는다', async () => {
    setup([], { unsupported: true });
    const { el } = await render(<RateLimitModal scope={{ kind: 'crew', space: 'S' }} onClose={() => undefined} />);
    await vi.waitFor(() => expect(el.textContent).toContain('지원하지 않습니다'));
    expect(button(el, '저장').disabled).toBe(true);
  });
});

describe('사용자 설정의 범위별 속도 제한 목록', () => {
  it('범위마다 이름·값을 보이고, 고치기는 창을 열며 지우기는 null로 보낸다', async () => {
    const crew: RateScope = { kind: 'crew', space: 'S' };
    const call = setup([
      { scope: crew, up_bps: 100 * KiB, down_bps: null },
      { scope: { kind: 'channel', space: 'S', channel: 'C' }, up_bps: null, down_bps: 3 * MiB },
      { scope: { kind: 'user', user: U }, up_bps: 1536, down_bps: 1536 },
    ]);
    const back = { kind: 'userSettings', tab: 'sync' } as const;
    const { el } = await render(<ScopedLimitsSection back={back} />);
    await vi.waitFor(() => expect(el.querySelectorAll('.rl-table .tr')).toHaveLength(3));
    const text = el.textContent!;
    expect(text).toContain('게임 모임');
    expect(text).toContain('↑ 100 KiB/s · ↓ 제한 없음');
    expect(text).toContain('#잡담 · 게임 모임');
    expect(text).toContain('↑ 제한 없음');
    expect(text).toContain('↓ 3.0 MiB/s');
    expect(text).toContain('민지');
    await click(el.querySelector<HTMLButtonElement>('button[aria-label="민지 속도 제한 고치기"]')!);
    expect(useStore.getState().modal).toEqual({ kind: 'rateLimit', scope: { kind: 'user', user: U }, back });
    await click(el.querySelector<HTMLButtonElement>('button[aria-label="게임 모임 속도 제한 지우기"]')!);
    await vi.waitFor(() => expect(sets(call)).toEqual([{ scope: crew, up_bps: null, down_bps: null }]));
    await vi.waitFor(() => expect(el.querySelectorAll('.rl-table .tr')).toHaveLength(2));
  });

  it('없으면 비었다고 알린다', async () => {
    setup();
    const { el } = await render(<ScopedLimitsSection />);
    await vi.waitFor(() => expect(el.textContent).toContain('범위별 속도 제한이 없습니다.'));
  });
});
