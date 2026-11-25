import { describe, expect, it } from 'vitest';
import type { ChannelView, CrewAiProfile, MemberView, SpaceView } from '../api/types';
import { EVERYONE_ROLE } from '../api/types';
import { DEFAULT_AI_TR, autoOn, channelAllowed, profileFor, sanitizeAiTr, trContext, trOn, usableCrewProfiles, type AiTrPrefs } from './aiTr';

const prof = (id: string, roles: string[]): CrewAiProfile => ({
  id,
  name: id,
  api: 'openai',
  base_url: 'https://api.example.com/v1',
  model: 'm',
  roles,
  holder: 'd1',
  key_version: 1,
});

function space(over: Partial<SpaceView> = {}): SpaceView {
  return {
    space: 's1',
    kind: 'crew',
    settings: { name: '크루', description: '', icon: null, allow_external_emoji: false },
    owner: 'u1',
    committer: 'd1',
    members: [{ user: 'u2', roles: ['r1'] } as unknown as MemberView, { user: 'u3', roles: [] } as unknown as MemberView],
    roles: [],
    channels: [],
    emojis: [],
    my_permissions: [],
    pending_commit: false,
    dormant: false,
    trusted: false,
    ai_profiles: [prof('p1', ['r1']), prof('p2', [EVERYONE_ROLE])],
    ...over,
  };
}

function chan(over: Partial<ChannelView> = {}, security: 'normal' | 'volatile' = 'normal'): ChannelView {
  return {
    channel: 'c1',
    settings: {
      name: 'c1',
      kind: 'text',
      topic: '',
      parent: null,
      position: 0,
      security,
      join_mode: 'always',
      ttl: { mode: 'none' },
      history: 'private',
      link_previews: true,
      overwrites: [],
      forum_tags: [],
    },
    unread: 0,
    api_read_allowed: false,
    p2p: false,
    p2p_joined: false,
    ...over,
  };
}

const prefs = (over: Partial<AiTrPrefs> = {}): AiTrPrefs => ({ ...DEFAULT_AI_TR, profile: 'own:a', target: '한국어', ...over });

