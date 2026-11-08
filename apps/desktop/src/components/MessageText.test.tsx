// @vitest-environment happy-dom
// 메시지 본문 그리기: 마크다운은 React 요소로만 그리고(HTML은 글자 그대로), 스포일러는 눌러야 보이며, 링크는 기본 브라우저로 엽니다.
// (문법 자체는 lib/markdown.test.ts)

import { act } from 'react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import type { ApiClient } from '../api/client';
import { WEB_BASE } from '../lib/links';
import { useStore } from '../store/store';
import { cleanup, click, render } from '../test/dom';
import { MessageText } from './MessageText';

const members = new Map();

function setup() {
  const openExternal = vi.fn(async () => undefined);
  useStore.setState({ client: { call: vi.fn(), transport: { openExternal } } as unknown as ApiClient, modal: null });
  return openExternal;
}

afterEach(async () => {
  await cleanup();
  useStore.setState({ client: null });
});

describe('MessageText', () => {
  it('HTML은 요소가 아니라 글자로 보인다', async () => {
    setup();
    const { el } = await render(<MessageText text={'<b>굵게</b> <img src=x onerror=alert(1)> <script>alert(1)</script>'} members={members} me={null} />);
    expect(el.querySelector('b, img, script')).toBeNull();
    expect(el.textContent).toBe('<b>굵게</b> <img src=x onerror=alert(1)> <script>alert(1)</script>');
  });

  it('제목·굵게·목록·인용·코드 블록을 요소로 그린다', async () => {
    setup();
    const text = '# 제목\n**굵게** *기울임* __밑줄__ ~~취소~~ `코드`\n- 하나\n- 둘\n> 인용\n```js\nlet a = 1;\n```';
    const { el } = await render(<MessageText text={text} members={members} me={null} />);
    expect(el.querySelector('h1.md-h1')?.textContent).toBe('제목');
    expect(['strong', 'em', 'u', 's', 'code'].map((t) => el.querySelector(t)?.textContent)).toEqual(['굵게', '기울임', '밑줄', '취소', '코드']);
    expect([...el.querySelectorAll('ul.md-list > li')].map((x) => x.textContent)).toEqual(['하나', '둘']);
    expect(el.querySelector('blockquote.md-quote')?.textContent).toBe('인용');
    expect(el.querySelector('pre')?.getAttribute('data-lang')).toBe('js');
    expect(el.querySelector('pre code')?.textContent).toBe('let a = 1;');
  });

  it('스포일러는 누르기 전까지 가리고(안의 링크도 보이지 않게), 누르면 그 자리에서 보인다', async () => {
    const openExternal = setup();
    const { el } = await render(<MessageText text="정답은 ||범인은 https://a.example 집사||" members={members} me={null} />);
    const sp = el.querySelector<HTMLElement>('.spoiler')!;
    expect(sp.getAttribute('role')).toBe('button');
    expect(sp.querySelector('.spoiler-inner')?.getAttribute('aria-hidden')).toBe('true');
    await click(sp);
    expect(openExternal).not.toHaveBeenCalled();
    expect(el.querySelector('.spoiler.shown')?.textContent).toBe('범인은 https://a.example 집사');
    expect(el.querySelector('.spoiler-inner')).toBeNull();
  });

  it('가린 링크는 실제 주소를 툴팁으로 보이고 기본 브라우저로 연다', async () => {
    const openExternal = setup();
    const { el } = await render(<MessageText text="[안내 문서](https://docs.example/guide) 참고" members={members} me={null} />);
    const a = el.querySelector<HTMLAnchorElement>('a.md-link')!;
    expect(a.textContent).toBe('안내 문서');
    expect(a.getAttribute('title')).toBe('https://docs.example/guide');
    // 실제 클릭처럼 취소할 수 있는 이벤트로: 웹뷰 안에서는 이동하지 않습니다.
    const ev = new MouseEvent('click', { bubbles: true, cancelable: true });
    await act(async () => {
      a.dispatchEvent(ev);
    });
    expect(ev.defaultPrevented).toBe(true);
    expect(openExternal).toHaveBeenCalledWith('https://docs.example/guide');
  });

  it('초대 링크는 브라우저(웹 초대 페이지)를 거치지 않고 크루 참가 창에 채워 연다', async () => {
    const openExternal = setup();
    const url = `${WEB_BASE}#invite/3yZe7dQx`;
    const { el } = await render(<MessageText text={`들어와: ${url}`} members={members} me={null} />);
    await click(el.querySelector('a')!);
    expect(openExternal).not.toHaveBeenCalled();
    expect(useStore.getState().modal).toEqual({ kind: 'joinCrew', url });
  });

  it('주소처럼 보이는 이름의 가린 링크와 http(s)가 아닌 링크는 링크로 만들지 않는다', async () => {
    setup();
    const { el } = await render(<MessageText text={'[https://bank.example](https://evil.example) [눌러](javascript:alert(1))'} members={members} me={null} />);
    expect(el.querySelector('a.md-link')).toBeNull();
    expect([...el.querySelectorAll('a')].every((a) => /^https:\/\//.test(a.getAttribute('href') ?? ''))).toBe(true);
    expect(el.textContent).toContain('javascript:alert(1)');
  });
});
