// 로컬 API(docs/api.md, crates/yangpa-rpc/src/api)의 TypeScript 미러.
// 식별자는 base58 문자열, 이모지 ID는 10진수 문자열, 시각은 유닉스 밀리초 정수입니다.

export type UserId = string;
export type DeviceId = string;
export type SpaceId = string;
export type ChannelId = string;
export type EventId = string;
export type BlobId = string;
export type RoleId = string;
export type CallId = string;
/** 커스텀 이모지 ID(64비트라 10진수 문자열). */
export type EmojiId = string;
/** 유닉스 밀리초. */
export type Timestamp = number;

/** 모든 멤버에게 적용되는 기본 역할 ID(16바이트 0의 base58). */
export const EVERYONE_ROLE: RoleId = '1111111111111111';
/**
 * 익명 글·통화 참가자의 작성자 자리(32바이트 0의 base58). 데몬이 익명 채널의 글쓴이를 이 값으로 가려 보냅니다.
 * 아무도 이 공개 키의 비밀 키를 갖지 않으므로 실제 사용자와 겹치지 않습니다.
 */
export const ANONYMOUS_USER: UserId = '11111111111111111111111111111111';
/** 익명 글의 기기 자리(32바이트 0). 통화 참가자는 연결에 써야 해서 실제 기기를 그대로 받습니다. */
export const ANONYMOUS_DEVICE: DeviceId = '11111111111111111111111111111111';

// ───────────── 모델(yangpa-core) ─────────────

export type SpaceKind = 'space' | 'crew' | 'self_space';
export type ChannelKind = 'text' | 'voice' | 'forum' | 'category' | { other: number };
export type SecurityLevel = 'normal' | 'volatile';
export type JoinMode = 'always' | 'join_leave';
export type ChatTtl = { mode: 'none' } | { mode: 'on_leave' } | { mode: 'seconds'; seconds: number };
export type HistoryVisibility = 'public' | 'private';

export type OverwriteTarget = { type: 'role'; id: RoleId } | { type: 'member'; id: UserId };

export interface PermissionOverwrite {
  target: OverwriteTarget;
  allow: number;
  deny: number;
}

export interface ForumTag {
  id: number;
  name: string;
  emoji?: string | null;
}

export interface ChannelSettings {
  name: string;
  kind: ChannelKind;
  topic: string;
  parent: ChannelId | null;
  position: number;
  security: SecurityLevel;
  join_mode: JoinMode;
  ttl: ChatTtl;
  history: HistoryVisibility;
  link_previews: boolean;
  overwrites: PermissionOverwrite[];
  forum_tags: ForumTag[];
}

export interface Role {
  id: RoleId;
  name: string;
  /** 0xRRGGBB */
  color: number;
  /** 클수록 높음. @everyone은 0. */
  rank: number;
  /** 권한 비트(u64). 2^53을 넘으면 JS에서 정밀도를 잃으므로 '전부'로 봅니다. */
  permissions: number;
  automation: boolean;
}

/**
 * 역할 표시 설정. 화면(로컬 API 보기)에서만 거릅니다. 기본값은 @everyone이 `{visible_to: null, see_admins: true}`,
 * 다른 역할은 `{visible_to: null, see_admins: false}`입니다.
 */
export interface RoleDisplay {
  /** 이 역할을 볼 수 있는 역할. null이면 모든 멤버. 역할을 가진 사람과 역할 관리자는 늘 봅니다. */
  visible_to: RoleId[] | null;
  /** 이 역할을 가진 멤버가 관리자(소유자·관리자 권한 역할)가 누구인지 보는지. 권한처럼 더해져, 멤버의 역할(@everyone 포함) 중 하나만 켜져 있어도 봅니다. */
  see_admins: boolean;
}

export interface Profile {
  version: number;
  name: string;
  bio: string;
  avatar: BlobId | null;
  banner: BlobId | null;
  theme_color: number | null;
  status: string;
  /** 이미지 키는 서명된 프로필(이진 형식)에만 실리고 로컬 API JSON에서는 늘 null입니다. */
  avatar_key?: null;
  banner_key?: null;
}

export interface DeviceEntry {
  device: DeviceId;
  /** 봉인 상자 공개키(X-Wing, base64). */
  box_key: string;
  name: string;
  added_at: Timestamp;
  revoked_at: Timestamp | null;
  /** 휴대폰인지(`device.list`만 알려 줌). */
  mobile?: boolean;
}

/** 체크포인트 경보 종류(`checkpoint.alert` 알림). */
export type CheckpointAlertKind = 'conflict' | 'equivocation' | 'fallback';

/** `device.link.create`: 새 기기에 넣을 링크와 그 QR. */
export interface DeviceLinkOffer {
  url: string;
  qr: QrCode;
  expires_at: Timestamp;
}

/** 승인을 기다리는 새 기기. `code`는 새 기기 화면의 번호와 같아야 하고, `contested`면 다른 기기도 이 링크를 썼습니다. */
export interface DeviceLinkRequestView {
  name: string;
  code: string;
  contested: boolean;
}

/** 새 기기가 받는 연결 결과(`device.link_result`). */
export type DeviceLinkStatus = 'approved' | 'rejected' | 'expired' | 'failed';

/** 후원 확인 경로. */
export type SupporterProvider = 'github' | 'patreon';

/** 연결한 후원 계정(`supporter.status`). `valid_until`까지 확인 결과를 믿고, `needs_login`이면 다시 로그인해야 합니다. */
export interface SupporterLink {
  provider: SupporterProvider;
  login: string;
  active: boolean;
  checked_at: Timestamp;
  valid_until: Timestamp;
  needs_login: boolean;
}

/** `supporter.status`: 후원자 전용 기능(라이브러리 탭·콘텐츠 뷰어)을 쓸 수 있는지. 확인 경로가 없는 빌드는 늘 참. */
export interface SupporterStatus {
  active: boolean;
  /** 기존 사용자 유예가 끝나는 시각(유예 중일 때만). */
  grace_until?: Timestamp;
  providers: SupporterProvider[];
  links: SupporterLink[];
  /** 내 다른 기기가 확인해 보내 준 결과(그 기기에서 연결한 계정). 이 기기는 따로 로그인하지 않아도 됩니다. */
  shared: SupporterShared[];
}

/** 내 다른 기기가 보내 준 후원 결과. */
export interface SupporterShared {
  device: DeviceId;
  provider: SupporterProvider;
  login: string;
  active: boolean;
  valid_until: Timestamp;
}

/** QR 코드: `modules`는 위 줄부터 `size`×`size` 칸, `1`이 검정. */
export interface QrCode {
  size: number;
  modules: string;
}

export interface SpaceSettings {
  name: string;
  description: string;
  icon: BlobId | null;
  /** 아이콘 키는 공간 상태(이진 형식)에만 있고 로컬 API JSON에서는 늘 null입니다. */
  icon_key?: null;
  allow_external_emoji: boolean;
  /**
   * 엄격한 키 회전: 나간·내보낸 기기를 뺀 키 갱신이 올 때까지 메시지 보내기를 끝없이 기다립니다(끄면 잠시 기다린 뒤
   * 옛 키로 보내고 `key_rotation_pending`을 켬). 옛 데몬은 보내지 않아 생략될 수 있습니다.
   */
  strict_rotation?: boolean;
}

// ───────────── 보기 타입(api/types.rs) ─────────────

export interface AttachmentView {
  blob: BlobId;
  name: string;
  mime: string;
  size: number;
  width?: number;
  height?: number;
  duration_ms?: number;
  /** 보내는 쪽이 만든 썸네일(base64, WebP/PNG). */
  thumbnail_base64?: string;
  /** 원본을 모두 받았는지. */
  downloaded: boolean;
}

export interface EmbedView {
  url: string;
  title: string;
  description: string;
  site_name: string;
  thumbnail_base64?: string;
  /** 눌러야 불러오는 미디어 플레이어 주소. */
  player_url?: string;
}

export interface ReactionView {
  /** 유니코드 또는 `<:이름:ID>` */
  emoji: string;
  count: number;
  me: boolean;
}

export interface MessageView {
  id: EventId;
  author: UserId;
  author_device: DeviceId;
  created_at: Timestamp;
  lamport: number;
  text: string;
  attachments: AttachmentView[];
  embeds: EmbedView[];
  reply_to?: EventId;
  reactions: ReactionView[];
  edited_at?: Timestamp;
  deleted: boolean;
  volatile: boolean;
  expires_at?: Timestamp;
  thread?: EventId;
  space?: SpaceId;
  channel?: ChannelId;
  /**
   * 내가 차단한 사람의 메시지(모더레이션 계약). 화면은 내용을 접어 두고 눌러야 보여 줍니다.
   * 모더레이션 전 데몬은 이 필드를 보내지 않으므로 없으면 false로 봅니다.
   */
  author_blocked?: boolean;
  /**
   * 익명 글: 익명 채널에서 썼거나 그 채널이 지금 익명. `author`·`author_device`는 ANONYMOUS_USER·ANONYMOUS_DEVICE이고
   * `author_blocked`는 늘 거짓입니다. 거짓이면 생략됩니다.
   */
  anonymous?: boolean;
  /** 익명 글의 가명('형용사 + 동물', 같은 채널에서 같은 사람은 늘 같은 이름). 익명이 아니거나 옛 데몬이면 생략. */
  anon_name?: string;
  /** 내가 쓴 글(익명 글도 내 글이면 참). 거짓이면 생략되고, 옛 데몬은 보내지 않습니다. */
  mine?: boolean;
  /**
   * 나를 멘션한 글: `<@내 ID>`, 또는 그 채널에서 `mention_many` 권한이 있는 사람의 `@everyone`, 작성 뒤 5분 안에 받은
   * `@here`. 받는 쪽 데몬이 작성자 권한을 보고 정합니다. 예전 데몬이면 생략되므로 글의 `<@ID>`도 봅니다.
   */
  mentions_me?: boolean;
}

