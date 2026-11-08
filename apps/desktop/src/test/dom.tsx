// 컴포넌트 시험 도우미(happy-dom 환경에서만 씀). React 공식 act + createRoot로 그리고, 끝나면 지웁니다.

import { act, type ReactNode } from 'react';
import { createRoot } from 'react-dom/client';

// act 경고 없이 상태 갱신을 기다리게 합니다.
(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

export interface Rendered {
  el: HTMLElement;
  unmount(): Promise<void>;
}

const mounted: Rendered[] = [];

export async function render(ui: ReactNode): Promise<Rendered> {
  const el = document.createElement('div');
  document.body.appendChild(el);
  const root = createRoot(el);
  await act(async () => root.render(ui));
  const r: Rendered = {
    el,
    async unmount() {
      await act(async () => root.unmount());
      el.remove();
    },
  };
  mounted.push(r);
  return r;
}

/** afterEach에서 부릅니다. */
export async function cleanup() {
  while (mounted.length) await mounted.pop()!.unmount();
}

/** 글자가 같은 버튼을 찾습니다(없으면 오류). */
export function button(el: HTMLElement, text: string): HTMLButtonElement {
  const b = [...el.querySelectorAll('button')].find((x) => x.textContent?.trim().includes(text));
  if (!b) throw new Error(`버튼 없음: ${text}`);
  return b;
}

export async function click(target: HTMLElement) {
  await act(async () => {
    target.dispatchEvent(new MouseEvent('click', { bubbles: true }));
  });
}
