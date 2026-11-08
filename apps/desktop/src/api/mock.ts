// 데몬 없이 UI를 개발·스크린숏하기 위한 메모리 안 MOCK 백엔드.
// API 계약(docs/api.md)대로 응답하고, 알림은 데몬처럼 프레임(약 16ms) 단위로 묶어 보냅니다.
// 실제 암호화·네트워크는 없고, 휘발성 내용도 이 객체 메모리에만 둡니다(어떤 저장소에도 쓰지 않음).
// 익명 채널은 데몬처럼 내보낼 때만 가립니다: 안에는 실제 작성자와 '익명으로 썼음'(anonymous)을 두고, 보기·알림을 만들 때
// 익명으로 썼거나 채널이 지금 익명이면 작성자를 ANONYMOUS_USER로 바꿉니다.

import { RpcError, type DaemonState, type PickedFile, type Transport } from './client';
import {
  EMOJI_SVGS,
  FORUM_TAGS,
  MEMBER_SEEDS,
  Rng,
  attachment,
  b64,
  channel,
  channelView,
  crewRoles,
  FORUM_BODIES,
  forumPosts,
  generateHistory,
  memberView,
  profile,
  sceneSvg,
} from './mockData';
import {
  ANONYMOUS_DEVICE,
  ANONYMOUS_USER,
  DATA_TYPES,
  EVERYONE_ROLE,
  ErrorCode,
  FORUM_PREVIEW_CHARS,
  type ArchiveView,
  type AttachmentInput,
  type AttachmentView,
  type AuditEntry,
  type BridgeKind,
  type BanView,
  type BlobCacheView,
  type BlobPinView,
  type CommitterCandidate,
  type ChannelCoverage,
  type ChannelKind,
  type ChannelSettings,
  type ChannelView,
  type ConnectionMode,
  type CrewServers,
  type DeviceEntry,
  type EmojiView,
  type ForumPostView,
  type ForumTag,
  type FriendLinkView,
  type FriendRequestView,
  type FriendView,
  type GapView,
  type IdentityView,
  type InviteView,
  type MemberView,
  type MessageView,
  type NetStatus,
  type Notification,
  type PeerPolicy,
  type PendingConfirmation,
  type PermissionOverwrite,
  type Profile,
  type RateLimitView,
  type RateScope,
  type ReportView,
  type Role,
  type SeederPolicy,
  type SpaceKind,
  type SpaceView,
  type SupporterProvider,
  type SupporterStatus,
  type FlairView,
  type PresenceView,
  type BadgeView,
  type ShownBadge,
  type TokenView,
  type TransferRates,
  type UploadRate,
  type VoiceParticipant,
  type DeviceLinkRequestView,
} from './types';
import { appLinkKind, WEB_BASE } from '../lib/links';
import { permissionNames, rankOf } from '../lib/misc';
import { scopeKey } from '../lib/ratelimit';
import { searchTerms } from '../lib/search';
import { overwriteDelta, sameTarget } from '../lib/perms';

interface MockSpace {
  view: SpaceView;
  messages: Map<string, MessageView[]>;
  gaps: Map<string, GapView[]>;
  hidden: Map<string, MessageView[]>;
  threads: Map<string, MessageView[]>;
  posts: Map<string, ForumPostView[]>;
  invites: InviteView[];
  voice: Map<string, VoiceParticipant[]>;
  /** 익명 통화의 참가자 번호(채널 → 기기 → 번호, 들어온 순서). 통화가 비면 처음부터 다시 셉니다. */
  voiceAnon: Map<string, Map<string, number>>;
  coverage: Map<string, number>;
  /** 지정된 보관 전용 노드. */
  archives: ArchiveView[];
  /** 크루 STUN 서버(`crew.set_servers`). 보기에는 관리자에게만 실립니다. */
  servers: CrewServers;
  /** 이 기기가 이 크루의 서버를 쓰기로 했는지(`crew.use_servers`). */
  useServers: boolean;
  /** 친구 공간이면 상대 */
  peer?: string;
  unread: number;
  mention: number;
}

export interface MockOptions {
  /** 처음부터 신원 없이 시작(온보딩 화면 확인). */
  noIdentity?: boolean;
  /** 무작위 활동(새 메시지·접속 변화)을 끕니다(스크린숏용). */
  quiet?: boolean;
  seed?: number;
  now?: number;
}

const DAY = 86_400_000;

export class MockBackend {
  readonly rng: Rng;
  readonly now0: number;
  identity: IdentityView | null = null;
  users = new Map<string, Profile>();
  userIds: Record<string, string> = {};
  devices = new Map<string, string>();
  online = new Map<string, boolean>();
  friends: FriendView[] = [];
  requests: FriendRequestView[] = [];
  friendLinks: FriendLinkView[] = [];
  spaces = new Map<string, MockSpace>();
  notes = new Map<string, string>();
  safety = new Map<string, { digits: string; verified: boolean }>();
  blobs = new Map<string, { svg: string; mime: string }>();
  /** 붙여넣기 ID → 받은 바이트 수(blob.paste). */
  pastes = new Map<string, number>();
  pngCache = new Map<string, Uint8Array>();
  seed: SeederPolicy;
  peer: PeerPolicy;
  net: NetStatus;
  tokens: TokenView[] = [];
  audit: AuditEntry[] = [];
  confirmations: PendingConfirmation[] = [];
  myDevices: DeviceEntry[] = [];
  private listeners = new Set<(batch: Notification[]) => void>();
  private pending: Notification[] = [];
  private flushTimer: ReturnType<typeof setTimeout> | null = null;
  private timers: Array<ReturnType<typeof setInterval>> = [];
  /** STUN 서버 목록(`net.stun_servers`). null이면 기본 목록. */
  stun: string[] | null = null;
  /** Tor 파일 회로 수(`net.file_circuits`, 0~4), 기본 0. */
  fileCircuits = 0;
  /** Tor 브리지(`net.bridges`), 기본 꺼짐. */
  bridges: { enabled: boolean; kind: BridgeKind; custom: string[] } = { enabled: false, kind: 'snowflake', custom: [] };
  /** 채널 묶음 토픽(`net.channel_topics`), 기본 꺼짐. */
  channelTopics = false;
  /** 내 연락처 링크로 만난 임시 연락처 → 그 링크(폐기하면 만료). */
  contactLinks = new Map<string, string>();
  bans = new Map<string, BanView[]>();
  /** 차단한 사람 → 차단 시각 */
  blocked = new Map<string, number>();
  /** 차단한 남의 기기 → [사용자, 차단 시각]. */
  blockedDevices = new Map<string, [string, number]>();
  private linkTimer?: ReturnType<typeof setTimeout>;
  private linkRequest: DeviceLinkRequestView | null = null;
  /** 후원 확인: 개발 화면에서 로그인 흐름을 보도록 두 경로를 다 열어 두고, 기능은 막지 않습니다. */
  supporter: SupporterStatus = { active: true, providers: ['github', 'patreon'], links: [], shared: [] };
  /** 내 표시 상태. 모의 데몬은 오프라인으로 다시 뜨지 않고 값만 바꿉니다. */
  presence: PresenceView = { mode: 'online', idle: false, hide_last_seen: false, offline: false };
  /** 사람마다 뱃지. 개발 화면에서 고르기를 보도록 내게 몇 개 둡니다. */
  badges = new Map<string, BadgeView>();
  /** 후원자 표시(카드 배너·후광). 나와 민지는 후광까지, 서연은 배너만. */
  flair = new Map<string, FlairView>();
  reports: ReportView[] = [];
  /** 신고하며 숨긴 메시지(이 기기에서만, 목록에서 뺌) */
  reportHidden = new Set<string>();
  blobProgress = new Map<string, { have: number; total: number }>();
  /** `blob.evict`로 비운 블롭(다시 받을 때까지 `blob.status`가 0조각으로 답함). */
  evicted = new Set<string>();
  /** 저장 공간(X4): 받은 첨부 정리 한도와 보관한 첨부. */
  cache: BlobCacheView = {
    cache_bytes: 734 * 1024 * 1024,
    cache_count: 213,
    pinned_bytes: 0,
    pinned_count: 0,
    kept_bytes: 96 * 1024 * 1024,
    kept_count: 41,
    limit_bytes: 2 * 1024 * 1024 * 1024,
    min_free_bytes: 1024 * 1024 * 1024,
    default_limit_bytes: 2 * 1024 * 1024 * 1024,
    default_min_free_bytes: 1024 * 1024 * 1024,
    disk_free_bytes: 118 * 1024 * 1024 * 1024,
    evicted_count: 0,
    evicted_bytes: 0,
  };
  pins: BlobPinView[] = [];
  /** 신뢰 스위치를 켠 사람(이 기기). 크루 신뢰는 그 공간 보기의 `trusted`. */
  trustedUsers = new Set<string>();
  /** 이 기기를 신뢰한다고 알려 온 상대(흉내). 서로 신뢰하면 P2P, 한쪽만이면 단방향으로 봅니다. */
  trustsMe = new Set<string>();
  /** 크루·채널·사람별 속도 제한(`rate.limits`, 범위 키 → 줄). */
  rateLimits = new Map<string, RateLimitView>();
  /** 내가 나눠 주는 첨부 흉내(잡담 채널의 정모 사진): 이것을 받는 사람이 `transfer.rates`의 uploads에 나옵니다. */
  uploadTarget: { space: string; channel: string; blob: string } | null = null;
  private auditSeq = 1;
  private lamport = 100_000;
  private joinedVolatile = new Set<string>();

  constructor(opts: MockOptions = {}) {
    this.rng = new Rng(opts.seed ?? 20261019);
    this.now0 = opts.now ?? Date.now();
    for (const s of MEMBER_SEEDS) {
      const id = this.rng.id(32);
      this.userIds[s.key] = id;
      this.users.set(id, profile(s.name, { bio: s.bio ?? '', status: s.status ?? '', theme_color: s.color ?? null }));
      this.devices.set(id, this.rng.id(32));
      this.online.set(id, s.online);
    }
    this.seed = {
      enabled: true,
      types: ['text', 'emoji', 'image'],
      include_channels: null,
      all_channels: false,
      exclude_channels: [],
      excluded_events: [],
      upload_limit_bps: 10 * 1024 * 1024,
      max_concurrent: 4,
      prefetch: false,
    };
    this.peer = {
      types: [...DATA_TYPES],
      priorities: { text: 255, emoji: 200, image: 150, video: 80, file: 50, history_bundle: 20 },
      download_limit_bps: 10 * 1024 * 1024,
      auto_download_max_bytes: 20 * 1024 * 1024,
    };
    this.net = {
      peer_id: '12D3KooW' + this.rng.id(32).slice(0, 44),
      mode: 'tor',
      listen_addrs: [],
      relay_addrs: [],
      nat: 'disabled',
      connections: 9,
      bootstrap_progress: 100,
      external_addrs: [],
      // 난수를 쓰지 않습니다(뒤에 만드는 세계의 난수 순서가 바뀌지 않게).
      onion_addr: `/onion3/${'yangpamockonion'.repeat(4).slice(0, 56)}:4001`,
      peers: 9,
    };
    if (!opts.noIdentity) this.createIdentity('세영', this.userIds.me);
    this.buildWorld();
    this.decorate();
    // 앞선 세계의 난수 순서가 바뀌지 않게 연락처와 P2P 전용 채널은 맨 나중에 만듭니다.
    this.addContacts();
    this.addP2pChannel();
    this.addAnonChannels();
    // 민지는 이미 나를 신뢰합니다(내가 켜면 P2P, 켜기 전에는 내게만 민지의 IP가 보이는 단방향).
    this.trustsMe.add(this.userIds.minji);
    // 범위별 속도 제한 하나(설정 목록이 비지 않게): 하준에게서 받기 512KiB/s.
    this.setRateLimit({ kind: 'user', user: this.userIds.hajun }, null, 512 * 1024);
    if (!opts.quiet) this.startActivity();
    this.timers.push(
      setInterval(() => {
        this.expireVolatile();
        this.expireContacts();
      }, 1000),
    );
  }

  dispose() {
    for (const t of this.timers) clearInterval(t);
    this.timers = [];
    if (this.flushTimer) clearTimeout(this.flushTimer);
  }

  get me(): string {
    return this.identity?.user ?? this.userIds.me;
  }

  get myDevice(): string {
    return this.identity?.device ?? this.devices.get(this.userIds.me) ?? '';
  }

  // ───────────── 알림 ─────────────

  subscribe(fn: (batch: Notification[]) => void) {
    this.listeners.add(fn);
    return () => this.listeners.delete(fn);
  }

  emit(n: Notification) {
    this.pending.push(n);
    if (!this.flushTimer) {
      this.flushTimer = setTimeout(() => {
        this.flushTimer = null;
        const batch = this.pending;
        this.pending = [];
        for (const l of this.listeners) l(batch);
      }, 16);
    }
  }

  // ───────────── 세계 만들기 ─────────────

  private createIdentity(name: string, user = this.rng.id(32)) {
    const device = this.devices.get(user) ?? this.rng.id(32);
    this.devices.set(user, device);
    this.myDevices = [
      { device, box_key: 'B'.repeat(1620), name: '이 PC (데스크톱)', added_at: this.now0 - 40 * DAY, revoked_at: null },
      { device: this.rng.id(32), box_key: 'D'.repeat(1620), name: '노트북', added_at: this.now0 - 12 * DAY, revoked_at: null },
      { device: this.rng.id(32), box_key: 'F'.repeat(1620), name: 'NAS 시더(헤드리스)', added_at: this.now0 - 5 * DAY, revoked_at: null },
    ];
    const p = this.users.get(user) ?? profile(name);
    p.name = name;
    this.users.set(user, p);
    this.online.set(user, true);
    this.identity = { user, device, profile: p, devices: this.myDevices, first_syncing: false };
  }

  /** 진짜 QR이 아닌 무늬(모의 데몬은 QR 부호기를 갖지 않음). */
  private fakeQr() {
    const size = 29;
    return { size, modules: Array.from({ length: size * size }, () => (this.rng.float() < 0.5 ? '1' : '0')).join('') };
  }

  /** 새 기기가 띄운 링크로 쓰던 기기가 곧 허용했다고 칩니다. */
  private acceptLink() {
    this.createIdentity('새 기기', this.userIds.me);
    this.identity!.first_syncing = true;
    this.emit({ type: 'device.link_result', status: 'approved' });
    setTimeout(() => {
      if (!this.identity) return;
      this.identity.first_syncing = false;
      this.identity.profile = { ...this.identity.profile, name: '양파' };
      this.emit({ type: 'profile.updated', user: this.identity.user });
      this.emit({ type: 'device.synced' });
    }, 2000);
  }