/** 받지 않은 기록 구간: `after` 다음부터 `before` 앞까지. */
export interface GapView {
  after?: EventId;
  before?: EventId;
}

export interface SpaceSummary {
  space: SpaceId;
  kind: SpaceKind;
  name: string;
  icon?: BlobId;
  unread: number;
  mention: number;
  dormant: boolean;
  member_count: number;
}

export interface MemberView {
  user: UserId;
  profile: Profile;
  nickname?: string;
  roles: RoleId[];
  /** 화면에 보일 접속 상태(숨기 중인 사람은 false, 나는 내 표시 상태). */
  online: boolean;
  /** 온라인이지만 자리 비움. */
  away?: boolean;
  automation: boolean;
  /** 내가 차단한 사람(모더레이션 계약). 없으면 false. */
  blocked?: boolean;
  /** 이 기기와 마지막으로 동기화한 시각(ms). 상대가 비공개로 했거나 기록이 없으면 없음. */
  last_seen?: number;
}

/** `member.search` 거르기: 전체, 지금 온라인, 온라인이거나 최근 7일 안에 연결됨. */
export type MemberFilter = 'all' | 'online' | 'recent';

export interface MemberSearchParams {
  space: SpaceId;
  query?: string;
  filter?: MemberFilter;
  offset?: number;
  /** 기본 100, 최대 200. */
  limit?: number;
}

/** 온라인 → 최근(마지막 연결 최신 순) → 나머지, 같은 묶음은 이름 순. */
export interface MemberPage {
  members: MemberView[];
  /** 조건에 맞는 멤버 수. */
  total: number;
  /** 지금 온라인인 멤버 수(조건과 무관). */
  online: number;
}

/** `message.search` 한 쪽(최신순). `total`은 조건에 맞는 전체 글 수(상한 없음), `offset`은 실제 시작 위치. */
export interface MessageSearchPage {
  messages: MessageView[];
  total: number;
  offset: number;
  /** 데몬에서 걸린 시간(ms). 첫 검색은 색인을 만드느라 깁니다. */
  took_ms: number;
}

export interface MessageSearchParams {
  space?: SpaceId;
  channel?: ChannelId;
  author?: UserId;
  /** 빈칸으로 나눈 낱말이 모두 든 글(큰따옴표로 묶으면 구절 그대로). */
  query: string;
  offset?: number;
  /** 쪽 크기. 기본 50, 최대 500. */
  limit?: number;
}

export interface ChannelView {
  channel: ChannelId;
  settings: ChannelSettings;
  unread: number;
  joined?: boolean;
  participants?: UserId[];
  api_read_allowed: boolean;
  /** P2P 전용 채널(참가한 기기끼리 내용을 직접 주고받아 서로 IP가 보임). 옛 데몬은 보내지 않아 false로 봅니다. */
  p2p: boolean;
  /** 이 기기가 IP 노출 경고를 받아들이고 그 P2P 전용 채널에 참가 중인지(`channel.p2p_join`). 아니면 이 기기에서 잠김. */
  p2p_joined: boolean;
  /** 익명 채널(크루에서만). 글쓴이가 가려지고 `participants`는 수만 맞춰 ANONYMOUS_USER로 채워 옵니다. 거짓이면 생략. */
  anonymous?: boolean;
  /** 포럼 채널의 대문 게시글(`forum.set_featured`). 없거나 이 기기에 그 글이 없으면(지운 글 포함) 생략. */
  featured_post?: EventId;
  /** 첨부 하나의 최대 바이트(크루 관리자가 채널마다 정함). 없으면 무제한, 0이면 첨부 금지. */
  attach_limit?: number;
  /** 수정 금지 채널(아무도 메시지를 고칠 수 없음). 거짓이면 생략. */
  no_edit?: boolean;
  /** 삭제 금지 채널(작성자가 자기 메시지를 지울 수 없음, 운영 삭제는 됨). 거짓이면 생략. */
  no_delete?: boolean;
  /** 상시 휘발성 채널의 참가 기기 상한(다른 채널은 생략). */
  volatile_cap?: number;
  /** 상한이 차서 이 기기가 그 상시 휘발성 채널에 참가하지 못하는 중(자리가 나면 자동으로 참가). 거짓이면 생략. */
  volatile_full?: boolean;
  /** 채널 토픽을 쓰는 큰 크루에서 따라가지 않는 이 채널에 아직 받지 않은 새 글이 있음(열면 받아 옴). 거짓이면 생략. */
  has_new?: boolean;
}

export interface EmojiView {
  id: EmojiId;
  name: string;
  animated: boolean;
  blob: BlobId;
  mime: string;
  /** 올린 사용자(이 판 전의 상태 캐시에서 온 이모지면 없음). */
  created_by?: UserId;
}

/** 크루 커스텀 이모지 한도. `null`이면 한도 없음(개수는 500까지, 크기 무제한). */
export interface EmojiLimits {
  max_count: number | null;
  /** 이모지 이미지 하나의 최대 바이트. */
  max_bytes: number | null;
}

export interface SpaceView {
  space: SpaceId;
  kind: SpaceKind;
  settings: SpaceSettings;
  owner: UserId;
  committer: DeviceId;
  members: MemberView[];
  roles: Role[];
  channels: ChannelView[];
  emojis: EmojiView[];
  /** 크루 이모지 한도(`crew.update`로 바꿈). 없으면 한도 없음. */
  emoji_limits?: EmojiLimits;
  my_permissions: string[];
  pending_commit: boolean;
  /**
   * 나간·내보낸 기기를 뺀 키 갱신을 기다리다 옛 키로 보냈고 아직 갱신이 오지 않음. 그 사이 보낸 글은 나간 기기도 읽을 수
   * 있어 앱이 "키 회전 대기 중"을 보입니다. 옛 데몬은 보내지 않아 생략될 수 있습니다.
   */
  key_rotation_pending?: boolean;
  pending_crew_conversion?: UserId;
  dormant: boolean;
  /** 이 기기가 이 크루를 신뢰하는지(크루 멤버에게 이 기기의 IP가 보임, `trust.set_space`). 옛 데몬은 보내지 않아 false로 봅니다. */
  trusted: boolean;
  /** 역할별 표시 설정(그 역할의 기본값과 같은 역할은 빠짐). 역할을 관리할 수 있는 사람(소유자·관리자·역할 관리)에게만 옵니다. */
  role_display?: Record<RoleId, RoleDisplay>;
  /**
   * 참이면 나는 관리자가 누구인지 볼 수 없습니다. 데몬이 관리자 권한 역할을 `roles`와 멤버 역할에서 이미 뺐고,
   * 앱은 소유자 표시를 숨깁니다(`owner`는 순위 계산에만 씀).
   */
  admins_hidden?: boolean;
  /** 크루 STUN 서버(고칠 수 있는 사람, `manage_space`에게만 옴. 목록이 비어도 옴). */
  crew_servers?: CrewServers;
  /** 크루에 STUN 서버가 있고 내 역할이 쓸 수 있음(`use_crew_stun`). 거짓이면 생략. */
  crew_stun?: boolean;
  /** 이 기기가 이 크루의 서버를 쓰기로 했음(`crew.use_servers`, 이 기기에만 저장). 거짓이면 생략. */
  use_crew_servers?: boolean;
  /** 소유자가 해산한 크루(`crew.disband`). 아무도 쓸 수 없고 `crew.leave`로 이 기기에서 지웁니다. 거짓이면 생략. */
  disbanded?: boolean;
}

/** 크루 STUN 서버 목록(`space.get`의 `crew_servers`, `crew.set_servers`). 8줄까지. */
export interface CrewServers {
  /** `host:port`. */
  stun: string[];
}

export interface InviteView {
  /** 64자리 hex */
  token_hash: string;
  expires_at?: Timestamp;
  max_uses?: number;
  uses: number;
  roles: RoleId[];
  created_by: UserId;
}

export type ForumSort = 'activity' | 'created';
/** 포럼 목록 정렬 방향(기본 `desc`: 새것부터). */
export type SortOrder = 'desc' | 'asc';
/** 여러 태그로 거를 때: 하나라도(`any`, 기본) / 모두(`all`). */
export type TagMatch = 'any' | 'all';

/** `ForumPostView.preview` 길이(글자, 데몬 FORUM_PREVIEW_CHARS). */
export const FORUM_PREVIEW_CHARS = 200;

