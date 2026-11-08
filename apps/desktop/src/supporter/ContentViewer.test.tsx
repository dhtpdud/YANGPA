// SPDX-License-Identifier: LicenseRef-YANGPA-Supporter
// @vitest-environment happy-dom
// 콘텐츠 뷰어: 키보드로 넘기고, 받지 않은 첨부는 누를 때만 받으며(보이기만 해서는 blob.fetch·blob.status 없음), 채널을 떠나면 닫힘.
// 라이브러리가 넘긴 정해진 목록은 고른 항목부터 보이고, 채널 기록을 더 읽지도 채널 선택에 따라 닫히지도 않음.

import { act } from 'react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import type { ApiClient } from '../api/client';
import type { ChannelView, MessageView, SpaceView } from '../api/types';
import { DEFAULT_VIEWER } from './viewer';
import { useStore } from '../store/store';
import { button, cleanup, click, render } from '../test/dom';
import { ContentViewer } from './ContentViewer';

function msg(id: string, text: string, extra: Partial<MessageView> = {}): MessageView {
  return { id, author: 'u1', author_device: 'd1', created_at: 0, lamport: 0, text, attachments: [], embeds: [], reactions: [], deleted: false, volatile: false, ...extra };
}

const channel = { channel: 'C', settings: { name: '일반', kind: 'text', security: 'normal', join_mode: 'always' }, p2p: false, p2p_joined: false } as unknown as ChannelView;
const image = { blob: 'B'.repeat(64), name: '사진.png', mime: 'image/png', size: 2048, width: 800, height: 600, downloaded: false };

/** `older`는 맨 앞(가장 오래된 쪽)에 붙습니다. */
function setup(older: MessageView[] = []) {
  const call = vi.fn(async () => ({}));
  const transport = {
    blobSrc: (_s: string, blob: string) => `yangpa-blob://localhost/blob/${blob}`,
    readBlob: async () => new Uint8Array(),
    savePrefs: async () => undefined,
  };
  useStore.setState({
    client: { call, transport } as unknown as ApiClient,
    views: { S: { space: 'S', members: [], channels: [channel] } as unknown as SpaceView },
    selected: { space: 'S', channel: 'C' },
    channels: {
      'S/C': {
        messages: [...older, msg('m1', '첫 글'), msg('m2', '사진 설명', { attachments: [image] }), msg('m3', '마지막 글')],
        gaps: [],
        reachedStart: true,
        loadingOlder: false,
        loaded: true,
        filling: [],
      },
    },
    blobProgress: {},
  });
  return call;
}

const key = (k: string) => act(async () => void window.dispatchEvent(new KeyboardEvent('keydown', { key: k, bubbles: true })));
const counter = (el: HTMLElement) => el.querySelector('.vw-counter')?.textContent;

afterEach(async () => {
  await cleanup();
  useStore.setState((s) => ({
    client: null,
    views: {},
    channels: {},
    selected: { space: 'home', channel: null },
    blobProgress: {},
    prefs: { ...s.prefs, viewer: DEFAULT_VIEWER },
  }));
});