  private buildWorld() {
    const u = this.userIds;
    const now = this.now0;
    const roleIds = { admin: this.rng.id(16), veteran: this.rng.id(16), newbie: this.rng.id(16), ai: this.rng.id(16) };
    const emojis: EmojiView[] = [
      ['yangpa', false],
      ['ddabong', false],
      ['dance', true],
      ['gg', false],
    ].map(([name, animated], i) => {
      const blob = this.rng.id(32);
      const svg = EMOJI_SVGS[name as string];
      this.blobs.set(blob, { svg, mime: 'image/svg+xml' });
      return {
        id: String(BigInt('0x' + this.rng.digits(15)) % 18446744073709551615n),
        name: name as string,
        animated: animated as boolean,
        blob,
        mime: 'image/svg+xml',
        created_by: [this.me, u.minji, u.doyun, u.minji][i],
      };
    });

    // 친구 두 명과 1대1 공간
    for (const key of ['minji', 'doyun'] as const) {
      const user = u[key];
      const spaceId = this.rng.id(16);
      this.friends.push({
        user,
        profile: this.users.get(user)!,
        space: spaceId,
        online: this.online.get(user) ?? false,
        safety_number_verified: key === 'minji',
        kind: 'friend',
        proposed: false,
      });
      this.safety.set(user, { digits: this.rng.digits(60), verified: key === 'minji' });
      const chans: Array<[string, ChannelSettings, Partial<ChannelView>?]> = [[this.rng.id(16), channel('일반', 'text', { position: 0 })]];
      if (key === 'minji') {
        chans.push([this.rng.id(16), channel('여행-계획', 'text', { position: 1, topic: '부산 여행 준비' })]);
        chans.push([
          this.rng.id(16),
          channel('비밀-이야기', 'text', {
            position: 2,
            security: 'volatile',
            ttl: { mode: 'seconds', seconds: 300 },
            link_previews: false,
            topic: '5분 뒤 사라지는 대화',
          }),
        ]);
        chans.push([this.rng.id(16), channel('통화', 'voice', { position: 3 })]);
      }
      const members = [this.memberOf('me', roleIds, []), this.memberOf(key, roleIds, [])];
      const sp = this.makeSpace(spaceId, 'space', this.users.get(user)!.name, u.me, members, [
        { id: EVERYONE_ROLE, name: '@everyone', color: 0, rank: 0, permissions: 1983, automation: false },
      ], chans, emojis.slice(0, 2));
      sp.peer = user;
      sp.view.my_permissions = ['administrator'];
      for (const [cid, s] of chans) {
        if (s.kind !== 'text' || s.security === 'volatile') continue;
        const count = key === 'minji' ? (s.name === '일반' ? 60 : 14) : 12;
        sp.messages.set(
          cid,
          generateHistory({
            count,
            authors: [u.me, user],
            endAt: now - (key === 'minji' ? 3 * 60_000 : 2 * DAY),
            spanMs: 9 * DAY,
            rng: this.rng,
            emojis: emojis.slice(0, 2),
            mentionable: [],
            me: u.me,
            blobs: this.blobs,
            lamportBase: this.nextLamport(count),
          }),
        );
      }
      if (key === 'minji') sp.unread = 2;
    }

    // 크루: 게임 모임
    {
      const spaceId = this.rng.id(16);
      const cat = (name: string, position: number) => [this.rng.id(16), channel(name, 'category', { position })] as const;
      const [catInfo, catInfoS] = cat('정보', 0);
      const [catTalk, catTalkS] = cat('대화', 1);
      const [catForum, catForumS] = cat('포럼', 2);
      const [catVoice, catVoiceS] = cat('통화', 3);
      const ids = {
        notice: this.rng.id(16),
        rules: this.rng.id(16),
        chat: this.rng.id(16),
        shots: this.rng.id(16),
        secret: this.rng.id(16),
        forum: this.rng.id(16),
        voice: this.rng.id(16),
        voice2: this.rng.id(16),
      };
      const chans: Array<[string, ChannelSettings, Partial<ChannelView>?]> = [
        [catInfo, catInfoS],
        [catTalk, catTalkS],
        [catForum, catForumS],
        [catVoice, catVoiceS],
        [ids.notice, channel('공지', 'text', { parent: catInfo, position: 0, topic: '운영진 공지 · 정모 일정', history: 'public' })],
        [ids.rules, channel('규칙', 'text', { parent: catInfo, position: 1, history: 'public' })],
        [ids.chat, channel('잡담', 'text', { parent: catTalk, position: 0, topic: '아무 이야기나 편하게 🧅', history: 'public' }), { unread: 12 }],
        [ids.shots, channel('스크린샷', 'text', { parent: catTalk, position: 1, topic: '게임 스샷·사진' })],
        [
          ids.secret,
          channel('비밀-작전', 'text', {
            parent: catTalk,
            position: 2,
            security: 'volatile',
            join_mode: 'join_leave',
            ttl: { mode: 'seconds', seconds: 60 },
            history: 'public',
            link_previews: false,
            topic: '들어와 있는 동안만 대화 · 60초 뒤 사라짐',
          }),
          { joined: false, participants: [u.minji, u.doyun, u.hajun] },
        ],
        [ids.forum, channel('공략-게시판', 'forum', { parent: catForum, position: 0, forum_tags: FORUM_TAGS, topic: '공략·질문·버그 제보' })],
        // 앱은 늘 Tor로 돌아 통화는 참가한 P2P 전용 채널에서만 됩니다. 통화 화면을 볼 수 있게 이 채널은 참가한 P2P 전용으로 둡니다.
        [
          ids.voice,
          channel('일반 음성', 'voice', { parent: catVoice, position: 0 }),
          { participants: [u.minji, u.doyun, u.hajun], p2p: true, p2p_joined: true },
        ],
        [ids.voice2, channel('게임 음성', 'voice', { parent: catVoice, position: 1 }), { participants: [u.seoyeon, u.jiho] }],
      ];
      const members = MEMBER_SEEDS.map((s) => this.memberOf(s.key, roleIds, s.roles));
      const sp = this.makeSpace(spaceId, 'crew', '게임 모임', u.me, members, crewRoles(roleIds), chans, emojis);
      sp.view.settings.description = '주말 레이드 · 정모 · 잡담하는 크루';
      sp.view.my_permissions = ['administrator'];
      sp.servers = { stun: ['stun.game-crew.example:3478'] };
      const authors = MEMBER_SEEDS.filter((s) => !s.automation).map((s) => u[s.key]);
      const chat = generateHistory({
        count: 2000,
        authors,
        endAt: now - 4 * 60_000,
        spanMs: 12 * DAY,
        rng: this.rng,
        emojis,
        mentionable: authors,
        me: u.me,
        blobs: this.blobs,
        lamportBase: this.nextLamport(2000),
      });
      // 받지 않은 기록 구간 하나(시더가 제공하지 않은 30개)
      const hidden = chat.splice(700, 30);
      sp.hidden.set(ids.chat, hidden);
      sp.gaps.set(ids.chat, [{ after: chat[699].id, before: chat[700].id }]);
      // 최근 메시지 몇 개는 보기 좋게 고정
      this.tailor(chat, emojis);
      const photo = chat[chat.length - 4]?.attachments[0];
      if (photo) this.uploadTarget = { space: sp.view.space, channel: ids.chat, blob: photo.blob };
      sp.messages.set(ids.chat, chat);
      sp.messages.set(
        ids.notice,
        generateHistory({
          count: 8,
          authors: [u.me, u.minji],
          endAt: now - 2 * DAY,
          spanMs: 20 * DAY,
          rng: this.rng,
          emojis,
          mentionable: [],
          me: u.me,
          blobs: this.blobs,
          lamportBase: this.nextLamport(8),
          extraLines: ['📢 이번 주 토요일 21시 정기 레이드 있습니다. 통화 채널로 모여 주세요!'],
        }),
      );
      sp.messages.set(ids.rules, []);
      const shots = generateHistory({
        count: 18,
        authors,
        endAt: now - 6 * 3600_000,
        spanMs: 6 * DAY,
        rng: this.rng,
        emojis,
        mentionable: [],
        me: u.me,
        blobs: this.blobs,
        lamportBase: this.nextLamport(18),
      });
      shots.forEach((m, i) => {
        if (!m.deleted && i % 2 === 0) m.attachments = [attachment(this.rng, this.blobs, 300 + i)];
      });
      sp.messages.set(ids.shots, shots);
      // 휘발성 채널: 지금 참가자들의 RAM에 남아 있는 메시지(입장 시 받음)
      sp.messages.set(ids.secret, []);
      sp.posts.set(ids.forum, forumPosts(this.rng, authors, now));
      for (const post of sp.posts.get(ids.forum)!) {
        sp.threads.set(
          post.id,
          generateHistory({
            count: Math.max(1, Math.min(post.reply_count + 1, 12)),
            authors,
            endAt: post.last_activity,
            spanMs: Math.max(60_000, post.last_activity - post.created_at),
            rng: this.rng,
            emojis,
            mentionable: [],
            me: u.me,
            blobs: this.blobs,
            lamportBase: this.nextLamport(12),
            // 데몬처럼 첫 메시지는 게시글 자체(ID가 게시글 ID, 글타래 없음)이고 그 뒤가 댓글입니다.
          }).map((m, i) => (i === 0 ? { ...m, id: post.id, author: post.author, created_at: post.created_at, thread: undefined } : { ...m, thread: post.id })),
        );
      }
      this.decorateForum(sp, ids.forum);
      sp.voice.set(ids.voice, [
        this.vp('minji', { screen: true }),
        this.vp('doyun', { muted: true }),
        this.vp('hajun', {}),
      ]);
      sp.voice.set(ids.voice2, [this.vp('seoyeon', {}), this.vp('jiho', {})]);
      for (const [cid, s] of chans) {
        if (s.kind === 'category') continue;
        sp.coverage.set(cid, s.security === 'volatile' || s.kind === 'voice' ? -1 : cid === ids.shots ? 0 : this.rng.int(1, 4));
      }
      sp.invites = [
        {
          token_hash: Array.from({ length: 64 }, () => '0123456789abcdef'[this.rng.int(0, 15)]).join(''),
          expires_at: now + 3 * DAY,
          max_uses: 10,
          uses: 3,
          roles: [roleIds.newbie],
          created_by: u.me,
        },
        {
          token_hash: Array.from({ length: 64 }, () => '0123456789abcdef'[this.rng.int(0, 15)]).join(''),
          uses: 12,
          roles: [],
          created_by: u.minji,
        },
      ];
      sp.unread = 12;
      sp.mention = 1;
    }

    // 크루: 러스트 스터디(레일에 여러 크루가 보이게)
    {
      const spaceId = this.rng.id(16);
      const general = this.rng.id(16);
      const members = ['me', 'jiho', 'seoyeon'].map((k) => this.memberOf(k, {}, []));
      const sp = this.makeSpace(spaceId, 'crew', '러스트 스터디', u.jiho, members, [
        { id: EVERYONE_ROLE, name: '@everyone', color: 0, rank: 0, permissions: 1983, automation: false },
      ], [[general, channel('일반', 'text', { topic: '매주 수요일 21시' })]], []);
      sp.view.my_permissions = ['view_channel', 'send_messages', 'read_history', 'add_reactions', 'attach_files', 'use_crew_stun'];
      sp.servers = { stun: ['stun.rust-study.example:3478'] };
      sp.messages.set(
        general,
        generateHistory({
          count: 30,
          authors: [u.jiho, u.seoyeon, u.me],
          endAt: now - DAY,
          spanMs: 14 * DAY,
          rng: this.rng,
          emojis: [],
          mentionable: [],
          me: u.me,
          blobs: this.blobs,
          lamportBase: this.nextLamport(30),
          extraLines: ['소유권 개념 다시 보니까 이해가 되네요', 'async 트레잇 이제 안정화됐죠?', 'clippy 경고 0개 달성!'],
        }),
      );
    }

    this.requests = [
      {
        request_id: 'req-' + this.rng.id(16),
        user: this.rng.id(32),
        profile: profile('하은', { bio: '민지 친구예요' }),
        received_at: now - 3 * 3600_000,
      },
    ];
    this.friendLinks = [
      { token_id: 'tok-' + this.rng.id(16).slice(0, 8), created_at: now - 2 * DAY, expires_at: now + DAY, single_use: true, uses: 0 },
    ];
    this.tokens = [
      {
        id: 'tk-' + this.rng.id(16).slice(0, 10),
        name: '요약봇(MCP)',
        scopes: ['read', 'write'],
        spaces: [[...this.spaces.values()].find((s) => s.view.settings.name === '게임 모임')!.view.space],
        automation: true,
        created_at: now - 7 * DAY,
        last_used_at: now - 20 * 60_000,
      },
      { id: 'tk-' + this.rng.id(16).slice(0, 10), name: 'CLI', scopes: ['owner'], spaces: [], automation: false, created_at: now - 30 * DAY },
    ];
    const crew = [...this.spaces.values()].find((s) => s.view.settings.name === '게임 모임')!;
    this.confirmations = [
      {
        id: 'cf-' + this.rng.id(16).slice(0, 10),
        method: 'member.kick',
        params: { space: crew.view.space, user: u.taemin, reason: '장기 미접속' },
        token_name: '요약봇(MCP)',
        space: crew.view.space,
        requested_at: now - 15 * 60_000,
        expires_at: now + 23 * 3600_000,
        summary: '「게임 모임」에서 태민 추방(사유: 장기 미접속)',
      },
    ];
    const methods = ['message.list', 'space.get', 'message.send', 'member.kick', 'seed.coverage', 'message.list', 'channel.list'];
    for (let i = 0; i < 40; i++) {
      const method = this.rng.pick(methods);
      const auto = this.rng.chance(0.4);
      const kick = method === 'member.kick';
      this.audit.push({
        id: this.auditSeq++,
        at: now - (40 - i) * 7 * 60_000,
        method,
        token_name: auto ? '요약봇(MCP)' : '데스크톱 앱',
        ok: !kick,
        code: kick ? 1006 : undefined,
        automation: auto,
      });
    }
  }

  /** 그림 블롭 하나(브라우저에서는 PNG로 바꿔 줌). */
  mockImage(seed: number, w: number, h: number, label = ''): string {
    const blob = this.rng.id(32);
    this.blobs.set(blob, { svg: sceneSvg(seed, w, h, label), mime: 'image/png' });
    return blob;
  }

  /** 프로필 이미지·크루 아이콘·모더레이션 예시(차단·추방 금지·신고)를 붙입니다. */
  private decorate() {
    const u = this.userIds;
    const image = (seed: number, w: number, h: number, label: string) => this.mockImage(seed, w, h, label);
    for (const [key, seed] of [
      ['minji', 3],
      ['seoyeon', 9],
      ['hajun', 14],
    ] as const) {
      const prof = this.users.get(u[key]);
      if (prof) prof.avatar = image(seed, 128, 128, '');
    }
    const minji = this.users.get(u.minji);
    if (minji) minji.banner = image(21, 600, 200, '');
    const seoyeon = this.users.get(u.seoyeon);
    if (seoyeon) seoyeon.banner = image(27, 600, 200, '');
    const until = Date.now() + 30 * 86_400_000;
    this.flair.set(u.me, { user: u.me, banner: true, halo_allowed: true, halo: [0xe8541e, 0xffc24b], until });
    this.flair.set(u.minji, { user: u.minji, banner: true, halo_allowed: true, halo: [0x7c4dff, 0x00e5ff], until });
    this.flair.set(u.seoyeon, { user: u.seoyeon, banner: true, halo_allowed: false, until });
    const crew = this.crew();
    if (!crew) return;
    crew.view.settings.icon = image(5, 128, 128, '🧅');
    const troll = this.rng.id(32);
    this.users.set(troll, profile('도배꾼', { bio: '' }));
    this.bans.set(crew.view.space, [
      {
        user: troll,
        profile: this.users.get(troll) ?? null,
        reason: '광고 도배',
        banned_by: u.me,
        banned_at: this.now0 - 2 * DAY,
      },
    ]);
    this.blocked.set(u.jiho, this.now0 - 3 * DAY);
    const chat = crew.view.channels.find((c) => c.settings.name === '잡담');
    const list = chat ? crew.messages.get(chat.channel) ?? [] : [];
    const targets = list.filter((m) => !m.deleted && m.author !== u.me && m.text).slice(-40, -30);
    for (const [i, m] of targets.slice(0, 2).entries()) {
      this.reports.push({
        report_id: 'rp-' + this.rng.id(16).slice(0, 12),
        space: crew.view.space,
        channel: chat!.channel,
        message: m.id,
        reporter: i === 0 ? u.minji : u.seoyeon,
        reason: i === 0 ? '욕설이 있어요' : '스팸 링크 같아요',
        reported_at: this.now0 - (i + 1) * 3600_000,
        status: 'open',
        resolution: null,
      });
    }
    if (targets[2]) {
      this.reports.push({
        report_id: 'rp-' + this.rng.id(16).slice(0, 12),
        space: crew.view.space,
        channel: chat!.channel,
        message: targets[2].id,
        reporter: u.doyun,
        reason: '도배',
        reported_at: this.now0 - 2 * DAY,
        status: 'resolved',
        resolution: '기각 · 대화 맥락상 문제없음',
      });
    }
    this.markMembersBlocked();
  }

  /** 잡담 채널 끝부분을 스크린숏에서 기능이 잘 보이게 다듬습니다. */
  private tailor(chat: MessageView[], emojis: EmojiView[]) {
    const u = this.userIds;
    const n = chat.length;
    const base = chat[n - 1].created_at;
    const set = (i: number, patch: Partial<MessageView>) => {
      chat[n - i] = { ...chat[n - i], ...patch };
    };
    const yangpa = emojis[0];
    const dance = emojis[2];
    set(8, { author: u.doyun, text: '이번 주 토요일 레이드 몇 시였죠?', created_at: base - 9 * 60_000, reactions: [], attachments: [], embeds: [], deleted: false, reply_to: undefined });
    set(7, { author: u.minji, text: '21시요! 공지 채널에 올려 뒀어요', created_at: base - 8 * 60_000, reactions: [{ emoji: '👍', count: 3, me: true }], attachments: [], embeds: [], deleted: false, reply_to: undefined });
    set(6, {
      author: u.minji,
      text: '공략 영상도 같이 봐 두세요 https://www.youtube.com/watch?v=dQw4w9WgXcQ',
      created_at: base - 8 * 60_000 + 20_000,
      embeds: [
        {
          url: 'https://www.youtube.com/watch?v=dQw4w9WgXcQ',
          title: '보스 레이드 3페이즈 공략 (노데스 클리어)',
          description: '패턴별 위치 선정과 딜 타이밍을 정리했습니다.',
          site_name: 'YouTube',
          thumbnail_base64: b64(sceneSvg(7, 320, 180, '▶ 공략 영상')),
          player_url: 'https://www.youtube-nocookie.com/embed/dQw4w9WgXcQ',
        },
      ],
      reactions: [{ emoji: `<:${yangpa.name}:${yangpa.id}>`, count: 4, me: false }],
      attachments: [],
      deleted: false,
      reply_to: undefined,
    });
    set(5, {
      author: u.hajun,
      text: `오 감사합니다 <a:${dance.name}:${dance.id}>`,
      created_at: base - 6 * 60_000,
      reply_to: chat[n - 6].id,
      reactions: [],
      attachments: [],
      embeds: [],
      deleted: false,
    });
    set(4, {
      author: u.seoyeon,
      text: '어제 정모 사진 올려요 📸',
      created_at: base - 5 * 60_000,
      attachments: [attachment(this.rng, this.blobs, 77)],
      reactions: [
        { emoji: '❤️', count: 5, me: true },
        { emoji: '🔥', count: 2, me: false },
      ],
      embeds: [],
      deleted: false,
      reply_to: undefined,
    });
    set(3, { author: u.doyun, text: '', deleted: true, created_at: base - 3 * 60_000, reactions: [], attachments: [], embeds: [], reply_to: undefined });
    set(2, {
      author: u.me,
      text: `<@${u.minji}> 레이드 끝나고 시더 커버리지 한번 봐 주세요. 스크린샷 채널이 0이에요`,
      created_at: base - 60_000,
      edited_at: base - 30_000,
      reactions: [],
      attachments: [],
      embeds: [],
      deleted: false,
      reply_to: undefined,
    });
    set(1, {
      author: u.minji,
      text: `넵 확인할게요! NAS 시더에 스크린샷 채널 추가해 둘게요 <:${yangpa.name}:${yangpa.id}>`,
      created_at: base,
      reactions: [{ emoji: '🙏', count: 1, me: true }],
      attachments: [],
      embeds: [],
      deleted: false,
      reply_to: undefined,
    });
  }

  private vp(key: string, extra: Partial<VoiceParticipant>): VoiceParticipant {
    const user = this.userIds[key];
    return { user, device: this.devices.get(user)!, muted: false, deafened: false, screen: false, ...extra };
  }

  /** 연락처 교환 흉내: 내 링크로 만난 임시 연락처 하나와 친구가 되지 않고 만료된 연락처 하나. */
  private addContacts() {
    const u = this.userIds;
    const now = this.now0;
    const contacts: Array<[string, FriendView['kind'], number | undefined]> = [
      ['hajun', 'temporary', now + 5 * 3600_000 + 20 * 60_000],
      ['yerin', 'expired', now - 2 * DAY],
    ];
    for (const [key, kind, expires_at] of contacts) {
      const user = u[key];
      const sp = this.contactSpace(user, [this.memberOf('me', {}, []), this.memberOf(key, {}, [])]);
      const general = sp.view.channels[0].channel;
      sp.messages.set(
        general,
        generateHistory({
          count: 6,
          authors: [u.me, user],
          endAt: kind === 'expired' ? now - 3 * DAY : now - 40 * 60_000,
          spanMs: 3600_000,
          rng: this.rng,
          emojis: [],
          mentionable: [],
          me: u.me,
          blobs: this.blobs,
          lamportBase: this.nextLamport(6),
        }),
      );
      this.friends.push({
        user,
        profile: this.users.get(user)!,
        space: sp.view.space,
        online: this.online.get(user) ?? false,
        safety_number_verified: false,
        kind,
        expires_at,
        proposed: false,
      });
      this.safety.set(user, { digits: this.rng.digits(60), verified: false });
    }
    // 하준은 내 링크로 들어왔습니다(그 링크를 폐기하면 만료).
    const link = this.friendLinks[0];
    if (link) {
      link.uses = 1;
      this.contactLinks.set(u.hajun, link.token_id);
    }
  }

  /** 게임 모임의 P2P 전용 채널 하나(이 기기는 아직 참가하지 않아 잠긴 채로 시작). */
  private addP2pChannel() {
    const crew = this.crew();
    if (!crew) return;
    const talk = crew.view.channels.find((c) => c.settings.kind === 'category' && c.settings.name === '대화');
    const id = this.rng.id(16);
    const position = crew.view.channels.filter((c) => c.settings.parent === (talk?.channel ?? null)).length;
    const settings = channel('직접-대화', 'text', {
      parent: talk?.channel ?? null,
      position,
      topic: '참가한 기기끼리 직접 연결 · 서로 IP가 보입니다',
      history: 'public',
    });
    crew.view.channels.push(channelView(id, settings, { p2p: true, p2p_joined: false }));
    const authors = [this.userIds.minji, this.userIds.doyun, this.me];
    crew.messages.set(
      id,
      generateHistory({
        count: 16,
        authors,
        endAt: this.now0 - 30 * 60_000,
        spanMs: 2 * DAY,
        rng: this.rng,
        emojis: [],
        mentionable: [],
        me: this.me,
        blobs: this.blobs,
        lamportBase: this.nextLamport(16),
      }),
    );
  }

  /** 게임 모임의 익명 채널: 글쓴이가 가려진 텍스트 채널과, 참가한 P2P 전용 익명 통화 채널(통화 화면을 볼 수 있게). */
  private addAnonChannels() {
    const crew = this.crew();
    if (!crew) return;
    const talk = crew.view.channels.find((c) => c.settings.kind === 'category' && c.settings.name === '대화');
    const voiceCat = crew.view.channels.find((c) => c.settings.kind === 'category' && c.settings.name === '통화');
    const text = this.rng.id(16);
    const voice = this.rng.id(16);
    const position = (parent: string | undefined) => crew.view.channels.filter((c) => c.settings.parent === (parent ?? null)).length;
    crew.view.channels.push(
      channelView(text, channel('익명-건의함', 'text', { parent: talk?.channel ?? null, position: position(talk?.channel), topic: '누가 썼는지 보이지 않아요' }), {
        anonymous: true,
      }),
      channelView(voice, channel('익명-수다', 'voice', { parent: voiceCat?.channel ?? null, position: position(voiceCat?.channel) }), {
        anonymous: true,
        p2p: true,
        p2p_joined: true,
      }),
    );
    const authors = [this.userIds.minji, this.userIds.doyun, this.userIds.seoyeon, this.me];
    crew.messages.set(
      text,
      generateHistory({
        count: 12,
        authors,
        endAt: this.now0 - 50 * 60_000,
        spanMs: DAY,
        rng: this.rng,
        emojis: [],
        mentionable: [],
        me: this.me,
        blobs: this.blobs,
        lamportBase: this.nextLamport(12),
      }).map((m) => ({ ...m, anonymous: true })),
    );
    const parts = [this.vp('minji', {}), this.vp('doyun', { muted: true })];
    crew.voice.set(voice, parts);
    const anon = new Map(parts.map((x, i) => [x.device, i + 1]));
    crew.voiceAnon.set(voice, anon);
    const ch = crew.view.channels.find((c) => c.channel === voice);
    if (ch) ch.participants = parts.map((x) => x.user);
    crew.coverage.set(text, 2);
    crew.coverage.set(voice, -1);
  }

  /** 익명으로 보여야 하는 글인지(익명으로 썼거나 채널이 지금 익명). */
  private anonymousIn(sp: MockSpace, channelId: string, written: boolean | undefined): boolean {
    return !!written || !!sp.view.channels.find((c) => c.channel === channelId)?.anonymous;
  }