export interface ForumPostView {
  id: EventId;
  title: string;
  tags: number[];
  author: UserId;
  created_at: Timestamp;
  last_activity: Timestamp;
  reply_count: number;
  pinned: boolean;
  locked: boolean;
  /** 익명 게시글(`author`는 ANONYMOUS_USER). 거짓이면 생략. */
  anonymous?: boolean;
  /** 익명 게시글의 가명(`MessageView.anon_name`과 같음). */
  anon_name?: string;
  /** 내가 쓴 게시글. 거짓이면 생략. */
  mine?: boolean;
  /** 이 채널의 대문 게시글. 거짓이면 생략. */
  featured?: boolean;
  /** 본문을 마지막으로 고친 때(수정 페이지에서 저장하면 제목·태그만 바꿔도 정해짐). 고친 적 없으면 생략. */
  edited_at?: Timestamp;
  /** 본문 앞부분(마크다운 그대로, 줄바꿈 포함, FORUM_PREVIEW_CHARS자까지). */
  preview: string;
  /** 갤러리 표지: 게시글 첨부 중 첫 이미지·영상. */
  cover?: AttachmentView;
  /** 게시글 첨부 수. */
  attachment_count: number;
}

/** 앱 사용자 기기는 늘 `tor`. `normal`은 직접 연결로 띄운 데몬(상시 시더·중계 서버·시험)뿐입니다. */
export type NetMode = 'normal' | 'tor';
export type NatStatus = 'unknown' | 'public' | 'private' | 'disabled';

export interface NetStatus {
  peer_id: string;
  mode: NetMode;
  listen_addrs: string[];
  relay_addrs: string[];
  nat: NatStatus;
  connections: number;
  bootstrap_progress?: number;
  external_addrs: string[];
  onion_addr?: string;
  peers: number;
  /** 시작할 때 다시 거는 연결(인증까지)이 아직 끝나지 않은 친구·나. 끝날 때마다 net.status 알림. */
  connecting?: UserId[];
  /** 오프라인 모드: 데몬이 망(Tor 포함) 없이 떠 있음. */
  offline?: boolean;
}

/** 도전과제 뱃지(번호 순, yangpa-core `badge::Badge`). 이모지·이름은 `lib/badges.ts`. */
export type Badge =
  | 'founder'
  | 'support3'
  | 'support14'
  | 'support50'
  | 'friends5'
  | 'friends10'
  | 'friends20'
  | 'friends50'
  | 'friends100'
  | 'crew10'
  | 'crew100'
  | 'crew1000'
  | 'crew10000'
  | 'seed1gb'
  | 'seed10gb'
  | 'seed100gb'
  | 'seed1tb'
  | 'seed10tb'
  | 'seed100tb'
  | 'seed1pb'
  | 'voice1h'
  | 'voice10h'
  | 'voice100h'
  | 'voice1000h'
  | 'voice10000h'
  | 'screen1h'
  | 'screen10h'
  | 'screen100h'
  | 'screen1000h'
  | 'screen10000h';

/** 이름 앞(`after: false`)이나 뒤에 보일 뱃지. */
export interface ShownBadge {
  badge: Badge;
  after: boolean;
}

/** `badge.get`·`badge.set_shown`. 원년멤버 말고는 그 사람의 데몬이 스스로 계산해 알린 값입니다. */
export interface BadgeView {
  earned: Badge[];
  shown: ShownBadge[];
  /** 내가 개발자 계정이고 이 사람에게 아직 원년멤버를 주지 않음. */
  can_grant_founder: boolean;
}

/** 표시 상태 고르기. `online`은 자동(입력이 한동안 없으면 자리 비움), `offline`은 데몬이 망 없이 다시 뜸. */
export type PresenceMode = 'online' | 'away' | 'invisible' | 'offline';

/** `presence.get`·`presence.set`·`presence.self`. `until`이 지나면 온라인으로 돌아갑니다. */
export interface PresenceView {
  mode: PresenceMode;
  until?: Timestamp;
  idle: boolean;
  hide_last_seen: boolean;
  /** 이 데몬이 망 없이 떠 있음. */
  offline: boolean;
}

export interface PresenceSetParams {
  mode?: PresenceMode;
  /** 온라인이 아닌 모드의 기간(없으면 바꿀 때까지). */
  duration_secs?: number;
  idle?: boolean;
  hide_last_seen?: boolean;
}

/**
 * `flair.list`·`flair.set`·`flair` 알림: 사용자가 낸 후원자 표시(데몬이 지금 시각으로 판정). `until`이 지나면 둘 다 끈
 * 것으로 봅니다.
 */
export interface FlairView {
  user: UserId;
  /** 프로필 카드 배경에 배너를 보임(월 14달러 이상). */
  banner: boolean;
  /** 후광을 쓸 수 있음(월 50달러 이상). 켰는지는 `halo`. */
  halo_allowed: boolean;
  /** 후광 그라데이션의 두 색(0xRRGGBB). 쓸 수 없거나 껐으면 없음. */
  halo?: [number, number];
  until: Timestamp;
}

/** `net.stun_servers`: P2P 연결이 공유기 밖 주소를 알아낼 때 쓰는 STUN 서버(`host:port`). Tor 연결에는 쓰지 않습니다. */
export interface StunServers {
  servers: string[];
  /** 저장한 목록이 없어 기본 목록을 쓰는 중 */
  default: boolean;
}

/** `net.file_circuits`: Tor로 큰 첨부를 파일 전용 회로 여럿으로 주고받을지(기본 꺼짐, 양쪽이 켜야 쓰임). */
export interface FileCircuits {
  enabled: boolean;
  /** 상대 하나에 더 여는 파일 회로 수(0~4). 예전 데몬은 없음. */
  count?: number;
}

/** Tor 브리지 종류: 내장 Snowflake·obfs4, 직접 넣은 줄. */
export type BridgeKind = 'snowflake' | 'obfs4' | 'custom';

/** `net.bridges`·`net.bridges` 알림: Tor 브리지(검열 우회, 기본 꺼짐). */
export interface BridgesView {
  enabled: boolean;
  /** 고른 브리지(꺼져 있어도 남김). */
  kind: BridgeKind;
  /** 직접 넣은 브리지 줄. */
  custom: string[];
  /** 직접 넣은 줄(줄바꿈으로 이음)의 QR. 줄이 없거나 다 들어가지 않으면 없음. */
  qr?: { size: number; modules: string };
  /** Tor에 들어가지 못해 데몬이 Snowflake를 스스로 켬. */
  auto?: boolean;
  /** 브리지 전송 프로그램(lyrebird)이 있음. 없으면 켤 수 없음. */
  available: boolean;
}

/** `net.channel_topics`: 큰 크루에서 채널 묶음 토픽·알림 토픽을 쓸지(기본 꺼짐). */
export interface ChannelTopics {
  enabled: boolean;
}

export interface TorStatus {
  enabled: boolean;
  bootstrap_progress?: number;
  onion_addr?: string;
}

export type DataType = 'text' | 'image' | 'video' | 'file' | 'emoji' | 'history_bundle';
export const DATA_TYPES: DataType[] = ['text', 'image', 'video', 'file', 'emoji', 'history_bundle'];

export interface SeederPolicy {
  enabled: boolean;
  types: DataType[];
  include_channels: ChannelId[] | null;
  all_channels: boolean;
  exclude_channels: ChannelId[];
  excluded_events: EventId[];
  upload_limit_bps: number | null;
  max_concurrent: number;
  /** 열지 않은 채널의 첨부도 저장 공간 한도 안에서 미리 받아 둠(헤드리스 시더). 예전 데몬은 보내지 않습니다. */
  prefetch?: boolean;
}

export interface PeerPolicy {
  types: DataType[];
  priorities: Partial<Record<DataType, number>>;
  download_limit_bps: number | null;
  auto_download_max_bytes: number;
}

export interface ChannelCoverage {
  channel: ChannelId;
  online_seeders: number;
  /** 그 채널을 제공하는 온라인 보관 전용 노드 수(시더와 따로 셈). */
  online_archives: number;
}

/** 크루에 지정된 보관 전용 노드(`archive.list`). 멤버가 아니고 내용을 읽을 수 없습니다. */
export interface ArchiveView {
  device: DeviceId;
  user: UserId;
  name: string;
  /** 함께 보관하는 비공개 채널(공개 채널은 늘 보관). */
  private_channels: ChannelId[];
  granted_by: UserId;
  granted_at: Timestamp;
  online: boolean;
}

/** 보관 노드가 받아 둔 크루(`archive.hosted`). 내용 없이 수치만. */
export interface ArchiveHostedView {
  space: SpaceId;
  owner: UserId;
  members: number;
  channels: number;
  events: number;
  blobs: number;
  used_bytes: number;
  designated_by: UserId;
  designated_at: Timestamp;
  roster_issued_at: Timestamp;
}

export type Scope = 'read' | 'write' | 'admin' | 'settings' | 'owner';
export const SCOPES: Scope[] = ['read', 'write', 'admin', 'settings', 'owner'];

export interface TokenView {
  id: string;
  name: string;
  scopes: Scope[];
  spaces: SpaceId[];
  automation: boolean;
  created_at: Timestamp;
  last_used_at?: Timestamp;
}

