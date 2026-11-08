/// <reference types="node" />
// 최소 W3C WebDriver 클라이언트(Node 내장 fetch만): 세션, 요소 찾기·누르기·입력, 스크립트 실행.
// 창 연기 시험 하나에 쓰려고 직접 씁니다(무거운 WebDriver 패키지를 개발 의존성에 들이지 않음).

/** W3C 요소 참조 키. */
const ELEMENT = 'element-6066-11e4-a52e-4f735466cecf';

export type ElementId = string;

function elementId(v: unknown): ElementId {
  const id = v && typeof v === 'object' ? (v as Record<string, unknown>)[ELEMENT] : undefined;
  if (typeof id !== 'string') throw new WebDriverError('unexpected response', '요소 참조가 없음');
  return id;
}

/** 드라이버가 돌려준 오류(`no such element` 등). */
export class WebDriverError extends Error {
  readonly error: string;
  constructor(error: string, message: string) {
    super(`${error}: ${message}`);
    this.error = error;
  }
}

/** 명령 하나의 최대 대기. 걸린 드라이버가 시험·뒷정리를 붙잡지 않게 합니다(앱 첫 실행을 감안해 넉넉히). */
export const REQUEST_MS = 90_000;

async function request(
  base: string,
  method: 'GET' | 'POST' | 'DELETE',
  path: string,
  body?: unknown,
  timeoutMs = REQUEST_MS,
): Promise<unknown> {
  const res = await fetch(`${base}${path}`, {
    method,
    headers: body === undefined ? undefined : { 'content-type': 'application/json; charset=utf-8' },
    body: body === undefined ? undefined : JSON.stringify(body),
    signal: AbortSignal.timeout(timeoutMs),
  });
  const text = await res.text();
  let value: unknown;
  try {
    value = (JSON.parse(text) as { value?: unknown }).value;
  } catch {
    value = undefined;
  }
  const err = value && typeof value === 'object' && 'error' in value ? (value as { error: string; message?: string }) : null;
  if (!res.ok || err) {
    throw new WebDriverError(err?.error ?? `HTTP ${res.status}`, err?.message ?? text.slice(0, 300));
  }
  return value;
}

export class WebDriver {
  private readonly base: string;
  readonly sessionId: string;

  private constructor(base: string, sessionId: string) {
    this.base = base;
    this.sessionId = sessionId;
  }

  /** 드라이버가 새 세션을 받을 수 있는지(`GET /status`). 아직 듣지 않으면 false. */
  static async ready(base: string): Promise<boolean> {
    try {
      const v = (await request(base, 'GET', '/status', undefined, 5_000)) as { ready?: boolean } | undefined;
      return v?.ready !== false;
    } catch {
      return false;
    }
  }

  static async newSession(base: string, alwaysMatch: Record<string, unknown>): Promise<WebDriver> {
    const v = (await request(base, 'POST', '/session', { capabilities: { alwaysMatch } })) as { sessionId: string };
    return new WebDriver(base, v.sessionId);
  }

  private s(path: string): string {
    return `/session/${this.sessionId}${path}`;
  }

  async find(css: string): Promise<ElementId> {
    return elementId(await request(this.base, 'POST', this.s('/element'), { using: 'css selector', value: css }));
  }

  async findAll(css: string): Promise<ElementId[]> {
    const v = await request(this.base, 'POST', this.s('/elements'), { using: 'css selector', value: css });
    if (!Array.isArray(v)) throw new WebDriverError('unexpected response', '요소 목록이 아님');
    return v.map(elementId);
  }

  async click(el: ElementId): Promise<void> {
    await request(this.base, 'POST', this.s(`/element/${el}/click`), {});
  }

  async type(el: ElementId, text: string): Promise<void> {
    await request(this.base, 'POST', this.s(`/element/${el}/value`), { text });
  }

  async enabled(el: ElementId): Promise<boolean> {
    return (await request(this.base, 'GET', this.s(`/element/${el}/enabled`))) === true;
  }

  /** 페이지에서 스크립트를 실행해 결과를 돌려받습니다(`return …`). */
  async execute<T>(script: string, args: unknown[] = []): Promise<T> {
    return (await request(this.base, 'POST', this.s('/execute/sync'), { script, args })) as T;
  }

  async title(): Promise<string> {
    return (await request(this.base, 'GET', this.s('/title'))) as string;
  }

  /** 세션을 닫습니다(앱 창도 닫힘). 뒷정리에서 부르므로 `timeoutMs`까지만 기다립니다. */
  async close(timeoutMs = REQUEST_MS): Promise<void> {
    await request(this.base, 'DELETE', this.s(''), undefined, timeoutMs);
  }
}

/** 요소가 아직 없거나 다시 그려져 참조가 낡은 것은 "아직"으로 봅니다. */
export function notYet(e: unknown): boolean {
  return e instanceof WebDriverError && (e.error === 'no such element' || e.error === 'stale element reference' || e.error === 'element not interactable');
}