  /** 내보낼 메시지 보기: 데몬처럼 `mine`을 채우고, 익명이면 작성자·기기를 가립니다(차단 표시도 지움). */
  present(sp: MockSpace, channelId: string, m: MessageView): MessageView {
    const out: MessageView = { ...m };
    delete out.anonymous;
    delete out.mine;
    if (m.author === this.me) out.mine = true;
    if (this.anonymousIn(sp, channelId, m.anonymous)) {
      out.anonymous = true;
      out.author = ANONYMOUS_USER;
      out.author_device = ANONYMOUS_DEVICE;
      out.author_blocked = false;
      out.anon_name = mockAnonName(m.author);
    }
    return out;
  }

  /**
   * 시드 포럼 꾸미기: 마크다운 본문, 표지가 될 이미지 첨부, 대문(첫 글). 무작위 값을 더 뽑지 않아(블롭 ID는 글 ID에서 만듦)
   * 다른 시드 데이터가 바뀌지 않습니다.
   */
  private decorateForum(sp: MockSpace, channelId: string) {
    const posts = sp.posts.get(channelId) ?? [];
    posts.forEach((post, i) => {
      const head = sp.threads.get(post.id)?.find((m) => m.id === post.id);
      if (!head) return;
      const body = FORUM_BODIES[i];
      if (body) head.text = body;
      if (i === 1 || i === 5) {
        const blob = [...post.id].reverse().join('');
        this.blobs.set(blob, { svg: sceneSvg(900 + i, 1600, 900, post.title), mime: 'image/png' });
        head.attachments = [
          {
            blob,
            name: `공략_${i}.png`,
            mime: 'image/png',
            size: 840_000 + i,
            width: 1600,
            height: 900,
            thumbnail_base64: b64(sceneSvg(900 + i, 200, 112, '')),
            downloaded: true,
          },
        ];
      }
    });
    const ch = sp.view.channels.find((c) => c.channel === channelId);
    if (ch && posts[0]) ch.featured_post = posts[0].id;
  }

  /** 보낼 첨부(경로) → 보기. 이미지는 그림을 만들어 두고, 동영상은 앱이 실어 보낸 썸네일·길이를 씁니다. */
  private mockAttachments(list: AttachmentInput[]): AttachmentView[] {
    return list.map((a, i) => {
      const name = a.name ?? a.path.split(/[\\/]/).pop() ?? '파일';
      const isImg = /\.(png|jpe?g|gif|webp)$/i.test(name);
      const isVideo = /\.(mp4|m4v|webm|mov)$/i.test(name);
      const blob = this.rng.id(32);
      if (isImg) this.blobs.set(blob, { svg: sceneSvg(500 + i, 1200, 800, name), mime: 'image/png' });
      return {
        blob,
        name,
        mime: isImg ? 'image/png' : isVideo ? 'video/mp4' : 'application/octet-stream',
        size: 120_000 + i * 1000,
        width: isImg ? 1200 : isVideo ? a.width : undefined,
        height: isImg ? 800 : isVideo ? a.height : undefined,
        duration_ms: isVideo ? a.duration_ms : undefined,
        thumbnail_base64: isImg ? b64(sceneSvg(500 + i, 150, 100, '')) : isVideo ? a.thumbnail_base64 : undefined,
        downloaded: true,
      };
    });
  }

  /** 게시글의 첫 메시지(본문·첨부). 지웠으면 undefined. */
  private postHead(sp: MockSpace, post: string): MessageView | undefined {
    const head = sp.threads.get(post)?.find((m) => m.id === post);
    return head && !head.deleted ? head : undefined;
  }

  /** 데몬 `check_attach_limit`과 같은 규칙: 크루의 카테고리가 아닌 채널, 0(첨부 금지) 또는 1MiB 이상, 채널 관리 권한. */
  private checkAttachLimit(sp: MockSpace, kind: ChannelKind, limit: number | null) {
    if (sp.view.kind !== 'crew') throw new RpcError(ErrorCode.InvalidInput, '첨부 한도는 크루 채널에서만 정할 수 있습니다');
    if (kind === 'category') throw new RpcError(ErrorCode.InvalidInput, '카테고리에는 첨부 한도가 없습니다');
    if (limit !== null && limit !== 0 && limit < 1024 * 1024) {
      throw new RpcError(ErrorCode.InvalidInput, '첨부 한도는 0(첨부 금지)이거나 1MiB 이상이어야 합니다');
    }
    this.requirePerm(sp, 'manage_channels');
  }

  /** 데몬 `check_edit_rules`와 같은 규칙: 크루의 카테고리가 아닌 채널, 채널 관리 권한. */
  private checkEditRules(sp: MockSpace, kind: ChannelKind) {
    if (sp.view.kind !== 'crew') throw new RpcError(ErrorCode.InvalidInput, '수정·삭제 금지는 크루 채널에서만 정할 수 있습니다');
    if (kind === 'category') throw new RpcError(ErrorCode.InvalidInput, '카테고리에는 수정·삭제 금지가 없습니다');
    this.requirePerm(sp, 'manage_channels');
  }

  private presentPost(sp: MockSpace, channelId: string, post: ForumPostView): ForumPostView {
    const out: ForumPostView = { ...post };
    delete out.anonymous;
    delete out.mine;
    delete out.featured;
    delete out.cover;
    if (post.author === this.me) out.mine = true;
    if (this.anonymousIn(sp, channelId, post.anonymous)) {
      out.anonymous = true;
      out.author = ANONYMOUS_USER;
    }
    // 데몬처럼 미리보기·표지·첨부 수는 게시글 메시지에서 만듭니다.
    const head = this.postHead(sp, post.id);
    out.preview = Array.from(head?.text ?? '')
      .slice(0, FORUM_PREVIEW_CHARS)
      .join('');
    out.attachment_count = head?.attachments.length ?? 0;
    if (head?.edited_at) out.edited_at = head.edited_at;
    else delete out.edited_at;
    const cover = head?.attachments.find((a) => a.mime.startsWith('image/') || a.mime.startsWith('video/'));
    if (cover) out.cover = { ...cover };
    if (sp.view.channels.find((c) => c.channel === channelId)?.featured_post === post.id) out.featured = true;
    return out;
  }

  /** 통화 참가자 보기: 이 기기 줄에 `mine`, 익명 채널이면 사용자를 가리고 들어온 순서 번호를 붙입니다(기기는 그대로). */
  private presentVoice(sp: MockSpace, channelId: string, list: VoiceParticipant[]): VoiceParticipant[] {
    const anonymous = this.anonymousIn(sp, channelId, false);
    const nums = sp.voiceAnon.get(channelId) ?? new Map<string, number>();
    sp.voiceAnon.set(channelId, nums);
    return list.map((x) => {
      const out: VoiceParticipant = { ...x };
      delete out.anon;
      delete out.mine;
      if (x.device === this.myDevice) out.mine = true;
      if (anonymous) {
        if (!nums.has(x.device)) nums.set(x.device, nums.size + 1);
        out.user = ANONYMOUS_USER;
        out.anon = nums.get(x.device);
      }
      return out;
    });
  }

  /**
   * 내보낼 공간 보기: 익명 채널의 참가자는 수만 맞춰 ANONYMOUS_USER로 채웁니다. 크루 서버 목록(비밀번호 포함)은 데몬처럼 고칠 수
   * 있는 사람(크루 관리)에게만 싣고, 다른 멤버에게는 내 역할로 쓸 수 있는지만 알립니다.
   */
  private presentView(sp: MockSpace): SpaceView {
    const out = structuredClone(sp.view);
    for (const c of out.channels) if (c.anonymous && c.participants) c.participants = c.participants.map(() => ANONYMOUS_USER);
    // 데몬처럼 지운 대문 게시글은 보기에서 뺍니다.
    for (const c of out.channels) if (c.featured_post && !this.postHead(sp, c.featured_post)) delete c.featured_post;
    if (out.kind === 'crew') {
      const has = (perm: string) => out.my_permissions.includes(perm) || out.my_permissions.includes('administrator');
      if (has('manage_space')) out.crew_servers = structuredClone(sp.servers);
      if (sp.servers.stun.length > 0 && has('use_crew_stun')) out.crew_stun = true;
      if (sp.useServers) out.use_crew_servers = true;
    }
    return out;
  }

  private emitRoster(sp: MockSpace, channelId: string, list: VoiceParticipant[], generation = 3, media_key = bytesToB64(new Uint8Array(32))) {
    this.emit({
      type: 'voice.participants',
      space: sp.view.space,
      channel: channelId,
      participants: this.presentVoice(sp, channelId, list),
      generation,
      media_key,
      ...this.mockTopology(sp, channelId, list),
    });
  }

  /** 데몬처럼 익명 채널이면 SFU 호스트의 사용자를 알리지 않습니다. */
  private mockTopology(sp: MockSpace, channelId: string, list: VoiceParticipant[]) {
    // 데몬처럼 Tor로 도는 기기의 일반 채널 통화는 Tor 통화(WebRTC 없음, 소리만).
    const ch = sp.view.channels.find((c) => c.channel === channelId);
    if (this.net.mode === 'tor' && !(ch?.p2p && ch.p2p_joined)) return { topology: 'tor' as const };
    const t = mockTopology(list);
    if (this.anonymousIn(sp, channelId, false)) delete t.sfu_host_user;
    return t;
  }

  /**
   * 익명 켜기·끄기. 끄면 데몬처럼 모든 참가자가 그 채널 통화에서 스스로 나옵니다(이름이 보이는 채널에 익명으로 들어온
   * 참가자가 남지 않게). 켜져 있는 동안 쓴 글은 `anonymous`가 남아 계속 익명으로 보입니다.
   */
  private setAnonymous(sp: MockSpace, ch: ChannelView, enabled: boolean) {
    if (enabled) {
      ch.anonymous = true;
      return;
    }
    delete ch.anonymous;
    sp.voiceAnon.delete(ch.channel);
    if ((sp.voice.get(ch.channel) ?? []).length > 0) {
      sp.voice.set(ch.channel, []);
      ch.participants = [];
      this.emitRoster(sp, ch.channel, []);
    }
  }

  /** 데몬의 익명 규칙: 크루의, 카테고리가 아닌 채널만, 채널 관리 권한. */
  private checkAnonymous(sp: MockSpace, kind: ChannelKind) {
    if (sp.view.kind !== 'crew') throw new RpcError(ErrorCode.InvalidInput, '익명 채널은 크루에서만 만들 수 있습니다');
    if (kind === 'category') throw new RpcError(ErrorCode.InvalidInput, '카테고리는 익명으로 만들 수 없습니다');
    this.requirePerm(sp, 'manage_channels');
  }

  private befriend(f: FriendView) {
    f.kind = 'friend';
    f.expires_at = undefined;
    f.proposed = false;
    this.contactLinks.delete(f.user);
    this.emit({ type: 'friend.accepted', user: f.user, space: f.space });
  }

  /** 1대1 대화 공간(친구·연락처 공통, 채널 하나). */
  private contactSpace(user: string, members: MemberView[]): MockSpace {
    const sp = this.makeSpace(this.rng.id(16), 'space', this.users.get(user)?.name ?? '', this.me, members, [
      { id: EVERYONE_ROLE, name: '@everyone', color: 0, rank: 0, permissions: 1983, automation: false },
    ], [[this.rng.id(16), channel('일반', 'text')]], []);
    sp.peer = user;
    sp.view.my_permissions = ['administrator'];
    return sp;
  }

  /** 만료 시각이 지난 임시 연락처를 만료된 연락처로(데몬처럼 friend.updated). */
  private expireContacts(now = Date.now(), only?: (f: FriendView) => boolean) {
    for (const f of this.friends) {
      if (f.kind !== 'temporary') continue;
      if (only ? !only(f) : f.expires_at === undefined || f.expires_at > now) continue;
      f.kind = 'expired';
      f.proposed = false;
      this.contactLinks.delete(f.user);
      this.requests = this.requests.filter((r) => r.user !== f.user);
      this.emit({ type: 'friend.updated', user: f.user });
    }
  }

  private memberOf(key: string, roleIds: Record<string, string>, roles: string[]): MemberView {
    const seed = MEMBER_SEEDS.find((s) => s.key === key)!;
    const user = this.userIds[key];
    const mv = memberView(user, { ...seed, roles }, roleIds);
    mv.profile = this.users.get(user)!;
    mv.online = this.online.get(user) ?? false;
    return mv;
  }

  private makeSpace(
    space: string,
    kind: SpaceKind,
    name: string,
    owner: string,
    members: MemberView[],
    roles: Role[],
    chans: Array<[string, ChannelSettings, Partial<ChannelView>?]>,
    emojis: EmojiView[],
  ): MockSpace {
    const view: SpaceView = {
      space,
      kind,
      settings: { name, description: '', icon: null, allow_external_emoji: false, strict_rotation: false },
      owner,
      committer: this.devices.get(owner) ?? this.myDevice,
      members,
      roles,
      channels: chans.map(([id, s, extra]) => channelView(id, s, extra)),
      emojis,
      my_permissions: [],
      pending_commit: false,
      dormant: false,
      trusted: false,
    };
    const sp: MockSpace = {
      view,
      messages: new Map(),
      gaps: new Map(),
      hidden: new Map(),
      threads: new Map(),
      posts: new Map(),
      invites: [],
      voice: new Map(),
      voiceAnon: new Map(),
      coverage: new Map(),
      archives: [],
      servers: { stun: [] },
      useServers: false,
      unread: 0,
      mention: 0,
    };
    for (const [id, s] of chans) {
      if (s.kind === 'text') sp.messages.set(id, []);
    }
    this.spaces.set(space, sp);
    return sp;
  }

  private nextLamport(n: number) {
    const base = this.lamport;
    this.lamport += n + 10;
    return base;
  }

  // ───────────── 활동 흉내 ─────────────

  private startActivity() {
    this.timers.push(
      setInterval(() => {
        const crew = this.crew();
        if (!crew) return;
        const ch = crew.view.channels.find((c) => c.settings.name === '잡담');
        if (!ch) return;
        const authors = crew.view.members.filter((m) => m.online && m.user !== this.me && !m.automation);
        if (authors.length === 0) return;
        const a = this.rng.pick(authors);
        this.postAs(crew, ch.channel, a.user, this.rng.pick(['ㅋㅋㅋ', '오 좋네요', '저도요!', '잠깐만요', '굿굿 👍', '확인했습니다']));
      }, 45_000),
    );
    this.timers.push(
      setInterval(() => {
        const ids = [...this.online.keys()].filter((id) => id !== this.me);
        const id = this.rng.pick(ids);
        const v = !this.online.get(id);
        this.online.set(id, v);
        for (const sp of this.spaces.values()) for (const m of sp.view.members) if (m.user === id) m.online = v;
        this.emit({ type: 'presence', user: id, online: v });
      }, 60_000),
    );
    // 전송 속도: 데몬처럼 1초마다 알립니다(흉내 값은 시각에 따라 바뀜).
    this.timers.push(setInterval(() => this.emit({ type: 'transfer.rates', ...this.transferRates() }), 1000));
    this.timers.push(
      setInterval(() => {
        for (const key of this.joinedVolatile) {
          const [spaceId, channelId] = key.split('/');
          const sp = this.spaces.get(spaceId);
          const ch = sp?.view.channels.find((c) => c.channel === channelId);
          if (!sp || !ch) continue;
          const parts = (ch.participants ?? []).filter((p) => p !== this.me);
          if (parts.length === 0) continue;
          this.postAs(sp, channelId, this.rng.pick(parts), this.rng.pick(VOLATILE_LINES));
        }
      }, 9_000),
    );
  }

  /** 휘발성 메시지에 실린 블롭인지(데몬처럼 저장에 확인을 요구하려고). */
  private volatileBlob(blob: string): boolean {
    for (const sp of this.spaces.values()) {
      for (const list of sp.messages.values()) {
        if (list.some((m) => m.volatile && m.attachments.some((a) => a.blob === blob))) return true;
      }
    }
    return false;
  }

  /**
   * 지금 전송 속도 흉내. 난수를 쓰지 않고 시각으로 출렁이게 해 세계의 난수 순서가 바뀌지 않습니다. 정모 사진을 민지·하준이
   * 받고 있고, 전체 올리기는 그 합보다 조금 큽니다(메시지 동기화).
   */
  transferRates(now = Date.now()): TransferRates {
    const t = now / 1000;
    const wave = (period: number, base: number) => Math.round(base * (1 + 0.35 * Math.sin(t / period)));
    const uploads: UploadRate[] = [];
    const target = this.uploadTarget;
    if (target) {
      uploads.push({ ...target, user: this.userIds.minji, bps: wave(3.1, 42 * 1024) });
      uploads.push({ ...target, user: this.userIds.hajun, bps: wave(4.7, 18 * 1024) });
      uploads.sort((a, b) => b.bps - a.bps);
    }
    const up = uploads.reduce((n, u) => n + u.bps, 0) + wave(5.3, 3 * 1024);
    return { up_bps: up, down_bps: wave(2.3, 96 * 1024), ...(uploads.length ? { uploads } : {}) };
  }

  /** `rate.set_limit`과 같은 규칙(0은 막음, 둘 다 null이면 지움, 1024개까지). */
  setRateLimit(scope: RateScope, up: number | null, down: number | null) {
    for (const v of [up, down]) {
      if (v !== null && (!Number.isInteger(v) || v < 0)) throw new RpcError(ErrorCode.InvalidParams, '속도 제한은 0 이상의 정수여야 합니다');
    }
    const key = scopeKey(scope);
    if (up === null && down === null) {
      this.rateLimits.delete(key);
      return;
    }
    if (!this.rateLimits.has(key) && this.rateLimits.size >= 1024) throw new RpcError(ErrorCode.InvalidInput, '속도 제한은 1024개까지입니다');
    this.rateLimits.set(key, { scope, up_bps: up, down_bps: down });
  }

  crew(): MockSpace | undefined {
    return [...this.spaces.values()].find((s) => s.view.settings.name === '게임 모임');
  }

  postAs(sp: MockSpace, channelId: string, author: string, text: string, extra: Partial<MessageView> = {}) {
    const ch = sp.view.channels.find((c) => c.channel === channelId);
    if (!ch) return null;
    const volatile = ch.settings.security === 'volatile';
    const ttl = ch.settings.ttl.mode === 'seconds' ? ch.settings.ttl.seconds : null;
    const now = Date.now();
    const m: MessageView = {
      id: this.rng.id(32),
      author,
      author_device: this.devices.get(author) ?? this.myDevice,
      created_at: now,
      lamport: this.lamport++,
      text,
      attachments: [],
      embeds: [],
      reactions: [],
      deleted: false,
      volatile,
      expires_at: volatile && ttl ? now + ttl * 1000 : undefined,
      ...extra,
    };
    // 익명 채널에서 쓴 글은 채널 익명을 꺼도 익명으로 남습니다.
    if (ch.anonymous) m.anonymous = true;
    const list = m.thread ? sp.threads.get(m.thread) : sp.messages.get(channelId);
    if (!list) return null;
    list.push(m);
    if (m.thread) {
      const post = sp.posts.get(channelId)?.find((p) => p.id === m.thread);
      if (post) {
        post.reply_count++;
        post.last_activity = now;
      }
    }
    if (author !== this.me && !m.thread) {
      sp.unread++;
      ch.unread++;
      if (m.text.includes(`<@${this.me}>`)) sp.mention++;
    }
    const spaceName = sp.view.kind === 'space' ? this.users.get(author)?.name ?? '' : sp.view.settings.name;
    const authorName = m.anonymous ? '익명' : this.users.get(author)?.name ?? '알 수 없음';
    this.emit({
      type: 'message.new',
      space: sp.view.space,
      channel: channelId,
      message: this.present(sp, channelId, m),
      // 휘발성 채널 알림에는 본문을 싣지 않습니다.
      notify: volatile
        ? { title: `${spaceName} #${ch.settings.name}` }
        : { title: `${authorName} (${spaceName} #${ch.settings.name})`, body: text },
    });
    return m;
  }

  private expireVolatile() {
    const now = Date.now();
    for (const sp of this.spaces.values()) {
      for (const [cid, list] of sp.messages) {
        const expired = list.filter((m) => m.expires_at !== undefined && m.expires_at <= now);
        if (expired.length === 0) continue;
        sp.messages.set(
          cid,
          list.filter((m) => !(m.expires_at !== undefined && m.expires_at <= now)),
        );
        this.emit({ type: 'message.expired', space: sp.view.space, channel: cid, ids: expired.map((m) => m.id) });
      }
    }
  }

  // ───────────── 메서드 ─────────────

  private space(id: unknown): MockSpace {
    const sp = typeof id === 'string' ? this.spaces.get(id) : undefined;
    if (!sp) throw new RpcError(ErrorCode.NotFound, '공간을 찾을 수 없습니다');
    return sp;
  }

