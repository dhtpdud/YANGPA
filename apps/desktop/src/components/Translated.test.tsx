// @vitest-environment happy-dom
// AI 번역 표시: 번역을 켠 크루의 글만 묶어 보내고, 번역되면 아이콘과 원본 언어를 붙이며 누르면 원문을 봅니다.

import { act } from 'react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import type { ApiClient } from '../api/client';
import type { AiTranslateParams, ChannelView, SpaceView } from '../api/types';
import { DEFAULT_AI_TR } from '../lib/aiTr';
import { useStore } from '../store/store';
import { cleanup, click, render } from '../test/dom';
import { clearTranslateCache } from '../hooks/useTranslated';
import { MessageBody, TrText, useTrText } from './Translated';

const members = new Map();

function setup(on: boolean, names = false) {
  const call = vi.fn(async (_method: string, p: AiTranslateParams & { texts: string[] }) => ({
    items: p.texts.map((t) => (t === 'already' ? { text: t, source: '영어' } : { text: `번역:${t}`, source: '영어' })),
  }));
  const channel = { channel: 'c1', settings: { security: 'normal' }, api_read_allowed: false } as unknown as ChannelView;
  const view = { space: 's1', kind: 'crew', members: [], my_permissions: ['use_ai_translate'], channels: [channel] } as unknown as SpaceView;
  useStore.setState({
    client: { call } as unknown as ApiClient,
    views: { s1: view },
    prefs: { ...useStore.getState().prefs, aiTr: { ...DEFAULT_AI_TR, profile: 'own:a', target: '한국어', always: on ? { s1: true } : {}, names } },
  });
  return call;
}

afterEach(async () => {
  await cleanup();
  useStore.setState({ client: null, views: {} });
});

const wait = () => act(() => new Promise((r) => setTimeout(r, 100)));

describe('MessageBody', () => {
  it('켠 크루의 글을 한 번에 묶어 번역하고, 표시를 누르면 원문을 본다', async () => {
    const call = setup(true);
    const { el } = await render(
      <>
        <MessageBody text="hello" space="s1" channel="c1" members={members} me={null} />
        <MessageBody text="world" space="s1" channel="c1" members={members} me={null} />
        <MessageBody text="already" space="s1" channel="c1" members={members} me={null} />
        <MessageBody text="mine" space="s1" channel="c1" members={members} me={null} skip />
      </>,
    );
    await wait();
    expect(call).toHaveBeenCalledTimes(1);
    expect(call.mock.calls[0][1]).toMatchObject({ profile: 'a', crew: false, space: 's1', channel: 'c1', target: '한국어', texts: ['hello', 'world', 'already'] });
    expect(el.textContent).toContain('번역:hello');
    expect(el.textContent).toContain('번역:world');
    // 원문과 같게 돌아온 글·내 글은 표시를 붙이지 않습니다.
    const marks = el.querySelectorAll<HTMLButtonElement>('.tr-mark');
    expect(marks.length).toBe(2);
    expect(marks[0].textContent).toContain('영어');
    await click(marks[0]);
    expect(el.textContent).toContain('hello');
    expect(el.textContent).not.toContain('번역:hello');
  });

  it('번역을 켜지 않은 크루는 보내지 않는다', async () => {
    const call = setup(false);
    const { el } = await render(<MessageBody text="hello" space="s1" channel="c1" members={members} me={null} />);
    await wait();
    expect(call).not.toHaveBeenCalled();
    expect(el.textContent).toBe('hello');
  });
});

describe('TrText', () => {
  it('채널 설명·포럼 글은 채널 내용으로 번역하고, 역할 이름은 ‘이름도 번역’을 켰을 때만 번역한다', async () => {
    const call = setup(true);
    const { el } = await render(
      <>
        <TrText text="topic" space="s1" channel="c1" />|<TrText text="role" space="s1" emoji />
      </>,
    );
    await wait();
    expect(call).toHaveBeenCalledTimes(1);
    expect(call.mock.calls[0][1]).toMatchObject({ space: 's1', channel: 'c1', texts: ['topic'] });
    expect(el.textContent).toBe('번역:topic|role');
    expect(el.querySelector('span[title]')?.getAttribute('title')).toContain('topic');
  });

  it('문자열 자리(입력창 안내·환영 문구 속 채널 이름)도 이름 번역을 따른다', async () => {
    function Probe() {
      return <i>{useTrText('welcome-name', 's1')}</i>;
    }
    const call = setup(true, true);
    const { el } = await render(<Probe />);
    await wait();
    expect(call).toHaveBeenCalledTimes(1);
    expect(el.textContent).toBe('번역:welcome-name');
  });
});

describe('clearTranslateCache', () => {
  it('캐시를 비우면 보이는 글을 다시 번역한다', async () => {
    const call = setup(true);
    const { el } = await render(<MessageBody text="again" space="s1" channel="c1" members={members} me={null} />);
    await wait();
    expect(call).toHaveBeenCalledTimes(1);
    await act(async () => clearTranslateCache());
    await wait();
    expect(call).toHaveBeenCalledTimes(2);
    expect(el.textContent).toContain('번역:again');
  });
});
