// MOCK 백엔드의 가짜 데이터(결정론적 난수로 만들어 스크린숏이 매번 같게 나옵니다).

import type {
  AttachmentView,
  ChannelSettings,
  ChannelView,
  EmbedView,
  EmojiView,
  ForumPostView,
  ForumTag,
  MemberView,
  MessageView,
  Profile,
  ReactionView,
  Role,
} from './types';
import { EVERYONE_ROLE } from './types';

const B58 = '123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz';

export function mulberry32(seed: number) {
  let a = seed >>> 0;
  return () => {
    a = (a + 0x6d2b79f5) >>> 0;
    let t = a;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

export class Rng {
  private readonly next: () => number;
  constructor(seed: number) {
    this.next = mulberry32(seed);
  }
  float() {
    return this.next();
  }
  int(lo: number, hi: number) {
    return lo + Math.floor(this.next() * (hi - lo + 1));
  }
  pick<T>(arr: readonly T[]): T {
    return arr[Math.floor(this.next() * arr.length)];
  }
  chance(p: number) {
    return this.next() < p;
  }
  /** base58 문자열 식별자(바이트 길이 32 → 44자, 16 → 22자). */
  id(bytes: 16 | 32): string {
    const len = bytes === 32 ? 44 : 22;
    let s = '';
    // 첫 글자를 '1'이 아니게 해서 @everyone(16바이트 0)과 겹치지 않게 합니다.
    s += B58[1 + Math.floor(this.next() * (B58.length - 1))];
    for (let i = 1; i < len; i++) s += B58[Math.floor(this.next() * B58.length)];
    return s;
  }
  digits(n: number) {
    let s = '';
    for (let i = 0; i < n; i++) s += String(Math.floor(this.next() * 10));
    return s;
  }
}

export const enc = new TextEncoder();

export function b64(str: string): string {
  const bytes = enc.encode(str);
  let bin = '';
  for (const b of bytes) bin += String.fromCharCode(b);
  return btoa(bin);
}

export function profile(name: string, extra: Partial<Profile> = {}): Profile {
  return { version: 1, name, bio: '', avatar: null, banner: null, theme_color: null, status: '', ...extra };
}

export function channel(name: string, kind: ChannelSettings['kind'], extra: Partial<ChannelSettings> = {}): ChannelSettings {
  return {
    name,
    kind,
    topic: '',
    parent: null,
    position: 0,
    security: 'normal',
    join_mode: 'always',
    ttl: { mode: 'none' },
    history: 'private',
    link_previews: true,
    overwrites: [],
    forum_tags: [],
    ...extra,
  };
}

export function channelView(id: string, settings: ChannelSettings, extra: Partial<ChannelView> = {}): ChannelView {
  return { channel: id, settings, unread: 0, api_read_allowed: false, p2p: false, p2p_joined: false, ...extra };
}

// ───────────── 그림(SVG) ─────────────

const PALETTES = [
  ['#ff9a8b', '#ff6a88', '#ff99ac'],
  ['#a1c4fd', '#c2e9fb', '#6a85b6'],
  ['#84fab0', '#8fd3f4', '#3eadcf'],
  ['#fbc2eb', '#a6c1ee', '#7f7fd5'],
  ['#f6d365', '#fda085', '#f76b1c'],
  ['#30cfd0', '#330867', '#5f72bd'],
];

/** 풍경 느낌의 SVG(첨부 이미지 대용). */
export function sceneSvg(seed: number, w: number, h: number, label: string): string {
  const r = new Rng(seed);
  const p = PALETTES[seed % PALETTES.length];
  const hills = Array.from({ length: 3 }, (_, i) => {
    const y = h * (0.55 + i * 0.12);
    const pts = Array.from({ length: 6 }, (_, k) => `${(w / 5) * k},${y - r.int(0, Math.round(h * 0.15))}`).join(' ');
    return `<polygon points="0,${h} ${pts} ${w},${h}" fill="${p[(i + 1) % 3]}" opacity="${0.55 + i * 0.15}"/>`;
  }).join('');
  return `<svg xmlns="http://www.w3.org/2000/svg" width="${w}" height="${h}" viewBox="0 0 ${w} ${h}"><defs><linearGradient id="g" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="${p[0]}"/><stop offset="1" stop-color="${p[1]}"/></linearGradient></defs><rect width="${w}" height="${h}" fill="url(#g)"/><circle cx="${w * 0.75}" cy="${h * 0.28}" r="${h * 0.12}" fill="#fff8" />${hills}<text x="${w / 2}" y="${h - 24}" font-family="sans-serif" font-size="${Math.round(h / 14)}" text-anchor="middle" fill="#fff" opacity="0.85">${label}</text></svg>`;
}

export const EMOJI_SVGS: Record<string, string> = {
  yangpa: `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64"><path d="M32 6c3 6 2 10 0 14" stroke="#4caf50" stroke-width="4" fill="none" stroke-linecap="round"/><ellipse cx="32" cy="40" rx="22" ry="20" fill="#c9a0dc"/><path d="M32 20c-10 6-14 14-14 20s4 14 14 20M32 20c10 6 14 14 14 20s-4 14-14 20" stroke="#8e5aa8" stroke-width="2.5" fill="none"/><circle cx="25" cy="38" r="3" fill="#3b2148"/><circle cx="39" cy="38" r="3" fill="#3b2148"/><path d="M26 47q6 5 12 0" stroke="#3b2148" stroke-width="2.5" fill="none" stroke-linecap="round"/></svg>`,
  ddabong: `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64"><rect x="8" y="28" width="12" height="28" rx="3" fill="#f4b400"/><path d="M22 30l10-18c3-4 8-2 7 3l-3 11h16c4 0 6 4 5 7l-5 18c-1 3-3 5-6 5H22z" fill="#ffcc4d"/></svg>`,
  dance: `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64"><circle cx="32" cy="14" r="8" fill="#ff7eb6"/><path d="M32 22v18M32 28l-12-6M32 28l14 4M32 40l-10 16M32 40l12 14" stroke="#ff7eb6" stroke-width="5" stroke-linecap="round" fill="none"><animateTransform attributeName="transform" type="rotate" values="-8 32 32;8 32 32;-8 32 32" dur="0.8s" repeatCount="indefinite"/></path></svg>`,
  gg: `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64"><rect x="4" y="12" width="56" height="40" rx="10" fill="#5865f2"/><text x="32" y="42" font-family="sans-serif" font-weight="800" font-size="26" text-anchor="middle" fill="#fff">GG</text></svg>`,
};

// ───────────── 문장 ─────────────

export const LINES = [
  '오늘 저녁에 한 판 하실 분?',
  '저 9시쯤 들어갈 수 있어요',
  'ㅋㅋㅋㅋ 그거 진짜 웃겼음',
  '어제 패치 노트 보셨어요? 밸런스 꽤 바뀌었던데',
  '탱커 너프 먹은 거 체감 확 되더라',
  '저는 이번 시즌 서포터만 할 듯',
  '혹시 레이드 공략 정리해 둔 거 있나요?',
  '포럼에 올려 놨어요! 공략-게시판 확인해 보세요',
  '감사합니다 🙏',
  '오 이거 좋다',
  '내일은 일찍 자야 해서 패스할게요',
  '주말에 정모 어때요?',
  '찬성!! 장소는 강남 쪽이 편할 것 같아요',
  '저번 그 카페 괜찮았는데',
  '사진 올려 주실 분~',
  '방금 전설 장비 떴다 ㄷㄷ',
  '부럽다… 나는 30판째 안 나옴',
  '확률 버그 아니에요? ㅋㅋ',
  '서버 핑이 좀 높은 것 같은데 저만 그런가요',
  '저도 아까 렉 걸렸어요',
  '공유기 재부팅하니까 괜찮아졌어요',
  '이 메신저 시더 설정 다들 켜 두셨죠?',
  '저는 텍스트랑 이모지만 제공 중이에요. 업로드 2MB/s로 제한해 둠',
  'NAS에 헤드리스 시더 띄워 놨어요. 이제 기록 안 날아갈 듯',
  '오 굿굿',
  '휘발성 채널 써 봤는데 30초 지나니까 진짜 사라지네',
  '스크린샷은 못 막는다고 경고 뜨는 거 맞죠?',
  '네, 캡처 제외 창 옵션은 켤 수 있는데 완전 차단은 안 된대요',
  '다음 주 일정 정리해서 공지에 올릴게요',
  '수고하셨습니다!',
  '저녁 뭐 드세요',
  '라면 🍜',
  '역시 국룰',
  '오늘 보스 몇 시 리젠이죠?',
  '22시 40분이요',
  '알람 맞춰 둘게요',
  '혹시 통화 채널에서 화면 공유 되나요?',
  '보기 누르면 그때부터 받아요. 안 누르면 트래픽 0',
  '오 디스코드 Go Live랑 같은 방식이네',
  '이번 업데이트 진짜 기대된다',
  '스포 금지 🤐',
  'ㅇㅋㅇㅋ',
  '잠깐 자리 비울게요',
  '돌아왔습니다',
  '혹시 이 빌드 어때요? 치명타 위주로 가 봤는데',
  '치피 좀 더 올리면 좋을 듯',
  '아 그렇구나 바꿔 볼게요',
  '굿밤 되세요 🌙',
  '좋은 아침입니다 ☀️',
  '커피 수혈 중 ☕',
  '와 이거 실화냐',
  '다들 투표 부탁드려요',
  '저는 2번!',
  '1번이요',
  '동점이네 ㅋㅋ',
  '가위바위보로 정합시다',
  '저번 정모 사진 여기 있어요',
  '이 노래 좋네요 🎵',
  '플레이리스트 공유해 주세요',
  '나중에 링크 보내 드릴게요',
];

export const EMBEDS: EmbedView[] = [
  {
    url: 'https://www.youtube.com/watch?v=dQw4w9WgXcQ',
    title: '보스 레이드 3페이즈 공략 (노데스 클리어)',
    description: '패턴별 위치 선정과 딜 타이밍을 정리했습니다. 타임라인은 설명란 참고.',
    site_name: 'YouTube',
    thumbnail_base64: b64(sceneSvg(7, 320, 180, '▶ 공략 영상')),
    player_url: 'https://www.youtube-nocookie.com/embed/dQw4w9WgXcQ',
  },
  {
    url: 'https://blog.example.com/patch-notes-14-2',
    title: '14.2 패치 노트 요약',
    description: '탱커 방어력 계수 조정, 신규 던전 추가, 매칭 개선 사항을 한눈에 정리했습니다.',
    site_name: 'example 블로그',
    thumbnail_base64: b64(sceneSvg(11, 160, 160, '패치')),
  },
];

export interface MemberSeed {
  key: string;
  name: string;
  bio?: string;
  status?: string;
  online: boolean;
  roles: string[];
  automation?: boolean;
  nickname?: string;
  color?: number | null;
}

export const MEMBER_SEEDS: MemberSeed[] = [
  { key: 'me', name: '세영', bio: '양파 메신저 만드는 중', status: '코딩 중 🧅', online: true, roles: ['admin'], color: 0x9b59b6 },
  { key: 'minji', name: '민지', bio: '서포터 장인', status: '레이드 준비', online: true, roles: ['admin'], color: 0xe91e63 },
  { key: 'doyun', name: '도윤', bio: '탱커', status: '', online: true, roles: ['veteran'] },
  { key: 'hajun', name: '하준', online: true, roles: ['veteran'], status: '🎮' },
  { key: 'seoyeon', name: '서연', online: true, roles: ['newbie'] },
  { key: 'jiho', name: '지호', online: true, roles: [] },
  { key: 'yerin', name: '예린', online: false, roles: ['newbie'] },
  { key: 'siwoo', name: '시우', online: false, roles: [] },
  { key: 'eunwoo', name: '은우', online: false, roles: ['veteran'] },
  { key: 'sua', name: '수아', online: false, roles: [] },
  { key: 'taemin', name: '태민', online: false, roles: [] },
  { key: 'bot', name: '요약봇', bio: '채널 요약을 도와주는 AI(사람 확인 후 관리 작업)', online: true, roles: ['ai'], automation: true },
];

export function crewRoles(ids: Record<string, string>): Role[] {
  return [
    { id: EVERYONE_ROLE, name: '@everyone', color: 0, rank: 0, permissions: 0b111_1011_1111, automation: false },
    {
      id: ids.admin,
      name: '운영진',
      color: 0x1abc9c,
      rank: 30,
      // manage_messages | manage_channels | kick | create_invite | manage_emoji | manage_voice
      permissions: (1 << 16) + (1 << 17) + (1 << 19) + (1 << 20) + (1 << 21) + (1 << 22),
      automation: false,
    },
    { id: ids.veteran, name: '고인물', color: 0xe67e22, rank: 20, permissions: (1 << 20) + (1 << 6), automation: false },
    { id: ids.newbie, name: '뉴비', color: 0x3498db, rank: 10, permissions: 0, automation: false },
    { id: ids.ai, name: 'AI 비서', color: 0x9b59b6, rank: 5, permissions: 0, automation: true },
  ];
}

export function memberView(user: string, seed: MemberSeed, roleIds: Record<string, string>): MemberView {
  return {
    user,
    profile: profile(seed.name, { bio: seed.bio ?? '', status: seed.status ?? '', theme_color: seed.color ?? null }),
    nickname: seed.nickname,
    roles: seed.roles.map((r) => roleIds[r]).filter(Boolean),
    online: seed.online,
    automation: !!seed.automation,
  };
}

export const FORUM_TAGS: ForumTag[] = [
  { id: 1, name: '공략', emoji: '📘' },
  { id: 2, name: '질문', emoji: '❓' },
  { id: 3, name: '버그', emoji: '🐞' },
  { id: 4, name: '잡담', emoji: '💬' },
];

export const FORUM_TITLES: Array<[string, number[], boolean, boolean]> = [
  ['[필독] 공략 게시판 이용 안내', [1], true, true],
  ['보스 레이드 3페이즈 노데스 공략', [1], false, false],
  ['서포터 빌드 질문 있어요 (치유량 vs 쿨감)', [2], false, false],
  ['14.2 패치 이후 매칭 오래 걸리는 버그', [3], false, false],
  ['다들 첫 전설 언제 먹었어요?', [4], false, false],
  ['초보용 골드 파밍 루트 정리', [1, 2], false, false],
];

export interface GeneratedHistory {
  messages: MessageView[];
}

export interface HistoryOptions {
  count: number;
  authors: string[];
  /** 가장 최근 메시지 시각 */
  endAt: number;
  /** 첫 메시지부터 마지막까지의 기간(ms) */
  spanMs: number;
  rng: Rng;
  emojis: EmojiView[];
  mentionable: string[];
  me: string;
  blobs: Map<string, { svg: string; mime: string }>;
  lamportBase: number;
  volatile?: boolean;
  ttlSecs?: number;
  extraLines?: string[];
}

/** 채널 기록을 만듭니다(오래된 것부터). */
export function generateHistory(o: HistoryOptions): MessageView[] {
  const { rng } = o;
  const out: MessageView[] = [];
  // 시각: 끝에서부터 거꾸로 간격을 쌓되, 가끔 긴 공백(밤 시간)을 둡니다.
  const times: number[] = [];
  let t = o.endAt;
  const avg = o.spanMs / Math.max(1, o.count);
  for (let i = 0; i < o.count; i++) {
    times.push(t);
    const burst = rng.chance(0.6);
    t -= burst ? rng.int(5_000, 90_000) : rng.int(Math.round(avg * 0.5), Math.round(avg * 2.6));
  }
  times.reverse();
  const lines = o.extraLines ? [...LINES, ...o.extraLines] : LINES;
  let lastAuthor = o.authors[0];
  for (let i = 0; i < o.count; i++) {
    // 같은 사람이 이어서 말하는 경우를 섞어 묶음 표시를 확인할 수 있게 합니다.
    const author = rng.chance(0.35) ? lastAuthor : rng.pick(o.authors);
    lastAuthor = author;
    let text = rng.pick(lines);
    if (o.emojis.length && rng.chance(0.05)) {
      const e = rng.pick(o.emojis);
      text += ` ${e.animated ? '<a:' : '<:'}${e.name}:${e.id}>`;
    }
    if (o.mentionable.length && rng.chance(0.03)) text = `<@${rng.pick(o.mentionable)}> ${text}`;
    const id = rng.id(32);
    const m: MessageView = {
      id,
      author,
      author_device: `${author.slice(0, 20)}device${author.slice(26)}`,
      created_at: times[i],
      lamport: o.lamportBase + i,
      text,
      attachments: [],
      embeds: [],
      reactions: [],
      deleted: false,
      volatile: !!o.volatile,
    };
    if (out.length > 3 && rng.chance(0.05)) m.reply_to = out[out.length - rng.int(1, 3)].id;
    if (rng.chance(0.09)) m.reactions = reactions(rng, o.emojis);
    if (rng.chance(0.03)) m.edited_at = times[i] + rng.int(10_000, 300_000);
    if (rng.chance(0.03)) m.attachments = [attachment(rng, o.blobs, i)];
    if (rng.chance(0.012)) {
      const e = rng.pick(EMBEDS);
      m.text = `${e.url}`;
      m.embeds = [e];
    }
    if (rng.chance(0.006)) {
      m.deleted = true;
      m.text = '';
      m.attachments = [];
      m.reactions = [];
    }
    if (o.volatile && o.ttlSecs) m.expires_at = times[i] + o.ttlSecs * 1000;
    out.push(m);
  }
  return out;
}

export function reactions(rng: Rng, emojis: EmojiView[]): ReactionView[] {
  const pool = ['👍', '❤️', '😂', '🔥', '🎉', '😮', '🙏', ...emojis.map((e) => `${e.animated ? '<a:' : '<:'}${e.name}:${e.id}>`)];
  const n = rng.int(1, 3);
  const used = new Set<string>();
  const out: ReactionView[] = [];
  for (let k = 0; k < n; k++) {
    const emoji = rng.pick(pool);
    if (used.has(emoji)) continue;
    used.add(emoji);
    out.push({ emoji, count: rng.int(1, 6), me: rng.chance(0.3) });
  }
  return out;
}

let attachmentSeq = 0;

export function attachment(rng: Rng, blobs: Map<string, { svg: string; mime: string }>, seed: number): AttachmentView {
  const blob = rng.id(32);
  const kind = rng.float();
  attachmentSeq++;
  if (kind < 0.75) {
    const w = rng.pick([1280, 1600, 1080]);
    const h = rng.pick([720, 900, 1350]);
    const label = rng.pick(['정모 사진', '스크린샷', '보스 처치!', '오늘의 하늘', '전설 획득']);
    const svg = sceneSvg(seed + attachmentSeq, w, h, label);
    blobs.set(blob, { svg, mime: 'image/png' });
    return {
      blob,
      name: `IMG_${2000 + attachmentSeq}.png`,
      mime: 'image/png',
      size: rng.int(180_000, 3_800_000),
      width: w,
      height: h,
      thumbnail_base64: b64(sceneSvg(seed + attachmentSeq, Math.round(w / 8), Math.round(h / 8), '')),
      downloaded: rng.chance(0.6),
    };
  }
  return {
    blob,
    name: rng.pick(['레이드_일정.xlsx', '공략_정리.pdf', 'build-notes.txt', '정모_회비.csv']),
    mime: 'application/octet-stream',
    size: rng.int(12_000, 2_400_000),
    downloaded: false,
  };
}

export function forumPosts(rng: Rng, authors: string[], now: number): ForumPostView[] {
  return FORUM_TITLES.map(([title, tags, pinned, locked], i) => {
    const created = now - (FORUM_TITLES.length - i) * 86_400_000 * rng.int(1, 3);
    return {
      id: rng.id(32),
      title,
      tags,
      author: rng.pick(authors),
      created_at: created,
      last_activity: created + rng.int(60_000, 3 * 86_400_000),
      reply_count: pinned ? 0 : rng.int(1, 24),
      pinned,
      locked,
      // 미리보기·첨부 수는 MOCK이 내보낼 때 게시글 메시지에서 채웁니다.
      preview: '',
      attachment_count: 0,
    };
  });
}

/** 포럼 시드 게시글 본문(마크다운). 키는 FORUM_TITLES 순서. */
export const FORUM_BODIES: Record<number, string> = {
  0: [
    '# 공략 게시판 이용 안내',
    '공략·질문·버그 제보를 나누는 곳입니다. **글을 쓰기 전에 꼭 읽어 주세요.**',
    '## 태그',
    '- **📘 공략**: 직접 해 보고 검증한 공략과 팁',
    '- **❓ 질문**: 막힌 곳을 물어보세요',
    '- **🐞 버그**: 재현 방법과 패치 버전을 같이 적어 주세요',
    '## 지켜 주세요',
    '1. 같은 질문이 있는지 먼저 찾아보기',
    '2. 신작 스토리는 ||이렇게|| 스포일러로 가리기',
    '3. 코드는 `한 줄 코드`나 코드 블록으로',
    '> 운영 문의는 #운영 채널로 해 주세요.',
    '-# 이 글은 대문으로 지정되어 포럼에 들어오면 맨 위에 보입니다.',
  ].join('\n'),
  1: [
    '3페이즈에서 다들 많이 눕길래 정리해 봤습니다.',
    '### 핵심',
    '- 장판은 *시계 방향*으로 피하기',
    '- 광역기 직전에 __무적기__ 아껴 두기',
    '- ~~힐러 탓하지 말기~~ 서로 탓하지 말기',
    '자세한 영상은 [공략 영상](https://www.example.com/raid-guide)에 있어요.',
  ].join('\n'),
  5: '골드 파밍 루트입니다. **하루 30분**이면 충분해요.\n1. 북쪽 광산\n2. 강가 낚시터\n3. 마을 의뢰판',
};