export interface AuditEntry {
  id: number;
  at: Timestamp;
  method: string;
  token_name: string;
  ok: boolean;
  code?: number;
  automation: boolean;
}

export interface PendingConfirmation {
  id: string;
  method: string;
  params: unknown;
  token_name: string;
  space?: SpaceId;
  requested_at: Timestamp;
  expires_at?: Timestamp;
  summary: string;
}

export interface VoiceParticipant {
  /** 익명 채널이면 ANONYMOUS_USER이고 `anon`에 번호가 있습니다. */
  user: UserId;
  /** 통화 연결에 쓰는 기기(익명 채널에서도 실제 기기). */
  device: DeviceId;
  /** 익명 채널의 참가자 번호('익명 1', '익명 2' …, 들어온 순서, 그 통화 동안 유지). */
  anon?: number;
  /** 익명 채널의 가명(그 채널의 글과 같은 이름). 옛 데몬이면 생략('익명 N'으로 그림). */
  anon_name?: string;
  /** 이 기기의 참가 줄. 거짓이면 생략. */
  mine?: boolean;
  muted: boolean;
  deafened: boolean;
  screen: boolean;
  /** 이 참가자의 화면을 보는 기기(화면공유 중일 때만, 비면 생략). */
  viewers?: DeviceId[];
  /** Tor 통화에서 이 기기와 서로 신뢰해 WebRTC로 바로 잇는지(서로 IP가 보임). 거짓이면 생략. */
  direct?: boolean;
}

/** WebRTC ICE 서버(RTCIceServer와 같은 모양, urls는 늘 배열). `stun:` 주소만 옵니다. */
export interface IceServer {
  urls: string[];
}

/** 'tor': Tor로 도는 기기의 일반 채널 통화. WebRTC 없이 소리 패킷을 데몬이 Tor로 나름(`voice.media`). */
export type Topology = 'mesh' | 'sfu' | 'tor';

/** SFU 트랙 출처(wire::TrackSource). */
export type TrackSource = 'audio' | 'camera' | 'screen';

/** 받는 쪽이 고르는 simulcast 층(wire::SfuLayer). `auto`는 호스트가 대역폭 추정으로 고르고, `off`는 받지 않습니다. */
export type SfuLayer = 'auto' | 'high' | 'medium' | 'low' | 'off';

/** SFU 세션의 m-line과 트랙의 대응. `device`(출처 기기)는 호스트가 보내는 표에만 있습니다. */
export interface SfuTrack {
  mid: string;
  device?: DeviceId;
  source: TrackSource;
}

/** 친구, 임시 연락처(연락처 링크로 만나 링크가 만료될 때까지), 만료된 연락처(대화는 읽기만). */
export type ContactKind = 'friend' | 'temporary' | 'expired';

export interface FriendView {
  user: UserId;
  profile: Profile;
  space: SpaceId;
  online: boolean;
  away?: boolean;
  /** 마지막 동기화 시각(ms, `MemberView.last_seen`과 같음). */
  last_seen?: number;
  safety_number_verified: boolean;
  kind: ContactKind;
  /** 임시 연락처가 만료되는 시각(만료 없음이면 없음). */
  expires_at?: Timestamp;
  /** 내가 친구 추가를 보내고 상대 수락을 기다리는 중. */
  proposed: boolean;
}

/**
 * 한 사람과의 연결 방식(`TrustView.connection`). `one_way_me_exposed`는 상대에게 내 IP가 보이고 상대는 숨음(나만 신뢰),
 * `one_way_peer_exposed`는 내게 상대 IP가 보이고 나는 숨음(상대만 신뢰). 앱은 어느 경우에도 IP 주소를 보이지 않습니다.
 */
export type ConnectionMode = 'p2p' | 'one_way_me_exposed' | 'one_way_peer_exposed' | 'tor' | 'offline';

/** `trust.get`: 한 사람에 대한 이 기기의 신뢰와 지금 연결 방식. IP는 싣지 않습니다. */
export interface TrustView {
  user: UserId;
  /** 사람 단위 신뢰 스위치(`trust.set_user`). */
  trusted: boolean;
  /** 이 사람과 함께 멤버인 크루 중 내가 신뢰한 크루(크루 신뢰로도 이 사람에게 내 IP가 보임). */
  via_crews: SpaceId[];
  connection: ConnectionMode;
}

export interface FriendRequestView {
  request_id: string;
  user: UserId;
  profile: Profile;
  received_at: Timestamp;
}

export interface FriendLinkView {
  token_id: string;
  created_at: Timestamp;
  expires_at?: Timestamp;
  single_use: boolean;
  uses: number;
}

export interface PeerView {
  peer_id: string;
  user?: UserId;
  rtt_ms?: number;
  relayed: boolean;
}

export interface DiagMetrics {
  delivery_ms_p50: number;
  delivery_ms_p95: number;
  memory_bytes: number;
  cpu_percent: number;
  queues: Record<string, number>;
  events_per_sec: number;
}

export interface BlobStatusView {
  have_chunks: number;
  total_chunks: number;
  seeders_online: number;
  /** 원본(평문) 크기. 모르면 0. */
  size: number;
  /** 내려받기 대기열에 있거나 받는 중. */
  downloading: boolean;
  /** 사용자가 고정함(저장 공간 정리에서 뺌). */
  pinned?: boolean;
  /** 늘 남김(내가 올린 첨부, 이모지·아이콘·프로필 이미지). */
  kept?: boolean;
  /** 휘발성 채널 첨부(RAM 전용). */
  volatile?: boolean;
}

// ───────────── 저장 공간(받은 첨부 정리) ─────────────

/** blob.cache_status. 바이트 단위. 정리 대상은 남에게서 받은, 고정하지 않은 첨부. */
export interface BlobCacheView {
  cache_bytes: number;
  cache_count: number;
  pinned_bytes: number;
  pinned_count: number;
  kept_bytes: number;
  kept_count: number;
  limit_bytes: number;
  min_free_bytes: number;
  default_limit_bytes: number;
  default_min_free_bytes: number;
  /** 데이터 경로 디스크의 여유 공간(알 수 없으면 없음). */
  disk_free_bytes?: number | null;
  /** 이번 실행에서 정리한 수·바이트. */
  evicted_count: number;
  evicted_bytes: number;
}

export interface BlobPinView {
  space: SpaceId;
  blob: BlobId;
  pinned_at: Timestamp;
  size: number;
  downloaded: boolean;
}

/** `library.list`의 한 줄: 첨부를 실은 메시지 하나(`message.space`·`channel`이 채워짐). */
export interface LibraryItem {
  message: MessageView;
  /** 이 기기가 그 메시지를 받은 시각(이 기기에서 쓴 글이면 만든 시각). */
  received_at: Timestamp;
  /** 그 메시지와 첨부를 남에게 내주지 않음(`seed.exclude_message`). */
  excluded: boolean;
}

export interface LibraryPage {
  items: LibraryItem[];
  total: number;
}

// ───────────── 전송 속도·범위별 제한 ─────────────

/**
 * `transfer.rates`(메서드·알림): 이 기기의 지금 전송 속도(바이트/초, 최근 몇 초 평균). 메시지 동기화·첨부 조각으로 주고받는
 * 바이트를 모든 연결(Tor, 직접 연결, 파일 회로)에서 셉니다. 통화 미디어는 들어가지 않습니다. 알림은 값이 바뀔 때만
 * 1초마다 오고, 모두 0이 되면 한 번 보낸 뒤 멈춥니다.
 */
export interface TransferRates {
  up_bps: number;
  down_bps: number;
  /** 이 기기가 내주는 첨부를 지금 받는 사람(첨부·사람마다, 빠른 순, 64줄까지). 비면 생략. 몇 초 조용하면 빠집니다. */
  uploads?: UploadRate[];
}

/** 내가 내주는 첨부 하나를 받는 사람 한 명. */
export interface UploadRate {
  space: SpaceId;
  channel?: ChannelId;
  blob: BlobId;
  /** 받는 사람. 익명 채널의 첨부면 ANONYMOUS_USER. */
  user: UserId;
  bps: number;
}

/** 속도 제한을 거는 범위(이 기기만의 설정). 크루에는 1대1 공간도 들어갑니다. */
export type RateScope =
  | { kind: 'crew'; space: SpaceId }
  | { kind: 'channel'; space: SpaceId; channel: ChannelId }
  | { kind: 'user'; user: UserId };

/** `rate.limits`의 한 줄. 값은 바이트/초, null이면 그 방향은 이 범위에서 제한하지 않음. */
export interface RateLimitView {
  scope: RateScope;
  up_bps: number | null;
  down_bps: number | null;
}

/** crew.committer_candidates: 확정 기기가 될 수 있는 기기(소유자·관리자의 활성 기기). */
export interface CommitterCandidate {
  user: UserId;
  device: DeviceId;
  name: string;
  online: boolean;
  current: boolean;
  mine: boolean;
}

// ───────────── 모더레이션(차단·추방 금지·신고) ─────────────

export interface BanView {
  user: UserId;
  /** 그 사람의 마지막 프로필(모르면 null). */
  profile: Profile | null;
  reason: string | null;
  banned_by: UserId;
  banned_at: Timestamp;
}