  private channelOf(sp: MockSpace, id: unknown): ChannelView {
    const ch = sp.view.channels.find((c) => c.channel === id);
    if (!ch) throw new RpcError(ErrorCode.NotFound, '채널을 찾을 수 없습니다');
    return ch;
  }

  private requireIdentity() {
    if (!this.identity) throw new RpcError(ErrorCode.NoIdentity, '아직 신원이 없습니다');
  }

  private findMessage(sp: MockSpace, channelId: string, id: string): { list: MessageView[]; idx: number } {
    const lists: MessageView[][] = [];
    const main = sp.messages.get(channelId);
    if (main) lists.push(main);
    for (const t of sp.threads.values()) lists.push(t);
    for (const list of lists) {
      const idx = list.findIndex((m) => m.id === id);
      if (idx >= 0) return { list, idx };
    }
    throw new RpcError(ErrorCode.NotFound, '메시지를 찾을 수 없습니다');
  }

  /** 데몬처럼 내가 차단한 사람의 메시지에 표시를 붙입니다. */
  markBlocked(m: MessageView): MessageView {
    return this.blocked.has(m.author) ? { ...m, author_blocked: true } : m;
  }

  private markMembersBlocked() {
    for (const sp of this.spaces.values()) {
      let changed = false;
      for (const m of sp.view.members) {
        const b = this.blocked.has(m.user);
        if (!!m.blocked !== b) {
          m.blocked = b;
          changed = true;
        }
      }
      if (changed) this.spaceUpdated(sp);
    }
  }

  /** 나와 함께 멤버인 공간이 있는지(데몬처럼 만료된 연락처의 1대1 공간은 닫힌 것으로 셈). */
  private sharesActiveSpace(user: string): boolean {
    for (const sp of this.spaces.values()) {
      const members = sp.view.members;
      if (!members.some((m) => m.user === this.me) || !members.some((m) => m.user === user)) continue;
      if (sp.view.kind === 'space' && this.friends.some((f) => f.space === sp.view.space && f.kind === 'expired')) continue;
      return true;
    }
    return false;
  }

  /** 이 사람과 함께 멤버인 크루 중 내가 신뢰한 크루. */
  private trustViaCrews(user: string): string[] {
    return [...this.spaces.values()]
      .filter((sp) => sp.view.kind === 'crew' && sp.view.trusted)
      .filter((sp) => sp.view.members.some((m) => m.user === this.me) && sp.view.members.some((m) => m.user === user))
      .map((sp) => sp.view.space);
  }

  /** 데몬의 연결 방식 흉내: 접속 중이고 함께 쓰는 공간이 있어야 이어지고, 신뢰 방향에 따라 P2P·단방향·Tor. */
  private connectionMode(user: string): ConnectionMode {
    if (user === this.me || !this.online.get(user) || !this.sharesActiveSpace(user)) return 'offline';
    const mine = this.trustedUsers.has(user) || this.trustViaCrews(user).length > 0;
    const theirs = this.trustsMe.has(user);
    if (mine && theirs) return 'p2p';
    if (mine) return 'one_way_me_exposed';
    if (theirs) return 'one_way_peer_exposed';
    return 'tor';
  }

  /** 데몬의 P2P 전용 규칙: 텍스트·통화 채널만, 내 기기끼리 공간은 안 됨, 그 채널을 관리할 수 있어야 함. */
  private checkP2p(sp: MockSpace, kind: ChannelKind) {
    if (sp.view.kind === 'self_space') throw new RpcError(ErrorCode.InvalidInput, '내 기기끼리 쓰는 공간에는 P2P 전용 채널을 둘 수 없습니다');
    if (kind !== 'text' && kind !== 'voice') throw new RpcError(ErrorCode.InvalidInput, 'P2P 전용은 텍스트·통화 채널만 됩니다');
    this.requirePerm(sp, 'manage_channels');
  }

  private requirePerm(sp: MockSpace, perm: string) {
    const mine = sp.view.my_permissions;
    if (!mine.includes(perm) && !mine.includes('administrator')) throw new RpcError(ErrorCode.Forbidden, '권한이 없습니다');
  }

  /**
   * 데몬의 덮어쓰기 검사: 모르는 비트, 같은 비트의 허용·거부, 대상 중복, 모르는 역할, 멤버가 아닌 대상, 100개 초과는 거절하고
   * 빈 항목은 버립니다. 바뀌는 비트가 모두 내게 있어야 합니다(관리자는 전부).
   */
  private checkOverwrites(sp: MockSpace, raw: unknown, old: PermissionOverwrite[]): PermissionOverwrite[] {
    const bad = (msg: string) => new RpcError(ErrorCode.InvalidInput, msg);
    if (!Array.isArray(raw)) throw bad('권한 덮어쓰기는 목록이어야 합니다');
    if (raw.length > 100) throw bad('권한 덮어쓰기는 100개까지 둘 수 있습니다');
    const known = Object.values(PERM_BITS).reduce((a, b) => a | (1n << BigInt(b)), 0n);
    const out: PermissionOverwrite[] = [];
    const seen: PermissionOverwrite['target'][] = [];
    for (const o of raw) {
      const t = o?.target;
      if ((t?.type !== 'role' && t?.type !== 'member') || typeof t.id !== 'string') throw bad('권한 덮어쓰기 대상이 잘못됐습니다');
      const target = { type: t.type, id: t.id } as PermissionOverwrite['target'];
      const { allow, deny } = o;
      if (![allow, deny].every((v) => Number.isSafeInteger(v) && v >= 0)) throw bad('권한 비트가 잘못됐습니다');
      const a = BigInt(allow);
      const d = BigInt(deny);
      if ((a | d) & ~known) throw bad('알 수 없는 권한 비트입니다');
      if (a & d) throw bad('같은 권한을 허용과 거부에 함께 둘 수 없습니다');
      if (seen.some((x) => sameTarget(x, target))) throw bad('권한 덮어쓰기 대상이 겹칩니다');
      seen.push(target);
      if (target.type === 'role' && !sp.view.roles.some((r) => r.id === target.id)) throw bad('알 수 없는 역할입니다');
      if (target.type === 'member' && !sp.view.members.some((m) => m.user === target.id)) throw bad('크루 멤버가 아닌 대상입니다');
      if (allow !== 0 || deny !== 0) out.push({ target, allow, deny });
    }
    const mine = sp.view.my_permissions;
    if (!mine.includes('administrator') && overwriteDelta(old, out).some((p) => !mine.includes(p))) {
      throw new RpcError(ErrorCode.Forbidden, '내게 없는 권한은 채널에서 바꿀 수 없습니다');
    }
    return out;
  }

  /** 역할 표시 설정을 바꾸고, 그 역할의 기본값(@everyone은 관리자 보기 켜짐, 다른 역할은 꺼짐)과 같으면 표에서 뺍니다. */
  private setRoleDisplay(sp: MockSpace, role: string, p: Record<string, any>) {
    this.requirePerm(sp, 'manage_roles');
    const everyone = role === EVERYONE_ROLE;
    if (p.visible_to !== undefined && everyone) throw new RpcError(ErrorCode.InvalidInput, '@everyone에는 볼 수 있는 역할을 정할 수 없습니다');
    if (p.visible_to != null && (!Array.isArray(p.visible_to) || p.visible_to.some((x: unknown) => !sp.view.roles.some((r) => r.id === x)))) {
      throw new RpcError(ErrorCode.InvalidInput, '볼 수 있는 역할에 모르는 역할이 있습니다');
    }
    if (p.see_admins !== undefined && typeof p.see_admins !== 'boolean') throw new RpcError(ErrorCode.InvalidInput, 'see_admins는 참·거짓이어야 합니다');
    const table = (sp.view.role_display ??= {});
    const cur = table[role] ?? { visible_to: null, see_admins: everyone };
    const next = {
      visible_to: p.visible_to === undefined ? cur.visible_to : p.visible_to && [...new Set<string>(p.visible_to)],
      see_admins: p.see_admins ?? cur.see_admins,
    };
    if (next.visible_to === null && next.see_admins === everyone) delete table[role];
    else table[role] = next;
  }

  /** 신고를 받는 운영진: 추방·추방 금지·메시지 관리 권한 중 하나. */
  private isModerator(sp: MockSpace): boolean {
    const mine = sp.view.my_permissions;
    return ['kick', 'ban', 'manage_messages', 'administrator'].some((x) => mine.includes(x));
  }

  /** 받지 않은 구간 바로 뒤(`before`)에서 다시 읽으면 시더에게서 받아 온 것처럼 그 구간을 채웁니다. */
  private async catchUpGap(sp: MockSpace, channelId: string, before: string) {
    const gaps = sp.gaps.get(channelId) ?? [];
    const g = gaps.find((x) => x.before === before);
    if (!g) return;
    await delay(1200);
    const hidden = sp.hidden.get(channelId) ?? [];
    const list = sp.messages.get(channelId) ?? [];
    const at = list.findIndex((m) => m.id === g.before);
    if (at >= 0) list.splice(at, 0, ...hidden);
    sp.hidden.delete(channelId);
    sp.gaps.set(
      channelId,
      gaps.filter((x) => x !== g),
    );
  }

  private updated(sp: MockSpace, channelId: string, m: MessageView) {
    this.emit({ type: 'message.updated', space: sp.view.space, channel: channelId, message: this.present(sp, channelId, m) });
  }

  private spaceUpdated(sp: MockSpace) {
    this.emit({ type: 'space.updated', space: sp.view.space });
  }

  private summary(sp: MockSpace) {
    return {
      space: sp.view.space,
      kind: sp.view.kind,
      name: sp.view.settings.name,
      unread: sp.unread,
      mention: sp.mention,
      dormant: sp.view.dormant,
      member_count: sp.view.members.length,
    };
  }

  private auditLog(method: string, ok: boolean, code?: number) {
    this.audit.push({ id: this.auditSeq++, at: Date.now(), method, token_name: '데스크톱 앱', ok, code, automation: false });
    if (this.audit.length > 500) this.audit.shift();
  }

  async handle(method: string, p: Record<string, any>): Promise<unknown> {
    try {
      const r = await this.dispatch(method, p ?? {});
      if (!method.startsWith('diag.') && method !== 'audit.list') this.auditLog(method, true);
      return r;
    } catch (e) {
      this.auditLog(method, false, e instanceof RpcError ? e.code : ErrorCode.Internal);
      throw e;
    }
  }

