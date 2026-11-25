// AI 번역 설정(Prefs.aiTr)과 이번에 쓸 프로필·언어를 고르는 규칙. 요청·결과는 hooks/useTranslated.ts.

import { EVERYONE_ROLE, type AiApi, type AiTranslateParams, type ChannelView, type CrewAiProfile, type SpaceId, type SpaceView, type UserId } from '../api/types';
import { currentLang, LANGS, t } from '../i18n';
import { hasChannelPerm } from './perms';

/** 원본 언어(`auto`면 자동 감지)와 번역할 언어. 둘 다 자연어 그대로 모델에 넘깁니다. */
export interface AiTrLang {
  source: string;
  auto: boolean;
  target: string;
}

export interface AiTrPrefs extends AiTrLang {
  /** 기본 프로필(`own:<id>`, 빈 글이면 없음). 크루 제공 프로필은 그 크루에서만 쓰므로 `crewProfile`에. */
  profile: string;
  /** 크루별로 고른 프로필(`own:<id>` 또는 `crew:<id>`). */
  crewProfile: Record<SpaceId, string>;
  /** ‘이 크루에서만 쓰는 언어로 저장’. */
  crewLang: Record<SpaceId, AiTrLang>;
  /**
   * ‘이 크루 번역하기’: 켠(true) 크루·대화는 어느 채널에 들어가도 번역하고, 끈(false) 곳은 번역하지 않습니다. 정하지 않은
   * 크루는 관리자의 멤버 자동 번역을 따릅니다(`autoOn`).
   */
  always: Record<SpaceId, boolean>;
  /** 번역이 켜진 크루에서 채널·크루 이름도 번역. */
  names: boolean;
  /** ‘내가 쓴 글도 번역’(비용이 들어 처음엔 끔). */
  mine: boolean;
}

export const DEFAULT_AI_TR: AiTrPrefs = {
  source: '',
  auto: true,
  target: '',
  profile: '',
  crewProfile: {},
  crewLang: {},
  always: {},
  names: false,
  mine: false,
};

/** 언어 입력 길이(데몬 MAX_LANG와 같음). */
export const MAX_LANG = 100;
/** 크루·채널별로 기억하는 수(설정 파일 64KiB 안에 들게). */
const MAX_ENTRIES = 200;

/** API 형식 이름(설정·자세히 보기). */
export function apiName(api: AiApi): string {
  return api === 'openai' ? t('OpenAI 호환') : api === 'anthropic' ? 'Anthropic' : 'Google Gemini';
}

/** 설정에서 고르는 서비스(Base URL을 채워 줌). 고른 뒤에도 주소는 바꿀 수 있습니다. */
export const AI_PRESETS: ReadonlyArray<{ name: string; api: AiApi; base_url: string; model: string }> = [
  { name: 'OpenAI', api: 'openai', base_url: 'https://api.openai.com/v1', model: 'gpt-4o-mini' },
  { name: 'Anthropic', api: 'anthropic', base_url: 'https://api.anthropic.com', model: 'claude-haiku-5-5' },
  { name: 'Google Gemini', api: 'gemini', base_url: 'https://generativelanguage.googleapis.com/v1beta', model: 'gemini-2.5-flash' },
  { name: 'OpenRouter', api: 'openai', base_url: 'https://openrouter.ai/api/v1', model: '' },
  { name: 'Groq', api: 'openai', base_url: 'https://api.groq.com/openai/v1', model: '' },
  { name: 'DeepSeek', api: 'openai', base_url: 'https://api.deepseek.com/v1', model: 'deepseek-chat' },
  { name: 'Mistral', api: 'openai', base_url: 'https://api.mistral.ai/v1', model: 'mistral-small-latest' },
  { name: 'xAI', api: 'openai', base_url: 'https://api.x.ai/v1', model: '' },
];

const str = (v: unknown) => (typeof v === 'string' ? v.slice(0, MAX_LANG) : '');

function sanitizeLang(v: unknown): AiTrLang {
  const x = (v && typeof v === 'object' ? v : {}) as Record<string, unknown>;
  return { source: str(x.source), auto: x.auto !== false, target: str(x.target) };
}

function sanitizeMap<T>(v: unknown, item: (x: unknown) => T | undefined): Record<string, T> {
  const out: Record<string, T> = {};
  if (!v || typeof v !== 'object') return out;
  for (const [k, x] of Object.entries(v as Record<string, unknown>).slice(-MAX_ENTRIES)) {
    const it = k.length <= 96 ? item(x) : undefined;
    if (it !== undefined) out[k] = it;
  }
  return out;
}

const profileRef = (v: unknown) => (typeof v === 'string' && /^(own|crew):[0-9A-Za-z_-]{1,32}$/.test(v) ? v : undefined);

export function sanitizeAiTr(raw: unknown): AiTrPrefs {
  const x = (raw && typeof raw === 'object' ? raw : {}) as Record<string, unknown>;
  const on = (v: unknown) => (v === true ? true : undefined);
  const always = sanitizeMap(x.always, (v) => (typeof v === 'boolean' ? v : undefined));
  // 예전 ‘이 채널 번역하기’(채널마다 `공간/채널`)는 그 크루 전체 번역으로 옮깁니다.
  for (const k of Object.keys(sanitizeMap(x.channels, on))) always[k.split('/')[0]] = true;
  return {
    ...sanitizeLang(x),
    profile: profileRef(x.profile)?.startsWith('own:') ? (x.profile as string) : '',
    crewProfile: sanitizeMap(x.crewProfile, profileRef),
    crewLang: sanitizeMap(x.crewLang, sanitizeLang),
    always,
    names: x.names === true,
    mine: x.mine === true,
  };
}