export interface BlockedView {
  user: UserId;
  profile: Profile | null;
  blocked_at: Timestamp;
}

/** 남의 기기 하나(`user.devices`): 이 기기가 아는 그 사람의 기기 목록 항목과 이 기기에서 차단했는지. */
export interface UserDeviceView {
  device: DeviceId;
  name: string;
  added_at: Timestamp;
  blocked: boolean;
}

/** 이 기기에서 차단한 남의 기기(`user.blocked_devices`). */
export interface BlockedDeviceView {
  user: UserId;
  device: DeviceId;
  name: string;
  profile: Profile | null;
  blocked_at: Timestamp;
}

export type ReportStatus = 'open' | 'resolved';
/** 신고 처리: 기각 · 메시지 삭제 · 추방 · 추방 금지. */
export type ReportAction = 'dismiss' | 'delete_message' | 'kick' | 'ban';
export const REPORT_ACTIONS: ReportAction[] = ['dismiss', 'delete_message', 'kick', 'ban'];

export interface ReportView {
  report_id: string;
  space: SpaceId;
  channel: ChannelId;
  message: EventId;
  reporter: UserId;
  reason: string;
  reported_at: Timestamp;
  status: ReportStatus;
  resolution: string | null;
}

/** 음성 협상 메시지(yangpa-core wire::SignalPayload). */
export type SignalPayload =
  | { type: 'join'; muted?: boolean }
  | { type: 'leave' }
  | { type: 'offer'; sdp: string }
  | { type: 'answer'; sdp: string }
  | { type: 'ice'; candidate: string; sdp_mid?: string | null; sdp_mline_index?: number | null }
  | { type: 'watch_screen'; watching: boolean }
  | { type: 'state'; muted: boolean; deafened: boolean; screen: boolean }
  // SFU(protocol.md "SFU"): 앱 ↔ 호스트 기기. 앱의 제안은 세션마다 한 번, 그 뒤 재협상은 호스트만 제안합니다.
  | { type: 'sfu_offer'; session: number; sdp: string; tracks: SfuTrack[] }
  | { type: 'sfu_answer'; session: number; sdp: string; tracks?: SfuTrack[] }
  | { type: 'sfu_ice'; session: number; candidate: string; sdp_mid?: string | null; sdp_mline_index?: number | null }
  | { type: 'sfu_select'; device: DeviceId; source: TrackSource; layer: SfuLayer }
  | { type: 'sfu_close'; session: number };

// ───────────── 메서드 인자·결과(api/params.rs) ─────────────

export type Empty = Record<string, never>;

export interface SpaceParams {
  space: SpaceId;
}
export interface SpaceChannelParams {
  space: SpaceId;
  channel: ChannelId;
}
export interface SpaceUserParams {
  space: SpaceId;
  user: UserId;
}
export interface UserParams {
  user: UserId;
}

export interface HelloResult {
  scopes: Scope[];
  automation: boolean;
  spaces?: SpaceId[];
}
export interface VersionResult {
  version: string;
  protocol: number;
}
/** `update.check`: 공개 저장소의 최신 정식 릴리스. */
export interface UpdateInfo {
  current: string;
  latest: string;
  newer: boolean;
}
/** 업데이트 파일을 받지 못한 까닭: 서명 없는 릴리스·연결·SHA-256 불일치·디스크. */
export type UpdateError = 'unsigned' | 'network' | 'hash' | 'failed';
export interface StatusResult {
  identity?: UserId;
  device?: DeviceId;
  net: NetStatus;
  spaces: number;
  tor: TorStatus;
}
export interface IdentityRef {
  user: UserId;
  device: DeviceId;
}
export interface IdentityView {
  user: UserId;
  device: DeviceId;
  profile: Profile;
  devices: DeviceEntry[];
  /** 내 기기로 연결된 뒤 첫 동기화 중(끝나면 `device.synced`). */
  first_syncing: boolean;
}
export interface ProfileSetParams {
  name?: string;
  bio?: string;
  /** 이미지 파일 경로. 빈 문자열이면 지웁니다. */
  avatar_path?: string;
  banner_path?: string;
  /** null이면 지움 */
  theme_color?: number | null;
  status?: string;
}
export interface FriendLinkCreateParams {
  expires_in_secs?: number;
  single_use?: boolean;
}
export interface FriendLinkCreateResult {
  url: string;
  token_id: string;
  expires_at?: Timestamp;
}
export interface FriendAddResult {
  status: 'pending' | 'connected';
  user: UserId;
}
export interface SafetyNumberView {
  digits: string;
  verified: boolean;
}
export interface ChannelCreateParams {
  space: SpaceId;
  name: string;
  kind: ChannelKind;
  parent?: ChannelId;
  topic?: string;
  security?: SecurityLevel;
  join_mode?: JoinMode;
  ttl_secs?: number;
  ttl_on_leave?: boolean;
  history?: HistoryVisibility;
  link_previews?: boolean;
  private_roles?: RoleId[];
  /**
   * 채널 권한 덮어쓰기 전체(바꿀 때는 목록을 통째로 바꿈). `private_roles`와 함께 줄 수 없습니다. 허용·거부가 모두 0인 항목은
   * 데몬이 버리고, 내게 없는 권한 비트는 바꿀 수 없습니다(소유자·관리자는 전부). 카테고리 덮어쓰기는 안의 채널로 물려주지 않습니다.
   */
  overwrites?: PermissionOverwrite[];
  position?: number;
  /** 포럼 채널 태그(id는 채널 안에서 고유). */
  forum_tags?: ForumTag[];
  /** 참이면 P2P 전용 채널(텍스트·음성만, 내 기기끼리 공간은 안 됨). 만든 기기도 따로 참가해야 합니다. */
  p2p?: boolean;
  /**
   * 익명 채널(크루에서만, 카테고리는 안 됨, 채널 관리 권한). 바꿀 때 끄면 켜져 있는 동안 쓴 글은 익명으로 남고,
   * 통화 채널이면 그 순간 모든 참가자가 통화에서 나옵니다.
   */
  anonymous?: boolean;
  /**
   * 첨부 하나의 최대 바이트(null이면 무제한, 0이면 첨부 금지). 크루의 카테고리가 아닌 채널만, 0 또는 ATTACH_LIMIT_MIN 이상.
   * 만들 때 빠지면 무제한이고 크루의 휘발성 채널만 10MiB입니다.
   */
  attach_limit?: number | null;
  /** 수정 금지(크루의 카테고리가 아닌 채널만, 채널 관리 권한). */
  no_edit?: boolean;
  /** 삭제 금지(작성자가 자기 메시지를 못 지움, 운영 삭제는 됨). */
  no_delete?: boolean;
}
/** 빠진 필드는 그대로 둡니다. @everyone은 권한·색·`see_admins`만 바꿀 수 있습니다. */
export interface RoleUpdateParams {
  space: SpaceId;
  role: RoleId;
  name?: string;
  color?: number;
  permissions?: string[];
  rank?: number;
  /** 이 역할을 볼 수 있는 역할(null이면 모든 멤버). @everyone에는 줄 수 없습니다. */
  visible_to?: RoleId[] | null;
  /** 이 역할을 가진 멤버가 관리자가 누구인지 보는지. */
  see_admins?: boolean;
}
/** P2P 전용 여부는 `channel.update`가 아니라 `channel.set_p2p`로 바꿉니다. */
export interface ChannelUpdateParams extends Partial<Omit<ChannelCreateParams, 'space' | 'kind' | 'parent' | 'p2p'>> {
  space: SpaceId;
  channel: ChannelId;
  /** null이면 카테고리에서 뺌 */
  parent?: ChannelId | null;
  /** 첨부 하나의 최대 바이트. null이면 무제한, 0이면 첨부 금지, 빠지면 그대로. 크루의 카테고리가 아닌 채널만. */
  attach_limit?: number | null;
  /** 상시 휘발성 채널의 참가 기기 상한(2..=2000). null이면 기본 200, 빠지면 그대로. 크루만. */
  volatile_cap?: number | null;
}
export interface MessageListParams {
  space: SpaceId;
  channel: ChannelId;
  before?: EventId;
  limit?: number;
  thread?: EventId;
}
export interface MessageListResult {
  messages: MessageView[];
  gaps: GapView[];
}
export interface AttachmentInput {
  /** 절대 경로. 붙여넣기(`paste`)면 빈 문자열. */
  path: string;
  /** `blob.paste`로 미리 올린 붙여넣기 ID(경로 대신). 이때는 `name`이 꼭 있어야 합니다. */
  paste?: string;
  name?: string;
  /** 동영상·음성: 앱이 만든 썸네일(PNG·JPEG·WebP base64, 64KiB·긴 변 640px 이하). 데몬이 검사하고 JPEG로 다시 만듭니다. */
  thumbnail_base64?: string;
  /** 동영상 가로·세로(함께). 음성에는 주지 않습니다. */
  width?: number;
  height?: number;
  /** 재생 길이(밀리초). 데몬이 파일에서 읽을 수 있으면(MP4·MOV) 그 값을 씁니다. */
  duration_ms?: number;
  /** 참이면 메타데이터(촬영 위치·기기·시각 등)를 지우지 않고 그대로 보냅니다. 없으면 데몬이 지웁니다. */
  keep_metadata?: boolean;
}
export interface MessageSendParams {
  space: SpaceId;
  channel: ChannelId;
  text: string;
  attachments?: AttachmentInput[];
  reply_to?: EventId;
  thread?: EventId;
}
export interface MessageRefParams {
  space: SpaceId;
  channel: ChannelId;
  id: EventId;
}
export interface ReactParams extends MessageRefParams {
  emoji: string;
}
export interface BlobReadParams {
  space: SpaceId;
  blob: BlobId;
  key_from: EventId;
  index?: number;
  offset?: number;
  len?: number;
  binary?: boolean;
}
export interface VoiceJoinResult {
  call: CallId;
  participants: VoiceParticipant[];
  /** base64 32바이트 미디어 키 */
  media_key: string;
  generation: number;
  topology: Topology;
  sfu_host?: DeviceId;
  /** SFU 호스트 기기의 사용자(통화 밖 호스트 노드도 누구 것인지 보여 줌). */
  sfu_host_user?: UserId;
  /** 이 통화에 쓸 STUN 서버(내 것 먼저, 그다음 쓰기로 한 크루 것). */
  ice_servers: IceServer[];
}
export interface SeedSetParams {
  space?: SpaceId;
  enabled?: boolean;
  types?: DataType[];
  channels?: ChannelId[] | null;
  all_channels?: boolean;
  exclude_channels?: ChannelId[];
  upload_limit_bps?: number | null;
  max_concurrent?: number;
  prefetch?: boolean;
}
export interface PeerSetParams {
  space?: SpaceId;
  types?: DataType[];
  priorities?: Partial<Record<DataType, number>>;
  download_limit_bps?: number | null;
  auto_download_max_bytes?: number;
}