  private async dispatch(method: string, p: Record<string, any>): Promise<unknown> {
    const ok = {};
    switch (method) {
      case 'system.version':
        return { version: '0.1.0-mock', protocol: 1 };
      case 'system.status':
        return {
          identity: this.identity?.user,
          device: this.identity?.device,
          net: this.net,
          spaces: this.spaces.size,
          tor: { enabled: this.net.mode === 'tor', bootstrap_progress: this.net.bootstrap_progress },
        };
      case 'diag.metrics':
        return {
          delivery_ms_p50: 42 + this.rng.int(0, 15),
          delivery_ms_p95: 180 + this.rng.int(0, 60),
          memory_bytes: 96 * 1024 * 1024 + this.rng.int(0, 8 * 1024 * 1024),
          cpu_percent: 0.4 + this.rng.float() * 0.5,
          queues: { sync: this.rng.int(0, 3), gossip: this.rng.int(0, 5), blobs: this.rng.int(0, 2), events: 0 },
          events_per_sec: 3 + this.rng.float() * 6,
        };
      case 'diag.leakcheck':
        return { clean: true, findings: [] };

      // 신원
      case 'identity.create': {
        if (this.identity) throw new RpcError(ErrorCode.Conflict, '이미 신원이 있습니다');
        const name = String(p.name ?? '').trim();
        if (!name) throw new RpcError(ErrorCode.InvalidInput, '이름이 비었습니다');
        this.createIdentity(name, this.userIds.me);
        return { user: this.identity!.user, device: this.identity!.device };
      }
      case 'identity.get':
        this.requireIdentity();
        return { ...this.identity!, devices: this.myDevices };
      case 'identity.delete':
        this.requireIdentity();
        throw new RpcError(ErrorCode.InvalidInput, 'MOCK에서는 계정을 지우지 않습니다');
      case 'device.list':
        return this.myDevices;
      case 'device.rename': {
        const d = this.myDevices.find((x) => x.device === p.device);
        if (!d) throw new RpcError(ErrorCode.NotFound, '내 기기가 아닙니다');
        d.name = p.name.trim();
        return ok;
      }
      case 'device.revoke': {
        const d = this.myDevices.find((x) => x.device === p.device);
        if (!d) throw new RpcError(ErrorCode.NotFound, '기기를 찾을 수 없습니다');
        if (d.device === this.myDevice) throw new RpcError(ErrorCode.InvalidInput, '지금 쓰는 기기는 폐기할 수 없습니다');
        d.revoked_at = Date.now();
        return ok;
      }
      case 'device.link.create':
        this.requireIdentity();
        clearTimeout(this.linkTimer);
        // 잠시 뒤 새 기기가 이 링크로 들어왔다고 칩니다.
        this.linkTimer = setTimeout(() => {
          this.linkRequest = { name: '새 휴대폰', code: '482913', contested: false };
          this.emit({ type: 'device.link_request', ...this.linkRequest });
        }, 4000);
        return { url: `yangpa://device/${this.rng.id(32)}`, qr: this.fakeQr(), expires_at: Date.now() + 10 * 60_000 };
      case 'device.link.pending':
        return this.linkRequest;
      case 'device.link.decide': {
        const request = this.linkRequest;
        if (!request) throw new RpcError(ErrorCode.NotFound, '승인을 기다리는 기기가 없습니다');
        this.linkRequest = null;
        if (p.approve) this.myDevices.push({ device: this.rng.id(32), box_key: 'G'.repeat(1620), name: request.name, added_at: Date.now(), revoked_at: null });
        return ok;
      }
      case 'device.link.cancel':
        clearTimeout(this.linkTimer);
        this.linkRequest = null;
        return ok;
      case 'device.link.join':
        if (this.identity) throw new RpcError(ErrorCode.Conflict, '이미 신원이 있습니다');
        if (appLinkKind(String(p.url ?? '')) !== 'device') throw new RpcError(ErrorCode.InvalidInput, '기기 연결 링크가 아닙니다');
        this.linkTimer = setTimeout(() => this.acceptLink(), 5000);
        return { code: '482913' };
      case 'device.link.abort':
        clearTimeout(this.linkTimer);
        return ok;

      case 'presence.get':
        return this.presence;
      case 'presence.set': {
        const mode = p.mode ?? this.presence.mode;
        const until = p.mode === undefined ? this.presence.until : mode !== 'online' && p.duration_secs ? Date.now() + p.duration_secs * 1000 : undefined;
        this.presence = { ...this.presence, mode, until, idle: p.idle ?? this.presence.idle, hide_last_seen: p.hide_last_seen ?? this.presence.hide_last_seen };
        const shown = mode === 'online' || mode === 'away';
        this.emit({ type: 'presence.self', ...this.presence });
        this.emit({ type: 'presence', user: this.me, online: shown, away: mode === 'away' || (mode === 'online' && this.presence.idle) });
        return this.presence;
      }

      case 'flair.list':
        return [...this.flair.values()];
      case 'flair.set': {
        const cur = this.flair.get(this.me);
        if (!cur?.halo_allowed) throw new RpcError(ErrorCode.Forbidden, '후광은 월 50달러 이상 정기 후원자와 GitHub에서 50달러 이상 1회 후원한 사람(30일 동안)만 쓸 수 있습니다');
        const { halo: _old, ...rest } = cur;
        const v: FlairView = p.halo ? { ...rest, halo: p.halo } : rest;
        this.flair.set(this.me, v);
        this.emit({ type: 'flair', ...v });
        return v;
      }

      // 후원 확인: 브라우저 승인을 흉내 내 잠시 뒤 연결됩니다.
      case 'supporter.status':
      case 'supporter.refresh':
        return this.supporter;
      case 'supporter.github_begin':
      case 'supporter.patreon_begin': {
        const provider: SupporterProvider = method === 'supporter.github_begin' ? 'github' : 'patreon';
        setTimeout(() => {
          const now = Date.now();
          const link = { provider, login: provider === 'github' ? 'octocat' : 'patron', active: true, checked_at: now, valid_until: now + 3 * 86_400_000, needs_login: false };
          this.supporter = { ...this.supporter, links: [...this.supporter.links.filter((l) => l.provider !== provider), link] };
          this.emit({ type: 'supporter.updated' });
        }, 1500);
        return provider === 'github'
          ? { user_code: 'WDJB-MJHT', verification_uri: 'https://github.com/login/device', expires_in_secs: 900 }
          : { url: 'https://www.patreon.com/oauth2/authorize' };
      }
      case 'supporter.unlink':
        this.supporter = { ...this.supporter, links: this.supporter.links.filter((l) => l.provider !== p.provider) };
        this.emit({ type: 'supporter.updated' });
        return ok;

      // 프로필·메모
      case 'profile.get': {
        const user = p.user ?? this.me;
        const prof = this.users.get(user) ?? this.requests.find((r) => r.user === user)?.profile;
        if (!prof) throw new RpcError(ErrorCode.NotFound, '프로필이 없습니다');
        return prof;
      }
      case 'profile.set': {
        this.requireIdentity();
        const prof = this.users.get(this.me)!;
        if (typeof p.name === 'string') prof.name = p.name;
        if (typeof p.bio === 'string') prof.bio = p.bio;
        if (typeof p.status === 'string') prof.status = p.status;
        if ('theme_color' in p) prof.theme_color = p.theme_color ?? null;
        // 빈 문자열 경로는 이미지를 지웁니다(ProfileSetParams).
        if (typeof p.avatar_path === 'string') prof.avatar = p.avatar_path ? this.mockImage(31, 128, 128) : null;
        if (typeof p.banner_path === 'string') prof.banner = p.banner_path ? this.mockImage(32, 600, 200) : null;
        prof.version++;
        this.identity!.profile = prof;
        return prof;
      }
      case 'profile.set_crew': {
        const sp = this.space(p.space);
        const m = sp.view.members.find((x) => x.user === this.me);
        if (m) m.nickname = p.nickname || undefined;
        this.spaceUpdated(sp);
        return ok;
      }
      case 'badge.get': {
        const me = p.user === this.me;
        const v = this.badges.get(p.user) ?? { earned: me ? ['founder', 'support3', 'friends5', 'seed1gb'] : [], shown: [], can_grant_founder: false };
        return { ...v, can_grant_founder: !me && !v.earned.includes('founder') };
      }
      case 'badge.set_shown': {
        const v: BadgeView = { ...((await this.handle('badge.get', { user: this.me })) as BadgeView), can_grant_founder: false };
        if (p.shown.length > 2) throw new RpcError(ErrorCode.InvalidInput, '뱃지는 두 개까지 보일 수 있습니다');
        if (p.shown.some((s: ShownBadge) => !v.earned.includes(s.badge))) throw new RpcError(ErrorCode.InvalidInput, '모으지 않은 뱃지는 보일 수 없습니다');
        this.badges.set(this.me, { ...v, shown: p.shown });
        this.emit({ type: 'badges.updated', user: this.me });
        return this.badges.get(this.me)!;
      }
      case 'badge.grant_founder': {
        const v = this.badges.get(p.user) ?? { earned: [], shown: [], can_grant_founder: false };
        this.badges.set(p.user, { ...v, earned: ['founder', ...v.earned] });
        this.emit({ type: 'badges.updated', user: p.user });
        return ok;
      }
      case 'note.get':
        return { text: this.notes.get(p.user) ?? '' };
      case 'note.set':
        if (String(p.text ?? '').length > 4000) throw new RpcError(ErrorCode.InvalidInput, '메모가 너무 깁니다');
        this.notes.set(p.user, String(p.text ?? ''));
        return ok;

      // 친구
      case 'friend.link.create': {
        const token_id = 'tok-' + this.rng.id(16).slice(0, 8);
        const now = Date.now();
        const expires_at = p.expires_in_secs ? now + p.expires_in_secs * 1000 : undefined;
        this.friendLinks.push({ token_id, created_at: now, expires_at, single_use: !!p.single_use, uses: 0 });
        return { url: `${WEB_BASE}#friend/${this.rng.id(32)}${this.rng.id(32)}${this.rng.id(16)}`, token_id, expires_at };
      }
      case 'friend.link.list':
        return this.friendLinks;
      case 'friend.link.revoke': {
        this.friendLinks = this.friendLinks.filter((l) => l.token_id !== p.token_id);
        // 링크를 폐기하면 그 링크로 만난 임시 연락처도 만료됩니다.
        this.expireContacts(Date.now(), (f) => this.contactLinks.get(f.user) === p.token_id);
        return ok;
      }
      case 'friend.add': {
        const url = String(p.url ?? '').trim();
        if (appLinkKind(url) !== 'friend' || url.length < 40) throw new RpcError(ErrorCode.InvalidInput, '연락처 링크 형식이 아닙니다');
        // 링크 주인의 승인 없이 바로 1대1 대화가 열리고, 링크가 만료될 때까지 임시 연락처로 남습니다.
        const user = this.rng.id(32);
        const prof = profile(this.rng.pick(['지우', '나연', '현우', '소율']), { bio: '연락처 링크로 만남' });
        this.users.set(user, prof);
        this.devices.set(user, this.rng.id(32));
        this.online.set(user, true);
        const me = this.users.get(this.me)!;
        const sp = this.contactSpace(user, [
          { user: this.me, profile: me, roles: [], online: true, automation: false },
          { user, profile: prof, roles: [], online: true, automation: false },
        ]);
        this.friends.push({
          user,
          profile: prof,
          space: sp.view.space,
          online: true,
          safety_number_verified: false,
          kind: 'temporary',
          expires_at: Date.now() + DAY,
          proposed: false,
        });
        this.safety.set(user, { digits: this.rng.digits(60), verified: false });
        this.emit({ type: 'friend.updated', user });
        this.emit({ type: 'space.updated', space: sp.view.space });
        return { status: 'connected', user };
      }
      case 'friend.propose': {
        const f = this.friends.find((x) => x.user === p.user);
        if (!f) throw new RpcError(ErrorCode.NotFound, '연락처가 아닙니다');
        if (f.kind === 'friend') return { status: 'friends' };
        if (f.kind === 'expired') throw new RpcError(ErrorCode.InvalidInput, '만료된 연락처입니다');
        // 상대가 먼저 친구 추가를 보내 뒀으면 바로 친구가 됩니다.
        if (this.requests.some((r) => r.user === f.user)) {
          this.requests = this.requests.filter((r) => r.user !== f.user);
          this.befriend(f);
          return { status: 'friends' };
        }
        f.proposed = true;
        this.emit({ type: 'friend.updated', user: f.user });
        return { status: 'requested' };
      }
      case 'friend.open': {
        const f = this.friends.find((x) => x.user === p.user);
        if (!f) throw new RpcError(ErrorCode.NotFound, '연락처가 아닙니다');
        // 크루로 바꾼 1:1이면 둘만의 1:1 공간을 새로 만듭니다(데몬과 같음).
        if (f.kind !== 'expired' && this.spaces.get(f.space)?.view.kind === 'crew') {
          const sp = this.contactSpace(f.user, [
            { user: this.me, profile: this.users.get(this.me)!, roles: [], online: true, automation: false },
            { user: f.user, profile: f.profile, roles: [], online: f.online, automation: false },
          ]);
          f.space = sp.view.space;
          this.emit({ type: 'friend.updated', user: f.user });
          this.spaceUpdated(sp);
        }
        return { space: f.space };
      }
      case 'friend.requests':
        return this.requests;
      case 'friend.accept': {
        const req = this.requests.find((r) => r.request_id === p.request_id);
        if (!req) throw new RpcError(ErrorCode.NotFound, '요청을 찾을 수 없습니다');
        this.requests = this.requests.filter((r) => r !== req);
        // 임시 연락처의 친구 추가면 새 대화 없이 지금 1대1 대화를 이어 갑니다.
        const contact = this.friends.find((f) => f.user === req.user);
        if (contact) {
          this.befriend(contact);
          return { space: contact.space };
        }
        this.users.set(req.user, req.profile);
        this.devices.set(req.user, this.rng.id(32));
        this.online.set(req.user, true);
        const spaceId = this.rng.id(16);
        const general = this.rng.id(16);
        const members: MemberView[] = [
          { user: this.me, profile: this.users.get(this.me)!, roles: [], online: true, automation: false },
          { user: req.user, profile: req.profile, roles: [], online: true, automation: false },
        ];
        const sp = this.makeSpace(spaceId, 'space', req.profile.name, this.me, members, [
          { id: EVERYONE_ROLE, name: '@everyone', color: 0, rank: 0, permissions: 1983, automation: false },
        ], [[general, channel('일반', 'text')]], []);
        sp.peer = req.user;
        sp.view.my_permissions = ['administrator'];
        this.friends.push({ user: req.user, profile: req.profile, space: spaceId, online: true, safety_number_verified: false, kind: 'friend', proposed: false });
        this.safety.set(req.user, { digits: this.rng.digits(60), verified: false });
        this.emit({ type: 'friend.accepted', user: req.user, space: spaceId });
        return { space: spaceId };
      }
      case 'friend.reject':
        this.requests = this.requests.filter((r) => r.request_id !== p.request_id);
        return ok;
      case 'friend.list':
        this.expireContacts();
        // 오프라인인 친구는 몇 시간 전에 마지막으로 동기화한 것으로 보입니다.
        return this.friends.map((f, i) => {
          const online = this.online.get(f.user) ?? false;
          return { ...f, online, ...(!online && { last_seen: Date.now() - (i + 1) * 3_600_000 }) };
        });
      case 'friend.remove': {
        const gone = this.friends.find((f) => f.user === p.user);
        this.friends = this.friends.filter((f) => f.user !== p.user);
        // 데몬처럼 1대1 공간과 그 기록도 지웁니다. 크루로 바꾼 공간은 그대로입니다.
        if (gone && this.spaces.get(gone.space)?.view.kind === 'space') {
          this.spaces.delete(gone.space);
          this.emit({ type: 'space.updated', space: gone.space });
        }
        return ok;
      }
      case 'friend.safety_number': {
        const s = this.safety.get(p.user);
        if (!s) throw new RpcError(ErrorCode.NotFound, '친구가 아닙니다');
        return s;
      }
      case 'friend.verify': {
        const s = this.safety.get(p.user);
        if (!s) throw new RpcError(ErrorCode.NotFound, '친구가 아닙니다');
        s.verified = !!p.verified;
        const f = this.friends.find((x) => x.user === p.user);
        if (f) f.safety_number_verified = s.verified;
        return ok;
      }

      // 신뢰 스위치(데몬처럼 IP는 싣지 않음)
      case 'trust.get': {
        this.requireIdentity();
        const user = String(p.user ?? '');
        return { user, trusted: this.trustedUsers.has(user), via_crews: this.trustViaCrews(user), connection: this.connectionMode(user) };
      }
      case 'trust.set_user': {
        this.requireIdentity();
        const user = String(p.user ?? '');
        if (user === this.me) throw new RpcError(ErrorCode.InvalidInput, '나 자신은 신뢰 대상이 아닙니다');
        // 끄기는 동의를 거두는 일이라 늘 됩니다.
        if (!p.enabled) {
          this.trustedUsers.delete(user);
          return ok;
        }
        if (this.trustedUsers.has(user)) return ok;
        if (!this.sharesActiveSpace(user)) throw new RpcError(ErrorCode.NotFound, '친구·연락처나 같은 크루의 멤버가 아닙니다');
        this.trustedUsers.add(user);
        return ok;
      }
      case 'trust.set_space': {
        const enabled = !!p.enabled;
        if (!enabled) {
          const sp = this.spaces.get(p.space);
          if (sp?.view.trusted) {
            sp.view.trusted = false;
            this.spaceUpdated(sp);
          }
          return ok;
        }
        const sp = this.space(p.space);
        if (sp.view.kind !== 'crew') throw new RpcError(ErrorCode.InvalidInput, '크루만 신뢰할 수 있습니다(1:1 대화는 상대 프로필에서)');
        if (sp.view.trusted) return ok;
        sp.view.trusted = true;
        this.spaceUpdated(sp);
        return ok;
      }

      // 공간
      case 'space.list':
        return [...this.spaces.values()].map((s) => this.summary(s));
      case 'space.get': {
        // 보기만 읽어서는 읽음 처리하지 않습니다(읽음은 channel.mark_read).
        return this.presentView(this.space(p.space));
      }
      case 'crew.create': {
        const name = String(p.name ?? '').trim();
        if (!name) throw new RpcError(ErrorCode.InvalidInput, '이름이 비었습니다');
        const spaceId = this.rng.id(16);
        const general = this.rng.id(16);
        const voice = this.rng.id(16);
        const sp = this.makeSpace(
          spaceId,
          'crew',
          name,
          this.me,
          [{ user: this.me, profile: this.users.get(this.me)!, roles: [], online: true, automation: false }],
          [{ id: EVERYONE_ROLE, name: '@everyone', color: 0, rank: 0, permissions: 1983, automation: false }],
          [
            [general, channel('일반', 'text')],
            [voice, channel('통화', 'voice', { position: 1 })],
          ],
          [],
        );
        sp.view.settings.description = p.description ?? '';
        sp.view.my_permissions = ['administrator'];
        sp.voice.set(voice, []);
        return { space: spaceId };
      }
      case 'crew.set_servers': {
        const sp = this.space(p.space);
        if (sp.view.kind !== 'crew') throw new RpcError(ErrorCode.InvalidInput, '크루에서만 STUN 서버를 정할 수 있습니다');
        this.requirePerm(sp, 'manage_space');
        const stun = Array.isArray(p.stun) ? p.stun : [];
        if (stun.length > 8) throw new RpcError(ErrorCode.InvalidInput, '크루 STUN 서버는 8개까지 둘 수 있음');
        const next: CrewServers = { stun: [] };
        for (const raw of stun) {
          const n = normalizeStun(String(raw));
          if (!n) throw new RpcError(ErrorCode.InvalidInput, 'STUN 서버는 host:port 꼴로 적어야 함(TURN·비밀번호는 안 됨)');
          if (!next.stun.includes(n)) next.stun.push(n);
        }
        sp.servers = next;
        this.spaceUpdated(sp);
        return ok;
      }
      case 'crew.use_servers': {
        const sp = this.space(p.space);
        if (sp.view.kind !== 'crew') throw new RpcError(ErrorCode.InvalidInput, '크루의 서버만 쓸 수 있습니다');
        sp.useServers = !!p.enabled;
        this.spaceUpdated(sp);
        return ok;
      }
      case 'crew.update': {
        const sp = this.space(p.space);
        if (typeof p.name === 'string') sp.view.settings.name = p.name;
        if (typeof p.description === 'string') sp.view.settings.description = p.description;
        if (typeof p.icon_path === 'string') sp.view.settings.icon = p.icon_path ? this.mockImage(33, 128, 128) : null;
        if (typeof p.strict_rotation === 'boolean') sp.view.settings.strict_rotation = p.strict_rotation;
        if (p.emoji_limits) {
          const { max_count, max_bytes } = p.emoji_limits;
          if ((max_count !== null && (max_count < 1 || max_count > 500)) || max_bytes === 0) {
            throw new RpcError(ErrorCode.InvalidInput, '이모지 수 한도는 1~500개, 크기 한도는 1바이트 이상이어야 합니다');
          }
          sp.view.emoji_limits = { max_count, max_bytes };
        }
        this.spaceUpdated(sp);
        return ok;
      }
      case 'crew.convert_request': {
        const sp = this.space(p.space);
        if (sp.view.kind !== 'space') throw new RpcError(ErrorCode.InvalidInput, '1대1 공간만 전환할 수 있습니다');
        sp.view.pending_crew_conversion = this.me;
        sp.view.settings.name = String(p.name ?? sp.view.settings.name);
        this.spaceUpdated(sp);
        // 상대가 몇 초 뒤 동의하는 것처럼 흉내
        setTimeout(() => {
          sp.view.kind = 'crew';
          sp.view.pending_crew_conversion = undefined;
          const admin: Role = { id: this.rng.id(16), name: '관리자', color: 0xf1c40f, rank: 50, permissions: 2 ** 31, automation: false };
          sp.view.roles.push(admin);
          const other = sp.view.members.find((m) => m.user !== this.me);
          if (other) other.roles = [admin.id];
          this.spaceUpdated(sp);
        }, 2500);
        return ok;
      }
      case 'crew.convert_accept': {
        const sp = this.space(p.space);
        sp.view.kind = 'crew';
        sp.view.pending_crew_conversion = undefined;
        this.spaceUpdated(sp);
        return ok;
      }
      case 'crew.leave': {
        const sp = this.space(p.space);
        this.spaces.delete(sp.view.space);
        return ok;
      }
      case 'crew.transfer_ownership': {
        const sp = this.space(p.space);
        sp.view.owner = p.user;
        this.spaceUpdated(sp);
        return ok;
      }
      case 'crew.disband': {
        const sp = this.space(p.space);
        if (sp.view.owner !== this.me) throw new RpcError(ErrorCode.Forbidden, '소유자만 크루를 해산할 수 있습니다');
        this.spaces.delete(sp.view.space);
        return ok;
      }
      case 'crew.committer_candidates': {
        const v = this.space(p.space).view;
        const admin = (m: MemberView) =>
          m.user === v.owner || m.roles.some((r) => permissionNames(v.roles.find((x) => x.id === r)?.permissions ?? 0).includes('administrator'));
        const out: CommitterCandidate[] = v.members
          .map((m) => ({ m, device: m.user === this.me ? this.myDevice : (this.devices.get(m.user) ?? '') }))
          .filter(({ m, device }) => device && (admin(m) || device === v.committer))
          .map(({ m, device }) => ({
            user: m.user,
            device,
            name: m.user === this.me ? '이 컴퓨터' : `${m.profile.name}의 기기`,
            online: m.user === this.me || !!this.online.get(m.user),
            current: device === v.committer,
            mine: m.user === this.me,
          }));
        return out.sort((a, b) => Number(b.current) - Number(a.current) || Number(b.user === v.owner) - Number(a.user === v.owner) || a.name.localeCompare(b.name));
      }
      case 'crew.set_committer': {
        const sp = this.space(p.space);
        if (sp.view.owner !== this.me) throw new RpcError(ErrorCode.Forbidden, '소유자만 확정 기기를 정할 수 있습니다');
        const list = (await this.dispatch('crew.committer_candidates', { space: p.space })) as CommitterCandidate[];
        if (!list.some((c) => c.device === p.device)) throw new RpcError(ErrorCode.InvalidInput, '소유자·관리자의 활성 기기가 아닙니다');
        sp.view.committer = p.device;
        this.spaceUpdated(sp);
        return ok;
      }
      case 'member.list':
        return structuredClone(this.space(p.space).view.members);
      case 'member.search': {
        // 모의 데몬은 연결 기록이 없어 "최근"은 온라인과 같습니다.
        const all = this.space(p.space).view.members;
        const q = String(p.query ?? '').trim().toLowerCase();
        const name = (m: MemberView) => (m.nickname || m.profile.name).toLowerCase();
        const hits = all
          .filter((m) => p.filter !== 'online' && p.filter !== 'recent' ? true : m.online)
          .filter((m) => !q || name(m).includes(q) || m.profile.name.toLowerCase().includes(q) || m.user.toLowerCase().startsWith(q))
          .sort((a, b) => Number(b.online) - Number(a.online) || name(a).localeCompare(name(b)));
        const offset = Number(p.offset ?? 0);
        const limit = Math.min(Math.max(Number(p.limit ?? 100), 1), 200);
        return { members: structuredClone(hits.slice(offset, offset + limit)), total: hits.length, online: all.filter((m) => m.online).length };
      }
      case 'member.kick': {
        const sp = this.space(p.space);
        if (p.user === sp.view.owner) throw new RpcError(ErrorCode.Forbidden, '소유자는 추방할 수 없습니다');
        sp.view.members = sp.view.members.filter((m) => m.user !== p.user);
        this.spaceUpdated(sp);
        return ok;
      }
      case 'member.set_roles': {
        const sp = this.space(p.space);
        const m = sp.view.members.find((x) => x.user === p.user);
        if (!m) throw new RpcError(ErrorCode.NotFound, '멤버가 아닙니다');
        m.roles = (p.roles as string[]).filter((r) => sp.view.roles.some((x) => x.id === r && x.id !== EVERYONE_ROLE));
        this.spaceUpdated(sp);
        return ok;
      }

      // 채널
      case 'channel.list':
        return this.presentView(this.space(p.space)).channels;
      case 'channel.create': {
        const sp = this.space(p.space);
        const name = String(p.name ?? '').trim();
        if (!name || name.length > 100) throw new RpcError(ErrorCode.InvalidInput, '채널 이름이 잘못됐습니다');
        // 데몬처럼 P2P 지정이 안 되는 채널은 만들기 전에 거절합니다(Tor 채널로만 남지 않게).
        if (p.p2p) this.checkP2p(sp, p.kind);
        if (p.anonymous) this.checkAnonymous(sp, p.kind);
        if (p.overwrites !== undefined && p.private_roles !== undefined) throw new RpcError(ErrorCode.InvalidInput, 'overwrites와 private_roles는 함께 줄 수 없습니다');
        const overwrites = p.overwrites !== undefined ? this.checkOverwrites(sp, p.overwrites, []) : privateOverwrites(p.private_roles);
        const forumTags = p.forum_tags !== undefined ? checkForumTags(p.kind, p.forum_tags) : [];
        // 데몬처럼 크루의 휘발성 채널은 따로 정하지 않으면 첨부 10MiB입니다.
        const attachLimit =
          p.attach_limit !== undefined ? p.attach_limit : sp.view.kind === 'crew' && p.security === 'volatile' ? 10 * 1024 * 1024 : null;
        if (attachLimit != null) this.checkAttachLimit(sp, p.kind, attachLimit);
        if (p.no_edit || p.no_delete) this.checkEditRules(sp, p.kind);
        const id = this.rng.id(16);
        const settings = channel(name, p.kind, {
          parent: p.parent ?? null,
          topic: p.topic ?? '',
          security: p.security ?? 'normal',
          join_mode: p.join_mode ?? 'always',
          ttl: p.ttl_on_leave ? { mode: 'on_leave' } : p.ttl_secs ? { mode: 'seconds', seconds: p.ttl_secs } : { mode: 'none' },
          history: p.history ?? 'private',
          link_previews: p.link_previews ?? p.security !== 'volatile',
          position: p.position ?? sp.view.channels.filter((c) => c.settings.parent === (p.parent ?? null)).length,
          overwrites,
          forum_tags: forumTags,
        });
        sp.view.channels.push(
          channelView(id, settings, {
            ...(settings.join_mode === 'join_leave' || p.kind === 'voice' ? { joined: false, participants: [] } : {}),
            // 만든 기기도 따로 참가해야 합니다.
            p2p: !!p.p2p,
            ...(p.anonymous ? { anonymous: true } : {}),
            ...(attachLimit != null ? { attach_limit: attachLimit } : {}),
            ...(p.no_edit ? { no_edit: true } : {}),
            ...(p.no_delete ? { no_delete: true } : {}),
          }),
        );
        if (p.kind === 'text') sp.messages.set(id, []);
        if (p.kind === 'forum') sp.posts.set(id, []);
        if (p.kind === 'voice') sp.voice.set(id, []);
        sp.coverage.set(id, settings.security === 'volatile' ? -1 : 1);
        this.spaceUpdated(sp);
        return { channel: id };
      }
      case 'channel.update': {
        const sp = this.space(p.space);
        const ch = this.channelOf(sp, p.channel);
        const s = ch.settings;
        if (p.overwrites !== undefined && p.private_roles !== undefined) throw new RpcError(ErrorCode.InvalidInput, 'overwrites와 private_roles는 함께 줄 수 없습니다');
        if (p.anonymous === true && !ch.anonymous) this.checkAnonymous(sp, s.kind);
        else if (p.anonymous === false && ch.anonymous) this.requirePerm(sp, 'manage_channels');
        const overwrites = p.overwrites !== undefined ? this.checkOverwrites(sp, p.overwrites, s.overwrites) : undefined;
        if (p.forum_tags !== undefined) this.requirePerm(sp, 'manage_channels');
        const forumTags = p.forum_tags !== undefined ? checkForumTags(s.kind, p.forum_tags) : undefined;
        if (p.attach_limit !== undefined) this.checkAttachLimit(sp, s.kind, p.attach_limit);
        if (p.no_edit !== undefined || p.no_delete !== undefined) this.checkEditRules(sp, s.kind);
        if (typeof p.name === 'string') s.name = p.name;
        if (typeof p.topic === 'string') s.topic = p.topic;
        if ('parent' in p) s.parent = p.parent ?? null;
        // 실제 동기화 규칙과 같게: 보안 등급은 만든 뒤 바꿀 수 없습니다.
        if (p.security && p.security !== s.security) throw new RpcError(ErrorCode.InvalidInput, '채널 종류·보안 등급은 바꿀 수 없습니다');
        if (p.join_mode && p.join_mode !== s.join_mode) {
          s.join_mode = p.join_mode;
          if (p.join_mode === 'join_leave') {
            ch.joined ??= false;
            ch.participants ??= [];
          }
        }
        if (p.ttl_on_leave) s.ttl = { mode: 'on_leave' };
        else if (p.ttl_secs) s.ttl = { mode: 'seconds', seconds: p.ttl_secs };
        else if (p.ttl_secs === 0) s.ttl = { mode: 'none' };
        if (p.history) s.history = p.history;
        if (typeof p.link_previews === 'boolean') s.link_previews = p.link_previews;
        if (Array.isArray(p.private_roles)) s.overwrites = privateOverwrites(p.private_roles);
        if (overwrites) s.overwrites = overwrites;
        if (typeof p.position === 'number') s.position = p.position;
        if (forumTags) s.forum_tags = forumTags;
        if (typeof p.anonymous === 'boolean' && p.anonymous !== !!ch.anonymous) this.setAnonymous(sp, ch, p.anonymous);
        if (p.attach_limit !== undefined) ch.attach_limit = p.attach_limit ?? undefined;
        if (typeof p.no_edit === 'boolean') ch.no_edit = p.no_edit || undefined;
        if (typeof p.no_delete === 'boolean') ch.no_delete = p.no_delete || undefined;
        this.spaceUpdated(sp);
        return ok;
      }
      case 'channel.delete': {
        const sp = this.space(p.space);
        sp.view.channels = sp.view.channels.filter((c) => c.channel !== p.channel && c.settings.parent !== p.channel);
        sp.messages.delete(p.channel);
        this.spaceUpdated(sp);
        return ok;
      }
      case 'channel.join': {
        const sp = this.space(p.space);
        const ch = this.channelOf(sp, p.channel);
        ch.joined = true;
        ch.participants = [...new Set([...(ch.participants ?? []), this.me])];
        if (ch.settings.security === 'volatile') {
          this.joinedVolatile.add(`${sp.view.space}/${ch.channel}`);
          // 과거 기록 공개면 지금 참가자 RAM에 남은 메시지(남은 수명 포함)를 받습니다.
          if (ch.settings.history === 'public') {
            const ttl = ch.settings.ttl.mode === 'seconds' ? ch.settings.ttl.seconds : 60;
            const parts = (ch.participants ?? []).filter((x) => x !== this.me);
            const now = Date.now();
            const list = sp.messages.get(ch.channel) ?? [];
            const snapshot = VOLATILE_LINES.slice(0, 5).map((text, i) => ({
              id: this.rng.id(32),
              author: parts[i % parts.length] ?? this.me,
              author_device: this.myDevice,
              created_at: now - (5 - i) * 7_000,
              lamport: this.lamport++,
              text,
              attachments: i === 3 ? [attachment(this.rng, this.blobs, 900)] : [],
              embeds: [],
              reactions: i === 1 ? [{ emoji: '🔥', count: 2, me: false }] : [],
              deleted: false,
              volatile: true,
              expires_at: now - (5 - i) * 7_000 + ttl * 1000,
            }));
            for (const a of snapshot) for (const att of a.attachments) att.downloaded = true;
            sp.messages.set(ch.channel, [...list, ...snapshot]);
          }
        }
        this.spaceUpdated(sp);
        return ok;
      }
      case 'channel.leave': {
        const sp = this.space(p.space);
        const ch = this.channelOf(sp, p.channel);
        ch.joined = false;
        ch.participants = (ch.participants ?? []).filter((x) => x !== this.me);
        this.joinedVolatile.delete(`${sp.view.space}/${ch.channel}`);
        if (ch.settings.security === 'volatile') {
          // 퇴장하면 이 기기의 채널 내용·키를 즉시 지웁니다.
          const ids = (sp.messages.get(ch.channel) ?? []).map((m) => m.id);
          sp.messages.set(ch.channel, []);
          if (ids.length) this.emit({ type: 'message.expired', space: sp.view.space, channel: ch.channel, ids });
        }
        this.spaceUpdated(sp);
        return ok;
      }
      case 'channel.allow_api_read': {
        const sp = this.space(p.space);
        const ch = this.channelOf(sp, p.channel);
        if (ch.settings.security !== 'volatile') throw new RpcError(ErrorCode.InvalidInput, '휘발성 채널이 아닙니다');
        ch.api_read_allowed = !!p.allowed;
        this.spaceUpdated(sp);
        return ok;
      }
      case 'channel.set_p2p': {
        const sp = this.space(p.space);
        const ch = this.channelOf(sp, p.channel);
        this.requirePerm(sp, 'manage_channels');
        const enabled = !!p.enabled;
        if (enabled) this.checkP2p(sp, ch.settings.kind);
        if (ch.p2p === enabled) return ok;
        // 데몬처럼 어느 쪽으로 바뀌든 이 기기의 참가 기록은 지웁니다(다시 P2P가 되면 경고부터 다시 받음).
        ch.p2p = enabled;
        ch.p2p_joined = false;
        this.spaceUpdated(sp);
        return ok;
      }
      case 'channel.p2p_join': {
        const sp = this.space(p.space);
        const ch = this.channelOf(sp, p.channel);
        if (!ch.p2p) throw new RpcError(ErrorCode.InvalidInput, 'P2P 전용 채널이 아닙니다');
        if (ch.p2p_joined) return ok;
        ch.p2p_joined = true;
        this.spaceUpdated(sp);
        return ok;
      }
      case 'channel.p2p_leave': {
        // 동의를 거두는 일이라 채널이 없어도 늘 됩니다.
        const sp = this.spaces.get(p.space);
        const ch = sp?.view.channels.find((c) => c.channel === p.channel);
        if (!sp || !ch?.p2p_joined) return ok;
        ch.p2p_joined = false;
        this.spaceUpdated(sp);
        return ok;
      }
      case 'channel.mark_read': {
        const sp = this.space(p.space);
        const ch = this.channelOf(sp, p.channel);
        // 1대1 공간은 채널 하나라 공간 전체를 읽음으로. 크루는 그 채널 몫만 뺍니다.
        sp.unread = sp.view.kind === 'space' ? 0 : Math.max(0, sp.unread - ch.unread);
        if (sp.unread === 0) sp.mention = 0;
        ch.unread = 0;
        return ok;
      }

      // 메시지
      case 'message.list': {
        const sp = this.space(p.space);
        const ch = this.channelOf(sp, p.channel);
        if (ch.settings.security === 'volatile' && ch.settings.join_mode === 'join_leave' && !ch.joined) {
          return { messages: [], gaps: [] };
        }
        // 구간 바로 뒤에서 다시 읽으면(앱의 ‘기록 받기’) 데몬의 따라잡기가 시더에게서 그 구간을 받아 온 것처럼 채웁니다.
        if (!p.thread && p.before) await this.catchUpGap(sp, p.channel, p.before);
        const list = p.thread ? sp.threads.get(p.thread) ?? [] : sp.messages.get(p.channel) ?? [];
        const limit = Math.max(1, Math.min(200, Number(p.limit ?? 50)));
        let end = list.length;
        if (p.before) {
          const idx = list.findIndex((m) => m.id === p.before);
          end = idx >= 0 ? idx : list.length;
        }
        const start = Math.max(0, end - limit);
        const page = list.slice(start, end);
        const ids = new Set(page.map((m) => m.id));
        const gaps = (sp.gaps.get(p.channel) ?? []).filter((g) => (g.before && ids.has(g.before)) || (g.after && ids.has(g.after)));
        await delay(page.length > 100 ? 60 : 20);
        // 데몬과 같이 before보다 앞의 가장 최근 limit개를 오래된 것부터 돌려줍니다.
        // 신고하며 숨긴 메시지는 데몬처럼 목록에서 뺍니다.
        const shown = page.filter((m) => !this.reportHidden.has(m.id));
        return { messages: structuredClone(shown).map((m) => this.present(sp, p.channel, this.markBlocked(m))), gaps };
      }
      case 'message.send': {
        const sp = this.space(p.space);
        const ch = this.channelOf(sp, p.channel);
        const text = String(p.text ?? '');
        const attachments = Array.isArray(p.attachments) ? p.attachments : [];
        if (!text.trim() && attachments.length === 0) throw new RpcError(ErrorCode.InvalidInput, '빈 메시지는 보낼 수 없습니다');
        if (text.length > 4000) throw new RpcError(ErrorCode.InvalidInput, '메시지는 4000자까지 보낼 수 있습니다');
        if (ch.settings.join_mode === 'join_leave' && !ch.joined) throw new RpcError(ErrorCode.Forbidden, '채널에 입장해야 보낼 수 있습니다');
        if (this.friends.some((f) => f.space === sp.view.space && f.kind === 'expired')) {
          throw new RpcError(ErrorCode.Forbidden, '만료된 연락처에는 메시지를 보낼 수 없습니다');
        }
        if (attachments.length > 0 && ch.attach_limit === 0) throw new RpcError(ErrorCode.InvalidInput, '이 채널은 첨부를 받지 않습니다');
        const atts = this.mockAttachments(attachments);
        const embeds =
          ch.settings.link_previews && /https?:\/\/(www\.)?youtube\.com\//.test(text)
            ? [
                {
                  url: text.match(/https?:\/\/\S+/)?.[0] ?? '',
                  title: '동영상',
                  description: '보내는 쪽이 만든 미리보기',
                  site_name: 'YouTube',
                  player_url: 'https://www.youtube-nocookie.com/embed/dQw4w9WgXcQ',
                },
              ]
            : [];
        const m = this.postAs(sp, ch.channel, this.me, text, { attachments: atts, embeds, reply_to: p.reply_to, thread: p.thread });
        if (!m) throw new RpcError(ErrorCode.NotFound, '보낼 곳이 없습니다');
        return { id: m.id };
      }
      case 'message.edit': {
        const sp = this.space(p.space);
        if (this.channelOf(sp, p.channel).no_edit) throw new RpcError(ErrorCode.Forbidden, '이 채널은 메시지 수정을 막아 두었습니다');
        const { list, idx } = this.findMessage(sp, p.channel, p.id);
        if (list[idx].author !== this.me) throw new RpcError(ErrorCode.Forbidden, '내 메시지만 수정할 수 있습니다');
        list[idx] = { ...list[idx], text: String(p.text ?? ''), edited_at: Date.now() };
        this.updated(sp, p.channel, list[idx]);
        return ok;
      }
      case 'message.delete': {
        const sp = this.space(p.space);
        if (this.channelOf(sp, p.channel).no_delete && !sp.view.my_permissions.includes('manage_messages') && !sp.view.my_permissions.includes('administrator')) {
          throw new RpcError(ErrorCode.Forbidden, '이 채널은 메시지 삭제를 막아 두었습니다');
        }
        const { list, idx } = this.findMessage(sp, p.channel, p.id);
        list[idx] = { ...list[idx], text: '', attachments: [], embeds: [], reactions: [], deleted: true };
        this.updated(sp, p.channel, list[idx]);
        return ok;
      }
      case 'message.react':
      case 'message.unreact': {
        const sp = this.space(p.space);
        const { list, idx } = this.findMessage(sp, p.channel, p.id);
        const m = { ...list[idx], reactions: list[idx].reactions.map((r) => ({ ...r })) };
        const r = m.reactions.find((x) => x.emoji === p.emoji);
        if (method === 'message.react') {
          if (r && !r.me) {
            r.me = true;
            r.count++;
          } else if (!r) m.reactions.push({ emoji: p.emoji, count: 1, me: true });
        } else if (r?.me) {
          r.me = false;
          r.count--;
          if (r.count <= 0) m.reactions = m.reactions.filter((x) => x !== r);
        }
        list[idx] = m;
        this.updated(sp, p.channel, m);
        return ok;
      }
      case 'message.search': {
        // 데몬처럼: 낱말이 모두 든 글(큰따옴표는 구절), 최신순, 쪽 나누기, 끝을 넘으면 마지막 쪽.
        const terms = searchTerms(String(p.query ?? ''));
        const empty = { messages: [] as MessageView[], total: 0, offset: 0, took_ms: 0 };
        if (terms.length === 0 && !p.author) return empty;
        const hits: MessageView[] = [];
        for (const sp of this.spaces.values()) {
          if (p.space && sp.view.space !== p.space) continue;
          for (const [cid, list] of sp.messages) {
            if (p.channel && cid !== p.channel) continue;
            const ch = sp.view.channels.find((c) => c.channel === cid);
            if (ch?.settings.security === 'volatile') continue; // 일반 등급만
            for (const m of list) {
              const text = m.text.toLowerCase().replace(/\s+/g, ' ');
              if (m.deleted || !terms.every((t) => text.includes(t))) continue;
              const v = { ...this.present(sp, cid, m), space: sp.view.space, channel: cid };
              // 작성자로 찾을 때 익명 글(가려진 작성자)은 뺍니다(데몬과 같음).
              if (p.author && v.author !== p.author) continue;
              hits.push(v);
            }
          }
        }
        hits.sort((a, b) => b.created_at - a.created_at || (a.id < b.id ? 1 : a.id > b.id ? -1 : 0));
        const limit = Math.min(500, Math.max(1, Number(p.limit ?? 50)));
        let offset = Math.max(0, Number(p.offset ?? 0));
        if (offset >= hits.length) offset = Math.floor(Math.max(0, hits.length - 1) / limit) * limit;
        return { messages: hits.slice(offset, offset + limit), total: hits.length, offset, took_ms: 0 };
      }
      case 'forum.create_post': {
        const sp = this.space(p.space);
        const ch = this.channelOf(sp, p.channel);
        const posts = sp.posts.get(p.channel);
        if (!posts) throw new RpcError(ErrorCode.NotFound, '포럼이 아닙니다');
        const text = String(p.text ?? '');
        const attachments: AttachmentInput[] = Array.isArray(p.attachments) ? p.attachments : [];
        if (!text.trim() && attachments.length === 0) throw new RpcError(ErrorCode.InvalidInput, '빈 게시글입니다');
        if (text.length > 4000) throw new RpcError(ErrorCode.InvalidInput, '본문은 4000자까지 쓸 수 있습니다');
        // 데몬처럼 첨부·대문 권한은 글을 만들기 전에 봅니다.
        if (attachments.length > 0) this.requirePerm(sp, 'attach_files');
        if (attachments.length > 0 && ch.attach_limit === 0) throw new RpcError(ErrorCode.InvalidInput, '이 채널은 첨부를 받지 않습니다');
        if (p.featured) this.requirePerm(sp, 'manage_channels');
        const now = Date.now();
        const post: ForumPostView = {
          id: this.rng.id(32),
          title: String(p.title ?? '').trim() || '제목 없음',
          tags: p.tags ?? [],
          author: this.me,
          created_at: now,
          last_activity: now,
          reply_count: 0,
          pinned: false,
          locked: false,
          preview: '',
          attachment_count: 0,
          ...(ch.anonymous ? { anonymous: true } : {}),
        };
        posts.push(post);
        // 데몬처럼 게시글 자체가 글타래의 첫 메시지(ID가 게시글 ID, 글타래 없음)입니다.
        const head: MessageView = {
          id: post.id,
          author: this.me,
          author_device: this.myDevice,
          created_at: now,
          lamport: this.lamport++,
          text,
          attachments: this.mockAttachments(attachments),
          embeds: [],
          reactions: [],
          deleted: false,
          volatile: false,
          ...(ch.anonymous ? { anonymous: true } : {}),
        };
        sp.threads.set(post.id, [head]);
        this.emit({
          type: 'message.new',
          space: sp.view.space,
          channel: p.channel,
          message: this.present(sp, p.channel, head),
          notify: { title: `${ch.anonymous ? '익명' : this.users.get(this.me)?.name ?? ''} (${sp.view.settings.name} #${ch.settings.name})`, body: post.title },
        });
        if (p.featured) {
          ch.featured_post = post.id;
          this.spaceUpdated(sp);
        }
        return { id: post.id };
      }
      case 'forum.list_posts': {
        const sp = this.space(p.space);
        const tags: number[] = Array.isArray(p.tags) ? [...p.tags] : [];
        if (p.tag != null && !tags.includes(p.tag)) tags.push(p.tag);
        // 지운 게시글은 데몬처럼 목록에서 뺍니다.
        let posts = (sp.posts.get(p.channel) ?? []).filter((x) => this.postHead(sp, x.id));
        if (tags.length > 0) {
          // 모두 일치는 데몬처럼 글의 태그가 고른 태그와 꼭 같은 글만.
          posts = posts.filter((x) =>
            p.tag_match === 'all'
              ? tags.every((t) => x.tags.includes(t)) && x.tags.every((t) => tags.includes(t))
              : tags.some((t) => x.tags.includes(t)),
          );
        }
        const key = p.sort === 'created' ? 'created_at' : 'last_activity';
        const dir = p.order === 'asc' ? -1 : 1;
        posts.sort((a, b) => Number(b.pinned) - Number(a.pinned) || dir * (b[key] - a[key]));
        return posts.slice(0, 200).map((x) => this.presentPost(sp, p.channel, x));
      }
      case 'forum.get_post': {
        const sp = this.space(p.space);
        const post = sp.posts.get(p.channel)?.find((x) => x.id === p.post);
        if (!post || !this.postHead(sp, post.id)) throw new RpcError(ErrorCode.NotFound, '없는 게시글');
        return this.presentPost(sp, p.channel, post);
      }
      case 'forum.set_featured': {
        const sp = this.space(p.space);
        const ch = this.channelOf(sp, p.channel);
        const posts = sp.posts.get(p.channel);
        if (!posts) throw new RpcError(ErrorCode.InvalidInput, '포럼 채널이 아닙니다');
        this.requirePerm(sp, 'manage_channels');
        const post = p.post == null ? undefined : String(p.post);
        if (post && (!posts.some((x) => x.id === post) || !this.postHead(sp, post))) throw new RpcError(ErrorCode.NotFound, '없는 게시글');
        if (ch.featured_post === post) return ok;
        if (post) ch.featured_post = post;
        else delete ch.featured_post;
        this.spaceUpdated(sp);
        return ok;
      }
      case 'forum.update_post': {
        const sp = this.space(p.space);
        const post = sp.posts.get(p.channel)?.find((x) => x.id === p.post);
        if (!post) throw new RpcError(ErrorCode.NotFound, '게시글을 찾을 수 없습니다');
        const manage = sp.view.my_permissions.includes('manage_messages') || sp.view.my_permissions.includes('administrator');
        if ((p.title !== undefined || p.tags !== undefined) && this.channelOf(sp, p.channel).no_edit && !manage) {
          throw new RpcError(ErrorCode.Forbidden, '이 채널은 메시지 수정을 막아 두었습니다');
        }
        if (typeof p.title === 'string') post.title = p.title;
        if (Array.isArray(p.tags)) post.tags = p.tags;
        if (typeof p.pinned === 'boolean') post.pinned = p.pinned;
        if (typeof p.locked === 'boolean') post.locked = p.locked;
        // 데몬은 바뀐 게시글을 message.updated로 알립니다.
        const head = sp.threads.get(post.id)?.find((m) => m.id === post.id);
        if (head) this.updated(sp, p.channel, head);
        this.spaceUpdated(sp);
        return ok;
      }

      // 블롭·이모지
      case 'blob.status': {
        const prog = this.blobProgress.get(p.blob);
        const total = prog?.total ?? 4;
        const have = this.evicted.has(p.blob) ? 0 : (prog?.have ?? total);
        return { have_chunks: have, total_chunks: total, seeders_online: 2, size: 0, downloading: !!prog && prog.have < prog.total };
      }
      case 'blob.fetch': {
        const sp = this.space(p.space);
        this.evicted.delete(p.blob);
        const total = 8;
        for (let i = 1; i <= total; i++) {
          setTimeout(() => {
            this.blobProgress.set(p.blob, { have: i, total });
            this.emit({ type: 'blob.progress', space: sp.view.space, blob: p.blob, have: i, total });
            if (i === total) {
              for (const [cid, list] of sp.messages) {
                list.forEach((m, k) => {
                  if (m.attachments.some((a) => a.blob === p.blob)) {
                    list[k] = { ...m, attachments: m.attachments.map((a) => (a.blob === p.blob ? { ...a, downloaded: true } : a)) };
                    this.updated(sp, cid, list[k]);
                  }
                });
              }
            }
          }, i * 180);
        }
        return ok;
      }
      case 'blob.read': {
        const bytes = await this.blobBytes(p.blob);
        return { data_base64: bytesToB64(bytes) };
      }
      case 'blob.save':
        if (this.volatileBlob(p.blob) && p.allow_volatile !== true) {
          throw new RpcError(ErrorCode.Conflict, '휘발성 첨부입니다. 저장한 파일은 메시지가 사라져도 남습니다. 확인했으면 allow_volatile을 켜고 다시 요청하세요');
        }
        return ok;
      case 'blob.cache_status':
        return structuredClone(this.cache);
      case 'blob.cache_set_limit': {
        const c = this.cache;
        if (p.reset && (p.limit_bytes !== undefined || p.min_free_bytes !== undefined)) {
          throw new RpcError(ErrorCode.InvalidInput, 'reset과 값을 함께 줄 수 없습니다');
        }
        if (p.reset) {
          c.limit_bytes = c.default_limit_bytes;
          c.min_free_bytes = c.default_min_free_bytes;
        }
        if (typeof p.limit_bytes === 'number') c.limit_bytes = p.limit_bytes;
        if (typeof p.min_free_bytes === 'number') c.min_free_bytes = p.min_free_bytes;
        // 한도를 넘으면 넘는 만큼 정리한 것으로 칩니다.
        const over = Math.max(0, c.cache_bytes - c.limit_bytes);
        if (over > 0) {
          const n = Math.max(1, Math.round((over / Math.max(1, c.cache_bytes)) * c.cache_count));
          c.cache_bytes -= over;
          c.cache_count = Math.max(0, c.cache_count - n);
          c.evicted_bytes += over;
          c.evicted_count += n;
        }
        return structuredClone(c);
      }
      case 'blob.cache_clear': {
        const c = this.cache;
        const r = { evicted_count: c.cache_count, freed_bytes: c.cache_bytes };
        c.evicted_count += c.cache_count;
        c.evicted_bytes += c.cache_bytes;
        c.cache_count = 0;
        c.cache_bytes = 0;
        return r;
      }
      case 'blob.pin': {
        const sp = this.space(p.space);
        if (!this.pins.some((x) => x.blob === p.blob)) {
          this.pins.unshift({ space: sp.view.space, blob: p.blob, pinned_at: Date.now(), size: 0, downloaded: true });
          this.cache.pinned_count = this.pins.length;
        }
        return ok;
      }
      case 'blob.unpin': {
        const before = this.pins.length;
        this.pins = this.pins.filter((x) => !(x.blob === p.blob && x.space === p.space));
        if (this.pins.length === before) throw new RpcError(ErrorCode.NotFound, '보관하지 않은 블롭');
        this.cache.pinned_count = this.pins.length;
        return ok;
      }
      case 'blob.pins':
        return structuredClone(this.pins.filter((x) => !p.space || x.space === p.space));
      case 'blob.evict': {
        const sp = this.space(p.space);
        let found = false;
        for (const [cid, list] of sp.messages) {
          list.forEach((m, i) => {
            if (!m.attachments.some((a) => a.blob === p.blob)) return;
            found = true;
            list[i] = { ...m, attachments: m.attachments.map((a) => (a.blob === p.blob ? { ...a, downloaded: false } : a)) };
            this.updated(sp, cid, list[i]);
          });
        }
        if (!found) throw new RpcError(ErrorCode.NotFound, '이 공간에서 쓰이지 않는 블롭');
        this.blobProgress.delete(p.blob);
        this.evicted.add(p.blob);
        this.pins = this.pins.filter((x) => x.blob !== p.blob);
        return ok;
      }
      case 'library.list': {
        // 데몬처럼: 일반 등급 채널에서 첨부를 실은 글, 최신순 쪽(목록은 썸네일 없이, 고른 글만 썸네일과 함께).
        const pick = Array.isArray(p.messages) ? new Set<string>(p.messages.slice(0, 200)) : null;
        const all: MessageView[] = [];
        for (const sp of this.spaces.values()) {
          if (p.space && sp.view.space !== p.space) continue;
          for (const [cid, list] of sp.messages) {
            const ch = sp.view.channels.find((c) => c.channel === cid);
            if (!ch || ch.settings.security === 'volatile' || (ch.p2p && !ch.p2p_joined)) continue;
            for (const m of list) {
              if (m.deleted || m.attachments.length === 0 || (pick && !pick.has(m.id))) continue;
              all.push({ ...this.present(sp, cid, m), space: sp.view.space, channel: cid });
            }
          }
        }
        all.sort((a, b) => b.created_at - a.created_at || (a.id < b.id ? 1 : a.id > b.id ? -1 : 0));
        const limit = Math.min(500, Math.max(1, Number(p.limit ?? 500)));
        const offset = Math.max(0, Number(p.offset ?? 0));
        const page = pick ? all : all.slice(offset, offset + limit);
        const excluded = new Set(this.seed.excluded_events);
        return {
          items: page.map((m) => ({
            message: pick ? m : { ...m, attachments: m.attachments.map(({ thumbnail_base64: _t, ...a }) => a) },
            received_at: m.created_at,
            excluded: excluded.has(m.id),
          })),
          total: all.length,
        };
      }
      case 'blob.relink': {
        const sp = this.space(p.space);
        const ch = this.channelOf(sp, p.channel);
        if (ch.settings.security === 'volatile') throw new RpcError(ErrorCode.InvalidInput, '휘발성 채널의 첨부는 다시 이을 수 없습니다');
        const { list, idx } = this.findMessage(sp, p.channel, p.message);
        const m = list[idx];
        const att = m.attachments.find((a) => a.blob === p.blob);
        if (!att) throw new RpcError(ErrorCode.NotFound, '이 메시지에 그런 첨부가 없습니다');
        if (typeof p.path !== 'string' || !p.path) throw new RpcError(ErrorCode.InvalidParams, 'path가 필요합니다');
        // 파일 내용이 없는 흉내라 첨부와 이름이 같은 파일만 '해시가 같다'고 봅니다.
        const name = p.path.split(/[\\/]/).pop();
        if (name !== att.name) throw new RpcError(ErrorCode.InvalidInput, '파일 내용이 첨부와 달라 다시 이을 수 없습니다(해시가 다름)');
        const total = 8;
        this.blobProgress.set(p.blob, { have: total, total });
        list[idx] = { ...m, attachments: m.attachments.map((a) => (a.blob === p.blob ? { ...a, downloaded: true } : a)) };
        if (!this.pins.some((x) => x.blob === p.blob)) this.pins.unshift({ space: sp.view.space, blob: p.blob, pinned_at: Date.now(), size: att.size, downloaded: true });
        this.emit({ type: 'blob.progress', space: sp.view.space, blob: p.blob, have: total, total });
        this.updated(sp, p.channel, list[idx]);
        return { have_chunks: total, total_chunks: total, seeders_online: 2, size: att.size, downloading: false, pinned: true };
      }
      case 'transfer.rates':
        return this.transferRates();
      case 'rate.limits':
        return [...this.rateLimits.values()].map((l) => structuredClone(l));
      case 'rate.set_limit': {
        const scope = p.scope as RateScope | undefined;
        if (!scope || !['crew', 'channel', 'user'].includes(scope.kind)) throw new RpcError(ErrorCode.InvalidParams, 'scope가 잘못됐습니다');
        this.setRateLimit(structuredClone(scope), p.up_bps ?? null, p.down_bps ?? null);
        return ok;
      }
      case 'emoji.list':
        return this.space(p.space).view.emojis;
      case 'emoji.create': {
        const sp = this.space(p.space);
        const name = String(p.name ?? '');
        if (!/^[\p{L}\p{N}_]{2,32}$/u.test(name)) throw new RpcError(ErrorCode.InvalidInput, '이모지 이름은 2~32자 글자·숫자·밑줄');
        if (!/\.(png|gif|webp)$/i.test(String(p.path ?? ''))) throw new RpcError(ErrorCode.InvalidInput, 'png·gif·webp만 지원합니다');
        const max = Math.min(500, sp.view.emoji_limits?.max_count ?? 500);
        if (sp.view.emojis.length >= max) throw new RpcError(ErrorCode.InvalidInput, `이모지는 ${max}개까지입니다`);
        const animated = /\.gif$/i.test(p.path);
        const blob = this.rng.id(32);
        this.blobs.set(blob, { svg: EMOJI_SVGS.yangpa, mime: 'image/svg+xml' });
        const e: EmojiView = { id: this.rng.digits(18).replace(/^0+/, '1'), name, animated, blob, mime: 'image/svg+xml', created_by: this.me };
        sp.view.emojis.push(e);
        this.spaceUpdated(sp);
        return { id: e.id, animated };
      }
      case 'emoji.delete': {
        const sp = this.space(p.space);
        sp.view.emojis = sp.view.emojis.filter((e) => e.id !== p.id);
        this.spaceUpdated(sp);
        return ok;
      }

      // 역할·초대
      case 'role.list':
        return this.space(p.space).view.roles;
      case 'role.create': {
        const sp = this.space(p.space);
        const role: Role = {
          id: this.rng.id(16),
          name: String(p.name ?? '새 역할'),
          color: p.color ?? 0x99aab5,
          rank: p.rank ?? 1,
          permissions: permsFromNames(p.permissions ?? []),
          automation: false,
        };
        sp.view.roles.push(role);
        this.spaceUpdated(sp);
        return { role: role.id };
      }
      case 'role.update': {
        const sp = this.space(p.space);
        const r = sp.view.roles.find((x) => x.id === p.role);
        if (!r) throw new RpcError(ErrorCode.NotFound, '역할을 찾을 수 없습니다');
        if (p.visible_to !== undefined || p.see_admins !== undefined) this.setRoleDisplay(sp, r.id, p);
        if (typeof p.name === 'string' && r.id !== EVERYONE_ROLE) r.name = p.name;
        if (typeof p.color === 'number') r.color = p.color;
        if (Array.isArray(p.permissions)) r.permissions = permsFromNames(p.permissions);
        if (typeof p.rank === 'number' && r.id !== EVERYONE_ROLE) r.rank = p.rank;
        this.spaceUpdated(sp);
        return ok;
      }
      case 'role.delete': {
        const sp = this.space(p.space);
        if (p.role === EVERYONE_ROLE) throw new RpcError(ErrorCode.InvalidInput, '@everyone은 지울 수 없습니다');
        sp.view.roles = sp.view.roles.filter((r) => r.id !== p.role);
        for (const m of sp.view.members) m.roles = m.roles.filter((r) => r !== p.role);
        if (sp.view.role_display) delete sp.view.role_display[p.role];
        // 데몬처럼 지운 역할을 대상으로 한 채널 덮어쓰기도 지웁니다.
        for (const c of sp.view.channels) c.settings.overwrites = c.settings.overwrites.filter((o) => !(o.target.type === 'role' && o.target.id === p.role));
        this.spaceUpdated(sp);
        return ok;
      }
      case 'invite.create': {
        const sp = this.space(p.space);
        const now = Date.now();
        sp.invites.push({
          token_hash: Array.from({ length: 64 }, () => '0123456789abcdef'[this.rng.int(0, 15)]).join(''),
          expires_at: p.expires_in_secs ? now + p.expires_in_secs * 1000 : undefined,
          max_uses: p.max_uses,
          uses: 0,
          roles: p.roles ?? [],
          created_by: this.me,
        });
        return { url: `${WEB_BASE}#invite/${this.rng.id(32)}${this.rng.id(32)}` };
      }
      case 'invite.list':
        return this.space(p.space).invites;
      case 'invite.revoke': {
        const sp = this.space(p.space);
        sp.invites = sp.invites.filter((i) => i.token_hash !== p.token_hash);
        return ok;
      }
      case 'invite.join': {
        const url = String(p.url ?? '').trim();
        if (appLinkKind(url) !== 'invite') throw new RpcError(ErrorCode.InvalidInput, '초대 링크 형식이 아닙니다');
        const crew = this.crew();
        return { space: crew?.view.space ?? '', status: 'pending' };
      }

      // 시더·피어
      case 'seed.get':
        return this.seed;
      case 'seed.set': {
        const s = this.seed;
        if (typeof p.enabled === 'boolean') s.enabled = p.enabled;
        if (Array.isArray(p.types)) s.types = p.types;
        if ('channels' in p) s.include_channels = p.channels;
        if (typeof p.all_channels === 'boolean') s.all_channels = p.all_channels;
        if (Array.isArray(p.exclude_channels)) s.exclude_channels = p.exclude_channels;
        if ('upload_limit_bps' in p) s.upload_limit_bps = p.upload_limit_bps;
        if (typeof p.max_concurrent === 'number') s.max_concurrent = p.max_concurrent;
        if (typeof p.prefetch === 'boolean') s.prefetch = p.prefetch;
        return ok;
      }
      case 'seed.exclude_message': {
        const s = this.seed;
        s.excluded_events = p.excluded ? [...new Set([...s.excluded_events, p.id])] : s.excluded_events.filter((x) => x !== p.id);
        return ok;
      }
      case 'peer.get':
        return this.peer;
      case 'peer.set': {
        const s = this.peer;
        if (Array.isArray(p.types)) s.types = p.types;
        if (p.priorities) s.priorities = { ...s.priorities, ...p.priorities };
        if ('download_limit_bps' in p) s.download_limit_bps = p.download_limit_bps;
        if (typeof p.auto_download_max_bytes === 'number') s.auto_download_max_bytes = p.auto_download_max_bytes;
        return ok;
      }
      case 'seed.coverage': {
        const sp = this.space(p.space);
        const out: ChannelCoverage[] = [];
        const archives = sp.archives.filter((a) => a.online).length;
        for (const [channel, n] of sp.coverage) if (n >= 0) out.push({ channel, online_seeders: n, online_archives: archives });
        return out;
      }

      // 보관 전용 노드
      case 'archive.offer': {
        const secs = typeof p.expires_in_secs === 'number' ? p.expires_in_secs : 24 * 3600;
        return { url: `${WEB_BASE}#archive/${this.rng.id(32)}`, expires_at: Date.now() + secs * 1000 };
      }
      case 'archive.hosted':
        return [];
      case 'archive.drop':
        throw new RpcError(ErrorCode.NotFound, '받아 둔 공간이 아닙니다');
      case 'archive.designate': {
        const sp = this.space(p.space);
        const url = String(p.url ?? '').trim();
        if (appLinkKind(url) !== 'archive') throw new RpcError(ErrorCode.InvalidInput, '보관 노드 링크 형식이 아닙니다');
        const name = typeof p.name === 'string' && p.name.trim() ? p.name.trim() : '보관 노드';
        const view: ArchiveView = {
          device: this.rng.id(32),
          user: this.rng.id(32),
          name,
          private_channels: Array.isArray(p.private_channels) ? p.private_channels : [],
          granted_by: this.me,
          granted_at: Date.now(),
          online: true,
        };
        sp.archives.push(view);
        return view;
      }
      case 'archive.revoke': {
        const sp = this.space(p.space);
        const before = sp.archives.length;
        sp.archives = sp.archives.filter((a) => a.device !== p.device);
        if (sp.archives.length === before) throw new RpcError(ErrorCode.NotFound, '지정된 보관 노드가 아닙니다');
        return ok;
      }
      case 'archive.list':
        return this.space(p.space).archives;

      // 네트워크
      case 'net.status':
        return this.net;
      case 'net.stun_servers':
        return { servers: [...(this.stun ?? MOCK_DEFAULT_STUN)], default: this.stun === null };
      case 'net.set_stun_servers': {
        if (p.servers === null || p.servers === undefined) {
          this.stun = null;
          return ok;
        }
        const list = Array.isArray(p.servers) ? p.servers : [];
        if (list.length > 8) throw new RpcError(ErrorCode.InvalidInput, 'STUN 서버는 8개까지 둘 수 있음');
        const out: string[] = [];
        for (const raw of list) {
          const n = normalizeStun(String(raw));
          if (!n) throw new RpcError(ErrorCode.InvalidInput, 'STUN 서버는 host:port 꼴로 적어야 함(TURN·비밀번호는 안 됨)');
          if (!out.includes(n)) out.push(n);
        }
        this.stun = out;
        return ok;
      }
      case 'net.file_circuits':
        return { enabled: this.fileCircuits > 0, count: this.fileCircuits };
      case 'net.set_file_circuits': {
        const n = p.count ?? (p.enabled ? 4 : 0);
        if (!Number.isInteger(n) || n < 0 || n > 4) throw new RpcError(ErrorCode.InvalidInput, '파일 회로는 0~4개입니다');
        this.fileCircuits = n;
        return ok;
      }
      case 'net.bridges':
        return { ...this.bridges, custom: [...this.bridges.custom], available: true };
      case 'net.set_bridges': {
        const custom = ((p.custom ?? []) as string[]).map((l) => l.trim()).filter(Boolean);
        if (custom.length > 32) throw new RpcError(ErrorCode.InvalidInput, '브리지 줄은 32줄까지 넣을 수 있습니다');
        // 흉내: 전송 이름·주소·지문이 있어야 브리지 줄로 봅니다.
        if (custom.some((l) => !/^(?:(obfs4|webtunnel|snowflake|meek_lite) )?\S+:\d+ [0-9A-Fa-f]{40}\b/.test(l)))
          throw new RpcError(ErrorCode.InvalidInput, '읽을 수 없는 브리지 줄이 있습니다');
        if (p.enabled && p.kind === 'custom' && !custom.length) throw new RpcError(ErrorCode.InvalidInput, '브리지 줄을 하나 이상 넣어 주세요');
        this.bridges = { enabled: !!p.enabled, kind: p.kind ?? 'snowflake', custom };
        return ok;
      }
      case 'net.channel_topics':
        return { enabled: this.channelTopics };
      case 'net.set_channel_topics':
        this.channelTopics = !!p.enabled;
        return ok;
      case 'net.peers':
        return this.friends.filter((f) => f.kind !== 'expired').map((f, i) => ({ peer_id: `12D3KooW${f.user.slice(0, 20)}`, user: f.user, rtt_ms: 30 + i * 25, relayed: i % 2 === 1 }));

      // 음성
      case 'voice.join': {
        const sp = this.space(p.space);
        const ch = this.channelOf(sp, p.channel);
        if (ch.settings.kind !== 'voice') throw new RpcError(ErrorCode.InvalidInput, '통화 채널이 아닙니다');
        if (ch.p2p && !ch.p2p_joined) throw new RpcError(ErrorCode.Forbidden, 'P2P 전용 채널에 먼저 참가해야 통화할 수 있습니다');
        const list = sp.voice.get(ch.channel) ?? [];
        if (!list.some((x) => x.user === this.me)) list.push({ user: this.me, device: this.myDevice, muted: false, deafened: false, screen: false });
        sp.voice.set(ch.channel, list);
        ch.participants = list.map((x) => x.user);
        const generation = 3;
        const media_key = bytesToB64(crypto.getRandomValues(new Uint8Array(32)));
        this.spaceUpdated(sp);
        this.emitRoster(sp, ch.channel, list, generation, media_key);
        return {
          call: this.rng.id(16),
          participants: this.presentVoice(sp, ch.channel, list),
          media_key,
          generation,
          ...this.mockTopology(sp, ch.channel, list),
          // 데몬은 STUN 서버를 내주지 않습니다.
          ice_servers: [],
        };
      }
      case 'voice.leave': {
        const sp = this.space(p.space);
        const ch = this.channelOf(sp, p.channel);
        const list = (sp.voice.get(ch.channel) ?? []).filter((x) => x.user !== this.me);
        sp.voice.set(ch.channel, list);
        // 통화가 비면 익명 번호를 처음부터 다시 셉니다.
        if (list.length === 0) sp.voiceAnon.delete(ch.channel);
        ch.participants = list.map((x) => x.user);
        this.spaceUpdated(sp);
        return ok;
      }
      case 'voice.state': {
        const sp = this.space(p.space);
        const list = sp.voice.get(p.channel) ?? [];
        const me = list.find((x) => x.user === this.me);
        if (me) Object.assign(me, { muted: !!p.muted, deafened: !!p.deafened, screen: !!p.screen });
        this.emitRoster(sp, p.channel, list);
        return ok;
      }
      case 'voice.signal':
      case 'voice.media':
      case 'voice.watch':
        return ok;

      // 토큰·감사·확인
      case 'token.create': {
        const name = String(p.name ?? '').trim();
        if (!name) throw new RpcError(ErrorCode.InvalidInput, '이름이 비었습니다');
        if (!Array.isArray(p.scopes) || p.scopes.length === 0) throw new RpcError(ErrorCode.InvalidInput, '범위를 하나 이상 고르세요');
        const id = 'tk-' + this.rng.id(16).slice(0, 10);
        this.tokens.push({ id, name, scopes: p.scopes, spaces: p.spaces ?? [], automation: !!p.automation, created_at: Date.now() });
        return { token: 'yp_' + this.rng.id(32) + this.rng.id(16), id };
      }
      case 'token.list':
        return this.tokens;
      case 'token.revoke':
        this.tokens = this.tokens.filter((t) => t.id !== p.id);
        return ok;
      case 'audit.list': {
        const limit = Math.min(500, Number(p.limit ?? 100));
        const list = [...this.audit].reverse().filter((a) => p.before == null || a.id < p.before);
        return list.slice(0, limit);
      }
      case 'confirm.list':
        return this.confirmations;
      case 'confirm.approve': {
        const c = this.confirmations.find((x) => x.id === p.id);
        if (!c) throw new RpcError(ErrorCode.NotFound, '확인 요청을 찾을 수 없습니다');
        this.confirmations = this.confirmations.filter((x) => x !== c);
        const result = await this.dispatch(c.method, c.params as Record<string, any>);
        return { result };
      }
      case 'confirm.deny':
        this.confirmations = this.confirmations.filter((x) => x.id !== p.id);
        return ok;
      case 'channel.typing':
        return ok;
      case 'blob.paste': {
        const bytes = typeof p.data_base64 === 'string' ? Math.floor((p.data_base64.length * 3) / 4) : 0;
        if (!p.paste) {
          if (!p.size) throw new RpcError(ErrorCode.InvalidInput, '처음 붙여넣을 때는 전체 크기(size)가 필요합니다');
          const id = this.rng.id(16);
          this.pastes.set(id, bytes);
          return { paste: id, received: bytes };
        }
        const have = this.pastes.get(p.paste);
        if (have === undefined) throw new RpcError(ErrorCode.NotFound, '붙여넣기가 없거나 만료되었습니다');
        this.pastes.set(p.paste, have + bytes);
        return { paste: p.paste, received: have + bytes };
      }
      case 'blob.paste_discard':
        this.pastes.delete(p.paste);
        return ok;
      case 'events.subscribe':
        return { subscription: 1 };
      case 'events.unsubscribe':
        return ok;

      // 모더레이션
      case 'member.ban': {
        const sp = this.space(p.space);
        this.requirePerm(sp, 'ban');
        if (p.user === this.me) throw new RpcError(ErrorCode.InvalidInput, '자기 자신은 밴할 수 없습니다(crew.leave로 나갑니다)');
        if (p.user === sp.view.owner) throw new RpcError(ErrorCode.Forbidden, '소유자는 밴할 수 없습니다');
        const member = sp.view.members.find((m) => m.user === p.user);
        const bans = this.bans.get(sp.view.space) ?? [];
        if (bans.some((b) => b.user === p.user)) throw new RpcError(ErrorCode.Conflict, '이미 밴된 사용자입니다');
        if (rankOf(sp.view, this.me) <= rankOf(sp.view, p.user)) throw new RpcError(ErrorCode.Forbidden, '자기보다 순위가 높거나 같은 멤버는 밴할 수 없습니다');
        const reason = typeof p.reason === 'string' ? p.reason : '';
        if ([...reason].length > MAX_REASON_CHARS) throw new RpcError(ErrorCode.InvalidInput, `밴 사유는 ${MAX_REASON_CHARS}자까지`);
        bans.push({
          user: p.user,
          profile: member?.profile ?? this.users.get(p.user) ?? null,
          reason: reason || null,
          banned_by: this.me,
          banned_at: Date.now(),
        });
        this.bans.set(sp.view.space, bans);
        sp.view.members = sp.view.members.filter((m) => m.user !== p.user);
        if (p.delete_recent) {
          for (const [cid, list] of sp.messages) {
            list.forEach((m, k) => {
              if (m.author === p.user && !m.deleted && Date.now() - m.created_at < DAY) {
                list[k] = { ...m, deleted: true, text: '', attachments: [], embeds: [], reactions: [] };
                this.updated(sp, cid, list[k]);
              }
            });
          }
        }
        this.spaceUpdated(sp);
        return ok;
      }
      case 'member.unban': {
        const sp = this.space(p.space);
        this.requirePerm(sp, 'ban');
        const bans = this.bans.get(sp.view.space) ?? [];
        if (!bans.some((b) => b.user === p.user)) throw new RpcError(ErrorCode.NotFound, '밴 목록에 없습니다');
        this.bans.set(
          sp.view.space,
          bans.filter((b) => b.user !== p.user),
        );
        this.spaceUpdated(sp);
        return ok;
      }
      case 'member.bans': {
        const sp = this.space(p.space);
        this.requirePerm(sp, 'ban');
        return structuredClone(this.bans.get(sp.view.space) ?? []);
      }
      case 'user.block': {
        if (p.user === this.me) throw new RpcError(ErrorCode.InvalidInput, '자기 자신은 차단할 수 없습니다');
        if (!this.blocked.has(p.user)) this.blocked.set(p.user, Date.now());
        this.markMembersBlocked();
        return ok;
      }
      case 'user.unblock':
        this.blocked.delete(p.user);
        this.markMembersBlocked();
        return ok;
      case 'user.blocked':
        return [...this.blocked].map(([user, blocked_at]) => ({ user, profile: this.users.get(user) ?? null, blocked_at }));
      case 'user.devices':
        // 모의 데몬은 다른 사람의 기기 목록을 모릅니다.
        return [];
      case 'user.block_device':
        if (p.user === this.me) throw new RpcError(ErrorCode.InvalidInput, '내 기기는 차단할 수 없습니다(사용자 설정 > 기기에서 폐기하세요)');
        if (!this.blockedDevices.has(p.device)) this.blockedDevices.set(p.device, [p.user, Date.now()]);
        return ok;
      case 'user.unblock_device':
        this.blockedDevices.delete(p.device);
        return ok;
      case 'user.blocked_devices':
        return [...this.blockedDevices].map(([device, [user, blocked_at]]) => ({ user, device, name: '', profile: this.users.get(user) ?? null, blocked_at }));
      case 'message.report': {
        const sp = this.space(p.space);
        if (sp.view.kind !== 'crew') throw new RpcError(ErrorCode.InvalidInput, '신고는 크루에서만 할 수 있습니다(1:1 대화는 user.block을 쓰세요)');
        this.channelOf(sp, p.channel);
        const reason = String(p.reason ?? '').trim();
        if (!reason) throw new RpcError(ErrorCode.InvalidInput, '신고 사유를 적어 주세요');
        if ([...reason].length > MAX_REASON_CHARS) throw new RpcError(ErrorCode.InvalidInput, `신고 사유는 ${MAX_REASON_CHARS}자까지`);
        const { list, idx } = this.findMessage(sp, p.channel, p.message);
        if (list[idx].author === this.me) throw new RpcError(ErrorCode.InvalidInput, '자기 메시지는 신고할 수 없습니다');
        if (p.hide) this.reportHidden.add(list[idx].id);
        // 같은 메시지를 다시 신고하면 앞 신고 ID를 돌려줍니다.
        const again = this.reports.find((r) => r.space === sp.view.space && r.message === list[idx].id && r.reporter === this.me);
        if (again) return { report_id: again.report_id };
        const report: ReportView = {
          report_id: 'rp-' + this.rng.id(16).slice(0, 12),
          space: sp.view.space,
          channel: p.channel,
          message: list[idx].id,
          reporter: this.me,
          reason,
          reported_at: Date.now(),
          status: 'open',
          resolution: null,
        };
        this.reports.push(report);
        if (this.isModerator(sp)) this.emit({ type: 'report.new', space: sp.view.space, report_id: report.report_id });
        return { report_id: report.report_id };
      }
      case 'report.list': {
        const sp = this.space(p.space);
        if (!this.isModerator(sp)) throw new RpcError(ErrorCode.Forbidden, '신고를 볼 권한이 없습니다');
        return structuredClone(
          this.reports.filter((r) => r.space === sp.view.space && (!p.status || r.status === p.status)).sort((a, b) => b.reported_at - a.reported_at),
        );
      }
      case 'report.resolve': {
        const sp = this.space(p.space);
        if (!this.isModerator(sp)) throw new RpcError(ErrorCode.Forbidden, '신고를 처리할 권한이 없습니다');
        const r = this.reports.find((x) => x.report_id === p.report_id && x.space === sp.view.space);
        if (!r) throw new RpcError(ErrorCode.NotFound, '신고를 찾을 수 없습니다');
        if (r.status === 'resolved') throw new RpcError(ErrorCode.Conflict, '이미 처리한 신고입니다');
        let author: string | null = null;
        try {
          const { list, idx } = this.findMessage(sp, r.channel, r.message);
          author = list[idx].author;
        } catch {
          author = null;
        }
        const note = typeof p.note === 'string' ? p.note : '';
        if ([...note].length > MAX_REASON_CHARS) throw new RpcError(ErrorCode.InvalidInput, `처리 메모는 ${MAX_REASON_CHARS}자까지`);
        if (p.action !== 'dismiss' && !author) throw new RpcError(ErrorCode.NotFound, '신고된 메시지를 이 기기에서 찾지 못해 처리할 수 없습니다(dismiss만 가능)');
        if (p.action === 'delete_message') await this.dispatch('message.delete', { space: sp.view.space, channel: r.channel, id: r.message });
        // 추방·밴 사유는 멤버 모두가 읽는 상태에 남으므로 운영진 메모를 싣지 않습니다(데몬과 같음).
        else if (p.action === 'kick' && author) {
          if (sp.view.members.some((m) => m.user === author)) await this.dispatch('member.kick', { space: sp.view.space, user: author });
        } else if (p.action === 'ban' && author) {
          if (!(this.bans.get(sp.view.space) ?? []).some((b) => b.user === author)) {
            await this.dispatch('member.ban', { space: sp.view.space, user: author, delete_recent: false });
          }
        } else if (p.action !== 'dismiss') throw new RpcError(ErrorCode.InvalidInput, '알 수 없는 처리입니다');
        r.status = 'resolved';
        r.resolution = note ? `${p.action}: ${note}` : String(p.action);
        return ok;
      }

      default:
        throw new RpcError(ErrorCode.MethodNotFound, `없는 메서드: ${method.slice(0, 64)}`);
    }
  }