describe('ContentViewer', () => {
  it('최신 것부터 보이고 ↓/↑로 넘기며, 받지 않은 이미지는 누를 때만 받는다', async () => {
    const call = setup();
    const { el } = await render(<ContentViewer space="S" channel="C" onClose={() => undefined} />);
    expect(counter(el)).toBe('1 / 3');
    expect(el.querySelector('.vw-text')?.textContent).toContain('마지막 글');

    await key('ArrowDown');
    expect(counter(el)).toBe('2 / 3');
    expect(el.querySelector('.vw-caption')?.textContent).toBe('사진 설명');
    expect(call).not.toHaveBeenCalled();

    await click(button(el, '원본 받기'));
    expect(call).toHaveBeenCalledWith('blob.fetch', { space: 'S', blob: image.blob, priority: 200 });

    await key('ArrowDown');
    await key('ArrowDown');
    expect(counter(el)).toBe('3 / 3');
    await key('ArrowUp');
    expect(counter(el)).toBe('2 / 3');
  });

  it('파일은 채팅의 첨부 카드 그대로, 받은 오디오는 파형 재생기 하나로 보인다', async () => {
    const file = { blob: 'F'.repeat(64), name: 'build.zip', mime: 'application/zip', size: 4096, downloaded: false };
    const song = { blob: 'A'.repeat(64), name: '노래.mp3', mime: 'audio/mpeg', size: 4096, duration_ms: 90_000, downloaded: true };
    const call = setup([msg('mf', '', { attachments: [file] }), msg('ma', '들어 보세요', { attachments: [song] })]);
    const { el } = await render(<ContentViewer space="S" channel="C" onClose={() => undefined} />);
    for (let i = 0; i < 3; i++) await key('ArrowDown');
    expect(counter(el)).toBe('4 / 5');
    expect(el.querySelector('.vw-caption')?.textContent).toBe('들어 보세요');
    expect(el.querySelector('.vw-wave')).not.toBeNull();
    expect(el.querySelector('.vw-audio-time')?.textContent).toBe('0:00 / 1:30');
    // 첨부 카드는 재생기를 그리지 않음(파형 재생기의 audio 하나뿐).
    expect(el.querySelectorAll('.vw-stage audio')).toHaveLength(1);
    expect(el.querySelector('.att-file-name')?.textContent).toBe('노래.mp3');

    await key('ArrowDown');
    expect(el.querySelector('.att-file-name')?.textContent).toBe('build.zip');
    expect(el.querySelector('button[title="받기"]')).not.toBeNull();
    // 이 기기의 보관 목록(blob.pins)만 읽고, 멤버 기기에 묻는 받기·상태 조회는 없음.
    expect(call.mock.calls.map((c) => (c as unknown[])[0])).not.toContain('blob.fetch');
    expect(call.mock.calls.map((c) => (c as unknown[])[0])).not.toContain('blob.status');
  });

  it('사용자 필터: 보기 설정에서 고른 사람의 글만 보이고, 설정 파일에는 넣지 않는다', async () => {
    setup([msg('d1', '도윤의 글', { author: 'u2' })]);
    const members = [
      { user: 'u1', profile: { name: '민지' }, roles: [] },
      { user: 'u2', profile: { name: '도윤' }, roles: [] },
    ];
    useStore.setState({ views: { S: { space: 'S', members, channels: [channel] } as unknown as SpaceView } });
    const { el } = await render(<ContentViewer space="S" channel="C" onClose={() => undefined} />);
    expect(counter(el)).toBe('1 / 4');
    await click(el.querySelector<HTMLButtonElement>('button[title="보기 설정"]')!);
    // 이름이 비면 불러온 글을 쓴 사람이 후보.
    const people = [...el.querySelectorAll('.vw-people label')];
    expect(people.map((l) => l.lastChild?.textContent)).toEqual(['도윤', '민지']);
    // 이름 앞에 프사(사진이 없으면 첫 글자).
    expect(people.map((l) => l.querySelector('.avatar')?.textContent)).toEqual(['도', '민']);
    await click(people[0].querySelector('input')!);
    expect(el.querySelector('.vw-panel .chip .avatar')).not.toBeNull();
    expect(el.querySelector('.vw-panel .chip')?.textContent).toContain('도윤');
    await act(async () => el.querySelector('form')!.requestSubmit());
    expect(counter(el)).toBe('1 / 1');
    expect(el.querySelector('.vw-text')?.textContent).toContain('도윤의 글');
    expect(JSON.stringify(useStore.getState().prefs.viewer)).not.toContain('u2');
  });

  it('필터로 빠진 종류는 건너뛴다', async () => {
    setup();
    useStore.setState((s) => ({ prefs: { ...s.prefs, viewer: { ...s.prefs.viewer, text: false } } }));
    const { el } = await render(<ContentViewer space="S" channel="C" onClose={() => undefined} />);
    expect(counter(el)).toBe('1 / 1');
  });

  it('미리보기 고정 단추로 아래 막대를 고정하고 설정에 기억한다', async () => {
    setup();
    const { el } = await render(<ContentViewer space="S" channel="C" onClose={() => undefined} />);
    expect(el.querySelector('.viewer.strip-pinned')).toBeNull();
    await click(el.querySelector<HTMLButtonElement>('button[title="미리보기 고정"]')!);
    expect(el.querySelector('.viewer.strip-pinned')).not.toBeNull();
    expect(useStore.getState().prefs.viewer.pinStrip).toBe(true);
    await click(el.querySelector<HTMLButtonElement>('button[title="미리보기 고정 풀기"]')!);
    expect(el.querySelector('.viewer.strip-pinned')).toBeNull();
  });

  it('Esc로 닫고, 다른 채널로 옮기면 저절로 닫힌다', async () => {
    setup();
    const onClose = vi.fn();
    await render(<ContentViewer space="S" channel="C" onClose={onClose} />);
    await key('Escape');
    expect(onClose).toHaveBeenCalledTimes(1);
    await act(async () => useStore.setState({ selected: { space: 'S', channel: 'D' } }));
    expect(onClose).toHaveBeenCalledTimes(2);
  });

  it('정해진 목록(라이브러리): 고른 항목부터, 기록을 더 읽지 않고 채널을 떠나도 열려 있음', async () => {
    const call = setup();
    const loadOlder = vi.fn(async () => undefined);
    useStore.setState({ selected: { space: 'home', channel: null }, loadOlder });
    const a = msg('x1', '', { attachments: [{ ...image, blob: 'X'.repeat(64), name: '첫째.png' }], space: 'S', channel: 'C' });
    const b = msg('x2', '둘째 설명', { attachments: [{ ...image, blob: 'Y'.repeat(64), name: '둘째.png' }], space: 'S', channel: 'C' });
    const items = [a, b].map((m) => ({ key: `${m.id}:${m.attachments[0].blob}`, type: 'image' as const, msg: m, att: m.attachments[0] }));
    const onClose = vi.fn();
    const { el } = await render(<ContentViewer list={{ items, start: items[1].key }} onClose={onClose} />);
    expect(counter(el)).toBe('2 / 2');
    expect(el.querySelector('.vw-caption')?.textContent).toBe('둘째 설명');
    await key('ArrowUp');
    expect(counter(el)).toBe('1 / 2');
    expect(onClose).not.toHaveBeenCalled();
    expect(loadOlder).not.toHaveBeenCalled();
    await click(button(el, '원본 받기'));
    expect(call).toHaveBeenCalledWith('blob.fetch', { space: 'S', blob: 'X'.repeat(64), priority: 200 });
  });
});