describe('AI 번역 조건', () => {
  it('크루 프로필은 역할(@everyone 포함)을 가진 멤버만 쓴다', () => {
    expect(usableCrewProfiles(space(), 'u2').map((p) => p.id)).toEqual(['p1', 'p2']);
    expect(usableCrewProfiles(space(), 'u3').map((p) => p.id)).toEqual(['p2']);
  });

  it('AI 번역 권한: 크루는 use_ai_translate(채널 덮어쓰기까지), 크루가 아닌 대화는 늘', () => {
    const allowed = space({ my_permissions: ['view_channel', 'use_ai_translate'] });
    expect(channelAllowed(allowed, chan(), 'u3')).toBe(true);
    expect(channelAllowed(allowed, undefined, 'u3')).toBe(true);
    expect(channelAllowed(space(), chan(), 'u3')).toBe(false);
    expect(channelAllowed(space({ my_permissions: ['administrator'] }), chan(), 'u3')).toBe(true);
    expect(channelAllowed(space({ kind: 'space' }), chan(), 'u3')).toBe(true);
    // 채널에서 @everyone은 막고 r1은 허용: 크루 설정을 따르지 않고 채널 설정대로.
    const bit = 2 ** 14;
    const deny = chan({
      settings: {
        ...chan().settings,
        overwrites: [
          { target: { type: 'role', id: EVERYONE_ROLE }, allow: 0, deny: bit },
          { target: { type: 'role', id: 'r1' }, allow: bit, deny: 0 },
        ],
      },
    });
    expect(channelAllowed(allowed, deny, 'u3')).toBe(false);
    expect(channelAllowed(allowed, deny, 'u2')).toBe(true);
    expect(channelAllowed(space({ my_permissions: ['view_channel'] }), deny, 'u2')).toBe(true);
  });

  it('크루 프로필은 그 크루 안에서만: 더 쓸 수 없거나 다른 크루면 기본(내) 프로필로', () => {
    expect(profileFor(prefs({ crewProfile: { s1: 'crew:p1' } }), space(), 'u2')).toBe('crew:p1');
    expect(profileFor(prefs({ crewProfile: { s1: 'crew:p1' } }), space(), 'u3')).toBe('own:a');
    // s1에서 고른 크루 프로필은 s2에서 쓰지 않고, s2에 저장된 같은 이름도 s2가 내준 프로필이 아니면 쓰지 않습니다.
    expect(profileFor(prefs({ crewProfile: { s1: 'crew:p1' } }), space({ space: 's2' }), 'u2')).toBe('own:a');
    expect(profileFor(prefs({ crewProfile: { s2: 'crew:p1' } }), space({ space: 's2', ai_profiles: [] }), 'u2')).toBe('own:a');
  });

  it('‘이 크루 번역하기’를 켠 크루의 모든 채널에서, 권한이 있을 때, 휘발성은 AI 읽기를 허용했을 때만 번역한다', () => {
    const sp = space({ my_permissions: ['view_channel', 'use_ai_translate'] });
    expect(trContext(prefs(), sp, chan(), 'u2')).toBeNull();
    const on = prefs({ always: { s1: true } });
    expect(trContext(on, sp, chan(), 'u2')?.params).toEqual({ profile: 'a', crew: false, space: 's1', channel: 'c1', source: undefined, target: '한국어' });
    expect(trContext(on, sp, chan({ channel: 'c9' }), 'u2')?.params.channel).toBe('c9');
    expect(trContext(on, space(), chan(), 'u2')).toBeNull();
    expect(trContext(on, sp, chan({}, 'volatile'), 'u2')).toBeNull();
    const v = trContext(on, sp, chan({ api_read_allowed: true }, 'volatile'), 'u2');
    expect(v?.volatile).toBe(true);
    // 이름은 ‘이름도 번역’을 켰을 때만, 크루 역할의 권한으로.
    expect(trContext(on, sp, undefined, 'u2')).toBeNull();
    expect(trContext({ ...on, names: true }, sp, undefined, 'u2')?.params.channel).toBeUndefined();
    expect(trContext({ ...on, names: true }, space(), undefined, 'u2')).toBeNull();
    expect(trOn(on, sp, 'u2')).toBe(true);
    expect(trOn(on, space({ space: 's2' }), 'u2')).toBe(false);
  });

  it('관리자의 멤버 자동 번역: 화면 언어가 크루 언어가 아니고 크루 프로필을 쓸 수 있으면, 내가 정하지 않았을 때만 켠다', () => {
    expect(autoOn(space({ ai_auto_lang: 'ko' }), 'u2', 'en')).toBe(true);
    expect(autoOn(space({ ai_auto_lang: 'ko' }), 'u2', 'ko')).toBe(false);
    expect(autoOn(space(), 'u2', 'en')).toBe(false);
    // 쓸 수 있는 크루 프로필이 없으면 켜지 않습니다(u3는 r1이 없음).
    expect(autoOn(space({ ai_auto_lang: 'ko', ai_profiles: [prof('p1', ['r1'])] }), 'u3', 'en')).toBe(false);
    expect(autoOn(space({ ai_auto_lang: 'ko', kind: 'space' }), 'u2', 'en')).toBe(false);

    // 'xx'는 어떤 화면 언어와도 다릅니다.
    const sp = space({ ai_auto_lang: 'xx', my_permissions: ['view_channel', 'use_ai_translate'] });
    expect(trOn(prefs(), sp, 'u2')).toBe(true);
    expect(trOn(prefs({ always: { s1: false } }), sp, 'u2')).toBe(false);
    expect(trOn(prefs({ always: { s1: true } }), space(), 'u2')).toBe(true);
    // 고르지 않았으면 내가 쓸 수 있는 크루 프로필로(내 키로 청구되지 않게), 고른 것이 있으면 그것.
    expect(profileFor(prefs(), sp, 'u2')).toBe('crew:p1');
    expect(profileFor(prefs(), sp, 'u3')).toBe('crew:p2');
    expect(profileFor(prefs({ crewProfile: { s1: 'own:a' } }), sp, 'u2')).toBe('own:a');
    expect(trContext(prefs(), sp, chan(), 'u2')?.params).toMatchObject({ profile: 'p1', crew: true, channel: 'c1' });
    // 휘발성 채널은 자동 번역이어도 AI 읽기를 허용해야 합니다.
    expect(trContext(prefs(), sp, chan({}, 'volatile'), 'u2')).toBeNull();
  });

  it('크루 전용 언어와 원본 언어 지정', () => {
    const p = prefs({ always: { s1: true }, crewLang: { s1: { source: 'English', auto: false, target: '쉬운 한국어' } } });
    const ctx = trContext(p, space({ my_permissions: ['view_channel', 'use_ai_translate'] }), chan(), 'u2')!;
    expect([ctx.params.source, ctx.params.target]).toEqual(['English', '쉬운 한국어']);
    expect(trContext({ ...p, crewProfile: { s1: 'crew:p1' } }, space({ my_permissions: ['view_channel', 'use_ai_translate'] }), chan(), 'u2')?.cacheKey).toContain('crew:s1:p1');
  });

  it('저장된 설정의 모르는 값·형식은 버린다', () => {
    const s = sanitizeAiTr({ profile: 'crew:x', crewProfile: { s1: 'own:a', s2: 'bad id!' }, always: { s1: true, s2: 1, s3: false }, target: 'x'.repeat(500), auto: 'no' });
    expect(s.profile).toBe('');
    expect(s.crewProfile).toEqual({ s1: 'own:a' });
    expect(s.always).toEqual({ s1: true, s3: false });
    expect(s.target.length).toBe(100);
    expect(s.auto).toBe(true);
    expect(s.mine).toBe(false);
    expect(sanitizeAiTr({ mine: true }).mine).toBe(true);
    // 예전 채널별 번역은 그 크루 전체 번역으로 옮깁니다.
    expect(sanitizeAiTr({ channels: { 's3/c1': true, 's3/c2': true } }).always).toEqual({ s3: true });
  });
});
