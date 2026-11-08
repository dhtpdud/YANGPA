// @vitest-environment happy-dom
// 크루 설정 → 모더레이션: 한계 안내, 밴 목록, 신고 목록과 처리(MOCK 백엔드).

import { act } from 'react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { ApiClient } from '../api/client';
import { createMockTransport } from '../api/mock';
import type { SpaceView } from '../api/types';
import { emptyChannel } from '../store/apply';
import { chKey, useStore } from '../store/store';
import { button, cleanup, click, render } from '../test/dom';
import { ModerationPanel, actionGate } from './Moderation';

const open: Array<{ dispose(): void }> = [];

afterEach(async () => {
  await cleanup();
  while (open.length) open.pop()?.dispose();
});

/** 사유 글이 든 신고 줄. */
function row(el: HTMLElement, reason: string): HTMLElement | undefined {
  return [...el.querySelectorAll<HTMLElement>('.report-row')].find((r) => r.textContent?.includes(reason));
}

async function setup(name: string): Promise<{ client: ApiClient; view: SpaceView }> {
  const t = createMockTransport({ quiet: true, seed: 7 });
  open.push(t.backend);
  const client = new ApiClient(t);
  const s = (await client.call('space.list', {})).find((x) => x.name === name)!;
  const view = await client.call('space.get', { space: s.space });
  useStore.setState({ client, views: { [s.space]: view }, me: (await client.call('identity.get', {})).user, channels: {}, reportsNew: { [s.space]: 2 }, toasts: [] });
  return { client, view };
}