  /** 블롭 원본 바이트. 이미지는 SVG를 캔버스로 PNG로 바꿔 MIME과 내용을 맞춥니다. */
  async blobBytes(blob: string): Promise<Uint8Array> {
    const b = this.blobs.get(blob);
    if (!b) return new TextEncoder().encode('MOCK 파일 내용\n');
    if (b.mime === 'image/svg+xml' || typeof document === 'undefined') return new TextEncoder().encode(b.svg);
    const cached = this.pngCache.get(blob);
    if (cached) return cached;
    const img = new Image();
    img.src = `data:image/svg+xml;base64,${b64(b.svg)}`;
    await img.decode();
    const canvas = document.createElement('canvas');
    canvas.width = img.naturalWidth || 800;
    canvas.height = img.naturalHeight || 600;
    canvas.getContext('2d')?.drawImage(img, 0, 0);
    const out = await new Promise<Uint8Array>((resolve, reject) =>
      canvas.toBlob(async (bl) => (bl ? resolve(new Uint8Array(await bl.arrayBuffer())) : reject(new Error('PNG 변환 실패'))), 'image/png'),
    );
    this.pngCache.set(blob, out);
    return out;
  }

  blobDataUrl(blob: string): string {
    const b = this.blobs.get(blob);
    if (!b) return '';
    return `data:image/svg+xml;base64,${b64(b.svg)}`;
  }
}

