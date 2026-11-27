// 화면에 보이는 글을 모아 `ai.translate`로 묶어 보내고 결과를 돌려줍니다.
// 여기 캐시는 메모리에만 둡니다(설정 파일 X). 앱을 다시 켜도 남는 캐시는 데몬이 암호화 DB에 둡니다(휘발성·수명 있는 채널 글은
// 빼고). 휘발성 채널 본문은 이 캐시에도 두지 않고, 그 글을 그린 컴포넌트가 사라지면 함께 사라집니다.

import { useEffect, useState, useSyncExternalStore } from 'react';
import type { AiTranslation, ChannelView, SpaceView } from '../api/types';
import { t } from '../i18n';
import { trContext, type TrCtx } from '../lib/aiTr';
import { useStore } from '../store/store';

/** 한 요청에 넣는 글 수·글자 수(데몬 상한 64개·16000자보다 작게: 응답이 잘리지 않게). */
const BATCH_TEXTS = 30;
const BATCH_CHARS = 3000;
/** 동시에 도는 요청 수. */
const MAX_RUNNING = 2;
/** 캐시 크기. 넘치면 오래된 것부터 버립니다. */
const MAX_CACHE = 4000;
/** 실패한 조건(프로필·언어)은 이만큼 다시 보내지 않습니다(같은 오류 알림이 쏟아지지 않게). */
const FAIL_COOLDOWN_MS = 60_000;

type Waiter = (r: AiTranslation | null) => void;

const cache = new Map<string, AiTranslation>();
const inflight = new Map<string, Waiter[]>();
const failedUntil = new Map<string, number>();
const queue = new Map<string, { ctx: TrCtx; items: Map<string, string> }>();
const jobs: Array<{ ctx: TrCtx; keys: string[]; texts: string[] }> = [];
let timer: ReturnType<typeof setTimeout> | null = null;
let running = 0;
/** 캐시를 비울 때마다 늘어, 보이는 글이 다시 번역을 요청하게 합니다. */
let generation = 0;
const genListeners = new Set<() => void>();

const groupOf = (ctx: TrCtx) => JSON.stringify(ctx.params);

function settle(key: string, r: AiTranslation | null, keep: boolean) {
  if (r && keep) {
    cache.delete(key);
    cache.set(key, r);
    if (cache.size > MAX_CACHE) for (const k of [...cache.keys()].slice(0, cache.size - MAX_CACHE)) cache.delete(k);
  }
  for (const w of inflight.get(key) ?? []) w(r);
  inflight.delete(key);
}

async function run(job: (typeof jobs)[number]) {
  const client = useStore.getState().client;
  const gen = generation;
  try {
    if (!client) throw new Error(t('데몬에 연결되지 않았습니다'));
    const { items } = await client.call('ai.translate', { ...job.ctx.params, texts: job.texts });
    // 도는 중에 캐시를 비웠으면 옛 결과는 캐시에 다시 넣지 않습니다.
    job.keys.forEach((k, i) => settle(k, items[i] ?? null, !job.ctx.volatile && gen === generation));
  } catch (e) {
    const group = groupOf(job.ctx);
    if ((failedUntil.get(group) ?? 0) < Date.now()) {
      useStore.getState().toast('error', t('AI 번역 실패: {error}', { error: e instanceof Error ? e.message : String(e) }));
    }
    failedUntil.set(group, Date.now() + FAIL_COOLDOWN_MS);
    for (const k of job.keys) settle(k, null, false);
  }
}

function pump() {
  while (running < MAX_RUNNING && jobs.length) {
    const job = jobs.shift()!;
    running++;
    void run(job).finally(() => {
      running--;
      pump();
    });
  }
}

function flush() {
  timer = null;
  for (const { ctx, items } of queue.values()) {
    let keys: string[] = [];
    let texts: string[] = [];
    let chars = 0;
    for (const [key, text] of items) {
      if (texts.length && (texts.length >= BATCH_TEXTS || chars + text.length > BATCH_CHARS)) {
        jobs.push({ ctx, keys, texts });
        keys = [];
        texts = [];
        chars = 0;
      }
      keys.push(key);
      texts.push(text);
      chars += text.length;
    }
    if (texts.length) jobs.push({ ctx, keys, texts });
  }
  queue.clear();
  pump();
}