/** 번역할 언어가 비어 있으면 화면 언어(그 언어 이름). */
export function defaultTarget(): string {
  const lang = currentLang();
  return LANGS.find(([c]) => c === lang)?.[1] ?? 'English';
}

/** 이 크루에서 쓸 언어(크루 전용으로 저장한 것이 먼저). */
export function langFor(p: AiTrPrefs, space: SpaceId | undefined): AiTrLang {
  return (space && p.crewLang[space]) || p;
}

function heldRoles(space: SpaceView, me: UserId | null | undefined): Set<string> {
  return new Set([EVERYONE_ROLE, ...(space.members.find((m) => m.user === me)?.roles ?? [])]);
}

/** 크루가 내게 내준 프로필(내가 가진 역할이 하나라도 들어 있는 것, 데몬 ai_profile_usable과 같은 규칙). */
export function usableCrewProfiles(space: SpaceView | undefined, me: UserId | null | undefined): CrewAiProfile[] {
  if (!space?.ai_profiles?.length) return [];
  const held = heldRoles(space, me);
  return space.ai_profiles.filter((p) => p.roles.some((r) => held.has(r)));
}

/**
 * AI 번역을 쓸 수 있는지(데몬 ai_translate_allowed와 같은 규칙): 크루는 `use_ai_translate` 권한(채널이 있으면 채널
 * 덮어쓰기까지, 없으면 크루 역할만), 크루가 아닌 대화는 늘.
 */
export function channelAllowed(space: SpaceView, channel: ChannelView | undefined, me: UserId | null | undefined): boolean {
  return space.kind !== 'crew' || hasChannelPerm(space, channel, me, 'use_ai_translate');
}

/**
 * 이 크루에서 고른 프로필(크루 것은 지금도 쓸 수 있을 때만). 고르지 않았으면, 관리자가 멤버 자동 번역을 켠 크루는 내가
 * 쓸 수 있는 크루 프로필(크루가 내는 번역이라 내 키로 청구되지 않게), 아니면 기본 프로필.
 */
export function profileFor(p: AiTrPrefs, space: SpaceView | undefined, me: UserId | null | undefined): string {
  const picked = space ? p.crewProfile[space.space] : undefined;
  if (picked?.startsWith('own:')) return picked;
  const crew = usableCrewProfiles(space, me);
  if (picked && crew.some((c) => `crew:${c.id}` === picked)) return picked;
  if (space?.ai_auto_lang && crew.length) return `crew:${crew[0].id}`;
  return p.profile;
}

/**
 * 관리자의 멤버 자동 번역이 나에게 적용되는지: 크루 언어를 정했고, 내 앱 화면 언어가 그 언어가 아니고, 내가 쓸 수 있는
 * 크루 프로필이 있을 때.
 */
export function autoOn(space: SpaceView | undefined, me: UserId | null | undefined, lang = currentLang()): boolean {
  return !!space?.ai_auto_lang && space.kind === 'crew' && space.ai_auto_lang !== lang && usableCrewProfiles(space, me).length > 0;
}

/** 이 크루(대화)에서 번역이 켜졌는지: 내가 켜거나 끈 것이 먼저, 정하지 않았으면 멤버 자동 번역. */
export function trOn(p: AiTrPrefs, space: SpaceView | undefined, me: UserId | null | undefined): boolean {
  if (!space) return false;
  const mine = p.always[space.space];
  return mine ?? autoOn(space, me);
}

/** 한 번의 번역 요청에서 같은 묶음으로 보낼 조건과 결과 캐시 열쇠의 앞부분. */
export interface TrCtx {
  params: Omit<AiTranslateParams, 'texts'>;
  /** 프로필·언어가 같으면 같은 결과(채널은 권한 확인용이라 넣지 않음). */
  cacheKey: string;
  /** 휘발성 채널 본문: 결과를 캐시에 두지 않습니다. */
  volatile: boolean;
}

/**
 * 이 글을 번역할 조건. 꺼져 있거나 프로필이 없거나 권한이 없으면 null.
 * `channel`이 있으면 본문(채널 권한·휘발성 확인), 없으면 채널·크루 이름(크루 역할의 권한만).
 */
export function trContext(p: AiTrPrefs, space: SpaceView | undefined, channel: ChannelView | undefined, me: UserId | null | undefined): TrCtx | null {
  if (!space || !trOn(p, space, me) || (!channel && !p.names)) return null;
  if (!channelAllowed(space, channel, me)) return null;
  const ref = profileFor(p, space, me);
  if (!ref) return null;
  const volatile = channel?.settings.security === 'volatile';
  // 휘발성 채널은 ‘AI 읽기 허용’을 켠 기기만 밖(AI 서비스)으로 보냅니다(데몬도 다시 확인).
  if (channel && volatile && !channel.api_read_allowed) return null;
  const crew = ref.startsWith('crew:');
  const profile = ref.slice(ref.indexOf(':') + 1);
  const lang = langFor(p, space.space);
  const source = lang.auto ? undefined : lang.source.trim() || undefined;
  const target = lang.target.trim() || defaultTarget();
  return {
    params: { profile, crew, space: space.space, channel: channel?.channel, source, target },
    cacheKey: [crew ? `crew:${space.space}:${profile}` : `own:${profile}`, source ?? '', target].join('\n'),
    volatile: !!volatile,
  };
}