/** 데몬의 포럼 태그 검사(check_forum_tags): 포럼만, 20개까지, ID 중복 금지, 이름 1~100자·앞뒤 공백·제어 문자 금지. */
function checkForumTags(kind: unknown, raw: unknown): ForumTag[] {
  const bad = (msg: string) => new RpcError(ErrorCode.InvalidInput, msg);
  if (!Array.isArray(raw)) throw new RpcError(ErrorCode.InvalidParams, 'forum_tags는 목록이어야 합니다');
  if (kind !== 'forum' && raw.length > 0) throw bad('포럼 채널에만 태그를 둘 수 있습니다');
  if (raw.length > 20) throw bad('포럼 태그는 20개까지입니다');
  const out: ForumTag[] = [];
  for (const t of raw as Array<Record<string, unknown>>) {
    const id = t?.id;
    const name = t?.name;
    if (typeof id !== 'number' || !Number.isInteger(id) || id < 0 || id > 0xffff_ffff || typeof name !== 'string') {
      throw new RpcError(ErrorCode.InvalidParams, '포럼 태그 형식이 잘못됐습니다');
    }
    if (out.some((x) => x.id === id)) throw bad('포럼 태그 ID가 겹칩니다');
    const n = Array.from(name.trim()).length;
    if (n === 0 || n > 100 || /\p{Cc}/u.test(name)) throw bad('포럼 태그 이름은(는) 1~100자여야 하고 제어 문자를 쓸 수 없습니다');
    if (name !== name.trim()) throw bad('포럼 태그 이름 앞뒤에 공백을 둘 수 없습니다');
    const emoji = typeof t.emoji === 'string' && t.emoji ? t.emoji : undefined;
    out.push(emoji ? { id, name, emoji } : { id, name });
  }
  return out;
}