/** 메서드 이름 → [인자, 결과]. client.call이 이 표로 타입을 맞춥니다. */
export interface Methods {
  'auth.hello': [{ token: string }, HelloResult];
  'system.version': [Empty, VersionResult];
  'system.status': [Empty, StatusResult];
  'system.shutdown': [Empty, Empty];
  'diag.metrics': [Empty, DiagMetrics];
  'diag.leakcheck': [{ marker: string }, { clean: boolean; findings: string[] }];

  'identity.create': [{ name: string }, IdentityRef];
  'identity.get': [Empty, IdentityView];
  'identity.delete': [Empty, Empty];
  'device.list': [Empty, DeviceEntry[]];
  'device.rename': [{ device: DeviceId; name: string }, Empty];
  'device.revoke': [{ device: DeviceId }, Empty];
  'device.link.create': [Empty, DeviceLinkOffer];
  'device.link.pending': [Empty, DeviceLinkRequestView | null];
  'device.link.decide': [{ approve: boolean }, Empty];
  'device.link.cancel': [Empty, Empty];
  'device.link.join': [{ url: string }, { code: string }];
  'device.link.abort': [Empty, Empty];

  'supporter.status': [Empty, SupporterStatus];
  'supporter.github_begin': [Empty, { user_code: string; verification_uri: string; expires_in_secs: number }];
  'supporter.patreon_begin': [Empty, { url: string }];
  'supporter.refresh': [Empty, SupporterStatus];
  'supporter.unlink': [{ provider: SupporterProvider }, Empty];
  'update.check': [Empty, UpdateInfo];
  'update.download': [{ version: string; asset: string }, Empty];
  'presence.get': [Empty, PresenceView];
  'presence.set': [PresenceSetParams, PresenceView];
  'flair.list': [Empty, FlairView[]];
  /** 후광 색(`null`이면 끔). 월 50달러 이상 후원자만. */
  'flair.set': [{ halo: [number, number] | null }, FlairView];

  'badge.get': [UserParams, BadgeView];
  'badge.set_shown': [{ shown: ShownBadge[] }, BadgeView];
  'badge.grant_founder': [UserParams, Empty];
  'profile.get': [{ user?: UserId }, Profile];
  'profile.set': [ProfileSetParams, Profile];
  'profile.set_crew': [{ space: SpaceId; nickname?: string }, Empty];
  'note.get': [UserParams, { text: string }];
  'note.set': [{ user: UserId; text: string }, Empty];

  'friend.link.create': [FriendLinkCreateParams, FriendLinkCreateResult];
  'friend.link.list': [Empty, FriendLinkView[]];
  'friend.link.revoke': [{ token_id: string }, Empty];
  'friend.add': [{ url: string }, FriendAddResult];
  'friend.requests': [Empty, FriendRequestView[]];
  'friend.accept': [{ request_id: string }, { space: SpaceId }];
  'friend.reject': [{ request_id: string }, Empty];
  'friend.list': [Empty, FriendView[]];
  /** 임시 연락처에게 친구 추가를 보냅니다. 상대가 이미 보내 뒀으면 바로 친구(`friends`). */
  'friend.propose': [UserParams, { status: 'requested' | 'friends' }];
  /** 그 연락처와 대화할 1:1 공간. 1:1을 크루로 바꿨으면 둘만의 1:1 공간을 새로 만들어 줍니다. */
  'friend.open': [UserParams, { space: SpaceId }];
  'friend.remove': [UserParams, Empty];
  'friend.safety_number': [UserParams, SafetyNumberView];
  'friend.verify': [{ user: UserId; verified: boolean }, Empty];
  /** 한 사람에 대한 신뢰 스위치와 지금 연결 방식(IP는 싣지 않음). */
  'trust.get': [UserParams, TrustView];
  /** 사람 단위 신뢰(그 사람에게 이 기기의 IP를 보여도 됨, 사람만). 나 자신이나 함께 쓰는 공간이 없는 사람은 켤 수 없습니다. */
  'trust.set_user': [{ user: UserId; enabled: boolean }, Empty];
  /** 크루 단위 신뢰(그 크루 멤버에게 이 기기의 IP를 보여도 됨, 크루만·사람만). 바뀌면 `space.updated`. */
  'trust.set_space': [{ space: SpaceId; enabled: boolean }, Empty];

  'space.list': [Empty, SpaceSummary[]];
  'space.get': [SpaceParams, SpaceView];
  'crew.create': [{ name: string; description?: string }, { space: SpaceId }];
  'crew.update': [{ space: SpaceId; name?: string; description?: string; icon_path?: string; strict_rotation?: boolean; emoji_limits?: EmojiLimits }, Empty];
  'crew.convert_request': [{ space: SpaceId; name: string }, Empty];
  'crew.convert_accept': [SpaceParams, Empty];
  'crew.leave': [SpaceParams, Empty];
  'crew.transfer_ownership': [SpaceUserParams, Empty];
  /** 크루 해산(소유자만). 모든 멤버에게서 크루가 끝나고 이 기기의 크루 데이터를 지웁니다. */
  'crew.disband': [SpaceParams, Empty];
  'crew.set_committer': [{ space: SpaceId; device: DeviceId }, Empty];
  'crew.committer_candidates': [SpaceParams, CommitterCandidate[]];
  /** 크루 STUN 서버 목록 전체를 바꿈(`manage_space`, 사람만, 8줄까지). */
  'crew.set_servers': [{ space: SpaceId; stun: string[] }, Empty];
  /** 이 기기가 그 크루의 STUN 서버를 쓸지(이 기기에만 저장, 사람만). 크루 서버가 이 기기의 공인 IP를 봅니다. */
  'crew.use_servers': [{ space: SpaceId; enabled: boolean }, Empty];
  'member.list': [SpaceParams, MemberView[]];
  /** 큰 크루용 멤버 목록 쪽(온라인·최근 거르기, 이름 검색). */
  'member.search': [MemberSearchParams, MemberPage];
  'member.kick': [{ space: SpaceId; user: UserId; reason?: string }, Empty];
  'member.set_roles': [{ space: SpaceId; user: UserId; roles: RoleId[] }, Empty];
  /** 추방하고 다시 들어오지 못하게 합니다. `delete_recent`면 그 사람의 최근 메시지도 지웁니다. */
  'member.ban': [{ space: SpaceId; user: UserId; reason?: string; delete_recent?: boolean }, Empty];
  'member.unban': [SpaceUserParams, Empty];
  'member.bans': [SpaceParams, BanView[]];
  /** 내 기기에서만: 그 사람의 메시지를 접고 알림·친구 요청·통화를 받지 않습니다. */
  'user.block': [UserParams, Empty];
  'user.unblock': [UserParams, Empty];
  'user.blocked': [Empty, BlockedView[]];
  'user.devices': [UserParams, UserDeviceView[]];
  'user.block_device': [{ user: UserId; device: DeviceId }, Empty];
  'user.unblock_device': [{ user: UserId; device: DeviceId }, Empty];
  'user.blocked_devices': [Empty, BlockedDeviceView[]];