/** 글 하나 번역(같은 글은 한 번만 보냄). 꺼져 있거나 실패하면 null. */
export function translate(ctx: TrCtx, text: string): Promise<AiTranslation | null> {
  const key = `${ctx.cacheKey}\n${text}`;
  const hit = cache.get(key);
  if (hit) return Promise.resolve(hit);
  const group = groupOf(ctx);
  if ((failedUntil.get(group) ?? 0) > Date.now()) return Promise.resolve(null);
  return new Promise((resolve) => {
    const waiting = inflight.get(key);
    if (waiting) {
      waiting.push(resolve);
      return;
    }
    inflight.set(key, [resolve]);
    let q = queue.get(group);
    if (!q) queue.set(group, (q = { ctx, items: new Map() }));
    q.items.set(key, text);
    timer ??= setTimeout(flush, 60);
  });
}

/** 설정을 바꿔 다시 시도할 때(프로필 저장 등) 실패 기록을 지웁니다. */
export function resetTranslateFailures() {
  failedUntil.clear();
}

/** 번역 캐시 초기화: 메모리의 번역 결과·실패 기록을 지우고 보이는 글을 다시 번역합니다. */
export function clearTranslateCache() {
  cache.clear();
  failedUntil.clear();
  generation++;
  for (const l of genListeners) l();
}

function subscribeGen(l: () => void) {
  genListeners.add(l);
  return () => genListeners.delete(l);
}

/** 번역할 만한 글인지(빈 글·기호만은 보내지 않음). */
const worth = (text: string | undefined): text is string => !!text && /\p{L}/u.test(text);

const SLUG_SEP = /[\p{L}\p{N}]([-_])[\p{L}\p{N}]/u;
const SPACED = /[\p{L}\p{N}]\s+[\p{L}\p{N}]/u;
const SEP_RUN = /(?<=[\p{L}\p{N}])[-_]+(?=[\p{L}\p{N}])/gu;
const SPACE_RUN = /(?<=[\p{L}\p{N}])\s+(?=[\p{L}\p{N}])/gu;

/**
 * `supporter-thanks-board`처럼 띄어쓰기 없이 `-`·`_`로 이은 이름이면 그 구분자. 모델이 이런 이름을 식별자로 보고 그대로
 * 두는 일이 잦아(지시문으로도 다 막히지 않음), 단어로 풀어 보내고 결과를 다시 잇습니다.
 */
export function slugSep(text: string): string | null {
  const m = SLUG_SEP.exec(text);
  return m && !SPACED.test(text) ? m[1] : null;
}

/**
 * 번역 결과. `channel`이 있으면 그 채널 본문, 없으면 채널·크루 이름으로 다룹니다.
 * 번역이 꺼져 있거나 아직 오지 않았거나 원문과 같으면 null.
 */
export function useTranslated(text: string | undefined, space: SpaceView | undefined, channel?: ChannelView): AiTranslation | null {
  const prefs = useStore((s) => s.prefs.aiTr);
  const me = useStore((s) => s.identity?.user);
  const gen = useSyncExternalStore(subscribeGen, () => generation);
  const sep = !channel && text ? slugSep(text) : null;
  const sent = sep && text ? text.replace(SEP_RUN, ' ') : text;
  const ctx = worth(sent) ? trContext(prefs, space, channel, me) : null;
  const key = ctx ? `${gen}\n${ctx.cacheKey}\n${JSON.stringify(ctx.params)}\n${sent}` : '';
  const [result, setResult] = useState<{ key: string; r: AiTranslation | null } | null>(null);
  useEffect(() => {
    if (!ctx || !sent) return;
    let alive = true;
    void translate(ctx, sent).then((r) => alive && setResult({ key, r }));
    return () => {
      alive = false;
    };
    // ctx는 key에 모두 들어 있습니다.
  }, [key]);
  const got = ctx && sent ? (result?.key === key ? result.r : cache.get(`${ctx.cacheKey}\n${sent}`) ?? null) : null;
  const r = got && sep ? { ...got, text: got.text.replace(SPACE_RUN, sep) } : got;
  return r && r.text.trim() && r.text !== text ? r : null;
}