function privateOverwrites(roles: unknown) {
  if (!Array.isArray(roles) || roles.length === 0) return [];
  return [
    { target: { type: 'role' as const, id: EVERYONE_ROLE }, allow: 0, deny: 1 },
    ...roles.map((r: string) => ({ target: { type: 'role' as const, id: r }, allow: 1, deny: 0 })),
  ];
}

const PERM_BITS: Record<string, number> = {
  view_channel: 0,
  send_messages: 1,
  read_history: 2,
  add_reactions: 3,
  attach_files: 4,
  embed_links: 5,
  use_external_emoji: 6,
  connect_voice: 7,
  speak: 8,
  stream: 9,
  create_posts: 10,
  use_crew_stun: 11,
  mention_many: 13,
  manage_messages: 16,
  manage_channels: 17,
  manage_roles: 18,
  kick: 19,
  create_invite: 20,
  manage_emoji: 21,
  manage_voice: 22,
  manage_space: 23,
  ban: 24,
  administrator: 31,
};

/** 사유·메모 최대 글자 수(데몬 MAX_REASON_CHARS·MAX_NOTE_CHARS). */
const MAX_REASON_CHARS = 512;

/** 데몬의 `anon::name`처럼 같은 사람은 늘 같은 가명. */
const ANON_NAMES = ['용감한 수달', '졸린 판다', '느긋한 고래', '수줍은 여우', '조용한 올빼미'];
function mockAnonName(user: string): string {
  return ANON_NAMES[[...user].reduce((sum, c) => sum + c.charCodeAt(0), 0) % ANON_NAMES.length];
}

function permsFromNames(names: string[]): number {
  let v = 0;
  for (const n of names) {
    const b = PERM_BITS[n];
    if (b === undefined) throw new RpcError(ErrorCode.InvalidInput, `알 수 없는 권한: ${n}`);
    v += 2 ** b;
  }
  return v;
}

/** 데몬의 기본 STUN 목록(`config::DEFAULT_STUN_SERVERS`)과 같게 둡니다. */
const MOCK_DEFAULT_STUN = ['stun.l.google.com:19302', 'stun1.l.google.com:19302', 'stun.cloudflare.com:3478'];

/** 데몬(`stun::normalize`)처럼 `host:port`로 맞춥니다. 받을 수 없는 값이면 null. */
function normalizeStun(raw: string): string | null {
  let v = raw.trim().toLowerCase();
  if (!v || /^turns?:/.test(v)) return null;
  if (v.startsWith('stun:')) v = v.slice(5);
  if (/[@/?#\\\s]/.test(v)) return null;
  const m = /^\[([0-9a-f:.]+)\](?::(\d+))?$/.exec(v) ?? /^([a-z0-9.-]+)(?::(\d+))?$/.exec(v);
  if (!m) return null;
  const port = m[2] === undefined ? 3478 : Number(m[2]);
  if (!Number.isInteger(port) || port < 1 || port > 65535) return null;
  const host = m[1];
  if (v.startsWith('[')) return `[${host}]:${port}`;
  if (host.length > 253 || host.split('.').some((l) => !l || l.length > 63 || l.startsWith('-') || l.endsWith('-'))) return null;
  return `${host}:${port}`;
}

function bytesToB64(bytes: Uint8Array): string {
  let s = '';
  for (let i = 0; i < bytes.length; i += 0x8000) s += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
  return btoa(s);
}

function delay(ms: number) {
  return new Promise((r) => setTimeout(r, ms));
}

const VOLATILE_LINES = [
  '작전명은 「양파 껍질」로 합시다 🧅',
  '내일 9시에 북쪽 다리에서 합류',
  '이 채널 내용은 1분 뒤에 사라져요',
  '지도 캡처 올려요. 저장하지 마세요!',
  '확인. 다들 퇴장하면 바로 지워짐',
  '비밀번호는 음성으로 알려 줄게요',
  '오케이 접수',
  '스크린샷은 못 막으니까 조심 ㅎㅎ',
];

/** 브라우저 개발용 MOCK 전송. URL에 ?onboarding이면 신원 없이, ?quiet면 무작위 활동 없이 시작합니다. */
export function createMockTransport(opts?: MockOptions): Transport & { backend: MockBackend } {
  const params = typeof location !== 'undefined' ? new URLSearchParams(location.search) : new URLSearchParams();
  const backend = new MockBackend({
    noIdentity: params.has('onboarding'),
    quiet: params.has('quiet'),
    ...opts,
  });
  const stateHandlers = new Set<(s: DaemonState) => void>();
  return {
    kind: 'mock',
    backend,
    call: (method, params) => backend.handle(method, (params ?? {}) as Record<string, any>),
    subscribe: (handler) => {
      const off = backend.subscribe(handler);
      return () => void off();
    },
    onDaemonState(handler) {
      stateHandlers.add(handler);
      queueMicrotask(() => handler({ state: 'connected', version: '0.1.0-mock' }));
      return () => void stateHandlers.delete(handler);
    },
    retryDaemon: async () => undefined,
    blobSrc: (_space, blob) => backend.blobDataUrl(blob),
    readBlob: (_space, blob) => backend.blobBytes(blob),
    async pickFiles({ multiple, images }) {
      // 브라우저에는 파일 경로가 없으므로 이름만 쓰는 가짜 경로를 만듭니다.
      const input = document.createElement('input');
      input.type = 'file';
      input.multiple = multiple;
      if (images) input.accept = 'image/png,image/gif,image/webp';
      const files = await new Promise<File[]>((resolve) => {
        input.onchange = () => resolve(Array.from(input.files ?? []));
        input.oncancel = () => resolve([]);
        input.click();
      });
      return files.map<PickedFile>((f) => ({ path: `/mock/${f.name}`, name: f.name, size: f.size }));
    },
    saveDialog: async (name) => `/mock/Downloads/${name}`,
    setContentProtection: async () => false,
    async openExternal(url) {
      window.open(url, '_blank', 'noopener,noreferrer');
    },
    async loadPrefs() {
      try {
        const raw = localStorage.getItem('yangpa-ui-prefs');
        return raw ? (JSON.parse(raw) as Record<string, unknown>) : {};
      } catch {
        return {};
      }
    },
    async savePrefs(prefs) {
      try {
        // UI 설정(테마 등)만 저장합니다. 메시지·휘발성 내용은 절대 넣지 않습니다.
        localStorage.setItem('yangpa-ui-prefs', JSON.stringify(prefs));
      } catch {
        // 저장 못 해도 무시
      }
    },
    onFileDrop: () => () => undefined,
  };
}

/** 데몬처럼 MESH_MAX(6)명을 넘으면 SFU, 호스트는 기기 ID가 가장 작은 참가자(흉내). */
function mockTopology(list: { device: string; user: string }[]): {
  topology: 'mesh' | 'sfu';
  sfu_host?: string;
  sfu_host_user?: string;
} {
  if (list.length <= 6) return { topology: 'mesh' };
  const host = [...list].sort((a, b) => (a.device < b.device ? -1 : a.device > b.device ? 1 : 0))[0];
  return { topology: 'sfu', sfu_host: host.device, sfu_host_user: host.user };
}