  'channel.list': [SpaceParams, ChannelView[]];
  'channel.create': [ChannelCreateParams, { channel: ChannelId }];
  'channel.update': [ChannelUpdateParams, Empty];
  'channel.delete': [SpaceChannelParams, Empty];
  'channel.join': [SpaceChannelParams, Empty];
  'channel.leave': [SpaceChannelParams, Empty];
  'channel.allow_api_read': [SpaceChannelParams & { allowed: boolean }, Empty];
  /** P2P 전용으로 바꾸거나(`enabled`) Tor로 되돌립니다(어느 쪽이든 언제든, 사람만). */
  'channel.set_p2p': [SpaceChannelParams & { enabled: boolean }, Empty];
  /** 이 기기로 P2P 전용 채널에 참가(서로 IP가 보인다는 경고를 받아들임, 사람만). */
  'channel.p2p_join': [SpaceChannelParams, Empty];
  /** P2P 전용 채널 참가를 그만둠(이 기기에서 다시 잠김). */
  'channel.p2p_leave': [SpaceChannelParams, Empty];
  'channel.mark_read': [SpaceChannelParams & { event?: EventId }, Empty];
  /** 지금 글을 쓰는 중이라고 알립니다(입력하는 동안 몇 초마다; 데몬이 3초에 한 번으로 줄임). */
  'channel.typing': [SpaceChannelParams, Empty];

  'message.list': [MessageListParams, MessageListResult];
  'message.send': [MessageSendParams, { id: EventId }];
  'message.edit': [MessageRefParams & { text: string }, Empty];
  'message.delete': [MessageRefParams, Empty];
  'message.react': [ReactParams, Empty];
  'message.unreact': [ReactParams, Empty];
  /** `author`를 주면 그 사람이 쓴 글만(익명 글 제외, `query`는 비어도 됨). */
  'message.search': [MessageSearchParams, MessageSearchPage];
  /** 크루 운영진(추방·추방 금지·메시지 관리 권한)에게 신고합니다. `hide`면 내 화면에서 접습니다. */
  'message.report': [SpaceChannelParams & { message: EventId; reason: string; hide?: boolean }, { report_id: string }];
  'report.list': [{ space: SpaceId; status?: ReportStatus }, ReportView[]];
  'report.resolve': [{ space: SpaceId; report_id: string; action: ReportAction; note?: string }, Empty];
  /** `attachments`는 message.send와 같음(`attach_files`). 첨부가 있으면 `text`가 비어도 됩니다. `featured`는 그 채널의 `manage_channels`. */
  'forum.create_post': [
    SpaceChannelParams & { title: string; text: string; tags?: number[]; attachments?: AttachmentInput[]; featured?: boolean },
    { id: EventId },
  ];
  /** 고정 글이 먼저, 그다음 고른 순서(200개까지). `tag`(예전 호출)를 주면 `tags`에 더합니다. */
  'forum.list_posts': [
    SpaceChannelParams & { sort: ForumSort; order?: SortOrder; tag?: number; tags?: number[]; tag_match?: TagMatch },
    ForumPostView[],
  ];
  'forum.update_post': [
    SpaceChannelParams & { post: EventId; title?: string; tags?: number[]; pinned?: boolean; locked?: boolean },
    Empty,
  ];
  /** 게시글 하나(목록 항목과 같은 모양). 없거나 지운 글이면 1004. */
  'forum.get_post': [SpaceChannelParams & { post: EventId }, ForumPostView];
  /** 대문 게시글 정하기·빼기(`null`). 그 채널의 `manage_channels`. 지금 값과 같으면 아무것도 하지 않고 성공. */
  'forum.set_featured': [SpaceChannelParams & { post: EventId | null }, Empty];

  'blob.status': [{ space: SpaceId; blob: BlobId }, BlobStatusView];
  'blob.fetch': [{ space: SpaceId; blob: BlobId; priority?: number }, Empty];
  'blob.read': [BlobReadParams, { data_base64?: string }];
  /** 휘발성 첨부는 사람 토큰이 `allow_volatile: true`로 확인했을 때만 저장(아니면 1010). */
  'blob.save': [{ space: SpaceId; blob: BlobId; key_from: EventId; path: string; allow_volatile?: boolean }, Empty];
  'blob.cache_status': [Empty, BlobCacheView];
  /** 빠진 값은 그대로, `reset`이면 둘 다 기본값(데몬 시작 옵션). */
  'blob.cache_set_limit': [{ limit_bytes?: number; min_free_bytes?: number; reset?: boolean }, BlobCacheView];
  /** 정리 대상을 모두 비웁니다(받는 중·방금 본 것은 남김). 다시 받을 수 있습니다. */
  'blob.cache_clear': [Empty, { evicted_count: number; freed_bytes: number }];
  'blob.pin': [{ space: SpaceId; blob: BlobId }, Empty];
  'blob.unpin': [{ space: SpaceId; blob: BlobId }, Empty];
  'blob.pins': [{ space?: SpaceId }, BlobPinView[]];
  /** 이 기기에 둔 첨부 데이터만 지웁니다(메시지는 그대로, 다시 받을 수 있음). 받는 중이면 받기를 멈추고 1010(잠시 뒤 다시). */
  'blob.evict': [{ space: SpaceId; blob: BlobId }, Empty];
  /**
   * 첨부를 실은 메시지(라이브러리 탭, 일반 등급 채널만). `messages`를 주면 그 메시지만(200개까지) 썸네일과 함께, 없으면 전체를
   * 최신순 쪽으로(500개까지) 썸네일 없이. `offset`이 `total`에 닿을 때까지 읽습니다.
   */
  'library.list': [{ space?: SpaceId; messages?: EventId[]; offset?: number; limit?: number }, LibraryPage];
  /**
   * 이 기기에 데이터가 없는 첨부(일반 채널만)를 같은 내용의 로컬 파일(절대 경로)로 다시 채웁니다. 데몬이 첨부의 키로 다시
   * 암호화해 블롭 ID(해시)가 같을 때만 받고 고정합니다. 다르면 1005, 휘발성 채널도 1005, 모르는 메시지·첨부는 1004.
   */
  'blob.relink': [{ space: SpaceId; channel: ChannelId; message: EventId; blob: BlobId; path: string }, BlobStatusView];
  /** 붙여넣은 첨부 바이트를 데몬 RAM에 조각씩 올립니다(처음엔 `size`, 이어 붙일 때는 `paste`). 조각 하나는 4MiB 이하. */
  'blob.paste': [{ paste?: string; size?: number; data_base64: string }, { paste: string; received: number }];
  /** 미리 올린 붙여넣기를 지웁니다(없어도 성공). */
  'blob.paste_discard': [{ paste: string }, Empty];
  /** 지금 전체 올리기·받기 속도와 내가 내주는 첨부를 받는 사람(자동화 토큰에는 `uploads`가 비어 옴). */
  'transfer.rates': [Empty, TransferRates];
  /** 크루·채널·사람별 속도 제한(이 기기만의 설정). */
  'rate.limits': [Empty, RateLimitView[]];
  /** 그 범위의 제한을 통째로 바꿈. 둘 다 null·생략이면 지움. 0은 거절(1005), 1024개까지. */
  'rate.set_limit': [{ scope: RateScope; up_bps?: number | null; down_bps?: number | null }, Empty];
  'emoji.list': [SpaceParams, EmojiView[]];
  'emoji.create': [{ space: SpaceId; name: string; path: string }, { id: EmojiId; animated: boolean }];
  'emoji.delete': [{ space: SpaceId; id: EmojiId }, Empty];

  'role.list': [SpaceParams, Role[]];
  'role.create': [{ space: SpaceId; name: string; color?: number; permissions: string[]; rank?: number }, { role: RoleId }];
  'role.update': [RoleUpdateParams, Empty];
  'role.delete': [{ space: SpaceId; role: RoleId }, Empty];
  'invite.create': [{ space: SpaceId; expires_in_secs?: number; max_uses?: number; roles?: RoleId[] }, { url: string }];
  'invite.list': [SpaceParams, InviteView[]];
  'invite.revoke': [{ space: SpaceId; token_hash: string }, Empty];
  'invite.join': [{ url: string }, { space: SpaceId; status: 'pending' | 'joined' }];

  'seed.get': [{ space?: SpaceId }, SeederPolicy];
  'seed.set': [SeedSetParams, Empty];
  'seed.exclude_message': [MessageRefParams & { excluded: boolean }, Empty];
  'peer.get': [{ space?: SpaceId }, PeerPolicy];
  'peer.set': [PeerSetParams, Empty];
  'seed.coverage': [SpaceParams, ChannelCoverage[]];

  'archive.offer': [{ expires_in_secs?: number; name?: string }, { url: string; expires_at?: Timestamp }];
  'archive.hosted': [Empty, ArchiveHostedView[]];
  'archive.drop': [SpaceParams, Empty];
  'archive.designate': [{ space: SpaceId; url: string; private_channels?: ChannelId[]; name?: string }, ArchiveView];
  'archive.revoke': [{ space: SpaceId; device: DeviceId }, Empty];
  'archive.list': [SpaceParams, ArchiveView[]];