describe('ModerationPanel', () => {
  it('한계를 알리고, 밴 목록과 신고 목록을 보여 주며, 열면 새 신고 표시를 지운다', async () => {
    const { client, view } = await setup('게임 모임');
    const { el } = await render(<ModerationPanel space={view.space} />);
    expect(el.textContent).toContain('모더레이션의 한계');
    expect(el.textContent).toContain('새 신원 막기');
    await vi.waitFor(() => expect(el.textContent).toContain('광고 도배'));
    expect(useStore.getState().reportsNew[view.space]).toBe(0);
    // 예시 데이터의 처리 전 신고 둘
    await vi.waitFor(() => expect(el.querySelectorAll('.report-row')).toHaveLength(2));
    expect(row(el, '욕설이 있어요')).toBeDefined();

    // 새 신고가 오면(report.new → reportsNew) 목록을 다시 읽습니다.
    const ch = view.channels.find((c) => c.settings.name === '잡담')!;
    const page = await client.call('message.list', { space: view.space, channel: ch.channel, limit: 50 });
    const target = page.messages.find((m) => m.author !== view.owner && !m.deleted)!;
    await client.call('message.report', { space: view.space, channel: ch.channel, message: target.id, reason: '시험 신고 사유' });
    await act(async () => useStore.setState({ reportsNew: { [view.space]: 1 } }));
    await vi.waitFor(() => expect(row(el, '시험 신고 사유')).toBeDefined());

    // 기각은 바로 처리되고 목록에서 빠집니다.
    await click(button(row(el, '시험 신고 사유')!, '기각'));
    await vi.waitFor(() => expect(row(el, '시험 신고 사유')).toBeUndefined());
    expect(el.querySelectorAll('.report-row')).toHaveLength(2);
    const resolved = await client.call('report.list', { space: view.space, status: 'resolved' });
    expect(resolved.find((r) => r.message === target.id)?.resolution).toBe('dismiss');
  });

  it('추방·밴은 한 번 더 확인한 뒤 처리한다', async () => {
    const { client, view } = await setup('게임 모임');
    const ch = view.channels.find((c) => c.settings.name === '잡담')!;
    const page = await client.call('message.list', { space: view.space, channel: ch.channel, limit: 50 });
    const target = page.messages.find((m) => m.author !== view.owner && !m.deleted)!;
    await client.call('message.report', { space: view.space, channel: ch.channel, message: target.id, reason: '삭제할 것' });
    const { el } = await render(<ModerationPanel space={view.space} />);
    await vi.waitFor(() => expect(row(el, '삭제할 것')).toBeDefined());
    await click(button(row(el, '삭제할 것')!, '메시지 삭제'));
    // 아직 처리하지 않고 확인을 묻습니다.
    expect(row(el, '삭제할 것')!.textContent).toContain('메시지 삭제할까요?');
    expect((await client.call('message.list', { space: view.space, channel: ch.channel, limit: 50 })).messages.find((m) => m.id === target.id)?.deleted).toBe(false);
    await click(button(row(el, '삭제할 것')!, '확인'));
    await vi.waitFor(() => expect(row(el, '삭제할 것')).toBeUndefined());
    const after = await client.call('message.list', { space: view.space, channel: ch.channel, limit: 50 });
    expect(after.messages.find((m) => m.id === target.id)?.deleted).toBe(true);
  });

  it('운영 권한이 없으면 신고·밴 목록 대신 안내만 보인다', async () => {
    const { view } = await setup('러스트 스터디');
    const { el } = await render(<ModerationPanel space={view.space} />);
    expect(el.textContent).toContain('모더레이션의 한계');
    expect(el.textContent).toContain('신고 목록은 운영진');
    expect(el.textContent).not.toContain('밴 목록');
  });

  it('신고된 메시지의 작성자가 나보다 순위가 낮지 않으면 추방·밴 버튼을 막는다', async () => {
    const { client, view } = await setup('게임 모임');
    const ch = view.channels.find((c) => c.settings.name === '잡담')!;
    const page = await client.call('message.list', { space: view.space, channel: ch.channel, limit: 50 });
    const target = page.messages.find((m) => m.author !== view.owner && !m.deleted)!;
    await client.call('message.report', { space: view.space, channel: ch.channel, message: target.id, reason: '순위 확인' });
    // 이 기기 화면에 사본이 있어 작성자를 압니다.
    useStore.setState({ channels: { [chKey(view.space, ch.channel)]: { ...emptyChannel(), messages: [target] } } });
    const { el } = await render(<ModerationPanel space={view.space} />);
    await vi.waitFor(() => expect(row(el, '순위 확인')).toBeDefined());
    // 소유자인 나는 작성자보다 순위가 높습니다.
    expect(button(row(el, '순위 확인')!, '추방').disabled).toBe(false);
    expect(button(row(el, '순위 확인')!, '밴').disabled).toBe(false);

    // 작성자가 소유자(나보다 높음)인 크루로 바꾸면 권한이 있어도 막고 이유를 알립니다.
    const me = useStore.getState().me!;
    const lower: SpaceView = { ...view, owner: target.author, my_permissions: ['kick', 'ban', 'manage_messages'], members: view.members };
    await act(async () => useStore.setState({ views: { [view.space]: lower }, me }));
    const kick = button(row(el, '순위 확인')!, '추방');
    expect(kick.disabled).toBe(true);
    expect(kick.title).toContain('순위');
    expect(button(row(el, '순위 확인')!, '밴').disabled).toBe(true);
    expect(button(row(el, '순위 확인')!, '메시지 삭제').disabled).toBe(false);
  });

  it('작성자를 모르면(사본 없음) 추방·밴을 막지 않고 데몬이 확인한다고 알린다', () => {
    const view = { owner: 'O', members: [], roles: [], my_permissions: ['kick', 'ban'] } as unknown as SpaceView;
    expect(actionGate(view, 'M', 'kick', undefined)).toMatchObject({ allowed: true, title: expect.stringContaining('데몬') });
    expect(actionGate(view, 'M', 'ban', 'O').allowed).toBe(false);
    expect(actionGate(view, 'M', 'ban', 'M').allowed).toBe(false);
    expect(actionGate({ ...view, my_permissions: [] }, 'M', 'kick', undefined)).toEqual({ allowed: false, title: '권한이 없습니다' });
    expect(actionGate(view, 'M', 'dismiss', 'O').allowed).toBe(true);
  });
});