  'net.status': [Empty, NetStatus];
  'net.stun_servers': [Empty, StunServers];
  /** `servers: null`이면 기본 목록, 빈 목록이면 STUN을 쓰지 않음(사람 전용). */
  'net.set_stun_servers': [{ servers: string[] | null }, Empty];
  'net.file_circuits': [Empty, FileCircuits];
  /** 사람 전용: 켜면 이 기기의 onion 서비스가 회로를 더 자주 만듭니다. */
  /** `count`(0~4)를 주면 그 수로, 없으면 예전처럼 켬 = 4개·끔 = 0개. */
  'net.set_file_circuits': [{ enabled: boolean; count?: number }, Empty];
  'net.channel_topics': [Empty, ChannelTopics];
  /** 사람 전용. 켜면 큰 크루에서 최근 연 채널의 묶음 토픽과 멘션·답장 알림 토픽만 구독합니다. */
  'net.set_channel_topics': [{ enabled: boolean }, Empty];
  /** 사람 전용(직접 넣은 브리지 줄은 공개되지 않은 브리지 주소). */
  'net.bridges': [Empty, BridgesView];
  /** 사람 전용. 직접 넣은 줄은 빈 줄을 빼고 32줄까지, 깨진 줄은 거절. 켜면 바로 브리지로 다시 연결합니다. */
  'net.set_bridges': [{ enabled: boolean; kind: BridgeKind; custom: string[] }, Empty];
  'net.peers': [Empty, PeerView[]];

  'voice.join': [SpaceChannelParams, VoiceJoinResult];
  'voice.leave': [SpaceChannelParams, Empty];
  'voice.signal': [SpaceChannelParams & { call: CallId; to: DeviceId; payload: SignalPayload }, Empty];
  /** Tor 통화의 봉인한 소리 패킷 하나(base64, 4KiB 이하). */
  'voice.media': [SpaceChannelParams & { call: CallId; data: string }, Empty];
  'voice.state': [SpaceChannelParams & { muted: boolean; deafened: boolean; screen: boolean }, Empty];
  'voice.watch': [SpaceChannelParams & { device: DeviceId; watching: boolean }, Empty];

  'token.create': [{ name: string; scopes: Scope[]; spaces?: SpaceId[]; automation?: boolean }, { token: string; id?: string }];
  'token.list': [Empty, TokenView[]];
  'token.revoke': [{ id: string }, Empty];
  'audit.list': [{ limit?: number; before?: number }, AuditEntry[]];
  'confirm.list': [Empty, PendingConfirmation[]];
  'confirm.approve': [{ id: string }, { result: unknown }];
  'confirm.deny': [{ id: string }, Empty];

  /** 구독은 전송(Tauri 쪽·Node 전송)이 연결할 때 직접 합니다. 웹뷰에서 부르면 거절됩니다. */
  'events.subscribe': [{ topics?: string[] }, { subscription: number }];
  'events.unsubscribe': [{ subscription: number }, Empty];
}

export type MethodName = keyof Methods;
export type ParamsOf<M extends MethodName> = Methods[M][0];
export type ResultOf<M extends MethodName> = Methods[M][1];

// ───────────── 알림(api/events.rs) ─────────────

export interface NotifyView {
  title: string;
  /** 휘발성 채널은 본문이 없습니다. */
  body?: string;
  /** 보낸 사람(OS 알림 프사). 휘발성 채널은 없습니다. */
  sender?: { user: UserId; name: string; avatar?: BlobId };
}

export type Notification =
  | { type: 'message.new'; space: SpaceId; channel: ChannelId; message: MessageView; notify: NotifyView }
  | { type: 'message.updated'; space: SpaceId; channel: ChannelId; message: MessageView }
  | { type: 'message.expired'; space: SpaceId; channel: ChannelId; ids: EventId[] }
  | { type: 'space.updated'; space: SpaceId }
  /** 새 신고(운영진에게만). 내용은 report.list로 읽습니다. */
  | { type: 'report.new'; space: SpaceId; report_id: string }
  | { type: 'friend.request'; request_id: string; user: UserId; profile: Profile }
  | { type: 'friend.accepted'; user: UserId; space: SpaceId }
  /** 연락처 종류가 바뀜(임시 연락처가 만료되거나 친구가 됨). `friend.list`를 다시 읽습니다. */
  | { type: 'friend.updated'; user: UserId }
  | { type: 'presence'; user: UserId; online: boolean; away?: boolean; last_seen?: number }
  /** 이 기기의 표시 상태가 바뀜(기간이 끝난 때 포함). */
  | ({ type: 'presence.self' } & PresenceView)
  /** 사용자(나 자신도)의 후원자 표시가 바뀜. */
  | ({ type: 'flair' } & FlairView)
  | { type: 'profile.updated'; user: UserId }
  /** 그 사람(나도)의 뱃지가 바뀜. `badge.get`을 다시 읽습니다. */
  | { type: 'badges.updated'; user: UserId }
  | { type: 'voice.signal'; space: SpaceId; channel: ChannelId; call: CallId; from: DeviceId; payload: SignalPayload }
  | { type: 'voice.media'; space: SpaceId; channel: ChannelId; call: CallId; from: DeviceId; data: string }
  | {
      type: 'voice.participants';
      space: SpaceId;
      channel: ChannelId;
      participants: VoiceParticipant[];
      generation: number;
      media_key: string;
      topology: Topology;
      /** SFU 호스트 기기(SFU이고 호스트가 정해졌을 때만, 넘겨받는 중이면 없음). */
      sfu_host?: DeviceId;
      /** SFU 호스트 기기의 사용자. */
      sfu_host_user?: UserId;
    }
  | { type: 'blob.progress'; space: SpaceId; blob: BlobId; have: number; total: number }
  | ({ type: 'net.status' } & NetStatus)
  /** 브리지 설정이 바뀜(`auto`면 Tor에 들어가지 못해 데몬이 Snowflake를 켬). */
  | ({ type: 'net.bridges' } & BridgesView)
  | ({ type: 'transfer.rates' } & TransferRates)
  | ({ type: 'confirm.pending' } & PendingConfirmation)
  | { type: 'seed.coverage_warning'; space: SpaceId; channel: ChannelId }
  | { type: 'events.dropped'; count: number }
  /** 내가 띄운 링크로 새 기기가 들어와 승인을 기다림. 같은 링크를 다른 기기도 쓰면 `contested`로 다시 옵니다. */
  | ({ type: 'device.link_request' } & DeviceLinkRequestView)
  /** 새 기기 쪽: 쓰던 기기의 판단. */
  | { type: 'device.link_result'; status: DeviceLinkStatus }
  | { type: 'device.synced' }
  /** 후원 확인 상태가 바뀌었거나 로그인이 끝났음(`supporter.status`로 다시 읽음). 로그인 실패면 까닭 코드. */
  | { type: 'supporter.updated'; error?: 'denied' | 'expired' | 'creator' | 'network' | 'failed' }
  /** 업데이트 파일 받는 중(`update.download`). `ready`면 서명·SHA-256까지 확인하고 다 받음. */
  | { type: 'update.progress'; version: string; done: number; total: number; ready?: boolean; error?: UpdateError }
  | { type: 'device.wiped' }
  /** 같은 순번의 갈라진 체크포인트를 봤거나(conflict·equivocation), 체크포인트 상태를 버리고 처음부터 받는 중(fallback). */
  | { type: 'checkpoint.alert'; space: SpaceId; kind: CheckpointAlertKind }
  /** 누가 이 채널에 글을 쓰는 중(몇 초 동안만 보여 줌). 익명 채널은 user가 익명 사용자이고 anon_name이 가명입니다. */
  | { type: 'channel.typing'; space: SpaceId; channel: ChannelId; user: UserId; anon_name?: string };

export type NotificationType = Notification['type'];

// ───────────── 오류 코드 ─────────────

export const ErrorCode = {
  ParseError: -32700,
  InvalidRequest: -32600,
  MethodNotFound: -32601,
  InvalidParams: -32602,
  Internal: -32603,
  /** 데스크톱 앱 전용: 데몬에 연결되지 않음. */
  DaemonUnavailable: -32001,
  AuthRequired: 1001,
  OutOfScope: 1002,
  Forbidden: 1003,
  NotFound: 1004,
  InvalidInput: 1005,
  ConfirmationRequired: 1006,
  VolatileReadDenied: 1007,
  NoIdentity: 1008,
  Network: 1009,
  Conflict: 1010,
} as const;

/** 권한 이름(yangpa-core Permissions::NAMES와 같은 순서·비트). */
export const PERMISSION_BITS: ReadonlyArray<readonly [string, number]> = [
  ['view_channel', 0],
  ['send_messages', 1],
  ['read_history', 2],
  ['add_reactions', 3],
  ['attach_files', 4],
  ['embed_links', 5],
  ['use_external_emoji', 6],
  ['connect_voice', 7],
  ['speak', 8],
  ['stream', 9],
  ['create_posts', 10],
  ['use_crew_stun', 11],
  // 12는 예전 use_crew_turn(TURN 삭제): 옛 역할에 켜져 있을 수 있어 다른 뜻으로 다시 쓰지 않습니다.
  ['mention_many', 13],
  ['manage_messages', 16],
  ['manage_channels', 17],
  ['manage_roles', 18],
  ['kick', 19],
  ['create_invite', 20],
  ['manage_emoji', 21],
  ['manage_voice', 22],
  ['manage_space', 23],
  ['ban', 24],
  ['administrator', 31],
];
